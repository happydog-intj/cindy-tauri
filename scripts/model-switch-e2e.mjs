// sidecar 级模型切换测试（不起 Tauri）：mock 模型 → 会话 → maker:set-model 同来源换模型 /
// 跨来源换模型 / 回合进行中切换（deferred，回合结束后生效）→ set-effort / set-fast-mode /
// set-permission-mode 落库 → 重启后按库里的新模型重建。以 mock 日志里的请求 model 为准判断实际路由。
// 用法：node scripts/model-switch-e2e.mjs   （需要先 npm run sidecar:build；Pi 未装时会自动下载）
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
const dataDir = process.env.CINDY_E2E_DATA_DIR || mkdtempSync(path.join(os.tmpdir(), 'cindy-model-switch-e2e-'));
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
    env: {
      ...process.env,
      CINDY_TAURI_DATA_DIR: dataDir,
      CINDY_TAURI_SMOKE_MODEL_BASE_URL: `http://127.0.0.1:${port}`,
      CINDY_TAURI_SMOKE_MODEL_ID: 'mock-model',
    },
    stdio: ['pipe', 'pipe', 'pipe'],
  });
  let seq = 0;
  const waiters = new Map();
  const pushes = [];
  const listeners = new Set();
  const errLines = [];
  readline.createInterface({ input: child.stderr }).on('line', (l) => { errLines.push(l); if (process.env.E2E_VERBOSE) console.error('  [sc]', l); });
  readline.createInterface({ input: child.stdout }).on('line', (line) => {
    const msg = JSON.parse(line);
    if (msg.push) {
      const p = { channel: msg.push.channel, args: decode(msg.push.payload) };
      pushes.push(p);
      for (const fn of listeners) fn(p);
      return;
    }
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
  return { call, invoke, pushes, listeners, stop, errLines };
}

const steps = [];
function check(name, cond, detail) {
  steps.push({ name, ok: !!cond });
  console.log(`${cond ? 'PASS' : 'FAIL'} ${name}${detail !== undefined ? ` — ${typeof detail === 'string' ? detail : JSON.stringify(detail)}` : ''}`);
  if (!cond) throw new Error(`step failed: ${name}`);
}

const terminalCount = (sc, sid) => sc.pushes.filter((p) => p.channel === 'maker:event' && p.args[0]?.sessionId === sid
  && ['done', 'error'].includes(p.args[0].event.type)).length;
async function waitTerminal(sc, sid, n, ms = 120_000) {
  const t0 = Date.now();
  while (terminalCount(sc, sid) < n) {
    if (Date.now() - t0 > ms) throw new Error(`timeout waiting for turn ${n}`);
    await new Promise((r) => setTimeout(r, 100));
  }
  return sc.pushes.filter((p) => p.channel === 'maker:event' && p.args[0]?.sessionId === sid
    && ['done', 'error'].includes(p.args[0].event.type))[n - 1].args[0].event;
}
// 本轮之后 mock 收到的聊天请求用的模型（去重）。
const modelsSince = (mock, mark) => [...new Set(mock.logLines.slice(mark)
  .map((l) => { try { return JSON.parse(l.replace(/^\[mock-openai\] /, '')); } catch { return null; } })
  .filter((e) => e?.path?.endsWith('/chat/completions') && e.model)
  .map((e) => e.model))];

const mock = await startMock();
let sc;
let failed = false;
try {
  sc = startSidecar(mock.port);
  await sc.call('secrets.unlock', { key: masterKey });

  // 第二个来源：无 key 的自定义 provider，两个模型，同一个 mock 端点。
  const altCfg = {
    id: 'alt', name: 'Alt', auth: { method: 'none' },
    runtimes: { pi: { baseUrl: `http://127.0.0.1:${mock.port}/v1`, wireProtocol: 'openai-chat', models: [{ id: 'alt-a', name: 'Alt A' }, { id: 'alt-b', name: 'Alt B' }] } },
  };
  check('create second provider', (await sc.invoke('maker:provider:custom:create', altCfg, {}, {}))?.ok === true);

  const row = await sc.invoke('local-db:sessions:create', { agentKind: 'pi', title: 'Switch E2E', workspaceKind: 'dialogue', model: 'mock-model', providerId: 'smoke', permissionMode: 'ask' });
  const sid = row.id;
  // renderer 入队项里的 createOpts 是建会话那刻的快照，切换后也不会变；sidecar 必须以 set-model 为准。
  const createOpts = { agentKind: 'pi', model: 'mock-model', providerId: 'smoke', permissionMode: 'ask', workingDir: row.workingDir, workspaceKind: 'dialogue' };
  let n = 0;
  const send = async (text) => {
    n += 1;
    const clientId = `u${n}`;
    await sc.invoke('maker:input:enqueue', sid, {
      clientId, text, persistedContent: JSON.stringify({ text }), model: 'mock-model', effort: 'medium', permissionMode: 'ask',
      workingDir: row.workingDir, chatMessage: { clientId, role: 'user', content: text }, createOpts,
    });
  };
  const turn = async (text) => {
    const mark = mock.logLines.length;
    await send(text);
    const ev = await waitTerminal(sc, sid, n);
    return { ev, models: modelsSince(mock, mark) };
  };

  const t1 = await turn('hi');
  check('first turn on smoke/mock-model', t1.ev.type === 'done' && t1.models.join() === 'mock-model', t1.models);

  // 1) 跨来源热切
  const r1 = await sc.invoke('maker:set-model', sid, 'alt-a', 'alt', undefined, { effort: null, fastMode: false });
  check('set-model cross-provider applied live', r1?.deferred === false, r1);
  const db1 = await sc.invoke('local-db:sessions:get', sid);
  check('cross-provider persisted', db1.model === 'alt-a' && db1.providerId === 'alt', { model: db1.model, providerId: db1.providerId });
  check('sessions:patched pushed', sc.pushes.some((p) => p.channel === 'local-db:sessions:patched' && p.args[0]?.sessionId === sid && p.args[0].patch?.model === 'alt-a'));
  const t2 = await turn('after cross-provider switch');
  check('next turn uses alt-a', t2.ev.type === 'done' && t2.models.join() === 'alt-a', t2.models);

  // 2) 同来源换模型（providerId 省略 = 沿用当前来源）
  const r2 = await sc.invoke('maker:set-model', sid, 'alt-b', undefined, undefined, { effort: null, fastMode: false });
  check('set-model same provider applied live', r2?.deferred === false, r2);
  const t3 = await turn('after same-provider switch');
  check('next turn uses alt-b', t3.ev.type === 'done' && t3.models.join() === 'alt-b', t3.models);

  // 3) 回合进行中切换 → deferred，当前回合不受影响，下一回合生效
  const mark4 = mock.logLines.length;
  await send('turn running while switching');
  const r3 = await sc.invoke('maker:set-model', sid, 'mock-model', 'smoke', undefined, { effort: null, fastMode: false });
  check('set-model during turn is deferred', r3?.deferred === true, r3);
  await waitTerminal(sc, sid, n);
  check('running turn kept alt-b', modelsSince(mock, mark4).join() === 'alt-b', modelsSince(mock, mark4));
  const t5 = await turn('after deferred switch');
  check('deferred switch applied after turn', t5.ev.type === 'done' && t5.models.join() === 'mock-model', t5.models);

  // 4) 其它运行时设置
  check('set-effort accepted', (await sc.invoke('maker:set-effort', sid, 'low')) === undefined);
  check('set-fast-mode accepted', (await sc.invoke('maker:set-fast-mode', sid, true)) === undefined);
  check('set-permission-mode accepted', (await sc.invoke('maker:set-permission-mode', sid, 'auto')) === undefined);
  const db2 = await sc.invoke('local-db:sessions:get', sid);
  check('effort / fast / permission persisted', db2.effort === 'low' && db2.fastMode === true && db2.permissionMode === 'auto',
    { effort: db2.effort, fastMode: db2.fastMode, permissionMode: db2.permissionMode });

  // 5) 非法参数和未配置的模型
  let bad = '';
  try { await sc.invoke('maker:set-model', sid, 'nope-model', 'alt'); } catch (e) { bad = e.message; }
  check('unknown model rejected', bad.includes('[MODEL_NOT_CONFIGURED]'), bad);
  bad = '';
  try { await sc.invoke('maker:set-model', sid, ''); } catch (e) { bad = e.message; }
  check('empty model rejected', bad.includes('[INVALID_PARAMS]'), bad);
  const db3 = await sc.invoke('local-db:sessions:get', sid);
  check('rejected switch leaves session unchanged', db3.model === 'mock-model' && db3.providerId === 'smoke');

  // 6) 换到 alt-a 后重启：懒创建按库里的模型走，而不是入队项里的旧 createOpts
  await sc.invoke('maker:set-model', sid, 'alt-a', 'alt', undefined, { effort: null, fastMode: false });
  await sc.stop();
  sc = startSidecar(mock.port);
  await sc.call('secrets.unlock', { key: masterKey });
  n = 0;
  const persisted = await sc.invoke('local-db:sessions:get', sid);
  check('selection survives restart', persisted.model === 'alt-a' && persisted.providerId === 'alt');
  // renderer 重开会话时 createOpts 取自库里的会话行；这里同样用库里的值。
  createOpts.model = persisted.model;
  createOpts.providerId = persisted.providerId;
  const t6 = await turn('after restart');
  check('restarted session runs on alt-a', t6.ev.type === 'done' && t6.models.join() === 'alt-a', t6.models);
} catch (err) {
  failed = true;
  console.error(String(err?.stack ?? err));
  if (sc) console.error(sc.errLines.slice(-30).join('\n'));
} finally {
  await sc?.stop().catch(() => {});
  mock.child.kill();
}
console.log(JSON.stringify({ ok: !failed, dataDir, steps: steps.length }));
process.exit(failed ? 1 : 0);
