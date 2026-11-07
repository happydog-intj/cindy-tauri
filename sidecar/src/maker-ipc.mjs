// A 子任务：maker:* 会话 IPC（创建 / 发送 / 输入队列 / 审批 / 关闭）。
// 返回形状对齐 desktop main/maker-ipc（preload.ts 类型 + register.ts / sessionCreateHandler.ts）。
// 只做最小链路：每个 session 一个串行输入队列，一次只跑一个 turn。
// 运行中切换：set-model / set-effort / set-fast-mode / set-thinking-enabled / set-permission-mode
// （对齐 desktop register.ts，空闲时热切；回合进行中登记、回合结束再应用，返回 deferred:true）。
// 未覆盖（保持 [NOT_IMPLEMENTED]）：中断恢复、队列合并/steer、device-link、Orca、IM、伙伴、自动续跑、
// 跨引擎切换（switch-session-agent）、Claude Code / Codex。
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
const PERMISSION_MODES = new Set(['ask', 'default', 'acceptEdits', 'plan', 'auto', 'bypassPermissions']);

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
        selection: null, // 最近一次 set-model 的 { model, providerId }；重建会话时优先于入队项里的旧 createOpts
        pendingSelection: null, // 回合进行中登记的 set-model，回合结束后应用
        resumeAfterRebuild: null, // 热切失败、关掉重建时保留的 Pi 会话文件，下次懒创建时续上历史
        switching: null, // 正在进行的运行时切换；派发新回合前等它结束
        liveRoute: null, // 运行中 Pi 进程实际用的 { model, providerId }
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
    // persist.mjs：ask_user / plan_review 的 pending 行回写最终状态（permission 不建行，返回 null）。
    try {
      db.persistInteractionResolved?.(entry.sessionId, requestId, decision);
    } catch (err) {
      process.stderr.write(`[maker-ipc] persist interaction resolved failed: ${err?.message ?? err}\n`);
    }
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
      const entry = { sessionId: session.id, kind: req.kind, request: req, resolve, timer: null, persistId: undefined };
      // 先登记再广播，避免 renderer 回得太快时找不到 resolver。
      pending.set(req.requestId, entry);
      if (req.kind === 'permission') {
        entry.timer = setTimeout(() => settleInteraction(req.requestId, defaultDecision(req.kind, 'timeout'), 'timeout'), PERMISSION_TIMEOUT_MS);
        entry.timer.unref?.();
      }
      signal?.addEventListener('abort', () => settleInteraction(req.requestId, defaultDecision(req.kind, 'session_aborted'), 'session_aborted'), { once: true });
      let persistId;
      try {
        persistId = db.persistInteractionRequest?.(session.id, req) ?? undefined;
      } catch (err) {
        process.stderr.write(`[maker-ipc] persist interaction request failed: ${err?.message ?? err}\n`);
      }
      entry.persistId = persistId;
      emitToRenderer('maker:interaction-request', { sessionId: session.id, request: req, persistId });
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
    syncSdkSessionId(sessionId);
    pushProjection(sessionId);
    if (s.pendingSelection) {
      const sel = s.pendingSelection;
      s.pendingSelection = null;
      runSwitch(sessionId, () => applySelection(sessionId, sel)).catch((err) => {
        process.stderr.write(`[maker-ipc] deferred set-model failed: ${err?.message ?? err}\n`);
      });
    }
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
    const s0 = getState(id);
    const selected = s0.selection;
    const task = (async () => {
      // 重启后重建：renderer 的 createOpts 是建会话那刻的快照，常常不带 resumeSessionId，工作目录也可能是旧值。
      // 续接以 maker-core 存下的 Pi 会话文件为准（其次是库里的 sdk_session_id），工作目录以库里记的为准。
      const savedRow = readSessionRow(id);
      const savedMeta = await maker.getSessionMeta?.(id).catch(() => null) ?? null;
      const resumeSessionId = (typeof raw.resumeSessionId === 'string' && raw.resumeSessionId)
        || s0.resumeAfterRebuild
        || (typeof savedMeta?.sdkSessionId === 'string' && savedMeta.sdkSessionId)
        || (typeof savedRow?.sdkSessionId === 'string' && savedRow.sdkSessionId)
        || undefined;
      const savedWorkingDir = (typeof savedRow?.workingDir === 'string' && savedRow.workingDir)
        || (typeof savedMeta?.workDir === 'string' && savedMeta.workDir)
        || null;
      await host.whenModelsReady?.();
      const cfg = selected
        ? host.resolveSessionModel(selected.providerId, selected.model)
        : host.resolveSessionModel(raw.providerId ?? null, raw.model);
      await host.ensurePiInstalled();
      const workingDir = savedWorkingDir
        ?? (typeof raw.workingDir === 'string' && raw.workingDir.trim() ? raw.workingDir : dialogueDir(dataDir, id));
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
        ...(resumeSessionId ? { resumeSessionId } : {}),
        ...(raw.workspaceKind === 'project' || raw.workspaceKind === 'dialogue' ? { workspaceKind: raw.workspaceKind } : {}),
        vendorOptions: {
          ...(raw.vendorOptions && typeof raw.vendorOptions === 'object' && !Array.isArray(raw.vendorOptions) ? raw.vendorOptions : {}),
          onStderrLine: (line) => { if (line.trim()) process.stderr.write(`[pi:${id}] ${line}\n`); },
        },
      };
      const session = await maker.createSession(opts);
      wireSession(session);
      const s = getState(id);
      if (s.resumeAfterRebuild === resumeSessionId) s.resumeAfterRebuild = null;
      s.liveRoute = { model: cfg.modelId, providerId: cfg.providerId };
      s.permissionMode = raw.permissionMode;
      if (resumeSessionId) process.stderr.write(`[maker-ipc] session ${id} resumed from saved Pi history\n`);
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
      if (s.switching) await s.switching.catch(() => undefined);
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

  // ── 运行中切换 ──────────────────────────────────────────────────────────
  // 同一 session 的切换串行执行；pump 派发新回合前会等 s.switching。
  function runSwitch(sessionId, fn) {
    const s = getState(sessionId);
    const run = (s.switching ?? Promise.resolve()).catch(() => undefined).then(fn);
    const tail = run.finally(() => { if (s.switching === tail) s.switching = null; });
    s.switching = tail;
    return run;
  }

  const inTurn = (sessionId) => getState(sessionId).activeTurn !== null || (maker.getSession(sessionId)?.isTurnRunning() ?? false);

  function readSessionRow(sessionId) {
    try {
      return db.getSession(sessionId) ?? null;
    } catch {
      return null;
    }
  }

  // Pi 会话文件路径落到 renderer 表的 sdk_session_id（desktop 同口径），变了才写并推 sessions:patched。
  function syncSdkSessionId(sessionId) {
    const sdk = maker.getSession(sessionId)?.sdkSessionId;
    if (typeof sdk !== 'string' || !sdk) return;
    const row = readSessionRow(sessionId);
    if (!row || row.sdkSessionId === sdk) return;
    try {
      db._internal.updateSession(sessionId, { sdkSessionId: sdk });
    } catch (err) {
      process.stderr.write(`[maker-ipc] persist sdkSessionId failed: ${err?.message ?? err}\n`);
    }
  }

  // 落库并推 local-db:sessions:patched；内存里的 s.row 同步改，避免回合结束的 upsertRow 写回旧模型。
  function persistSessionPatch(sessionId, patch) {
    const s = getState(sessionId);
    if (s.row) {
      if (patch.model !== undefined) s.row.model = patch.model;
      if (patch.providerId !== undefined) s.row.providerId = patch.providerId;
      if (patch.permissionMode !== undefined) s.row.permissionMode = patch.permissionMode;
    }
    if (!readSessionRow(sessionId)) return;
    db._internal.updateSession(sessionId, patch);
  }

  function modelEfforts(modelId) {
    const d = typeof models?.getPiModelDescriptors === 'function'
      ? models.getPiModelDescriptors().find((m) => m.id === modelId)
      : null;
    return Array.isArray(d?.efforts) ? d.efforts : [];
  }

  // 把 (providerId, model) 落到已配置的 provider；model-only 切换沿用会话当前来源。
  function resolveTarget(sessionId, model, providerId) {
    const s = getState(sessionId);
    const current = s.selection?.providerId ?? readSessionRow(sessionId)?.providerId ?? s.row?.providerId ?? null;
    const pid = typeof providerId === 'string' && providerId ? providerId : current;
    const cfg = host.resolveSessionModel(pid, model);
    if (cfg.modelId !== model) fail('MODEL_NOT_CONFIGURED', `model ${model} is not configured`);
    return cfg;
  }

  // 空闲 session 上热切；Pi 判定需要换启动参数（窗口 / 压缩设置变了）时关掉进程，
  // 下一条消息按新模型懒创建，并用原 Pi 会话文件续上历史。
  async function applySelection(sessionId, sel) {
    const session = maker.getSession(sessionId);
    if (!session || session.getStatus() === 'closed') return;
    const s = getState(sessionId);
    const route = s.liveRoute;
    const sameRoute = route?.model === sel.model && route?.providerId === sel.providerId;
    if (sameRoute && sel.effort === undefined && sel.thinking === undefined) return;
    try {
      await session.setModel(sel.model, {
        providerId: sel.providerId,
        ...(sel.effort !== undefined ? { effort: sel.effort } : {}),
        ...(sel.thinking !== undefined ? { thinkingEnabled: sel.thinking } : {}),
      });
      s.liveRoute = { model: sel.model, providerId: sel.providerId };
    } catch (err) {
      const sdk = session.sdkSessionId;
      process.stderr.write(`[maker-ipc] live set-model failed, rebuilding on next send: ${err?.message ?? err}\n`);
      s.liveRoute = null;
      if (sdk && sdk !== '<pending>') s.resumeAfterRebuild = sdk;
      cancelInteractionsForSession(sessionId, 'session_closed');
      await maker.closeSession(sessionId, 'runtime-refresh');
    }
  }

  async function setModel(sessionId, model, providerId, expectedRevision, selection) {
    const sid = requireSessionId(sessionId);
    if (typeof model !== 'string' || !model) fail('INVALID_PARAMS', 'model required');
    if (providerId !== undefined && providerId !== null && typeof providerId !== 'string') fail('INVALID_PARAMS', 'invalid providerId');
    if (expectedRevision !== undefined && expectedRevision !== null && !(Number.isSafeInteger(expectedRevision) && expectedRevision >= 0)) {
      fail('INVALID_PARAMS', 'invalid expectedAgentSwitchRevision');
    }
    if (selection !== undefined && selection !== null && (typeof selection !== 'object' || Array.isArray(selection))) {
      fail('INVALID_PARAMS', 'invalid selection');
    }
    await host.whenModelsReady?.();
    const cfg = resolveTarget(sid, model, providerId);
    const efforts = modelEfforts(cfg.modelId);
    const effort = typeof selection?.effort === 'string' && efforts.includes(selection.effort) ? selection.effort : undefined;
    const thinking = typeof selection?.thinking === 'boolean' && efforts.length > 0 ? selection.thinking : undefined;
    const fastMode = typeof selection?.fastMode === 'boolean' ? selection.fastMode : undefined;
    const sel = { model: cfg.modelId, providerId: cfg.providerId, effort, thinking };
    const s = getState(sid);
    s.selection = { model: sel.model, providerId: sel.providerId };
    persistSessionPatch(sid, {
      model: sel.model,
      providerId: sel.providerId,
      ...(effort !== undefined ? { effort } : {}),
      ...(fastMode !== undefined ? { fastMode } : {}),
    });
    if (inTurn(sid)) {
      s.pendingSelection = sel;
      return { deferred: true, superseded: false };
    }
    s.pendingSelection = null;
    await runSwitch(sid, async () => {
      await applySelection(sid, sel);
      if (fastMode !== undefined) await maker.getSession(sid)?.setFastMode(fastMode).catch(() => undefined);
    });
    return { deferred: false };
  }

  // ── 通道 ────────────────────────────────────────────────────────────────
  function unsupportedCapabilities() {
    const no = { supported: false, reason: 'not-implemented', message: 'Only Pi is available in the Tauri build' };
    return {
      switchModel: no, availableModels: [], hasFastMode: false, effort: no, effortLevels: [], reasoningDisplay: ['off'],
      permissionModes: [], setPermissionModeMidSession: no, planMode: no,
      multimodal: { text: no, image: no, file: no },
      fork: no, rewind: no, abort: no, sameTurnSteer: no, memory: { supported: no }, extraDirs: no,
      supportsSessionAgentSwitch: false, supportsModelWindowSwitchGuard: false,
    };
  }
  registerInvoke('maker:list-available-agents', async () => maker.listAvailableAgents());

  registerInvoke('maker:get-capabilities', async (agentKind) => {
    // renderer 的 loadLocalCapabilitiesSnapshot 并行查 claude-code/codex/pi，任一抛错整份快照作废
    // （模型选择器显示 "Model Details Unavailable"）。Tauri 版只注册 Pi，其余引擎返回全不支持的合法能力。
    if (agentKind === 'claude-code' || agentKind === 'codex') return unsupportedCapabilities();
    if (agentKind !== 'pi') fail('INVALID_PARAMS', `unknown agentKind ${String(agentKind)}`);
    // desktop register.ts GET_CAPABILITIES：agent 能力 + host 级标志位；可选模型取自已连接的自定义 provider。
    const caps = maker.getCapabilities('pi');
    await host.whenModelsReady?.();
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

  registerInvoke('maker:set-model', (sessionId, model, providerId, expectedRevision, selection) =>
    setModel(sessionId, model, providerId, expectedRevision, selection));

  registerInvoke('maker:set-effort', async (sessionId, effort) => {
    const sid = requireSessionId(sessionId);
    if (typeof effort !== 'string' || !effort) fail('INVALID_PARAMS', 'sessionId + effort required');
    persistSessionPatch(sid, { effort });
    const s = getState(sid);
    if (s.pendingSelection) { s.pendingSelection.effort = effort; return undefined; }
    const session = maker.getSession(sid);
    if (!session || session.getStatus() === 'closed') return undefined;
    // 无档位模型的占位 effort 不下发（Pi 会拒绝不在目录里的档位）。
    if (!modelEfforts(session.model).includes(effort)) return undefined;
    try {
      await runSwitch(sid, () => session.setEffort(effort));
    } catch (err) {
      process.stderr.write(`[maker-ipc] set-effort failed: ${err?.message ?? err}\n`);
      fail('INTERNAL', 'Runtime effort update failed');
    }
    return undefined;
  });

  registerInvoke('maker:set-fast-mode', async (sessionId, enabled) => {
    const sid = requireSessionId(sessionId);
    if (typeof enabled !== 'boolean' && enabled !== 'ultrafast') fail('INVALID_PARAMS', 'sessionId + enabled required');
    if (enabled === 'ultrafast') fail('NOT_SUPPORTED', 'Ultrafast requires the Codex runtime');
    persistSessionPatch(sid, { fastMode: enabled });
    const session = maker.getSession(sid);
    if (session && session.getStatus() !== 'closed') await session.setFastMode(enabled);
    return undefined;
  });

  registerInvoke('maker:set-thinking-enabled', async (sessionId, enabled) => {
    const sid = requireSessionId(sessionId);
    if (typeof enabled !== 'boolean') fail('INVALID_PARAMS', 'sessionId + enabled required');
    const s = getState(sid);
    if (s.pendingSelection) { s.pendingSelection.thinking = enabled; return undefined; }
    const session = maker.getSession(sid);
    if (!session || session.getStatus() === 'closed' || modelEfforts(session.model).length === 0) return undefined;
    await runSwitch(sid, () => session.setThinkingEnabled(enabled));
    return undefined;
  });

  registerInvoke('maker:set-permission-mode', async (sessionId, mode) => {
    const sid = requireSessionId(sessionId);
    if (typeof mode !== 'string' || !PERMISSION_MODES.has(mode)) fail('INVALID_PARAMS', 'invalid permission mode');
    persistSessionPatch(sid, { permissionMode: mode });
    const session = maker.getSession(sid);
    if (session && session.getStatus() !== 'closed') await applyPermissionMode(session, mode);
    return undefined;
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

  // 待处理的审批 / 提问快照（desktop GET_PENDING_INTERACTIONS）：手机打开会话、重连时据此重建审批面板。
  registerInvoke('maker:get-pending-interactions', async (sessionId) => {
    if (typeof sessionId !== 'string' || !sessionId) return [];
    return [...pending.values()]
      .filter((entry) => entry.sessionId === sessionId)
      .map((entry) => ({ request: entry.request, ...(entry.persistId ? { persistId: entry.persistId } : {}) }));
  });

  // 直接中止当前回合（desktop ABORT_SESSION）：不动排队，取消待审批。
  registerInvoke('maker:abort-session', async (sessionId) => {
    const sid = requireSessionId(sessionId);
    const s = getState(sid);
    cancelInteractionsForSession(sid, 'session_aborted');
    const session = maker.getSession(sid);
    if (session && s.activeTurn) await session.abort().catch(() => undefined);
    if (s.activeTurn) s.activeTurn = null;
    pushProjection(sid);
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

  // 对齐 desktop anySessionInTurn：协调器记下的 activeTurn，加上 live session 自报的 isTurnRunning()。
  const anySessionInTurn = () => {
    for (const s of states.values()) if (s.activeTurn) return true;
    return maker.listActiveSessions().some((s) => s.isTurnRunning());
  };
  registerInvoke('maker:any-session-in-turn', async () => anySessionInTurn());

  return {
    host,
    anySessionInTurn,
    isTurnRunning: (sid) => getState(sid).activeTurn !== null || (maker.getSession(sid)?.isTurnRunning() ?? false),
    shutdown: () => host.shutdown(),
  };
}
