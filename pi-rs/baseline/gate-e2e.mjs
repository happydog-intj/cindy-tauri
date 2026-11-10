// 权限门端到端：Ask 档下读普通文件直接放行，读 .env / 指向凭证的 symlink 必须发 cindy:permission 并带证据；
// 拒绝时工具结果是原版的拒绝文案；bypass 档不问；review-only 拒绝凭证读。
import { spawn } from 'node:child_process';
import { mkdtempSync, writeFileSync, mkdirSync, symlinkSync, realpathSync } from 'node:fs';
import { tmpdir } from 'node:os';
import path from 'node:path';
const PI = process.env.PI_BIN;
const mock = spawn(process.execPath, [path.join(import.meta.dirname, 'mock-anthropic.mjs')], { stdio: ['ignore', 'pipe', 'inherit'] });
const port = await new Promise(r => mock.stdout.once('data', d => r(JSON.parse(d).port)));
const root = realpathSync(mkdtempSync(path.join(tmpdir(), 'gate-')));
const agentDir = path.join(root, 'agent'), cwd = path.join(root, 'work');
mkdirSync(agentDir); mkdirSync(cwd); mkdirSync(path.join(root, '.ssh'));
writeFileSync(path.join(agentDir, 'models.json'), JSON.stringify({ providers: { smoke: { baseUrl: `http://127.0.0.1:${port}`, api: 'anthropic-messages', apiKey: 'k', models: [{ id: 'm' }] } } }));
writeFileSync(path.join(cwd, 'plain.txt'), 'hello'); writeFileSync(path.join(cwd, '.env'), 'SECRET=1');
writeFileSync(path.join(root, '.ssh', 'id_rsa'), 'KEY'); symlinkSync(path.join(root, '.ssh', 'id_rsa'), path.join(cwd, 'notes.txt'));
const permFile = path.join(root, 'perm.json');
const setPerm = (p) => writeFileSync(permFile, JSON.stringify(p));
const child = spawn(PI, ['--mode', 'rpc', '--no-approve', '--provider', 'smoke', '--model', 'm', '--extension', process.env.BRIDGE || '/x/internal-extensions/cindy-bridge.ts'],
  { cwd, env: { ...process.env, PI_CODING_AGENT_DIR: agentDir, PI_OFFLINE: '1', CINDY_PI_PERMISSION_FILE: permFile }, stdio: ['pipe', 'pipe', 'inherit'] });
let buf = '', events = [], answer = 'allow';
child.stdout.on('data', d => { buf += d; let i; while ((i = buf.indexOf('\n')) >= 0) { const l = buf.slice(0, i); buf = buf.slice(i + 1); const j = JSON.parse(l); events.push(j);
  if (j.type === 'extension_ui_request' && j.title === 'cindy:permission') child.stdin.write(JSON.stringify({ type: 'extension_ui_response', id: j.id, value: answer }) + '\n'); } });
const sleep = ms => new Promise(r => setTimeout(r, ms));
async function turn(text) {
  const k = events.length;
  child.stdin.write(JSON.stringify({ type: 'prompt', message: text }) + '\n');
  for (let t = 0; t < 200 && !events.slice(k).some(e => e.type === 'agent_settled'); t++) await sleep(50);
  const ev = events.slice(k);
  const ask = ev.find(e => e.type === 'extension_ui_request' && e.title === 'cindy:permission');
  const res = ev.find(e => e.type === 'tool_execution_end');
  return { asked: ask ? JSON.parse(ask.placeholder) : null, result: res?.result?.content?.[0]?.text, isError: res?.isError };
}
let fail = 0;
const check = (name, ok, info) => { console.log(`${ok ? 'PASS' : 'FAIL'} ${name}${info ? ' — ' + JSON.stringify(info) : ''}`); if (!ok) fail++; };
setPerm({ mode: 'ask' });
let r = await turn('MOCK_READ:plain.txt');
check('ask: plain read not asked', !r.asked && r.result === 'hello', r);
r = await turn('MOCK_READ:.env');
check('ask: .env read asks with resolved evidence', r.asked?.toolName === 'read' && r.asked.resolvedCredentialPaths?.[0] === path.join(cwd, '.env'), r.asked);
answer = 'user-deny:not now';
r = await turn('MOCK_READ:notes.txt');
check('ask: symlink to ~/.ssh resolved', r.asked?.resolvedCredentialPaths?.[0] === path.join(root, '.ssh', 'id_rsa'), r.asked);
check('deny text matches bridge', r.isError && r.result === 'User denied this tool call via Cindy: not now', r.result);
answer = 'allow';
setPerm({ mode: 'bypassPermissions' });
r = await turn('MOCK_READ:.env');
check('bypass: credential read not asked', !r.asked && r.result === 'SECRET=1', r);
setPerm({ mode: 'ask', reviewOnly: true, reviewReadPaths: [cwd] });
r = await turn('MOCK_READ:.env');
check('review-only: credential read blocked', !r.asked && r.isError, r.result);
r = await turn('MOCK_READ:plain.txt');
check('review-only: in-scope read allowed', r.result === 'hello', r.result);
writeFileSync(permFile, '{broken');
r = await turn('MOCK_READ:.env');
check('broken permission file fails closed to ask', r.asked !== null, r.asked?.toolName);
child.kill(); mock.kill();
console.log(JSON.stringify({ ok: fail === 0 }));
process.exit(fail ? 1 : 0);
