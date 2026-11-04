// AgentEvent → messages 行。对齐 cindy apps/desktop/src/main/maker-ipc/sessionEventStream.ts
// （persistSessionStreamEvent 的事件分派）、sessionEventTerminal.ts（done/终止型 error 的收尾顺序）
// 和 apps/desktop/src/main/messagePersistBroadcaster.ts（各 on*Event 的聚合与 persistId 规则）。
// desktop 的写入走异步 FIFO；node:sqlite 是同步的，这里直接写库，顺序天然与事件顺序一致。
// 省略的 desktop 分支：background turn、Codex 计划行、/clear 竞态、跨窗 dedup、媒体挂账、
// late-final / item-link 校正（Codex 专属）。

import { randomBytes } from 'node:crypto';

// 同 desktop shared/toolResultPersistCap.ts 的上限与后缀（省略 diagnostics / 卡片压缩，
// 那部分依赖 @cindy/maker-shared/tool-result-compaction，sidecar 打包解析不到）。
const TOOL_RESULT_PERSIST_CONTENT_LIMIT = 8 * 1024;
const TOOL_RESULT_PERSIST_TRUNCATION_SUFFIX = '\n\n[tool result truncated: stored first 8KB]';

export function capToolResultTextForPersist(text, limit = TOOL_RESULT_PERSIST_CONTENT_LIMIT) {
  if (text.length <= limit) return text;
  let cut = Math.max(0, limit - TOOL_RESULT_PERSIST_TRUNCATION_SUFFIX.length);
  const lastKept = text.charCodeAt(cut - 1);
  if (cut > 0 && lastKept >= 0xd800 && lastKept <= 0xdbff) cut -= 1;
  return `${text.slice(0, cut)}${TOOL_RESULT_PERSIST_TRUNCATION_SUFFIX}`;
}

/** cuid 形态的 id（desktop 用 @paralleldrive/cuid2 createId：小写字母开头、24 位 [a-z0-9]）。 */
export function createId() {
  const alphabet = 'abcdefghijklmnopqrstuvwxyz0123456789';
  const bytes = randomBytes(24);
  let out = alphabet[bytes[0] % 26];
  for (let i = 1; i < 24; i++) out += alphabet[bytes[i] % 36];
  return out;
}

// 抄自 messagePersistBroadcaster.ts PROXY_SERVER_CARRYING_TOOLS / redactToolInputForUntrustedBoundary
// 的简化版：只处理顶层 proxyServer 字段，整体替换成占位。
const PROXY_SERVER_CARRYING_TOOLS = new Set([
  'cindy_browser',
  'cindy_mcp_call_tool',
  'mcp__cindy_browser__call_tool',
  'mcp:cindy_browser:call_tool',
  'mcp:cindy_browser',
]);
const REDACTED_PROXY_SERVER = '[redacted]';
function redactToolInput(toolName, input) {
  if (!PROXY_SERVER_CARRYING_TOOLS.has(toolName)) return input;
  if (typeof input === 'string') return input.includes('proxyServer') ? REDACTED_PROXY_SERVER : input;
  if (input && typeof input === 'object' && !Array.isArray(input) && 'proxyServer' in input) {
    return { ...input, proxyServer: REDACTED_PROXY_SERVER };
  }
  return input;
}

// 抄自 messagePersistBroadcaster.ts markFailedToolResultText。
function markFailedToolResultText(text, isError) {
  if (!isError || text.includes('<tool_use_error>')) return text;
  return `<tool_use_error>${text}</tool_use_error>`;
}

// 抄自 maker-core types/events.ts isTerminalTurnErrorEvent 的判定口径：
// 先看 isTerminal；没有时用 !willRetry；都没有按终止处理。
export function isTerminalError(data) {
  if (typeof data?.isTerminal === 'boolean') return data.isTerminal;
  if (typeof data?.willRetry === 'boolean') return !data.willRetry;
  return true;
}

const hasMeta = (m) => !!m && typeof m === 'object' && Object.keys(m).length > 0;

/**
 * 每个 db 一份。store 提供同步写库原语：
 *   insert(sessionId, row) → Message | null（按 (sessionId, clientId) 幂等）
 *   updateContent(sessionId, clientId, content) → Message | null
 *   latestCreatedAt(sessionId) → number | null
 *   providerIdOf(sessionId) → string | null
 *   noteTurnEnded(sessionId, at)
 *   emitError(sessionId, persistId) / emitRow(sessionId, message)
 */
export function createEventPersister(store) {
  const states = new Map();
  const stateOf = (sid) => {
    let s = states.get(sid);
    if (!s) {
      s = {
        block: null, // 在飞 assistant：{ persistId, text, agentMessageId, agentMeta, createdAt }
        lastAgentMeta: null,
        lastPersisted: null, // { role, persistId, text?, agentMessageId? }
        boundaryFlushed: null, // { persistId, text, agentMessageId? }
        toolUseCreatedAt: new Map(),
        toolUsePersistId: new Map(),
        toolResultIdByToolUseId: new Map(),
        pendingFullText: new Map(), // toolUseId → { text, createdAt }
        toolResultContent: new Map(), // persistId → 未截断全文
        interactions: new Map(), // requestId → { persistId, kind, request }
      };
      states.set(sid, s);
    }
    return s;
  };

  const persistRow = (sid, row, { broadcast = true } = {}) => {
    const msg = store.insert(sid, row);
    if (msg && broadcast) store.emitRow(sid, msg);
    return msg;
  };

  // messagePersistBroadcaster.ts enqueuePersistAssistant：content 为字符串，agentMeta 带 agentMessageId。
  const persistAssistant = (sid, s, persistId, text, agentMeta, createdAt, agentMessageId) => {
    const meta = agentMessageId ? { ...(agentMeta ?? {}), agentMessageId } : agentMeta;
    persistRow(sid, { clientId: persistId, role: 'assistant', content: text, agentMeta: meta ?? null, createdAt });
    s.lastPersisted = { role: 'assistant', persistId, text, agentMessageId };
  };

  // messagePersistBroadcaster.ts flushAssistantBlockInternal。
  const flushAssistantBlock = (sid, agentMetaFallback = null) => {
    const s = stateOf(sid);
    const block = s.block;
    if (!block) return undefined;
    s.block = null;
    if (!block.text) return undefined;
    const meta = block.agentMeta ?? agentMetaFallback ?? s.lastAgentMeta ?? null;
    persistAssistant(sid, s, block.persistId, block.text, meta, block.createdAt, block.agentMessageId);
    s.boundaryFlushed = { persistId: block.persistId, text: block.text, agentMessageId: block.agentMessageId };
    return block.persistId;
  };

  // messagePersistBroadcaster.ts assembleAssistantTextEvent（去掉 Codex late-final / item-link 分支）。
  const onText = (sid, data, agentMeta) => {
    const s = stateOf(sid);
    if (typeof data.phase === 'string' || data.runtimeRecovery === true) {
      agentMeta = { ...agentMeta, assistantPhase: data.runtimeRecovery === true ? 'commentary' : data.phase };
    }
    const rawText = typeof data.text === 'string' ? data.text : '';
    const isFinal = data.isFinal === true;
    const isFullText = data.isFullText === true;
    const agentMessageId = typeof data.agentMessageId === 'string' && data.agentMessageId ? data.agentMessageId : undefined;

    if (s.block?.agentMessageId && agentMessageId && s.block.agentMessageId !== agentMessageId) {
      flushAssistantBlock(sid);
    }

    if (isFinal) {
      const block = s.block;
      if (block) {
        // 流式确认：不落库，留给边界 flush；权威全文或更长前缀才覆盖累积文本。
        const acceptedLongerPrefix = rawText.length > block.text.length && rawText.startsWith(block.text);
        if (isFullText || acceptedLongerPrefix) block.text = rawText;
        if (agentMeta) block.agentMeta = agentMeta;
        return block.persistId;
      }
      // 交互边界（ask_user / plan_review）紧随 flush 落库时，同源全文快照复用已落库行。
      const bf = s.boundaryFlushed;
      const atInteractionBoundary = s.lastPersisted?.role === 'ask_user' || s.lastPersisted?.role === 'plan_review';
      if (bf && rawText && atInteractionBoundary && (!agentMessageId || bf.agentMessageId === agentMessageId) &&
        (bf.text === rawText || isFullText)) {
        if (isFullText && bf.text !== rawText) {
          const updated = store.updateContent(sid, bf.persistId, rawText);
          if (updated) {
            bf.text = rawText;
            store.emitRow(sid, updated);
          }
        }
        return bf.persistId;
      }
      if (rawText) {
        const last = s.lastPersisted;
        if (last?.role === 'assistant' && last.text === rawText && (!agentMessageId || last.agentMessageId === agentMessageId)) {
          return last.persistId; // 相邻 DUP-SKIP
        }
        const persistId = createId();
        persistAssistant(sid, s, persistId, rawText, agentMeta, Date.now(), agentMessageId);
        return persistId;
      }
      return undefined;
    }

    if (!s.block) {
      s.boundaryFlushed = null;
      s.block = { persistId: createId(), text: rawText, agentMessageId, agentMeta, createdAt: Date.now() };
    } else {
      s.block.text += rawText;
      if (agentMeta) s.block.agentMeta = agentMeta;
    }
    return s.block.persistId;
  };

  // messagePersistBroadcaster.ts onToolUseEvent（去掉 Codex 计划/后台分支）。
  const onToolUse = (sid, data, agentMeta) => {
    const s = stateOf(sid);
    const createdAt = Date.now();
    const toolUseId = typeof data.toolUseId === 'string' ? data.toolUseId : '';
    const toolName = typeof data.toolName === 'string' ? data.toolName : '';
    const input = redactToolInput(toolName, data.input);
    const content = { toolUseId, toolName, input };
    const existing = toolUseId ? s.toolUsePersistId.get(toolUseId) : undefined;
    if (existing) {
      // 同一 toolUseId 再次到达（update_plan / web_search 等可更新工具）：改写原行。
      store.updateContent(sid, existing, content);
      s.lastPersisted = { role: 'tool_use', persistId: existing };
      return existing;
    }
    const persistId = createId();
    persistRow(sid, {
      clientId: persistId,
      role: 'tool_use',
      content,
      toolUseId: toolUseId || null,
      agentMeta: agentMeta ?? s.lastAgentMeta ?? null,
      createdAt,
    });
    if (toolUseId) {
      s.toolUseCreatedAt.set(toolUseId, createdAt);
      s.toolUsePersistId.set(toolUseId, persistId);
    }
    s.lastPersisted = { role: 'tool_use', persistId };
    return persistId;
  };

  // messagePersistBroadcaster.ts clampAfterToolUse：tool_result 不早于对应 tool_use。
  const clampAfterToolUse = (s, toolUseId, createdAt) => {
    const t = s.toolUseCreatedAt.get(toolUseId);
    return t === undefined || createdAt > t ? createdAt : t + 1;
  };

  // messagePersistBroadcaster.ts onToolResultEvent（turn scope）。返回 { persistId, content }。
  const onToolResult = (sid, data, agentMeta) => {
    const s = stateOf(sid);
    const summary = typeof data.summary === 'string' ? data.summary : '';
    const ids = Array.isArray(data.toolUseIds) ? data.toolUseIds.filter((x) => typeof x === 'string' && x) : [];
    let createdAt = Date.now();
    let content = summary;
    for (const id of ids) {
      const buffered = s.pendingFullText.get(id);
      if (buffered && buffered.text.length > content.length) {
        content = buffered.text;
        createdAt = clampAfterToolUse(s, id, buffered.createdAt);
      }
      s.pendingFullText.delete(id);
    }
    for (const id of ids) createdAt = clampAfterToolUse(s, id, createdAt);
    let existing;
    for (const id of ids) {
      existing = s.toolResultIdByToolUseId.get(id);
      if (existing) break;
    }
    if (existing) {
      for (const id of ids) s.toolResultIdByToolUseId.set(id, existing);
      const prev = s.toolResultContent.get(existing);
      if (prev === undefined || content.length <= prev.length) return { persistId: existing, content: prev ?? content };
      s.toolResultContent.set(existing, content);
      if (capToolResultTextForPersist(content) !== capToolResultTextForPersist(prev)) {
        store.updateContent(sid, existing, capToolResultTextForPersist(content));
      }
      s.lastPersisted = { role: 'tool_result', persistId: existing };
      return { persistId: existing, content };
    }
    const persistId = createId();
    for (const id of ids) s.toolResultIdByToolUseId.set(id, persistId);
    s.toolResultContent.set(persistId, content);
    persistRow(sid, {
      clientId: persistId,
      role: 'tool_result',
      content: capToolResultTextForPersist(content),
      toolUseId: ids[0] ?? null,
      agentMeta: agentMeta ?? s.lastAgentMeta ?? null,
      createdAt,
    });
    s.lastPersisted = { role: 'tool_result', persistId };
    return { persistId, content };
  };

  // messagePersistBroadcaster.ts onToolResultFullEvent（turn scope）。
  const onToolResultFull = (sid, data, agentMeta) => {
    const s = stateOf(sid);
    const toolUseId = typeof data.toolUseId === 'string' ? data.toolUseId : '';
    const rawText = typeof data.fullText === 'string' ? data.fullText : null;
    if (!toolUseId || rawText === null) return null;
    const fullText = markFailedToolResultText(rawText, data.isError === true);
    const createdAt = Date.now();
    const target = s.toolResultIdByToolUseId.get(toolUseId);
    if (!target) {
      if (s.toolUsePersistId.has(toolUseId)) {
        // tool_use 已到、摘要未到：eager-create。
        const persistId = createId();
        s.toolResultIdByToolUseId.set(toolUseId, persistId);
        s.pendingFullText.delete(toolUseId);
        s.toolResultContent.set(persistId, fullText);
        persistRow(sid, {
          clientId: persistId,
          role: 'tool_result',
          content: capToolResultTextForPersist(fullText),
          toolUseId,
          agentMeta: agentMeta ?? s.lastAgentMeta ?? null,
          createdAt: clampAfterToolUse(s, toolUseId, createdAt),
        });
        s.lastPersisted = { role: 'tool_result', persistId };
        return { persistId, content: fullText };
      }
      s.pendingFullText.set(toolUseId, { text: fullText, createdAt });
      return null;
    }
    const prev = s.toolResultContent.get(target);
    if (prev === fullText) return null;
    s.toolResultContent.set(target, fullText);
    const capped = capToolResultTextForPersist(fullText);
    if (prev === undefined || capped !== capToolResultTextForPersist(prev)) store.updateContent(sid, target, capped);
    s.lastPersisted = { role: 'tool_result', persistId: target };
    return { persistId: target, content: fullText };
  };

  // messagePersistBroadcaster.ts onThinkingEvent：只有 final / redacted 落库，clientId = blockId。
  const onThinking = (sid, data, agentMeta) => {
    const s = stateOf(sid);
    const blockId = typeof data.blockId === 'string' ? data.blockId : '';
    if (!blockId) return;
    const meta = agentMeta ?? s.lastAgentMeta ?? null;
    const finishedAt = Date.now();
    if (data.stage === 'final') {
      const text = typeof data.text === 'string' ? data.text : '';
      const durationMs = typeof data.durationMs === 'number' ? data.durationMs : 0;
      persistRow(sid, {
        clientId: blockId,
        role: 'thinking',
        content: { kind: 'thinking', text, durationMs, isRedacted: false, finishedAt },
        agentMeta: meta,
        createdAt: finishedAt - Math.max(0, durationMs),
      });
      s.lastPersisted = { role: 'thinking', persistId: blockId };
    } else if (data.stage === 'redacted') {
      persistRow(sid, {
        clientId: blockId,
        role: 'thinking',
        content: { kind: 'thinking', text: '', durationMs: 0, isRedacted: true, finishedAt },
        agentMeta: meta,
        createdAt: finishedAt,
      });
      s.lastPersisted = { role: 'thinking', persistId: blockId };
    }
  };

  // messagePersistBroadcaster.ts flushOrphanToolResults：done 时把只有全文、没有摘要的结果落成行。
  const flushOrphanToolResults = (sid, agentMeta) => {
    const s = stateOf(sid);
    for (const [toolUseId, buffered] of s.pendingFullText) {
      const persistId = createId();
      s.toolResultIdByToolUseId.set(toolUseId, persistId);
      s.toolResultContent.set(persistId, buffered.text);
      persistRow(sid, {
        clientId: persistId,
        role: 'tool_result',
        content: capToolResultTextForPersist(buffered.text),
        toolUseId,
        agentMeta: agentMeta ?? s.lastAgentMeta ?? null,
        createdAt: clampAfterToolUse(s, toolUseId, buffered.createdAt),
      });
    }
    s.pendingFullText.clear();
  };

  // messagePersistBroadcaster.ts onTurnErrorEvent + buildTurnErrorContent + enqueueTurnErrorRow。
  // error 行不广播 messages:created，改推 local-db:session:error-persisted（renderer 下次打开重拉）。
  const onTurnError = (sid, data, agentMeta) => {
    const message = typeof data?.message === 'string' ? data.message : '';
    if (!message) return null;
    const s = stateOf(sid);
    const orderedAt = Date.now();
    flushAssistantBlock(sid, agentMeta);
    const content = { message };
    if (typeof data.reason === 'string' && data.reason) content.reason = data.reason;
    if (typeof data.sdkError === 'string' && data.sdkError) content.sdkError = data.sdkError;
    if (data.toolLoop && typeof data.toolLoop === 'object') content.toolLoop = data.toolLoop;
    const providerId = store.providerIdOf(sid);
    if (providerId) content.providerId = providerId;
    const latest = store.latestCreatedAt(sid);
    const createdAt = (latest != null ? Math.min(latest, orderedAt) : orderedAt) + 1;
    const persistId = createId();
    persistRow(sid, {
      clientId: persistId,
      role: 'error',
      content,
      agentMeta: agentMeta ?? s.lastAgentMeta ?? null,
      createdAt,
    }, { broadcast: false });
    store.emitError(sid, persistId);
    return persistId;
  };

  // messagePersistBroadcaster.ts resetTurnPersistState 的子集：清掉本轮的聚合状态。
  const resetTurn = (sid) => {
    const s = stateOf(sid);
    s.block = null;
    s.boundaryFlushed = null;
    s.toolUseCreatedAt.clear();
    s.toolUsePersistId.clear();
    s.toolResultIdByToolUseId.clear();
    s.pendingFullText.clear();
    s.toolResultContent.clear();
  };

  // sessionEventTerminal.ts：flush assistant → orphan tool_result → error 行 → reset → 记 turn 结束。
  const onTerminal = (sid, event, agentMeta) => {
    flushAssistantBlock(sid, agentMeta);
    flushOrphanToolResults(sid, agentMeta);
    const persistId = event.type === 'error' ? onTurnError(sid, event.data, agentMeta) : null;
    resetTurn(sid);
    store.noteTurnEnded(sid, Date.now());
    return persistId;
  };

  // messagePersistBroadcaster.ts onInteractionMessage：ask_user / plan_review 落 pending 行，
  // permission 不建行。同一 requestId 重复到达返回同一 persistId。
  const persistInteractionRequest = (sid, req) => {
    const requestId = typeof req?.requestId === 'string' ? req.requestId : '';
    if (!requestId) return null;
    const s = stateOf(sid);
    const known = s.interactions.get(requestId);
    if (known) return known.persistId;
    if (req.kind !== 'ask_user_question' && !(req.kind === 'plan_review' && typeof req.plan === 'string' && req.plan)) {
      return null;
    }
    flushAssistantBlock(sid);
    const meta = { ...(s.lastAgentMeta ?? {}) };
    delete meta.autoReviewUserText;
    const persistId = createId();
    const createdAt = Date.now();
    const role = req.kind === 'ask_user_question' ? 'ask_user' : 'plan_review';
    const content = role === 'ask_user'
      ? { requestId, questions: req.questions ?? [], status: 'pending', answers: null }
      : { requestId, plan: req.plan, planFilePath: typeof req.planFilePath === 'string' ? req.planFilePath : '', status: 'pending', feedback: null };
    persistRow(sid, { clientId: persistId, role, content, agentMeta: meta, createdAt });
    s.interactions.set(requestId, { persistId, kind: req.kind, request: req, resolved: false });
    s.lastPersisted = { role, persistId };
    return persistId;
  };

  // messagePersistBroadcaster.ts onInteractionResolved：把 pending 行回写成最终状态并广播权威行。
  const persistInteractionResolved = (sid, requestId, decision = {}) => {
    const entry = stateOf(sid).interactions.get(requestId);
    if (!entry || entry.resolved) return null;
    entry.resolved = true;
    const { persistId, kind, request } = entry;
    let content;
    if (kind === 'ask_user_question') {
      content = {
        requestId,
        questions: request.questions ?? [],
        status: decision.dismissed === true ? 'cancelled' : 'answered',
        answers: decision.answers ?? {},
      };
    } else {
      const behavior = decision.behavior === 'allow' ? 'allow' : 'deny';
      const dismissed = behavior === 'deny' && decision.dismissed === true;
      content = {
        requestId,
        plan: typeof decision.editedPlan === 'string' ? decision.editedPlan : request.plan ?? '',
        planFilePath: typeof request.planFilePath === 'string' ? request.planFilePath : '',
        status: behavior === 'allow' ? 'approved' : dismissed ? 'cancelled' : 'revised',
        feedback: behavior === 'deny' && !dismissed ? (decision.reason ?? null) : null,
      };
    }
    const updated = store.updateContent(sid, persistId, content);
    if (updated) store.emitRow(sid, updated);
    return updated;
  };

  /** sessionEventStream.ts persistSessionStreamEvent + sessionEventTerminal 的分派。返回 persistId 或 null。 */
  const persistMakerEvent = (sid, event) => {
    if (!event || typeof event !== 'object' || typeof event.type !== 'string') return null;
    if (event.turnScope === 'background') return null; // 后台迟到事件：本原型不落库
    const s = stateOf(sid);
    const agentMeta = hasMeta(event.agentMeta) ? event.agentMeta : null;
    if (agentMeta) s.lastAgentMeta = agentMeta; // noteAgentMeta
    const data = event.data && typeof event.data === 'object' ? event.data : {};
    switch (event.type) {
      case 'text': {
        if (event.standaloneText === true) {
          // onStandaloneTextEvent：完整的扩展通知，独立成行，不与在飞文本拼接。
          const text = typeof data.text === 'string' ? data.text : '';
          if (!text) return null;
          flushAssistantBlock(sid, agentMeta);
          const persistId = createId();
          persistAssistant(sid, s, persistId, text, agentMeta ?? s.lastAgentMeta, Date.now());
          return persistId;
        }
        const persistId = onText(sid, data, agentMeta) ?? null;
        // Pi message_end 带整条 assistant 全文：立即提交，不等 agent_settled。
        if (event.source === 'pi' && data.isFinal === true && data.isFullText === true) flushAssistantBlock(sid, agentMeta);
        return persistId;
      }
      case 'tool_use':
        flushAssistantBlock(sid, agentMeta);
        return onToolUse(sid, data, agentMeta);
      case 'tool_result':
        return onToolResult(sid, data, agentMeta)?.persistId ?? null;
      case 'tool_result_full':
        return onToolResultFull(sid, data, agentMeta)?.persistId ?? null;
      case 'thinking':
        onThinking(sid, data, agentMeta);
        return null;
      case 'interaction_request':
        return persistInteractionRequest(sid, data);
      case 'done':
        return onTerminal(sid, event, agentMeta);
      case 'error':
        return isTerminalError(data) ? onTerminal(sid, event, agentMeta) : null;
      default:
        return null;
    }
  };

  return {
    persistMakerEvent,
    persistInteractionRequest,
    persistInteractionResolved,
    flushAssistantBlock,
    forget: (sid) => states.delete(sid),
  };
}
