// 思考档位对照：reasoning 模型在各档位下发出的请求体（thinking / output_config / reasoning_effort / max_tokens / beta 头）。
// 用法：PI_BIN=... OUT=x.json API=anthropic-messages|openai-completions COMPAT='{"forceAdaptiveThinking":true}' node think-test.mjs
import { spawn } from 'node:child_process';
import { mkdtempSync, mkdirSync, writeFileSync, readFileSync, existsSync, readdirSync } from 'node:fs';
import os from 'node:os'; import path from 'node:path'; import readline from 'node:readline';
const API = process.env.API || 'anthropic-messages';
const tmp = mkdtempSync(path.join(os.tmpdir(), 'pith-'));
const agentDir = path.join(tmp, 'agent'), sessions = path.join(tmp, 'sessions'), cwd = path.join(tmp, 'work');
for (const d of [agentDir, sessions, cwd]) mkdirSync(d, { recursive: true });
const LOG = path.join(tmp, 'req.log');
const root = path.resolve(import.meta.dirname, '../..');
const mock = spawn(process.execPath, [API === 'anthropic-messages' ? path.join(import.meta.dirname, 'mock-anthropic.mjs') : root + '/scripts/mock-openai.mjs'], { stdio: ['ignore', 'pipe', 'ignore'], env: { ...process.env, MOCK_LOG: LOG, MOCK_DUMP: '1', MOCK_DUMP_DIR: tmp } });
const port = await new Promise(r => readline.createInterface({ input: mock.stdout }).once('line', l => r(JSON.parse(l).port)));
const model = { id: 'mock-model', name: 'Mock', reasoning: true, maxTokens: Number(process.env.MAXTOK || 8000), contextWindow: 200000, ...(process.env.COMPAT ? { compat: JSON.parse(process.env.COMPAT) } : {}), ...(process.env.LEVELMAP ? { thinkingLevelMap: JSON.parse(process.env.LEVELMAP) } : {}) };
writeFileSync(path.join(agentDir, 'models.json'), JSON.stringify({ providers: { smoke: { baseUrl: `http://127.0.0.1:${port}`, api: API, apiKey: 'sk-none', models: [model] } } }));
const child = spawn(process.env.PI_BIN, ['--mode', 'rpc', '--no-extensions', '--no-skills', '--no-context-files', '--no-prompt-templates', '--session-dir', sessions, '--provider', 'smoke', '--model', 'mock-model'],
  { cwd, env: { ...process.env, PI_CODING_AGENT_DIR: agentDir, PI_OFFLINE: '1' }, stdio: ['pipe', 'pipe', 'inherit'] });
const out = []; let buf = '';
child.stdout.on('data', d => { buf += d; let i; while ((i = buf.indexOf('\n')) >= 0) { out.push(JSON.parse(buf.slice(0, i))); buf = buf.slice(i + 1); } });
const send = o => child.stdin.write(JSON.stringify(o) + '\n');
const sleep = ms => new Promise(r => setTimeout(r, ms));
const waitFor = async (pred, from, ms = 15000) => { const t = Date.now(); while (Date.now() - t < ms) { if (out.slice(from).some(pred)) return true; await sleep(30); } return false; };
const res = { levels: null, state0: null, steps: [] };
let k = out.length; send({ id: 'lv', type: 'get_available_thinking_levels' }); await waitFor(r => r.id === 'lv', k); res.levels = out.find(r => r.id === 'lv').data.levels;
k = out.length; send({ id: 'st', type: 'get_state' }); await waitFor(r => r.id === 'st', k); res.state0 = out.find(r => r.id === 'st').data.thinkingLevel;
for (const level of (process.env.LEVELS || 'off,minimal,low,medium,high,xhigh,max').split(',')) {
  k = out.length; send({ id: 'sl' + level, type: 'set_thinking_level', level }); await waitFor(r => r.id === 'sl' + level, k);
  const ev = out.slice(k).filter(r => r.type === 'thinking_level_changed').map(r => r.level);
  k = out.length; send({ id: 'gs' + level, type: 'get_state' }); await waitFor(r => r.id === 'gs' + level, k);
  const eff = out.find((r, i) => i >= k && r.id === 'gs' + level).data.thinkingLevel;
  const readReqs = () => API === 'anthropic-messages'
    ? (existsSync(LOG) ? readFileSync(LOG, 'utf8').split('\n').filter(Boolean).map(l => JSON.parse(l)) : [])
    : readdirSync(tmp).filter(f => f.startsWith('mock-openai-req-')).sort((a, b) => Number(a.split('-').pop().split('.')[0]) - Number(b.split('-').pop().split('.')[0])).map(f => ({ body: JSON.parse(readFileSync(path.join(tmp, f), 'utf8')), headers: {} }));
  const before = readReqs().length;
  k = out.length; send({ id: 'p' + level, type: 'prompt', message: 'hi ' + level }); await waitFor(r => r.type === 'agent_settled', k);
  const reqs = readReqs().slice(before);
  const r = reqs[0];
  const b = r?.body ?? {};
  res.sessionId = out.find(r => r.type === 'response' && r.command === 'get_state')?.data?.sessionId;
  res.steps.push({ level, changed: ev, effective: eff, thinking: b.thinking, output_config: b.output_config, reasoning_effort: b.reasoning_effort, enable_thinking: b.enable_thinking, max_tokens_field: b.max_tokens, store: b.store, max_tokens: b.max_tokens, keys: Object.keys(b).sort().join(','), cache_key: b.prompt_cache_key === res.sessionId ? 'SESSION' : b.prompt_cache_key, cache_ret: b.prompt_cache_retention, max_completion_tokens: b.max_completion_tokens, beta: r?.headers?.['anthropic-beta'], temperature: b.temperature });
}
child.stdin.end(); await sleep(300); child.kill(); mock.kill();
writeFileSync(process.env.OUT, JSON.stringify(res, null, 1));
console.log(JSON.stringify(res));
