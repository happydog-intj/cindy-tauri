// 冒烟模式下追加到 preload bundle 末尾：等真实 renderer 渲染出界面后，
// 汇总第一阶段结果、界面摘要和未实现的 IPC，写结果并退出。
const BOOT_TIMEOUT_MS = 90_000;
// 这些 scope 的 error 说明首屏没进到可用状态（路由崩溃、启动闸失败）。
const FATAL_SCOPES = /error-fallback|ErrorBoundary|Gate|EnvCheck/i;

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
    const ok = rendered && unexpected.length === 0 && fatal.length === 0 && !stats?.error;
    const detail = JSON.stringify({
      phase1: sessionStorage.getItem('cindySmokePhase1'),
      renderer,
      errors: unexpected.slice(0, 20),
      notImplementedRejections: errors.length - unexpected.length,
      fatalRendererErrors: fatal.slice(0, 10),
      ipc: stats,
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
