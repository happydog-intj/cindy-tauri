// 从原版录下指定 --tools 组合的系统消息（sections + toolsAdded），用来生成 pi-rs 的静态数据。
import { spawn } from 'node:child_process';
import { mkdtempSync, mkdirSync, writeFileSync } from 'node:fs';
import os from 'node:os'; import path from 'node:path'; import readline from 'node:readline';
const sets = (process.env.SETS || 'read,bash,edit,write|read,grep,find,ls|read,bash,grep,find,ls').split('|');
const result = {};
for (const tools of sets) {
  const tmp = mkdtempSync(path.join(os.tmpdir(), 'pits2-'));
  const a = tmp + '/a', s = tmp + '/s', w = tmp + '/w'; for (const d of [a, s, w]) mkdirSync(d, { recursive: true });
  const mock = spawn(process.execPath, [path.join(import.meta.dirname, 'mock-anthropic.mjs')], { stdio: ['ignore', 'pipe', 'ignore'] });
  const port = await new Promise(r => readline.createInterface({ input: mock.stdout }).once('line', l => r(JSON.parse(l).port)));
  writeFileSync(a + '/models.json', JSON.stringify({ providers: { smoke: { baseUrl: `http://127.0.0.1:${port}`, api: 'anthropic-messages', apiKey: 'k', models: [{ id: 'm', name: 'M' }] } } }));
  const c = spawn(process.env.PI_BIN, ['--mode', 'rpc', '--no-approve', '--no-extensions', '--tools', tools, '--no-skills', '--no-context-files', '--no-prompt-templates', '--session-dir', s, '--provider', 'smoke', '--model', 'm'], { cwd: w, env: { ...process.env, PI_CODING_AGENT_DIR: a, PI_OFFLINE: '1' }, stdio: ['pipe', 'pipe', 'inherit'] });
  const out = []; let buf = ''; c.stdout.on('data', d => { buf += d; let i; while ((i = buf.indexOf('\n')) >= 0) { out.push(JSON.parse(buf.slice(0, i))); buf = buf.slice(i + 1); } });
  c.stdin.write(JSON.stringify({ id: 'p', type: 'prompt', message: 'hi' }) + '\n');
  const t = Date.now(); while (Date.now() - t < 8000 && !out.some(r => r.type === 'agent_settled')) await new Promise(r => setTimeout(r, 50));
  c.kill(); mock.kill();
  const sys = out.find(r => r.type === 'message_end' && r.message.role === 'system')?.message;
  result[tools] = sys;
}
writeFileSync(process.env.OUT, JSON.stringify(result, null, 1));
for (const [k, v] of Object.entries(result)) console.log(k, '=>', v ? Object.keys(v.sections).join(',') : 'NONE', (v?.toolsAdded || []).map(t => t.name).join(','));
