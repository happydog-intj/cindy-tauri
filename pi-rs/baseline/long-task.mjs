// 真实模型长任务：同一批提示分别交给 PI_BIN，小 contextWindow 强制压缩，记录事件、耗时和 pi 进程 RSS 峰值。
// 用法：PI_BIN=... OUT=x.json MODEL=claude-shein-glm-5.3-flashx node long-task.mjs
import { spawn, execSync } from 'node:child_process';
import { mkdtempSync, mkdirSync, writeFileSync, readdirSync, cpSync } from 'node:fs';
import os from 'node:os'; import path from 'node:path';
const tmp = mkdtempSync(path.join(os.tmpdir(), 'pilong-'));
const agentDir = path.join(tmp, 'agent'), sessions = path.join(tmp, 'sessions'), cwd = path.join(tmp, 'work');
for (const d of [agentDir, sessions, cwd]) mkdirSync(d, { recursive: true });
cpSync(path.resolve(import.meta.dirname, '../src'), path.join(cwd, 'src'), { recursive: true });
const MODEL = process.env.MODEL || 'claude-shein-glm-5.3-flashx';
const CW = Number(process.env.CW || 12000);
writeFileSync(path.join(agentDir, 'models.json'), JSON.stringify({ providers: { br: { baseUrl: 'http://127.0.0.1:30001/brconnector', api: 'openai-completions', apiKey: 'none',
  models: [{ id: MODEL, name: MODEL, contextWindow: CW, maxTokens: Number(process.env.MAXTOK || 4096) }] } } }));
writeFileSync(path.join(agentDir, 'settings.json'), JSON.stringify({ compaction: { reserveTokens: Number(process.env.RESERVE || 3000), keepRecentTokens: Number(process.env.KEEP || 2500) } }));
const child = spawn(process.env.PI_BIN, ['--mode', 'rpc', '--no-extensions', '--no-skills', '--no-context-files', '--no-prompt-templates', '--session-dir', sessions, '--provider', 'br', '--model', MODEL],
  { cwd, env: { ...process.env, PI_CODING_AGENT_DIR: agentDir, PI_OFFLINE: '1' }, stdio: ['pipe', 'pipe', 'inherit'] });
const out = []; let buf = '';
child.stdout.on('data', d => { buf += d; let i; while ((i = buf.indexOf('\n')) >= 0) { out.push(JSON.parse(buf.slice(0, i))); buf = buf.slice(i + 1); } });
let peak = 0;
const rss = () => { try { return Number(execSync(`ps -o rss= -p ${child.pid}`).toString().trim()) / 1024; } catch { return 0; } };
const poll = setInterval(() => { peak = Math.max(peak, rss()); }, 200);
child.stdout.on('data', d => { if (process.env.DBG) for (const l of String(d).split('\n')) if (/compaction_|agent_end|agent_settled|"error"/.test(l)) console.error(new Date().toISOString().slice(17,23), l.slice(0,200)); });
const send = o => child.stdin.write(JSON.stringify(o) + '\n');
const sleep = ms => new Promise(r => setTimeout(r, ms));
const waitFor = async (pred, from, ms) => { const t = Date.now(); while (Date.now() - t < ms) { if (out.slice(from).some(pred)) return true; await sleep(100); } return false; };
const files = ['agent.rs', 'compact.rs', 'session.rs', 'rpc.rs', 'anthropic.rs', 'openai.rs', 'tools.rs', 'perm.rs'];
const t0 = Date.now(); const turns = [];
for (let i = 0; i < files.length; i++) {
  const k = out.length, ts = Date.now();
  send({ id: 'p' + i, type: 'prompt', message: `Use the read tool to read src/${files[i]}, then answer in two sentences what it does.` });
  const ok = await waitFor(r => r.type === 'agent_settled', k, 180000);
  const last = [...out.slice(k)].reverse().find(r => r.type === 'message_end' && r.message.role === 'assistant')?.message;
  turns.push({ file: files[i], ok, ms: Date.now() - ts, stop: last?.stopReason, err: last?.errorMessage, tools: out.slice(k).filter(r => r.type === 'tool_execution_end').length, ctx: last?.usage?.totalTokens });
}
const k = out.length; send({ id: 'cm', type: 'compact' }); await waitFor(r => r.id === 'cm', k, 180000);
send({ id: 'ss', type: 'get_session_stats' }); await waitFor(r => r.id === 'ss', k, 10000);
const stats = out.find(r => r.id === 'ss')?.data;
const comp = out.filter(r => r.type === 'compaction_end').map(r => ({ reason: r.reason, ok: !!r.result, err: r.errorMessage, before: r.result?.tokensBefore, after: r.result?.estimatedTokensAfter, retry: r.willRetry }));
const rssEnd = rss(); clearInterval(poll); child.stdin.end(); await sleep(300); child.kill();
const res = { bin: process.env.PI_BIN.split('/').slice(-4).join('/'), model: MODEL, cw: CW, totalMs: Date.now() - t0, peakRssMB: +peak.toFixed(1), endRssMB: +rssEnd.toFixed(1), turns, compactions: comp, stats: stats && { tokens: stats.tokens, contextUsage: stats.contextUsage, total: stats.totalMessages } };
writeFileSync(process.env.OUT, JSON.stringify(res, null, 1)); console.log(JSON.stringify(res));
