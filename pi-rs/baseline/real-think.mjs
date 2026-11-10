// 真实网关上的思考块：同一个问题，Anthropic 和 OpenAI chat 两种协议，检查流事件、消息里的 thinking 块，
// 以及第二轮请求能否把思考块带回去（Anthropic 需要 signature，OpenAI 用 reasoning_content）。
// 用法：PI_BIN=... API=anthropic-messages|openai-completions MODEL=claude-shein-glm-5.3-flashx node real-think.mjs
import { spawn } from 'node:child_process';
import { mkdtempSync, mkdirSync, writeFileSync } from 'node:fs';
import os from 'node:os'; import path from 'node:path';
const MODEL = process.env.MODEL, API = process.env.API || 'anthropic-messages';
const tmp = mkdtempSync(path.join(os.tmpdir(), 'pirt-')); const a = tmp + '/a', s = tmp + '/s', w = tmp + '/w';
for (const d of [a, s, w]) mkdirSync(d, { recursive: true });
writeFileSync(a + '/models.json', JSON.stringify({ providers: { gw: { baseUrl: 'http://127.0.0.1:30001/brconnector', api: API, apiKey: 'none', models: [{ id: MODEL, name: MODEL, reasoning: true, contextWindow: 200000, maxTokens: 8000 }] } } }));
const child = spawn(process.env.PI_BIN, ['--mode', 'rpc', '--no-extensions', '--no-skills', '--no-context-files', '--no-prompt-templates', '--session-dir', s, '--provider', 'gw', '--model', MODEL],
  { cwd: w, env: { ...process.env, PI_CODING_AGENT_DIR: a, PI_OFFLINE: '1' }, stdio: ['pipe', 'pipe', 'inherit'] });
const out = []; let buf = ''; child.stdout.on('data', d => { buf += d; let i; while ((i = buf.indexOf('\n')) >= 0) { out.push(JSON.parse(buf.slice(0, i))); buf = buf.slice(i + 1); } });
const send = o => child.stdin.write(JSON.stringify(o) + '\n');
const sleep = ms => new Promise(r => setTimeout(r, ms));
const until = async (f, from, ms = 150000) => { const t = Date.now(); while (Date.now() - t < ms) { if (out.slice(from).some(f)) return true; await sleep(50); } return false; };
const results = {};
for (const level of (process.env.LEVELS || 'low,high').split(',')) {
  send({ id: 'lv', type: 'set_thinking_level', level }); await sleep(200);
  for (const q of ['What is 17*23? Just the number.', 'Now multiply that by 2. Just the number.']) {
    const k = out.length; send({ id: 'p', type: 'prompt', message: q });
    const settled = await until(r => r.type === 'agent_settled', k);
    const ev = out.slice(k);
    const kinds = [...new Set(ev.filter(r => r.type === 'message_update').map(r => r.assistantMessageEvent.type))];
    const msg = ev.filter(r => r.type === 'message_end' && r.message.role === 'assistant').map(r => r.message).at(-1);
    const th = msg?.content.filter(b => b.type === 'thinking') ?? [];
    const key = `${level}/${q.startsWith('Now') ? 'turn2' : 'turn1'}`;
    results[key] = { settled, stop: msg?.stopReason, err: msg?.errorMessage, text: msg?.content.filter(b => b.type === 'text').map(b => b.text).join('').slice(0, 40), thinkingBlocks: th.length, thinkingChars: th.reduce((n, b) => n + b.thinking.length, 0), sig: th[0] ? (th[0].thinkingSignature || '').slice(0, 16) : null, events: kinds.filter(x => x.startsWith('thinking')) };
    console.log(key, JSON.stringify(results[key]));
  }
}
child.kill();
const ok = Object.values(results).every(r => r.settled && r.stop === 'stop' && !r.err);
console.log('RESULT', ok ? 'ok' : 'FAIL');
process.exit(ok ? 0 : 1);
