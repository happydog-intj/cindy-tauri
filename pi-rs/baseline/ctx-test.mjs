// 上下文文件对照：cwd 链上放 AGENTS.md / CLAUDE.md，全局目录放 AGENTS.md，看原版系统消息里多出什么段。
import { spawn } from 'node:child_process';
import { mkdtempSync, mkdirSync, writeFileSync } from 'node:fs';
import os from 'node:os'; import path from 'node:path'; import readline from 'node:readline';
const tmp = mkdtempSync(path.join(os.tmpdir(), 'pictx-'));
const a = tmp + '/a', s = tmp + '/s', root = tmp + '/proj', w = root + '/sub/deep'; for (const d of [a, s, w]) mkdirSync(d, { recursive: true });
const files = JSON.parse(process.env.FILES || '{}');
for (const [rel, text] of Object.entries(files)) { const p = rel.startsWith('GLOBAL/') ? path.join(a, rel.slice(7)) : path.join(root, rel); mkdirSync(path.dirname(p), { recursive: true }); writeFileSync(p, text); }
mkdirSync(root + '/.git', { recursive: true });
const mock = spawn(process.execPath, [path.join(import.meta.dirname, 'mock-anthropic.mjs')], { stdio: ['ignore', 'pipe', 'ignore'] });
const port = await new Promise(r => readline.createInterface({ input: mock.stdout }).once('line', l => r(JSON.parse(l).port)));
writeFileSync(a + '/models.json', JSON.stringify({ providers: { smoke: { baseUrl: `http://127.0.0.1:${port}`, api: 'anthropic-messages', apiKey: 'k', models: [{ id: 'm', name: 'M' }] } } }));
const c = spawn(process.env.PI_BIN, ['--mode', 'rpc', '--no-approve', '--no-extensions', ...(process.env.EXTRA ? process.env.EXTRA.split(' ') : []), '--session-dir', s, '--provider', 'smoke', '--model', 'm'], { cwd: w, env: { ...process.env, PI_CODING_AGENT_DIR: a, PI_OFFLINE: '1' }, stdio: ['pipe', 'pipe', 'inherit'] });
const out = []; let buf = ''; c.stdout.on('data', d => { buf += d; let i; while ((i = buf.indexOf('\n')) >= 0) { out.push(JSON.parse(buf.slice(0, i))); buf = buf.slice(i + 1); } });
c.stdin.write(JSON.stringify({ id: 'p', type: 'prompt', message: 'hi' }) + '\n');
const t = Date.now(); while (Date.now() - t < 6000 && !out.some(r => r.type === 'agent_settled')) await new Promise(r => setTimeout(r, 40));
c.kill(); mock.kill();
const sys = out.find(r => r.type === 'message_end' && r.message.role === 'system')?.message;
const norm = x => typeof x === 'string' ? x.split(tmp).join('<TMP>').split('/private<TMP>').join('<TMP>') : x;
const res = { content: norm(sys?.content), sections: Object.fromEntries(Object.entries(sys?.sections ?? {}).filter(([k]) => !['preamble', 'tools', 'rules', 'docs'].includes(k)).map(([k, v]) => [k, norm(v)])), keys: Object.keys(sys?.sections ?? {}) };
writeFileSync(process.env.OUT, JSON.stringify(res, null, 1)); console.log(JSON.stringify(res, null, 1));
