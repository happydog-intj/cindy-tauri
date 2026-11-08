// sidecar 级 Goal（/goal 自主续跑）测试：mock 模型按 [Goal] 指令回裁决块，看控制器能不能自己跑完多轮。
// 覆盖：设目标即开跑 → 多轮后 complete；blocked；轮数上限 budgetLimited；暂停 / 恢复；改目标；清除；
// 重启后 active 目标接着跑；/goal 命令（设置、清除、无参开弹窗、无会话报错）；参数校验。
// 用法：node scripts/goal-e2e.mjs   （需要先 npm run sidecar:build；Pi 未装时会自动下载）
import { spawn } from 'node:child_process';
import { randomBytes } from 'node:crypto';
import { mkdtempSync } from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import readline from 'node:readline';
import { fileURLToPath } from 'node:url';
import { decode, encode } from '../shared/ipc-codec.mjs';

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const sidecarJs = path.join(root, 'sidecar', 'dist', 'sidecar.cjs');
const node = process.env.CINDY_E2E_NODE || process.execPath;
const dataDir = process.env.CINDY_E2E_DATA_DIR || mkdtempSync(path.join(os.tmpdir(), 'cindy-goal-e2e-'));
const masterKey = randomBytes(32).toString('hex');

function startMock() {
  const child = spawn(process.execPath, [path.join(root, 'scripts', 'mock-openai.mjs')], { stdio: ['ignore', 'pipe', 'pipe'] });
  const logLines = [];
  readline.createInterface({ input: child.stderr }).on('line', (l) => logLines.push(l));
  return new Promise((resolve) => {
    readline.createInterface({ input: child.stdout }).once('line', (l) => resolve({ child, port: JSON.parse(l).port, logLines }));
  });
}

function startSidecar(port) {
  const child = spawn(node, [sidecarJs], {
    cwd: path.dirname(sidecarJs),
    env: { ...process.env, CINDY_TAURI_DATA_DIR: dataDir, CINDY_TAURI_SMOKE_MODEL_BASE_URL: `http://127.0.0.1:${port}`, CINDY_TAURI_SMOKE_MODEL_ID: 'mock-model' },
    stdio: ['pipe', 'pipe', 'pipe'],
  });
  let seq = 0;
  const waiters = new Map();
  const pushes = [];
  const errLines = [];
  readline.createInterface({ input: child.stderr }).on('line', (l) => { errLines.push(l); if (process.env.E2E_VERBOSE) console.error('  [sc]', l); });
  readline.createInterface({ input: child.stdout }).on('line', (line) => {
    const msg = JSON.parse(line);
    if (msg.push) { pushes.push({ channel: msg.push.channel, args: decode(msg.push.payload) }); return; }
    const w = waiters.get(msg.id);
    if (!w) return;
    waiters.delete(msg.id);
    if (msg.error) w.reject(new Error(msg.error)); else w.resolve(msg.result);
  });
  const call = (method, params) => new Promise((resolve, reject) => {
    const id = ++seq;
    waiters.set(id, { resolve, reject });
    child.stdin.write(JSON.stringify({ id, method, params }) + '\n');
  });
  const invoke = async (channel, ...a) => decode(await call('ipc.invoke', { channel, payload: encode(a) }));
  const stop = () => new Promise((resolve) => { child.once('exit', resolve); child.stdin.end(); });
  return { call, invoke, pushes, stop, errLines };
}

const steps = [];
function check(name, cond, detail) {
  steps.push({ name, ok: !!cond });
  console.log(`${cond ? 'PASS' : 'FAIL'} ${name}${detail !== undefined ? ` — ${typeof detail === 'string' ? detail : JSON.stringify(detail)}` : ''}`);
  if (!cond) throw new Error(`step failed: ${name}`);
}
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
async function waitFor(fn, ms = 60_000, what = 'condition') {
  const t0 = Date.now();
  for (;;) {
    const v = await fn();
    if (v) return v;
    if (Date.now() - t0 > ms) throw new Error(`timeout waiting for ${what}`);
    await sleep(100);
  }
}
const goalLines = (mock, mark) => mock.logLines.slice(mark)
  .map((l) => { try { return JSON.parse(l.replace(/^\[mock-openai\] /, '')); } catch { return null; } })
  .filter((e) => e?.path?.endsWith('/chat/completions') && String(e.reason ?? '').startsWith('goal-'));

const mock = await startMock();
let sc;
let failed = false;
try {
  sc = startSidecar(mock.port);
  await sc.call('secrets.unlock', { key: masterKey });
  const newSession = async (title) => (await sc.invoke('local-db:sessions:create', {
    agentKind: 'pi', title, workspaceKind: 'dialogue', model: 'mock-model', providerId: 'smoke', permissionMode: 'ask',
  }));
  const statusOf = (sid) => sc.invoke('maker:goal:get-status', sid);
  const waitStatus = (sid, wanted, ms = 90_000) => waitFor(async () => {
    const s = await statusOf(sid);
    return s && wanted.includes(s.status) ? s : null;
  }, ms, `goal ${wanted.join('|')}`);

  check('no goal → null', (await statusOf((await newSession('empty')).id)) === null);

  // 1) 设目标 → 自己跑 3 轮后 complete
  const a = await newSession('goal complete');
  const markA = mock.logLines.length;
  check('set goal', (await sc.invoke('maker:goal:set', { sessionId: a.id, objective: 'ship it MOCK_GOAL_COMPLETE_AFTER=3' })).ok === true);
  const doneA = await waitStatus(a.id, ['complete']);
  const turnsA = goalLines(mock, markA);
  check('goal ran multiple turns by itself, then complete', doneA.turnsUsed === 3 && turnsA.length === 3, { turnsUsed: doneA.turnsUsed, requests: turnsA.length });
  check('verdict reason recorded', /goal turn 3/.test(doneA.lastReason ?? ''), doneA.lastReason);
  check('status pushes include active and complete', ['active', 'complete'].every((st) => sc.pushes.some((p) => p.channel === 'maker:goal:status-changed' && p.args[0]?.sessionId === a.id && p.args[0].goal?.status === st)));
  check('first turn carries clarify + verdict contract, later turns only the verdict', turnsA.length === 3);
  await sleep(1500);
  check('no extra turn after complete', goalLines(mock, markA).length === 3);

  // 2) blocked
  const b = await newSession('goal blocked');
  await sc.invoke('maker:goal:set', { sessionId: b.id, objective: 'needs a password MOCK_GOAL_BLOCKED' });
  const blocked = await waitStatus(b.id, ['blocked']);
  check('blocked verdict stops the loop', blocked.turnsUsed === 1 && /blocked/.test(blocked.lastReason ?? ''), blocked.lastReason);

  // 3) 轮数上限
  const c = await newSession('goal max turns');
  await sc.invoke('maker:goal:set', { sessionId: c.id, objective: 'never ends', limits: { maxTurns: 2, budgetTokens: null, noProgressLimit: null } });
  const limited = await waitStatus(c.id, ['budgetLimited']);
  check('max turns → budgetLimited', limited.turnsUsed === 2 && limited.maxTurns === 2, limited.lastReason);

  // 4) 连续空轮 → paused，恢复后继续
  const d = await newSession('goal no progress');
  await sc.invoke('maker:goal:set', { sessionId: d.id, objective: 'chat only', limits: { maxTurns: null, budgetTokens: null, noProgressLimit: 2 } });
  const paused = await waitStatus(d.id, ['paused']);
  check('no-progress limit → paused', paused.turnsUsed === 2 && /no tool use/.test(paused.lastReason ?? ''), paused.lastReason);
  await sc.invoke('maker:goal:update', { sessionId: d.id, patch: { noProgressLimit: null, maxTurns: 4 } });
  check('update limits', (await statusOf(d.id)).maxTurns === 4);
  await sc.invoke('maker:goal:resume', d.id);
  const afterResume = await waitStatus(d.id, ['budgetLimited']);
  check('resume continues counting and hits the new limit', afterResume.turnsUsed === 4, afterResume.turnsUsed);

  // 5) 暂停 / 清除 / 改目标
  const e = await newSession('goal pause clear');
  await sc.invoke('maker:goal:set', { sessionId: e.id, objective: 'slow burn', limits: { maxTurns: 50, budgetTokens: null, noProgressLimit: null } });
  await sc.invoke('maker:goal:pause', e.id);
  const pausedE = await statusOf(e.id);
  check('pause', pausedE.status === 'paused');
  const turnsAtPause = pausedE.turnsUsed;
  await sleep(1200);
  check('paused goal does not run', (await statusOf(e.id)).turnsUsed === turnsAtPause);
  await sc.invoke('maker:goal:update', { sessionId: e.id, patch: { objective: 'changed objective' } });
  check('update objective', (await statusOf(e.id)).objective === 'changed objective');
  await sc.invoke('maker:goal:clear', e.id);
  check('clear → null + push', (await statusOf(e.id)) === null && sc.pushes.some((p) => p.channel === 'maker:goal:status-changed' && p.args[0]?.sessionId === e.id && p.args[0].goal === null));

  // 6) 校验
  const bad = async (fn) => { try { await fn(); return null; } catch (err) { return String(err.message); } };
  check('empty objective rejected', /INVALID_PARAMS/.test(await bad(() => sc.invoke('maker:goal:set', { sessionId: a.id, objective: '   ' })) ?? ''));
  check('unknown session rejected', /NOT_FOUND/.test(await bad(() => sc.invoke('maker:goal:set', { sessionId: 'no-such-session', objective: 'x' })) ?? ''));
  check('bad limit rejected', /INVALID_PARAMS/.test(await bad(() => sc.invoke('maker:goal:update', { sessionId: d.id, patch: { maxTurns: -1 } })) ?? ''));
  check('update without goal → GOAL_NOT_FOUND', /GOAL_NOT_FOUND/.test(await bad(() => sc.invoke('maker:goal:update', { sessionId: e.id, patch: { maxTurns: 3 } })) ?? ''));

  // 7) /goal 命令
  const cmds = await sc.invoke('maker:list-desktop-commands');
  check('palette lists /goal', cmds.success && cmds.commands.some((x) => x.name === 'goal'));
  const f = await newSession('goal command');
  const triggered = (pred) => sc.pushes.find((p) => p.channel === 'maker:desktop-command-triggered' && pred(p.args[0]));
  await sc.invoke('maker:execute-desktop-command', 'goal', { args: 'x' });
  check('/goal without session → goal-no-session', triggered((x) => x.error === 'goal-no-session'));
  await sc.invoke('maker:execute-desktop-command', 'goal', { sessionId: f.id, args: '' });
  check('bare /goal → open-dialog', triggered((x) => x.goalAction === 'open-dialog' && x.sessionId === f.id));
  await sc.invoke('maker:execute-desktop-command', 'goal', { sessionId: f.id, args: 'via command MOCK_GOAL_COMPLETE_AFTER=2' });
  check('/goal <text> → set', triggered((x) => x.goalAction === 'set' && x.sessionId === f.id));
  const viaCmd = await waitStatus(f.id, ['complete']);
  check('goal set by command runs to complete', viaCmd.turnsUsed === 2);
  await sc.invoke('maker:execute-desktop-command', 'goal', { sessionId: f.id, args: 'clear' });
  check('/goal clear → cleared', triggered((x) => x.goalAction === 'cleared') && (await statusOf(f.id)) === null);

  // 8) 重启：active 目标接着跑
  const g = await newSession('goal restart');
  // mock 的回合不调用工具，默认的「连续 3 轮空轮就暂停」会先触发，这里关掉它，只测重启续跑。
  await sc.invoke('maker:goal:set', { sessionId: g.id, objective: 'survive restart MOCK_GOAL_COMPLETE_AFTER=4', limits: { maxTurns: null, budgetTokens: null, noProgressLimit: null } });
  await waitFor(async () => ((await statusOf(g.id))?.turnsUsed ?? 0) >= 1, 60_000, 'first turn');
  await sc.stop();
  sc = startSidecar(mock.port);
  await sc.call('secrets.unlock', { key: masterKey });
  const resumed = await waitStatus(g.id, ['complete'], 120_000);
  check('active goal resumes after restart and completes', resumed.turnsUsed === 4, resumed.turnsUsed);
  check('no goal errors in log', !sc.errLines.some((l) => /\[goal\] (warn|error)/.test(l)), sc.errLines.filter((l) => /\[goal\]/.test(l)).slice(0, 3));
} catch (err) {
  failed = true;
  console.error(`ERROR ${err?.stack ?? err}`);
  if (sc) console.error(sc.errLines.filter((l) => /goal|maker-ipc/.test(l)).slice(-20).join('\n'));
} finally {
  if (sc) await Promise.race([sc.stop(), sleep(5000)]);
  mock.child.kill();
}
console.log(JSON.stringify({ ok: !failed && steps.every((s) => s.ok), dataDir, steps: steps.length }));
process.exit(failed ? 1 : 0);
