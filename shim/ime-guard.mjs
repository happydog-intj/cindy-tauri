// WKWebView（Safari 内核）的输入法回车：提交组字的那次 Enter 先触发 compositionend，再发一个 keydown，
// 此时 isComposing 已是 false（只有 keyCode 仍为 229）。上游 renderer 只判断 !isComposing（Chromium 下够用），
// 在这里会把「选词确认」当成「发送 / 保存」。捕获阶段先拦掉这类 Enter，不 preventDefault（输入法已消费该键）。
const AFTER_COMPOSITION_MS = 50;

export function installImeGuard() {
  let lastCompositionEnd = -Infinity;
  window.addEventListener('compositionend', () => { lastCompositionEnd = performance.now(); }, true);
  window.addEventListener(
    'keydown',
    (e) => {
      if (e.key !== 'Enter') return;
      const composing = e.isComposing || e.keyCode === 229 || performance.now() - lastCompositionEnd < AFTER_COMPOSITION_MS;
      if (composing) e.stopImmediatePropagation();
    },
    true,
  );
}
