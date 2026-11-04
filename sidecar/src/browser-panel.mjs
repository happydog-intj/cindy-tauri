// 浏览器面板的 sidecar 通道（P3）：rsb-browser-bridge:*、browser-backend:*、rsb-native-popup:*，
// 加上 shim/webview.mjs 专用的 tauri-browser:popup。
// desktop 的 TabRegistry 记 tabId → webContentsId，供 Agent 浏览器自动化后端和弹窗归属使用。Tauri 版没有
// Agent 自动化后端（browser-backend 报不可用），registry 只用来给弹窗补 openerTabId / openerSessionId。
// webContentsId 就是 shim 分配的子 webview id；Tauri 版只有主窗口一个 renderer，不做 desktop 的宿主校验。
import path from 'node:path';
import { emitToRenderer, registerInvoke } from './ipc.mjs';
import { callNative } from './native.mjs';
import { createOverrideSettingsFile } from './settings/override-file.mjs';

// shared/browserBackend.ts RECOMMENDED_PAGE_CONTROL_EXTENSION_STORE_URL
const EXTENSION_STORE_URL = 'https://chromewebstore.google.com/detail/cindy/dmbhhhdijpeaibfeocdcjhfefkgpkeef';
const KINDS = ['external', 'rsb-webview', 'user-browser'];
// 弹窗先于 report 到达时等待归属的上限（与 desktop DEFERRED_POPUP_ROUTE_TIMEOUT_MS 一致）。
const POPUP_OPENER_WAIT_MS = 10_000;

const fail = (code, message) => { throw new Error(`[${code}] ${message}`); };
const requireObject = (v, name) => {
  if (!v || typeof v !== 'object' || Array.isArray(v)) fail('INVALID_PARAMS', `${name} must be an object`);
  return v;
};
const requireString = (v, name) => {
  if (typeof v !== 'string' || !v) fail('INVALID_PARAMS', `${name} must be a non-empty string`);
  return v;
};
const requireId = (v, name) => {
  if (typeof v !== 'number' || !Number.isInteger(v) || v < 0) fail('INVALID_PARAMS', `${name} must be a non-negative integer`);
  return v;
};

function isRoutablePopupUrl(url) {
  try {
    const p = new URL(url);
    return p.protocol === 'http:' || p.protocol === 'https:';
  } catch {
    return false;
  }
}

export function registerBrowserPanelIpc({ dataDir }) {
  // ---------- rsb-browser-bridge ----------
  const records = new Map(); // tabId → { sessionId, tabId, webContentsId }
  const pinned = new Set();
  const reportWaiters = new Set();
  let activeSessionId = null;
  let foregroundTabId = null;

  const recordById = (webContentsId) => {
    for (const r of records.values()) if (r.webContentsId === webContentsId) return r;
    return null;
  };

  registerInvoke('rsb-browser-bridge:report', (payload) => {
    const obj = requireObject(payload, 'report payload');
    const record = {
      sessionId: requireString(obj.sessionId, 'sessionId'),
      tabId: requireString(obj.tabId, 'tabId'),
      webContentsId: requireId(obj.webContentsId, 'webContentsId'),
    };
    records.set(record.tabId, record);
    for (const w of [...reportWaiters]) w(record);
    return { ok: true };
  });

  registerInvoke('rsb-browser-bridge:release', (payload) => {
    const obj = requireObject(payload, 'release payload');
    const tabId = requireString(obj.tabId, 'tabId');
    if (obj.webContentsId !== undefined && (typeof obj.webContentsId !== 'number' || !Number.isInteger(obj.webContentsId))) {
      fail('INVALID_PARAMS', 'webContentsId must be an integer when provided');
    }
    const current = records.get(tabId);
    // 带旧 id 的 release 只在记录仍指向它时生效（desktop registry.release 的 stale 防护）。
    if (current && (obj.webContentsId === undefined || current.webContentsId === obj.webContentsId)) {
      records.delete(tabId);
      pinned.delete(tabId);
    }
    return { ok: true };
  });

  registerInvoke('rsb-browser-bridge:snapshot', (payload) => {
    const obj = requireObject(payload, 'snapshot payload');
    if (!Array.isArray(obj.liveTabIds)) fail('INVALID_PARAMS', 'liveTabIds must be an array');
    const live = new Set(obj.liveTabIds.map((id, i) => requireString(id, `liveTabIds[${i}]`)));
    // renderer 重新加载后 shim 会关掉上一代全部子 webview，不在 live 里的记录都已失效。
    const dropped = [...records.keys()].filter((tabId) => !live.has(tabId));
    for (const tabId of dropped) records.delete(tabId);
    return { ok: true, dropped, kept: records.size, pinnedTabIds: [...pinned] };
  });

  // 截图要读子 webview 的像素，WKWebView / WebView2 的截图 API 尚未接入（DESIGN.md §10 的降级项）。
  const captureUnsupported = (payload) => {
    requireString(requireObject(payload, 'capture-screenshot payload').tabId, 'tabId');
    fail('UNSUPPORTED_CAPABILITY', 'browser screenshots are not available in this build yet');
  };
  registerInvoke('rsb-browser-bridge:capture-screenshot', captureUnsupported);
  registerInvoke('rsb-browser-bridge:capture-screenshot-data', captureUnsupported);

  registerInvoke('rsb-browser-bridge:tab-op-result', () => ({ ok: true }));
  registerInvoke('rsb-browser-bridge:set-active-session', (payload) => {
    const obj = requireObject(payload, 'set-active-session payload');
    activeSessionId = typeof obj.sessionId === 'string' && obj.sessionId ? obj.sessionId : null;
    return { ok: true };
  });
  registerInvoke('rsb-browser-bridge:set-foreground', (payload) => {
    const obj = requireObject(payload, 'set-foreground payload');
    if (obj.tabId !== null && (typeof obj.tabId !== 'string' || obj.tabId === '')) {
      fail('INVALID_PARAMS', 'tabId must be a non-empty string or null');
    }
    foregroundTabId = obj.tabId;
    return { ok: true };
  });
  // 系统 webview 不给强杀单个页面进程的接口；资源看门狗也没有移植，所以这条路径不会被触发。
  registerInvoke('rsb-browser-bridge:force-kill', (payload) => {
    requireString(requireObject(payload, 'force-kill payload').tabId, 'tabId');
    fail('UNSUPPORTED_CAPABILITY', 'terminating a browser page is not available in this build');
  });

  // shim/webview.mjs：子 webview 里 window.open / target=_blank 被原生拒绝后转到这里，补归属后推给 renderer 开新标签页。
  registerInvoke('tauri-browser:popup', async (payload) => {
    const obj = requireObject(payload, 'popup payload');
    const webContentsId = requireId(obj.webContentsId, 'webContentsId');
    const url = requireString(obj.url, 'url');
    const disposition = typeof obj.disposition === 'string' && obj.disposition ? obj.disposition : 'foreground-tab';
    if (!isRoutablePopupUrl(url)) return { ok: false };
    let opener = recordById(webContentsId);
    if (!opener) {
      // 页面 head 里的同步脚本可能在 renderer report 之前就 window.open，等一会儿归属。
      opener = await new Promise((resolve) => {
        const waiter = (r) => {
          if (r.webContentsId !== webContentsId) return;
          done(r);
        };
        const timer = setTimeout(() => done(null), POPUP_OPENER_WAIT_MS);
        function done(r) {
          clearTimeout(timer);
          reportWaiters.delete(waiter);
          resolve(r);
        }
        reportWaiters.add(waiter);
      });
    }
    emitToRenderer('rsb:browser-popup', {
      url,
      disposition,
      ...(opener ? { openerTabId: opener.tabId, openerSessionId: opener.sessionId } : {}),
    });
    return { ok: true };
  });

  // ---------- rsb-native-popup ----------
  // desktop 把 Chromium 预建的弹窗 WebContents 收进原生视图；Tauri 版弹窗一律变成普通新标签页，没有原生弹窗面。
  registerInvoke('rsb-native-popup:claim', () => ({ alive: false }));
  registerInvoke('rsb-native-popup:set-bounds', () => ({ ok: true }));
  registerInvoke('rsb-native-popup:command', () => ({ ok: true }));
  registerInvoke('rsb-native-popup:close', () => ({ ok: true }));

  // ---------- browser-backend ----------
  // Agent 浏览器自动化（外置 Chrome / 侧栏 webview / 用户浏览器插件）还没有移植：选择照常保存，健康检查报不可用。
  const settings = createOverrideSettingsFile({
    file: path.join(dataDir, 'settings', 'browser-backend-settings.json'),
    defaults: { kind: 'external', useRealProfile: false },
    normalize: (raw) => ({
      kind: KINDS.includes(raw?.kind) ? raw.kind : 'external',
      useRealProfile: raw?.useRealProfile === true,
    }),
  });
  const active = () => settings.read().kind;
  const health = () => ({ active: active(), status: 'error', canRecover: false, reason: 'host-unavailable', errorCode: 'NOT_PORTED' });

  registerInvoke('browser-backend:get-state', () => {
    const s = settings.readState();
    return { active: s.value.kind, systemDefault: s.defaults.kind, isOverride: s.customizedKeys.includes('kind'), useRealProfile: s.value.useRealProfile };
  });
  registerInvoke('browser-backend:set-kind', (payload) => {
    const obj = requireObject(payload, 'set-kind payload');
    if (!KINDS.includes(obj.kind)) fail('INVALID_PARAMS', `kind must be one of ${KINDS.join(', ')}`);
    settings.writePatch({ kind: obj.kind }, { preserveDefaults: true });
    return { ok: true, active: active() };
  });
  registerInvoke('browser-backend:reset', () => {
    settings.reset();
    return { ok: true, active: active() };
  });
  registerInvoke('browser-backend:set-use-real-profile', (payload) => {
    const obj = requireObject(payload, 'set-use-real-profile payload');
    if (typeof obj.enabled !== 'boolean') fail('INVALID_PARAMS', 'enabled must be a boolean');
    // 复制系统 Chrome 登录态依赖外置 Chrome 后端，未移植。
    if (obj.enabled) fail('NO_CHROMIUM', 'copying browser logins is not available in this build');
    settings.writePatch({ useRealProfile: false });
    return { ok: true, enabled: false };
  });
  registerInvoke('browser-backend:get-health', () => health());
  registerInvoke('browser-backend:recover', () => ({ ok: false, health: health() }));
  registerInvoke('browser-backend:probe-source-read', () => ({ readable: false }));
  registerInvoke('browser-backend:open-extension-store', async () => {
    try {
      await callNative('open.url', { url: EXTENSION_STORE_URL });
      return { ok: true };
    } catch {
      return { ok: false, error: 'OPEN_FAILED' };
    }
  });

  return { _test: { records, pinned, state: () => ({ activeSessionId, foregroundTabId }) } };
}
