// 自建服务端 e2e：起真实的 cindy-server，用上游 DeviceLinkClient 当两台设备，走完 登录 → 建链 → 互发消息 → 登出。
// 用法：node scripts/server-e2e.mjs    （需要先 cargo build --manifest-path server/Cargo.toml）
import { spawn } from 'node:child_process';
import path from 'node:path';
import os from 'node:os';
import { mkdtempSync, existsSync } from 'node:fs';
import { fileURLToPath, pathToFileURL } from 'node:url';
import { build } from 'esbuild';

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const cindyDir = path.join(root, 'frontend');
const bin = path.join(root, 'server', 'target', 'debug', 'cindy-server');
if (!existsSync(bin)) { console.error(`先构建：cargo build --manifest-path server/Cargo.toml（${bin} 不存在）`); process.exit(2); }

const tmp = mkdtempSync(path.join(os.tmpdir(), 'server-e2e-'));
const port = 20000 + Math.floor(Math.random() * 20000);
const base = `http://127.0.0.1:${port}`;
let code = null;
const server = spawn(bin, [], {
  env: { ...process.env, CINDY_SERVER_ADDR: `127.0.0.1:${port}`, CINDY_SERVER_DB: path.join(tmp, 'e2e.db'), CINDY_SERVER_ALLOWED_EMAILS: 'me@example.com' },
  stdio: ['ignore', 'ignore', 'pipe'],
});
server.stderr.on('data', (d) => { const m = /登录验证码 \S+: (\d{6})/.exec(String(d)); if (m) code = m[1]; });
const stop = () => { try { server.kill(); } catch {} };
process.on('exit', stop);
const fail = (msg) => { console.error(`FAIL: ${msg}`); stop(); process.exit(1); };
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const waitFor = async (fn, label, ms = 5000) => { const t = Date.now(); while (Date.now() - t < ms) { const v = await fn(); if (v) return v; await sleep(50); } fail(`超时：${label}`); };
const post = async (p, body, token) => {
  const r = await fetch(base + p, { method: 'POST', headers: { 'content-type': 'application/json', ...(token ? { authorization: `Bearer ${token}` } : {}) }, body: JSON.stringify(body) });
  return { status: r.status, body: await r.json().catch(() => null) };
};

await waitFor(async () => { try { return (await fetch(`${base}/api/auth/providers`)).ok; } catch { return false; } }, '服务端启动');

// 登录：验证码只出现在服务端日志里
async function login(deviceId) {
  code = null;
  const r = await post('/api/auth/email/request-code', { email: 'me@example.com' });
  if (r.status !== 200) fail(`request-code ${r.status}`);
  await waitFor(() => code, '日志里的验证码');
  const v = await post('/api/auth/email/verify-code', { email: 'me@example.com', code, deviceId, clientType: 'desktop' });
  if (v.status !== 200) fail(`verify-code ${v.status} ${JSON.stringify(v.body)}`);
  return v.body;
}
const a = await login('e2e-desk');
await sleep(31_000); // 同一邮箱 30 秒内不能再请求验证码
const b = await login('e2e-phone');

await build({
  stdin: { contents: "export { DeviceLinkClient } from '@cindy/device-link';", resolveDir: root, loader: 'js' },
  bundle: true, platform: 'node', format: 'esm', outfile: path.join(tmp, 'client.mjs'), logLevel: 'error',
  nodePaths: [path.join(cindyDir, 'apps', 'desktop', 'node_modules'), path.join(cindyDir, 'node_modules')],
});
const { DeviceLinkClient } = await import(pathToFileURL(path.join(tmp, 'client.mjs')).href);
const { default: WebSocket } = await import(pathToFileURL(path.join(cindyDir, 'node_modules', 'ws', 'wrapper.mjs')).href);
const quiet = { debug() {}, info() {}, warn() {}, error() {} };
const mk = (name, token, remote) => new DeviceLinkClient({
  getWsUrl: () => `ws://127.0.0.1:${port}/api/device-link/ws`,
  getToken: async () => token,
  getHello: () => ({ deviceName: name, platform: 'test', appVersion: '1.0.0', remoteControlEnabled: remote, busy: false }),
  createWebSocket: (url, headers) => new WebSocket(url, { headers }),
  logger: quiet,
});
const desk = mk('桌面', a.accessToken, true);
const phone = mk('手机', b.accessToken, false);
const got = [];
phone.onFrame((env) => { if (env.kind === 'push') got.push(env.payload); });
desk.start(); phone.start();
await waitFor(() => desk.getStatus() === 'online', '桌面连上中继');
await waitFor(() => phone.getStatus() === 'online', '手机连上中继');

const list = await (await fetch(`${base}/api/device-link/devices`, { headers: { authorization: `Bearer ${b.accessToken}` } })).json();
const ids = list.devices.map((d) => d.deviceId).sort();
if (JSON.stringify(ids) !== JSON.stringify(['e2e-desk', 'e2e-phone'])) fail(`设备列表 ${ids}`);
console.log('ok 登录 + 两台设备在线，设备列表正确');

// 两台设备经中继互发 push（消息同步走的路径）。建链需要桌面端宿主应答 link-accept，由 cargo e2e 覆盖。
const deskGot = [];
desk.onFrame((env) => { if (env.kind === 'push') deskGot.push(env.payload); });
desk.sendPush('e2e-phone', 'e2e:msg', { text: '桌面→手机' });
phone.sendPush('e2e-desk', 'e2e:msg', { text: '手机→桌面' });
await waitFor(() => got.length >= 1 && deskGot.length >= 1, '两个方向的 push 都到达');
const text = (x) => JSON.stringify(x);
if (!text(got).includes('桌面→手机') || !text(deskGot).includes('手机→桌面')) fail(`push 内容不对 ${text(got)} ${text(deskGot)}`);
console.log('ok 两个方向的消息经中继送达');

// 登出后令牌失效，连接建不起来
const out = await post('/api/auth/logout', {}, b.accessToken);
if (out.status !== 200) fail(`logout ${out.status}`);
const again = await post('/api/auth/refresh', { refreshToken: b.refreshToken });
if (again.status !== 401) fail(`登出后 refresh 仍可用 ${again.status}`);
console.log('ok 登出后刷新令牌失效');
desk.stop(); phone.stop(); stop();
console.log('PASS');
process.exit(0);
