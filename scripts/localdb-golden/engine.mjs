// 本地库引擎桥（差距清单 #3）：宿主（Rust）持有 SQLite 文件，sidecar 经 127.0.0.1 行协议访问。
// 这层把「宿主 RPC」包装成 node:sqlite DatabaseSync 的兼容形状（prepare/get/all/run/exec/close），
// 业务 SQL（local-db/*.mjs、goal.mjs、maker-storage.mjs）原样保留，只换底层驱动。
//
// 同步语义（关键设计）：主线程用 Atomics.wait 阻塞等结果期间事件循环停摆，postMessage 无法投递
// —— 所以请求/响应全走 SharedArrayBuffer 握手，双方都用 Atomics.wait/notify（共享内存操作，
// 不依赖事件循环）唤醒对方：
//   主线程：请求字节写 SAB 请求区 → cell0=seq → notify → Atomics.wait(cell2 === 上次完成值)
//   worker：Atomics.wait(cell0) 醒来 → 读请求 → socket 写出 → 事件循环收回复 → 写 SAB 响应区
//           → cell2=-seq → notify → 回到 Atomics.wait
// 裸 sidecar（e2e）没有宿主 → 回退 node:sqlite。
import { DatabaseSync } from 'node:sqlite';
import { Worker } from 'node:worker_threads';

/** 宿主引擎的同步 RPC（secret.* 等非 SQL 操作也走这条协议）。 */
export function hostRpcSync(payload, timeoutMs) {
  return getTransport().rpc(payload, timeoutMs);
}

export function hasRemoteEngine() {
  return !!process.env.CINDY_TAURI_DB_ADDR;
}

// ── SAB 布局 ────────────────────────────────────────────────────────
// header（i32 ×4）：[0]=请求 seq，[1]=请求字节长，[2]=响应 seq（worker 写 -seq 表示完成），[3]=响应字节长
// 数据区：请求区 REQ_OFF 起 MAX_REQ 字节；响应区 RESP_OFF 起 MAX_RESP 字节。
const MAX_REQ_BYTES = 8 * 1024 * 1024;
const MAX_RESP_BYTES = 16 * 1024 * 1024;
const REQ_OFF = 16;
const RESP_OFF = REQ_OFF + MAX_REQ_BYTES;
// "负无穷"替身：差值计算不可溢出 i32，且不与任何合法 seq（正数）或完成标记（负数）冲突。
const WAKE_NONE = -2147483000;

// worker 源（eval 执行）。注意：本文件里的 \\n 是给 worker 代码的字符串转义 ——
// worker 拿到的代码里是 '\n'（换行转义），不能被降级成真实换行（会让引号内断行 = 语法错误）。
const WORKER_SRC = `
const { parentPort, workerData } = require('node:worker_threads');
const net = require('node:net');
const i32 = new Int32Array(workerData.sab);
const u8 = new Uint8Array(workerData.sab);
const dec = new TextDecoder();
// 序号从 1 起，初值固定 0：worker 引导可能晚于主线程写入第一个请求（包越小越容易），
// 取启动时的值会把那个请求当成已处理而永远不发出。
let seen = 0;
let pendingWrite = null;
const sock = net.connect(Number(workerData.port), workerData.host);
let buf = '';
sock.setEncoding('utf8');
// 连接建立前到达的请求先存着，连上后补发（否则首个请求赶在连接之前就永远等不到回复）。
sock.on('connect', () => {
  if (pendingWrite !== null) {
    sock.write(pendingWrite);
    pendingWrite = null;
  }
});
sock.on('data', (d) => {
  buf += d;
  let i;
  while ((i = buf.indexOf('\\n')) >= 0) {
    const line = buf.slice(0, i);
    buf = buf.slice(i + 1);
    if (!line.trim()) continue;
    let reply;
    try { reply = JSON.parse(line); } catch { continue; }
    const seq = Number(reply.id);
    if (!Number.isFinite(seq)) continue;
    const bytes = Buffer.from(line, 'utf8');
    const cap = workerData.maxResp;
    const out = bytes.length > cap
      ? Buffer.from(JSON.stringify({ id: reply.id, ok: false, error: 'reply exceeds bridge capacity (' + bytes.length + '>' + cap + ')' }), 'utf8')
      : bytes;
    u8.set(out, workerData.respOff);
    Atomics.store(i32, 3, out.length);
    Atomics.store(i32, 2, -seq);
    Atomics.notify(i32, 2);
  }
});
sock.on('error', () => {
  // 连接失败/断开：标记当前请求失败（fail-closed，绝不静默吞掉写库失败）。
  Atomics.store(i32, 3, 0);
  Atomics.store(i32, 2, -seen);
  Atomics.notify(i32, 2);
});
parentPort.on('message', () => { /* 未用：通信全走 SAB */ });
// 请求派发：用 1ms interval 轮询 SAB —— 不能用 Atomics.wait 停车（会饿死 worker 自己的
// socket 事件循环，connect/data 回调永远不跑）。1ms 轮询的 CPU 成本可忽略。
setInterval(() => {
  const cur = Atomics.load(i32, 0);
  if (cur === seen) return;
  seen = cur;
  const len = Atomics.load(i32, 1);
  const req = dec.decode(new Uint8Array(workerData.sab, workerData.reqOff, len));
  if (sock.readyState === 'open') sock.write(req + '\\n');
  else pendingWrite = req + '\\n';
}, 1);
`;

let transport = null;

function getTransport() {
  if (transport) return transport;
  const addr = process.env.CINDY_TAURI_DB_ADDR;
  if (!addr) throw new Error('CINDY_TAURI_DB_ADDR not set');
  const [host, port] = addr.split(':');
  const sab = new SharedArrayBuffer(RESP_OFF + MAX_RESP_BYTES);
  const i32 = new Int32Array(sab);
  const worker = new Worker(WORKER_SRC, {
    eval: true,
    workerData: { sab, port: Number(port), host, reqOff: REQ_OFF, respOff: RESP_OFF, maxResp: MAX_RESP_BYTES },
  });
  let workerError = null;
  const fail = (err) => {
    workerError = workerError ?? err;
    // 唤醒可能在等待的调用（fail-closed，绝不静默吞掉写库失败）。
    Atomics.store(i32, 2, WAKE_NONE);
    Atomics.notify(i32, 2);
  };
  worker.on('error', (err) => fail(err));
  worker.on('exit', () => fail(new Error('db engine worker exited unexpectedly')));
  const decoder = new TextDecoder();
  const encoder = new TextEncoder();
  let seq = 0;
  const rpc = (payload, timeoutMs = 10_000) => {
    if (workerError) throw new Error(`[DB_ENGINE] worker failed: ${workerError.message}`);
    seq += 1;
    const reqStr = JSON.stringify({ id: seq, ...payload });
    const bytes = encoder.encode(reqStr);
    if (bytes.length > MAX_REQ_BYTES) {
      throw new Error(`[DB_ENGINE] request exceeds bridge capacity (${bytes.length} > ${MAX_REQ_BYTES})`);
    }
    new Uint8Array(sab, REQ_OFF).set(bytes);
    Atomics.store(i32, 1, bytes.length);
    Atomics.store(i32, 0, seq);
    Atomics.notify(i32, 0);
    const lastDone = Atomics.load(i32, 2);
    const res = Atomics.wait(i32, 2, lastDone, timeoutMs);
    if (res !== 'ok') {
      throw new Error(`[DB_ENGINE_TIMEOUT] local db engine did not reply within ${timeoutMs}ms (addr ${addr})`);
    }
    if (Atomics.load(i32, 2) !== -seq) {
      throw workerError ?? new Error('[DB_ENGINE] worker failed');
    }
    const len = Atomics.load(i32, 3);
    const reply = JSON.parse(decoder.decode(new Uint8Array(sab, RESP_OFF, len)));
    if (!reply.ok) throw new Error(`[DB_ENGINE] ${reply.error}`);
    return reply.result;
  };
  transport = { rpc };
  return transport;
}

let connCounter = 0;

// node:sqlite 的 get() 无行时返回 undefined —— 本地库代码按 `!row` 判空，这里保持同形。
class RemoteDatabaseSync {
  #conn;
  constructor(path) {
    this.#conn = `c${++connCounter}`;
    getTransport().rpc({ op: 'open', conn: this.#conn, path });
  }
  prepare(sql) {
    const conn = this.#conn;
    const bind = (params) => params.map((p) => (p === undefined ? null : p));
    return {
      get: (...params) => {
        const r = getTransport().rpc({ op: 'get', conn, sql, params: bind(params) });
        return r === null ? undefined : r;
      },
      all: (...params) => getTransport().rpc({ op: 'all', conn, sql, params: bind(params) }),
      run: (...params) => {
        const r = getTransport().rpc({ op: 'run', conn, sql, params: bind(params) });
        return { changes: r.changes, lastInsertRowid: r.lastInsertRowid };
      },
    };
  }
  exec(sql) {
    getTransport().rpc({ op: 'exec', conn: this.#conn, sql });
  }
  close() {
    try {
      getTransport().rpc({ op: 'close', conn: this.#conn });
    } catch { /* 宿主已退出 */ }
  }
}

/** 打开一个本地库连接：有宿主走 Rust 引擎，否则回退 node:sqlite。 */
export function createDatabase(path) {
  if (hasRemoteEngine()) {
    return new RemoteDatabaseSync(path);
  }
  return new DatabaseSync(path);
}

// 关键：worker 线程的 bootstrap 需要主线程事件循环推进（实测：创建后立刻阻塞主线程，
// worker 永不启动）。所以 transport 在本模块求值期就创建 —— 到 index.mjs 主体调用
// openLocalDb 之前，后续模块的异步加载 tick 已让 worker 完成引导。
if (hasRemoteEngine()) {
  try {
    getTransport();
  } catch (err) {
    // 启动失败不在这里爆：首次同步调用时会带着真实原因抛错（fail-closed）。
    console.error('[db-engine] warmup failed:', err?.message ?? err);
  }
}
