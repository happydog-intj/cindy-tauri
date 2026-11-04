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
    let settingsTour;
    if (boot.settingsSmoke && ok) {
      settingsTour = await runSettingsTour(invoke, errors);
      ok = ok && settingsTour.ok;
    }
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
      ...(settingsTour ? { settingsTour } : {}),
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

// ---------- 设置页巡检 ----------
// 依次打开设置页各分区，记录每个分区新出现的未实现通道、渲染层错误日志和错误边界文案。
// 未实现通道只记录不算失败；出现错误边界或非 NOT_IMPLEMENTED 的页面错误算失败。
const SETTINGS_TABS = ['general', 'appearance', 'personalization', 'providers', 'billing', 'usage', 'voice-input', 'im-bot',
  'shortcuts', 'agent-island', 'import', 'remote-control', 'shared-tasks', 'ghosts', 'builtin-tools', 'computer-use',
  'storage', 'help', 'about'];
const ERROR_BOUNDARY_TEXT = /界面出错了|This view failed to render/;

async function waitStable(ms = 1500, max = 8000) {
  const t0 = Date.now();
  let last = -1;
  let since = Date.now();
  while (Date.now() - t0 < max) {
    const n = document.getElementById('root')?.getElementsByTagName('*').length ?? 0;
    if (n !== last) { last = n; since = Date.now(); }
    if (Date.now() - since >= ms) return n;
    await new Promise((r) => setTimeout(r, 200));
  }
  return last;
}

async function runSettingsTour(invoke, errors) {
  const tabs = [];
  let ok = true;
  const stats = async () => invoke('sidecar_call', { method: 'ipc.stats', params: {} }).catch(() => null);
  let before = await stats();
  for (const tab of SETTINGS_TABS) {
    const errCount = errors.length;
    const logCount = before?.rendererErrors?.length ?? 0;
    location.hash = `#/settings?tab=${tab}`;
    const elements = await waitStable();
    const after = await stats();
    const missing = {};
    for (const kind of ['invoke', 'send', 'sendSync']) {
      for (const [ch, n] of Object.entries(after?.missing?.[kind] ?? {})) {
        if (n > (before?.missing?.[kind]?.[ch] ?? 0)) missing[ch] = kind;
      }
    }
    const pageErrors = errors.slice(errCount).filter((e) => !e.includes('[NOT_IMPLEMENTED]'));
    const logs = (after?.rendererErrors ?? []).slice(logCount).map((e) => `${e.scope}: ${e.msg}`.slice(0, 200));
    const text = document.getElementById('root')?.innerText ?? '';
    const boundary = ERROR_BOUNDARY_TEXT.test(text);
    const tabOk = !boundary && pageErrors.length === 0 && elements > 0;
    ok = ok && tabOk;
    tabs.push({ tab, ok: tabOk, elements, missing: Object.keys(missing), ...(pageErrors.length ? { pageErrors } : {}), ...(logs.length ? { logs } : {}), ...(boundary ? { boundary: true } : {}) });
    before = after;
  }
  location.hash = '#/';
  await waitStable(1000, 4000);
  return { ok, tabs };
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
    else if (phase === 'verify') await sessionVerify(api, invoke, result);
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

  if (info?.apiKey) await keyedProviderRun(api, info, model, result);
}

const KEYED_ID = 'smoke-keyed';
// 带 key 的 mock 路径（scripts/mock-openai.mjs 的 /keyed/... 要求 Bearer）。
const keyedBaseUrl = (info) => `${String(info.modelBaseUrl).replace(/\/+$/, '')}/keyed`;

async function waitKeyedConnected(api) {
  return waitFor(async () => {
    const list = await api.maker.listProviders();
    const p = list?.providers?.find((x) => x.id === KEYED_ID);
    return p?.connected === true ? p : null;
  }, 'keyed provider connected', 20_000);
}

// 带 key 的 provider：按设置页的调用顺序 models-fetch → createCustomProvider → 测试连接 → 跑一轮会话。
// 结果里只记布尔值和状态码，不记 key。
async function keyedProviderRun(api, info, model, result) {
  const step = stepRunner(result);
  const baseUrl = keyedBaseUrl(info);
  await step('keyed models-fetch', async () => {
    const r = await api.maker.fetchProviderModels({ agent: 'pi', baseUrl, authMethod: 'apiKey', apiKey: info.apiKey });
    if (!r?.ok) throw new Error(`models-fetch failed: ${r?.code ?? 'unknown'}`);
    return { models: r.models.map((m) => m.id) };
  });
  await step('keyed createCustomProvider', async () => {
    const r = await api.maker.createCustomProvider({
      id: KEYED_ID, name: 'Smoke (API key)', auth: { method: 'apiKey' },
      runtimes: { pi: { baseUrl, wireProtocol: 'openai-chat', models: [{ id: model, name: `${model} (keyed)` }] } },
    }, { pi: info.apiKey }, {});
    if (r?.ok !== true) throw new Error(`create failed: ${JSON.stringify(r)?.slice(0, 200)}`);
    return { ok: true };
  });
  await step('keyed provider connected', async () => ({ connected: (await waitKeyedConnected(api)).connected }));
  await step('keyed test-connection (saved)', async () => {
    const r = await api.maker.testProviderConnection({ kind: 'saved', providerId: KEYED_ID, agent: 'pi' });
    if (r?.ok !== true) throw new Error(`test-connection failed: ${r?.code ?? r?.decision}`);
    return { status: r.status };
  });
  await step('keyed Pi turn', async () => {
    const s = await api.localDb.sessions.create({
      agentKind: 'pi', title: `${SMOKE_TITLE} (keyed)`, model, effort: '', permissionMode: 'ask', workspaceKind: 'dialogue', providerId: KEYED_ID,
    });
    const events = [];
    const unsub = api.maker.onEvent((p) => { if (p?.sessionId === s.id && p.event) events.push(p.event); });
    try {
      const created = await api.maker.createSession({
        id: s.id, agentKind: 'pi', workingDir: s.workingDir ?? '', model, title: s.title,
        effort: '', permissionMode: 'ask', planMode: false, displayReasoning: 'summarized', providerId: KEYED_ID,
      });
      const ctx = { model, permissionMode: 'ask', workingDir: created.workDir || s.workingDir || '', providerId: KEYED_ID };
      await api.maker.input.enqueue(s.id, buildQueued('Hello', ctx), { sendAtMs: Date.now() });
      const end = await waitFor(() => events.find((e) => e.type === 'done' || (e.type === 'error' && e.data?.willRetry !== true)), 'keyed turn end');
      if (end.type === 'error') throw new Error(`turn error: ${JSON.stringify(end.data).slice(0, 300)}`);
      const text = events.filter((e) => e.type === 'text').map((e) => e.data?.text ?? '').join('');
      if (!text.includes('Hello from mock.')) throw new Error('unexpected keyed reply');
      return { sessionId: s.id };
    } finally {
      try { unsub?.(); } catch { /* 忽略 */ }
      await withTimeout(api.maker.closeSession(s.id), 'closeSession', 15_000).catch(() => {});
    }
  });
}

// verify 阶段：重启后主密钥要从系统钥匙串重新取出，已存 key 仍可解密并用于鉴权；最后断开连接。
async function keyedProviderVerify(api, result) {
  const step = stepRunner(result);
  await step('keyed provider connected after restart', async () => ({ connected: (await waitKeyedConnected(api)).connected }));
  await step('keyed test-connection after restart', async () => {
    const r = await api.maker.testProviderConnection({ kind: 'saved', providerId: KEYED_ID, agent: 'pi' });
    if (r?.ok !== true) throw new Error(`test-connection failed: ${r?.code ?? r?.decision}`);
    return { status: r.status };
  });
  await step('keyed disconnect', async () => {
    const r = await api.maker.disconnectCustomProvider(KEYED_ID, { dataOwnerId: 'local-v1', ownerGeneration: 1 }, {});
    if (r?.ok !== true) throw new Error(`disconnect failed: ${JSON.stringify(r)?.slice(0, 200)}`);
    const list = await api.maker.listProviders();
    const p = list?.providers?.find((x) => x.id === KEYED_ID);
    if (!p || p.connected !== false) throw new Error('provider still connected or missing after disconnect');
    if ((await api.safeStorageRead(`provider_key_${KEYED_ID}_pi`)) !== null) throw new Error('key still stored after disconnect');
    return { connected: false };
  });
}

// verify 阶段：重启后会话列表里应有上次 run 的会话（scripts/smoke.mjs 再按 sessionId 交叉核对）。
async function sessionVerify(api, invoke, result) {
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
  const info = await invoke('smoke_info').catch(() => null);
  if (info?.apiKey) await keyedProviderVerify(api, result);
}
