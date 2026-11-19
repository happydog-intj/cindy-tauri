// 黄金生成用的 ipc 替身：收集 registerInvoke 的处理器与推给 renderer 的事件。
// 状态挂 globalThis：bundle 内联的副本与生成器 import 的副本共享同一份。
const g = (globalThis.__ldbStub ??= { handlers: new Map(), events: [] });
export const handlers = g.handlers;
export const events = g.events;
export function registerInvoke(ch, fn) { handlers.set(ch, fn); }
export function emitToRenderer(channel, payload) { events.push({ channel, payload: JSON.parse(JSON.stringify(payload ?? null)) }); }
