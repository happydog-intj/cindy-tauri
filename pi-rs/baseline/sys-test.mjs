// 系统消息对照：不同 --tools / 上下文文件 / --append-system-prompt 组合下，原版与 pi-rs 的系统消息（路径归一化后）逐字段比较。
import { spawn } from 'node:child_process';
import { mkdtempSync, mkdirSync, writeFileSync } from 'node:fs';
import os from 'node:os'; import path from 'node:path'; import readline from 'node:readline';
const cases = JSON.parse(process.env.CASES);
const result = {};
for (const [name, c] of Object.entries(cases)) {
  const tmp = mkdtempSync(path.join(os.tmpdir(), 'pisys-')); const a = tmp + '/a', s = tmp + '/s', root = tmp + '/proj', w = root + '/sub/deep';
  for (const d of [a, s, w]) mkdirSync(d, { recursive: true }); mkdirSync(root + '/.git', { recursive: true });
  for (const [rel, text] of Object.entries(c.files || {})) { const p = rel.startsWith('GLOBAL/') ? path.join(a, rel.slice(7)) : path.join(root, rel); mkdirSync(path.dirname(p), { recursive: true }); writeFileSync(p, text); }
  const mock = spawn(process.execPath, [path.join(import.meta.dirname, 'mock-anthropic.mjs')], { stdio: ['ignore', 'pipe', 'ignore'] });
  const port = await new Promise(r => readline.createInterface({ input: mock.stdout }).once('line', l => r(JSON.parse(l).port)));
  writeFileSync(a + '/models.json', JSON.stringify({ providers: { smoke: { baseUrl: `http://127.0.0.1:${port}`, api: 'anthropic-messages', apiKey: 'k', models: [{ id: 'm', name: 'M' }] } } }));
  const child = spawn(process.env.PI_BIN, ['--mode', 'rpc', '--no-approve', '--no-extensions', '--no-skills', '--no-prompt-templates', ...(c.args || []), '--session-dir', s, '--provider', 'smoke', '--model', 'm'], { cwd: w, env: { ...process.env, PI_CODING_AGENT_DIR: a, PI_OFFLINE: '1' }, stdio: ['pipe', 'pipe', 'inherit'] });
  const out = []; let buf = ''; child.stdout.on('data', d => { buf += d; let i; while ((i = buf.indexOf('\n')) >= 0) { out.push(JSON.parse(buf.slice(0, i))); buf = buf.slice(i + 1); } });
  child.stdin.write(JSON.stringify({ id: 'p', type: 'prompt', message: 'hi' }) + '\n');
  const t = Date.now(); while (Date.now() - t < 6000 && !out.some(r => r.type === 'agent_settled')) await new Promise(r => setTimeout(r, 40));
  child.kill(); mock.kill();
  const sys = out.find(r => r.type === 'message_end' && r.message.role === 'system')?.message;
  const norm = x => JSON.parse(JSON.stringify(x ?? null).split('/private' + tmp).join('<TMP>').split(tmp).join('<TMP>').replace(/\/Users\/[^"\\]*?\/(darwin-arm64|release)(?=[\/"\\])/g, '<PKG>'));
  result[name] = norm(sys && { content: sys.content, sections: sys.sections, toolsAdded: sys.toolsAdded, keys: Object.keys(sys) });
}
writeFileSync(process.env.OUT, JSON.stringify(result, null, 1)); console.log(Object.keys(result).length, 'cases');
