// 确定性的 Cindy 云端 mock（零依赖），给登录 / 云端模型 e2e 用：auth-server + model-access 两套接口同一个端口。
// 启动后在 stdout 打印一行 {"port":N}；请求日志写 stderr，前缀 [mock-cloud]，只记路径、状态和布尔事实，不记 token / key。
//
// 环境变量：
// - MOCK_GATEWAY_ENDPOINT：credentials 下发的网关 endpoint（e2e 指向 mock-openai 的 /keyed）。
// - MOCK_GATEWAY_KEY：credentials 下发的网关 key（与 mock-openai 的 MOCK_API_KEY 相同）。
// - MOCK_EMAIL_CODE：邮箱验证码（默认 123456）。
// 控制接口（只给测试脚本用）：
// - POST /__mock/authorize {clientState}：模拟用户在浏览器里完成授权（等价打开 authorize 页并同意）。
// - POST /__mock/refresh-fail {code}：下一次 /api/auth/refresh 按给定错误码失败。
// - GET  /__mock/state：返回计数（refresh 次数、有效 refresh token 数、logout 次数、captcha 是否带上等）。
import { createServer } from 'node:http';
import { createHash, randomBytes } from 'node:crypto';

const GATEWAY_ENDPOINT = process.env.MOCK_GATEWAY_ENDPOINT || 'http://127.0.0.1:9/keyed';
let gatewayKey = process.env.MOCK_GATEWAY_KEY || 'mock-gateway-key';
const EMAIL_CODE = process.env.MOCK_EMAIL_CODE || '123456';
const GATEWAY_MODEL_ID = 'mock-gw-model';

const log = (obj) => process.stderr.write(`[mock-cloud] ${JSON.stringify({ at: new Date().toISOString(), ...obj })}\n`);

const membership = (email) => ({
  id: 'mem-personal-1', passportId: 'passport-1', kind: 'personal', role: 'owner',
  displayName: 'E2E User', avatarUrl: null, email, orgId: null, orgName: null, orgLogoUrl: null,
});

// 状态
const accessTokens = new Map(); // token → email
const refreshTokens = new Map(); // token → email（只有最新一枚有效：轮换后旧的失效）
const authorizeRequests = new Map(); // clientState → { codeChallenge, deviceId }
const authorizationCodes = new Map(); // code → { codeChallenge, email }
const callbackResults = new Map(); // clientState → code
const counters = { refresh: 0, logout: 0, requestCode: 0, captchaSeen: 0, credentials: 0, rotate: 0, models: 0, balance: 0, creditUsage: 0 };
let refreshFailCode = null;

const b64url = (v) => Buffer.from(v).toString('base64url');
function issueTokens(email) {
  const exp = Math.floor(Date.now() / 1000) + 3600;
  // MOCK_ACCESS_TOKEN_SUFFIX：固定签名段，测试的 mock 中继据此认出这台设备（token 前缀随 exp 变，中继按后缀匹配）。
  const sig = process.env.MOCK_ACCESS_TOKEN_SUFFIX || randomBytes(8).toString('base64url');
  const access = `${b64url('{"alg":"none"}')}.${b64url(JSON.stringify({ sub: 'u1', exp }))}.${sig}`;
  const refresh = `rt-${randomBytes(16).toString('hex')}`;
  accessTokens.set(access, email);
  for (const [t, e] of refreshTokens) if (e === email) refreshTokens.delete(t);
  refreshTokens.set(refresh, email);
  return { accessToken: access, refreshToken: refresh, membership: membership(email) };
}

function sendJson(res, status, obj) {
  const data = JSON.stringify(obj);
  res.writeHead(status, { 'content-type': 'application/json', 'content-length': Buffer.byteLength(data) });
  res.end(data);
}
const fail = (res, status, code, message = code) => sendJson(res, status, { error: { code, message } });

function readJson(req) {
  return new Promise((resolve) => {
    const chunks = [];
    req.on('data', (c) => chunks.push(c));
    req.on('end', () => {
      const raw = Buffer.concat(chunks).toString('utf8');
      try { resolve(raw ? JSON.parse(raw) : {}); } catch { resolve(null); }
    });
    req.on('error', () => resolve(null));
  });
}

function bearer(req) {
  const h = req.headers.authorization;
  return typeof h === 'string' && h.startsWith('Bearer ') ? h.slice(7) : null;
}
const authedEmail = (req) => accessTokens.get(bearer(req) ?? '') ?? null;

const NOW_ISO = () => new Date().toISOString().replace(/\.\d+Z$/, 'Z');

function modelsPayload() {
  return {
    schemaVersion: 5,
    accountTier: 'paid',
    models: [{
      id: GATEWAY_MODEL_ID,
      name: 'Mock Gateway Model',
      agents: ['pi'],
      availability: 'available',
      mode: 'chat',
      contextWindow: 128000,
      maxOutputTokens: 8192,
      sortOrder: 1,
      perAgent: { pi: { wireProtocol: 'openai-completions' } },
    }],
  };
}

function balancePayload() {
  return { scale: 9, planCredits: '1.5', purchasedCredits: '2', promotionalCredits: '0.5', available: '4', observedAt: NOW_ISO() };
}

function creditUsagePayload() {
  return {
    scale: 9,
    available: '4',
    plan: { remaining: '1.5', used: '0.5', total: '2' },
    purchased: { remaining: '2', used: null, total: null },
    promotional: { remaining: '0.5', used: null, total: null },
    promotionalGrants: [],
    promotionalGrantsComplete: false,
    promotionalGrantConsistency: 'OBSERVED',
    ledgerUpdatedAt: null,
    observedAt: NOW_ISO(),
  };
}

async function handle(req, res, url) {
  const route = url.pathname.replace(/\/+$/, '');
  const method = req.method;

  // ── 测试控制 ──
  if (route === '/__mock/state' && method === 'GET') {
    return sendJson(res, 200, { ...counters, liveRefreshTokens: refreshTokens.size, liveAccessTokens: accessTokens.size, pendingAuthorize: authorizeRequests.size });
  }
  if (route === '/__mock/authorize' && method === 'POST') {
    const body = await readJson(req);
    const pending = authorizeRequests.get(body?.clientState);
    if (!pending) return fail(res, 404, 'NOT_FOUND');
    const code = `ac-${randomBytes(8).toString('hex')}`;
    authorizationCodes.set(code, { codeChallenge: pending.codeChallenge, email: 'browser@example.com' });
    callbackResults.set(body.clientState, code);
    authorizeRequests.delete(body.clientState);
    return sendJson(res, 200, { ok: true });
  }
  if (route === '/__mock/refresh-fail' && method === 'POST') {
    const body = await readJson(req);
    refreshFailCode = typeof body?.code === 'string' ? body.code : null;
    return sendJson(res, 200, { ok: true });
  }

  // ── auth-server ──
  if (route === '/api/auth/providers' && method === 'GET') {
    return sendJson(res, 200, {
      region: 'global', attribution: 'email', email: true, phone: false, social: ['google', 'apple'],
      captcha: { provider: 'turnstile', siteKey: 'mock-site-key', requiredFor: ['email_request_code'] },
    });
  }
  if (route === '/api/auth/discovery' && method === 'POST') {
    return sendJson(res, 200, { methods: [{ type: 'email_code' }] });
  }
  if (route === '/api/auth/sso/discovery' && method === 'POST') {
    return fail(res, 404, 'ORG_SSO_NOT_FOUND');
  }
  if (route === '/api/auth/email/request-code' && method === 'POST') {
    const body = await readJson(req);
    counters.requestCode += 1;
    if (!body?.captchaToken) return fail(res, 400, 'CAPTCHA_REQUIRED');
    counters.captchaSeen += 1;
    return sendJson(res, 200, { status: 'sent' });
  }
  if (route === '/api/auth/email/verify-code' && method === 'POST') {
    const body = await readJson(req);
    if (body?.code !== EMAIL_CODE) return fail(res, 400, 'INVALID_CODE');
    if (!body.deviceId || body.clientType !== 'desktop') return fail(res, 400, 'INVALID_PARAMS');
    return sendJson(res, 200, { status: 'ok', ...issueTokens(body.email) });
  }
  const authorizeMatch = /^\/api\/auth\/social\/([a-z]+)\/authorize$/.exec(route);
  if (authorizeMatch && method === 'GET') {
    const q = url.searchParams;
    if (q.get('code_challenge_method') !== 'S256' || !q.get('code_challenge') || !q.get('client_state') || q.get('client_type') !== 'desktop') {
      return fail(res, 400, 'INVALID_PARAMS');
    }
    authorizeRequests.set(q.get('client_state'), { codeChallenge: q.get('code_challenge'), deviceId: q.get('device_id') });
    res.writeHead(200, { 'content-type': 'text/html' });
    return res.end('<html><body>mock authorize</body></html>');
  }
  if (route === '/api/auth/desktop/callback/poll' && method === 'POST') {
    const body = await readJson(req);
    if (typeof body?.pollSecret !== 'string') return fail(res, 400, 'INVALID_PARAMS');
    const clientState = createHash('sha256').update(body.pollSecret, 'utf8').digest('base64url');
    const code = callbackResults.get(clientState);
    if (!code) return sendJson(res, 200, { status: authorizeRequests.has(clientState) ? 'pending' : 'pending' });
    callbackResults.delete(clientState);
    return sendJson(res, 200, { status: 'ok', code });
  }
  if (route === '/api/auth/token' && method === 'POST') {
    const body = await readJson(req);
    const entry = authorizationCodes.get(body?.code);
    if (!entry || body.grantType !== 'authorization_code') return fail(res, 400, 'INVALID_AUTH_CODE');
    authorizationCodes.delete(body.code);
    const challenge = createHash('sha256').update(String(body.codeVerifier ?? '')).digest('base64url');
    if (challenge !== entry.codeChallenge) return fail(res, 400, 'INVALID_AUTH_CODE');
    return sendJson(res, 200, { status: 'ok', ...issueTokens(entry.email) });
  }
  if (route === '/api/auth/refresh' && method === 'POST') {
    const body = await readJson(req);
    counters.refresh += 1;
    if (refreshFailCode) {
      const code = refreshFailCode;
      refreshFailCode = null;
      return fail(res, 401, code);
    }
    const email = refreshTokens.get(body?.refreshToken);
    if (!email) return fail(res, 401, 'INVALID_REFRESH_TOKEN');
    return sendJson(res, 200, issueTokens(email));
  }
  if (route === '/api/auth/logout' && method === 'POST') {
    const email = authedEmail(req);
    if (!email) return fail(res, 401, 'INVALID_TOKEN');
    counters.logout += 1;
    for (const [t, e] of accessTokens) if (e === email) accessTokens.delete(t);
    for (const [t, e] of refreshTokens) if (e === email) refreshTokens.delete(t);
    return sendJson(res, 200, { status: 'ok' });
  }

  // ── model-access ──
  if (route.startsWith('/api/model-access/')) {
    if (!authedEmail(req)) return fail(res, 401, 'INVALID_TOKEN');
    if (route === '/api/model-access/credentials' && method === 'GET') {
      counters.credentials += 1;
      return sendJson(res, 200, { endpoint: GATEWAY_ENDPOINT, apiKey: gatewayKey });
    }
    if (route === '/api/model-access/credentials/rotate' && method === 'POST') {
      counters.rotate += 1;
      gatewayKey = `${gatewayKey}-r${counters.rotate}`;
      return sendJson(res, 200, { endpoint: GATEWAY_ENDPOINT, apiKey: gatewayKey });
    }
    if (route === '/api/model-access/models' && method === 'GET') {
      counters.models += 1;
      if (url.searchParams.get('schemaVersion') !== '5') return fail(res, 400, 'INVALID_PARAMS');
      return sendJson(res, 200, modelsPayload());
    }
    if (route === '/api/model-access/balance' && method === 'GET') {
      counters.balance += 1;
      return sendJson(res, 200, balancePayload());
    }
    if (route === '/api/model-access/credit-usage' && method === 'GET') {
      counters.creditUsage += 1;
      return sendJson(res, 200, creditUsagePayload());
    }
  }

  return fail(res, 404, 'NOT_FOUND', `not found: ${method} ${route}`);
}

const server = createServer(async (req, res) => {
  const url = new URL(req.url ?? '/', 'http://127.0.0.1');
  try {
    await handle(req, res, url);
  } catch (err) {
    log({ path: url.pathname, error: String(err?.message ?? err) });
    if (!res.headersSent) fail(res, 500, 'INTERNAL');
    else res.end();
  }
  log({ method: req.method, path: url.pathname, status: res.statusCode, bearer: bearer(req) !== null });
});

server.listen(Number(process.env.MOCK_PORT) || 0, '127.0.0.1', () => {
  process.stdout.write(JSON.stringify({ port: server.address().port }) + '\n');
});

for (const sig of ['SIGINT', 'SIGTERM']) process.on(sig, () => server.close(() => process.exit(0)));
