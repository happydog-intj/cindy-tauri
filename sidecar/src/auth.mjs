// Cindy 账号登录（desktop main/authManager.ts 的单数据集精简版，第一批）。
// - 邮箱验证码（含 Turnstile captcha）、Google / Apple 浏览器登录（托管回调 + 轮询，不起本地监听端口）、
//   自动续期、退出登录、本机模式进出；
// - 只有一个区域（global），只保存一个账号：refresh token 存 secret store 的 cindy_auth_session_v1，
//   access token 只在内存里；renderer 的 safe-storage-* 通道读不到这些键（见 secret-store.mjs）；
// - 登录 / 退出只切换应用会话模式（owner.mjs），任务、设置、自定义模型都保留；
// - 多账号切换、企业 SSO 区域确认、注销账号属于第二批，相关通道返回 desktop 的“不支持”形状。
import crypto from 'node:crypto';
import {
  AuthApiError,
  CAPTCHA_CHALLENGE_PAGE_PATH,
  CindyAuthClient,
  discoverEmailLogin,
  parseAuthSessionRecord,
  reduceAuthFlow,
  serializeAuthSessionRecord,
  ssoOrgDiscoveryToMethods,
} from '@cindy/auth-client';
import { loginPreparingErrorState, parseDesktopLoginAction } from '@cindy-desktop-shared/authIpc';
import { createDesktopPollCredentials, runHostedCallbackPolling } from '@cindy-desktop-main/authHostedCallback.ts';
import { AuthRefreshBackoff } from '@cindy-desktop-main/authRefreshBackoff.ts';
import { deviceId as loadDeviceId } from './boot-stubs.mjs';
import { AUTH_REGION, authBaseUrl, authDesktopCallbackUrl } from './cloud-endpoints.mjs';
import { emitToRenderer, registerInvoke, registerSync } from './ipc.mjs';
import { getMainLocale } from './main-locale.mjs';
import { callNative } from './native.mjs';
import { commitAppSession, getAppMode, getCachedUser, ownerStamp } from './owner.mjs';
import { getSecretSync, hasSecret, removeSecret, setSecret, whenReady } from './secret-store.mjs';

export const AUTH_SESSION_KEY = 'cindy_auth_session_v1';
const DEFAULT_MODEL = 'claude-sonnet-4-6';
const DEFAULT_EFFORT = 'medium';
const LOGIN_PROVIDERS_GATE_MS = 30_000;
const COLD_START_GATE_MS = 20_000;
const BROWSER_AUTH_TIMEOUT_MS = 5 * 60_000;
const BROWSER_LAUNCH_TIMEOUT_MS = 15_000;
const LOGOUT_REQUEST_TIMEOUT_MS = 5_000;
// 只有这几类 refresh 失败说明 token 已确定失效，才清掉本地 refresh token（authRefreshFailure.ts）。
const DEFINITIVE_REFRESH_FAILURES = new Map([
  ['INVALID_REFRESH_TOKEN', 'replaced-elsewhere'],
  ['REFRESH_TOKEN_EXPIRED', 'expired'],
  ['MEMBERSHIP_DISABLED', 'account-unavailable'],
]);
// 登录流程里这些错误不能原地重试，要回到起点。
const FLOW_CANNOT_RETRY = new Set([
  'INVALID_LOGIN_TICKET', 'INVALID_BIND_TICKET', 'INVALID_SSO_VERIFICATION_TICKET',
  'INVALID_AUTH_CODE', 'INVALID_TOKEN', 'TOKEN_EXPIRED', 'USER_CANCELLED',
]);

let deviceId = '';
// 会话
let accessToken = null;
let latestRefreshToken = null; // 最近一次轮换拿到的 refresh token；落盘失败时以内存值为准
let currentUser = null;
let authStateEpoch = 0;
let refreshTimer = null;
let refreshPromise = null;
let credentialStoreUnavailable = false;
const refreshBackoff = new AuthRefreshBackoff();
const authListeners = new Set();
// 登录流程
let loginFlowEpoch = 0;
let loginFlowState = null;
let providerConfig = null;
let discoveredMethods = [];
let pendingAccountToken = null;
let pendingLoginTicket = null;
let pendingBindTicket = null;
let pendingSsoVerificationTicket = null;
let loginActionPromise = null;
let loginActionPromiseEpoch = null;
let commitSealed = false;
let browserCancel = null;

function log(level, msg) {
  process.stderr.write(`[auth] ${level} ${msg}\n`);
}

function ipcError(code, message) {
  return new Error(`[${code}] ${message}`);
}

function createClient() {
  return new CindyAuthClient({
    baseUrl: authBaseUrl(),
    region: AUTH_REGION,
    deviceId,
    clientType: 'desktop',
    locale: getMainLocale(),
    fetch: (input, init) => globalThis.fetch(input, init),
    // 只记路径、状态码和固定文案；请求体与 token 不进日志。
    onRequestError: (requestPath, status, error) => {
      const name = error instanceof Error ? error.name : typeof error;
      const message = error instanceof Error ? error.message.slice(0, 200) : '';
      log('warn', `auth request failed ${requestPath} status=${status} ${name} ${message}`);
    },
  });
}

// ── 用户与快照 ──────────────────────────────────────────────────────────────

function mapMembership(membership) {
  return {
    id: membership.id,
    name: membership.displayName || membership.email || 'Cindy',
    avatar: membership.avatarUrl ?? null,
    email: membership.email,
    defaultModel: DEFAULT_MODEL,
    defaultEffort: DEFAULT_EFFORT,
    membershipKind: membership.kind,
    membershipRole: membership.role,
    orgId: membership.orgId,
    orgName: membership.orgName,
    orgLogoUrl: membership.orgLogoUrl ?? null,
    passportId: membership.passportId ?? '',
  };
}

function mergeMembership(membership, existing) {
  const mapped = mapMembership(membership);
  if (!existing || existing.id !== mapped.id) return mapped;
  return { ...mapped, avatar: mapped.avatar ?? existing.avatar, passportId: mapped.passportId || existing.passportId };
}

// 冷启动还没续期成功时，用 app-session.json 里缓存的展示信息撑起界面。
function userFromCache(cached) {
  if (!cached) return null;
  return {
    id: cached.id, name: cached.name || cached.email || 'Cindy', avatar: null, email: cached.email ?? null,
    defaultModel: DEFAULT_MODEL, defaultEffort: DEFAULT_EFFORT,
    membershipKind: cached.membershipKind, membershipRole: 'member',
    orgId: null, orgName: null, orgLogoUrl: null, passportId: '',
  };
}

// access token 的 orgSlug claim（只解码不验签，等价 desktop authTokenClaims.ts）。
function orgSlugFromToken(token) {
  const payload = token?.split('.')[1];
  if (!payload) return null;
  try {
    const claims = JSON.parse(Buffer.from(payload, 'base64url').toString('utf8'));
    return typeof claims.orgSlug === 'string' && claims.orgSlug ? claims.orgSlug : null;
  } catch {
    return null;
  }
}

export function snapshotAuthState() {
  const mode = getAppMode();
  const user = mode === 'cloud' ? currentUser : null;
  const stamp = ownerStamp();
  return {
    user: user ? { ...user, orgSlug: orgSlugFromToken(accessToken) } : null,
    mode,
    dataOwnerId: stamp.dataOwnerId,
    ownerGeneration: stamp.ownerGeneration,
    canEnterApp: mode !== 'signed-out',
    isAuthenticated: mode === 'cloud' && accessToken !== null && currentUser !== null,
    isCanary: false,
    deviceId,
    hasAccountDeletionReceipt: false,
    accountDeletionRestored: false,
    credentialStoreUnavailable,
  };
}

function notifyRenderer() {
  emitToRenderer('auth:state-change', snapshotAuthState());
}

function notifyListeners() {
  const state = authInfo();
  for (const fn of authListeners) {
    try { fn(state); } catch (err) { log('warn', `auth listener failed: ${err?.message ?? err}`); }
  }
}

/** 给云端模型等模块用：当前是否已登录、用户 id、会员类型。不含 token。 */
export function authInfo() {
  const authenticated = getAppMode() === 'cloud' && accessToken !== null && currentUser !== null;
  return {
    authenticated,
    userId: authenticated ? currentUser.id : null,
    membershipKind: authenticated ? currentUser.membershipKind : null,
    epoch: authStateEpoch,
  };
}

export function onAuthChange(fn) {
  authListeners.add(fn);
  return () => authListeners.delete(fn);
}

/** 当前 access token（只给 sidecar 内部的服务端请求用）。 */
export function getAccessToken() {
  return getAppMode() === 'cloud' ? accessToken : null;
}

// ── 续期 ────────────────────────────────────────────────────────────────────

function clearRefreshTimer() {
  if (refreshTimer !== null) clearTimeout(refreshTimer);
  refreshTimer = null;
}

function scheduleRefresh(token) {
  refreshBackoff.clear();
  clearRefreshTimer();
  try {
    const payload = JSON.parse(Buffer.from(token.split('.')[1], 'base64url').toString('utf8'));
    const delay = (payload.exp - 300) * 1000 - Date.now();
    if (!Number.isFinite(delay)) return;
    if (delay <= 0) void refresh();
    else refreshTimer = setTimeout(() => void refresh(), Math.min(delay, 2 ** 31 - 1));
  } catch {
    // JWT 格式不对：不排续期，等 401 触发。
  }
}

function scheduleRetry(epoch, retryAt, networkFailure) {
  clearRefreshTimer();
  const delay = refreshBackoff.defer(epoch, retryAt, networkFailure);
  refreshTimer = setTimeout(() => void refresh(), Math.max(1_000, delay));
}

function clearSessionMemory() {
  clearRefreshTimer();
  refreshBackoff.clear();
  accessToken = null;
  latestRefreshToken = null;
  currentUser = null;
}

/** 会话失效：回到未登录。确定性失效才删 refresh token；DEVICE_MISMATCH 保留（desktop 同规则）。 */
export async function expireSession(reason, { removeToken = true, notifyExpired = true } = {}) {
  authStateEpoch += 1;
  clearSessionMemory();
  if (removeToken) {
    try { await removeSecret(AUTH_SESSION_KEY); } catch (err) { log('warn', `remove auth session failed: ${err?.message ?? err}`); }
  }
  commitAppSession('signed-out');
  resetLoginFlowState();
  notifyRenderer();
  if (notifyExpired) emitToRenderer('auth:session-expired', { message: '', reason: reason ?? 'unknown' });
  notifyListeners();
}

async function readStoredRefreshToken() {
  if (latestRefreshToken) return { token: latestRefreshToken };
  if (!(await whenReady())) return { unavailable: true };
  const record = parseAuthSessionRecord(getSecretSync(AUTH_SESSION_KEY));
  if (record && record.realm === AUTH_REGION) return { token: record.refreshToken };
  // 条目在但读不出（解密失败 / 格式不对）按瞬时故障处理，不踢人。
  return hasSecret(AUTH_SESSION_KEY) ? { unavailable: true } : { missing: true };
}

async function runRefresh({ coldStart }) {
  const epoch = authStateEpoch;
  const stored = await readStoredRefreshToken();
  if (epoch !== authStateEpoch) return false;
  if (stored.unavailable) {
    log('warn', 'refresh token unreadable (secret store unavailable); retrying later');
    if (!credentialStoreUnavailable) { credentialStoreUnavailable = true; notifyRenderer(); }
    scheduleRetry(epoch, undefined, false);
    return false;
  }
  if (stored.missing) {
    // 云端模式却没有 refresh token：凭据丢了，只能重新登录。
    await expireSession('credential-lost', { removeToken: false, notifyExpired: !coldStart });
    return false;
  }
  try {
    const pair = await createClient().refresh(stored.token);
    if (epoch !== authStateEpoch) return false;
    latestRefreshToken = pair.refreshToken;
    try {
      await setSecret(AUTH_SESSION_KEY, serializeAuthSessionRecord(AUTH_REGION, pair.refreshToken));
      credentialStoreUnavailable = false;
    } catch (err) {
      credentialStoreUnavailable = true;
      log('warn', `persist rotated refresh token failed: ${err?.message ?? err}`);
    }
    if (epoch !== authStateEpoch) return false;
    const previousId = accessToken ? currentUser?.id : null;
    accessToken = pair.accessToken;
    currentUser = mergeMembership(pair.membership, currentUser);
    commitAppSession('cloud', currentUser);
    scheduleRefresh(pair.accessToken);
    notifyRenderer();
    if (previousId !== currentUser.id) notifyListeners();
    return true;
  } catch (err) {
    if (epoch !== authStateEpoch) return false;
    const code = err instanceof AuthApiError ? err.code : 'AUTH_REQUEST_FAILED';
    const status = err instanceof AuthApiError ? err.statusCode : 0;
    log('warn', `refresh failed status=${status} code=${code}`);
    if (DEFINITIVE_REFRESH_FAILURES.has(code)) {
      await expireSession(DEFINITIVE_REFRESH_FAILURES.get(code), { notifyExpired: !coldStart });
      return false;
    }
    if (code === 'DEVICE_MISMATCH') {
      await expireSession('device-mismatch', { removeToken: false, notifyExpired: !coldStart });
      return false;
    }
    scheduleRetry(epoch, err instanceof AuthApiError ? err.retryAt : undefined, status === 0);
    return false;
  }
}

/** 续期 access token。本机 / 未登录模式直接返回 false；并发调用共用一次请求。 */
export function refresh() {
  if (getAppMode() !== 'cloud') return Promise.resolve(false);
  if (refreshPromise) return refreshPromise;
  if (refreshBackoff.runtimeRemaining(authStateEpoch) > 0) return Promise.resolve(false);
  clearRefreshTimer();
  refreshBackoff.startRuntimeAttempt(authStateEpoch);
  const run = runRefresh({ coldStart: false }).finally(() => { if (refreshPromise === run) refreshPromise = null; });
  refreshPromise = run;
  return run;
}

// ── 启动 ────────────────────────────────────────────────────────────────────

async function initialize() {
  if (getAppMode() !== 'cloud') return snapshotAuthState();
  if (accessToken && currentUser) return snapshotAuthState();
  if (!currentUser) currentUser = userFromCache(getCachedUser());
  if (!refreshPromise) {
    const run = runRefresh({ coldStart: true })
      .catch((err) => { log('warn', `cold start refresh failed: ${err?.message ?? err}`); return false; })
      .finally(() => { if (refreshPromise === run) refreshPromise = null; });
    refreshPromise = run;
  }
  const pending = refreshPromise;
  let timer;
  const timedOut = await Promise.race([
    pending.then(() => false),
    new Promise((resolve) => { timer = setTimeout(() => resolve(true), COLD_START_GATE_MS); }),
  ]);
  clearTimeout(timer);
  // 超时先放行界面（仍是云端模式，只是暂未认证）；后台完成后 runRefresh 自己会推 auth:state-change。
  if (timedOut) log('warn', `cold start refresh still pending after ${COLD_START_GATE_MS}ms`);
  return snapshotAuthState();
}

// ── 登录流程 ────────────────────────────────────────────────────────────────

function superseded() {
  return new AuthApiError('AUTH_FLOW_SUPERSEDED', 409, 'Login was superseded by a newer auth action');
}

function assertLoginFlowCurrent(epoch) {
  if (epoch !== loginFlowEpoch) throw superseded();
}

function resetLoginFlowState() {
  loginFlowState = null;
  discoveredMethods = [];
  pendingAccountToken = null;
  pendingLoginTicket = null;
  pendingBindTicket = null;
  pendingSsoVerificationTicket = null;
}

function cancelBrowser() {
  if (!browserCancel) return false;
  browserCancel.abort();
  browserCancel = null;
  return true;
}

async function withGate(promise, ms, onTimeout) {
  let timer;
  try {
    return await Promise.race([
      promise,
      new Promise((_, reject) => { timer = setTimeout(() => reject(onTimeout()), ms); }),
    ]);
  } finally {
    clearTimeout(timer);
  }
}

async function loadLoginProviders(epoch = loginFlowEpoch) {
  resetLoginFlowState();
  const pending = createClient().getProviders();
  pending.catch(() => {}); // 超时后迟到的失败不再上抛
  const providers = await withGate(pending, LOGIN_PROVIDERS_GATE_MS, () =>
    new AuthApiError('AUTH_SERVICE_UNAVAILABLE', 0, 'Authentication server did not respond in time'));
  assertLoginFlowCurrent(epoch);
  providerConfig = providers;
  loginFlowState = reduceAuthFlow(loginFlowState, { type: 'providers-loaded', providers });
  return loginFlowState;
}

function providersFailure(error) {
  const code = error instanceof AuthApiError ? error.code : 'AUTH_SERVICE_UNAVAILABLE';
  const state = loginPreparingErrorState(code, error instanceof AuthApiError ? error.retryAt : undefined);
  return { success: false, code, state, ...(state.retryAt !== undefined ? { retryAt: state.retryAt } : {}) };
}

async function getLoginState() {
  const epoch = loginFlowEpoch;
  try {
    if (loginFlowState) return { success: true, state: loginFlowState };
    return { success: true, state: await loadLoginProviders(epoch) };
  } catch (error) {
    if (epoch !== loginFlowEpoch) return { success: false, code: 'AUTH_FLOW_SUPERSEDED', state: loginFlowState };
    const failure = providersFailure(error);
    log('warn', `load login providers failed code=${failure.code}`);
    loginFlowState = failure.state;
    return failure;
  }
}

async function assertCredentialStoreReady() {
  if (await whenReady()) return;
  throw new AuthApiError('CREDENTIAL_STORE_UNAVAILABLE', 503, 'Saved account credentials are temporarily unavailable');
}

async function completeLogin(outcome, epoch) {
  assertLoginFlowCurrent(epoch);
  commitSealed = true;
  try {
    const loginEpoch = ++authStateEpoch;
    try {
      await setSecret(AUTH_SESSION_KEY, serializeAuthSessionRecord(AUTH_REGION, outcome.refreshToken));
    } catch {
      throw new AuthApiError('CREDENTIAL_STORE_UNAVAILABLE', 503, 'Saved account credentials are temporarily unavailable');
    }
    if (loginEpoch !== authStateEpoch || epoch !== loginFlowEpoch) throw superseded();
    clearSessionMemory();
    accessToken = outcome.accessToken;
    latestRefreshToken = outcome.refreshToken;
    currentUser = mapMembership(outcome.membership);
    credentialStoreUnavailable = false;
    cancelBrowser();
    commitAppSession('cloud', currentUser);
    scheduleRefresh(outcome.accessToken);
    resetLoginFlowState();
    loginFlowState = reduceAuthFlow(null, { type: 'outcome', outcome });
    notifyRenderer();
    notifyListeners();
    return loginFlowState;
  } finally {
    commitSealed = false;
  }
}

async function acceptLoginOutcome(outcome, epoch) {
  assertLoginFlowCurrent(epoch);
  pendingAccountToken = outcome.status === 'select_account' ? (outcome.accountToken ?? null) : null;
  // 第一批不做“个人登录后提示改用企业 SSO”，直接提交。
  if (outcome.status === 'ok') return completeLogin(outcome, epoch);
  pendingLoginTicket = outcome.status === 'select_account' ? outcome.loginTicket : null;
  pendingBindTicket = outcome.status === 'binding_required' ? outcome.bindTicket : null;
  pendingSsoVerificationTicket = outcome.status === 'sso_verification_required' ? outcome.verificationTicket : null;
  loginFlowState = reduceAuthFlow(loginFlowState, { type: 'outcome', outcome });
  return loginFlowState;
}

function sleepUnlessAborted(ms, signal) {
  if (signal.aborted) return Promise.resolve();
  return new Promise((resolve) => {
    const finish = () => { clearTimeout(timer); signal.removeEventListener('abort', finish); resolve(); };
    const timer = setTimeout(finish, ms);
    signal.addEventListener('abort', finish, { once: true });
  });
}

// 系统浏览器打开授权页（Rust 侧 opener）。打开本身限时 15 秒，不算用户授权的时间。
function launchBrowser(url, signal) {
  if (signal.aborted) return Promise.resolve({ opened: false, error: 'USER_CANCELLED' });
  return new Promise((resolve) => {
    let settled = false;
    const finish = (result) => {
      if (settled) return;
      settled = true;
      clearTimeout(timer);
      signal.removeEventListener('abort', cancel);
      resolve(result);
    };
    const cancel = () => finish({ opened: false, error: 'USER_CANCELLED' });
    const timer = setTimeout(() => finish({ opened: false, error: 'BROWSER_OPEN_TIMEOUT' }), BROWSER_LAUNCH_TIMEOUT_MS);
    signal.addEventListener('abort', cancel, { once: true });
    callNative('open.url', { url }, { timeoutMs: 0 }).then(
      () => finish({ opened: true }),
      () => finish({ opened: false, error: 'BROWSER_OPEN_FAILED' }),
    );
  });
}

async function browserAuthorization(client, input, signal) {
  const { clientState, pollSecret } = createDesktopPollCredentials();
  const url = client.buildAuthorizeUrl({ ...input, state: clientState, redirectUri: authDesktopCallbackUrl() });
  const deadline = Date.now() + BROWSER_AUTH_TIMEOUT_MS;
  const launched = await launchBrowser(url, signal);
  if (!launched.opened) return { error: launched.error };
  return runHostedCallbackPolling({
    poll: async () => {
      try {
        return await client.pollDesktopAuthorization(pollSecret, { signal });
      } catch (error) {
        if (!signal.aborted) log('warn', `hosted auth callback poll failed code=${error?.code ?? error?.name}`);
        throw error;
      }
    },
    sleep: (ms) => sleepUnlessAborted(ms, signal),
    now: () => Date.now(),
    signal,
    timeoutMs: Math.max(0, deadline - Date.now()),
  });
}

async function startBrowser(client, action, epoch) {
  await assertCredentialStoreReady();
  if (action.kind === 'social') {
    if (!providerConfig?.social.includes(action.providerOrConnectionId)) {
      throw new AuthApiError('SOCIAL_PROVIDER_DISABLED', 400, 'Provider is disabled');
    }
  } else if (!discoveredMethods.some((m) => m.type === 'sso' && m.connectionId === action.providerOrConnectionId)) {
    throw new AuthApiError('CONNECTION_NOT_FOUND', 404, 'SSO connection is unavailable');
  }
  const codeVerifier = crypto.randomBytes(32).toString('base64url');
  const codeChallenge = crypto.createHash('sha256').update(codeVerifier).digest('base64url');
  loginFlowState = reduceAuthFlow(loginFlowState, { type: 'browser-started', label: action.label });
  cancelBrowser();
  const cancellation = new AbortController();
  browserCancel = cancellation;
  try {
    const callback = await browserAuthorization(client, {
      kind: action.kind, providerOrConnectionId: action.providerOrConnectionId, codeChallenge,
    }, cancellation.signal);
    assertLoginFlowCurrent(epoch);
    if ('error' in callback) throw new AuthApiError(callback.error, 0, 'Browser authorization did not complete');
    if (cancellation.signal.aborted) throw new AuthApiError('USER_CANCELLED', 0, 'Browser authorization was cancelled');
    const outcome = await client.exchangeAuthorizationCode(callback.code, codeVerifier);
    if (cancellation.signal.aborted) throw new AuthApiError('USER_CANCELLED', 0, 'Browser authorization was cancelled');
    return acceptLoginOutcome(outcome, epoch);
  } finally {
    if (browserCancel === cancellation) browserCancel = null;
  }
}

// 第一批只有 global 一个区域：企业发现落在别的区域时按“未找到”处理，不进区域确认。
async function discoverOrgSameRegion(client, org) {
  const discovery = await client.discoverSsoOrg(org);
  if (discovery.region !== AUTH_REGION) throw new AuthApiError('ORG_SSO_NOT_FOUND', 404, 'Organization is in another region');
  return discovery;
}

async function runLoginAction(action) {
  const epoch = loginFlowEpoch;
  const client = createClient();
  const stateBeforeAction = loginFlowState?.step === 'error' ? null : loginFlowState;
  try {
    if (action.type === 'cancel-browser') throw new AuthApiError('INVALID_AUTH_ACTION', 400, 'Unexpected browser cancellation');
    if (action.type === 'reset') {
      // 云端模式下冷启动续期还没成功：先重试一次，避免让用户用新登录替换仍然有效的会话。
      if (getAppMode() === 'cloud' && !accessToken) {
        refreshBackoff.release(authStateEpoch);
        await refresh();
        assertLoginFlowCurrent(epoch);
        if (accessToken && currentUser) return { success: true, state: loginFlowState ?? { step: 'entry' } };
      }
      return { success: true, state: await loadLoginProviders(epoch) };
    }
    if (action.type === 'confirm-sso-realm') {
      throw new AuthApiError('INVALID_AUTH_ACTION', 400, 'No enterprise region switch is waiting for confirmation');
    }
    if (action.type === 'cancel-sso-realm') {
      throw new AuthApiError('INVALID_AUTH_ACTION', 400, 'No enterprise region switch is waiting for cancellation');
    }
    if (!providerConfig) await loadLoginProviders(epoch);

    if (action.type === 'discover') {
      discoveredMethods = [];
      const { email, methods } = await discoverEmailLogin(action.email, {
        buildRegion: AUTH_REGION,
        discoverOrganization: (domain) => discoverOrgSameRegion(client, domain),
        discoverPersonal: (value) => client.discover(value),
      });
      assertLoginFlowCurrent(epoch);
      discoveredMethods = methods;
      loginFlowState = reduceAuthFlow(loginFlowState, { type: 'discovery-loaded', email, methods });
      return { success: true, state: loginFlowState };
    }
    if (action.type === 'discover-sso-org') {
      const discovery = await discoverOrgSameRegion(client, action.org.trim().toLowerCase());
      assertLoginFlowCurrent(epoch);
      discoveredMethods = ssoOrgDiscoveryToMethods(discovery);
      loginFlowState = reduceAuthFlow(loginFlowState, { type: 'discovery-loaded', email: '', methods: discoveredMethods });
      return { success: true, state: loginFlowState };
    }
    if (action.type === 'request-code') {
      if (action.kind === 'phone' && !providerConfig?.phone) {
        throw new AuthApiError('PHONE_LOGIN_DISABLED', 400, 'Phone login is disabled');
      }
      await assertCredentialStoreReady();
      await client.requestCode(action.kind, action.identifier, { captchaToken: action.captchaToken });
      assertLoginFlowCurrent(epoch);
      discoveredMethods = [];
      loginFlowState = reduceAuthFlow(loginFlowState, { type: 'code-requested', kind: action.kind, identifier: action.identifier });
      return { success: true, state: loginFlowState };
    }
    if (action.type === 'verify-code') {
      await assertCredentialStoreReady();
      const outcome = await client.verifyCode(action.kind, action.identifier, action.code);
      return { success: true, state: await acceptLoginOutcome(outcome, epoch) };
    }
    if (action.type === 'start-browser') {
      return { success: true, state: await startBrowser(client, action, epoch) };
    }
    if (action.type === 'select-account') {
      if (pendingAccountToken) {
        const pair = await client.exchangeAccountMembership(pendingAccountToken, action.accountId);
        assertLoginFlowCurrent(epoch);
        pendingAccountToken = null;
        return { success: true, state: await completeLogin({ status: 'ok', ...pair }, epoch) };
      }
      if (!pendingLoginTicket) throw new AuthApiError('INVALID_LOGIN_TICKET', 401, 'Missing login ticket');
      const outcome = await client.selectAccount(pendingLoginTicket, action.accountId);
      return { success: true, state: await acceptLoginOutcome(outcome, epoch) };
    }
    if (action.type === 'request-sso-verification-code' || action.type === 'verify-sso-verification') {
      if (!pendingSsoVerificationTicket || loginFlowState?.step !== 'sso-verification') {
        throw new AuthApiError('INVALID_SSO_VERIFICATION_TICKET', 401, 'Missing SSO verification ticket');
      }
      if (action.type === 'verify-sso-verification') {
        const outcome = await client.verifySsoVerification(pendingSsoVerificationTicket, action.code);
        return { success: true, state: await acceptLoginOutcome(outcome, epoch) };
      }
      await client.requestSsoVerificationCode(pendingSsoVerificationTicket);
      assertLoginFlowCurrent(epoch);
      loginFlowState = reduceAuthFlow(loginFlowState, {
        type: 'sso-verification-code-requested', channel: loginFlowState.channel, targetMasked: loginFlowState.targetMasked,
      });
      return { success: true, state: loginFlowState };
    }
    // request-binding-code / verify-binding
    if (!pendingBindTicket || loginFlowState?.step !== 'binding') {
      throw new AuthApiError('INVALID_BIND_TICKET', 401, 'Missing binding ticket');
    }
    if (action.type === 'request-binding-code') {
      await client.requestBindingCode(pendingBindTicket, loginFlowState.bindType, action.contact);
      assertLoginFlowCurrent(epoch);
      loginFlowState = reduceAuthFlow(loginFlowState, { type: 'binding-code-requested', bindType: loginFlowState.bindType, contact: action.contact });
      return { success: true, state: loginFlowState };
    }
    const outcome = await client.verifyBinding(pendingBindTicket, loginFlowState.bindType, action.contact, action.code);
    return { success: true, state: await acceptLoginOutcome(outcome, epoch) };
  } catch (error) {
    if (loginFlowEpoch !== epoch) return { success: false, code: 'AUTH_FLOW_SUPERSEDED', state: loginFlowState };
    const code = error instanceof AuthApiError ? error.code : 'AUTH_REQUEST_FAILED';
    const status = error instanceof AuthApiError ? error.statusCode : 0;
    log('warn', `login action failed action=${action.type} status=${status} code=${code}`);
    if (!(error instanceof AuthApiError)) log('warn', `login action error: ${error?.message ?? error}`);
    const cannotRetry = FLOW_CANNOT_RETRY.has(code);
    if (cannotRetry) {
      pendingAccountToken = null;
      pendingLoginTicket = null;
      pendingBindTicket = null;
      pendingSsoVerificationTicket = null;
    }
    const errorState = loginPreparingErrorState(code, error instanceof AuthApiError ? error.retryAt : undefined);
    loginFlowState = cannotRetry || code === 'CREDENTIAL_STORE_UNAVAILABLE' ? errorState : (stateBeforeAction ?? errorState);
    return { success: false, code, state: loginFlowState, ...(errorState.retryAt !== undefined ? { retryAt: errorState.retryAt } : {}) };
  }
}

async function dispatchLoginAction(raw) {
  const action = parseDesktopLoginAction(raw);
  if (!action) return { success: false, code: 'INVALID_AUTH_ACTION', state: loginFlowState };
  if (action.type === 'back') {
    if (commitSealed) return { success: false, code: 'LOGIN_BUSY', state: loginFlowState };
    const providers = providerConfig;
    loginFlowEpoch += 1;
    cancelBrowser();
    resetLoginFlowState();
    if (providers) {
      providerConfig = providers;
      loginFlowState = reduceAuthFlow(null, { type: 'providers-loaded', providers });
      return { success: true, state: loginFlowState };
    }
    return { success: true, state: { step: 'entry' } };
  }
  if (action.type === 'cancel-browser') {
    const pending = loginActionPromiseEpoch === loginFlowEpoch ? loginActionPromise : null;
    const cancelled = cancelBrowser();
    if (!cancelled && !pending) return { success: false, code: 'NO_BROWSER_AUTH_IN_PROGRESS', state: loginFlowState };
    const settled = pending ? await pending : null;
    return { success: true, state: settled?.state ?? loginFlowState ?? (await loadLoginProviders()) };
  }
  if (loginActionPromise && loginActionPromiseEpoch === loginFlowEpoch) {
    return { success: false, code: 'LOGIN_BUSY', state: loginFlowState };
  }
  const run = runLoginAction(action);
  loginActionPromise = run;
  loginActionPromiseEpoch = loginFlowEpoch;
  try {
    return await run;
  } finally {
    if (loginActionPromise === run) {
      loginActionPromise = null;
      loginActionPromiseEpoch = null;
    }
  }
}

// ── 退出 / 本机模式 ─────────────────────────────────────────────────────────

async function logout() {
  if (getAppMode() !== 'cloud' || !currentUser) throw ipcError('UNAUTHENTICATED', 'No current account to log out');
  const token = accessToken;
  authStateEpoch += 1;
  loginFlowEpoch += 1;
  cancelBrowser();
  clearSessionMemory();
  // 服务端注销尽力而为、限时；本地凭据无论如何都清掉。
  if (token) {
    const client = createClient();
    await withGate(client.logout(token), LOGOUT_REQUEST_TIMEOUT_MS, () => new Error('logout request timed out'))
      .catch((err) => log('warn', `server logout failed: ${err?.code ?? err?.message ?? err}`));
  }
  try { await removeSecret(AUTH_SESSION_KEY); } catch (err) { log('warn', `remove auth session failed: ${err?.message ?? err}`); }
  commitAppSession('signed-out');
  resetLoginFlowState();
  providerConfig = null;
  notifyRenderer();
  notifyListeners();
}

function enterLocal() {
  if (getAppMode() === 'local') return snapshotAuthState();
  const wasCloud = getAppMode() === 'cloud';
  authStateEpoch += 1;
  loginFlowEpoch += 1;
  cancelBrowser();
  clearSessionMemory();
  commitAppSession('local');
  resetLoginFlowState();
  notifyRenderer();
  if (wasCloud) notifyListeners();
  return snapshotAuthState();
}

function exitLocal() {
  if (getAppMode() !== 'local') throw ipcError('PRECONDITION_FAILED', 'Local mode is not active.');
  commitAppSession('signed-out');
  notifyRenderer();
  return snapshotAuthState();
}

// ── IPC ─────────────────────────────────────────────────────────────────────

const unsupportedDeletion = async () => ({ success: false, code: 'ACCOUNT_DELETION_UNAVAILABLE' });
const unsupportedAccounts = async () => { throw ipcError('NOT_SUPPORTED', 'Multiple accounts are not supported in the Tauri build yet'); };

export function registerAuthIpc({ dataDir }) {
  deviceId = loadDeviceId(dataDir);
  registerInvoke('auth:initialize', () => initialize());
  registerInvoke('auth:get-login-state', () => getLoginState());
  registerInvoke('auth:dispatch-login-action', (action) => dispatchLoginAction(action));
  registerInvoke('auth:get-captcha-challenge-url', async () => authBaseUrl() + CAPTCHA_CHALLENGE_PAGE_PATH);
  registerInvoke('auth:logout', () => logout());
  registerInvoke('auth:refresh', () => refresh());
  registerInvoke('auth:enter-local', async () => enterLocal());
  registerInvoke('auth:exit-local', async () => exitLocal());
  registerSync('auth:has-persisted-session-hint-sync', () => getAppMode() === 'cloud');
  // 第二批：多账号、注销账号。先返回“只有当前账号、不可变更”的空形状，界面不报错。
  registerInvoke('auth:accounts:list', async () => ({ accounts: [], mutationAllowed: false }));
  registerInvoke('auth:accounts:sync', async () => ({ accounts: [], mutationAllowed: false }));
  registerInvoke('auth:accounts:switch', unsupportedAccounts);
  registerInvoke('auth:accounts:begin-add', unsupportedAccounts);
  registerInvoke('auth:accounts:cancel-add', async () => undefined);
  registerInvoke('auth:account-deletion:get-availability', unsupportedDeletion);
  registerInvoke('auth:account-deletion:request-challenge', unsupportedDeletion);
  registerInvoke('auth:account-deletion:confirm', unsupportedDeletion);
  registerInvoke('auth:account-deletion:get-status', async () => ({ success: true, value: null }));
  registerInvoke('auth:account-deletion:clear-receipt', async () => undefined);
  registerInvoke('auth:account-deletion:consume-restored-notice', async () => false);
  return { dispose: () => { clearRefreshTimer(); cancelBrowser(); } };
}
