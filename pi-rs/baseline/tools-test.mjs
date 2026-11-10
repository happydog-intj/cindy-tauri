// 工具输出对照：在固定目录树上让原版/pi-rs 各跑一批 grep/find/ls 调用，比较 toolResult 文本。
// 用 mock-anthropic 的 MOCK_TOOL=<json> 让模型发出指定工具调用。
import { spawn } from 'node:child_process';
import { mkdtempSync, mkdirSync, writeFileSync, symlinkSync } from 'node:fs';
import os from 'node:os'; import path from 'node:path'; import readline from 'node:readline';
import { readFileSync } from 'node:fs';
const calls = JSON.parse(process.env.CALLS || readFileSync(path.join(import.meta.dirname, 'tools-calls.json'), 'utf8'));
const tmp = mkdtempSync(path.join(os.tmpdir(), 'pitool-')); const a = tmp + '/a', s = tmp + '/s', w = tmp + '/w';
for (const d of [a, s, w]) mkdirSync(d, { recursive: true });
// 固定目录树
const T = { 'src/main.rs': 'fn main() {\n    println!("hello");\n}\n// TODO: fix\n', 'src/lib.rs': 'pub fn add(a: i32, b: i32) -> i32 { a + b }\n// todo lower\n', 'src/util/mod.rs': 'pub mod x;\n', 'src/util/x.rs': 'const X: u8 = 1; // TODO later\n', 'README.md': '# Title\nHello world\nTODO list\n', '.hidden/secret.txt': 'TODO hidden\n', 'node_modules/pkg/index.js': 'TODO in node_modules\n', 'docs/a.md': 'alpha\nbeta\ngamma\ndelta\nepsilon\nTODO zeta\neta\ntheta\n', 'empty/.keep': '', 'Zed.txt': 'z\n', 'apple.txt': 'a\n', 'Banana.txt': 'b\n', '.gitignore': 'ignored/\n*.log\n', 'ignored/file.txt': 'TODO ignored\n', 'x.log': 'TODO log\n' };
for (const [rel, text] of Object.entries(T)) { const p = path.join(w, rel); mkdirSync(path.dirname(p), { recursive: true }); writeFileSync(p, text); }
mkdirSync(w + '/.git', { recursive: true }); mkdirSync(w + '/many', { recursive: true });
for (let i = 0; i < 30; i++) writeFileSync(path.join(w, 'many', `f${String(i).padStart(2, '0')}.txt`), `line TODO ${i}\n`);
const mock = spawn(process.execPath, [path.join(import.meta.dirname, 'mock-anthropic.mjs')], { stdio: ['ignore', 'pipe', 'ignore'], env: { ...process.env, MOCK_TOOLS: JSON.stringify(calls) } });
const port = await new Promise(r => readline.createInterface({ input: mock.stdout }).once('line', l => r(JSON.parse(l).port)));
writeFileSync(a + '/models.json', JSON.stringify({ providers: { smoke: { baseUrl: `http://127.0.0.1:${port}`, api: 'anthropic-messages', apiKey: 'k', models: [{ id: 'm', name: 'M' }] } } }));
const child = spawn(process.env.PI_BIN, ['--mode', 'rpc', '--no-approve', '--no-extensions', '--tools', 'read,bash,edit,write,grep,find,ls', '--no-skills', '--no-context-files', '--no-prompt-templates', '--session-dir', s, '--provider', 'smoke', '--model', 'm'], { cwd: w, env: { ...process.env, PI_CODING_AGENT_DIR: a, PI_OFFLINE: '1' }, stdio: ['pipe', 'pipe', 'inherit'] });
const out = []; let buf = ''; child.stdout.on('data', d => { buf += d; let i; while ((i = buf.indexOf('\n')) >= 0) { out.push(JSON.parse(buf.slice(0, i))); buf = buf.slice(i + 1); } });
const send = o => child.stdin.write(JSON.stringify(o) + '\n');
for (let i = 0; i < calls.length; i++) {
  const k = out.length; send({ id: 'p' + i, type: 'prompt', message: 'MOCK_TOOLS_NEXT ' + i });
  const t = Date.now(); while (Date.now() - t < 20000 && !out.slice(k).some(r => r.type === 'agent_settled')) await new Promise(r => setTimeout(r, 30));
}
child.kill(); mock.kill();
const norm = x => String(x).split('/private' + tmp).join('<TMP>').split(tmp).join('<TMP>');
const results = out.filter(r => r.type === 'tool_execution_end').map((r, i) => ({ call: calls[i], isError: r.isError, text: norm(r.result.content.map(c => c.text).join('')) }));
writeFileSync(process.env.OUT, JSON.stringify(results, null, 1)); console.log(results.length, 'results');
