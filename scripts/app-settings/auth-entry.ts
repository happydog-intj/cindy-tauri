// 账号登录纯逻辑的黄金样例：运行上游 auth-client / authIpc / authRefreshBackoff / authHostedCallback，输出（输入, 输出）。
import {
  CindyAuthClient, desktopAuthorizationPollSchema, isValidEmail, loginOutcomeSchema, membershipSchema, parseAuthSessionRecord, providerConfigSchema,
  authRequestId, retryAfterDeadline, recognizeLoginMethods, reduceAuthFlow, serializeAuthSessionRecord, ssoOrgDiscoverySchema, tokenPairSchema,
} from '@cindy/auth-client';
import { loginPreparingErrorState, parseDesktopLoginAction } from '@cindy-desktop-shared/authIpc';
import { AuthRefreshBackoff } from '@cindy-desktop-main/authRefreshBackoff.ts';
import {
  deriveClientStateFromPollSecret, hostedCallbackPollDelayMs, mapDesktopAuthorizationPoll, pollErrorCode,
  runHostedCallbackPolling,
} from '@cindy-desktop-main/authHostedCallback.ts';

const U = '\u0000undef';
const norm = (v: any) => JSON.parse(JSON.stringify(v === undefined ? null : v));

// ── isValidEmail ──
const emails = ['a@b.co', ' a@b.co ', 'a@b', 'a@b.', 'a@.b', '@b.co', 'a@@b.co', 'a b@c.de', '', ' ', 'a@b.c.d', 'x@' + 'a'.repeat(50),
  'é@ü.de', 'a@b\u00a0.co', 'a\u2003@b.co', 'a@b.co\n', '\ufeffa@b.co', 'a@b..co', 'a@b.c@d', 'a@b.\u3000c', 'user+tag@example.com'];
const emailCases = emails.map((input) => ({ input, output: isValidEmail(input) }));

// ── reduceAuthFlow ──
const providers = { region: 'global', attribution: 'email', email: true, phone: false, social: ['google', 'apple'] };
const membership = { id: 'm1', kind: 'personal', role: 'owner', displayName: 'A', email: 'a@b.co', orgId: null, orgName: null };
const methods = [{ type: 'email_code' }, { type: 'sso', connectionId: 'c1', protocol: 'oidc', orgName: 'O', connectionName: 'N', ssoRequired: false }];
const actions: any[] = [
  { type: 'providers-loaded', providers },
  { type: 'realm-switch-required', targetRegion: 'cn', providers, methods },
  { type: 'realm-switch-required', targetRegion: 'cn', email: 'a@b.co', personalLoginAvailable: true, providers, methods },
  { type: 'realm-switch-required', targetRegion: 'cn', email: '', personalLoginAvailable: false, providers, methods },
  { type: 'discovery-loaded', email: 'a@b.co', methods },
  { type: 'code-requested', kind: 'email', identifier: 'a@b.co' },
  { type: 'browser-started', label: 'Google' },
  { type: 'sso-verification-code-requested', channel: 'email', targetMasked: 'a***@b.co' },
  { type: 'binding-code-requested', bindType: 'phone', contact: '+8613800000000' },
  { type: 'failed', code: 'X', recoverTo: 'identifier' },
  { type: 'outcome', outcome: { status: 'ok', accessToken: 'a', refreshToken: 'r', membership } },
  { type: 'outcome', outcome: { status: 'select_account', loginTicket: 't', accounts: [membership] } },
  { type: 'outcome', outcome: { status: 'binding_required', bindType: 'email', bindTicket: 't' } },
  { type: 'outcome', outcome: { status: 'sso_verification_required', verificationTicket: 't', channel: 'sms', targetMasked: '138****' } },
];
const flowCases = actions.map((input) => ({ input, output: norm(reduceAuthFlow(null, input)) }));

// ── loginPreparingErrorState ──
const prepArgs: any[][] = [
  [], ['RATE_LIMITED'], ['RATE_LIMITED', 12345], ['RATE_LIMITED', undefined, 429], ['INVALID_RESPONSE', undefined, 502],
  ['INVALID_RESPONSE', undefined, 200], ['INVALID_RESPONSE', undefined, 400], ['INVALID_RESPONSE', undefined, 599], ['INVALID_RESPONSE', undefined, 600],
  ['INVALID_RESPONSE', undefined, 502.5], ['AUTH_SERVICE_UNAVAILABLE', 5, 500], ['X', 1, 2],
];
const prepCases = prepArgs.map((args) => ({ args: args.map((a) => (a === undefined ? U : a)), output: norm(loginPreparingErrorState(...(args as [any])) ) }));

// ── parseDesktopLoginAction ──
const long = (n: number) => 'x'.repeat(n);
const raw: any[] = [
  null, undefined, 3, 'x', [], {}, { type: 3 }, { type: 'nope' },
  { type: 'reset', extra: 1 }, { type: 'back' }, { type: 'cancel-browser' }, { type: 'confirm-sso-realm' }, { type: 'cancel-sso-realm' }, { type: 'request-sso-verification-code', z: 1 },
  { type: 'discover', email: 'a@b.co' }, { type: 'discover', email: '' }, { type: 'discover', email: long(320) }, { type: 'discover', email: long(321) }, { type: 'discover', email: 5 },
  { type: 'discover', email: '😀'.repeat(160) }, { type: 'discover', email: '😀'.repeat(161) },
  { type: 'discover-sso-org', org: long(253) }, { type: 'discover-sso-org', org: long(254) },
  { type: 'request-code', kind: 'email', identifier: 'a' }, { type: 'request-code', kind: 'phone', identifier: 'a', captchaToken: 't' },
  { type: 'request-code', kind: 'sms', identifier: 'a' }, { type: 'request-code', kind: 'email', identifier: 'a', captchaToken: '' },
  { type: 'request-code', kind: 'email', identifier: 'a', captchaToken: long(2048) }, { type: 'request-code', kind: 'email', identifier: 'a', captchaToken: long(2049) },
  { type: 'request-code', kind: 'email', identifier: 'a', captchaToken: null }, { type: 'request-code', kind: 'email', identifier: 'a', captchaToken: 5 },
  { type: 'verify-code', kind: 'email', identifier: 'a', code: '123456' }, { type: 'verify-code', kind: 'email', identifier: 'a', code: long(33) },
  { type: 'verify-code', kind: 'email', identifier: 'a' },
  { type: 'start-browser', kind: 'social', providerOrConnectionId: 'google', label: 'G' }, { type: 'start-browser', kind: 'sso', providerOrConnectionId: 'c', label: 'L' },
  { type: 'start-browser', kind: 'x', providerOrConnectionId: 'c', label: 'L' }, { type: 'start-browser', kind: 'sso', providerOrConnectionId: long(257), label: 'L' },
  { type: 'select-account', accountId: 'a' }, { type: 'select-account', accountId: '' },
  { type: 'verify-sso-verification', code: '1' }, { type: 'verify-sso-verification', code: '' },
  { type: 'request-binding-code', contact: 'c' }, { type: 'request-binding-code' },
  { type: 'verify-binding', contact: 'c', code: '1' }, { type: 'verify-binding', contact: 'c' },
];
const actionCases = raw.map((input) => ({ input: input === undefined ? U : input, output: norm(parseDesktopLoginAction(input)) }));

// ── session record ──
const recs: (string | null)[] = [null, '', 'x', '{}', '[]', '3', 'null',
  JSON.stringify({ version: 1, realm: 'global', refreshToken: 'r' }), JSON.stringify({ version: 1, realm: 'cn', refreshToken: 'r', extra: 1 }),
  JSON.stringify({ version: 2, realm: 'global', refreshToken: 'r' }), JSON.stringify({ version: 1, realm: 'eu', refreshToken: 'r' }),
  JSON.stringify({ version: 1, realm: 'global', refreshToken: '' }), JSON.stringify({ version: 1, realm: 'global' }),
  JSON.stringify({ version: 1.0, realm: 'global', refreshToken: 5 })];
const recordCases = recs.map((input) => ({ input, output: norm(parseAuthSessionRecord(input)) }));
const serialized = ['global', 'cn'].map((realm) => ({ realm, token: 'tok', output: serializeAuthSessionRecord(realm as any, 'tok') }));

// ── client_state / 轮询节奏 / 结果映射 / 错误码 ──
const secrets = ['', 'a', 'abc', 'é😀', 'x'.repeat(43)];
const stateCases = secrets.map((input) => ({ input, output: deriveClientStateFromPollSecret(input) }));
const delays = [0, 1, 29_999, 30_000, 30_001, 1e9].map((input) => ({ input, output: hostedCallbackPollDelayMs(input) }));
const polls: any[] = [{ status: 'pending' }, { status: 'ok', code: 'c' }, { status: 'error', error: 'E' }, { status: 'expired' }];
const mapCases = polls.map((input) => ({ input, output: norm(mapDesktopAuthorizationPoll(input)) }));
const errCodes: any[] = [null, undefined, 3, 'x', {}, { code: 'C' }, { code: '' }, { code: 3 }, new Error('x')];
const errCases = errCodes.map((input) => ({ input: input instanceof Error ? { __error: true } : (input === undefined ? U : input), output: pollErrorCode(input) }));

// ── 退避：随机数与时钟都固定，按脚本重放 ──
const randoms = [0.0, 0.5, 0.999, 0.25, 0.75, 0.1, 0.9, 0.33, 0.66, 0.05, 0.95, 0.42];
type Op = { op: string; epoch?: number; server?: number | null; net?: boolean; adv?: number };
const script: Op[] = [];
let seed = 12345;
const rnd = () => { seed = (seed * 1103515245 + 12345) & 0x7fffffff; return seed / 0x80000000; };
const ops = ['defer', 'defer', 'rejected', 'start', 'release', 'clear', 'remaining', 'runtimeRemaining', 'advance', 'advance'];
for (let i = 0; i < 400; i++) {
  const op = ops[Math.floor(rnd() * ops.length)];
  const epoch = Math.floor(rnd() * 3);
  script.push({ op, epoch, server: rnd() < 0.3 ? Math.floor(rnd() * 200_000) : null, net: rnd() < 0.7, adv: Math.floor(rnd() * 90_000) });
}
let clock = 1_000_000;
let ri = 0;
const bo = new AuthRefreshBackoff(() => clock, () => randoms[ri++ % randoms.length]);
const backoffTrace = script.map((s) => {
  let out: any = null;
  switch (s.op) {
    case 'defer': out = bo.defer(s.epoch!, s.server === null ? undefined : clock + s.server!, s.net); break;
    case 'rejected': out = bo.serverRejectedToken(s.epoch!); break;
    case 'start': bo.startRuntimeAttempt(s.epoch!); break;
    case 'release': bo.release(s.epoch!); break;
    case 'clear': bo.clear(); break;
    case 'remaining': out = bo.remaining(s.epoch!); break;
    case 'runtimeRemaining': out = bo.runtimeRemaining(s.epoch!); break;
    case 'advance': clock += s.adv!; break;
  }
  return { ...s, out: norm(out), now: clock, randomIndex: ri };
});

// ── 托管回调轮询：脚本化响应 + 假时钟（sleep 推进时钟） ──
type Step = { r?: any; e?: string | null; abort?: boolean };
const pollScenarios: { steps: Step[]; timeoutMs: number; maxFailures?: number }[] = [
  { steps: [{ r: { status: 'ok', code: 'c1' } }], timeoutMs: 300_000 },
  { steps: [{ r: { status: 'pending' } }, { r: { status: 'pending' }}, { r: { status: 'ok', code: 'c2' } }], timeoutMs: 300_000 },
  { steps: [{ r: { status: 'error', error: 'access_denied' } }], timeoutMs: 300_000 },
  { steps: [{ r: { status: 'pending' } }, { r: { status: 'expired' } }], timeoutMs: 300_000 },
  { steps: Array.from({ length: 8 }, () => ({ e: 'NETWORK_ERROR' })), timeoutMs: 300_000 },
  { steps: Array.from({ length: 8 }, () => ({ e: null })), timeoutMs: 300_000 },
  { steps: [...Array.from({ length: 7 }, () => ({ e: 'NETWORK_ERROR' })), { r: { status: 'pending' } }, ...Array.from({ length: 7 }, () => ({ e: 'NETWORK_ERROR' })), { r: { status: 'ok', code: 'c3' } }], timeoutMs: 300_000 },
  { steps: [{ e: 'A' }, { e: 'B' }, { e: 'C' }], timeoutMs: 300_000, maxFailures: 3 },
  { steps: Array.from({ length: 200 }, () => ({ r: { status: 'pending' } })), timeoutMs: 45_000 },
  { steps: Array.from({ length: 200 }, () => ({ r: { status: 'pending' } })), timeoutMs: 0 },
  { steps: [{ r: { status: 'pending' } }, { abort: true, e: 'NETWORK_ERROR' }], timeoutMs: 300_000 },
  { steps: [{ abort: true, r: { status: 'pending' } }], timeoutMs: 300_000 },
];
// ── zod schema：有效样本 + 逐字段变异（删除 / null / 错类型 / 多余字段），记录 zod 的解析结果 ──
function mutations(base: any): any[] {
  const out: any[] = [base];
  const walk = (node: any, path: (string | number)[]) => {
    if (node && typeof node === 'object') {
      const keys = Array.isArray(node) ? node.map((_, i) => i) : Object.keys(node);
      for (const k of keys) {
        const p = [...path, k];
        const edit = (fn: (parent: any, key: any) => void) => {
          const c = JSON.parse(JSON.stringify(base));
          let t = c;
          for (const q of p.slice(0, -1)) t = t[q];
          fn(t, p[p.length - 1]);
          out.push(c);
        };
        edit((t, key) => { if (Array.isArray(t)) t.splice(key, 1); else delete t[key]; });
        edit((t, key) => { t[key] = null; });
        edit((t, key) => { t[key] = 5; });
        edit((t, key) => { t[key] = ''; });
        edit((t, key) => { t[key] = 'zz'; });
        edit((t, key) => { t[key] = true; });
        walk(node[k], p);
      }
      if (!Array.isArray(node)) {
        const c = JSON.parse(JSON.stringify(base));
        let t = c;
        for (const q of path) t = t[q];
        t.__extra = 1;
        out.push(c);
      }
    }
  };
  walk(base, []);
  out.push(null, 5, 'x', [], {});
  return out;
}
const parseWith = (schema: any) => (input: any) => { const r = schema.safeParse(input); return r.success ? { ok: true, data: norm(r.data) } : { ok: false }; };
const mem = { id: 'm1', passportId: 'p', kind: 'org', role: 'admin', displayName: 'A', avatarUrl: null, email: 'a@b.co', orgId: 'o', orgName: 'O', orgLogoUrl: 'u' };
const memMin = { id: 'm1', kind: 'personal', role: 'owner', displayName: '', email: null, orgId: null, orgName: null };
const sso = { type: 'sso', connectionId: 'c', protocol: 'saml', orgName: 'O', connectionName: 'N', ssoRequired: true };
const schemaSets: { name: string; fn: (i: any) => any; bases: any[] }[] = [
  { name: 'membership', fn: parseWith(membershipSchema), bases: [mem, memMin] },
  { name: 'providerConfig', fn: parseWith(providerConfigSchema), bases: [
    { region: 'global', attribution: 'email', email: true, phone: false, social: ['google', 'apple', 'wechat'] },
    { region: 'cn', attribution: 'phone', email: false, phone: true, social: [], captcha: { provider: 'turnstile', siteKey: 'k', requiredFor: ['email_request_code', 'future', 'phone_request_code'] } },
  ] },
  { name: 'ssoOrgDiscovery', fn: parseWith(ssoOrgDiscoverySchema), bases: [
    { region: 'global', orgName: 'O', connections: [{ connectionId: 'c', protocol: 'oidc', connectionName: 'N' }] },
    { region: 'cn', orgName: '', connections: [] },
  ] },
  { name: 'tokenPair', fn: parseWith(tokenPairSchema), bases: [{ accessToken: 'a', refreshToken: 'r', membership: mem }] },
  { name: 'loginOutcome', fn: parseWith(loginOutcomeSchema), bases: [
    { status: 'ok', accessToken: 'a', refreshToken: 'r', membership: mem },
    { status: 'ok', accessToken: 'a', refreshToken: 'r', membership: mem, accountToken: 't', accountRefreshToken: 'u', accountDeletionRestored: true },
    { status: 'select_account', loginTicket: 'l', accounts: [mem, memMin], accountToken: 't', accountRefreshToken: 'u' },
    { status: 'binding_required', bindType: 'email', bindTicket: 'b' },
    { status: 'sso_verification_required', verificationTicket: 'v', channel: 'sms', targetMasked: '1**' },
  ] },
  { name: 'desktopPoll', fn: parseWith(desktopAuthorizationPollSchema), bases: [
    { status: 'pending' }, { status: 'ok', code: 'c' }, { status: 'error', error: 'e' }, { status: 'expired' },
  ] },
  { name: 'recognizeLoginMethods', fn: (i: any) => { try { return { ok: true, data: norm(recognizeLoginMethods(i)) }; } catch { return { ok: false }; } },
    bases: [[{ type: 'email_code' }, sso], [{ type: 'future' }, { type: 'email_code' }], [{ type: 'sso' }], [{}], [{ type: 5 }], [{ type: '' }], []] },
];
const schemaCases = schemaSets.map((set) => ({
  name: set.name,
  cases: set.bases.flatMap((b) => mutations(b)).map((input) => ({ input: input === undefined ? U : input, output: set.fn(input) })),
}));

// ── request id 脱敏 / Retry-After ──
const hdrs = (h: Record<string, string>) => ({ headers: { get: (n: string) => (n in h ? h[n] : null) } });
const idValues = ['abc', 'req-123', ' req-1 ', '', ' ', '-bad', 'a'.repeat(128), 'a'.repeat(129), '13800138000', '+8613800138000', '138-0013-8000', '1234567', '123456',
  'sk-abcdef', 'SK-abcdef', 'ghp_abc', 'github_pat_xx', 'xoxb-1', 'Bearer abc', 'Bearer', 'Bearerabcdefgh', 'Token12345678', 'Tokenabc', 'basic xyz', 'BASIC1234567890',
  'a b', 'é', 'a:b.c_d-e', 'Bearersk-x', 'tokenpk-x', 'bearer\tx', 'ak-1', 'rk-1', 'xoxz-1'];
const idCases: any[] = [];
for (const a of idValues) {
  idCases.push({ h: { 'x-request-id': a }, output: authRequestId(hdrs({ 'x-request-id': a })) ?? null });
  idCases.push({ h: { 'x-litellm-call-id': a }, output: authRequestId(hdrs({ 'x-litellm-call-id': a })) ?? null });
  idCases.push({ h: { 'x-request-id': 'sk-bad', 'x-litellm-call-id': a }, output: authRequestId(hdrs({ 'x-request-id': 'sk-bad', 'x-litellm-call-id': a })) ?? null });
}
idCases.push({ h: {}, output: authRequestId(hdrs({})) ?? null });
const NOW = 1_700_000_000_000;
const realNow = Date.now;
Date.now = () => NOW;
const raCases: any[] = [];
const ras: [string | null, string | null][] = [
  [null, null], ['', null], [' ', null], ['0', null], ['30', null], [' 30 ', null], ['120', 'Tue, 14 Nov 2023 22:13:20 GMT'], ['99999999999999999999', null], ['9007199254740', null],
  ['9007199254741', null], ['1e3', null], ['-5', null], ['12.5', null], ['abc', null], ['12345678', null],
  ['Tue, 14 Nov 2023 22:15:00 GMT', null], ['Tue, 14 Nov 2023 22:15:00 GMT', 'Tue, 14 Nov 2023 22:13:20 GMT'], ['Tue, 14 Nov 2023 22:10:00 GMT', 'Tue, 14 Nov 2023 22:13:20 GMT'],
  ['Tue, 14 Nov 2023 22:15:00 GMT', 'garbage'], ['Tue, 14 Nov 2023 22:15:00 +0000', null], ['Tue, 14 Nov 2023 22:15:00 +0800', 'Tue, 14 Nov 2023 22:13:20 GMT'],
  ['Mon, 01 Jan 1970 00:00:00 GMT', null], ['Mon, 01 Jan 1970 00:00:00 GMT', 'Tue, 14 Nov 2023 22:13:20 GMT'], ['not a date at all', null], ['Tue, 14 Nov 2023 25:15:00 GMT', null],
];
for (const [ra, date] of ras) {
  const h: Record<string, string> = {};
  if (ra !== null) h['retry-after'] = ra;
  if (date !== null) h['date'] = date;
  raCases.push({ retryAfter: ra, date, now: NOW, output: retryAfterDeadline(hdrs(h)) ?? null });
}
Date.now = realNow;

// ── buildAuthorizeUrl ──
const urlCases: any[] = [];
for (const locale of [undefined, '', 'en', 'zh-Hans-x-lzh', 'pt-BR']) {
  for (const base of ['https://auth.cindy.app', 'http://127.0.0.1:8080///']) {
    for (const [kind, id] of [['social', 'google'], ['sso', 'c 1/é?&#'], ['sso', "a'(b)*!~"], ['social', '']] as const) {
      const c = new CindyAuthClient({ baseUrl: base, region: 'global', deviceId: 'dev-1', clientType: 'desktop', locale, fetch: (async () => { throw new Error('x'); }) as any });
      const redirect = 'https://auth.cindy.app/api/auth/desktop/callback?x=1&y=é';
      urlCases.push({ base, locale: locale ?? U, kind, id, redirect, challenge: 'ch+/=', state: 'st~a', output: c.buildAuthorizeUrl({ kind, providerOrConnectionId: id, redirectUri: redirect, codeChallenge: 'ch+/=', state: 'st~a' }) });
    }
  }
}

async function main() {
const pollCases: any[] = [];
for (const sc of pollScenarios) {
  let t = 0;
  const times: number[] = [];
  const sleeps: number[] = [];
  const ac = new AbortController();
  let i = 0;
  const result = await runHostedCallbackPolling({
    poll: async () => {
      times.push(t);
      const step = sc.steps[i++] ?? { r: { status: 'pending' } };
      if (step.abort) ac.abort();
      if (step.e !== undefined) { const err: any = new Error('x'); if (step.e) err.code = step.e; throw err; }
      return step.r;
    },
    sleep: async (ms) => { sleeps.push(ms); t += ms; },
    now: () => t,
    signal: ac.signal,
    timeoutMs: sc.timeoutMs,
    ...(sc.maxFailures ? { maxConsecutiveFailures: sc.maxFailures } : {}),
  });
  pollCases.push({ ...sc, result: norm(result), pollTimes: times, sleeps });
}

process.stdout.write(JSON.stringify({
  emailCases, flowCases, prepCases, actionCases, recordCases, serialized, stateCases, delays, mapCases, errCases,
  backoff: { randoms, trace: backoffTrace }, pollCases, schemaCases, idCases, raCases, urlCases,
}));
}
main().catch((e) => { console.error(e); process.exit(1); });
