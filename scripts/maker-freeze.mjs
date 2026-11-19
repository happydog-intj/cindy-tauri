// maker 事件形状冻结（差距清单 #1 的双产出比对）：
//   同一帧流（baseline/record.mjs 场景：hi → MOCK_WRITE_FILE 工具调用）
//   → TS translator（cindy 仓库 maker-core，打包重放）与 pi-rs --replay-maker 各自产出 AgentEvent
//   → 规范化随机/时间后逐行比对。differing=0 才通过。
// 用法：node scripts/maker-freeze.mjs [--update]
//   --update：把 TS 产出覆盖到 scripts/fixtures/maker-freeze/golden-translator.jsonl（人工 review 后提交）。
//   Rust 侧永远与 TS 侧现算比对（两侧行数/内容都不同即失败）。
import { execFileSync } from 'node:child_process';
import { openSync } from 'node:fs';
import { build } from 'esbuild';
import { existsSync, mkdtempSync, readdirSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import readline from 'node:readline';
import { fileURLToPath } from 'node:url';

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const cindyDir = path.resolve(process.env.CINDY_DIR ?? path.join(root, '..', 'cindy'));
const piRs = path.join(root, 'pi-rs', 'target', 'release', 'pi');
const UPDATE = process.argv.includes('--update');

const UUID_RE = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i;
// 产出里随机/时间字段：uuid、毫秒时长、时间戳、内部 id。形状（字段名与结构）必须一致，值可以不同。
const isTsKey = (k) => ['startedAt', 'durationMs', 'recordedAt', 'ts', 'timestamp', 'createdAt', 'updatedAt', 'at', 'time', 'elapsedMs', 'receivedAt'].includes(k) || k.endsWith('DurationMs');

function normalize(v, depth = 0) {
  if (depth > 12) return '<deep>';
  // blockId 形如 pi-think-<随机 uuid>-<序号>：前缀随机，序号是形状的一部分（并行 block 必须各自递增）。
  if (typeof v === 'string') return UUID_RE.test(v) ? '<uuid>' : v.replace(/^(pi-think-)[0-9a-f-]{36}(-\d+)$/i, '$1<uuid>$2');
  if (typeof v === 'number' || v === null || typeof v === 'boolean') return v;
  if (Array.isArray(v)) return v.map((x) => normalize(x, depth + 1));
  // 按键排序：TS / Rust 的 JSON 字段序不同，形状比对只看字段集合。
  const out = {};
  for (const k of Object.keys(v).sort()) {
    const val = v[k];
    if (isTsKey(k)) { out[k] = '<ts>'; continue; }
    if (k === 'rate' || k === 'average') { out[k] = '<rate>'; continue; }
    if (UUID_RE.test(String(val))) { out[k] = '<uuid>'; continue; }
    out[k] = normalize(val, depth + 1);
  }
  return out;
}

// 流式节奏（delta 拆分 / 心跳）不冻结：每组 (type, isFinal/stage, blockId) 只保留首个代表。
function collapse(events) {
  const seen = new Set();
  const out = [];
  for (const e of events) {
    if ((e.type === 'text' && e.data?.isFinal === false) || (e.type === 'thinking' && !e.data?.finished)) {
      const key = JSON.stringify([e.type, e.data?.isFinal === false ? 'delta' : e.data?.stage ?? '', e.data?.blockId ?? '']);
      if (seen.has(key)) continue;
      seen.add(key);
      out.push({ ...e, collapsed: true });
      continue;
    }
    out.push(e);
  }
  return out;
}

const linesOf = (file) => readFileSync(file, 'utf8').trim().split('\n').filter(Boolean).map((l) => JSON.stringify(normalize(JSON.parse(l))));

// ── 1. 录帧（baseline/record.mjs：hi → MOCK_WRITE_FILE，mock 模型）──
const tmp = mkdtempSync(path.join(os.tmpdir(), 'maker-freeze-'));
const framesFile = path.join(tmp, 'frames.jsonl');
execFileSync('node', [path.join(root, 'pi-rs', 'baseline', 'record.mjs')], {
  env: { ...process.env, PI_BIN: piRs, OUT: framesFile },
  stdio: ['ignore', 'pipe', 'inherit'],
});
const frames = readFileSync(framesFile, 'utf8').trim().split('\n').filter((l) => JSON.parse(l).type !== 'response');
const framesFileFiltered = path.join(tmp, 'frames-events.jsonl');
writeFileSync(framesFileFiltered, frames.join('\n') + '\n');

// ── 2. TS 侧：打包 translator 重放器并执行 ──
const bundle = path.join(tmp, 'translator-replay.cjs');
await build({
  entryPoints: [path.join(root, 'scripts', 'freeze', 'translator-replay.ts')],
  bundle: true,
  platform: 'node',
  format: 'cjs',
  target: 'node22',
  outfile: bundle,
  nodePaths: [path.join(cindyDir, 'apps', 'cindy-headless', 'node_modules'), path.join(cindyDir, 'apps', 'desktop', 'node_modules'), path.join(cindyDir, 'node_modules')],
  // maker-core 的 exports 不放行深层子路径，直接指到源文件。
  alias: { '@cindy/maker-core/agents/pi/translator': path.join(cindyDir, 'packages/maker-core/src/agents/pi/translator.ts') },
  loader: { '.md': 'text' },
  logLevel: 'silent',
  define: { 'import.meta.url': JSON.stringify(pathToFileUrl(path.join(root, 'scripts', 'freeze'))), 'import.meta.env': '{"VITE_CINDY_AUTH_REGION":"global"}' },
});
function pathToFileUrl(p) { return 'file://' + p.split('\\').join('/'); }
const tsOut = path.join(tmp, 'ts-events.jsonl');
execFileSync(process.execPath, [bundle, framesFileFiltered], { stdio: ['ignore', openSync(tsOut, 'w'), 'inherit'], timeout: 60_000 });

// ── 3. Rust 侧：pi-rs --replay-maker ──
const rsOut = path.join(tmp, 'rs-events.jsonl');
if (!existsSync(piRs)) {
  console.error(`pi-rs binary missing: ${piRs}（先 npm run pirs:build 或 cargo build --release）`);
  process.exit(2);
}
execFileSync(piRs, ["--replay-maker", framesFileFiltered], { stdio: ["ignore", openSync(rsOut, "w"), "inherit"], timeout: 60_000 });

// ── 3b. 手写场景（错误 / 重试路径）：同一帧流分别喂 TS translator 与 pi-rs，比对产出 ──
const scenDir = path.join(root, 'scripts', 'fixtures', 'maker-freeze', 'scenarios');
const scenarios = existsSync(scenDir) ? readdirSync(scenDir).filter((f) => f.endsWith('.jsonl')).sort() : [];
const scenarioOut = [];
for (const f of scenarios) {
  const name = f.replace(/\.jsonl$/, '');
  const tsFile = path.join(tmp, `scn-${name}-ts.jsonl`);
  const rsFile = path.join(tmp, `scn-${name}-rs.jsonl`);
  execFileSync(process.execPath, [bundle, path.join(scenDir, f)], { stdio: ['ignore', openSync(tsFile, 'w'), 'inherit'], timeout: 60_000 });
  execFileSync(piRs, ['--replay-maker', path.join(scenDir, f)], { stdio: ['ignore', openSync(rsFile, 'w'), 'inherit'], timeout: 60_000 });
  const load = (file) => collapse(readFileSync(file, 'utf8').trim().split('\n').filter(Boolean).map(JSON.parse)).map((e) => JSON.stringify(normalize(e)));
  scenarioOut.push({ name, ts: load(tsFile), rs: load(rsFile) });
}

// ── 4. 规范化 + 比对 ──
const ts = collapse(readFileSync(tsOut, 'utf8').trim().split('\n').filter(Boolean).map(JSON.parse)).map((e) => JSON.stringify(normalize(e)));
const rs = collapse(readFileSync(rsOut, 'utf8').trim().split('\n').filter(Boolean).map(JSON.parse)).map((e) => JSON.stringify(normalize(e)));

if (UPDATE) {
  const golden = path.join(root, 'scripts', 'fixtures', 'maker-freeze', 'golden-translator.jsonl');
  writeFileSync(golden, ts.join('\n') + '\n');
  console.log(`golden updated: ${ts.length} TS events -> ${golden}`);
}
const golden = existsSync(path.join(root, 'scripts', 'fixtures', 'maker-freeze', 'golden-translator.jsonl'))
  ? readFileSync(path.join(root, 'scripts', 'fixtures', 'maker-freeze', 'golden-translator.jsonl'), 'utf8').trim().split('\n')
  : null;
if (golden && golden.join('\n') !== ts.join('\n')) {
  console.error(`TS translator output drifted from golden (${golden.length} -> ${ts.length}); 先人工确认后用 --update 刷新 golden`);
  for (let i = 0; i < Math.max(golden.length, ts.length); i++) {
    if (golden[i] !== ts[i]) console.error(`  #${i + 1}\n    golden: ${(golden[i] ?? '(missing)').slice(0, 200)}\n    actual: ${(ts[i] ?? '(missing)').slice(0, 200)}`);
  }
  process.exit(1);
}

let fail = 0;
const n = Math.max(ts.length, rs.length);
for (let i = 0; i < n; i++) {
  if (ts[i] !== rs[i]) {
    if (!fail) console.error('TS vs Rust drift:');
    fail++;
    if (fail <= 10) console.error(`  #${i + 1}\n    ts: ${(ts[i] ?? '(missing)').slice(0, 240)}\n    rs: ${(rs[i] ?? '(missing)').slice(0, 240)}`);
  }
}
console.log(`maker events: ts=${ts.length} rs=${rs.length} differing=${fail}`);
let scenFail = 0;
for (const sc of scenarioOut) {
  let d = 0;
  const m = Math.max(sc.ts.length, sc.rs.length);
  for (let i = 0; i < m; i++) {
    if (sc.ts[i] !== sc.rs[i]) {
      if (!d) console.error(`scenario ${sc.name} TS vs Rust drift:`);
      d++;
      if (d <= 6) console.error(`  #${i + 1}\n    ts: ${(sc.ts[i] ?? '(missing)').slice(0, 300)}\n    rs: ${(sc.rs[i] ?? '(missing)').slice(0, 300)}`);
    }
  }
  console.log(`scenario ${sc.name}: ts=${sc.ts.length} rs=${sc.rs.length} differing=${d}`);
  scenFail += d;
}
if (scenFail && process.env.MAKER_FREEZE_SCENARIOS_ADVISORY !== '1') process.exitCode = 1;
if (fail) {
  // 定位每处差异的深层字段路径（数组错位时路径仅示意，重点看对象内差异）。
  const firstDiff = (a, b, p) => {
    if (a === b) return null;
    const ta = typeof a, tb = typeof b;
    if (ta !== tb || a === null || b === null || Array.isArray(a) !== Array.isArray(b)) return `${p}: ${JSON.stringify(a)?.slice(0, 80)} != ${JSON.stringify(b)?.slice(0, 80)}`;
    if (Array.isArray(a)) { for (let i = 0; i < Math.max(a.length, b.length); i++) { const r = firstDiff(a[i], b[i], `${p}[${i}]`); if (r) return r; } return null; }
    if (ta === 'object') { const keys = [...new Set([...Object.keys(a ?? {}), ...Object.keys(b ?? {})])].sort(); for (const k of keys) { const r = firstDiff(a?.[k], b?.[k], `${p}.${k}`); if (r) return r; } return null; }
    return `${p}: ${JSON.stringify(a)?.slice(0, 80)} != ${JSON.stringify(b)?.slice(0, 80)}`;
  };
  for (let i = 0; i < n; i++) {
    if (ts[i] !== rs[i]) console.error(`  first-diff #${i + 1}: ${firstDiff(JSON.parse(ts[i] ?? '{}'), JSON.parse(rs[i] ?? '{}'), '$')}`);
  }
  process.exit(1);
}
if (process.env.MAKER_FREEZE_KEEP) console.log(`kept: ${tmp}`); else rmSync(tmp, { recursive: true, force: true });
