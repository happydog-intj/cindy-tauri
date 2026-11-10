// 压力测试：用 mock 快速跑很多轮，带随机的 abort / 压缩 / 重试 / 工具调用，检测卡死（agent_settled 没出现）和进程崩溃。
// 用法：PI_BIN=... ROUNDS=300 SEED=1 node stress-test.mjs
import { spawn } from 'node:child_process';
import { mkdtempSync, mkdirSync, writeFileSync } from 'node:fs';
import os from 'node:os'; import path from 'node:path'; import readline from 'node:readline';
const ROUNDS = Number(process.env.ROUNDS || 200); let seed = Number(process.env.SEED || 1);
const rnd = () => (seed = (seed * 1664525 + 1013904223) >>> 0) / 2 ** 32;
const tmp = mkdtempSync(path.join(os.tmpdir(), 'pistress-')); const a = tmp + '/a', s = tmp + '/s', w = tmp + '/w';
for (const d of [a, s, w]) mkdirSync(d, { recursive: true });
writeFileSync(w + '/f.txt', 'hello world\n'.repeat(200));
const mock = spawn(process.execPath, [path.join(import.meta.dirname, 'mock-anthropic.mjs')], { stdio: ['ignore', 'pipe', 'ignore'], env: { ...process.env, MOCK_REAL_USAGE: '1', MOCK_FAIL_FIRST: process.env.FAILS || '0' } });
const port = await new Promise(r => readline.createInterface({ input: mock.stdout }).once('line', l => r(JSON.parse(l).port)));
writeFileSync(a + '/models.json', JSON.stringify({ providers: { smoke: { baseUrl: `http://127.0.0.1:${port}`, api: 'anthropic-messages', apiKey: 'k', models: [{ id: 'm', name: 'M', contextWindow: 6000, maxTokens: 1024 }] } } }));
writeFileSync(a + '/settings.json', JSON.stringify({ compaction: { reserveTokens: 1500, keepRecentTokens: 1000 }, retry: { maxRetries: 2, baseDelayMs: 50 } }));
const child = spawn(process.env.PI_BIN, ['--mode', 'rpc', '--no-extensions', '--no-skills', '--no-context-files', '--no-prompt-templates', '--session-dir', s, '--provider', 'smoke', '--model', 'm'],
  { cwd: w, env: { ...process.env, PI_CODING_AGENT_DIR: a, PI_OFFLINE: '1' }, stdio: ['pipe', 'pipe', 'pipe'] });
let exited = null; child.on('exit', (c, sig) => { exited = { c, sig }; });
let stderr = ''; child.stderr.on('data', d => { stderr += d; });
const out = []; let buf = ''; child.stdout.on('data', d => { buf += d; let i; while ((i = buf.indexOf('\n')) >= 0) { try { out.push(JSON.parse(buf.slice(0, i))); } catch { out.push({ type: 'BADJSON', raw: buf.slice(0, 200) }); } buf = buf.slice(i + 1); } });
const send = o => child.stdin.write(JSON.stringify(o) + '\n');
const sleep = ms => new Promise(r => setTimeout(r, ms));
const waitFor = async (pred, from, ms) => { const t = Date.now(); while (Date.now() - t < ms) { if (exited) return false; if (out.slice(from).some(pred)) return true; await sleep(10); } return false; };
const stats = { rounds: 0, aborted: 0, compactCmd: 0, hung: 0, badjson: 0, errors: 0 }; let hang = null;
const t0 = Date.now();
for (let i = 0; i < ROUNDS && !exited && !hang; i++) {
  const k = out.length; const r = rnd();
  const msg = r < 0.3 ? `MOCK_TOOLS_NEXT x read f.txt` : r < 0.5 ? `Use the read tool to read f.txt (round ${i})` : `round ${i} ` + 'blah '.repeat(Math.floor(rnd() * 200));
  send({ id: 'p' + i, type: 'prompt', message: msg });
  if (rnd() < 0.2) { await sleep(Math.floor(rnd() * 60)); send({ id: 'a' + i, type: 'abort' }); stats.aborted++; }
  if (rnd() < 0.1) { send({ id: 'c' + i, type: 'compact' }); stats.compactCmd++; }
  if (rnd() < 0.1) send({ id: 'q' + i, type: 'prompt', message: 'queued', streamingBehavior: 'followUp' });
  const ok = await waitFor(x => x.type === 'agent_settled', k, 20000);
  if (!ok && !exited) { hang = { round: i, tail: out.slice(-12).map(x => JSON.stringify(x).slice(0, 200)) }; break; }
  // 等残留的 compact 响应也结束，避免把上一轮命令算进下一轮
  await sleep(5);
  stats.rounds++;
}
stats.badjson = out.filter(x => x.type === 'BADJSON').length; stats.errors = out.filter(x => x.type === 'message_end' && x.message.stopReason === 'error').length;
stats.settled = out.filter(x => x.type === 'agent_settled').length; stats.compactions = out.filter(x => x.type === 'compaction_end').length;
child.kill(); mock.kill();
console.log(JSON.stringify({ seed: process.env.SEED || 1, ms: Date.now() - t0, ...stats, exited, hang, stderr: stderr.slice(0, 500) }));
process.exit(hang || exited ? 1 : 0);
