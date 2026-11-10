// 真实网关上的上下文溢出恢复：把 contextWindow 声明得比网关实际限制（20 万）大，文件内容让请求真的超限，
// 网关返回 "prompt is too long" → pi-rs 应识别为溢出、压缩、重试，最后答出文件里的暗号。
import { spawn } from 'node:child_process';
import { mkdtempSync, mkdirSync, writeFileSync } from 'node:fs';
import os from 'node:os'; import path from 'node:path';
const MODEL = process.env.MODEL || 'claude-shein-shannon-auto', API = process.env.API || 'anthropic-messages';
const tmp = mkdtempSync(path.join(os.tmpdir(), 'piov-')); const a = tmp + '/a', s = tmp + '/s', w = tmp + '/w';
for (const d of [a, s, w]) mkdirSync(d, { recursive: true });
// 读文件结果截断到 2000 行 / 50KB，所以要分多个文件各读一次，累计超过 20 万 token
const WORDS = Number(process.env.WORDS || 700000);
writeFileSync(w + '/code.txt', 'The override passphrase is MANGO-77.\n');
writeFileSync(a + '/models.json', JSON.stringify({ providers: { gw: { baseUrl: 'http://127.0.0.1:30001/brconnector', api: API, apiKey: 'none', models: [{ id: MODEL, name: MODEL, contextWindow: 2000000, maxTokens: Number(process.env.MAXTOK || 8000) }] } } }));
writeFileSync(a + '/settings.json', JSON.stringify({ retry: { maxRetries: 1, baseDelayMs: 100 }, compaction: { reserveTokens: 20000, keepRecentTokens: 30000 } }));
const child = spawn(process.env.PI_BIN, ['--mode', 'rpc', '--no-extensions', '--no-skills', '--no-context-files', '--no-prompt-templates', '--session-dir', s, '--provider', 'gw', '--model', MODEL],
  { cwd: w, env: { ...process.env, PI_CODING_AGENT_DIR: a, PI_OFFLINE: '1' }, stdio: ['pipe', 'pipe', 'inherit'] });
const out = []; let buf = ''; child.stdout.on('data', d => { buf += d; let i; while ((i = buf.indexOf('\n')) >= 0) { out.push(JSON.parse(buf.slice(0, i))); buf = buf.slice(i + 1); } });
const send = o => child.stdin.write(JSON.stringify(o) + '\n');
const sleep = ms => new Promise(r => setTimeout(r, ms));
const until = async (f, from, ms = 280000) => { const t = Date.now(); while (Date.now() - t < ms) { if (out.slice(from).some(f)) return true; await sleep(100); } return false; };
let k = out.length; send({ id: 'c', type: 'prompt', message: 'Use the read tool on code.txt and remember the passphrase. Reply "noted".' }); await until(r => r.type === 'agent_settled', k);
// 真实的溢出场景：很多条中等大小的消息逐步累积（每条都低于网关限制），声明的 contextWindow 比网关实际限制大，
// 累计超过 20 万 token 时网关拒绝，pi-rs 要识别溢出、把旧消息摘要掉、重试并成功。
const CHUNK = Number(process.env.CHUNK || 55000);  // 每条约 11 万 token，一条放得下，三条放不下
const chunk = Array.from({ length: CHUNK }, (_, i) => 'w' + i).join(' ');
let overflowSeen = false;
for (let i = 0; i < Number(process.env.MSGS || 4); i++) {
  k = out.length; send({ id: 'm' + i, type: 'prompt', message: `Part ${i}:\n` + chunk + '\n\nReply with exactly: ack' + i });
  const done = await until(r => r.type === 'agent_settled', k, 400000);
  const evs = out.slice(k);
  const asst = evs.filter(r => r.type === 'message_end' && r.message.role === 'assistant').map(r => r.message);
  const marks = evs.filter(r => /^(compaction_|auto_retry_)/.test(r.type) || r.type === 'agent_end').map(r => r.type + (r.reason ? ':' + r.reason : '') + (r.willRetry !== undefined ? ':willRetry=' + r.willRetry : ''));
  if (marks.some(m => m.startsWith('compaction_start:overflow'))) overflowSeen = true;
  console.log(JSON.stringify({ i, settled: done, msgs: asst.map(m => m.stopReason + (m.errorMessage ? ':' + m.errorMessage.slice(0, 60) : '')), marks }));
}
k = out.length; send({ id: 'q', type: 'prompt', message: 'What is the override passphrase from code.txt? Do not call tools; answer from memory.' });
await until(r => r.type === 'agent_settled', k);
const fin = out.slice(k).filter(r => r.type === 'message_end' && r.message.role === 'assistant').map(r => r.message).at(-1);
const answer = fin?.content?.filter(b => b.type === 'text').map(b => b.text).join('') ?? '';
const comps = out.filter(r => r.type === 'compaction_end').map(r => ({ reason: r.reason, ok: !!r.result, err: r.errorMessage, willRetry: r.willRetry, before: r.result?.tokensBefore }));
console.log('compactions:', JSON.stringify(comps)); console.log('answer:', JSON.stringify(answer.slice(0, 100)), 'stop:', fin?.stopReason);
child.kill();
const ok = overflowSeen && comps.some(c => c.ok && c.reason === 'overflow') && fin?.stopReason === 'stop' && /MANGO-77/.test(answer);
console.log('RESULT', ok ? 'ok' : 'FAIL'); process.exit(ok ? 0 : 1);
