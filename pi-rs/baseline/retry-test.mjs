// 重试对照：mock 前 N 个请求返回 503（或挂起），看 auto_retry_* 事件、重试后的结果、abort 重试。
import { spawn } from 'node:child_process';
import { mkdtempSync, mkdirSync, writeFileSync } from 'node:fs';
import os from 'node:os'; import path from 'node:path'; import readline from 'node:readline';
const tmp = mkdtempSync(path.join(os.tmpdir(), 'pirt-'));
const agentDir = path.join(tmp, 'agent'), sessions = path.join(tmp, 'sessions'), cwd = path.join(tmp, 'work');
for (const d of [agentDir, sessions, cwd]) mkdirSync(d, { recursive: true });
const API = process.env.API || 'anthropic-messages';
const mock = spawn(process.execPath, [path.join(import.meta.dirname, API === 'anthropic-messages' ? 'mock-anthropic.mjs' : 'mock-openai-think.mjs')], { stdio: ['ignore', 'pipe', 'ignore'], env: process.env });
const port = await new Promise(r => readline.createInterface({ input: mock.stdout }).once('line', l => r(JSON.parse(l).port)));
writeFileSync(path.join(agentDir, 'models.json'), JSON.stringify({ providers: { smoke: { baseUrl: `http://127.0.0.1:${port}`, api: API, apiKey: 'sk-none', models: [{ id: 'mock-model', name: 'Mock' }] } } }));
writeFileSync(path.join(agentDir, 'settings.json'), JSON.stringify({ ...(process.env.IDLE ? { httpIdleTimeoutMs: Number(process.env.IDLE) } : {}), retry: { maxRetries: 2, baseDelayMs: Number(process.env.BASE || 150), provider: { maxRetries: 0 } } }));
const child = spawn(process.env.PI_BIN, ['--mode', 'rpc', '--no-extensions', '--no-skills', '--no-context-files', '--no-prompt-templates', '--session-dir', sessions, '--provider', 'smoke', '--model', 'mock-model'],
  { cwd, env: { ...process.env, PI_CODING_AGENT_DIR: agentDir, PI_OFFLINE: '1' }, stdio: ['pipe', 'pipe', 'inherit'] });
const out = []; let buf = '';
child.stdout.on('data', d => { buf += d; let i; while ((i = buf.indexOf('\n')) >= 0) { out.push(buf.slice(0, i)); buf = buf.slice(i + 1); } });
const send = o => child.stdin.write(JSON.stringify(o) + '\n');
const sleep = ms => new Promise(r => setTimeout(r, ms));
const waitFor = async (pred, from = 0, ms = 30000) => { const t = Date.now(); while (Date.now() - t < ms) { if (out.slice(from).some(l => pred(JSON.parse(l)))) return true; await sleep(30); } return false; };
const k = out.length;
send({ id: 'p1', type: 'prompt', message: 'hi' });
if (process.env.ABORT_AFTER) { await waitFor(r => r.type === 'auto_retry_start', k, 10000); send({ id: 'ab', type: 'abort' }); }
await waitFor(r => r.type === 'agent_settled', k, Number(process.env.WAIT || 20000));
await sleep(200);
send({ id: 'ss', type: 'get_session_stats' }); await waitFor(r => r.id === 'ss', k, 3000);
send({ id: 'gm', type: 'get_messages' }); await waitFor(r => r.id === 'gm', k, 3000);
child.stdin.end(); await sleep(300); child.kill(); mock.kill();
writeFileSync(process.env.OUT, out.join('\n') + '\n');
console.log(out.map(l => JSON.parse(l)).filter(r => r.type !== 'message_update').map(r => r.type + (r.type === 'auto_retry_start' ? `#${r.attempt}/${r.maxAttempts}@${r.delayMs}` : '') + (r.type === 'agent_end' ? `(willRetry=${r.willRetry})` : '') + (r.type === 'auto_retry_end' ? `(ok=${r.success},a=${r.attempt},${r.finalError})` : '') + (r.type === 'message_end' ? `[${r.message.role}${r.message.stopReason ? ':' + r.message.stopReason : ''}${r.message.errorMessage ? ':' + r.message.errorMessage.slice(0, 60) : ''}]` : '')).join(' '));
