// 冒烟模式下追加到 preload bundle 末尾：等真实 renderer 渲染出界面后，
// 汇总第一阶段结果、界面摘要和未实现的 IPC，写结果并退出。
// boot.sessionSmoke 为 true 时，首屏通过后再经 window.electronAPI 跑会话冒烟（见 runSessionSmoke）。
const BOOT_TIMEOUT_MS = 90_000;
// 会话冒烟每一步的超时。
const STEP_TIMEOUT_MS = 60_000;
// 这些 scope 的 error 说明首屏没进到可用状态（路由崩溃、启动闸失败）。
const FATAL_SCOPES = /error-fallback|ErrorBoundary|Gate|EnvCheck/i;
// 冒烟会话标题，verify 阶段据此辅助识别。
const SMOKE_TITLE = 'cindy-tauri session smoke';
// 触发 mock 模型返回写文件工具调用的标记（见 scripts/mock-openai.mjs）。
const MOCK_TRIGGER = 'MOCK_WRITE_FILE';

export function installRendererSmoke() {
  const boot = window.__CINDY_BOOT__ ?? {};
  if (!boot.smoke || location.pathname.startsWith('/__smoke/')) return;
  const invoke = (cmd, args) => window.__TAURI_INTERNALS__.invoke(cmd, args);
  const errors = [];
  window.addEventListener('error', (e) => errors.push(String(e.message ?? e)));
  window.addEventListener('unhandledrejection', (e) => errors.push(String(e.reason?.message ?? e.reason)));

  const started = Date.now();
  const finish = async (rendered, renderer) => {
    let stats = null;
    try {
      stats = await invoke('sidecar_call', { method: 'ipc.stats', params: {} });
    } catch (err) {
      stats = { error: String(err) };
    }
    const fatal = (stats?.rendererErrors ?? []).filter((e) => FATAL_SCOPES.test(e.scope));
    // 未实现通道的 rejection 是预期内的（数量见 ipc.missing），其余错误都算失败。
    const unexpected = errors.filter((e) => !e.includes('[NOT_IMPLEMENTED]'));
    let ok = rendered && unexpected.length === 0 && fatal.length === 0 && !stats?.error;
    let sessionSmoke;
    if (boot.sessionSmoke) {
      if (ok) {
        sessionSmoke = await runSessionSmoke(boot.sessionSmokePhase, invoke);
      } else {
        sessionSmoke = { phase: boot.sessionSmokePhase, ok: false, skipped: 'first screen check failed' };
      }
      ok = ok && sessionSmoke.ok;
    }
    const detail = JSON.stringify({
      phase1: sessionStorage.getItem('cindySmokePhase1'),
      renderer,
      errors: unexpected.slice(0, 20),
      notImplementedRejections: errors.length - unexpected.length,
      fatalRendererErrors: fatal.slice(0, 10),
      ipc: stats,
      ...(sessionSmoke ? { sessionSmoke } : {}),
    });
    await invoke('smoke_finish', { ok, detail });
  };

  // 根节点有内容且连续 3 秒不再变化，视为首屏渲染完成。
  let lastSize = -1;
  let stableSince = 0;
  const timer = setInterval(() => {
    const root = document.getElementById('root');
    const size = root ? root.getElementsByTagName('*').length : 0;
    if (size !== lastSize) {
      lastSize = size;
      stableSince = Date.now();
    }
    const elapsed = Date.now() - started;
    const stable = size > 0 && Date.now() - stableSince >= 3000;
    if (stable || elapsed > BOOT_TIMEOUT_MS) {
      clearInterval(timer);
      const text = (root?.innerText ?? '').replace(/\s+/g, ' ').trim().slice(0, 300);
      void finish(size > 0, { elements: size, ms: elapsed, text });
    }
  }, 500);
}

// ---------- 会话冒烟 ----------

const errText = (err) => String(err?.message ?? err).slice(0, 500);

function withTimeout(promise, label, ms = STEP_TIMEOUT_MS) {
  let t;
  return Promise.race([
    promise,
    new Promise((_, reject) => { t = setTimeout(() => reject(new Error(`${label}: timeout after ${ms}ms`)), ms); }),
  ]).finally(() => clearTimeout(t));
}

// 轮询等待条件成立，返回条件函数的真值结果；超时后停止轮询。
function waitFor(check, label, ms = STEP_TIMEOUT_MS) {
  return new Promise((resolve, reject) => {
    const deadline = Date.now() + ms;
    const tick = () => {
      const v = check();
      if (v) resolve(v);
      else if (Date.now() >= deadline) reject(new Error(`${label}: timeout after ${ms}ms`));
      else setTimeout(tick, 100);
    };
    tick();
  });
}

// 依次执行步骤，把每步结果记到 steps；任一步失败即停止并 ok=false。
function stepRunner(result) {
  return async (name, fn) => {
    const t0 = Date.now();
    try {
      const value = await withTimeout(Promise.resolve().then(fn), name);
      result.steps.push({ name, ok: true, ms: Date.now() - t0, ...(value && typeof value === 'object' ? { info: value } : {}) });
      return value;
    } catch (err) {
      result.steps.push({ name, ok: false, ms: Date.now() - t0, error: errText(err) });
      throw err;
    }
  };
}

async function runSessionSmoke(phase, invoke) {
  const result = { phase, ok: false, steps: [] };
  const api = window.electronAPI;
  if (!api?.maker || !api?.localDb) {
    result.error = 'window.electronAPI.maker/localDb missing';
    return result;
  }
  try {
    if (phase === 'run') await sessionRun(api, invoke, result);
    else if (phase === 'verify') await sessionVerify(api, result);
    else throw new Error(`unknown sessionSmokePhase: ${phase}`);
    result.ok = true;
  } catch (err) {
    result.error = errText(err);
  }
  return result;
}

// 对齐 renderer makerChatStore.buildQueuedMessage 的 AgentInputQueuedMessage 形状（无附件、无引用）。
function buildQueued(text, { model, permissionMode, workingDir, providerId }) {
  const clientId = crypto.randomUUID();
  const createdAt = new Date().toISOString();
  const createOpts = {
    agentKind: 'pi',
    workingDir,
    model,
    effort: '',
    permissionMode,
    planMode: false,
    displayReasoning: 'summarized',
    ...(providerId ? { providerId } : {}),
  };
  return {
    clientId,
    text,
    persistedContent: JSON.stringify({ text, images: [], files: [], slashCommandRanges: [] }),
    model,
    effort: '',
    permissionMode,
    workingDir,
    chatMessage: { clientId, role: 'user', content: text, isStreaming: false, createdAt },
    createOpts,
  };
}

// run 阶段：建 Pi 会话（Ask 模式）→ 发普通消息看流式回复 → 发 MOCK_WRITE_FILE，批准工具调用 → 等回合结束。
async function sessionRun(api, invoke, result) {
  const step = stepRunner(result);
  const info = await invoke('smoke_info').catch(() => null);
  const model = info?.modelId || 'mock-model';
  const permissionMode = 'ask'; // Pi capabilities 里 Ask（Default permissions）对应的 id

  // 选中冒烟 provider：优先名为 Smoke 的，其次包含该模型的；拿不到就交给后端默认路由。
  const providerId = await step('provider', async () => {
    const list = await api.maker.listProviders();
    const providers = Array.isArray(list?.providers) ? list.providers : [];
    const hasModel = (p) => JSON.stringify(p.models ?? p.modelIds ?? []).includes(model);
    const p = providers.find((x) => /smoke/i.test(x.name ?? x.id ?? '')) ?? providers.find(hasModel);
    if (!p) throw new Error(`no provider for ${model} among [${providers.map((x) => x.id).join(', ')}]`);
    return { providerId: p.id, name: p.name ?? null };
  }).then((v) => v.providerId);
  result.providerId = providerId;
  result.model = model;

  // 和 NewMakerDraftRoute 一样先在本地库建会话行，再起 maker 会话。
  const row = await step('localDb.sessions.create', async () => {
    const s = await api.localDb.sessions.create({
      agentKind: 'pi', title: SMOKE_TITLE, model, effort: '', permissionMode,
      workspaceKind: 'dialogue', providerId,
    });
    if (!s?.id) throw new Error(`unexpected create result: ${JSON.stringify(s)?.slice(0, 200)}`);
    return { id: s.id, workingDir: s.workingDir ?? null };
  });
  const sessionId = row.id;
  result.sessionId = sessionId;

  // 先订阅推送，避免错过早到的事件。
  const events = [];
  const interactions = [];
  const unsubs = [];
  unsubs.push(api.maker.onEvent((payload) => {
    if (payload?.sessionId === sessionId && payload.event) events.push(payload.event);
  }));
  unsubs.push(api.maker.onInteractionRequest((payload) => {
    if (payload?.sessionId === sessionId && payload.request) interactions.push(payload.request);
  }));

  try {
    const created = await step('maker.createSession', async () => {
      const r = await api.maker.createSession({
        id: sessionId, agentKind: 'pi', workingDir: row.workingDir ?? '', model, title: SMOKE_TITLE,
        effort: '', permissionMode, planMode: false, displayReasoning: 'summarized', providerId,
      });
      if (r?.sessionId !== sessionId) throw new Error(`sessionId mismatch: ${r?.sessionId}`);
      return { workDir: r.workDir, agentKind: r.agentKind };
    });
    const workingDir = created.workDir || row.workingDir || '';
    result.workDir = workingDir;
    const ctx = { model, permissionMode, workingDir, providerId };

    // 发送：renderer 实际走 maker.input.enqueue；sendAtMs 表示立即派发。
    const send = async (text) => {
      const projection = await api.maker.input.enqueue(sessionId, buildQueued(text, ctx), { sendAtMs: Date.now() });
      return { turnInProgress: projection?.turnInProgress ?? null, pending: projection?.pendingQueue?.length ?? null, error: projection?.error ?? null };
    };
    const isTerminal = (e) => e.type === 'done'
      || (e.type === 'error' && e.data?.isTerminal !== false && e.data?.willRetry !== true);
    const waitTurnEnd = (from, label) => waitFor(() => events.slice(from).find(isTerminal), label);
    const textDeltas = (from) => events.slice(from).filter((e) => e.type === 'text' && e.data?.isFinal === false);
    const fullText = (from) => {
      const final = events.slice(from).filter((e) => e.type === 'text' && e.data?.isFinal).pop();
      return final ? String(final.data.text ?? '') : textDeltas(from).map((e) => e.data.text ?? '').join('');
    };

    // 第一轮：普通消息，检查流式 delta 和最终文本。
    let mark = events.length;
    await step('send hello', () => send('Hello'));
    await step('turn 1 streamed reply', async () => {
      const end = await waitTurnEnd(mark, 'turn 1 end');
      if (end.type === 'error') throw new Error(`turn error: ${JSON.stringify(end.data).slice(0, 300)}`);
      const deltas = textDeltas(mark).length;
      const text = fullText(mark);
      if (deltas < 2) throw new Error(`expected >=2 text deltas, got ${deltas}`);
      if (!text.includes('Hello from mock.')) throw new Error(`unexpected reply: ${JSON.stringify(text)}`);
      return { deltas, text };
    });

    // 第二轮：触发写文件工具调用，Ask 模式下应弹审批，允许后等工具结果和 done。
    mark = events.length;
    const askMark = interactions.length;
    await step('send tool request', () => send(`Please ${MOCK_TRIGGER} now`));
    const request = await step('interaction request', async () => {
      const req = await waitFor(() => interactions.slice(askMark).find((r) => r.kind === 'permission'), 'permission request');
      return { requestId: req.requestId, toolName: req.toolName ?? null };
    });
    await step('resolve-interaction allow', async () => {
      const r = await api.maker.resolveInteraction(request.requestId, { kind: 'permission', behavior: 'allow' });
      if (r?.accepted !== true) throw new Error(`not accepted: ${JSON.stringify(r)}`);
      return { accepted: true };
    });
    await step('turn 2 tool result', async () => {
      const end = await waitTurnEnd(mark, 'turn 2 end');
      if (end.type === 'error') throw new Error(`turn error: ${JSON.stringify(end.data).slice(0, 300)}`);
      const toolUses = events.slice(mark).filter((e) => e.type === 'tool_use').length;
      const toolResults = events.slice(mark).filter((e) => e.type === 'tool_result');
      const okResult = toolResults.find((e) => e.data?.summary !== 'failed');
      if (!okResult) throw new Error(`no successful tool_result (tool_use=${toolUses}, tool_result=${toolResults.length})`);
      const text = fullText(mark);
      if (!text.includes('done')) throw new Error(`unexpected final text: ${JSON.stringify(text)}`);
      return { toolUses, toolResults: toolResults.length, text };
    });
  } finally {
    result.eventCounts = events.reduce((acc, e) => ({ ...acc, [e.type]: (acc[e.type] ?? 0) + 1 }), {});
    result.interactionCount = interactions.length;
    for (const u of unsubs) { try { u?.(); } catch { /* 忽略 */ } }
  }

  // 关闭会话让 Pi 进程退出；失败不影响结论。
  try {
    await withTimeout(api.maker.closeSession(sessionId), 'closeSession', 15_000);
    result.closed = true;
  } catch (err) {
    result.closed = errText(err);
  }
}

// verify 阶段：重启后会话列表里应有上次 run 的会话（scripts/smoke.mjs 再按 sessionId 交叉核对）。
async function sessionVerify(api, result) {
  const step = stepRunner(result);
  const found = await step('localDb.sessions.list', async () => {
    const list = await api.localDb.sessions.list(100);
    if (!Array.isArray(list)) throw new Error(`unexpected list result: ${JSON.stringify(list)?.slice(0, 200)}`);
    result.sessionIds = list.map((s) => s.id);
    const smoke = list.filter((s) => s.title === SMOKE_TITLE || s.agentKind === 'pi');
    if (smoke.length === 0) throw new Error(`no smoke session among ${list.length} sessions`);
    return { total: list.length, smokeSessions: smoke.map((s) => ({ id: s.id, title: s.title, agentKind: s.agentKind })) };
  });
  // 历史消息：能读到就记录条数，读不到只记信息不判失败（历史渲染接口不在本里程碑出口内）。
  const id = found.smokeSessions[0].id;
  try {
    const view = await withTimeout(api.localDb.messages.list(id, { limit: 50 }), 'messages.list', 15_000);
    const rows = Array.isArray(view) ? view : view?.messages ?? view?.items ?? [];
    result.messages = { sessionId: id, count: rows.length, roles: rows.map((m) => m.role).slice(0, 20) };
  } catch (err) {
    result.messages = { sessionId: id, error: errText(err) };
  }
}
