// 「手机」驱动：用 cindy 上游 DeviceLinkClient 当控制端，经 stdin/stdout 的 JSON 行被 cargo e2e 遥控。
// 用法：node device-link-phone.mjs <relayPort> <token>；命令 {id, op, ...}，应答 {id, result|error}。
import path from 'node:path';
import os from 'node:os';
import readline from 'node:readline';
import { mkdtempSync } from 'node:fs';
import { fileURLToPath, pathToFileURL } from 'node:url';
import { build } from 'esbuild';

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const cindyDir = path.resolve(root, process.env.CINDY_DIR ?? '../cindy');
const [relayPort, token] = process.argv.slice(2);
const tmp = mkdtempSync(path.join(os.tmpdir(), 'dl-phone-'));
const bundle = path.join(tmp, 'phone-client.mjs');
await build({
  stdin: { contents: "export { DeviceLinkClient } from '@cindy/device-link';", resolveDir: root, loader: 'js' },
  bundle: true, platform: 'node', format: 'esm', outfile: bundle, logLevel: 'error',
  nodePaths: [path.join(cindyDir, 'apps', 'desktop', 'node_modules'), path.join(cindyDir, 'node_modules')],
});
const { DeviceLinkClient } = await import(pathToFileURL(bundle).href);
const { default: WebSocket } = await import(pathToFileURL(path.join(cindyDir, 'node_modules', 'ws', 'wrapper.mjs')).href);

const pushes = [];
const phone = new DeviceLinkClient({
  getWsUrl: () => `ws://127.0.0.1:${relayPort}/api/device-link/ws`,
  getToken: async () => token,
  getHello: () => ({ deviceName: 'E2E Phone', platform: 'ios', appVersion: '9.9.9', remoteControlEnabled: false, busy: false }),
  createWebSocket: (url, headers) => new WebSocket(url, { headers }),
  logger: { debug() {}, info() {}, warn() {}, error() {} },
});
phone.onFrame((env) => { if (env.kind === 'push') pushes.push(env.payload); });

const caps = ['background-link-v1', 'reliable-transport-v1', 'reliable-link-confirm-v1', 'transport-timeout-close-v1', 'transport-ack-v2'];
const ops = {
  start: async () => { phone.start(); return true; },
  status: async () => phone.getStatus(),
  open: async ({ dst, capabilities }) => {
    try { const a = await phone.openLink(dst, { controllerName: 'E2E Phone', protocolVersion: 1, appVersion: '9.9.9', capabilities: capabilities ?? caps }, 10_000); return { ok: true, accept: a }; }
    catch (err) { return { ok: false, error: String(err?.code ?? err?.message ?? err) }; }
  },
  invoke: async ({ dst, channel, args, timeoutMs }) => {
    try { return await phone.invoke(dst, { channel, args: args ?? [] }, timeoutMs ?? 20_000); }
    catch (err) { return { ok: false, error: { code: String(err?.code ?? 'ERR'), message: String(err?.message ?? err) } }; }
  },
  pushes: async () => pushes,
  stop: async () => { phone.stop(); return true; },
};
const rl = readline.createInterface({ input: process.stdin });
rl.on('line', async (line) => {
  let cmd;
  try { cmd = JSON.parse(line); } catch { return; }
  try {
    const result = await ops[cmd.op](cmd);
    process.stdout.write(JSON.stringify({ id: cmd.id, result }) + '\n');
  } catch (err) {
    process.stdout.write(JSON.stringify({ id: cmd.id, error: String(err?.message ?? err) }) + '\n');
  }
});
process.stdout.write(JSON.stringify({ ready: true }) + '\n');
rl.on('close', () => { try { phone.stop(); } catch {} process.exit(0); });
