// export_html 对照：一次对话后导出，输出 HTML 与会话数据（解 base64）。
import { spawn } from 'node:child_process';
import { mkdtempSync, mkdirSync, writeFileSync, readFileSync, existsSync } from 'node:fs';
import os from 'node:os'; import path from 'node:path'; import readline from 'node:readline';
const tmp = mkdtempSync(path.join(os.tmpdir(), 'piexp-')); const a = tmp + '/a', s = tmp + '/s', w = tmp + '/w';
for (const d of [a, s, w]) mkdirSync(d, { recursive: true });
const mock = spawn(process.execPath, [path.join(import.meta.dirname, 'mock-anthropic.mjs')], { stdio: ['ignore', 'pipe', 'ignore'] });
const port = await new Promise(r => readline.createInterface({ input: mock.stdout }).once('line', l => r(JSON.parse(l).port)));
writeFileSync(a + '/models.json', JSON.stringify({ providers: { smoke: { baseUrl: `http://127.0.0.1:${port}`, api: 'anthropic-messages', apiKey: 'k', models: [{ id: 'm', name: 'M' }] } } }));
const child = spawn(process.env.PI_BIN, ['--mode', 'rpc', '--no-approve', '--no-extensions', '--no-skills', '--no-context-files', '--no-prompt-templates', '--session-dir', s, '--provider', 'smoke', '--model', 'm'], { cwd: w, env: { ...process.env, PI_CODING_AGENT_DIR: a, PI_OFFLINE: '1' }, stdio: ['pipe', 'pipe', 'inherit'] });
const out = []; let buf = ''; child.stdout.on('data', d => { buf += d; let i; while ((i = buf.indexOf('\n')) >= 0) { out.push(JSON.parse(buf.slice(0, i))); buf = buf.slice(i + 1); } });
const send = o => child.stdin.write(JSON.stringify(o) + '\n');
const wait = async f => { const t = Date.now(); while (Date.now() - t < 8000 && !out.some(f)) await new Promise(r => setTimeout(r, 30)); };
const res = {};
send({ id: 'e0', type: 'export_html' }); await wait(r => r.id === 'e0'); res.empty = out.find(r => r.id === 'e0');
send({ id: 'p', type: 'prompt', message: 'hello' }); await wait(r => r.type === 'agent_settled');
const target = tmp + '/out.html';
send({ id: 'e1', type: 'export_html', outputPath: target }); await wait(r => r.id === 'e1'); res.explicit = out.find(r => r.id === 'e1');
send({ id: 'e2', type: 'export_html' }); await wait(r => r.id === 'e2'); res.implicit = out.find(r => r.id === 'e2');
child.kill(); mock.kill();
const norm = x => JSON.stringify(x).split('/private' + tmp).join('<TMP>').split(tmp).join('<TMP>');
console.log(norm(res));
if (existsSync(target)) { writeFileSync(process.env.OUT, readFileSync(target)); console.log('html bytes', readFileSync(target).length); }
