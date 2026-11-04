// sidecar 级会话链路测试（不起 Tauri）：mock 模型 → create-session → enqueue → 流式回复 →
// Ask 模式审批写文件 → 带 API key 的 provider（安全存储、Bearer、Pi 经 env 拿 key）→
// 重启 sidecar → 会话列表、历史和已存 key 仍在 → 断开连接。
// 主密钥正常由 Rust 从系统钥匙串取出后经 secrets.unlock 送来；这里直接用随机值模拟。
// 用法：node scripts/session-e2e.mjs   （需要先 npm run sidecar:build；Pi 未装时会自动下载）
import { spawn } from 'node:child_process';
import { randomBytes } from 'node:crypto';
import { existsSync, mkdtempSync, readdirSync, readFileSync, statSync } from 'node:fs';
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

const masterKey = randomBytes(32).toString('hex');
// 测试用的假 key，只存在于本进程、mock 进程 env 和加密后的 secrets 文件里。
const API_KEY = `sk-e2e-${randomBytes(12).toString('hex')}`;

function startMock() {
  const child = spawn(process.execPath, [path.join(root, 'scripts', 'mock-openai.mjs')], {
    stdio: ['ignore', 'pipe', 'pipe'],
    env: { ...process.env, MOCK_API_KEY: API_KEY },
  });
  const logLines = [];
  readline.createInterface({ input: child.stderr }).on('line', (l) => logLines.push(l));
  return new Promise((resolve) => {
    readline.createInterface({ input: child.stdout }).once('line', (l) => resolve({ child, port: JSON.parse(l).port, logLines }));
  });
}

// 数据目录里任何文件（跳过 Pi 安装目录和大文件）都不能出现明文 key。
function findPlaintext(dir, needle, hits = []) {
  for (const name of readdirSync(dir)) {
    const p = path.join(dir, name);
    const st = statSync(p);
    if (st.isDirectory()) { if (name !== 'agents') findPlaintext(p, needle, hits); continue; }
    if (st.size > 20 * 1024 * 1024) continue;
    if (readFileSync(p).includes(needle)) hits.push(path.relative(dir, p));
  }
  return hits;
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
  return { call, invoke, waitPush, pushes, stop, errLines };
}

const SCOPE = { dataOwnerId: 'local-v1', ownerGeneration: 1 };
const keyedView = async (sc) => (await sc.invoke('maker:provider:list')).providers.find((p) => p.id === 'keyed');
const doneFor = (sc, sid) => sc.pushes.filter((p) => p.channel === 'maker:event' && p.args[0]?.sessionId === sid && p.args[0].event.type === 'done').length;

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
  await sc.call('secrets.unlock', { key: masterKey });
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

  // 3) 带 API key 的 provider
  const keyedUrl = `http://127.0.0.1:${mock.port}/keyed`;
  const noKey = await sc.invoke('maker:provider:models-fetch', { agent: 'pi', baseUrl: keyedUrl, authMethod: 'apiKey' });
  check('models-fetch without key → AUTH_INVALID', noKey.ok === false && noKey.code === 'AUTH_INVALID', noKey);
  const withKey = await sc.invoke('maker:provider:models-fetch', { agent: 'pi', baseUrl: keyedUrl, authMethod: 'apiKey', apiKey: API_KEY });
  check('models-fetch with key', withKey.ok === true && withKey.models?.some((m) => m.id === 'mock-model'), withKey.ok ? withKey.models.map((m) => m.id) : withKey);
  const keyedConfig = (baseUrl) => ({
    id: 'keyed', name: 'Keyed', auth: { method: 'apiKey' },
    runtimes: { pi: { baseUrl, wireProtocol: 'openai-chat', models: [{ id: 'mock-model', name: 'Mock (keyed)' }] } },
  });
  check('create keyed provider', (await sc.invoke('maker:provider:custom:create', keyedConfig(keyedUrl), { pi: API_KEY }, {}))?.ok === true);
  check('keyed provider connected', (await keyedView(sc))?.connected === true);
  check('renderer reads back provider key', (await sc.invoke('safe-storage-read', 'provider_key_keyed_pi')) === API_KEY);
  check('header blobs stay main-only', (await sc.invoke('safe-storage-store', 'provider_headers_keyed_pi', '{}')) === false
    && (await sc.invoke('safe-storage-read', 'provider_headers_keyed_pi')) === null);
  const saved = await sc.invoke('maker:provider:test-connection', { kind: 'saved', providerId: 'keyed', agent: 'pi' });
  check('saved test-connection uses stored key', saved.ok === true, saved);
  const fetchSaved = await sc.invoke('maker:provider:models-fetch', { agent: 'pi', baseUrl: keyedUrl, authMethod: 'apiKey', savedProviderId: 'keyed' });
  check('models-fetch with savedProviderId uses stored key', fetchSaved.ok === true, fetchSaved.ok ? 'ok' : fetchSaved);

  const krow = await sc.invoke('local-db:sessions:create', { agentKind: 'pi', title: 'E2E keyed', workspaceKind: 'dialogue', model: 'mock-model', providerId: 'keyed', permissionMode: 'ask' });
  const kOpts = { agentKind: 'pi', model: 'mock-model', providerId: 'keyed', permissionMode: 'ask', workingDir: krow.workingDir, workspaceKind: 'dialogue' };
  const authOkBefore = mock.logLines.filter((l) => l.includes('/keyed/chat/completions') && l.includes('"auth":"ok"')).length;
  await sc.invoke('maker:input:enqueue', krow.id, {
    clientId: 'k1', text: 'hi', persistedContent: JSON.stringify({ text: 'hi' }), model: 'mock-model', effort: 'medium', permissionMode: 'ask',
    workingDir: krow.workingDir, chatMessage: { clientId: 'k1', role: 'user', content: 'hi' }, createOpts: kOpts,
  });
  await sc.waitPush((p) => p.channel === 'maker:event' && p.args[0]?.sessionId === krow.id && ['done', 'error'].includes(p.args[0].event.type));
  const kErr = sc.pushes.find((p) => p.channel === 'maker:event' && p.args[0]?.sessionId === krow.id && p.args[0].event.type === 'error');
  const authOk = mock.logLines.filter((l) => l.includes('/keyed/chat/completions') && l.includes('"auth":"ok"')).length;
  check('Pi turn authenticated with Bearer key', !kErr && doneFor(sc, krow.id) >= 1 && authOk > authOkBefore, kErr ? kErr.args[0].event : `${authOk - authOkBefore} authed requests`);
  check('no plaintext key on disk', findPlaintext(dataDir, API_KEY).length === 0, findPlaintext(dataDir, API_KEY));
  check('no key in sidecar stderr', !sc.errLines.some((l) => l.includes(API_KEY)));
  await sc.stop();

  // 4) 重启后会话列表与历史
  sc = startSidecar(mock.port);
  // 主密钥到位前，带 key 的 provider 不能被当成已连接。
  check('keyed provider not connected while locked', (await keyedView(sc))?.connected === false);
  await sc.call('secrets.unlock', { key: masterKey });
  check('keyed provider connected after unlock', (await keyedView(sc))?.connected === true);
  let rejected = false;
  try { await sc.call('secrets.unlock', { key: masterKey }); } catch { rejected = true; }
  check('second unlock rejected', rejected);
  const list = await sc.invoke('local-db:sessions:list', 20);
  const again = list.find((s) => s.id === sid);
  check('session listed after restart', again, again ? { title: again.title, preview: again.preview, count: again._count } : list.map((s) => s.id));
  const view = await sc.invoke('local-db:messages:view', sid, {});
  check('history view after restart', view?.version === 1 && view.items.length > 0, `${view.items.length} items, types=${view.items.map((i) => i.type).join(',')}`);
  const interrupted = await sc.invoke('local-db:sessions:interrupted-pending');
  check('no false interrupted alert', !interrupted.includes(sid), interrupted);

  // 5) 改端点不带新 key → 旧 key 清掉；重新给 key → 恢复；断开 → 清 key、保留配置。
  await sc.invoke('maker:provider:custom:update', keyedConfig(`http://localhost:${mock.port}/keyed`), {}, {});
  check('endpoint change clears key', (await keyedView(sc))?.connected === false && (await sc.invoke('safe-storage-read', 'provider_key_keyed_pi')) === null);
  await sc.invoke('maker:provider:custom:update', keyedConfig(keyedUrl), { pi: API_KEY }, {});
  check('update with key reconnects', (await keyedView(sc))?.connected === true);
  check('disconnect', (await sc.invoke('maker:provider:custom:disconnect', 'keyed', SCOPE, {}))?.ok === true
    && (await keyedView(sc))?.connected === false && (await sc.invoke('safe-storage-read', 'provider_key_keyed_pi')) === null);
  check('mock never logged the key', !mock.logLines.some((l) => l.includes(API_KEY)));
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
