// 真实网关上的自动压缩：小 contextWindow，连续读文件让上下文越过阈值，观察 compaction_start/end(reason=threshold)，
// 压缩后仍能回答压缩前的事实。用法：PI_BIN=... API=... MODEL=... node real-autocompact.mjs
import { spawn } from 'node:child_process';
import { mkdtempSync, mkdirSync, writeFileSync } from 'node:fs';
import os from 'node:os'; import path from 'node:path';
const MODEL = process.env.MODEL, API = process.env.API || 'anthropic-messages';
const tmp = mkdtempSync(path.join(os.tmpdir(), 'pirac-')); const a = tmp + '/a', s = tmp + '/s', w = tmp + '/w';
for (const d of [a, s, w]) mkdirSync(d, { recursive: true });
for (let i = 0; i < 6; i++) writeFileSync(`${w}/doc${i}.txt`, `Document ${i}. Secret number for doc ${i} is ${700 + i * 11}.\n` + Array.from({ length: Number(process.env.LINES || 40) }, (_, j) => `Line ${j} of document ${i}: unrelated notes ${(i * 7919 + j * 104729) % 99991} about subject ${(i * 31 + j * 17) % 977}.`).join('\n'));
writeFileSync(a + '/models.json', JSON.stringify({ providers: { gw: { baseUrl: 'http://127.0.0.1:30001/brconnector', api: API, apiKey: 'none', models: [{ id: MODEL, name: MODEL, contextWindow: Number(process.env.CW || 5000), maxTokens: 1024 }] } } }));
writeFileSync(a + '/settings.json', JSON.stringify({ compaction: { reserveTokens: 1200, keepRecentTokens: 800 } }));
const child = spawn(process.env.PI_BIN, ['--mode', 'rpc', '--no-extensions', '--no-skills', '--no-context-files', '--no-prompt-templates', '--session-dir', s, '--provider', 'gw', '--model', MODEL],
  { cwd: w, env: { ...process.env, PI_CODING_AGENT_DIR: a, PI_OFFLINE: '1' }, stdio: ['pipe', 'pipe', 'inherit'] });
const out = []; let buf = ''; child.stdout.on('data', d => { buf += d; let i; while ((i = buf.indexOf('\n')) >= 0) { out.push(JSON.parse(buf.slice(0, i))); buf = buf.slice(i + 1); } });
const send = o => child.stdin.write(JSON.stringify(o) + '\n');
const sleep = ms => new Promise(r => setTimeout(r, ms));
const until = async (f, from, ms = 150000) => { const t = Date.now(); while (Date.now() - t < ms) { if (out.slice(from).some(f)) return true; await sleep(50); } return false; };
const turns = [];
for (let i = 0; i < 6; i++) {
  const k = out.length; send({ id: 'p' + i, type: 'prompt', message: `Use the read tool to read doc${i}.txt and reply with only its secret number.` });
  const settled = await until(r => r.type === 'agent_settled', k);
  const last = out.slice(k).filter(r => r.type === 'message_end' && r.message.role === 'assistant').map(r => r.message).at(-1);
  const text = last?.content?.filter(b => b.type === 'text').map(b => b.text).join('') ?? '';
  turns.push({ i, settled, stop: last?.stopReason, err: last?.errorMessage, answer: text.trim().slice(0, 30), correct: text.includes(String(700 + i * 11)) });
  { send({ id: 'ss' + i, type: 'get_session_stats' }); await until(r => r.id === 'ss' + i, k, 5000); const st = out.find(r => r.id === 'ss' + i)?.data; turns.at(-1).ctx = st?.contextUsage?.tokens; turns.at(-1).lastUsage = last?.usage && { in: last.usage.input, out: last.usage.output, cR: last.usage.cacheRead, cW: last.usage.cacheWrite, total: last.usage.totalTokens }; }
  console.log(JSON.stringify(turns.at(-1)));
}
const k = out.length; send({ id: 'rec', type: 'prompt', message: 'Without reading any file: what was the secret number for doc 1?' });
await until(r => r.type === 'agent_settled', k);
const rec = out.slice(k).filter(r => r.type === 'message_end' && r.message.role === 'assistant').map(r => r.message).at(-1);
const recText = rec?.content?.filter(b => b.type === 'text').map(b => b.text).join('') ?? '';
const comps = out.filter(r => r.type === 'compaction_end').map(r => ({ reason: r.reason, ok: !!r.result, err: r.errorMessage, before: r.result?.tokensBefore, willRetry: r.willRetry }));
console.log('compactions:', JSON.stringify(comps)); console.log('recall doc1:', JSON.stringify(recText.slice(0, 80)));
child.kill();
const ok = turns.every(t => t.settled && t.stop === 'stop' && t.correct) && comps.some(c => c.ok && c.reason === 'threshold');
console.log('RESULT', ok ? 'ok' : 'FAIL'); process.exit(ok ? 0 : 1);
