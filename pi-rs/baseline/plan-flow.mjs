// 计划模式流程对照。STEPS: JSON 数组，元素 ["prompt", msg] | ["ui", choiceIndex|"cancel"]（响应下一个 select 请求）| ["cmd", {...}] | ["wait", ms]
import { spawn } from 'node:child_process';
import { mkdtempSync, mkdirSync, writeFileSync } from 'node:fs';
import os from 'node:os'; import path from 'node:path'; import readline from 'node:readline';
const tmp = mkdtempSync(path.join(os.tmpdir(), 'pipf-'));
const a = tmp + '/a', s = tmp + '/s', w = tmp + '/w'; for (const d of [a, s, w]) mkdirSync(d, { recursive: true });
const mock = spawn(process.execPath, [path.join(import.meta.dirname, 'mock-anthropic.mjs')], { stdio: ['ignore', 'pipe', 'ignore'], env: { ...process.env, MOCK_PLAN: '1' } });
const port = await new Promise(r => readline.createInterface({ input: mock.stdout }).once('line', l => r(JSON.parse(l).port)));
writeFileSync(a + '/models.json', JSON.stringify({ providers: { smoke: { baseUrl: `http://127.0.0.1:${port}`, api: 'anthropic-messages', apiKey: 'k', models: [{ id: 'm', name: 'M' }] } } }));
const ext = (process.env.EXTS || '').split(',').filter(Boolean).flatMap(e => ['--extension', e]);
const tools = process.env.TOOLS ? ['--tools', process.env.TOOLS] : [];
const c = spawn(process.env.PI_BIN, ['--mode', 'rpc', '--no-approve', '--no-extensions', ...ext, ...tools, '--no-skills', '--no-context-files', '--no-prompt-templates', '--session-dir', s, '--provider', 'smoke', '--model', 'm'], { cwd: w, env: { ...process.env, PI_CODING_AGENT_DIR: a, PI_OFFLINE: '1' }, stdio: ['pipe', 'pipe', 'inherit'] });
const out = []; let buf = ''; const pendingUi = [];
c.stdout.on('data', d => { buf += d; let i; while ((i = buf.indexOf('\n')) >= 0) { const l = buf.slice(0, i); buf = buf.slice(i + 1); out.push(l); const r = JSON.parse(l); if (r.type === 'extension_ui_request' && ['select', 'confirm', 'input', 'editor'].includes(r.method)) pendingUi.push(r); } });
const send = o => c.stdin.write(JSON.stringify(o) + '\n');
const sleep = ms => new Promise(r => setTimeout(r, ms));
const waitFor = async (pred, from, ms = 6000) => { const t = Date.now(); while (Date.now() - t < ms) { if (out.slice(from).some(l => pred(JSON.parse(l)))) return true; await sleep(25); } return false; };
let n = 0;
for (const st of JSON.parse(process.env.STEPS)) {
  const k = out.length, id = 'st' + n++;
  if (st[0] === 'prompt') { send({ id, type: 'prompt', message: st[1] }); await waitFor(r => r.id === id, k); const resp = out.slice(k).map(l => JSON.parse(l)).find(r => r.id === id); if (resp?.data?.disposition === 'started') await waitFor(r => r.type === 'agent_settled' || r.type === 'extension_ui_request' && r.method === 'select', k, 5000); await sleep(120); }
  else if (st[0] === 'ui') { const t = Date.now(); while (!pendingUi.length && Date.now() - t < 4000) await sleep(25); const req = pendingUi.shift(); if (req) { const reply = st[1] === 'cancel' ? { type: 'extension_ui_response', id: req.id, cancelled: true } : st[1] === 'text' ? { type: 'extension_ui_response', id: req.id, value: st[2] } : { type: 'extension_ui_response', id: req.id, value: req.options[st[1]] }; send(reply); await waitFor(r => r.type === 'agent_settled', k, 4000); await sleep(150); } }
  else if (st[0] === 'cmd') { send({ id, ...st[1] }); await waitFor(r => r.id === id, k); }
  else if (st[0] === 'wait') await sleep(st[1]);
}
send({ id: 'ent', type: 'get_entries' }); send({ id: 'msgs', type: 'get_messages' }); send({ id: 'cmds', type: 'get_commands' }); send({ id: 'stt', type: 'get_state' }); await sleep(300);
c.kill(); mock.kill();
writeFileSync(process.env.OUT, out.join('\n') + '\n');
console.log('records', out.length);
