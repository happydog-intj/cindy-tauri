// sidecar 级设备互联测试（不起 Tauri）：mock 中继 + mock 云端 + mock 模型，本脚本同时扮演 Rust 和「手机」。
// 手机用 cindy 上游同一个 DeviceLinkClient（经 esbuild 打包），走 link-open → subscribe → invoke 的真实线协议。
// 覆盖：登录后上线、被控开关关闭时拒绝、打开后接入、列任务、看历史、发消息收推送、远程审批、
// 待审批快照、未实现通道回 NOT_IMPLEMENTED、白名单外通道被拒、撤销后拒绝、退出后下线。
// 用法：node scripts/device-link-e2e.mjs   （需要先 npm run sidecar:build；Pi 未装时会自动下载）
import { spawn } from 'node:child_process';
import { randomBytes } from 'node:crypto';
import { mkdtempSync } from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import readline from 'node:readline';
import { fileURLToPath, pathToFileURL } from 'node:url';
import { build } from 'esbuild';
import { decode, encode } from '../shared/ipc-codec.mjs';

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const cindyDir = path.resolve(root, process.env.CINDY_DIR ?? '../cindy');
const sidecarJs = path.join(root, 'sidecar', 'dist', 'sidecar.cjs');
const node = process.env.CINDY_E2E_NODE || process.execPath;
const dataDir = process.env.CINDY_E2E_DATA_DIR || mkdtempSync(path.join(os.tmpdir(), 'cindy-device-link-e2e-'));
const masterKey = randomBytes(32).toString('hex');
const PHONE_TOKEN = `phone-${randomBytes(8).toString('hex')}`;
const PHONE_ID = 'phone-device-0001';
const DESKTOP_ID = 'tauri-device-0001';

// 手机端：把上游 DeviceLinkClient 打成一个 ESM 文件再 import。
const phoneBundle = path.join(dataDir, 'phone-client.mjs');
await build({
  stdin: { contents: "export { DeviceLinkClient } from '@cindy/device-link';", resolveDir: root, loader: 'js' },
  bundle: true, platform: 'node', format: 'esm', outfile: phoneBundle, logLevel: 'error',
  nodePaths: [path.join(cindyDir, 'apps', 'desktop', 'node_modules'), path.join(cindyDir, 'node_modules')],
});
const { DeviceLinkClient } = await import(pathToFileURL(phoneBundle).href);
const { default: WebSocket } = await import(pathToFileURL(path.join(cindyDir, 'node_modules', 'ws', 'wrapper.mjs')).href);

function startProc(script, env) {
  const child = spawn(process.execPath, [path.join(root, 'scripts', script)], { stdio: ['ignore', 'pipe', 'pipe'], env: { ...process.env, ...env } });
  const logLines = [];
  readline.createInterface({ input: child.stderr }).on('line', (l) => { logLines.push(l); if (process.env.E2E_VERBOSE) console.error(`  [${script}]`, l); });
  return new Promise((resolve, reject) => {
    readline.createInterface({ input: child.stdout }).once('line', (l) => resolve({ child, port: JSON.parse(l).port, logLines }));
    child.once('exit', (code) => reject(new Error(`${script} exited early (${code}): ${logLines.slice(-3).join(' | ')}`)));
  });
}

function startSidecar(env) {
  const child = spawn(node, [sidecarJs], { cwd: path.dirname(sidecarJs), env: { ...process.env, CINDY_TAURI_DATA_DIR: dataDir, ...env }, stdio: ['pipe', 'pipe', 'pipe'] });
  let seq = 0;
  const waiters = new Map();
  const pushes = [];
  const errLines = [];
  readline.createInterface({ input: child.stderr }).on('line', (l) => { errLines.push(l); if (process.env.E2E_VERBOSE) console.error('  [sc]', l); });
  readline.createInterface({ input: child.stdout }).on('line', (line) => {
    const msg = JSON.parse(line);
    if (msg.push) { pushes.push({ channel: msg.push.channel, args: decode(msg.push.payload) }); return; }
    if (msg.native) {
      const { id } = msg.native;
      if (id !== undefined) child.stdin.write(JSON.stringify({ nativeReply: { id, result: null } }) + '\n');
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
  return { call, invoke, pushes, stop, errLines };
}

const steps = [];
function check(name, cond, detail) {
  steps.push({ name, ok: !!cond });
  console.log(`${cond ? 'PASS' : 'FAIL'} ${name}${detail !== undefined ? ` — ${typeof detail === 'string' ? detail : JSON.stringify(detail)}` : ''}`);
  if (!cond) throw new Error(`step failed: ${name}`);
}
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
async function waitFor(fn, ms = 15_000, what = 'condition') {
  const t0 = Date.now();
  for (;;) {
    const v = await fn();
    if (v) return v;
    if (Date.now() - t0 > ms) throw new Error(`timeout waiting for ${what}`);
    await sleep(100);
  }
}

const DESKTOP_SIG = `sig${randomBytes(6).toString('hex')}`;
const mock = await startProc('mock-openai.mjs', {});
const cloud = await startProc('mock-cindy-cloud.mjs', { MOCK_ACCESS_TOKEN_SUFFIX: DESKTOP_SIG, MOCK_GATEWAY_ENDPOINT: `http://127.0.0.1:${mock.port}` });
const relay = await startProc('mock-device-link-relay.mjs', { MOCK_TOKENS: JSON.stringify({ [`*.${DESKTOP_SIG}`]: DESKTOP_ID, [PHONE_TOKEN]: PHONE_ID }) });
const base = `http://127.0.0.1:${cloud.port}`;

let sc;
let phone;
let failed = false;
try {
  sc = startSidecar({
    CINDY_TAURI_AUTH_BASE_URL: base,
    CINDY_TAURI_MODEL_ACCESS_BASE_URL: base,
    CINDY_TAURI_DEVICE_LINK_BASE_URL: `http://127.0.0.1:${relay.port}`,
    CINDY_TAURI_SMOKE_MODEL_BASE_URL: `http://127.0.0.1:${mock.port}`,
    CINDY_TAURI_SMOKE_MODEL_ID: 'mock-model',
  });
  await sc.call('secrets.unlock', { key: masterKey });

  // ── 未登录：不连中继 ──
  const st0 = await sc.invoke('device-link:get-state');
  check('offline while signed out', st0.linkStatus === 'stopped' && st0.remoteControlEnabled === false, st0.linkStatus);

  // ── 登录 → 上线 ──
  await sc.invoke('auth:exit-local');
  await sc.invoke('auth:get-login-state');
  await sc.invoke('auth:dispatch-login-action', { type: 'request-code', kind: 'email', identifier: 'dl@example.com', captchaToken: 'ok' });
  const login = await sc.invoke('auth:dispatch-login-action', { type: 'verify-code', kind: 'email', identifier: 'dl@example.com', code: '123456' });
  check('signed in', login.success === true);
  await waitFor(async () => (await sc.invoke('device-link:get-state')).linkStatus === 'online', 15_000, 'desktop online');
  check('desktop online on relay after login', true);
  check('status push to renderer', sc.pushes.some((p) => p.channel === 'device-link:status-changed' && p.args[0]?.status === 'online'));

  const devices = await sc.invoke('device-link:list-devices');
  const self = devices.devices.find((d) => d.isSelf);
  check('device list marks self with (Tauri) suffix', self?.deviceId === DESKTOP_ID && /\(Tauri\)$/.test(self.name), self?.name);

  // ── 手机上线 ──
  phone = new DeviceLinkClient({
    getWsUrl: () => `ws://127.0.0.1:${relay.port}/api/device-link/ws`,
    getToken: async () => PHONE_TOKEN,
    getHello: () => ({ deviceName: 'E2E Phone', platform: 'ios', appVersion: '9.9.9', remoteControlEnabled: false, busy: false }),
    createWebSocket: (url, headers) => new WebSocket(url, { headers }),
    logger: { debug() {}, info() {}, warn() {}, error() {} },
  });
  const phonePushes = [];
  phone.onFrame((env) => { if (env.kind === 'push') phonePushes.push(env.payload); });
  phone.start();
  await waitFor(() => phone.getStatus() === 'online', 10_000, 'phone online');

  // ── 被控开关默认关闭：中继拒绝 link-open ──
  let rejected = null;
  try { await phone.openLink(DESKTOP_ID, { controllerName: 'E2E Phone', protocolVersion: 1, appVersion: '9.9.9', capabilities: ['background-link-v1'] }, 5_000); }
  catch (err) { rejected = String(err?.code ?? err?.message ?? err); }
  check('link refused while remote control is off', rejected && /REMOTE_DISABLED|disabled/i.test(rejected), rejected);

  // ── 打开被控 → 接入 ──
  const on = await sc.invoke('device-link:set-enabled', true);
  check('enable remote control', on.remoteControlEnabled === true);
  await sleep(300);
  const accept = await phone.openLink(DESKTOP_ID, { controllerName: 'E2E Phone', protocolVersion: 1, appVersion: '9.9.9', capabilities: ['background-link-v1'] }, 10_000);
  check('link accepted with history-view capability', Array.isArray(accept.capabilities) && accept.capabilities.includes('history-view-v1'), accept.capabilities);
  const rinvoke = async (channel, ...args) => {
    const r = await phone.invoke(DESKTOP_ID, { channel, args }, 30_000);
    if (!r.ok) throw new Error(r.error?.message ?? r.error?.code);
    return r.result;
  };
  await rinvoke('device-link:subscribe', { topics: ['sessions'], controllerName: 'E2E Phone' });
  await waitFor(async () => (await sc.invoke('device-link:get-state')).controlledBy.some((c) => c.deviceId === PHONE_ID), 5_000, 'controlledBy');
  check('desktop shows phone as controller', true);

  // ── 手机列任务、建任务、发消息 ──
  const row = await sc.invoke('local-db:sessions:create', { agentKind: 'pi', title: 'Desktop task', workspaceKind: 'dialogue', model: 'mock-model', providerId: 'smoke', permissionMode: 'ask' });
  const list = await rinvoke('local-db:sessions:list', 20);
  check('phone lists desktop sessions', Array.isArray(list) && list.some((s) => s.id === row.id), list?.length);
  check('phone gets sessions:created push', phonePushes.some((p) => p.channel === 'local-db:sessions:created'));

  await rinvoke('device-link:subscribe', { topics: [`session:${row.id}`], controllerName: 'E2E Phone' });
  const createOpts = { agentKind: 'pi', model: 'mock-model', providerId: 'smoke', permissionMode: 'ask', workingDir: row.workingDir, workspaceKind: 'dialogue' };
  const queued = (clientId, text) => ({
    clientId, text, persistedContent: JSON.stringify({ text }), model: 'mock-model', effort: 'medium', permissionMode: 'ask',
    workingDir: row.workingDir, chatMessage: { clientId, role: 'user', content: text }, createOpts,
  });
  await rinvoke('maker:input:enqueue', row.id, queued('p1', 'hi from phone'));
  const doneOnPhone = (n) => phonePushes.filter((p) => p.channel === 'maker:event' && p.payload?.sessionId === row.id && p.payload.event?.type === 'done').length >= n;
  await waitFor(() => doneOnPhone(1), 120_000, 'turn done on phone');
  check('phone receives streamed maker:event for its turn', phonePushes.some((p) => p.channel === 'maker:event' && p.payload?.event?.type === 'text'));
  check('desktop renderer also sees the turn', sc.pushes.some((p) => p.channel === 'maker:event' && p.args[0]?.sessionId === row.id && p.args[0].event.type === 'done'));
  const view = await rinvoke('local-db:messages:view', row.id, {});
  check('phone reads history view', view?.version === 1 && view.items.length > 0, `${view?.items?.length} items`);

  // ── 远程审批 ──
  await rinvoke('maker:input:enqueue', row.id, queued('p2', 'MOCK_WRITE_FILE please'));
  const req = await waitFor(() => phonePushes.find((p) => p.channel === 'maker:interaction-request' && p.payload?.sessionId === row.id), 60_000, 'approval on phone');
  const snapshot = await rinvoke('maker:get-pending-interactions', row.id);
  check('pending interactions snapshot', Array.isArray(snapshot) && snapshot.some((e) => e.request.requestId === req.payload.request.requestId), snapshot?.length);
  const resolved = await rinvoke('maker:resolve-interaction', req.payload.request.requestId, { kind: 'permission', behavior: 'allow' });
  check('phone approves tool call', resolved?.accepted === true);
  await waitFor(() => doneOnPhone(2), 120_000, 'second turn done');
  check('approved tool ran', (await import('node:fs')).existsSync(path.join(row.workingDir, 'mock-ok.txt')));
  check('dismiss pushed to phone', phonePushes.some((p) => p.channel === 'maker:interaction-dismissed' && p.payload?.requestId === req.payload.request.requestId));

  // ── 边界 ──
  let notImpl = null;
  try { await rinvoke('maker:schedule:list'); } catch (err) { notImpl = String(err.message); }
  check('unimplemented allowlisted channel → NOT_IMPLEMENTED', /NOT_IMPLEMENTED|FORBIDDEN/.test(notImpl ?? ''), notImpl);
  let forbidden = null;
  try { await rinvoke('safe-storage-read', 'api_key'); } catch (err) { forbidden = String(err.message); }
  check('non-allowlisted channel is refused (no secret read)', /FORBIDDEN/.test(forbidden ?? ''), forbidden);

  // ── 撤销 → 拒绝 ──
  const revokedState = await sc.invoke('device-link:revoke', PHONE_ID);
  check('revoke drops the controller', revokedState.revokedControllers.includes(PHONE_ID) && !revokedState.controlledBy.some((c) => c.deviceId === PHONE_ID));
  // 撤销后链路被关：再开链被拒（desktop 同口径，只关链不回错误码），已有链路上的 invoke 拿不到结果。
  let reopen = null;
  try { await phone.openLink(DESKTOP_ID, { controllerName: 'E2E Phone', protocolVersion: 1, appVersion: '9.9.9', capabilities: ['background-link-v1'] }, 5_000); reopen = 'accepted'; }
  catch (err) { reopen = String(err?.code ?? err?.message ?? err); }
  check('revoked controller cannot reopen link', reopen !== 'accepted', reopen);
  let revokedErr = null;
  try { const r = await phone.invoke(DESKTOP_ID, { channel: 'local-db:sessions:list', args: [5] }, 5_000); revokedErr = r.ok ? 'served' : r.error?.code; }
  catch (err) { revokedErr = String(err?.code ?? err?.message ?? err); }
  check('revoked controller gets no data', revokedErr !== 'served', revokedErr);
  await sc.invoke('device-link:restore', PHONE_ID);

  // ── 退出 → 下线 ──
  await sc.invoke('auth:logout');
  await waitFor(async () => (await sc.invoke('device-link:get-state')).linkStatus === 'stopped', 10_000, 'desktop offline after logout');
  check('desktop goes offline after logout', true);
  const logs = [...sc.errLines, ...relay.logLines, ...cloud.logLines].join('\n');
  check('logs never contain tokens', !logs.includes(PHONE_TOKEN) && !logs.includes(DESKTOP_SIG));
} catch (err) {
  failed = true;
  console.error(`ERROR ${err?.stack ?? err}`);
  if (sc) console.error(sc.errLines.filter((l) => /device-link|ipc|maker-ipc/.test(l)).slice(-30).join('\n'));
} finally {
  try { phone?.stop(); } catch { /* 已停 */ }
  if (sc) await Promise.race([sc.stop(), sleep(5000)]);
  for (const p of [mock, cloud, relay]) p.child.kill();
}
console.log(JSON.stringify({ ok: !failed && steps.every((s) => s.ok), dataDir, steps: steps.length }));
process.exit(failed ? 1 : 0);
