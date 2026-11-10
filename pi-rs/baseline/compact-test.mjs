// 压缩对照：小 contextWindow + 小 keepRecentTokens，跑几轮后手动 compact，再自动触发阈值压缩。
// 用法：PI_BIN=... OUT=x.jsonl node compact-test.mjs（API 默认 anthropic-messages）
import { spawn } from 'node:child_process';
import { mkdtempSync, mkdirSync, writeFileSync, readdirSync, readFileSync } from 'node:fs';
import os from 'node:os'; import path from 'node:path'; import readline from 'node:readline';
const tmp = mkdtempSync(path.join(os.tmpdir(), 'picmp-'));
const agentDir = path.join(tmp, 'agent'), sessions = path.join(tmp, 'sessions'), cwd = path.join(tmp, 'work');
for (const d of [agentDir, sessions, cwd]) mkdirSync(d, { recursive: true });
const mock = spawn(process.execPath, [path.join(import.meta.dirname, 'mock-anthropic.mjs')], { stdio: ['ignore', 'pipe', 'ignore'], env: process.env });
const port = await new Promise(r => readline.createInterface({ input: mock.stdout }).once('line', l => r(JSON.parse(l).port)));
const CW = Number(process.env.CW || 6000);
writeFileSync(path.join(agentDir, 'models.json'), JSON.stringify({ providers: { smoke: { baseUrl: `http://127.0.0.1:${port}`, api: 'anthropic-messages', apiKey: 'sk-none', models: [{ id: 'mock-model', name: 'Mock', contextWindow: CW, maxTokens: 1024 }] } } }));
writeFileSync(path.join(agentDir, 'settings.json'), JSON.stringify({ retry: { maxRetries: 2, baseDelayMs: 100 }, compaction: { reserveTokens: Number(process.env.RESERVE || 1000), keepRecentTokens: Number(process.env.KEEP || 300) } }));
const child = spawn(process.env.PI_BIN, ['--mode', 'rpc', '--no-extensions', '--no-skills', '--no-context-files', '--no-prompt-templates', '--session-dir', sessions, '--provider', 'smoke', '--model', 'mock-model'],
  { cwd, env: { ...process.env, PI_CODING_AGENT_DIR: agentDir, PI_OFFLINE: '1' }, stdio: ['pipe', 'pipe', 'inherit'] });
const out = []; let buf = '';
child.stdout.on('data', d => { buf += d; let i; while ((i = buf.indexOf('\n')) >= 0) { out.push(buf.slice(0, i)); buf = buf.slice(i + 1); } });
const send = o => child.stdin.write(JSON.stringify(o) + '\n');
const sleep = ms => new Promise(r => setTimeout(r, ms));
const waitFor = async (pred, from = 0, ms = 30000) => { const t = Date.now(); while (Date.now() - t < ms) { if (out.slice(from).some(l => pred(JSON.parse(l)))) return; await sleep(30); } throw new Error('timeout ' + out.length); };
const big = 'lorem ipsum dolor sit amet '.repeat(40);
const turn = async (id, msg) => { const k = out.length; send({ id, type: 'prompt', message: msg }); await waitFor(r => r.type === 'agent_settled', k); };
const cmd = async (id, c) => { const k = out.length; send({ id, ...c }); await waitFor(r => r.id === id, k); };
await turn('p1', 'first ' + big);
await turn('p2', 'MOCK_WRITE_FILE second ' + big);
await turn('p3', 'third ' + big);
await cmd('c1', { type: 'compact' });
await cmd('c2', { type: 'compact' });
await cmd('st1', { type: 'get_state' });
await cmd('ss1', { type: 'get_session_stats' });
for (let i = 0; i < 4; i++) await turn('q' + i, 'more ' + i + ' ' + big);
await cmd('c3', { type: 'compact', customInstructions: 'focus on files' });
await cmd('ss2', { type: 'get_session_stats' });
await cmd('m', { type: 'get_messages' });
child.stdin.end(); await sleep(400); child.kill(); mock.kill();
writeFileSync(process.env.OUT, out.join('\n') + '\n');
const sf = readdirSync(sessions).flatMap(d => { const p = path.join(sessions, d); try { return readdirSync(p).map(f => path.join(p, f)); } catch { return [p]; } });
if (sf[0]) writeFileSync(process.env.OUT.replace(/\.jsonl$/, '.session.jsonl'), readFileSync(sf[0]));
console.log('records', out.length);
