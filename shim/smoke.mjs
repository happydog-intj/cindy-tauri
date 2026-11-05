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
    let browserSmoke;
    if (boot.browserSmokeUrl && ok) {
      browserSmoke = await runBrowserSmoke(boot.browserSmokeUrl);
      ok = ok && browserSmoke.ok;
    }
    // 自动更新：旧版本检查 → 下载 → 安装重启（不返回）；重启后的新版本确认版本号并收尾。
    let updateSmoke;
    if (boot.updateSmokeExpect && ok) {
      updateSmoke = await runUpdateSmoke(boot.updateSmokeExpect);
      ok = ok && updateSmoke.ok;
    }
    // 终端：打包后的 node-pty 能从 resources 加载并跑通一条命令（会话冒烟两阶段、更新冒烟不重复跑）。
    let terminalSmoke;
    if (!boot.sessionSmoke && !boot.updateSmokeExpect && ok) {
      terminalSmoke = await runTerminalSmoke(boot.platform);
      ok = ok && terminalSmoke.ok;
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
      ...(browserSmoke ? { browserSmoke } : {}),
      ...(terminalSmoke ? { terminalSmoke } : {}),
      ...(updateSmoke ? { updateSmoke } : {}),
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

// ---------- 浏览器面板冒烟 ----------
// 用 renderer 同款 document.createElement('webview') 建一个子 webview，打开本地测试页，走一遍
// 事件、executeJavaScript、页内跳转、跳转 / 后退、隐藏、销毁。测试页见 scripts/smoke.mjs startBrowserPages。
async function runBrowserSmoke(base) {
  const result = { ok: false, steps: [] };
  const step = stepRunner(result);
  const seen = [];
  const wv = document.createElement('webview');
  const once = (type, pred = () => true) => new Promise((resolve) => {
    const fn = (e) => {
      if (!pred(e)) return;
      wv.removeEventListener(type, fn);
      resolve(e);
    };
    wv.addEventListener(type, fn);
  });
  const host = document.createElement('div');
  host.style.cssText = 'position:fixed;right:16px;bottom:16px;width:480px;height:320px;z-index:2147483647;';
  try {
    await step('element', () => {
      if (wv.tagName.toLowerCase() !== 'cindy-webview') throw new Error(`unexpected tag ${wv.tagName}`);
      let threw = false;
      try { wv.getWebContentsId(); } catch { threw = true; }
      if (!threw) throw new Error('getWebContentsId should throw before attach');
    });
    for (const t of ['did-attach', 'did-start-loading', 'did-navigate', 'dom-ready', 'did-stop-loading', 'page-title-updated', 'did-navigate-in-page', 'page-favicon-updated']) {
      wv.addEventListener(t, () => seen.push(t));
    }
    const ready = once('dom-ready');
    const titled = once('page-title-updated', (e) => e.title === 'Cindy Smoke A');
    wv.setAttribute('partition', 'persist:xdmaker-browser-app');
    wv.setAttribute('src', `${base}/a.html`);
    wv.setAttribute('style', 'width:100%;height:100%;');
    host.appendChild(wv);
    document.body.appendChild(host);
    await step('load page A', async () => {
      await ready;
      await titled;
      const id = wv.getWebContentsId();
      if (!Number.isInteger(id) || id <= 0) throw new Error(`bad id ${id}`);
      if (!wv.getURL().endsWith('/a.html')) throw new Error(`url ${wv.getURL()}`);
      return { id, url: wv.getURL() };
    });
    await step('executeJavaScript', async () => {
      const v = await wv.executeJavaScript('({ title: document.title, n: 1 + 1, list: [1, "x"] })');
      if (v?.title !== 'Cindy Smoke A' || v.n !== 2 || v.list?.[1] !== 'x') throw new Error(JSON.stringify(v));
      const undef = await wv.executeJavaScript('void 0');
      if (undef !== undefined) throw new Error(`void 0 → ${JSON.stringify(undef)}`);
      const stmt = await wv.executeJavaScript('var cindySmoke = 5; cindySmoke += 1;');
      const read = await wv.executeJavaScript('window.cindySmoke');
      if (read !== 6) throw new Error(`statement script → ${JSON.stringify(stmt)}, read ${read}`);
      let msg = '';
      try { await wv.executeJavaScript('(() => { throw new Error("boom") })()'); } catch (e) { msg = String(e.message); }
      if (!msg.includes('boom')) throw new Error(`exception not surfaced: ${msg}`);
      return { title: v.title };
    });
    // 外部页面拿不到 preload（electronAPI / 启动参数），调 Tauri 命令会被 ACL 拒绝。
    await step('page cannot reach app IPC', async () => {
      const probe = `(() => { window.__smokeIpc = {}; const i = window.__TAURI_INTERNALS__;
        if (!i) { window.__smokeIpc = { done: true, internals: false }; return 0; }
        const one = (cmd, args) => i.invoke(cmd, args).then(() => 'allowed', (e) => 'denied: ' + String(e).slice(0, 120));
        Promise.all([one('ipc_invoke', { channel: 'app:get-version', payload: '[]' }), one('sidecar_call', { method: 'ping', params: {} }),
          one('browser_op', { id: 1, op: 'state', arg: null })])
          .then((r) => { window.__smokeIpc = { done: true, internals: true, r }; });
        return 0; })()`;
      await wv.executeJavaScript(probe);
      const env = await wv.executeJavaScript('({ api: typeof window.electronAPI, boot: typeof window.__CINDY_BOOT__ })');
      if (env.api !== 'undefined' || env.boot !== 'undefined') throw new Error(`preload leaked: ${JSON.stringify(env)}`);
      let r;
      for (let i = 0; i < 50; i++) {
        r = await wv.executeJavaScript('window.__smokeIpc');
        if (r?.done) break;
        await new Promise((res) => setTimeout(res, 100));
      }
      if (!r?.done) throw new Error('probe did not finish');
      if (r.internals && r.r.some((x) => x === 'allowed')) throw new Error(`IPC reachable from page: ${JSON.stringify(r.r)}`);
      return r;
    });
    await step('in-page navigation', async () => {
      const ev = once('did-navigate-in-page', (e) => String(e.url).endsWith('#section'));
      await wv.executeJavaScript('(location.hash = "section", 0)');
      const e = await ev;
      return { url: e.url };
    });
    await step('navigate to page B', async () => {
      const titled = once('page-title-updated', (e) => e.title === 'Cindy Smoke B');
      const stopped = once('did-stop-loading');
      await wv.loadURL(`${base}/b.html`);
      await titled;
      await stopped;
      await waitFor(() => wv.canGoBack(), 'canGoBack', 5000);
      return { url: wv.getURL(), canGoBack: wv.canGoBack() };
    });
    await step('go back', async () => {
      const titled = once('page-title-updated', (e) => e.title === 'Cindy Smoke A');
      wv.goBack();
      await titled;
      await waitFor(() => wv.getURL().includes('/a.html'), 'url after back', 5000);
      await waitFor(() => wv.canGoForward(), 'canGoForward', 5000);
      return { url: wv.getURL() };
    });
    await step('blocked schemes stay out', async () => {
      let msg = '';
      try { await wv.loadURL('http://tauri.localhost/index.html'); } catch (e) { msg = String(e?.message ?? e); }
      if (!msg.includes('INVALID_PARAMS')) throw new Error(`tauri.localhost not rejected: ${msg}`);
      return { msg: msg.slice(0, 80) };
    });
    await step('hide and destroy', async () => {
      host.style.display = 'none';
      await new Promise((r) => setTimeout(r, 400));
      const id = wv.getWebContentsId();
      host.remove();
      await new Promise((r) => setTimeout(r, 400));
      let gone = false;
      try {
        await window.__TAURI_INTERNALS__.invoke('browser_op', { id, op: 'state', arg: null });
      } catch (e) {
        gone = String(e).includes('NOT_FOUND');
      }
      if (!gone) throw new Error('child webview still alive after remove()');
    });
    result.ok = true;
  } catch {
    // 失败详情已记在 steps 里
    host.remove();
  }
  result.events = [...new Set(seen)];
  return result;
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
async function runTerminalSmoke(platform) {
  const result = { ok: false, steps: [] };
  const step = stepRunner(result);
  const api = window.electronAPI.terminal;
  const id = `smoke-term-${Date.now()}`;
  let out = '';
  let exit = null;
  const offData = api.onData((p) => { if (p?.id === id) out += p.chunk; });
  const offExit = api.onExit((p) => { if (p?.id === id) exit = p.exit; });
  const waitUntil = async (pred, what) => {
    const end = Date.now() + 20_000;
    while (!pred()) {
      if (Date.now() > end) throw new Error(`timeout: ${what}; output tail ${JSON.stringify(out.slice(-200))}`);
      await new Promise((r) => setTimeout(r, 50));
    }
  };
  try {
    const shells = await step('list shells', async () => {
      const list = await api.listAvailableShells();
      if (!Array.isArray(list) || list.length === 0) throw new Error('no shells');
      return { ids: list.map((s) => s.id) };
    });
    const win = platform === 'win32';
    const pref = win ? (shells.ids.includes('pwsh') ? 'pwsh' : 'powershell') : 'auto';
    await step('create', async () => {
      const r = await api.create({ id, cwd: '/', cols: 90, rows: 24, shellPref: pref });
      if (!r?.pid) throw new Error(`bad create result ${JSON.stringify(r)}`);
      return { shellId: r.shellId };
    });
    await step('echo', async () => {
      await api.write(id, win ? "Write-Output ('tauri' + (6*7))\r" : 'echo tauri$((6*7))\r');
      await waitUntil(() => out.includes('tauri42'), 'echo output');
    });
    await step('resize', () => api.resize(id, 100, 30));
    await step('exit', async () => {
      await api.write(id, 'exit 0\r');
      await waitUntil(() => exit !== null, 'exit push');
      return exit;
    });
    result.ok = true;
  } catch {
    // 失败步骤已记在 steps
  } finally {
    try { await api.dispose(id); } catch { /* 已清理 */ }
    offData?.();
    offExit?.();
  }
  return result;
}

// 旧版本：手动检查应得到 ready，再走 UpdateBanner 的「立即重启」通道。安装成功时进程会被替换，
// 这里只会在失败（status=error 或超时）时返回。新版本：版本号对上、再检查是 idle 即成功。
async function runUpdateSmoke(expect) {
  const api = window.electronAPI;
  const result = { ok: false, appVersion: api.appVersion, expect, steps: [] };
  const step = stepRunner(result);
  const statuses = [];
  const off = api.onUpdateStatus?.((s) => statuses.push(s));
  try {
    if (api.appVersion === expect) {
      await step('updated version', () => ({ appVersion: api.appVersion }));
      await step('check after update is idle', async () => {
        const r = await api.checkForUpdate();
        if (r?.result !== 'idle') throw new Error(`unexpected ${JSON.stringify(r)}`);
      });
      result.ok = true;
      return result;
    }
    await step('check finds and downloads update', async () => {
      const r = await withTimeout(api.checkForUpdate(), 'checkForUpdate', 180_000);
      if (r?.result !== 'ready') throw new Error(`unexpected ${JSON.stringify(r)} statuses ${JSON.stringify(statuses.slice(-3))}`);
      const st = await api.getUpdateStatus();
      if (st?.status !== 'ready' || st?.version !== expect) throw new Error(`status ${JSON.stringify(st)}`);
      return { progressPushes: statuses.filter((s) => s.status === 'downloading').length };
    });
    await step('not busy before relaunch', async () => {
      const s = await api.describeRelaunchBlockingActivity();
      if (s?.busy !== false) throw new Error(`busy ${JSON.stringify(s)}`);
    });
    // 成功路径到不了下面的等待结束：进程被安装 / 重启替换。
    api.relaunchToUpdate('dark');
    await new Promise((resolve, reject) => {
      const t0 = Date.now();
      const tick = () => {
        const err = statuses.find((s) => s.status === 'error');
        if (err) reject(new Error(`install failed: ${JSON.stringify(err)}`));
        else if (Date.now() - t0 > 120_000) reject(new Error('install did not restart the app in 120s'));
        else setTimeout(tick, 200);
      };
      tick();
    }).catch((err) => { result.steps.push({ name: 'install and relaunch', ok: false, error: errText(err) }); });
  } catch {
    // 失败步骤已记在 steps
  } finally {
    off?.();
  }
  return result;
}

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
