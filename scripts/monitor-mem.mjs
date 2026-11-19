// 内存采样：定期记录 cindy-tauri / pi-rs 进程的 RSS，验证长跑内存趋势。
// 用法：node scripts/monitor-mem.mjs [--interval 60] [--duration 0] [--out mem-log.csv]
//   --interval  采样间隔秒数，默认 60
//   --duration  总时长秒数，0 = 一直跑到手动 Ctrl+C，默认 0
//   --out       输出 CSV 路径，默认写到 /tmp/cindy-mem-<启动时间>.csv
// CSV 列：ts,uptime_min,host_rss_kb,node_rss_kb,pirs_rss_kb,total_rss_kb,node_cmd_has_flags
// 结束时打印每列随时间的对比摘要（首采样 vs 末采样）。
import { execFileSync } from 'node:child_process';
import { appendFileSync, writeFileSync } from 'node:fs';
import path from 'node:path';

const args = process.argv.slice(2);
const arg = (name, def) => {
  const i = args.indexOf(`--${name}`);
  return i >= 0 && args[i + 1] ? args[i + 1] : def;
};
const interval = Math.max(5, Number(arg('interval', 60)) * 1000);
const duration = Number(arg('duration', 0)) * 1000;
const startedAt = new Date().toISOString().replace(/[:.]/g, '-').slice(0, 19);
const outFile = path.resolve(arg('out', `/tmp/cindy-mem-${startedAt}.csv`));

// 只采本仓库 bundle 跑出来的进程，避免误伤别的副本。
const BUNDLE = 'src-tauri/target/release/bundle';

function sample() {
  let out;
  try {
    out = execFileSync('ps', ['axo', 'pid,rss,command'], { encoding: 'utf8' });
  } catch {
    return null;
  }
  const rows = { host: null, node: null, pirs: null, flags: false };
  for (const line of out.split('\n')) {
    if (!line.includes(BUNDLE)) continue;
    const m = line.trim().match(/^(\d+)\s+(\d+)\s+(.*)$/);
    if (!m) continue;
    const [, pid, rss, cmd] = m;
    if (cmd.includes('MacOS/cindy-tauri')) rows.host = Number(rss);
    else if (cmd.includes('MacOS/cindy-node')) {
      rows.node = Number(rss);
      // 确认堆限制参数在位。
      rows.flags = cmd.includes('--max-old-space-size=256');
    } else if (cmd.includes('/pi-rs/pi ')) rows.pirs = Number(rss);
  }
  if (rows.host === null && rows.node === null) return null; // app 没在跑
  return rows;
}

const header = 'ts,uptime_min,host_rss_kb,node_rss_kb,pirs_rss_kb,total_rss_kb,node_cmd_has_flags';
writeFileSync(outFile, `${header}\n`);
console.log(`sampling every ${interval / 1000}s${duration ? `, total ${duration / 1000}s` : ''} -> ${outFile}`);

const samples = [];
const t0 = Date.now();
const timer = setInterval(() => {
  const s = sample();
  const now = new Date().toISOString();
  if (!s) {
    console.log(`${now}  (cindy-tauri 未运行，跳过)`);
    if (duration && Date.now() - t0 >= duration) finish();
    return;
  }
  const upMin = ((Date.now() - t0) / 60000).toFixed(1);
  const total = (s.host ?? 0) + (s.node ?? 0) + (s.pirs ?? 0);
  appendFileSync(outFile, `${now},${upMin},${s.host ?? ''},${s.node ?? ''},${s.pirs ?? ''},${total},${s.flags ? 1 : 0}\n`);
  samples.push({ upMin, ...s, total });
  console.log(`${now}  +${upMin}min  host=${fmt(s.host)}  node=${fmt(s.node)}${s.flags ? '[capped]' : '[NO-FLAGS!]'}  pi-rs=${fmt(s.pirs)}  total=${fmt(total)}`);
  if (duration && Date.now() - t0 >= duration) finish();
}, interval);

function fmt(kb) {
  return kb === null || kb === undefined ? '-' : `${(kb / 1024).toFixed(1)}MB`;
}

function finish() {
  clearInterval(timer);
  const first = samples[0];
  const last = samples.at(-1);
  if (first && last && samples.length > 1) {
    console.log('\n── 摘要（首采样 → 末采样，共 ' + samples.length + ' 次）──');
    for (const k of ['host', 'node', 'pirs', 'total']) {
      const a = first[k] ?? 0;
      const b = last[k] ?? 0;
      console.log(`${k.padEnd(5)}: ${fmt(a)} -> ${fmt(b)}  (${b - a >= 0 ? '+' : ''}${((b - a) / 1024).toFixed(1)}MB)`);
    }
    const peakNode = Math.max(...samples.map((s) => s.node ?? 0));
    console.log(`node 峰值: ${fmt(peakNode)}${peakNode > 256 * 1024 ? '  ⚠️ 超过 256MB 上限？！' : ''}`);
  }
  process.exit(0);
}

process.on('SIGINT', finish);
