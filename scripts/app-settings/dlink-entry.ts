// device-link 被控端的纯协议部分：允许列表（数据）+ 黄金样例（运行上游 allowlist/topics/transport）。
import { REMOTE_INVOKE_ALLOWLIST, PUSH_FORWARD_ALLOWLIST, computeAllowlistHash } from '../../frontend/packages/device-link/src/allowlist.ts';
import { encodeModelSpeedPayload } from '../../frontend/packages/device-link/src/sessionListTransport.ts';
import { topicForPush } from '../../frontend/packages/device-link/src/topics.ts';
import {
  encodeReliableFrames, parseTransportPayload, makeTransportAck, parseTransportAck, makeTransportSkipPayload,
  reliableRetryDelayMs, retransmitBaseMs, updateRetransmitEstimate, updateDeliveryEstimate, byteLength,
  MAX_TRANSPORT_CHUNK_BYTES,
} from '../../frontend/packages/device-link/src/transport.ts';

const norm = (v: any) => JSON.parse(JSON.stringify(v === undefined ? null : v));
const run = (fn: () => unknown) => { try { return { ok: true, v: norm(fn()) }; } catch (e: any) { return { ok: false, v: String(e?.message ?? e) }; } };

// topic 样例：所有推送白名单通道 × 一批 payload 形状
const payloads: unknown[] = [
  undefined, null, 1, 'x', [], {}, { sessionId: 's1' }, { sessionId: '' }, { sessionId: 3 }, { workdir: '/a/b' }, { workdir: '' },
  { parentSessionId: 'p1' }, { leadSessionId: 'l1' }, { sessionId: 's1', workdir: '/w', parentSessionId: 'p', leadSessionId: 'l' },
];
const channels = [...PUSH_FORWARD_ALLOWLIST, 'maker:file-browser:event', 'maker:bot-delegation:changed', 'maker:orca:worker-changed', 'unknown:channel', ''];
const topics: any[] = [];
for (const ch of channels) for (const p of payloads) topics.push({ channel: ch, payload: norm(p), topic: topicForPush(ch, p) });

// 可靠传输编码：含多字节、分片边界、baseSeq
const mk = (payload: unknown) => ({ v: 1, kind: 'invoke-result', id: 'r1', dst: 'd1', payload } as any);
const big = (n: number, ch = 'a') => ch.repeat(n);
const encodeCases: any[] = [];
for (const [name, payload] of [
  ['small', { ok: true, result: 1 }], ['cjk', { t: '中文字符串测试' }], ['emoji', { t: '😀😀😀' }],
  ['edge', { t: big(MAX_TRANSPORT_CHUNK_BYTES - 20) }], ['two', { t: big(MAX_TRANSPORT_CHUNK_BYTES + 10) }],
  ['cjk-split', { t: big(60000, '中') }], ['emoji-split', { t: big(40000, '😀') }], ['undefined', undefined],
  ['huge', { t: big(MAX_TRANSPORT_CHUNK_BYTES * 33) }], ['maxok', { t: big(4 * 1024 * 1024 - 20) }],
] as const) {
  for (const [seq, base] of [[1, 1], [5, 3], [64, 64], [0, 1], [3, 4]]) {
    const r = run(() => encodeReliableFrames(mk(payload), 'stream-abcdef', seq, base));
    // 大结果只存摘要，避免黄金文件膨胀：帧数、每帧 data 字节数、meta
    const v: any = r.v;
    encodeCases.push({
      name, seq, base, payload: JSON.stringify(payload) === undefined || JSON.stringify(payload).length > 400 ? null : norm(payload),
      big: JSON.stringify(payload ?? null).length > 400 ? { kind: name } : null,
      ok: r.ok,
      frames: r.ok ? v.map((f: any) => ({ meta: f.payload.__cindyDeviceLinkTransport, bytes: byteLength(f.payload.data), head: f.payload.data.slice(0, 12), tail: f.payload.data.slice(-12) })) : null,
      error: r.ok ? null : v,
    });
  }
}

// 解析：合法 / 非法 / 边界
const meta = (o: any = {}) => ({ version: 1, streamId: 'stream-abcdef', seq: 3, ...o });
const wrap = (m: any, data: any = '{}') => ({ __cindyDeviceLinkTransport: m, data });
const parseInputs: unknown[] = [
  null, 1, 'x', [], {}, wrap(meta()), wrap(meta({ baseSeq: 2 })), wrap(meta({ baseSeq: 4 })), wrap(meta({ baseSeq: 0 })), wrap(meta({ version: 2 })),
  wrap(meta({ streamId: '' })), wrap(meta({ streamId: 'x'.repeat(129) })), wrap(meta({ streamId: 'x'.repeat(128) })), wrap(meta({ seq: 0 })), wrap(meta({ seq: 1.5 })),
  wrap(meta({ seq: '3' })), wrap(meta(), 5), wrap(meta({ segment: { index: 0, total: 2, totalBytes: 200000 } })),
  wrap(meta({ segment: { index: 1, total: 2, totalBytes: 200000 } })), wrap(meta({ segment: { index: 2, total: 2, totalBytes: 200000 } })),
  wrap(meta({ segment: { index: 0, total: 1, totalBytes: 200000 } })), wrap(meta({ segment: { index: 0, total: 65, totalBytes: 200000 } })),
  wrap(meta({ segment: { index: 0, total: 2, totalBytes: 100 } })), wrap(meta({ segment: { index: 0, total: 2, totalBytes: 4 * 1024 * 1024 + 1 } })),
  wrap(meta({ segment: { index: 0, total: 2, totalBytes: 2 * MAX_TRANSPORT_CHUNK_BYTES + 1 } })), wrap(meta({ segment: 3 })), wrap(meta({ segment: { index: -1, total: 2, totalBytes: 200000 } })),
  makeTransportSkipPayload(), { data: 'x' },
];
const parseCases = parseInputs.map((p) => ({ input: norm(p), out: norm(parseTransportPayload(p)) }));

// ACK 编解码
const ackMakeCases: any[] = [];
for (const compact of [false, true]) for (const delayed of [false, true]) for (const link of [undefined, 'link-request-0123456789']) {
  for (const [stream, seq] of [['stream-abcdef-ghijkl-mnop', 7], ['short', 0]] as const) {
    ackMakeCases.push({ stream, seq, link: link ?? null, compact, delayed, out: norm(makeTransportAck('dst', stream, seq, link, compact, delayed)) });
  }
}
const ackParseInputs: any[] = [
  ...ackMakeCases.map((c) => c.out),
  { v: 1, kind: 'push', payload: null }, { v: 1, kind: 'invoke', payload: {} },
  { v: 1, kind: 'push', payload: { channel: '__cindy/device-link/transport-ack', payload: { streamId: 's', ackSeq: -1 } } },
  { v: 1, kind: 'push', payload: { channel: '__cindy/device-link/transport-ack', payload: { streamId: 's', ackSeq: 1.5 } } },
  { v: 1, kind: 'push', payload: { channel: '__cindy/device-link/transport-ack', payload: { streamId: 's', ackSeq: 3, linkRequestId: '' } } },
  { v: 1, kind: 'push', payload: { channel: '__cindy/device-link/transport-ack', payload: { streamId: 's', ackSeq: 3, linkRequestId: 'x'.repeat(257) } } },
  { v: 1, kind: 'push', payload: { channel: '__cindy/device-link/transport-ack', payload: { streamId: 's', ackSeq: 3, delayed: true } } },
  { v: 1, kind: 'push', payload: { channel: '__cindy/device-link/transport-ack', payload: { s: 'short', a: 3 } } },
  { v: 1, kind: 'push', payload: { channel: '__cindy/device-link/transport-ack', payload: { s: 'longenough12', a: 3, l: 'short' } } },
  { v: 1, kind: 'push', payload: { channel: '__cindy/device-link/transport-ack', payload: { s: 'longenough12', a: -1 } } },
  { v: 1, kind: 'push', payload: { channel: '__cindy/device-link/transport-ack', payload: { s: 'x'.repeat(129), a: 1 } } },
  { v: 1, kind: 'push', payload: { channel: '__cindy/device-link/transport-ack', payload: { s: 'longenough12', a: 5, l: 'linklinklink', d: 1 } } },
  { v: 1, kind: 'push', payload: { channel: 'other', payload: { streamId: 's', ackSeq: 1 } } },
];
const ackParseCases = ackParseInputs.map((e) => ({ input: norm(e), out: norm(parseTransportAck(e)) }));

// 重发节奏：RTT 估计序列 + 延迟
const rttCases: any[] = [];
for (const samples of [[], [100], [100, 300], [5000, 20000, 0], [50, 50, 50, 50, 50, 900]]) {
  const est: any = { srttMs: null, rttVarMs: 0 };
  for (const s of samples) updateRetransmitEstimate(est, s);
  const steps: any[] = [];
  for (const [intervals, attempts, bytes] of [[1, 0, 100], [1, 1, 100], [4, 2, 70000], [60, 3, 600000], [60, 6, 2_000_000], [2, 1, 20000]]) {
    steps.push({ intervals, attempts, bytes, delay: reliableRetryDelayMs(est, 2000, intervals, attempts, bytes), base: retransmitBaseMs(est, 2000) });
  }
  rttCases.push({ samples, est: norm(est), steps });
}
const deliveryCases: any[] = [];
{
  const est: any = { srttMs: null, rttVarMs: 0 };
  for (const [bytes, ms] of [[1000, 10], [100000, 500], [200000, 100], [200000, 0], [100000, 4000]]) {
    updateDeliveryEstimate(est, bytes as number, ms as number);
    deliveryCases.push({ bytes, ms, rate: est.deliveryBytesPerMs ?? null });
  }
  est.srttMs = 100; est.rttVarMs = 10;
  deliveryCases.push({ afterDelay: reliableRetryDelayMs(est, 2000, 60, 1, 1_000_000) });
}

// 模型速度投影：ultrafast 档对老控制端降级成布尔
const speedCases: any[] = [];
const speedChannels = ['local-db:sessions:list', 'local-db:bots:get', 'maker:list-active', 'maker:model-favorites:changed', 'maker:schedule:x', 'maker:session-sync', 'maker:session-sync:x', 'maker:event', 'other', 'local-db:messages:list'];
const speedPayloads: any[] = [
  null, 1, 'x', [], {}, { fast: 'ultrafast' }, { fast: true }, { fast: 'ultrafast', fastTier: 'x' }, { fastMode: 'ultrafast', other: 1 },
  { fastModeByModel: { a: 'ultrafast', b: true, c: 'fast' } }, { fastByModel: { a: 'ultrafast' }, fastByModelTiers: { z: 'ultrafast' } },
  { fastModeByModel: { a: 'ultrafast' }, fastModeByModelTiers: { b: 'ultrafast' } },
  { sessions: [{ fast: 'ultrafast' }, { fast: false }] }, { items: { value: { patch: { fastMode: 'ultrafast' } } } },
  { providerModelMemory: { k: { fast: 'ultrafast' }, j: { fastModeByModel: { m: 'ultrafast' } } } },
  { state: { runtime: { pi: { fast: 'ultrafast' }, claudeCode: { fastMode: 'ultrafast' } } } },
  { nested: { fast: 'ultrafast' } }, { messages: [{ fast: 'ultrafast' }] },
  { favorites: [{ fast: 'ultrafast' }], modelChain: { fast: 'ultrafast' }, modelOverride: { fastMode: 'ultrafast' } },
  [{ fast: 'ultrafast' }, 3, [{ fastMode: 'ultrafast' }]],
  JSON.parse('{"a":' .repeat(1) + '1}'),
];
// 深度 16 的边界
let deep: any = { fast: 'ultrafast' };
for (let i = 0; i < 18; i++) deep = { state: deep };
speedPayloads.push(deep);
for (const ch of speedChannels) for (const p of speedPayloads) speedCases.push({ channel: ch, payload: norm(p), out: norm(encodeModelSpeedPayload(ch, p)) });

export const data = {
  invoke: [...REMOTE_INVOKE_ALLOWLIST].sort(),
  push: [...PUSH_FORWARD_ALLOWLIST].sort(),
  // 落在 sessions topic 的通道（SESSION_LIST_CHANNELS ∪ ACCOUNT_CHANNELS，未导出，按行为探测；只有转发白名单内的通道会被查询）
  sessions: [...PUSH_FORWARD_ALLOWLIST].filter((c) => topicForPush(c, {}) === 'sessions').sort(),
};
export const golden = {
  hash: computeAllowlistHash(), invokeCount: REMOTE_INVOKE_ALLOWLIST.size, pushCount: PUSH_FORWARD_ALLOWLIST.size,
  topics, speedCases, encodeCases, parseCases, ackMakeCases, ackParseCases, rttCases, deliveryCases,
};
console.log(JSON.stringify({ data, golden }));
