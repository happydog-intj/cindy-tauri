// 最小 device-link 中继 mock（给 device-link e2e 用），线协议同 cindy packages/device-link-relay/src/hub.ts：
// - ws 路径 /api/device-link/ws，Authorization: Bearer <token>；token → deviceId 由 MOCK_TOKENS（JSON）给定。
// - 首帧必须是 hello，回 hello-ack（deviceId 来自 token，不信客户端自报），并互相广播 presence-changed。
// - 路由帧覆写 src 原样转发；目标不在线回 DEVICE_OFFLINE；控制类帧（link-open / invoke）目标关了被控回 REMOTE_DISABLED。
// - REST GET /api/device-link/devices：返回该 token 账号下的设备视图。
// 启动后 stdout 打印 {"port":N}；日志只记帧类型与设备 id 前缀，不记 token 和 payload。
import { createServer } from 'node:http';
import path from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';

// 本仓库不直接依赖 ws：从 cindy 克隆（CINDY_DIR，默认 ../cindy）的 node_modules 加载。
const here = path.dirname(fileURLToPath(import.meta.url));
const cindyDir = path.resolve(here, '..', process.env.CINDY_DIR ?? '../cindy');
const { WebSocketServer } = await import(pathToFileURL(path.join(cindyDir, 'node_modules', 'ws', 'wrapper.mjs')).href);

const PROTOCOL_VERSION = 1;
const ROUTED = new Set(['link-open', 'link-accept', 'link-close', 'invoke', 'invoke-result', 'push', 'transport-ack', 'transport-reset']);
const CONTROL = new Set(['link-open', 'invoke']);
const tokens = JSON.parse(process.env.MOCK_TOKENS || '{}'); // token → deviceId
// JWT 形的 access token 按最后一段（签名段）匹配：exp 每次签发都变，签名段由测试固定。
const lookup = (token) => tokens[token] ?? (token ? tokens[`*.${token.split('.').pop()}`] : undefined);
const log = (o) => process.stderr.write(`[mock-relay] ${JSON.stringify(o)}\n`);
const short = (id) => String(id ?? '').slice(0, 8);

const devices = new Map(); // deviceId → { name, platform, appVersion, lastSeenAt }
const active = new Map(); // deviceId → { ws, remoteControlEnabled, busy }

function tokenOf(req) {
  const h = req.headers.authorization;
  return typeof h === 'string' && h.startsWith('Bearer ') ? h.slice(7) : null;
}
function presenceOf(deviceId) {
  const d = devices.get(deviceId) ?? {};
  const c = active.get(deviceId);
  return {
    deviceId, online: Boolean(c), deviceName: d.name ?? '', selfName: d.name ?? '', deviceInfo: null,
    platform: d.platform ?? '', appVersion: d.appVersion ?? '', lastSeenAt: c ? Date.now() : d.lastSeenAt ?? 0,
    remoteControlEnabled: c?.remoteControlEnabled ?? false, busy: c?.busy ?? false,
  };
}
const send = (ws, env) => { try { ws.send(JSON.stringify(env)); return true; } catch { return false; } };
function broadcast(snap) {
  for (const [id, c] of active) if (id !== snap.deviceId) send(c.ws, { v: PROTOCOL_VERSION, kind: 'presence-changed', payload: snap });
}
function relayError(ws, id, code, message, dst) {
  send(ws, { v: PROTOCOL_VERSION, kind: 'relay-error', ...(id ? { id } : {}), payload: { code, message, ...(dst ? { dst } : {}) } });
}

const server = createServer((req, res) => {
  const url = new URL(req.url ?? '/', 'http://127.0.0.1');
  const deviceId = lookup(tokenOf(req));
  if (req.method === 'GET' && url.pathname === '/api/device-link/devices') {
    if (!deviceId) { res.writeHead(401, { 'content-type': 'application/json' }); res.end('{"error":{"code":"UNAUTHORIZED"}}'); return; }
    const list = [...devices.keys()].map((id) => {
      const p = presenceOf(id);
      return {
        deviceId: id, name: p.deviceName, selfName: p.selfName, deviceInfo: null, platform: p.platform, appVersion: p.appVersion,
        lastSeenAt: new Date(p.lastSeenAt || Date.now()).toISOString(), online: p.online, busy: p.busy,
        remoteControlEnabled: p.remoteControlEnabled, isSelf: id === deviceId,
      };
    });
    res.writeHead(200, { 'content-type': 'application/json' });
    res.end(JSON.stringify({ devices: list }));
    return;
  }
  res.writeHead(404); res.end();
});

const wss = new WebSocketServer({ noServer: true });
server.on('upgrade', (req, socket, head) => {
  const url = new URL(req.url ?? '/', 'http://127.0.0.1');
  const deviceId = lookup(tokenOf(req));
  if (url.pathname !== '/api/device-link/ws' || !deviceId) {
    log({ upgrade: 'rejected', path: url.pathname, known: Boolean(deviceId) });
    socket.write('HTTP/1.1 401 Unauthorized\r\n\r\n'); socket.destroy(); return;
  }
  wss.handleUpgrade(req, socket, head, (ws) => {
    let ready = false;
    ws.on('message', (raw) => {
      let env;
      try { env = JSON.parse(String(raw)); } catch { return; }
      if (!ready) {
        if (env.kind !== 'hello') { ws.close(4401, 'hello required'); return; }
        const h = env.payload ?? {};
        const prev = active.get(deviceId);
        if (prev && prev.ws !== ws) prev.ws.close(4409, 'replaced by a newer connection');
        devices.set(deviceId, { name: h.deviceName, platform: h.platform, appVersion: h.appVersion, lastSeenAt: Date.now() });
        active.set(deviceId, { ws, remoteControlEnabled: h.remoteControlEnabled === true, busy: h.busy === true });
        ready = true;
        log({ hello: short(deviceId), remoteControlEnabled: h.remoteControlEnabled === true });
        send(ws, { v: PROTOCOL_VERSION, kind: 'hello-ack', payload: { serverProtocolVersion: PROTOCOL_VERSION, deviceId, userId: 'u-e2e', capabilities: [] } });
        for (const id of active.keys()) if (id !== deviceId) send(ws, { v: PROTOCOL_VERSION, kind: 'presence-changed', payload: presenceOf(id) });
        broadcast(presenceOf(deviceId));
        return;
      }
      const conn = active.get(deviceId);
      if (env.kind === 'ping') { send(ws, { v: PROTOCOL_VERSION, kind: 'pong', ...(env.id ? { id: env.id } : {}) }); return; }
      if (env.kind === 'presence-set') {
        const p = env.payload ?? {};
        if (conn && typeof p.remoteControlEnabled === 'boolean') conn.remoteControlEnabled = p.remoteControlEnabled;
        if (conn && typeof p.busy === 'boolean') conn.busy = p.busy;
        broadcast(presenceOf(deviceId));
        return;
      }
      if (!ROUTED.has(env.kind)) return;
      const target = active.get(env.dst);
      if (!target) { relayError(ws, env.id, 'DEVICE_OFFLINE', 'target device is offline', env.dst); return; }
      if (CONTROL.has(env.kind) && !target.remoteControlEnabled) { relayError(ws, env.id, 'REMOTE_DISABLED', 'target device does not allow remote control', env.dst); return; }
      log({ route: env.kind, from: short(deviceId), to: short(env.dst) });
      const fwd = { v: PROTOCOL_VERSION, kind: env.kind, src: deviceId, dst: env.dst };
      if (env.id !== undefined) fwd.id = env.id;
      if (env.payload !== undefined) fwd.payload = env.payload;
      if (env.auth !== undefined) fwd.auth = env.auth;
      send(target.ws, fwd);
    });
    ws.on('close', () => {
      if (active.get(deviceId)?.ws === ws) {
        active.delete(deviceId);
        const d = devices.get(deviceId);
        if (d) d.lastSeenAt = Date.now();
        broadcast(presenceOf(deviceId));
      }
    });
  });
});

server.listen(Number(process.env.MOCK_PORT) || 0, '127.0.0.1', () => {
  process.stdout.write(JSON.stringify({ port: server.address().port }) + '\n');
});
for (const sig of ['SIGINT', 'SIGTERM']) process.on(sig, () => server.close(() => process.exit(0)));
