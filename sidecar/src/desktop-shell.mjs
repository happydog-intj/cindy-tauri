// 桌面外壳：系统通知、Dock / 任务栏角标、原生对话框、打开链接 / 文件、窗口关闭行为、托盘菜单。
// IPC 逻辑和校验放在 sidecar（对齐 desktop bootstrap-electron.ts / notificationService.ts /
// appBadgeService.ts），真正的原生动作经 native.mjs 交给 Rust（tauri-plugin-dialog / opener / notification、tray）。
import { existsSync, mkdirSync, readFileSync, renameSync, statSync, writeFileSync } from 'node:fs';
import path from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';
import { emitToRenderer, registerInvoke, registerSend } from './ipc.mjs';
import { onMainLocaleChange, setMainLocale, t } from './main-locale.mjs';
import { callNative, notifyNative } from './native.mjs';

// desktop main/profile/profileRegistryModel.ts 的本机 owner。
const LOCAL_DATA_OWNER_ID = 'local-v1';
const LOCAL_OWNER_GENERATION = 1;
const CLIENT_NOTIFICATION_NAME = 'Cindy';
const CHATGPT_APP_URL = 'codex://';
const BILLING_SUPPORT_EMAIL = 'xd-billing@xd.com';
// desktop：Windows 首次关闭时先等 renderer 弹自绘对话框，2 秒内没确认显示就弹原生对话框。
const CLOSE_BEHAVIOR_PROMPT_FALLBACK_DELAY_MS = 2_000;
const MAX_SESSION_IDS = 10_000;
const ALLOWED_SYSTEM_SETTINGS_URLS = new Set([
  'x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility',
  'x-apple.systempreferences:com.apple.preference.security?Privacy_ScreenCapture',
  'x-apple.systempreferences:com.apple.preference.security?Privacy_AllFiles',
]);
const SESSION_EVENT_KINDS = new Set(['done', 'error', 'needs-reply']);
const BODY_KEY_BY_KIND = {
  done: 'settings.notifications.sessionEvent.done',
  error: 'settings.notifications.sessionEvent.error',
  'needs-reply': 'settings.notifications.sessionEvent.needsReply',
};

function fail(code, msg) {
  throw new Error(`[${code}] ${msg}`);
}

function isPlainObject(v) {
  return !!v && typeof v === 'object' && !Array.isArray(v);
}

// ── 路径策略：desktop main/filePathPolicy.ts 的系统目录黑名单（isPathAllowedAgainst） ──

function systemPathBlocklist(platform = process.platform, env = process.env) {
  if (platform !== 'win32') return ['/etc', '/proc', '/sys', '/dev', '/root', '/var/log'];
  const seen = new Set();
  const out = [];
  for (const c of [env.SystemRoot ?? 'C:\\Windows', env.ProgramFiles ?? 'C:\\Program Files', env['ProgramFiles(x86)'] ?? 'C:\\Program Files (x86)', env.ProgramData ?? 'C:\\ProgramData']) {
    if (!c || seen.has(c.toLowerCase())) continue;
    seen.add(c.toLowerCase());
    out.push(c);
  }
  return out;
}

function stripWinNamespace(input) {
  if (input.startsWith('\\\\?\\') || input.startsWith('\\\\.\\')) {
    const rest = input.slice(4);
    return /^UNC\\/i.test(rest) ? '\\\\' + rest.slice(4) : rest;
  }
  return input;
}

export function isPathAllowed(filePath, platform = process.platform) {
  const p = platform === 'win32' ? path.win32 : path.posix;
  const isWin = platform === 'win32';
  if (typeof filePath !== 'string' || !filePath) return false;
  const input = isWin ? stripWinNamespace(filePath) : filePath;
  if (!p.isAbsolute(input)) return false;
  const trimSep = (s) => (s.length > 1 && s.endsWith(p.sep) ? s.slice(0, -1) : s);
  const resolved = p.resolve(p.normalize(input));
  const target = trimSep(isWin ? resolved.toLowerCase() : resolved);
  for (const blocked of systemPathBlocklist(platform)) {
    const nb = p.resolve(isWin ? stripWinNamespace(blocked) : blocked);
    const key = trimSep(isWin ? nb.toLowerCase() : nb);
    if (target === key || target.startsWith(key + p.sep)) return false;
  }
  return true;
}

// desktop shared/browserOpenableExts.ts
function isBrowserOpenablePath(filePath) {
  const lastSep = Math.max(filePath.lastIndexOf('/'), filePath.lastIndexOf('\\'));
  const name = lastSep >= 0 ? filePath.slice(lastSep + 1) : filePath;
  const dot = name.lastIndexOf('.');
  if (dot <= 0) return false;
  return ['.html', '.htm'].includes(name.slice(dot).toLowerCase());
}

// desktop main/openFileInBrowser.ts resolveBrowserFileTarget
function resolveBrowserFileTarget(value) {
  if (typeof value !== 'string' || !value) return null;
  if (path.isAbsolute(value)) return { filePath: value, fileUrl: pathToFileURL(value).toString(), hasUrlState: false };
  let parsed;
  try { parsed = new URL(value); } catch { return null; }
  if (parsed.protocol !== 'file:' || parsed.username || parsed.password || parsed.port) return null;
  if (parsed.hostname && parsed.hostname !== 'localhost') return null;
  let filePath;
  try { filePath = fileURLToPath(parsed); } catch { return null; }
  if (!path.isAbsolute(filePath)) return null;
  return { filePath, fileUrl: parsed.toString(), hasUrlState: Boolean(parsed.search || parsed.hash) };
}

// desktop shared/billing.ts isAllowedBillingMailto
function isAllowedBillingMailto(value) {
  try {
    const u = new URL(value);
    const keys = [...u.searchParams.keys()];
    const subject = u.searchParams.get('subject');
    return u.protocol === 'mailto:'
      && decodeURIComponent(u.pathname).toLowerCase() === BILLING_SUPPORT_EMAIL
      && !u.username && !u.password && !u.host && !u.hash
      && new Set(keys).size === keys.length
      && !(subject !== null && /[\u0000-\u001f\u007f-\u009f]/u.test(subject))
      && keys.every((k) => k === 'subject' || k === 'body');
  } catch {
    return false;
  }
}

export function isAllowedExternalUrl(url, platform = process.platform) {
  if (typeof url !== 'string' || url.length > 32_768) return false;
  try {
    const u = new URL(url);
    if (u.protocol === 'http:' || u.protocol === 'https:') return true;
  } catch {
    return false;
  }
  if (isAllowedBillingMailto(url)) return true;
  return platform === 'darwin' && ALLOWED_SYSTEM_SETTINGS_URLS.has(url);
}

// ── 窗口行为设置：<dataDir>/window-behavior-settings.json（desktop window-behavior-settings-store.ts） ──

function createWindowBehaviorStore(dataDir) {
  const file = path.join(dataDir, 'window-behavior-settings.json');
  const DEFAULTS = { swallowActivationClick: false, windowsCloseBehavior: null, linuxCloseBehavior: null };
  const normalize = (raw) => ({
    swallowActivationClick: typeof raw?.swallowActivationClick === 'boolean' ? raw.swallowActivationClick : false,
    windowsCloseBehavior: raw?.windowsCloseBehavior === 'quit' || raw?.windowsCloseBehavior === 'tray' ? raw.windowsCloseBehavior : null,
    linuxCloseBehavior: raw?.linuxCloseBehavior === 'quit' || raw?.linuxCloseBehavior === 'minimize' ? raw.linuxCloseBehavior : null,
  });
  let cache = null;
  const read = () => {
    if (cache) return cache;
    try { cache = normalize(JSON.parse(readFileSync(file, 'utf8'))); } catch { cache = { ...DEFAULTS }; }
    return cache;
  };
  const writePatch = (patch) => {
    const next = { ...read(), ...patch };
    mkdirSync(dataDir, { recursive: true });
    const tmp = `${file}.${process.pid}.tmp`;
    writeFileSync(tmp, JSON.stringify(next, null, 2));
    renameSync(tmp, file);
    cache = next;
  };
  return { read, writePatch };
}

// ── 注册 ──

export function registerDesktopShellIpc({ dataDir, db, anySessionInTurn = () => false, platform = process.platform }) {
  const logsDir = path.join(dataDir, 'logs');
  const behavior = createWindowBehaviorStore(dataDir);

  // ── 托盘文案：Rust 只在 Windows 建托盘；语言变化时重发 ──
  const pushTrayLabels = () => notifyNative('tray.setLabels', {
    show: t('settings.windowBehavior.trayMenu.show'),
    openLogs: t('credentialStore.dialog.openLogs'),
    quit: t('settings.windowBehavior.trayMenu.quit'),
    tooltip: CLIENT_NOTIFICATION_NAME,
  });
  pushTrayLabels();
  onMainLocaleChange(pushTrayLabels);
  // renderer 启动和切换语言时同步主进程语言（desktop app-menu:set-locale；应用菜单本身还没移植）。
  registerInvoke('app-menu:set-locale', async (locale) => {
    setMainLocale(locale);
    return { ok: true };
  });

  async function openLogsDirectory() {
    try {
      mkdirSync(logsDir, { recursive: true });
      await callNative('open.path', { path: logsDir });
      return { success: true };
    } catch (err) {
      console.error('[desktop-shell] could not open logs directory:', err?.message ?? err);
      return { success: false };
    }
  }

  // ── 角标（appBadgeService.ts）：首份投影前按逐任务通知计数，之后以 renderer 投影为准 ──
  const attention = new Set();
  const projectedIds = new Set();
  let projectedCount = null;
  let lastBadge = -1;
  const attentionCount = () => (projectedCount === null ? attention.size : projectedCount);
  const applyBadge = () => {
    const count = attentionCount();
    if (count === lastBadge) return;
    lastBadge = count;
    notifyNative('badge.set', { count });
  };
  const markAttention = (sid) => {
    const before = attention.size;
    attention.add(sid);
    if (attention.size !== before && (projectedCount === null || !projectedIds.has(sid))) applyBadge();
  };
  const clearAttention = (sid, intent) => {
    if (attention.delete(sid) && (projectedCount === null || !projectedIds.has(sid))) applyBadge();
    emitToRenderer('notification:session-attention-cleared', { sessionId: sid, intent });
  };

  registerInvoke('notification:set-app-attention-count', async (snapshot) => {
    if (!isPlainObject(snapshot) || typeof snapshot.dataOwnerId !== 'string' && snapshot.dataOwnerId !== null || typeof snapshot.ownerGeneration !== 'number') {
      fail('INVALID_PARAMS', 'App attention snapshot requires an owner stamp');
    }
    const { count, sessionIds } = snapshot;
    if (typeof count !== 'number' || !Number.isSafeInteger(count) || count < 0 || count > MAX_SESSION_IDS) {
      fail('INVALID_PARAMS', 'App attention count exceeds the supported inventory range');
    }
    if (!Array.isArray(sessionIds) || sessionIds.length > MAX_SESSION_IDS
      || sessionIds.some((id) => typeof id !== 'string' || id.length === 0 || id.length > 256)) {
      fail('INVALID_PARAMS', 'App attention snapshot requires session IDs');
    }
    if (snapshot.dataOwnerId !== LOCAL_DATA_OWNER_ID || snapshot.ownerGeneration !== LOCAL_OWNER_GENERATION) return undefined;
    projectedCount = count;
    for (const id of sessionIds) projectedIds.add(id);
    if (projectedIds.size > MAX_SESSION_IDS * 2) projectedIds.clear();
    applyBadge();
    return undefined;
  });
  registerInvoke('notification:mark-session-attention', async (sid) => {
    if (typeof sid === 'string' && sid.length > 0 && sid.length <= 256) markAttention(sid);
    return undefined;
  });
  registerInvoke('notification:clear-session-attention', async (sid, intent) => {
    if (typeof sid === 'string' && sid.length > 0 && sid.length <= 256) clearAttention(sid, intent === 'explicit' ? 'explicit' : 'passive');
    return undefined;
  });

  // ── 系统通知（notificationService.ts）：只走桌面通道；飞书 / 手机 / 灵动岛未移植 ──
  let desktopNotificationsEnabled = true;
  registerInvoke('notification:set-desktop-enabled', async (enabled) => {
    if (typeof enabled !== 'boolean') fail('INVALID_PARAMS', 'notification desktop enabled must be a boolean');
    desktopNotificationsEnabled = enabled;
    return { ok: true };
  });
  const isMuted = (sid) => {
    try { return db?.getSession?.(sid)?.notificationsMuted === true; } catch { return false; }
  };
  registerInvoke('notification:show-session-event', async (p) => {
    if (!isPlainObject(p)
      || typeof p.sessionId !== 'string' || p.sessionId.length === 0 || p.sessionId.length > 256
      || typeof p.title !== 'string' || p.title.length > 1024
      || typeof p.kind !== 'string' || !SESSION_EVENT_KINDS.has(p.kind)
      || (p.channels !== undefined && !isPlainObject(p.channels))
      || (p.markAttention !== undefined && typeof p.markAttention !== 'boolean')
      || (p.inputHandoffRequestId !== undefined && (p.kind !== 'needs-reply' || typeof p.inputHandoffRequestId !== 'string'
        || p.inputHandoffRequestId.length === 0 || p.inputHandoffRequestId.length > 256))) {
      fail('INVALID_PARAMS', 'invalid session event notification payload');
    }
    const mark = p.markAttention !== false;
    if (mark) markAttention(p.sessionId);
    if (mark && isMuted(p.sessionId)) return undefined;
    // 窗口聚焦时不弹由 renderer 判断（document.hasFocus），与 desktop 一致。
    if ((p.channels?.desktop ?? true) !== true || !desktopNotificationsEnabled) return undefined;
    const safeTitle = p.title.trim() || p.sessionId.slice(0, 8) || t('settings.notifications.sessionEvent.untitled');
    notifyNative('notify', {
      title: `${CLIENT_NOTIFICATION_NAME} · ${safeTitle}`,
      body: t(BODY_KEY_BY_KIND[p.kind]),
      sessionId: p.sessionId,
    });
    return undefined;
  });
  // 供将来 scheduler 等 main 内来源复用：与 desktop getDesktopNotificationsEnabled 一致。
  const getDesktopNotificationsEnabled = () => desktopNotificationsEnabled;

  // ── 打开链接 / 文件 ──
  registerInvoke('shell:open-external', async (url) => {
    if (!isAllowedExternalUrl(url, platform)) return { success: false };
    try {
      await callNative('open.url', { url });
      return { success: true };
    } catch {
      return { success: false };
    }
  });
  registerInvoke('shell:open-chatgpt-app', async () => {
    try {
      await callNative('open.url', { url: CHATGPT_APP_URL });
      return { success: true };
    } catch {
      return { success: false };
    }
  });
  registerInvoke('shell:open-file-in-browser', async (value) => {
    const target = resolveBrowserFileTarget(value);
    if (!target) return { success: false, error: '路径必须是绝对路径或本地 file:// URL', errorCode: 'BROWSER_FILE_INVALID_TARGET' };
    if (!isPathAllowed(target.filePath, platform)) return { success: false, error: '不允许访问该路径', errorCode: 'BROWSER_FILE_PATH_NOT_ALLOWED' };
    if (!isBrowserOpenablePath(target.filePath)) return { success: false, error: '该文件类型不支持浏览器查看', errorCode: 'BROWSER_FILE_UNSUPPORTED_TYPE' };
    if (!existsSync(target.filePath)) return { success: false, error: '文件不存在', errorCode: 'BROWSER_FILE_NOT_FOUND' };
    try {
      await callNative('open.url', { url: target.fileUrl });
      return { success: true };
    } catch (err) {
      if (target.hasUrlState) return { success: false, error: '无法在浏览器中打开该文件', errorCode: 'BROWSER_FILE_OPEN_FAILED' };
      try {
        await callNative('open.path', { path: target.filePath });
        return { success: true };
      } catch (err2) {
        return { success: false, error: String(err2?.message ?? err ?? err2), errorCode: 'BROWSER_FILE_OPEN_FAILED' };
      }
    }
  });
  registerInvoke('shell:show-item-in-folder', async (params) => {
    // xdt-image:// / xdt-video:// / cindy-media:// 缓存还没移植，只支持绝对路径。
    const filePath = isPlainObject(params) && typeof params.filePath === 'string' && path.isAbsolute(params.filePath) ? path.normalize(params.filePath) : null;
    if (!filePath) return { success: false, error: 'url 或 filePath 必须二选一' };
    if (!isPathAllowed(filePath, platform)) return { success: false, error: '不允许访问该路径' };
    if (!existsSync(filePath)) return { success: false, error: '文件不存在' };
    try {
      await callNative('reveal', { path: filePath });
      return { success: true };
    } catch (err) {
      return { success: false, error: String(err?.message ?? err) };
    }
  });
  registerInvoke('app:open-logs-dir', async () => openLogsDirectory());

  // ── 原生选择器（tauri-plugin-dialog），用户操作期间不超时 ──
  const optionalAbsolute = (v, name) => {
    if (v === undefined || v === null || v === '') return undefined;
    if (typeof v !== 'string' || v.length > 4096 || !path.isAbsolute(v)) fail('INVALID_PARAMS', `${name} must be an absolute path`);
    return v;
  };
  const pickFolder = (defaultPath) => callNative('dialog.pickFolder', { defaultPath }, { timeoutMs: 0 });
  const pickFile = (defaultPath, filters) => callNative('dialog.pickFile', { defaultPath, filters }, { timeoutMs: 0 });

  registerInvoke('show-open-directory-dialog', async () => {
    const picked = await pickFolder(undefined);
    return picked ? { canceled: false, path: picked } : { canceled: true };
  });
  registerInvoke('dialog:show-open-directory', async (params = {}) => {
    const body = isPlainObject(params) ? params : {};
    if (body.writableGrantScope !== undefined && (typeof body.writableGrantScope !== 'string' || !body.writableGrantScope || body.writableGrantScope.length > 128)) {
      fail('INVALID_PARAMS', 'invalid writable directory grant scope');
    }
    // extraDirs 可写授权还没移植（Pi 能力里 extraDirs 为 false），这里只做选择。
    const picked = await pickFolder(optionalAbsolute(body.defaultPath, 'defaultPath'));
    return { success: true, path: picked ?? null };
  });
  registerInvoke('dialog:show-open-file', async (params = {}) => {
    const body = isPlainObject(params) ? params : {};
    let filters;
    if (body.filters !== undefined) {
      if (!Array.isArray(body.filters) || body.filters.length > 20) fail('INVALID_PARAMS', 'filters must be an array');
      filters = body.filters.map((f) => {
        if (!isPlainObject(f) || typeof f.name !== 'string' || !Array.isArray(f.extensions)
          || f.extensions.some((e) => typeof e !== 'string' || !/^[A-Za-z0-9*_-]{1,32}$/.test(e))) {
          fail('INVALID_PARAMS', 'invalid file filter');
        }
        return { name: f.name.slice(0, 128), extensions: f.extensions };
      });
    }
    const picked = await pickFile(optionalAbsolute(body.defaultPath, 'defaultPath'), filters);
    return { success: true, path: picked ?? null };
  });
  registerInvoke('dialog:show-open-resource', async (params = {}) => {
    if (!isPlainObject(params)) fail('INVALID_PARAMS', 'params must be an object');
    // desktop 在 macOS 上可同时选文件或目录；Tauri 选择器一次只能选一种，这里统一选文件。
    const picked = await pickFile(optionalAbsolute(params.defaultPath, 'defaultPath'), undefined);
    if (!picked) return { success: true, path: null, kind: null };
    let kind = 'file';
    try { kind = statSync(picked).isDirectory() ? 'directory' : 'file'; } catch { /* 选完被删：仍按文件返回 */ }
    return { success: true, path: picked, kind };
  });

  // ── 窗口关闭行为（Windows / Linux 首次关闭询问，之后沿用） ──
  let fallbackTimer = null;
  const cancelFallback = () => {
    if (fallbackTimer) clearTimeout(fallbackTimer);
    fallbackTimer = null;
  };
  const applyClose = (choice) => notifyNative('window.closeAction', { action: choice === 'tray' ? 'tray' : 'quit' });

  async function showNativeClosePrompt() {
    const tray = t('settings.windowBehavior.closeBehavior.tray');
    const quit = t('settings.windowBehavior.closeBehavior.quit');
    const index = await callNative('dialog.message', {
      kind: 'info',
      title: t('settings.windowBehavior.closePrompt.title'),
      message: t('settings.windowBehavior.closePrompt.message'),
      detail: t('settings.windowBehavior.closePrompt.detail'),
      buttons: [tray, quit],
    }, { timeoutMs: 0 }).catch(() => 0);
    return index === 1 ? 'quit' : 'tray';
  }

  const requestWindowsClosePrompt = () => {
    emitToRenderer('window-behavior:windows-close-behavior-requested');
    if (fallbackTimer) return;
    fallbackTimer = setTimeout(async () => {
      fallbackTimer = null;
      if (behavior.read().windowsCloseBehavior) return;
      const choice = await showNativeClosePrompt();
      // 原生框打开期间 renderer 可能已经存了选择，保留先到的那个。
      if (behavior.read().windowsCloseBehavior) return;
      behavior.writePatch({ windowsCloseBehavior: choice });
      applyClose(choice);
    }, CLOSE_BEHAVIOR_PROMPT_FALLBACK_DELAY_MS);
    fallbackTimer.unref?.();
  };

  registerInvoke('window-behavior:set-swallow-activation-click', async (enabled) => {
    if (typeof enabled !== 'boolean') fail('INVALID_PARAMS', 'enabled must be a boolean');
    behavior.writePatch({ swallowActivationClick: enabled });
    return { ok: true };
  });
  registerInvoke('window-behavior:get-windows-close-behavior', async () => behavior.read().windowsCloseBehavior);
  registerInvoke('window-behavior:set-windows-close-behavior', async (value) => {
    if (value !== 'quit' && value !== 'tray') fail('INVALID_PARAMS', 'Windows close behavior required (quit|tray)');
    cancelFallback();
    behavior.writePatch({ windowsCloseBehavior: value });
    return value;
  });
  registerSend('window-behavior:windows-close-behavior-shown', () => cancelFallback());
  registerInvoke('window-behavior:get-linux-close-behavior', async () => behavior.read().linuxCloseBehavior);
  registerInvoke('window-behavior:set-linux-close-behavior', async (value) => {
    if (value !== 'quit' && value !== 'minimize') fail('INVALID_PARAMS', 'Linux close behavior required (quit|minimize)');
    behavior.writePatch({ linuxCloseBehavior: value });
    return value;
  });
  registerSend('window-behavior:linux-close-behavior-shown', () => undefined);

  async function confirmQuitIfBusy() {
    let busy;
    try { busy = anySessionInTurn(); } catch { busy = true; } // 探测失败不能变成无确认退出
    if (!busy) return true;
    const index = await callNative('dialog.message', {
      kind: 'warning',
      title: t('titleBar.closeConfirm.title'),
      message: t('titleBar.closeConfirm.title'),
      detail: t('titleBar.closeConfirm.description'),
      // 第一个按钮是默认 / 取消，与 desktop defaultId: 0 一致。
      buttons: [t('titleBar.closeConfirm.cancel'), t('titleBar.closeConfirm.confirm')],
    }, { timeoutMs: 0 }).catch(() => 0);
    return index === 1;
  }

  // Rust 专用方法（sidecar_call 拒绝 webview 调 native.*）。
  const nativeMethods = {
    // 主窗口收到关闭请求（自绘 X、Alt+F4、任务栏关闭）：Windows 按持久化选择执行，没选过就先问。
    'native.mainWindowCloseRequested': () => {
      if (platform !== 'win32') return { action: 'quit' };
      const choice = behavior.read().windowsCloseBehavior;
      if (choice) return { action: choice };
      requestWindowsClosePrompt();
      return { action: 'none' };
    },
    // 点击任务通知：Rust 已把主窗口拉到前台，这里让 renderer 跳到对应任务（desktop openMainWindowSession）。
    'native.notificationClicked': (params) => {
      const sid = params?.sessionId;
      if (typeof sid === 'string' && sid.length > 0 && sid.length <= 256) {
        emitToRenderer('deep-link:navigate', { type: 'session', id: sid });
      }
      return null;
    },
    'native.trayOpenLogs': async () => {
      const r = await openLogsDirectory();
      if (!r.success) {
        await callNative('dialog.message', {
          kind: 'error', title: t('logSupport.title'), message: t('settings.about.openLogsDirError'), buttons: [],
        }, { timeoutMs: 0 }).catch(() => undefined);
      }
      return r;
    },
    'native.trayQuit': async () => ({ quit: await confirmQuitIfBusy() }),
    'native.trayUnavailable': async () => {
      await callNative('dialog.message', {
        kind: 'error', title: t('settings.windowBehavior.trayError.title'), message: t('settings.windowBehavior.trayError.message'), buttons: [],
      }, { timeoutMs: 0 }).catch(() => undefined);
      return null;
    },
  };

  return { nativeMethods, getDesktopNotificationsEnabled, _test: { attentionCount, behavior } };
}
