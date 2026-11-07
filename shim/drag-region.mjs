// Electron 的 -webkit-app-region: drag 在 Tauri 的 WebView 里不生效，这里把它换成 startDragging。
// renderer 用两种写法标拖动区：React 内联 style（WebkitAppRegion，WebView 不认识时成了 style 对象上的普通属性）
// 和 Tailwind 类名（[-webkit-app-region:drag]）。从按下的元素往上找，最近一个声明的值说了算。
const DRAG_CLASS = '[-webkit-app-region:drag]';
const NO_DRAG_CLASS = '[-webkit-app-region:no-drag]';
const INTERACTIVE = 'button, a, input, textarea, select, [contenteditable=""], [contenteditable="true"]';

function regionOf(el) {
  const inline = el.style && (el.style.WebkitAppRegion || el.style.webkitAppRegion);
  if (inline === 'drag' || inline === 'no-drag') return inline;
  const cls = el.classList;
  if (cls) {
    if (cls.contains(NO_DRAG_CLASS)) return 'no-drag';
    if (cls.contains(DRAG_CLASS)) return 'drag';
    // [&_button]:[-webkit-app-region:no-drag]：祖先声明“内部按钮不拖动”，按交互元素统一处理。
  }
  const computed = getComputedStyle(el).getPropertyValue('-webkit-app-region').trim();
  if (computed === 'drag' || computed === 'no-drag') return computed;
  return null;
}

function isDragTarget(target) {
  if (target.closest(INTERACTIVE)) return false;
  for (let el = target; el && el.nodeType === 1; el = el.parentElement) {
    const region = regionOf(el);
    if (region) return region === 'drag';
  }
  return false;
}

// renderer 的 useManualWindowDrag（no-drag 元素上按住移动超过死区）会经 IPC 发 window-drag-move-start，
// desktop 由 main 手动跟随光标。Tauri 的 startDragging 必须在按键仍按下时调用，这里在发送端直接拦下。
function installManualDragBridge(startDrag) {
  const api = window.electronAPI;
  if (!api || typeof api.windowDragMoveStart !== 'function') return;
  api.windowDragMoveStart = () => startDrag();
  api.windowDragMoveStop = () => {};
}

export function installDragRegionShim() {
  const internals = window.__TAURI_INTERNALS__;
  if (!internals) return;
  const label = internals.metadata?.currentWindow?.label ?? 'main';
  const startDrag = () => internals.invoke('plugin:window|start_dragging', { label }).catch((err) => console.warn('[drag-region]', err));
  installManualDragBridge(startDrag);
  document.addEventListener('mousedown', (e) => {
    if (e.button !== 0 || !(e.target instanceof Element) || !isDragTarget(e.target)) return;
    if (e.detail === 2) {
      internals.invoke('plugin:window|toggle_maximize', { label }).catch(() => {});
      return;
    }
    e.preventDefault();
    startDrag();
  });
}
