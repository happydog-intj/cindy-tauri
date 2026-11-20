// renderer shim 与 sidecar 共用的 IPC 编解码。
// Electron IPC 用结构化克隆，JSON 会丢掉 undefined、二进制和 Date；这里用带标签的对象保留它们：
//   undefined          → {"__cindy_t":"u"}
//   Uint8Array/Buffer/ArrayBuffer → {"__cindy_t":"b","v":"<base64>"}
//   Date               → {"__cindy_t":"d","v":<ms>}
// 解码时 undefined 在数组里留空位（读出来仍是 undefined），在对象里等同于缺省。
const TAG = '__cindy_t';

function toBase64(bytes) {
  if (typeof Buffer !== 'undefined') return Buffer.from(bytes.buffer, bytes.byteOffset, bytes.byteLength).toString('base64');
  let s = '';
  for (let i = 0; i < bytes.length; i += 0x8000) s += String.fromCharCode.apply(null, bytes.subarray(i, i + 0x8000));
  return btoa(s);
}

function fromBase64(text) {
  if (typeof Buffer !== 'undefined') return new Uint8Array(Buffer.from(text, 'base64'));
  const s = atob(text);
  const out = new Uint8Array(s.length);
  for (let i = 0; i < s.length; i += 1) out[i] = s.charCodeAt(i);
  return out;
}

function replacer(key, value) {
  // this[key] 是 toJSON 之前的原值（Buffer、Date 会先被 toJSON 改写）。
  const raw = this[key];
  if (raw === undefined) return { [TAG]: 'u' };
  if (raw instanceof Date) return { [TAG]: 'd', v: raw.getTime() };
  if (raw instanceof ArrayBuffer) return { [TAG]: 'b', v: toBase64(new Uint8Array(raw)) };
  if (ArrayBuffer.isView(raw)) return { [TAG]: 'b', v: toBase64(new Uint8Array(raw.buffer, raw.byteOffset, raw.byteLength)) };
  return value;
}

function reviver(_key, value) {
  if (value && typeof value === 'object' && typeof value[TAG] === 'string') {
    if (value[TAG] === 'u') return undefined;
    if (value[TAG] === 'd') return new Date(value.v);
    if (value[TAG] === 'b') return fromBase64(value.v);
  }
  return value;
}

export function encode(value) {
  return JSON.stringify({ v: value }, replacer);
}

export function decode(text) {
  if (typeof text !== 'string' || !text) return undefined;
  return JSON.parse(text, reviver).v;
}
