// sidecar 级登录 / 云端模型测试（不起 Tauri）：本脚本扮演 Rust（回 open.url 反向调用），起两个 mock：
// - mock-cindy-cloud.mjs：auth-server + model-access（credentials / models / balance / credit-usage）；
// - mock-openai.mjs：Cindy AI 网关（/keyed 下要求真实网关 key）。
// 覆盖：邮箱验证码登录（带 captcha）、owner 代际、safe-storage 读不到登录凭据但读得到网关 key、
// 云端模型进目录、Pi 会话经本地转发走网关（上游只见真实 key，没有内部头 / 占位 key）、余额、
// 退出（网关 key 删除、xd 消失）、浏览器登录（托管回调轮询）、重启后冷启动续期、
// 续期被判定失效 → auth:session-expired、本机模式进出。
// 用法：node scripts/auth-e2e.mjs   （需要先 npm run sidecar:build；Pi 未装时会自动下载）
import { spawn } from 'node:child_process';
import { randomBytes } from 'node:crypto';
import { mkdtempSync, readFileSync } from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import readline from 'node:readline';
import { fileURLToPath } from 'node:url';
import { decode, encode } from '../shared/ipc-codec.mjs';

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const sidecarJs = path.join(root, 'sidecar', 'dist', 'sidecar.cjs');
const node = process.env.CINDY_E2E_NODE || process.execPath;
const dataDir = process.env.CINDY_E2E_DATA_DIR || mkdtempSync(path.join(os.tmpdir(), 'cindy-auth-e2e-'));
const masterKey = randomBytes(32).toString('hex');
const gatewayKey = `gw-${randomBytes(12).toString('hex')}`;
const GW_MODEL = 'mock-gw-model';

function startProc(script, env) {
  const child = spawn(process.execPath, [path.join(root, 'scripts', script)], { stdio: ['ignore', 'pipe', 'pipe'], env: { ...process.env, ...env } });
  const logLines = [];
  readline.createInterface({ input: child.stderr }).on('line', (l) => { logLines.push(l); if (process.env.E2E_VERBOSE) console.error('  [mock]', l); });
  return new Promise((resolve) => {
    readline.createInterface({ input: child.stdout }).once('line', (l) => resolve({ child, port: JSON.parse(l).port, logLines }));
  });
}

let lastAuthorizeUrl = null;
function startSidecar(cloudPort) {
  const child = spawn(node, [sidecarJs], {
    cwd: path.dirname(sidecarJs),
    env: {
      ...process.env,
      CINDY_TAURI_DATA_DIR: dataDir,
      CINDY_TAURI_AUTH_BASE_URL: `http://127.0.0.1:${cloudPort}`,
      CINDY_TAURI_MODEL_ACCESS_BASE_URL: `http://127.0.0.1:${cloudPort}`,
    },
    stdio: ['pipe', 'pipe', 'pipe'],
  });
  let seq = 0;
  const waiters = new Map();
  const pushes = [];
  const natives = [];
  const errLines = [];
  readline.createInterface({ input: child.stderr }).on('line', (l) => { errLines.push(l); if (process.env.E2E_VERBOSE) console.error('  [sc]', l); });
  readline.createInterface({ input: child.stdout }).on('line', (line) => {
    const msg = JSON.parse(line);
    if (msg.push) { pushes.push({ channel: msg.push.channel, args: decode(msg.push.payload) }); return; }
    if (msg.native) {
      natives.push(msg.native);
      const { id, method, params } = msg.native;
      if (id === undefined) return;
      if (method === 'open.url') lastAuthorizeUrl = params.url;
      const ok = method === 'open.url' || method === 'tray.setLabels';
      child.stdin.write(JSON.stringify({ nativeReply: ok ? { id, result: null } : { id, error: `[NOT_IMPLEMENTED] ${method}` } }) + '\n');
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
  const sendSync = async (channel, ...a) => decode(await call('ipc.sendSync', { channel, payload: encode(a) }));
  const stop = () => new Promise((resolve) => { child.once('exit', resolve); child.stdin.end(); });
  return { call, invoke, sendSync, pushes, natives, stop, errLines };
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
const parseLog = (lines, prefix) => lines.map((l) => { try { return JSON.parse(l.replace(prefix, '')); } catch { return null; } }).filter(Boolean);
const cloudState = async (port) => (await fetch(`http://127.0.0.1:${port}/__mock/state`)).json();
const mockPost = (port, p, body) => fetch(`http://127.0.0.1:${port}${p}`, { method: 'POST', headers: { 'content-type': 'application/json' }, body: JSON.stringify(body) });
const xdProvider = async (sc) => (await sc.invoke('maker:provider:list', {}))?.providers?.find((p) => p.id === 'xd' || p.providerId === 'xd');

const gateway = await startProc('mock-openai.mjs', { MOCK_API_KEY: gatewayKey, MOCK_MODEL_ID: GW_MODEL, MOCK_LOG_HEADER_NAMES: '1' });
const cloud = await startProc('mock-cindy-cloud.mjs', { MOCK_GATEWAY_ENDPOINT: `http://127.0.0.1:${gateway.port}/keyed`, MOCK_GATEWAY_KEY: gatewayKey });
let sc;
let failed = false;
try {
  sc = startSidecar(cloud.port);
  await sc.call('secrets.unlock', { key: masterKey });

  // ── 初始：本机模式 ──
  const s0 = await sc.invoke('auth:initialize');
  check('fresh data dir starts in local mode', s0.mode === 'local' && s0.dataOwnerId === 'local-v1' && !s0.isAuthenticated, { mode: s0.mode, gen: s0.ownerGeneration });
  check('no persisted session hint', (await sc.sendSync('auth:has-persisted-session-hint-sync')) === false);
  check('no xd provider while local', !(await xdProvider(sc)));

  // ── 退出本机模式 → 登录页 ──
  const s1 = await sc.invoke('auth:exit-local');
  check('exit-local → signed-out with null owner', s1.mode === 'signed-out' && s1.dataOwnerId === null && s1.ownerGeneration === s0.ownerGeneration + 1, { gen: s1.ownerGeneration });
  const lp = await sc.invoke('auth:get-login-state');
  check('login providers loaded from mock', lp?.step === 'identifier' || lp?.state?.step === 'identifier' || lp?.providers?.email === true, lp);
  const captchaUrl = await sc.invoke('auth:get-captcha-challenge-url');
  check('captcha url points at auth base', captchaUrl === `http://127.0.0.1:${cloud.port}/captcha/turnstile`, captchaUrl);

  // ── 邮箱验证码登录 ──
  const email = 'e2e@example.com';
  const d = await sc.invoke('auth:dispatch-login-action', { type: 'discover', email });
  check('discover email', d.success === true, d);
  const noCaptcha = await sc.invoke('auth:dispatch-login-action', { type: 'request-code', kind: 'email', identifier: email });
  check('request-code without captcha is rejected', noCaptcha.success === false && noCaptcha.code === 'CAPTCHA_REQUIRED', { code: noCaptcha.code });
  const rc = await sc.invoke('auth:dispatch-login-action', { type: 'request-code', kind: 'email', identifier: email, captchaToken: 'turnstile-ok' });
  check('request-code with captcha', rc.success === true && rc.state?.step === 'verification-code', { step: rc.state?.step });
  const bad = await sc.invoke('auth:dispatch-login-action', { type: 'verify-code', kind: 'email', identifier: email, code: '000000' });
  check('wrong code rejected', bad.success === false && bad.code === 'INVALID_CODE', { code: bad.code });
  const vc = await sc.invoke('auth:dispatch-login-action', { type: 'verify-code', kind: 'email', identifier: email, code: '123456' });
  check('verify-code completes login', vc.success === true && vc.state?.step === 'completed', { step: vc.state?.step });
  const s2 = await sc.invoke('auth:initialize');
  check('now cloud mode with user', s2.mode === 'cloud' && s2.isAuthenticated && s2.user?.email === email && s2.dataOwnerId === 'local-v1'
    && s2.ownerGeneration === s1.ownerGeneration + 1, { mode: s2.mode, gen: s2.ownerGeneration });
  check('auth:state-change pushed', sc.pushes.some((p) => p.channel === 'auth:state-change' && p.args[0]?.mode === 'cloud'));
  check('persisted session hint', (await sc.sendSync('auth:has-persisted-session-hint-sync')) === true);
  const appSession = JSON.parse(readFileSync(path.join(dataDir, 'app-session.json'), 'utf8'));
  check('app-session.json has no tokens', appSession.mode === 'cloud' && !/rt-|accessToken|refreshToken/.test(JSON.stringify(appSession)));

  // ── 登录凭据对 renderer 不可见 ──
  check('safe-storage cannot read login session', (await sc.invoke('safe-storage-read', 'cindy_auth_session_v1')) === null);
  check('safe-storage cannot overwrite login session', (await sc.invoke('safe-storage-store', 'cindy_auth_session_v1', 'x')) === false);
  const secretsFile = readFileSync(path.join(dataDir, 'secrets.v1.json'), 'utf8');
  check('secrets file holds no plaintext refresh token', !secretsFile.includes('rt-') && !secretsFile.includes(gatewayKey));

  // ── 云端模型 ──
  const status = await waitFor(async () => {
    const st = await sc.invoke('model-access:get-status');
    // accountTier 跟着模型清单同步回来，比凭据晚一步。
    return st?.state === 'ok' && st.accountTier ? st : null;
  }, 15_000, 'model-access ok');
  check('model-access credentials synced', status.state === 'ok' && status.accountTier === 'paid', { state: status.state, tier: status.accountTier });
  check('gateway key readable by renderer (desktop parity)', (await sc.invoke('safe-storage-read', 'api_key')) === gatewayKey);
  const xd = await waitFor(async () => {
    const p = await xdProvider(sc);
    const models = JSON.stringify(p ?? {});
    return p && models.includes(GW_MODEL) ? p : null;
  }, 15_000, 'xd provider with gateway model');
  check('xd provider listed with gateway model', !!xd);
  const providers = (await sc.invoke('maker:provider:list', {})).providers;
  check('xd is first in provider list', (providers[0]?.id ?? providers[0]?.providerId) === 'xd', providers.map((p) => p.id ?? p.providerId));
  await sc.invoke('maker:provider:models-refresh', 'xd');
  check('models refresh hits mock', (await cloudState(cloud.port)).models >= 2);
  const bal = await sc.invoke('billing:get-balance');
  check('balance projected', bal?.available === '4' && bal?.scale === 9, bal);
  const usage = await sc.invoke('billing:get-credit-usage');
  check('credit usage projected', usage?.available === '4' && usage?.plan?.total === '2', { available: usage?.available });

  // ── Pi 会话经本地转发走网关 ──
  const row = await sc.invoke('local-db:sessions:create', { agentKind: 'pi', title: 'Gateway E2E', workspaceKind: 'dialogue', model: GW_MODEL, providerId: 'xd', permissionMode: 'ask' });
  const createOpts = { agentKind: 'pi', model: GW_MODEL, providerId: 'xd', permissionMode: 'ask', workingDir: row.workingDir, workspaceKind: 'dialogue' };
  const gwMark = gateway.logLines.length;
  await sc.invoke('maker:input:enqueue', row.id, {
    clientId: 'g1', text: 'hi gateway', persistedContent: JSON.stringify({ text: 'hi gateway' }), model: GW_MODEL, effort: 'medium', permissionMode: 'ask',
    workingDir: row.workingDir, chatMessage: { clientId: 'g1', role: 'user', content: 'hi gateway' }, createOpts,
  });
  const term = await waitFor(() => sc.pushes.find((p) => p.channel === 'maker:event' && p.args[0]?.sessionId === row.id
    && ['done', 'error'].includes(p.args[0].event.type)), 120_000, 'gateway turn');
  const ev = term.args[0].event;
  const chats = parseLog(gateway.logLines.slice(gwMark), /^\[mock-openai\] /).filter((e) => e.path?.endsWith('/chat/completions'));
  check('gateway turn done', ev.type === 'done', ev.type === 'error' ? ev : undefined);
  check('gateway got the real key', chats.length > 0 && chats.every((e) => e.auth === 'ok') && chats.every((e) => e.model === GW_MODEL), chats.map((e) => ({ auth: e.auth, model: e.model })));
  const leaked = chats.flatMap((e) => e.headers ?? []).filter((h) => h.startsWith('x-cindy-pi-'));
  check('no internal Pi headers reach the gateway', leaked.length === 0, leaked);
  // Pi 的 models.json 里 cindy 块指向本地转发；不带已登记会话 token 的请求必须被拒。
  const modelsJsonLine = sc.errLines.find((l) => l.includes('pi writeModelsJson'));
  const modelsJson = JSON.parse(readFileSync(JSON.parse(modelsJsonLine).modelsJsonPath, 'utf8'));
  const cindyBlock = modelsJson.providers?.cindy;
  check('Pi models.json has no real gateway key', !JSON.stringify(modelsJson).includes(gatewayKey) && /^http:\/\/127\.0\.0\.1:\d+/.test(cindyBlock?.baseUrl ?? ''), cindyBlock?.baseUrl);
  const fwdBase = cindyBlock.baseUrl.replace(/\/+$/, '');
  const gwBefore = gateway.logLines.length;
  const anon = await fetch(`${fwdBase}/chat/completions`, { method: 'POST', headers: { 'content-type': 'application/json' }, body: '{}' });
  const forged = await fetch(`${fwdBase}/chat/completions`, { method: 'POST', headers: { 'content-type': 'application/json', 'x-cindy-pi-session-id': row.id, 'x-cindy-pi-session-token': 'forged' }, body: '{}' });
  check('forwarder rejects requests without a registered session token', anon.status === 403 && forged.status === 403 && gateway.logLines.length === gwBefore, [anon.status, forged.status]);

  // ── 退出 ──
  await sc.invoke('auth:logout');
  const out = await sc.invoke('auth:initialize');
  check('logout → signed-out', out.mode === 'signed-out' && out.dataOwnerId === null && out.ownerGeneration === s2.ownerGeneration + 1, { mode: out.mode });
  const st1 = await cloudState(cloud.port);
  check('server logout called', st1.logout === 1 && st1.liveRefreshTokens === 0, st1);
  await waitFor(async () => (await sc.invoke('safe-storage-read', 'api_key')) === null, 5_000, 'gateway key removed');
  check('gateway key removed on logout', true);
  check('xd provider gone after logout', !(await xdProvider(sc)));
  check('model-access status idle after logout', (await sc.invoke('model-access:get-status'))?.state !== 'ok');
  let billingDenied = false;
  try { await sc.invoke('billing:get-balance'); } catch (err) { billingDenied = /PERMISSION_DENIED/.test(String(err.message)); }
  check('balance denied when signed out', billingDenied);

  // ── 浏览器登录（托管回调轮询）──
  lastAuthorizeUrl = null;
  await sc.invoke('auth:get-login-state');
  const browserRun = sc.invoke('auth:dispatch-login-action', { type: 'start-browser', kind: 'social', providerOrConnectionId: 'google', label: 'Google' });
  await waitFor(() => lastAuthorizeUrl, 10_000, 'open.url');
  const authorizeUrl = new URL(lastAuthorizeUrl);
  check('open.url called with authorize url', authorizeUrl.pathname === '/api/auth/social/google/authorize'
    && authorizeUrl.searchParams.get('redirect_uri') === `http://127.0.0.1:${cloud.port}/api/auth/desktop/callback`, authorizeUrl.pathname);
  // 模拟浏览器打开授权页再同意。
  await fetch(lastAuthorizeUrl);
  await mockPost(cloud.port, '/__mock/authorize', { clientState: authorizeUrl.searchParams.get('client_state') });
  const br = await browserRun;
  check('browser login completes', br.success === true && br.state?.step === 'completed', { code: br.code, step: br.state?.step });
  const s3 = await sc.invoke('auth:initialize');
  check('cloud mode after browser login', s3.mode === 'cloud' && s3.user?.email === 'browser@example.com');
  await waitFor(async () => (await sc.invoke('model-access:get-status'))?.state === 'ok', 15_000, 'model-access ok again');
  check('gateway key fetched again after re-login', (await sc.invoke('safe-storage-read', 'api_key')) === gatewayKey);

  // ── 重启：冷启动续期（refresh token 轮换）──
  const refreshBefore = (await cloudState(cloud.port)).refresh;
  await sc.stop();
  sc = startSidecar(cloud.port);
  await sc.call('secrets.unlock', { key: masterKey });
  const s4 = await sc.invoke('auth:initialize');
  check('cold start refresh restores cloud session', s4.mode === 'cloud' && s4.isAuthenticated && s4.user?.email === 'browser@example.com', { mode: s4.mode, auth: s4.isAuthenticated });
  check('refresh endpoint called once on cold start', (await cloudState(cloud.port)).refresh === refreshBefore + 1);
  const r2 = await sc.invoke('auth:refresh');
  check('runtime refresh succeeds with rotated token', r2 === true && (await cloudState(cloud.port)).liveRefreshTokens === 1);

  // ── 续期被判定失效 ──
  await mockPost(cloud.port, '/__mock/refresh-fail', { code: 'INVALID_REFRESH_TOKEN' });
  const r3 = await sc.invoke('auth:refresh');
  check('definitive refresh failure returns false', r3 === false);
  const expired = await waitFor(() => sc.pushes.find((p) => p.channel === 'auth:session-expired'), 5_000, 'session-expired');
  check('auth:session-expired pushed with reason', expired.args[0]?.reason === 'replaced-elsewhere', expired.args[0]);
  const s5 = await sc.invoke('auth:initialize');
  check('signed-out after expiry', s5.mode === 'signed-out');
  await waitFor(async () => (await sc.invoke('safe-storage-read', 'api_key')) === null, 5_000, 'gateway key removed after expiry');
  check('gateway key removed after expiry', true);

  // ── 本机模式 ──
  const s6 = await sc.invoke('auth:enter-local');
  check('enter-local from signed-out', s6.mode === 'local' && s6.dataOwnerId === 'local-v1' && s6.ownerGeneration === s5.ownerGeneration + 1);
  let logoutRejected = false;
  try { await sc.invoke('auth:logout'); } catch (err) { logoutRejected = /UNAUTHENTICATED/.test(String(err.message)); }
  check('logout rejected in local mode', logoutRejected);
  // 本机模式侧栏菜单的「登录」走 /add-account 页：begin-add 要给出登录态，且在本机模式下能直接登录到云端。
  const ba = await sc.invoke('auth:accounts:begin-add');
  check('begin-add from local returns login state', ba?.success === true && ba.state?.step === 'identifier', { step: ba?.state?.step });
  check('begin-add keeps local mode until login completes', (await sc.invoke('auth:initialize')).mode === 'local');
  await sc.invoke('auth:accounts:cancel-add');
  await sc.invoke('auth:accounts:begin-add');
  await sc.invoke('auth:dispatch-login-action', { type: 'request-code', kind: 'email', identifier: email, captchaToken: 'turnstile-ok' });
  const addLogin = await sc.invoke('auth:dispatch-login-action', { type: 'verify-code', kind: 'email', identifier: email, code: '123456' });
  const s7 = await sc.invoke('auth:initialize');
  check('login via add-account page from local → cloud', addLogin.success === true && s7.mode === 'cloud' && s7.isAuthenticated, { mode: s7.mode });
  await sc.invoke('auth:logout');
  await sc.invoke('auth:enter-local');
  check('multi-account stub returns empty list', JSON.stringify(await sc.invoke('auth:accounts:list')) === '{"accounts":[],"mutationAllowed":false}');

  // ── 日志不含凭据 ──
  const allLogs = [...sc.errLines, ...cloud.logLines, ...gateway.logLines].join('\n');
  check('logs never contain the gateway key or tokens', !allLogs.includes(gatewayKey) && !/rt-[0-9a-f]{32}/.test(allLogs) && !/ac-[0-9a-f]{16}/.test(allLogs));
} catch (err) {
  failed = true;
  console.error(`ERROR ${err?.stack ?? err}`);
  if (sc) console.error(sc.errLines.slice(-40).join('\n'));
} finally {
  if (sc) await Promise.race([sc.stop(), sleep(5000)]);
  gateway.child.kill();
  cloud.child.kill();
}
console.log(JSON.stringify({ ok: !failed && steps.every((s) => s.ok), dataDir, steps: steps.length }));
process.exit(failed ? 1 : 0);
