// renderer 里的 Electron <webview> 替身（P3 浏览器面板）。
// document.createElement('webview') 返回自定义元素 <cindy-webview>：元素本身只占位，真正的网页是主窗口里的
// Tauri 子 webview（src-tauri/src/browser.rs），按元素在页面上的位置和可见性摆放：
//   - 元素可见（已挂到 DOM、有尺寸、祖先没有 display:none / opacity:0 / visibility:hidden、不在浏览器池停车区）
//     → 子 webview 摆到元素的位置；否则隐藏。
//   - 原生视图总是盖在页面上面，下拉菜单 / 对话框和它重叠时暂时隐藏子 webview（降级，见 DESIGN.md §10）。
// 元素方法和事件对齐 desktop 用到的 WebviewTag 子集；页面事件来自 Rust 的 "cindy:webview" 事件。
import { ipcRenderer } from './electron.mjs';

const internals = () => window.__TAURI_INTERNALS__;
const invoke = (cmd, args) => internals().invoke(cmd, args);

const TAG = 'cindy-webview';
// 浏览器池停车区（desktop browserWebviewPool.ts POOL_CONTAINER_ID）：里面的 webview 一律视为不可见。
const POOL_CONTAINER_SELECTOR = '#browser-webview-pool';
// 这些浮层和子 webview 重叠时隐藏子 webview，否则会被原生视图挡住。
const OVERLAY_SELECTOR = [
  '[data-radix-popper-content-wrapper]',
  '[role="dialog"]',
  '[role="alertdialog"]',
  '[role="menu"]',
  '[role="listbox"]',
].join(',');
// 页面内跳转（hash / pushState）没有原生事件，定时读 URL 补发 did-navigate-in-page。
const POLL_MS = 500;
const CHECK_MS = 250;

const elements = new Map(); // id → 元素
let nextId = 0;
let resetDone = null;
let eventsBound = false;
let installed = false;

function allocId() {
  // renderer 重新加载后上一代的 label 可能还没销毁完，起点随机，避免撞号。
  if (nextId === 0) nextId = (crypto.getRandomValues(new Uint32Array(1))[0] % 1_000_000_000) + 1;
  return nextId++;
}

function ensureBackend() {
  if (!resetDone) {
    // 新 renderer 启动时清掉上一代留下的子 webview（它们已经没有宿主元素）。
    resetDone = invoke('browser_reset').catch((err) => console.warn('[cindy-webview] reset failed', err));
  }
  if (!eventsBound) {
    eventsBound = true;
    const handler = internals().transformCallback((evt) => {
      const msg = evt?.payload;
      const el = msg && elements.get(msg.id);
      if (el) el._native(msg);
    });
    invoke('plugin:event|listen', { event: 'cindy:webview', target: { kind: 'Any' }, handler })
      .catch((err) => console.error('[cindy-webview] event listen failed', err));
  }
  return resetDone;
}

function makeEvent(type, fields) {
  const ev = new Event(type);
  if (fields) {
    for (const [k, v] of Object.entries(fields)) {
      try {
        Object.defineProperty(ev, k, { value: v, enumerable: true });
      } catch { /* 只读字段，忽略 */ }
    }
  }
  return ev;
}

const notAttached = () =>
  new Error('The WebView must be attached to the DOM and the dom-ready event emitted before this method can be called.');

// ---------- 位置与可见性 ----------

function intersect(a, b) {
  const x1 = Math.max(a.x1, b.x1);
  const y1 = Math.max(a.y1, b.y1);
  const x2 = Math.min(a.x2, b.x2);
  const y2 = Math.min(a.y2, b.y2);
  return { x1, y1, x2, y2 };
}

// 元素在窗口里的可见区域（CSS px = 逻辑像素）；不可见返回 null。
function visibleBounds(el) {
  if (!el.isConnected || el.closest(POOL_CONTAINER_SELECTOR)) return null;
  const r = el.getBoundingClientRect();
  if (r.width < 1 || r.height < 1) return null;
  let box = { x1: r.left, y1: r.top, x2: r.right, y2: r.bottom };
  for (let n = el; n && n !== document.documentElement; n = n.parentElement) {
    const cs = getComputedStyle(n);
    if (cs.display === 'none' || cs.visibility === 'hidden' || Number(cs.opacity) === 0) return null;
    if (n !== el && cs.overflow !== 'visible') {
      const p = n.getBoundingClientRect();
      box = intersect(box, { x1: p.left, y1: p.top, x2: p.right, y2: p.bottom });
    }
  }
  box = intersect(box, { x1: 0, y1: 0, x2: window.innerWidth, y2: window.innerHeight });
  if (box.x2 - box.x1 < 1 || box.y2 - box.y1 < 1) return null;
  return { x: box.x1, y: box.y1, width: box.x2 - box.x1, height: box.y2 - box.y1 };
}

function coveredByOverlay(el, b) {
  for (const o of document.querySelectorAll(OVERLAY_SELECTOR)) {
    // 包含子 webview 的对话框（如验证码弹层）和工具提示不算遮挡；提示条很小，隐藏整个网页反而闪烁。
    if (o.contains(el) || o.querySelector('[role="tooltip"]')) continue;
    const r = o.getBoundingClientRect();
    if (r.width < 1 || r.height < 1) continue;
    if (r.left < b.x + b.width && r.right > b.x && r.top < b.y + b.height && r.bottom > b.y) return true;
  }
  return false;
}

function placement(el) {
  const b = visibleBounds(el);
  if (!b || coveredByOverlay(el, b)) return null;
  return b;
}

let checkScheduled = false;
function scheduleCheck() {
  if (checkScheduled) return;
  checkScheduled = true;
  requestAnimationFrame(() => {
    checkScheduled = false;
    for (const el of elements.values()) el._place();
  });
}

let watchersInstalled = false;
function installWatchers() {
  if (watchersInstalled) return;
  watchersInstalled = true;
  // 切换页签（class hidden）、拖动分栏、弹出菜单都会改 DOM；合并到下一帧统一检查。
  new MutationObserver(scheduleCheck).observe(document.documentElement, {
    subtree: true,
    childList: true,
    attributes: true,
    attributeFilter: ['class', 'style', 'hidden', 'open', 'data-state'],
  });
  window.addEventListener('resize', scheduleCheck);
  window.addEventListener('scroll', scheduleCheck, true);
  // 兜底：CSS 动画 / 过渡不一定触发上面的信号。
  setInterval(scheduleCheck, CHECK_MS);
  setInterval(() => {
    for (const el of elements.values()) el._poll();
  }, POLL_MS);
}

// ---------- executeJavaScript ----------

// 子页面的结果统一包成字符串再回传，避免宿主序列化不了的值（DOM 节点、Date 等）。
// 能当表达式解析的脚本拿到返回值和异常；语句形式的脚本只知道是否执行完（返回 undefined）。
function wrapScript(code) {
  const trimmed = code.trim().replace(/;+\s*$/, '');
  let expression = true;
  try {
    // 只做语法检查，不执行（主 renderer 没有 CSP 限制）。
    // eslint-disable-next-line no-new-func
    new Function(`return (\n${trimmed}\n)`);
  } catch {
    expression = false;
  }
  if (expression) {
    return `(() => { try { const v = (\n${trimmed}\n);`
      + ` if (v && typeof v.then === 'function') return JSON.stringify({ v: null });`
      + ` return JSON.stringify({ v: v === undefined ? null : v, u: v === undefined }); }`
      + ` catch (e) { return JSON.stringify({ e: String(e && e.message || e) }); } })()`;
  }
  return `${code}\n;'{"v":null,"u":true}'`;
}

function unwrapResult(raw) {
  let text = raw;
  try {
    text = JSON.parse(raw);
  } catch { /* 不是 JSON 字符串 */ }
  if (typeof text !== 'string' || !text) throw new Error('Script failed to execute');
  const r = JSON.parse(text);
  if (r.e !== undefined) throw new Error(r.e);
  return r.u ? undefined : r.v;
}

// ---------- 元素 ----------

class CindyWebview extends HTMLElement {
  static get observedAttributes() {
    return ['src'];
  }

  constructor() {
    super();
    this._id = 0;
    this._state = 'idle'; // idle | creating | attached
    this._closeAfterCreate = false;
    this._shown = null; // 上次摆放的 bounds（null = 隐藏）
    this._url = '';
    this._title = '';
    this._loading = false;
    this._canGoBack = false;
    this._canGoForward = false;
    this._pending = []; // 创建完成前到达的页面事件
    this._queue = Promise.resolve();
    this._polling = false;
    this._resizeObserver = null;
  }

  connectedCallback() {
    installWatchers();
    if (!this._resizeObserver) {
      this._resizeObserver = new ResizeObserver(scheduleCheck);
      this._resizeObserver.observe(this);
    }
    this._closeAfterCreate = false;
    if (this._state === 'idle') void this._create();
    scheduleCheck();
  }

  disconnectedCallback() {
    // 池在停车区和页签之间移动 wrapper 是“拔下再插上”，同一轮里重新挂上就不销毁。
    setTimeout(() => {
      if (!this.isConnected) this._destroy();
    }, 0);
    scheduleCheck();
  }

  attributeChangedCallback(name, oldValue, value) {
    if (name !== 'src' || !value || value === oldValue) return;
    if (this._state === 'attached') void this._navigate(value).catch(() => undefined);
  }

  get src() {
    return this.getAttribute('src') ?? '';
  }

  set src(v) {
    this.setAttribute('src', String(v));
  }

  get partition() {
    return this.getAttribute('partition') ?? '';
  }

  async _create() {
    this._state = 'creating';
    const id = allocId();
    this._id = id;
    elements.set(id, this);
    const initialUrl = this.getAttribute('src') || 'about:blank';
    const b = placement(this);
    try {
      await ensureBackend();
      await invoke('browser_create', {
        id,
        url: initialUrl,
        partition: this.partition,
        bounds: b ?? { x: 0, y: 0, width: 1280, height: 720 },
        visible: !!b,
      });
    } catch (err) {
      elements.delete(id);
      this._state = 'idle';
      this._id = 0;
      console.warn('[cindy-webview] create failed', err);
      this.dispatchEvent(makeEvent('did-fail-load', {
        errorCode: -2, errorDescription: String(err), validatedURL: initialUrl, isMainFrame: true,
      }));
      return;
    }
    this._shown = b;
    this._state = 'attached';
    if (this._closeAfterCreate) {
      this._destroy();
      return;
    }
    this._url = initialUrl === 'about:blank' ? '' : initialUrl;
    this.dispatchEvent(makeEvent('did-attach'));
    const pending = this._pending.splice(0);
    for (const msg of pending) this._native(msg);
    const src = this.getAttribute('src');
    if (src && src !== initialUrl) void this._navigate(src).catch(() => undefined);
    scheduleCheck();
  }

  _destroy() {
    if (this._state === 'creating') {
      this._closeAfterCreate = true;
      return;
    }
    if (this._state !== 'attached') return;
    const id = this._id;
    elements.delete(id);
    this._state = 'idle';
    this._id = 0;
    this._shown = null;
    this._loading = false;
    invoke('browser_op', { id, op: 'close' }).catch(() => undefined);
  }

  _op(op, arg) {
    if (this._state !== 'attached') return Promise.reject(notAttached());
    return invoke('browser_op', { id: this._id, op, arg: arg ?? null });
  }

  _place() {
    if (this._state !== 'attached') return;
    const b = placement(this);
    const prev = this._shown;
    if (!b && !prev) return;
    if (b && prev && Math.abs(b.x - prev.x) < 0.5 && Math.abs(b.y - prev.y) < 0.5
      && Math.abs(b.width - prev.width) < 0.5 && Math.abs(b.height - prev.height) < 0.5) return;
    this._shown = b;
    if (!b) {
      this._op('hide').catch(() => undefined);
      return;
    }
    this._op('setBounds', b).then(() => (prev ? undefined : this._op('show'))).catch(() => undefined);
  }

  async _refreshState() {
    try {
      const s = await this._op('state');
      this._canGoBack = !!s.canGoBack;
      this._canGoForward = !!s.canGoForward;
      return s;
    } catch {
      return null;
    }
  }

  // 定时读 URL：页面内跳转补 did-navigate-in-page；macOS 加载失败没有完成事件，靠 isLoading 收尾。
  _poll() {
    if (this._state !== 'attached' || this._polling || (!this._shown && !this._loading)) return;
    this._polling = true;
    this._enqueue(async () => {
      try {
        const s = await this._refreshState();
        if (!s || this._state !== 'attached') return;
        if (!this._loading && s.url && this._url && s.url !== this._url) {
          this._url = s.url;
          this.dispatchEvent(makeEvent('did-navigate-in-page', { url: s.url, isMainFrame: true }));
        }
        if (this._loading && s.isLoading === false) {
          this._loading = false;
          this.dispatchEvent(makeEvent('did-stop-loading'));
        }
      } finally {
        this._polling = false;
      }
    });
  }

  _enqueue(fn) {
    this._queue = this._queue.then(fn).catch((err) => console.warn('[cindy-webview] event failed', err));
  }

  _native(msg) {
    if (this._state === 'creating') {
      this._pending.push(msg);
      return;
    }
    if (this._state !== 'attached' || msg.id !== this._id) return;
    this._enqueue(() => this._handle(msg));
  }

  async _handle(msg) {
    // 没设 src 时子 webview 先停在 about:blank；Electron 的空 <webview> 不产生导航事件，这里也不报。
    if ((msg.type === 'load-started' || msg.type === 'load-finished') && msg.url === 'about:blank'
      && this.getAttribute('src') !== 'about:blank') return;
    switch (msg.type) {
      case 'load-started': {
        // macOS / Windows 的“开始”都在导航提交之后，所以同时发 did-navigate。
        this._loading = true;
        this.dispatchEvent(makeEvent('did-start-loading'));
        if (msg.url) this._url = msg.url;
        await this._refreshState();
        this.dispatchEvent(makeEvent('did-navigate', { url: this._url, httpResponseCode: 200, httpStatusText: 'OK' }));
        break;
      }
      case 'load-finished': {
        if (msg.url) this._url = msg.url;
        await this._refreshState();
        this._loading = false;
        this.dispatchEvent(makeEvent('dom-ready'));
        this.dispatchEvent(makeEvent('did-finish-load'));
        this.dispatchEvent(makeEvent('did-stop-loading'));
        void this._readFavicons();
        break;
      }
      case 'title':
        this._title = String(msg.title ?? '');
        this.dispatchEvent(makeEvent('page-title-updated', { title: this._title, explicitSet: true }));
        break;
      case 'new-window':
        // desktop 只给带 allowpopups 的 webview 开新标签页，其余弹窗直接丢弃。
        if (!this.hasAttribute('allowpopups') || typeof msg.url !== 'string') break;
        // sidecar 按 rsb-browser-bridge 登记的归属补上 openerTabId / openerSessionId，再推 rsb:browser-popup。
        ipcRenderer
          .invoke('tauri-browser:popup', { webContentsId: this._id, url: msg.url, disposition: 'foreground-tab' })
          .catch((err) => console.warn('[cindy-webview] popup routing failed', err));
        break;
      default:
        break;
    }
  }

  async _readFavicons() {
    if (!/^https?:/.test(this._url)) return;
    try {
      const icons = await this.executeJavaScript(
        `Array.from(document.querySelectorAll('link[rel~="icon"]')).map((l) => l.href).filter(Boolean)`,
      );
      const list = Array.isArray(icons) ? icons.filter((s) => typeof s === 'string') : [];
      if (list.length === 0) list.push(new URL('/favicon.ico', this._url).href);
      this.dispatchEvent(makeEvent('page-favicon-updated', { favicons: list }));
    } catch { /* 页面已跳走或脚本被拦 */ }
  }

  _navigate(url) {
    this._url = url;
    return this._op('navigate', { url });
  }

  // ----- WebviewTag API 子集 -----

  getWebContentsId() {
    if (this._state !== 'attached') throw notAttached();
    return this._id;
  }

  loadURL(url) {
    if (this._state !== 'attached') throw notAttached();
    // 调用方多半不 await（desktop 的 navigate），挂一个空 catch 避免未处理的 rejection；await 的调用方照样收到错误。
    const p = this._navigate(String(url)).then(() => undefined);
    p.catch(() => undefined);
    return p;
  }

  getURL() {
    if (this._state !== 'attached') throw notAttached();
    return this._url;
  }

  getTitle() {
    return this._title;
  }

  isLoading() {
    return this._loading;
  }

  canGoBack() {
    if (this._state !== 'attached') throw notAttached();
    return this._canGoBack;
  }

  canGoForward() {
    if (this._state !== 'attached') throw notAttached();
    return this._canGoForward;
  }

  goBack() {
    this._op('back').catch(() => undefined);
  }

  goForward() {
    this._op('forward').catch(() => undefined);
  }

  reload() {
    if (this._state !== 'attached') throw notAttached();
    this._op('reload').catch(() => undefined);
  }

  stop() {
    if (this._state !== 'attached') return;
    this._op('stop').catch(() => undefined);
    if (this._loading) {
      this._loading = false;
      this.dispatchEvent(makeEvent('did-stop-loading'));
    }
  }

  focus() {
    if (this._state === 'attached' && this._shown) this._op('focus').catch(() => undefined);
  }

  executeJavaScript(code) {
    if (this._state !== 'attached') return Promise.reject(notAttached());
    return this._op('eval', { code: wrapScript(String(code)) }).then(unwrapResult);
  }

  insertCSS(css) {
    const key = `cindy-css-${Math.random().toString(36).slice(2)}`;
    const script = `(() => { const s = document.createElement('style'); s.dataset.cindyCss = ${JSON.stringify(key)};`
      + ` s.textContent = ${JSON.stringify(String(css))}; (document.head || document.documentElement).appendChild(s); return 0; })()`;
    return this.executeJavaScript(script).then(() => key);
  }

  removeInsertedCSS(key) {
    return this.executeJavaScript(
      `(() => { document.querySelectorAll('style[data-cindy-css=${JSON.stringify(String(key))}]').forEach((s) => s.remove()); return 0; })()`,
    ).then(() => undefined);
  }

  // Tauri 子 webview 没有这些能力，保持可调用、无效果。
  openDevTools() {}
  closeDevTools() {}
  isDevToolsOpened() {
    return false;
  }
  setAudioMuted() {}
  isAudioMuted() {
    return false;
  }
  isCurrentlyAudible() {
    return false;
  }
  setZoomFactor() {}
  getZoomFactor() {
    return 1;
  }
  setZoomLevel() {}
  getZoomLevel() {
    return 0;
  }
  send(channel) {
    console.warn(`[cindy-webview] send(${channel}) is not supported`);
  }
}

function define() {
  if (customElements.get(TAG)) return;
  customElements.define(TAG, CindyWebview);
  // Electron <webview> 默认 display:flex（作者样式里的 display:none 等仍然生效）。
  const style = document.createElement('style');
  style.textContent = `${TAG} { display: flex; }`;
  (document.head ?? document.documentElement).prepend(style);
}

export function installWebviewShim() {
  if (installed) return;
  installed = true;
  const original = Document.prototype.createElement;
  Document.prototype.createElement = function createElement(tag, options) {
    if (typeof tag === 'string' && tag.toLowerCase() === 'webview') {
      define();
      return original.call(this, TAG, options);
    }
    return original.call(this, tag, options);
  };
}

// 冒烟测试用。
export const _webviewShimTest = { wrapScript, unwrapResult, elements };
