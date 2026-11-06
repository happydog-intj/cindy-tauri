// Cindy AI 网关的本地转发：Pi 的 models.json 里 cindy 块指向这里（127.0.0.1 随机端口）。
// 对齐 desktop anthropic-compat-proxy-host 的 gateway-key 路由，只保留 Tauri 需要的部分：
// - 只接受带已登记 Pi 会话 token 的请求（x-cindy-pi-session-id / -token，maker-core 每个会话
//   生成一次，经 registerPiProxySession 登记）。回环端口本身不当作信任边界；
// - 转发前一律去掉 x-cindy-pi-session-id / -token / -provider-id，以及 Pi 带来的占位
//   authorization / x-api-key / x-goog-api-key，再注入真实网关 key；
// - 网关 key 和 endpoint 每次请求现取，Pi 进程环境里只有占位值，拿不到真实 key；
// - 网关回 401 时通知上层静默重新拉凭据（上层自己限频）。
import http from 'node:http';
import { Readable } from 'node:stream';
import { timingSafeEqual } from 'node:crypto';

const INTERNAL_HEADERS = ['x-cindy-pi-session-id', 'x-cindy-pi-session-token', 'x-cindy-pi-provider-id'];
const CREDENTIAL_HEADERS = ['authorization', 'x-api-key', 'x-goog-api-key'];
// 逐跳头和由 fetch 重新计算的头不转发。
const HOP_HEADERS = ['host', 'connection', 'keep-alive', 'proxy-connection', 'transfer-encoding', 'upgrade', 'te', 'trailer', 'content-length', 'accept-encoding'];
const RESPONSE_DROP = new Set(['connection', 'keep-alive', 'transfer-encoding', 'content-encoding', 'content-length']);

function log(msg) {
  process.stderr.write(`[gateway-forwarder] ${msg}\n`);
}

function tokenEquals(a, b) {
  const x = Buffer.from(a);
  const y = Buffer.from(b);
  return x.length === y.length && timingSafeEqual(x, y);
}

function headerValue(headers, name) {
  const v = headers[name];
  return typeof v === 'string' ? v : Array.isArray(v) ? v[0] ?? null : null;
}

function reply(res, status, code, message) {
  if (res.headersSent) { res.destroy(); return; }
  res.writeHead(status, { 'content-type': 'application/json' });
  res.end(JSON.stringify({ error: { code, message } }));
}

/**
 * getUpstream(): 当前可用的 { endpoint, key }；未登录或凭据未就绪返回 null。
 * onRejected(key): 网关对该 key 回了 401。
 */
export function startGatewayForwarder({ getUpstream, onRejected = () => {} }) {
  // sessionId → Set<{ token, scope }>
  const sessions = new Map();
  let port = 0;

  function isRegistered(sessionId, token) {
    const regs = sessionId ? sessions.get(sessionId) : null;
    if (!regs || !token) return false;
    for (const r of regs) if (tokenEquals(r.token, token)) return true;
    return false;
  }

  /** maker-core AgentDeps.registerPiProxySession 的实现。返回注销函数。 */
  function registerSession(sessionId, token, _resolveProviderId, options = {}) {
    if (typeof sessionId !== 'string' || !sessionId || typeof token !== 'string' || !token) {
      throw new Error('Pi proxy session registration requires an id and token');
    }
    const regs = sessions.get(sessionId) ?? new Set();
    for (const r of regs) if (tokenEquals(r.token, token)) regs.delete(r);
    const entry = { token, scope: options.scope ?? 'session' };
    regs.add(entry);
    sessions.set(sessionId, regs);
    return () => {
      const cur = sessions.get(sessionId);
      if (!cur) return;
      cur.delete(entry);
      if (cur.size === 0) sessions.delete(sessionId);
    };
  }

  const server = http.createServer(async (req, res) => {
    const sessionId = headerValue(req.headers, 'x-cindy-pi-session-id');
    const token = headerValue(req.headers, 'x-cindy-pi-session-token');
    if (!isRegistered(sessionId, token)) {
      reply(res, 403, 'FORBIDDEN', 'unknown Pi session');
      req.resume();
      return;
    }
    const upstream = getUpstream();
    if (!upstream) {
      reply(res, 401, 'UNAUTHORIZED', 'Cindy AI is not signed in');
      req.resume();
      return;
    }
    const headers = {};
    for (const [name, value] of Object.entries(req.headers)) {
      const lower = name.toLowerCase();
      if (HOP_HEADERS.includes(lower) || INTERNAL_HEADERS.includes(lower) || CREDENTIAL_HEADERS.includes(lower)) continue;
      headers[lower] = Array.isArray(value) ? value.join(', ') : value;
    }
    headers.authorization = `Bearer ${upstream.key}`;
    headers['x-api-key'] = upstream.key;
    // 让上游回未压缩的流，SSE 按块透传，不用这边解压。
    headers['accept-encoding'] = 'identity';
    const controller = new AbortController();
    res.on('close', () => { if (!res.writableFinished) controller.abort(); });
    const hasBody = req.method !== 'GET' && req.method !== 'HEAD';
    let upstreamRes;
    try {
      upstreamRes = await fetch(upstream.endpoint + req.url, {
        method: req.method,
        headers,
        ...(hasBody ? { body: Readable.toWeb(req), duplex: 'half' } : {}),
        signal: controller.signal,
        redirect: 'manual',
      });
    } catch (err) {
      if (!controller.signal.aborted) log(`upstream request failed: ${err?.name ?? 'Error'}`);
      reply(res, 502, 'BAD_GATEWAY', 'Cindy AI gateway is unreachable');
      return;
    }
    if (upstreamRes.status === 401) {
      try { onRejected(upstream.key); } catch { /* 上层自己记日志 */ }
    }
    const outHeaders = {};
    upstreamRes.headers.forEach((value, name) => {
      if (!RESPONSE_DROP.has(name.toLowerCase())) outHeaders[name] = value;
    });
    res.writeHead(upstreamRes.status, outHeaders);
    if (!upstreamRes.body) { res.end(); return; }
    const body = Readable.fromWeb(upstreamRes.body);
    body.on('error', () => res.destroy());
    body.pipe(res);
  });
  server.keepAliveTimeout = 5_000;

  const ready = new Promise((resolve, reject) => {
    server.once('error', reject);
    server.listen(0, '127.0.0.1', () => {
      port = server.address().port;
      server.off('error', reject);
      resolve();
    });
  });
  ready.catch((err) => log(`listen failed: ${err?.message ?? err}`));
  server.unref();

  return {
    ready,
    /** Pi 网关块的 baseUrl；还没监听上返回 undefined。 */
    url: () => (port ? `http://127.0.0.1:${port}` : undefined),
    registerSession,
    close: () => new Promise((resolve) => server.close(() => resolve())),
    _test: { sessionCount: () => sessions.size },
  };
}
