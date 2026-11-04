// sidecar 级会话链路测试（不起 Tauri）：mock 模型 → create-session → enqueue → 流式回复 →
// Ask 模式审批写文件 → 重启 sidecar → 会话列表与历史仍在。
// 用法：node scripts/session-e2e.mjs   （需要先 npm run sidecar:build；Pi 未装时会自动下载）
import { spawn } from 'node:child_process';
import { existsSync, mkdtempSync, readFileSync } from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import readline from 'node:readline';
import { fileURLToPath } from 'node:url';
import { decode, encode } from '../shared/ipc-codec.mjs';

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const sidecarJs = path.join(root, 'sidecar', 'dist', 'sidecar.cjs');
const node = process.env.CINDY_E2E_NODE || process.execPath;
// 复用已下载的 Pi（只读），避免每次重下 31MB。
const dataDir = process.env.CINDY_E2E_DATA_DIR || mkdtempSync(path.join(os.tmpdir(), 'cindy-e2e-'));

function startMock() {
  const child = spawn(process.execPath, [path.join(root, 'scripts', 'mock-openai.mjs')], { stdio: ['ignore', 'pipe', 'pipe'] });
  child.stderr.on('data', () => {});
  return new Promise((resolve) => {
    readline.createInterface({ input: child.stdout }).once('line', (l) => resolve({ child, port: JSON.parse(l).port }));
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
  const waitPush = (pred, ms = 120_000) => new Promise((resolve, reject) => {
    const hit = pushes.find(pred);
    if (hit) return resolve(hit);
    const t = setTimeout(() => { listeners.delete(fn); reject(new Error('push timeout')); }, ms);
    const fn = (p) => { if (pred(p)) { clearTimeout(t); listeners.delete(fn); resolve(p); } };
    listeners.add(fn);
  });
  const stop = () => new Promise((resolve) => { child.once('exit', resolve); child.stdin.end(); });
  return { invoke, waitPush, pushes, stop, errLines };
}

const steps = [];
function check(name, cond, detail) {
  steps.push({ name, ok: !!cond, ...(detail !== undefined ? { detail } : {}) });
  console.log(`${cond ? 'PASS' : 'FAIL'} ${name}${detail !== undefined ? ` — ${typeof detail === 'string' ? detail : JSON.stringify(detail)}` : ''}`);
  if (!cond) throw new Error(`step failed: ${name}`);
}

const mock = await startMock();
let sc;
let failed = false;
try {
  sc = startSidecar(mock.port);
  const caps = await sc.invoke('maker:get-capabilities', 'pi');
  const model = caps.availableModels?.find((m) => m.id === 'mock-model');
  check('capabilities lists mock model', model, caps.availableModels?.map((m) => `${m.providerId ?? ''}/${m.id}`));
  const providerId = model.providerId ?? 'smoke';

  const row = await sc.invoke('local-db:sessions:create', { agentKind: 'pi', title: 'E2E', workspaceKind: 'dialogue', model: 'mock-model', providerId, permissionMode: 'ask' });
  check('local-db session created', row?.id && row.workingDir, row?.workingDir);
  const sid = row.id;
  const createOpts = { agentKind: 'pi', model: 'mock-model', providerId, permissionMode: 'ask', workingDir: row.workingDir, workspaceKind: 'dialogue' };
  // 形状同 renderer 入队的 AgentInputQueuedMessage（shared/agentInputQueue.ts）。
  const queued = (clientId, text) => ({
    clientId, text, persistedContent: JSON.stringify({ text }), model: 'mock-model', effort: 'medium', permissionMode: 'ask',
    workingDir: row.workingDir, chatMessage: { clientId, role: 'user', content: text }, createOpts,
  });

  // 1) 流式回复
  await sc.invoke('maker:input:enqueue', sid, queued('u1', 'hi'));
  await sc.waitPush((p) => p.channel === 'maker:event' && p.args[0]?.sessionId === sid && p.args[0].event.type === 'done');
  const textEvents = sc.pushes.filter((p) => p.channel === 'maker:event' && p.args[0].event.type === 'text');
  check('streaming text events', textEvents.length >= 2, `${textEvents.length} text events`);

  // 2) Ask 模式审批写文件
  await sc.invoke('maker:input:enqueue', sid, queued('u2', 'MOCK_WRITE_FILE please'));
  const req = await sc.waitPush((p) => p.channel === 'maker:interaction-request' && p.args[0]?.sessionId === sid);
  const request = req.args[0].request;
  check('approval requested', request.kind === 'permission', request.toolName ?? request.kind);
  const resolved = await sc.invoke('maker:resolve-interaction', request.requestId, { kind: 'permission', behavior: 'allow' });
  check('approval accepted', resolved?.accepted === true);
  const doneCount = () => sc.pushes.filter((p) => p.channel === 'maker:event' && p.args[0]?.sessionId === sid && p.args[0].event.type === 'done').length;
  const t0 = Date.now();
  while (doneCount() < 2 && Date.now() - t0 < 120_000) await new Promise((r) => setTimeout(r, 200));
  const written = path.join(row.workingDir, 'mock-ok.txt');
  check('tool wrote file after approval', existsSync(written) && readFileSync(written, 'utf8').trim() === 'ok', written);

  const hist = await sc.invoke('local-db:messages:list', sid, { limit: 100 });
  check('history persisted', hist.length >= 4, hist.map((m) => m.role).join(','));
  await sc.stop();

  // 3) 重启后会话列表与历史
  sc = startSidecar(mock.port);
  const list = await sc.invoke('local-db:sessions:list', 20);
  const again = list.find((s) => s.id === sid);
  check('session listed after restart', again, again ? { title: again.title, preview: again.preview, count: again._count } : list.map((s) => s.id));
  const view = await sc.invoke('local-db:messages:view', sid, {});
  check('history view after restart', view?.version === 1 && view.items.length > 0, `${view.items.length} items, types=${view.items.map((i) => i.type).join(',')}`);
  const interrupted = await sc.invoke('local-db:sessions:interrupted-pending');
  check('no false interrupted alert', !interrupted.includes(sid), interrupted);
} catch (err) {
  failed = true;
  console.error(String(err?.stack ?? err));
  if (sc) console.error(sc.errLines.slice(-30).join('\n'));
  if (sc) console.error('pushes:', JSON.stringify(sc.pushes.slice(-25).map((p) => [p.channel, p.args[0]?.event?.type ?? p.args[0]?.error ?? p.args[0]?.turnInProgress ?? ''])));
} finally {
  await sc?.stop().catch(() => {});
  mock.child.kill();
}
console.log(JSON.stringify({ ok: !failed, dataDir, steps: steps.length }));
process.exit(failed ? 1 : 0);
