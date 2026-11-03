// 编译 cindy preload.ts 时替换 `electron` 模块。preload 的代码原样运行，
// ipcRenderer 的调用改走 Tauri：
//   invoke   → Tauri command ipc_invoke（异步）
//   send     → Tauri command ipc_send（不等结果）
//   sendSync → 同步 XHR 访问 cindy-ipc 自定义协议
//   on       → 订阅 Rust 转发的 Tauri 事件 "cindy:ipc"，按 channel 分发
import { decode, encode } from '../shared/ipc-codec.mjs';

const internals = () => window.__TAURI_INTERNALS__;
const boot = () => window.__CINDY_BOOT__ ?? {};

function invokeError(channel, err) {
  const msg = typeof err === 'string' ? err : String(err?.message ?? err);
  const e = new Error(`Error invoking remote method '${channel}': ${msg}`);
  // 保留主进程错误码（形如 "Error: [CODE] message"），renderer 依赖这个格式判断错误类型。
  const code = /\[([A-Z0-9_]+)\]/.exec(msg)?.[1];
  if (code) e.code = code;
  return e;
}

// IpcRendererEvent 的最小替身。sender 只给 preload 里可能读到的字段。
function fakeEvent(channel) {
  return { sender: ipcRenderer, senderId: 0, ports: [], channel };
}

const listeners = new Map(); // channel → Set<fn>
let eventBound = false;

function bindEvents() {
  if (eventBound) return;
  eventBound = true;
  const handler = internals().transformCallback((evt) => {
    const { channel, payload } = evt?.payload ?? {};
    const set = listeners.get(channel);
    if (!set || set.size === 0) return;
    const args = decode(payload) ?? [];
    for (const fn of [...set]) {
      try {
        fn(fakeEvent(channel), ...args);
      } catch (err) {
        queueMicrotask(() => { throw err; });
      }
    }
  });
  internals()
    .invoke('plugin:event|listen', { event: 'cindy:ipc', target: { kind: 'Any' }, handler })
    .catch((err) => console.error('[cindy-shim] event listen failed', err));
}

function syncUrl(channel) {
  const base = boot().platform === 'win32' ? 'http://cindy-ipc.localhost' : 'cindy-ipc://localhost';
  return `${base}/sync/${encodeURIComponent(channel)}`;
}

export const ipcRenderer = {
  invoke(channel, ...args) {
    return internals()
      .invoke('ipc_invoke', { channel, payload: encode(args) })
      .then(decode, (err) => {
        throw invokeError(channel, err);
      });
  },
  send(channel, ...args) {
    internals()
      .invoke('ipc_send', { channel, payload: encode(args) })
      .catch((err) => console.warn(`[cindy-shim] send ${channel} failed:`, err));
  },
  sendSync(channel, ...args) {
    const xhr = new XMLHttpRequest();
    // 参数放在请求体里；同步调用的参数都很小。
    xhr.open('POST', syncUrl(channel), false);
    xhr.setRequestHeader('Content-Type', 'text/plain');
    xhr.send(encode(args));
    if (xhr.status !== 200) {
      console.warn(`[cindy-shim] sendSync ${channel} failed: ${xhr.status} ${xhr.responseText}`);
      return undefined;
    }
    return decode(xhr.responseText);
  },
  on(channel, fn) {
    bindEvents();
    if (!listeners.has(channel)) listeners.set(channel, new Set());
    listeners.get(channel).add(fn);
    return ipcRenderer;
  },
  once(channel, fn) {
    const wrapped = (...a) => {
      ipcRenderer.removeListener(channel, wrapped);
      fn(...a);
    };
    return ipcRenderer.on(channel, wrapped);
  },
  removeListener(channel, fn) {
    listeners.get(channel)?.delete(fn);
    return ipcRenderer;
  },
  off(channel, fn) {
    return ipcRenderer.removeListener(channel, fn);
  },
  removeAllListeners(channel) {
    if (channel) listeners.delete(channel);
    else listeners.clear();
    return ipcRenderer;
  },
  // 只用于 <webview> guest 页，Tauri 版没有 <webview>（P3 处理）。
  sendToHost(channel) {
    console.warn(`[cindy-shim] sendToHost(${channel}) is not supported`);
  },
};

export const contextBridge = {
  exposeInMainWorld(key, api) {
    Object.defineProperty(window, key, { value: api, configurable: false, enumerable: true, writable: false });
  },
};

// Tauri 的 webview 里拿不到 File 的本地路径；拖拽文件改走 Tauri 的 drag-drop 事件（后续实现）。
export const webUtils = {
  getPathForFile() {
    return '';
  },
};
