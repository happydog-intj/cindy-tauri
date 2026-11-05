// P4 自动更新：对齐 desktop main/updateService.ts 给 renderer 的通道和状态推送。
// 检查 / 下载 / 安装由 Rust 的 tauri-plugin-updater 完成（反向调用 update.*），这里只管状态机和重启策略：
//   idle → checking → downloading(progress) → ready ──(用户点重启 / 空闲自动重启)──→ update.install
//   ready 之后再检查到更新的版本 → superseding（banner 保持可见），失败回到 ready。
// 更新包由 Rust 用内置公钥校验签名（requireSignedVersion），sidecar 拿不到也不处理安装包本身。
// CINDY_TAURI_UPDATES：auto（正式包：10 秒后首查、每 30 分钟轮询）/ manual（冒烟：只响应手动检查）/ off（开发包）。
import { callNative } from './native.mjs';
import { emitToRenderer, registerInvoke, registerSend } from './ipc.mjs';

const FIRST_CHECK_DELAY_MS = 10_000;
const POLL_INTERVAL_MS = 30 * 60 * 1000;
const AUTO_RELAUNCH_POLL_INTERVAL_MS = 30_000;
// desktop updateAutoRelaunchPolicy.ts
const AUTO_UPDATE_IDLE_THRESHOLD_SECONDS = 10 * 60;
const AUTO_UPDATE_BUSY_QUIET_PERIOD_MS = 60 * 1000;
const CHECK_TIMEOUT_MS = 60_000;
const DOWNLOAD_TIMEOUT_MS = 30 * 60 * 1000;

// desktop updateService.ts isVersionlessAppVersion：占位版本不参与更新。
export function isVersionlessAppVersion(version) {
  return version === '0.0.0' || String(version).startsWith('0.0.0-');
}

// 冒烟里可以把计时调短（只认 CINDY_TAURI_SMOKE=1）。
function timing(name, fallback) {
  if (process.env.CINDY_TAURI_SMOKE !== '1') return fallback;
  const v = Number(process.env[name]);
  return Number.isFinite(v) && v >= 0 ? v : fallback;
}

export function registerUpdateIpc({
  appVersion = process.env.CINDY_TAURI_APP_VERSION ?? '0.0.0',
  mode = process.env.CINDY_TAURI_UPDATES ?? 'off',
  anySessionInTurn = () => false,
  readAutoRelaunchOnIdle = () => false,
  log = (...a) => console.error('[update]', ...a),
} = {}) {
  const enabled = (mode === 'auto' || mode === 'manual') && !isVersionlessAppVersion(appVersion);
  let status = 'idle';
  let readyVersion;
  let lastErrorCode;
  let lastProgress;
  let downloadingVersion;
  // Rust 给的更新说明（latest.json 的 notes），按版本记，给 release-notes:fetch 用。
  const notes = new Map();
  let inFlight = null;
  let relaunching = false;
  let lastBusyAtMs = null;
  const timers = [];

  function setStatus(next, extra = {}) {
    status = next;
    if (next !== 'downloading' && next !== 'superseding') downloadingVersion = undefined;
    lastErrorCode = extra.errorCode;
    lastProgress = extra.progress;
    emitToRenderer('update-status', { status: next, ...extra });
    if (next === 'ready' && !extra.errorCode) void evaluateAutoRelaunch('status-ready');
  }

  async function doCheck() {
    const wasReady = status === 'ready';
    const previousReady = wasReady ? readyVersion : undefined;
    if (!wasReady) setStatus('checking');
    let found;
    try {
      found = await callNative('update.check', {}, { timeoutMs: CHECK_TIMEOUT_MS });
    } catch (err) {
      log('check failed:', err.message);
      if (wasReady) setStatus('ready', { version: previousReady });
      else setStatus('idle');
      return 'manifest_failed';
    }
    if (!found?.available) {
      // Rust 只在远端版本更高时报 available；同版本 / 降级都当作已是最新。
      if (wasReady) setStatus('ready', { version: previousReady });
      else setStatus('idle');
      return wasReady ? 'ready' : 'idle';
    }
    const version = String(found.version);
    if (typeof found.body === 'string' && found.body.trim()) notes.set(version, { body: found.body, date: found.date ?? null });
    if (wasReady && version === previousReady) {
      setStatus('ready', { version });
      return 'ready';
    }
    downloadingVersion = version;
    if (wasReady) setStatus('superseding', { version: previousReady, progress: 0 });
    else setStatus('downloading', { version, progress: 0 });
    try {
      await callNative('update.download', { version }, { timeoutMs: DOWNLOAD_TIMEOUT_MS });
    } catch (err) {
      log('download failed:', err.message);
      emitToRenderer('app-update-progress', { progress: lastProgress ?? 0, received: 0, total: 0, failed: true, error: 'DOWNLOAD_FAILED' });
      if (wasReady) {
        setStatus('ready', { version: previousReady });
        return 'ready';
      }
      setStatus('idle');
      return 'download_failed';
    }
    readyVersion = version;
    setStatus('ready', { version });
    return 'ready';
  }

  // desktop checkForUpdate：同一时间只跑一次，重复调用复用同一个 Promise。
  function checkForUpdate() {
    if (!enabled) return Promise.resolve('idle');
    if (!inFlight) inFlight = doCheck().finally(() => { inFlight = null; });
    return inFlight;
  }

  // Rust 下载回调（native.updateProgress），按整数百分比去重后推给 renderer。
  function onProgress({ received, total } = {}) {
    if (status !== 'downloading' && status !== 'superseding') return;
    const r = Number(received) || 0;
    const t = Number(total) || 0;
    const pct = t > 0 ? Math.min(100, Math.floor((r / t) * 100)) : undefined;
    emitToRenderer('app-update-progress', { progress: pct ?? 0, received: r, total: t });
    if (pct === undefined || pct === lastProgress) return;
    const version = status === 'superseding' ? readyVersion : downloadingVersion;
    setStatus(status, { version, progress: pct });
  }

  function isBusy() {
    try {
      const busy = !!anySessionInTurn();
      if (busy) lastBusyAtMs = Date.now();
      return busy;
    } catch {
      // desktop：忙闲探测失败按忙处理。
      lastBusyAtMs = Date.now();
      return true;
    }
  }

  // 安装并重启。成功时 Rust 会结束本进程（Windows 交给安装程序，macOS 替换 .app 后重启），不会返回。
  async function executeRelaunch(reason) {
    if (status !== 'ready' || relaunching) return false;
    relaunching = true;
    log(`relaunching to install ${readyVersion} (${reason})`);
    try {
      await callNative('update.install', { version: readyVersion }, { timeoutMs: 0 });
      return true;
    } catch (err) {
      log('install failed:', err.message);
      relaunching = false;
      setStatus('error', { errorCode: 'updater_spawn_failed', version: readyVersion });
      return false;
    }
  }

  async function autoRelaunchBlockReason() {
    if (!readAutoRelaunchOnIdle()) return 'disabled';
    if (status !== 'ready') return 'not-ready';
    if (relaunching) return 'relaunching';
    if (isBusy()) return 'busy';
    if (lastBusyAtMs !== null && Date.now() - lastBusyAtMs < AUTO_UPDATE_BUSY_QUIET_PERIOD_MS) return 'recent-busy';
    let idle;
    try {
      idle = await callNative('update.idleSeconds', {}, { timeoutMs: 5_000 });
    } catch {
      return 'screen-state-unknown';
    }
    if (typeof idle !== 'number' || !Number.isFinite(idle)) return 'screen-state-unknown';
    if (idle < timing('CINDY_TAURI_UPDATE_IDLE_SECONDS', AUTO_UPDATE_IDLE_THRESHOLD_SECONDS)) return 'user-active';
    // 探测期间状态可能变了，落地前再看一眼。
    if (status !== 'ready' || relaunching || isBusy()) return 'not-ready';
    return null;
  }

  async function evaluateAutoRelaunch(reason) {
    if (mode !== 'auto' && process.env.CINDY_TAURI_SMOKE !== '1') return;
    const block = await autoRelaunchBlockReason();
    if (!block) await executeRelaunch(`auto:${reason}`);
  }

  registerInvoke('update-get-status', async () => ({ status, version: readyVersion, errorCode: lastErrorCode }));
  registerInvoke('update-check-now', async () => {
    if (status === 'downloading' || status === 'superseding') return { result: 'downloading' };
    if (status === 'ready') return { result: 'ready' };
    return { result: await checkForUpdate() };
  });
  // 用户在 UpdateBanner 点「立即重启」。不 await：安装期间 Rust 会关掉 sidecar 的 stdin，挂着的请求会拖住退出。
  registerSend('update-relaunch', () => { void executeRelaunch('user'); });
  // 启动页的自动重启入口（preload 本地的 checkAppUpdate 恒为无更新，这里只做 desktop 的启动策略）。
  registerInvoke('update-relaunch-auto', async () => {
    if (status !== 'ready') return { accepted: false, blockReason: 'not-ready' };
    if (relaunching) return { accepted: false, blockReason: 'relaunching' };
    void executeRelaunch('startup');
    return { accepted: true };
  });
  registerSend('update-set-relaunch-theme', () => {});
  // macOS「移到应用程序」：Tauri 版没有 translocation 检测，不会走到这里。
  registerInvoke('update-move-to-applications', async () => ({ moved: false }));
  // desktop relaunchBusyActivityIpc.ts：Tauri 版只有本地会话一种活动来源。
  registerInvoke('update-relaunch:blocking-activity', async () => isBusy());
  registerInvoke('update-relaunch:blocking-summary', async () => {
    const busy = isBusy();
    return { busy, categories: busy ? ['conversation'] : [], companionNames: [] };
  });
  // 更新公告：用 latest.json 的 notes 拼一条 topic（desktop RawReleaseNotes 形状），没有就返回 null。
  registerInvoke('release-notes:fetch', async (version) => {
    const n = notes.get(String(version));
    if (!n) return null;
    return { version: String(version), date: n.date ?? '', topics: [{ title: `v${version}`, text: n.body }] };
  });
  registerInvoke('release-notes:fetch-index', async () => null);

  if (enabled && mode === 'auto') {
    const first = setTimeout(() => {
      void checkForUpdate();
      const poll = setInterval(() => void checkForUpdate(), timing('CINDY_TAURI_UPDATE_POLL_MS', POLL_INTERVAL_MS));
      poll.unref?.();
      timers.push(poll);
    }, timing('CINDY_TAURI_UPDATE_FIRST_CHECK_MS', FIRST_CHECK_DELAY_MS));
    first.unref?.();
    timers.push(first);
  }
  // 自动重启轮询：manual 模式（冒烟）也开，方便测试空闲重启；没打开设置时 autoRelaunchBlockReason 直接返回。
  if (enabled) {
    const t = setInterval(() => void evaluateAutoRelaunch('poll'), timing('CINDY_TAURI_UPDATE_AUTO_RELAUNCH_MS', AUTO_RELAUNCH_POLL_INTERVAL_MS));
    t.unref?.();
    timers.push(t);
  }

  return {
    nativeMethods: {
      'native.updateProgress': (params) => { onProgress(params); return null; },
    },
    dispose: () => timers.forEach((t) => clearInterval(t)),
    _test: { state: () => ({ status, readyVersion, lastErrorCode, enabled, mode }) },
  };
}
