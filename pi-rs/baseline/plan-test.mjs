// /plan 对照：经 prompt 发 "/plan" 切换，再发普通 prompt，看事件、会话条目、请求里的工具与注入内容。
import { spawn } from 'node:child_process';
import { mkdtempSync, mkdirSync, writeFileSync, readFileSync, existsSync } from 'node:fs';
import os from 'node:os'; import path from 'node:path'; import readline from 'node:readline';
const tmp = mkdtempSync(path.join(os.tmpdir(), 'pipl-'));
const agentDir = path.join(tmp, 'agent'), sessions = path.join(tmp, 'sessions'), cwd = path.join(tmp, 'work');
for (const d of [agentDir, sessions, cwd]) mkdirSync(d, { recursive: true });
const LOG = path.join(tmp, 'req.log');
const mock = spawn(process.execPath, [path.join(import.meta.dirname, 'mock-anthropic.mjs')], { stdio: ['ignore', 'pipe', 'ignore'], env: { ...process.env, MOCK_LOG: LOG } });
const port = await new Promise(r => readline.createInterface({ input: mock.stdout }).once('line', l => r(JSON.parse(l).port)));
writeFileSync(path.join(agentDir, 'models.json'), JSON.stringify({ providers: { smoke: { baseUrl: `http://127.0.0.1:${port}`, api: 'anthropic-messages', apiKey: 'sk-none', models: [{ id: 'mock-model', name: 'Mock' }] } } }));
const ext = process.env.PLAN_EXT ? ['--extension', process.env.PLAN_EXT] : [];
const child = spawn(process.env.PI_BIN, ['--mode', 'rpc', '--no-approve', '--no-extensions', ...ext, '--no-skills', '--no-context-files', '--no-prompt-templates', '--session-dir', sessions, '--provider', 'smoke', '--model', 'mock-model'],
  { cwd, env: { ...process.env, PI_CODING_AGENT_DIR: agentDir, PI_OFFLINE: '1' }, stdio: ['pipe', 'pipe', 'inherit'] });
const out = []; let buf = '';
child.stdout.on('data', d => { buf += d; let i; while ((i = buf.indexOf('\n')) >= 0) { out.push(JSON.parse(buf.slice(0, i))); buf = buf.slice(i + 1); } });
const send = o => child.stdin.write(JSON.stringify(o) + '\n');
const sleep = ms => new Promise(r => setTimeout(r, ms));
const waitFor = async (pred, from, ms = 8000) => { const t = Date.now(); while (Date.now() - t < ms) { if (out.slice(from).some(pred)) return true; await sleep(30); } return false; };
const reqs = () => existsSync(LOG) ? readFileSync(LOG, 'utf8').split('\n').filter(Boolean).map(l => JSON.parse(l).body) : [];
const trace = [];
const step = async (name, msg) => {
  const k = out.length, before = reqs().length;
  send({ id: name, type: 'prompt', message: msg });
  await waitFor(r => r.id === name, k); await waitFor(r => r.type === 'agent_settled', k, 2500); await sleep(150);
  const evs = out.slice(k).filter(r => r.type !== 'message_update');
  const rq = reqs().slice(before);
  trace.push({ name, msg, response: evs.find(r => r.id === name), types: evs.filter(r => r.id === undefined).map(r => r.type + (r.type === 'extension_ui_request' ? ':' + r.method : '')),
    requests: rq.map(b => ({ tools: (b.tools ?? []).map(t => t.name), lastUser: JSON.stringify(b.messages.filter(m => m.role === 'user').pop()?.content).slice(0, 200), nUser: b.messages.filter(m => m.role === 'user').length })) });
};
await step('s1', '/plan');
await step('s2', 'please analyse');
await step('s3', '/plan');
await step('s4', 'now do it');
const k = out.length; send({ id: 'ent', type: 'get_entries' }); await waitFor(r => r.id === 'ent', k);
const entries = out.find(r => r.id === 'ent').data.entries.map(e => e.type === 'message' ? { type: 'message', role: e.message.role, toolsAdded: e.message.toolsAdded?.map(t => t.name), toolsRemoved: e.message.toolsRemoved?.map(t => t.name), sections: e.message.sections ? Object.keys(e.message.sections) : undefined, content: e.message.role === 'system' ? e.message.content : undefined, keys: Object.keys(e.message) } : { type: e.type, customType: e.customType, data: e.data, display: e.display, keys: Object.keys(e) });
child.stdin.end(); await sleep(300); child.kill(); mock.kill();
const sanitize = s => JSON.stringify({ trace, entries }, null, 1);
writeFileSync(process.env.OUT, sanitize());
console.log(JSON.stringify({ trace, entries }));
