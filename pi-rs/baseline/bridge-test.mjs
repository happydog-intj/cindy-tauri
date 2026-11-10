// bridge 行为测试（只测 pi-rs；原版需要真实 bridge 扩展）：bash 环境剔除密钥、ask_user_question、纯文本轮、启动通知。
import { spawn } from 'node:child_process';
import { mkdtempSync, mkdirSync, writeFileSync } from 'node:fs';
import os from 'node:os'; import path from 'node:path'; import readline from 'node:readline';
import assert from 'node:assert/strict';
const tmp = mkdtempSync(path.join(os.tmpdir(), 'pibr-')); const a = tmp + '/a', s = tmp + '/s', w = tmp + '/w';
for (const d of [a, s, w]) mkdirSync(d, { recursive: true });
const calls = [
  { name: 'bash', args: { command: 'env | grep -E "^(CINDY_PI_|PI_CODING|MYSECRET)" | sort; echo ---; echo $PI_CODING_AGENT_DIR' } },
  { name: 'ask_user_question', args: { questions: [{ question: 'Pick one', options: ['A', 'B'] }, { question: 'Name?' }] } },
  { name: 'ask_user_question', args: { questions: [{ question: 'Cancel me' }] } },
  { name: 'ask_user_question', args: { questions: [] } },
  { name: 'bash', args: { command: 'echo should-not-run' } },
  { name: 'bash', args: { command: 'echo works-again' } },
];
const mock = spawn(process.execPath, [path.join(import.meta.dirname, 'mock-anthropic.mjs')], { stdio: ['ignore', 'pipe', 'ignore'], env: { ...process.env, MOCK_TOOLS: JSON.stringify(calls) } });
const port = await new Promise(r => readline.createInterface({ input: mock.stdout }).once('line', l => r(JSON.parse(l).port)));
writeFileSync(a + '/models.json', JSON.stringify({ providers: { smoke: { baseUrl: `http://127.0.0.1:${port}`, api: 'anthropic-messages', apiKey: 'k', models: [{ id: 'm', name: 'M' }] } } }));
writeFileSync(tmp + '/perm.json', JSON.stringify({ mode: 'bypassPermissions', writableRoots: [w], readOnlyRoots: [] }));
const TOKEN = 'tok123';
const child = spawn(process.env.PI_BIN, ['--mode', 'rpc', '--no-approve', '--no-extensions', '--no-skills', '--no-context-files', '--no-prompt-templates', '--session-dir', s, '--provider', 'smoke', '--model', 'm', '--extension', '/x/cindy-bridge.ts'],
  { cwd: w, env: { ...process.env, PI_CODING_AGENT_DIR: a, PI_OFFLINE: '1', CINDY_PI_PERMISSION_FILE: tmp + '/perm.json', CINDY_PI_TURN_TOOL_POLICY: TOKEN, CINDY_PI_API_KEY: 'sekret-1', CINDY_PI_KEY_FOO: 'sekret-2', MYSECRET: 'sekret-3', CINDY_PI_SECRET_ENV_NAMES: '["MYSECRET"]', CINDY_PI_BASH_PACKAGE_HOME: tmp + '/pkg' }, stdio: ['pipe', 'pipe', 'inherit'] });
const out = []; let buf = ''; child.stdout.on('data', d => { buf += d; let i; while ((i = buf.indexOf('\n')) >= 0) { out.push(JSON.parse(buf.slice(0, i))); buf = buf.slice(i + 1); } });
const send = o => child.stdin.write(JSON.stringify(o) + '\n');
const until = async (f, ms = 8000) => { const t = Date.now(); while (Date.now() - t < ms) { const r = out.find(f); if (r) return r; await new Promise(r => setTimeout(r, 25)); } throw new Error('timeout'); };
// 1) 启动通知
const ready = await until(r => r.type === 'extension_ui_request' && r.method === 'notify');
assert.equal(ready.message, 'cindy:text-only-ready:' + TOKEN);
const toolEnd = async (i) => { const ends = () => out.filter(r => r.type === 'tool_execution_end'); const t = Date.now(); while (ends().length <= i && Date.now() - t < 8000) await new Promise(r => setTimeout(r, 25)); return ends()[i]; };
const text = r => r.result.content.map(c => c.text).join('');
// 2) bash 环境
let k = out.length; send({ id: 'p0', type: 'prompt', message: 'MOCK_TOOLS_NEXT 0' });
const e0 = text(await toolEnd(0)); await until(r => r.type === 'agent_settled' && out.indexOf(r) > k);
console.log('bash env:', JSON.stringify(e0));
assert.ok(!e0.includes('sekret'), 'secret leaked into bash env');
assert.ok(!e0.includes('CINDY_PI_API_KEY') && !e0.includes('CINDY_PI_KEY_FOO') && !e0.includes('CINDY_PI_PERMISSION_FILE') && !e0.includes('CINDY_PI_TURN_TOOL_POLICY'));
assert.ok(e0.trim().endsWith(tmp + '/pkg'), 'PI_CODING_AGENT_DIR should point at the isolated package home');
// 3) ask_user_question：select + input
k = out.length; send({ id: 'p1', type: 'prompt', message: 'MOCK_TOOLS_NEXT 1' });
const q1 = await until(r => r.type === 'extension_ui_request' && r.method === 'select' && out.indexOf(r) > k);
assert.deepEqual(q1.options, ['A', 'B']); assert.equal(q1.title, 'Pick one');
send({ type: 'extension_ui_response', id: q1.id, value: 'B' });
const q2 = await until(r => r.type === 'extension_ui_request' && r.method === 'input' && r.title === 'Name?');
send({ type: 'extension_ui_response', id: q2.id, value: 'Ann' });
const e1 = text(await toolEnd(1)); await until(r => r.type === 'agent_settled' && out.indexOf(r) > k);
console.log('ask answers:', e1);
assert.deepEqual(JSON.parse(e1), { answers: { 'Pick one': 'B', 'Name?': 'Ann' }, cancelled: false });
// 4) 取消
k = out.length; send({ id: 'p2', type: 'prompt', message: 'MOCK_TOOLS_NEXT 2' });
const q3 = await until(r => r.type === 'extension_ui_request' && r.title === 'Cancel me' && out.indexOf(r) > k);
send({ type: 'extension_ui_response', id: q3.id, cancelled: true });
const e2 = text(await toolEnd(2)); await until(r => r.type === 'agent_settled' && out.indexOf(r) > k);
assert.deepEqual(JSON.parse(e2), { answers: {}, cancelled: true });
// 5) 无效参数
k = out.length; send({ id: 'p3', type: 'prompt', message: 'MOCK_TOOLS_NEXT 3' });
const e3 = await toolEnd(3); await until(r => r.type === 'agent_settled' && out.indexOf(r) > k);
assert.equal(e3.isError, true); assert.match(text(e3), /one to three distinct questions/);
// 6) 纯文本轮：带前缀 → 工具被拒绝；不带前缀 → 正常
k = out.length; send({ id: 'p4', type: 'prompt', message: `[CINDY_TEXT_ONLY_INPUT]:${TOKEN}\nMOCK_TOOLS_NEXT 4` });
const e4 = await toolEnd(4); await until(r => r.type === 'agent_settled' && out.indexOf(r) > k);
assert.equal(e4.isError, true); assert.equal(text(e4), 'Tools are disabled for this host-owned text-only turn.');
const user = out.slice(k).find(r => r.type === 'message_end' && r.message.role === 'user');
assert.ok(!JSON.stringify(user).includes('CINDY_TEXT_ONLY_INPUT'), 'prefix must be stripped from the stored user message');
k = out.length; send({ id: 'p5', type: 'prompt', message: 'MOCK_TOOLS_NEXT 5' });
const e5 = await toolEnd(5); await until(r => r.type === 'agent_settled' && out.indexOf(r) > k);
assert.equal(e5.isError, false, 'tools work again after the text-only turn'); assert.equal(text(e5).trim(), 'works-again');
// 7) get_commands + 分支切换
send({ id: 'gc', type: 'get_commands' }); const gc = await until(r => r.id === 'gc');
assert.deepEqual(gc.data.commands.map(c => c.name), ['cindy-branch-switch']); assert.equal(gc.data.commands[0].source, 'extension');
send({ id: 'tr', type: 'get_tree' }); const tree = await until(r => r.id === 'tr');
const users = []; (function walk(ns) { for (const n of ns) { if (n.entry.type === 'message' && n.entry.message.role === 'user') users.push(n.entry); walk(n.children); } })(tree.data.tree);
const firstUser = users[0];
const before = out.length;
send({ id: 'bs', type: 'prompt', message: '/cindy-branch-switch ' + encodeURIComponent(JSON.stringify({ entryId: firstUser.id })) });
const bs = await until(r => r.id === 'bs' && out.indexOf(r) >= before);
assert.equal(bs.success, true); assert.equal(bs.data.disposition, 'handled');
assert.ok(!out.slice(before).some(r => r.type === 'agent_start'), 'branch switch must not start a model run');
send({ id: 'gm', type: 'get_messages' }); const gm = await until(r => r.id === 'gm');
assert.ok(!gm.data.messages.some(m => m.role === 'user'), 'leaf moved to the parent of the first user message, so no user messages remain on the branch');
send({ id: 'bs2', type: 'prompt', message: '/cindy-branch-switch ' + encodeURIComponent(JSON.stringify({ entryId: firstUser.id, summarize: true })) });
const bs2 = await until(r => r.id === 'bs2'); assert.equal(bs2.success, false); assert.match(bs2.error, /branch summaries/);
send({ id: 'bs3', type: 'prompt', message: '/cindy-branch-switch ' + encodeURIComponent(JSON.stringify({ entryId: 'nope' })) });
const bs3 = await until(r => r.id === 'bs3'); assert.equal(bs3.success, false); assert.match(bs3.error, /not found/);
child.kill(); mock.kill(); console.log('bridge-test ok');
