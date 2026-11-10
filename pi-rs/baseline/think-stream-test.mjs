// thinking 内容对照：流事件、assistant 消息里的 thinking 块、第二轮请求里历史的回传形式。
import { spawn } from 'node:child_process';
import { mkdtempSync, mkdirSync, writeFileSync, readFileSync, readdirSync } from 'node:fs';
import os from 'node:os'; import path from 'node:path'; import readline from 'node:readline';
const API = process.env.API || 'anthropic-messages';
const tmp = mkdtempSync(path.join(os.tmpdir(), 'pits-'));
const agentDir = path.join(tmp, 'agent'), sessions = path.join(tmp, 'sessions'), cwd = path.join(tmp, 'work');
for (const d of [agentDir, sessions, cwd]) mkdirSync(d, { recursive: true });
const LOG = path.join(tmp, 'req.log');
const mock = spawn(process.execPath, [path.join(import.meta.dirname, API === 'anthropic-messages' ? 'mock-anthropic.mjs' : 'mock-openai-think.mjs')], { stdio: ['ignore', 'pipe', 'ignore'], env: { ...process.env, MOCK_LOG: LOG, MOCK_THINK: '1' } });
const port = await new Promise(r => readline.createInterface({ input: mock.stdout }).once('line', l => r(JSON.parse(l).port)));
const model = { id: 'mock-model', name: 'Mock', reasoning: true, maxTokens: 8000, contextWindow: 200000, ...(process.env.COMPAT ? { compat: JSON.parse(process.env.COMPAT) } : {}) };
writeFileSync(path.join(agentDir, 'models.json'), JSON.stringify({ providers: { smoke: { baseUrl: `http://127.0.0.1:${port}`, api: API, apiKey: 'sk-none', models: [model] } } }));
const child = spawn(process.env.PI_BIN, ['--mode', 'rpc', '--no-extensions', '--no-skills', '--no-context-files', '--no-prompt-templates', '--session-dir', sessions, '--provider', 'smoke', '--model', 'mock-model'],
  { cwd, env: { ...process.env, PI_CODING_AGENT_DIR: agentDir, PI_OFFLINE: '1' }, stdio: ['pipe', 'pipe', 'inherit'] });
const out = []; let buf = '';
child.stdout.on('data', d => { buf += d; let i; while ((i = buf.indexOf('\n')) >= 0) { out.push(JSON.parse(buf.slice(0, i))); buf = buf.slice(i + 1); } });
const send = o => child.stdin.write(JSON.stringify(o) + '\n');
const sleep = ms => new Promise(r => setTimeout(r, ms));
const waitFor = async (pred, from, ms = 15000) => { const t = Date.now(); while (Date.now() - t < ms) { if (out.slice(from).some(pred)) return true; await sleep(30); } return false; };
for (const m of ['first', 'second']) { const k = out.length; send({ id: m, type: 'prompt', message: m }); await waitFor(r => r.type === 'agent_settled', k); }
const k = out.length; send({ id: 'gm', type: 'get_messages' }); await waitFor(r => r.id === 'gm', k);
child.stdin.end(); await sleep(300); child.kill(); mock.kill();
const reqs = readFileSync(LOG, 'utf8').split('\n').filter(Boolean).map(l => JSON.parse(l).body);
const evs = out.filter(r => r.type === 'message_update').map(r => r.assistantMessageEvent).filter(e => e.type.startsWith('thinking'));
const asst = out.find(r => r.id === 'gm').data.messages.filter(m => m.role === 'assistant').map(m => m.content);
const hist = reqs[1]?.messages?.filter(m => m.role === 'assistant');
console.log(JSON.stringify({ events: evs, assistant: asst, historyInSecondRequest: hist, thinkingParam: reqs[0]?.thinking, beta: undefined }, null, 1));
