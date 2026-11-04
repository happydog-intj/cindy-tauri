// A 子任务：maker:* 会话 IPC（创建 / 发送 / 输入队列 / 审批 / 关闭）。
// 返回形状对齐 desktop main/maker-ipc（preload.ts 类型 + register.ts / sessionCreateHandler.ts）。
// 只做最小链路：每个 session 一个串行输入队列，一次只跑一个 turn。
// 未覆盖（保持 [NOT_IMPLEMENTED]）：中断恢复、队列合并/steer、device-link、Orca、IM、伙伴、自动续跑、
// Claude Code / Codex。
import { randomUUID } from 'node:crypto';
import { mkdirSync } from 'node:fs';
import path from 'node:path';
import { isTerminalTurnEvent } from '@cindy/maker-core';
import { buildMakerUserMessage } from '@cindy-desktop-shared/agentInputQueue';

import { emitToRenderer, registerInvoke } from './ipc.mjs';
import { createMakerHost } from './maker-host.mjs';

// desktop schedulePendingPermissionTimeout 同款：permission 10 分钟无人处理按 deny 收尾。
const PERMISSION_TIMEOUT_MS = 10 * 60 * 1000;
const SAFE_ID = /^[A-Za-z0-9_.:-]{1,128}$/;

function fail(code, msg) {
  throw new Error(`[${code}] ${msg}`);
}

function requireSessionId(v) {
  if (typeof v !== 'string' || !SAFE_ID.test(v) || v === '.' || v === '..') fail('INVALID_PARAMS', 'sessionId required');
  return v;
}

// desktop register.ts defaultDecisionForPending。
function defaultDecision(kind, reason) {
  if (kind === 'ask_user_question') return { kind, answers: {}, dismissed: true };
  if (kind === 'plan_review') return { kind, behavior: 'deny', reason, dismissed: true };
  return { kind: 'permission', behavior: 'deny', reason };
}

function dialogueDir(dataDir, sessionId, now = new Date()) {
  const day = `${now.getFullYear()}-${String(now.getMonth() + 1).padStart(2, '0')}-${String(now.getDate()).padStart(2, '0')}`;
  return path.join(dataDir, 'dialogues', day, sessionId);
}

export function registerMakerIpc({ dataDir, db, models, agentManifestDir, host: injectedHost }) {
  const host = injectedHost ?? createMakerHost({ dataDir, models, agentManifestDir });
  const { maker } = host;

  /** sessionId → 输入/turn 状态（进程内，不持久化队列）。 */
  const states = new Map();
  /** requestId → 等 renderer 决策的 interaction。 */
  const pending = new Map();
  /** sessionId → 正在进行的 createSession（同 id 并发只建一次）。 */
  const creating = new Map();

  function getState(sessionId) {
    let s = states.get(sessionId);
    if (!s) {
      s = {
        pendingQueue: [],
        activeTurn: null, // { clientId, token }
        turnSeq: 0,
        queuePaused: false,
        error: null,
        row: null, // 最近一次 upsertSession 的行
        permissionMode: undefined,
        eventChain: Promise.resolve(),
        wired: null, // 已接线的 Session 实例
      };
      states.set(sessionId, s);
    }
    return s;
  }

  // 形状同 desktop AgentInputCoordinator.toProjection（无 clear/续跑/凭证切换等扩展态）。
  function projection(sessionId) {
    const s = getState(sessionId);
    return {
      sessionId,
      pendingQueue: [...s.pendingQueue],
      turnInProgress: s.activeTurn !== null,
      clearBoundaryMs: null,
      continuationInFlightClientId: null,
      continuationTurnClientId: null,
      steeringQueueClientIds: [],
      queuePaused: s.queuePaused,
      queueExpanded: false,
      queueInteractionLocks: [],
      queueEditLocks: [],
      queueAbortPending: false,
      error: s.error,
      recovery: null,
      errorRetryText: null,
      credentialSwitchWait: null,
    };
  }

  function pushProjection(sessionId) {
    emitToRenderer('maker:input:projection', projection(sessionId));
  }

  async function upsertRow(sessionId, patch) {
    const s = getState(sessionId);
    const now = Date.now();
    s.row = { ...(s.row ?? { id: sessionId, createdAt: now, status: 'active' }), ...patch, updatedAt: now };
    try {
      await db.upsertSession(s.row);
    } catch (err) {
      process.stderr.write(`[maker-ipc] upsertSession failed: ${err?.message ?? err}\n`);
    }
  }

  // ── interaction（审批 / 提问 / 计划）────────────────────────────────────
  function settleInteraction(requestId, decision, reason) {
    const entry = pending.get(requestId);
    if (!entry) return false;
    pending.delete(requestId);
    clearTimeout(entry.timer);
    entry.resolve(decision);
    const resolvedAs = decision?.behavior === 'deny' || decision?.dismissed ? 'deny' : 'allow';
    const carryDecision = reason === 'resolved' && (entry.kind === 'ask_user_question' || entry.kind === 'plan_review');
    emitToRenderer('maker:interaction-dismissed', {
      sessionId: entry.sessionId,
      requestId,
      reason,
      resolvedAs,
      ...(carryDecision ? { decision } : {}),
    });
    return true;
  }

  function cancelInteractionsForSession(sessionId, reason) {
    for (const [requestId, entry] of [...pending]) {
      if (entry.sessionId === sessionId) settleInteraction(requestId, defaultDecision(entry.kind, reason), reason);
    }
  }

  function installInteractionListener(session) {
    session.setInteractionListener((req, signal) => new Promise((resolve) => {
      const entry = { sessionId: session.id, kind: req.kind, request: req, resolve, timer: null };
      // 先登记再广播，避免 renderer 回得太快时找不到 resolver。
      pending.set(req.requestId, entry);
      if (req.kind === 'permission') {
        entry.timer = setTimeout(() => settleInteraction(req.requestId, defaultDecision(req.kind, 'timeout'), 'timeout'), PERMISSION_TIMEOUT_MS);
        entry.timer.unref?.();
      }
      signal?.addEventListener('abort', () => settleInteraction(req.requestId, defaultDecision(req.kind, 'session_aborted'), 'session_aborted'), { once: true });
      emitToRenderer('maker:interaction-request', { sessionId: session.id, request: req, persistId: undefined });
    }));
  }

  // ── 事件管线 ────────────────────────────────────────────────────────────
  function onTurnSettled(sessionId, event) {
    const s = getState(sessionId);
    if (event.type === 'error') {
      const msg = event.data && typeof event.data.message === 'string' ? event.data.message : 'Agent error';
      s.error = msg;
    }
    s.activeTurn = null;
    void upsertRow(sessionId, {});
    pushProjection(sessionId);
    void pump(sessionId);
  }

  function wireSession(session) {
    const s = getState(session.id);
    if (s.wired === session) return;
    s.wired = session;
    installInteractionListener(session);
    session.onEvent((event) => {
      // 落库是异步的；按 session 串行，保证 renderer 收到的顺序与 maker-core 一致。
      s.eventChain = s.eventChain.then(async () => {
        let persistId = null;
        try {
          persistId = (await db.persistMakerEvent(session.id, event)) ?? null;
        } catch (err) {
          process.stderr.write(`[maker-ipc] persistMakerEvent failed: ${err?.message ?? err}\n`);
        }
        emitToRenderer('maker:event', { sessionId: session.id, event, ...(persistId ? { persistId } : {}) });
        const active = s.activeTurn;
        if (
          active && event.turnScope !== 'background' && event.turnAttemptToken === active.token &&
          isTerminalTurnEvent(event) && session.getObservedCurrentTurnTerminal().kind !== 'none'
        ) {
          onTurnSettled(session.id, event);
        }
      });
    });
    session.onStatusChange((status) => {
      emitToRenderer('maker:status-changed', { sessionId: session.id, status });
      if (status === 'closed') {
        cancelInteractionsForSession(session.id, 'session_closed');
        if (s.wired === session) s.wired = null;
        if (s.activeTurn) { s.activeTurn = null; pushProjection(session.id); }
      }
    });
  }

  // ── 创建 ────────────────────────────────────────────────────────────────
  async function createSession(raw) {
    if (!raw || typeof raw !== 'object' || Array.isArray(raw)) fail('INVALID_PARAMS', 'createSession opts must be an object');
    if (raw.agentKind !== 'pi') fail('NOT_IMPLEMENTED', `agentKind ${String(raw.agentKind)} is not supported in the Tauri build (Pi only)`);
    if (typeof raw.model !== 'string' || !raw.model) fail('INVALID_PARAMS', 'model required');
    const id = raw.id === undefined || raw.id === null ? randomUUID() : requireSessionId(raw.id);
    const live = maker.getSession(id);
    if (live && live.getStatus() !== 'closed') return live;
    if (creating.has(id)) return creating.get(id);
    const task = (async () => {
      const cfg = host.resolveSessionModel(raw.providerId ?? null, raw.model);
      await host.ensurePiInstalled();
      const workingDir = typeof raw.workingDir === 'string' && raw.workingDir.trim()
        ? raw.workingDir
        : dialogueDir(dataDir, id);
      mkdirSync(workingDir, { recursive: true });
      // 权限档原样透传（ask / auto / bypassPermissions），由 maker-core 决定审批行为。
      const opts = {
        id,
        agentKind: 'pi',
        workingDir,
        model: cfg.modelId,
        providerId: cfg.providerId,
        ...(raw.title ? { title: String(raw.title) } : {}),
        ...(raw.effort ? { effort: raw.effort } : {}),
        ...(raw.permissionMode ? { permissionMode: raw.permissionMode } : {}),
        ...(typeof raw.planMode === 'boolean' ? { planMode: raw.planMode } : {}),
        ...(typeof raw.userPrompt === 'string' ? { userPrompt: raw.userPrompt } : {}),
        ...(typeof raw.resumeSessionId === 'string' && raw.resumeSessionId ? { resumeSessionId: raw.resumeSessionId } : {}),
        ...(raw.workspaceKind === 'project' || raw.workspaceKind === 'dialogue' ? { workspaceKind: raw.workspaceKind } : {}),
        vendorOptions: {
          ...(raw.vendorOptions && typeof raw.vendorOptions === 'object' && !Array.isArray(raw.vendorOptions) ? raw.vendorOptions : {}),
          onStderrLine: (line) => { if (line.trim()) process.stderr.write(`[pi:${id}] ${line}\n`); },
        },
      };
      const session = await maker.createSession(opts);
      wireSession(session);
      const s = getState(id);
      s.permissionMode = raw.permissionMode;
      await upsertRow(id, {
        agentKind: 'pi',
        title: raw.title ?? s.row?.title ?? '',
        workDir: session.workDir,
        providerId: cfg.providerId,
        model: cfg.modelId,
        permissionMode: raw.permissionMode ?? null,
        status: 'active',
      });
      return session;
    })();
    creating.set(id, task);
    try {
      return await task;
    } finally {
      creating.delete(id);
    }
  }

  async function ensureSession(sessionId, createOpts) {
    const live = maker.getSession(sessionId);
    if (live && live.getStatus() !== 'closed') return live;
    if (!createOpts) fail('SESSION_NOT_FOUND', `session ${sessionId} is not running and no createOpts were given`);
    return createSession({ ...createOpts, id: sessionId });
  }

  // permissionMode 跟随用户最新一条输入（desktop 在发送前同样以队列项的档位为准）。
  async function applyPermissionMode(session, mode) {
    const s = getState(session.id);
    if (!mode || mode === s.permissionMode) return;
    await session.setPermissionMode(mode);
    s.permissionMode = mode;
    await upsertRow(session.id, { permissionMode: mode });
  }

  // ── 队列派发 ────────────────────────────────────────────────────────────
  async function pump(sessionId) {
    const s = getState(sessionId);
    if (s.activeTurn || s.queuePaused || s.pendingQueue.length === 0) return;
    const item = s.pendingQueue.shift();
    const token = ++s.turnSeq;
    s.activeTurn = { clientId: item.clientId, token };
    s.error = null;
    pushProjection(sessionId);
    try {
      const session = await ensureSession(sessionId, { ...item.createOpts, workingDir: item.createOpts?.workingDir || item.workingDir });
      await applyPermissionMode(session, item.permissionMode || item.createOpts?.permissionMode);
      const message = buildMakerUserMessage(item);
      const result = await session.send(message, {
        turnAttemptToken: token,
        onAccepted: async () => {
          // B 若提供用户消息落库就调用；desktop 在 onAccepted 里写 user 行。
          await db.persistUserMessage?.(sessionId, {
            clientId: item.clientId,
            role: 'user',
            content: item.persistedContent ?? item.text,
          });
        },
      });
      if (!result.accepted) throw new Error(`send not accepted: ${result.reason}`);
    } catch (err) {
      if (s.activeTurn?.token === token) {
        s.activeTurn = null;
        s.error = err instanceof Error ? err.message : String(err);
        pushProjection(sessionId);
      }
    }
  }

  // ── 通道 ────────────────────────────────────────────────────────────────
  registerInvoke('maker:list-available-agents', async () => maker.listAvailableAgents());

  registerInvoke('maker:get-capabilities', async (agentKind) => {
    if (agentKind !== 'pi') fail('NOT_IMPLEMENTED', `agentKind ${String(agentKind)} is not supported`);
    // desktop register.ts GET_CAPABILITIES：agent 能力 + host 级标志位；可选模型取自已连接的自定义 provider。
    const caps = maker.getCapabilities('pi');
    const fromProviders = typeof models?.getPiModelDescriptors === 'function' ? models.getPiModelDescriptors() : [];
    return {
      ...caps,
      availableModels: fromProviders.length ? fromProviders : (caps.availableModels ?? []),
      supportsSessionAgentSwitch: false,
      supportsModelWindowSwitchGuard: false,
    };
  });

  // desktop statusHandlers.ts → maker.getAgentStatus；binaryReady 以实际安装状态为准，缺失时后台安装。
  registerInvoke('maker:agent:status', async (agentKind) => {
    if (agentKind !== 'pi') return { binaryReady: false, binaryPath: null, authReady: false };
    const st = host.piStatus();
    if (!st.installed) void host.ensurePiInstalled().catch((err) => process.stderr.write(`[maker-ipc] pi install failed: ${err?.message ?? err}\n`));
    const base = await maker.getAgentStatus('pi');
    return { ...base, binaryReady: st.installed, binaryPath: st.installed ? st.binary : null };
  });

  registerInvoke('maker:create-session', async (opts) => {
    const session = await createSession(opts);
    return {
      sessionId: session.id,
      agentKind: session.agentKind,
      workDir: session.workDir,
      capabilities: session.capabilities,
      usedProjectContext: false,
    };
  });

  // 旧直发通道（hooks/useMakerSession.ts）；主路径是 maker:input:enqueue。
  registerInvoke('maker:send', async (sessionId, message, createOpts) => {
    const sid = requireSessionId(sessionId);
    const s = getState(sid);
    if (s.activeTurn) return { accepted: false, reason: 'turn-in-progress' };
    const session = await ensureSession(sid, createOpts);
    const token = ++s.turnSeq;
    s.activeTurn = { clientId: `direct:${token}`, token };
    pushProjection(sid);
    try {
      const result = await session.send(message, { turnAttemptToken: token });
      if (!result.accepted) { s.activeTurn = null; pushProjection(sid); }
      return result;
    } catch (err) {
      s.activeTurn = null;
      pushProjection(sid);
      throw err;
    }
  });

  registerInvoke('maker:input:get-projection', async (sessionId) => ({
    ...projection(requireSessionId(sessionId)),
    inputDeliveryVersion: 1,
  }));

  registerInvoke('maker:input:enqueue', async (sessionId, item) => {
    const sid = requireSessionId(sessionId);
    if (!item || typeof item !== 'object' || typeof item.clientId !== 'string' || typeof item.text !== 'string') {
      fail('INVALID_PARAMS', 'queued message requires clientId and text');
    }
    const s = getState(sid);
    if (s.activeTurn?.clientId === item.clientId || s.pendingQueue.some((q) => q.clientId === item.clientId)) {
      return projection(sid); // 同一 clientId 重试不重复入队
    }
    // 首条消息懒创建：失败直接 reject，renderer 的 catch 路径会解析 [CODE]。
    if (!maker.getSession(sid)) {
      await ensureSession(sid, { ...item.createOpts, workingDir: item.createOpts?.workingDir || item.workingDir });
    }
    s.pendingQueue.push(item);
    void pump(sid);
    return projection(sid);
  });

  registerInvoke('maker:input:stop', async (sessionId, opts) => {
    const sid = requireSessionId(sessionId);
    const s = getState(sid);
    if (!opts?.keepQueue) s.pendingQueue = [];
    if (opts?.pauseQueue) s.queuePaused = true;
    cancelInteractionsForSession(sid, 'session_aborted');
    const session = maker.getSession(sid);
    if (session && s.activeTurn) await session.abort().catch(() => undefined);
    // abort 后 maker-core 会发终态事件；兜底直接收尾，避免 UI 卡在运行中。
    if (s.activeTurn) s.activeTurn = null;
    pushProjection(sid);
    return projection(sid);
  });

  registerInvoke('maker:input:resume', async (sessionId) => {
    const sid = requireSessionId(sessionId);
    getState(sid).queuePaused = false;
    pushProjection(sid);
    void pump(sid);
    return projection(sid);
  });

  registerInvoke('maker:input:clear-error', async (sessionId) => {
    const sid = requireSessionId(sessionId);
    getState(sid).error = null;
    pushProjection(sid);
    return projection(sid);
  });

  registerInvoke('maker:input:remove', async (sessionId, clientId) => {
    const sid = requireSessionId(sessionId);
    const s = getState(sid);
    s.pendingQueue = s.pendingQueue.filter((q) => q.clientId !== clientId);
    pushProjection(sid);
    return projection(sid);
  });

  registerInvoke('maker:resolve-interaction', async (requestId, decision) => {
    if (typeof requestId !== 'string') fail('INVALID_PARAMS', 'requestId required');
    if (!decision || typeof decision !== 'object' || typeof decision.kind !== 'string') fail('INVALID_PARAMS', 'decision required');
    const entry = pending.get(requestId);
    if (!entry || entry.kind !== decision.kind) return { accepted: false };
    return { accepted: settleInteraction(requestId, decision, 'resolved') };
  });

  registerInvoke('maker:close-session', async (sessionId) => {
    const sid = requireSessionId(sessionId);
    cancelInteractionsForSession(sid, 'session_closed');
    await maker.closeSession(sid);
    const s = states.get(sid);
    if (s) { s.activeTurn = null; s.pendingQueue = []; pushProjection(sid); }
    return undefined;
  });

  registerInvoke('maker:list-active', async () => maker.listActiveSessions().map((s) => ({
    sessionId: s.id,
    agentKind: s.agentKind,
    workDir: s.workDir,
    capabilities: s.capabilities,
    isTurnRunning: s.isTurnRunning(),
  })));

  registerInvoke('maker:session-in-turn', async (sessionId) => {
    const sid = requireSessionId(sessionId);
    // 对齐 desktop：以 live session 的 isTurnRunning() 为准，未加载即 false。
    return maker.getSession(sid)?.isTurnRunning() ?? false;
  });

  return {
    host,
    isTurnRunning: (sid) => getState(sid).activeTurn !== null || (maker.getSession(sid)?.isTurnRunning() ?? false),
    shutdown: () => host.shutdown(),
  };
}
