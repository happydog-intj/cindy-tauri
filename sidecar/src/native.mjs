// sidecar → Rust 的反向调用：原生对话框、打开链接 / 文件、系统通知、角标、托盘文案。
// - 需要结果：{"native":{"id":n,"method":"...","params":{...}}}，Rust 回 {"nativeReply":{"id":n,"result"|"error"}}；
// - 不需要结果：{"native":{"method":"...","params":{...}}}（无 id，Rust 不回）。
// nativeReply 只能由 Rust 写进 stdin：webview 经 sidecar_call 发来的只会是 {id,method,params}。
let writeLine = () => {};
let seq = 0;
const pending = new Map();

// 默认超时：普通原生调用 30 秒；对话框等用户操作的调用传 timeoutMs: 0（不超时）。
const DEFAULT_TIMEOUT_MS = 30_000;

export function setNativeWriter(fn) {
  writeLine = fn;
}

export function callNative(method, params = {}, { timeoutMs = DEFAULT_TIMEOUT_MS } = {}) {
  const id = ++seq;
  return new Promise((resolve, reject) => {
    let timer = null;
    if (timeoutMs > 0) {
      timer = setTimeout(() => {
        pending.delete(id);
        reject(new Error(`[INTERNAL] native ${method} timed out`));
      }, timeoutMs);
      timer.unref?.();
    }
    pending.set(id, { resolve, reject, timer });
    writeLine({ native: { id, method, params } });
  });
}

export function notifyNative(method, params = {}) {
  writeLine({ native: { method, params } });
}

// index.mjs 收到 nativeReply 行时调用。
export function handleNativeReply(reply) {
  const id = reply?.id;
  const p = pending.get(id);
  if (!p) return;
  pending.delete(id);
  if (p.timer) clearTimeout(p.timer);
  if (reply.error !== undefined) p.reject(new Error(String(reply.error)));
  else p.resolve(reply.result ?? null);
}
