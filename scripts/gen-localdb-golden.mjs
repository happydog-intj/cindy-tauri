// 用原 sidecar 的 JS 实现（冻结在 scripts/localdb-golden/original/，业务层已迁 Rust）跑 scenario.mjs，记录每步结果 / 推送事件 / 全表内容，
// 写成 src-tauri/src/local_db_golden.json 供 Rust 回放对比。固定时钟、固定 uuid / randomBytes。
// 用法：node scripts/gen-localdb-golden.mjs
import { build } from 'esbuild';
import { mkdtempSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import path from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';

process.env.TZ = 'UTC';
delete process.env.CINDY_TAURI_DB_ADDR;
const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const gd = path.join(root, 'scripts/localdb-golden');

const out = path.join(mkdtempSync(path.join(tmpdir(), 'ldb-gen-')), 'bundle.mjs');
await build({
  entryPoints: [path.join(gd, 'original/index.mjs')],
  bundle: true, platform: 'node', format: 'esm', outfile: out, logLevel: 'silent',
  external: ['node:sqlite', 'node:worker_threads', 'node:fs', 'node:path', 'node:net'],
  plugins: [{
    name: 'stubs',
    setup(b) {
      b.onResolve({ filter: /^node:crypto$/ }, () => ({ path: path.join(gd, 'crypto-shim.mjs') }));
      b.onResolve({ filter: /\/engine\.mjs$/ }, () => ({ path: path.join(gd, 'engine.mjs') }));
      b.onResolve({ filter: /\/ipc\.mjs$/ }, () => ({ path: path.join(gd, 'ipc-stub.mjs') }));
      b.onResolve({ filter: /^@cindy\/maker-shared\/message-window$/ }, () => ({ path: path.join(gd, 'hv-stub.mjs') }));
    },
  }],
});
const mod = await import(pathToFileURL(out).href);
const { handlers, events } = await import(pathToFileURL(path.join(gd, 'ipc-stub.mjs')).href);
const { buildOps, BASE } = await import(pathToFileURL(path.join(gd, 'scenario.mjs')).href);

const dataDir = mkdtempSync(path.join(tmpdir(), 'ldb-data-'));
let clock = BASE;
Date.now = () => clock;
const rt = (v) => (v === undefined ? null : JSON.parse(JSON.stringify(v)));
const scrub = (v) => JSON.parse(JSON.stringify(v ?? null).split(dataDir).join('<DATA>'));

let db;
let registered = false;
const open = () => {
  db = mod.openLocalDb({ dataDir, ownerId: 'local-v1' });
  if (!registered || true) mod.registerLocalDbIpc(db, { isRunning: () => false });
  registered = true;
};
const dump = () => {
  const tables = {};
  for (const [t, order] of [['sessions', 'id'], ['messages', 'rowid'], ['recent_workdirs', 'path'], ['project_aliases', 'project_key'], ['right_sidebar_tabs', 'id'], ['session_goals', 'session_id']]) {
    tables[t] = db.sql.prepare(`SELECT ${t === 'messages' ? 'rowid AS _rowid, ' : ''}* FROM ${t} ORDER BY ${order}`).all().map((r) => ({ ...r }));
  }
  return tables;
};

open();
const steps = [];
for (const op of buildOps()) {
  clock = op.now;
  const before = events.length;
  const step = { k: op.k, now: op.now };
  let result = null, error = null;
  try {
    if (op.k === 'invoke') {
      step.channel = op.channel; step.args = rt(op.args);
      result = await handlers.get(op.channel)(...JSON.parse(JSON.stringify(op.args ?? [])));
    } else if (op.k === 'upsert') {
      step.row = rt(op.row); db.upsertSession(rt(op.row));
    } else if (op.k === 'persistEvent') {
      step.sid = op.sid; step.event = rt(op.event); result = db.persistMakerEvent(op.sid, rt(op.event));
    } else if (op.k === 'persistUser') {
      step.sid = op.sid; step.clientId = op.clientId; step.content = rt(op.content);
      result = db.persistUserMessage(op.sid, { clientId: op.clientId, content: rt(op.content) });
    } else if (op.k === 'interactionReq') {
      step.sid = op.sid; step.req = rt(op.req); result = db.persistInteractionRequest(op.sid, rt(op.req));
    } else if (op.k === 'interactionRes') {
      step.sid = op.sid; step.requestId = op.requestId; step.decision = rt(op.decision);
      result = db.persistInteractionResolved(op.sid, op.requestId, rt(op.decision));
    } else if (op.k === 'reopen') {
      db.close(); open();
    } else if (op.k === 'dump') {
      result = dump();
    } else throw new Error(`unknown op ${op.k}`);
  } catch (e) { error = String(e?.message ?? e); }
  step.result = scrub(result ?? null);
  step.error = error === null ? null : scrub(error);
  step.events = scrub(events.slice(before));
  steps.push(step);
}
db.close();
const dst = path.join(root, 'src-tauri/src/local_db_golden.json');
writeFileSync(dst, JSON.stringify({ base: BASE, steps }, null, 1));
rmSync(dataDir, { recursive: true, force: true });
rmSync(path.dirname(out), { recursive: true, force: true });
const errs = steps.filter((s) => s.error).length;
console.log(`steps=${steps.length} errors=${errs} events=${steps.reduce((n, s) => n + s.events.length, 0)} -> ${dst}`);
