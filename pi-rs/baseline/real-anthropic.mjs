// 真实网关上的 Anthropic 协议检查：pi-rs（或 PI_BIN 指定的二进制）对本机网关发 /v1/messages，
// 覆盖文本流、思考档位、工具调用、多轮、压缩。用法：PI_BIN=... MODEL=claude-shein-shannon-auto node real-anthropic.mjs
import { spawn } from 'node:child_process';
import { mkdtempSync, mkdirSync, writeFileSync } from 'node:fs';
import os from 'node:os'; import path from 'node:path';
const MODEL = process.env.MODEL || 'claude-shein-shannon-auto';
const tmp = mkdtempSync(path.join(os.tmpdir(), 'pireal-')); const a = tmp + '/a', s = tmp + '/s', w = tmp + '/w';
for (const d of [a, s, w]) mkdirSync(d, { recursive: true });
if (process.env.CW) writeFileSync(a + '/settings.json', JSON.stringify({ compaction: { reserveTokens: Number(process.env.RESERVE || 1200), keepRecentTokens: Number(process.env.KEEP || 600) } }));
writeFileSync(w + '/notes.txt', 'The launch code word is PINEAPPLE-42.\nSecond line: ignore this.\n');
writeFileSync(a + '/models.json', JSON.stringify({ providers: { gw: { baseUrl: 'http://127.0.0.1:30001/brconnector', api: 'anthropic-messages', apiKey: 'none', models: [{ id: MODEL, name: MODEL, reasoning: process.env.REASONING === '1', contextWindow: Number(process.env.CW || 200000), maxTokens: 4096 }] } } }));
const child = spawn(process.env.PI_BIN, ['--mode', 'rpc', '--no-extensions', '--no-skills', '--no-context-files', '--no-prompt-templates', '--session-dir', s, '--provider', 'gw', '--model', MODEL],
  { cwd: w, env: { ...process.env, PI_CODING_AGENT_DIR: a, PI_OFFLINE: '1' }, stdio: ['pipe', 'pipe', 'inherit'] });
const out = []; let buf = ''; child.stdout.on('data', d => { buf += d; let i; while ((i = buf.indexOf('\n')) >= 0) { out.push(JSON.parse(buf.slice(0, i))); buf = buf.slice(i + 1); } });
const send = o => child.stdin.write(JSON.stringify(o) + '\n');
const sleep = ms => new Promise(r => setTimeout(r, ms));
const until = async (f, from, ms = 120000) => { const t = Date.now(); while (Date.now() - t < ms) { if (out.slice(from).some(f)) return true; await sleep(50); } return false; };
const res = [];
async function turn(name, message, check) {
  const k = out.length, t0 = Date.now(); send({ id: name, type: 'prompt', message });
  const settled = await until(r => r.type === 'agent_settled', k);
  const ev = out.slice(k);
  const asst = ev.filter(r => r.type === 'message_end' && r.message.role === 'assistant').map(r => r.message);
  const last = asst.at(-1);
  const text = asst.flatMap(m => m.content.filter(b => b.type === 'text').map(b => b.text)).join(' ');
  const tools = ev.filter(r => r.type === 'tool_execution_end').map(r => ({ name: r.toolName, err: r.isError, out: r.result.content[0].text.slice(0, 80) }));
  const r = { name, settled, ms: Date.now() - t0, stop: last?.stopReason, err: last?.errorMessage, usage: last?.usage && { in: last.usage.input, out: last.usage.output, cacheRead: last.usage.cacheRead, cacheWrite: last.usage.cacheWrite }, thinking: asst.some(m => m.content.some(b => b.type === 'thinking')), tools, text: text.slice(0, 160) };
  r.pass = settled && last?.stopReason !== 'error' && (check ? check(r, text) : true);
  res.push(r); console.log(JSON.stringify(r));
}
await turn('plain', 'Reply with exactly: pong', (r, t) => /pong/i.test(t));
await turn('tool-read', 'Use the read tool to read notes.txt and tell me the launch code word.', (r, t) => r.tools.some(x => x.name === 'read' && !x.err) && /PINEAPPLE-42/.test(t));
await turn('tool-bash', 'Use the bash tool to run: echo hello-from-bash. Then say what it printed.', (r, t) => r.tools.some(x => x.name === 'bash' && /hello-from-bash/.test(x.out)) && /hello-from-bash/.test(t));
for (let i = 0; i < Number(process.env.EXTRA || 0); i++) await turn('filler' + i, 'Use the read tool to read notes.txt, then reply in one short sentence.', () => true);
await turn('multi-turn', 'What was the launch code word you found earlier? One short sentence.', (r, t) => /PINEAPPLE-42/.test(t));
for (const level of (process.env.LEVELS || '').split(',').filter(Boolean)) {
  send({ id: 'lv' + level, type: 'set_thinking_level', level }); await sleep(200);
  await turn('think-' + level, 'What is 17 * 23? Answer with just the number.', (r, t) => /391/.test(t));
}
const k = out.length; send({ id: 'cm', type: 'compact' }); await until(r => r.id === 'cm', k, 120000);
const cm = out.find(r => r.id === 'cm' && out.indexOf(r) >= k); console.log('compact:', JSON.stringify({ ok: cm?.success, err: cm?.error, summary: cm?.data?.summary?.slice(0, 100) }));
res.push({ name: 'compact', pass: cm?.success === true || (!process.env.CW && /too small/.test(cm?.error || '')) });
const comps = out.filter(r => r.type === 'compaction_end'); console.log('compaction events:', JSON.stringify(comps.map(c => ({ reason: c.reason, ok: !!c.result, err: c.errorMessage, before: c.result?.tokensBefore }))));
send({ id: 'ss', type: 'get_session_stats' }); await until(r => r.id === 'ss', k, 5000);
child.kill();
console.log('SUMMARY', res.map(r => `${r.name}:${r.pass ? 'ok' : 'FAIL'}`).join(' '));
process.exit(res.every(r => r.pass) ? 0 : 1);
