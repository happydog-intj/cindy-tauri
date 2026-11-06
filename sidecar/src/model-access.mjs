// Cindy AI 云端模型：登录后的网关凭据、模型目录、XD provider 投影、余额。
// 对齐 desktop main/model-access/index.ts + maker-host/active-catalog.ts 的 xd 部分 + billing 两个余额通道，
// 只保留单账号、global、Pi 需要的部分：
// - 凭据状态机直接用 desktop credentialsSync.ts（白名单文件）：登录后拉 {endpoint, apiKey}，
//   key 写 secret store 的 api_key（与 desktop 同名，renderer 可读，用户已确认），
//   endpoint 与来源标记写 <dataDir>/model-access-credentials.json；
// - 凭据就绪后拉 /api/model-access/models（v5），投影成 xd provider 的 Pi 模型清单；
//   拉取失败保留上次清单里可执行的部分，成功的空清单清空模型；
// - Pi 的 cindy 网关块指向本地转发（gateway-forwarder.mjs），真实 key 只在转发时注入；
// - 退出云端模式（登出 / 会话失效 / 进入本机模式）时删掉网关 key，下次登录重新下发。
//   desktop 按账号分目录所以不删；Tauri 只有一份本地数据，留着没有用处。
import { existsSync, readFileSync, renameSync, rmSync, writeFileSync } from 'node:fs';
import path from 'node:path';
import {
  BUNDLED_CATALOG,
  clampEffortToSupported,
  defaultEffortForCapabilities,
  resolveModelNativeApi,
} from '@cindy/model-providers';
import { effectiveGatewayModelCost } from '@cindy-desktop-shared/modelPriceQuote';
import { MODEL_ACCESS_STATUS_CHANNEL } from '@cindy-desktop-shared/modelAccess';
import { createCredentialsSync } from '@cindy-desktop-main/model-access/credentialsSync.ts';
import {
  buildModelsSyncRequest,
  modelsWithoutStalePaymentUpsell,
  parseModelsSyncPayload,
  withModelsSyncOverallDeadline,
} from '@cindy-desktop-main/model-access/modelsSyncRefresh.ts';
import {
  isCatalogPiGatewayModelRetired,
  resolveBundledPiGatewayModelProfile,
  resolveCatalogPiGatewayModelApi,
} from '@cindy-desktop-main/maker-host/pi-gateway-model-catalog.ts';
import { projectModelAccessBalance, projectModelAccessCreditUsage } from '@cindy-desktop-main/billing/projection.ts';
import { AUTH_REGION, modelAccessBaseUrl } from './cloud-endpoints.mjs';
import { authInfo, expireSession, getAccessToken, onAuthChange, refresh as refreshAuth } from './auth.mjs';
import { emitToRenderer, registerInvoke } from './ipc.mjs';
import { getMainLocale } from './main-locale.mjs';
import { getAppMode } from './owner.mjs';
import { getSecretSync, hasSecret, onSecretChange, removeSecret, setSecret } from './secret-store.mjs';
import { startGatewayForwarder } from './gateway-forwarder.mjs';

export const XD_PROVIDER_ID = 'xd';
// 与 desktop shared/providerSecrets.ts STORAGE_KEYS.xd 一致。
export const XD_KEY_NAME = 'api_key';
const CREDENTIALS_PATH = '/api/model-access/credentials';
const BALANCE_PATH = '/api/model-access/balance';
const CREDIT_USAGE_PATH = '/api/model-access/credit-usage';
const API_FETCH_TIMEOUT_MS = 15_000;
const BILLING_REQUEST_TIMEOUT_MS = 20_000;
const FOREGROUND_REFRESH_INTERVAL_MS = 5 * 60_000;
const KEY_REJECTION_RESYNC_INTERVAL_MS = 5 * 60_000;
const PI_APIS = new Set(['anthropic-messages', 'openai-responses', 'openai-completions', 'google-generative-ai']);
// desktop pi-host.ts GATEWAY_UNSUPPORTED_ANTHROPIC_COMPAT：网关的 Messages schema 还不接受这几项。
const GATEWAY_UNSUPPORTED_ANTHROPIC_COMPAT = ['supportsStrictTools', 'supportsMidConvoToolChanges', 'supportsMidConvoEffort'];
// desktop pi-host.ts PI_GATEWAY_DEEPSEEK_REASONING_REPLAY_MODELS。
const DEEPSEEK_REASONING_REPLAY_MODELS = new Set([
  'deepseek/deepseek-v4.1-flash',
  'deepseek/deepseek-flash',
  'deepseek/deepseek-v4-pro',
  'deepseek/deepseek-v4-flash-vision-exp',
  'deepseek/deepseek-v4-flash',
]);

function log(level, msg) {
  process.stderr.write(`[model-access] ${level}: ${msg}\n`);
}

function ipcError(code, message) {
  return new Error(`[${code}] ${message}`);
}

// ── 服务端请求（desktop serverApiClient.ts 的精简版）─────────────────────────────

export class ServerApiError extends Error {
  constructor(code, statusCode, message, retryAt) {
    super(message);
    this.name = 'ServerApiError';
    this.code = code;
    this.statusCode = statusCode;
    if (retryAt !== undefined) this.retryAt = retryAt;
  }
}

function statusToCode(status) {
  if (status === 0) return 'NETWORK_ERROR';
  if (status === 401) return 'UNAUTHORIZED';
  if (status === 429) return 'RATE_LIMITED';
  if (status >= 500) return 'INTERNAL_ERROR';
  return `HTTP_${status}`;
}

function readError(data) {
  const err = data && typeof data === 'object' ? data.error : null;
  return {
    code: err && typeof err.code === 'string' ? err.code : null,
    message: err && typeof err.message === 'string' ? err.message : null,
  };
}

function isRefreshableUnauthorized(code) {
  return code === 'TOKEN_EXPIRED' || code === 'INVALID_TOKEN' || code === 'UNAUTHORIZED';
}

function redactedPath(apiPath) {
  const segments = apiPath.split(/[?#]/, 1)[0].split('/').filter(Boolean).slice(0, 3);
  return segments.length ? `/${segments.join('/')}` : '/';
}

async function rawFetch(apiPath, opts) {
  const headers = {
    'Content-Type': 'application/json',
    'Accept-Language': getMainLocale(),
    ...(opts.headers ?? {}),
  };
  const token = getAccessToken();
  if (token) headers.Authorization = `Bearer ${token}`;
  const timeoutMs = opts.timeoutMs ?? API_FETCH_TIMEOUT_MS;
  const controller = new AbortController();
  const timer = timeoutMs > 0 ? setTimeout(() => controller.abort(), timeoutMs) : null;
  try {
    const response = await fetch(modelAccessBaseUrl() + apiPath, {
      method: opts.method ?? 'GET',
      headers,
      ...(opts.body !== undefined ? { body: JSON.stringify(opts.body) } : {}),
      cache: 'no-store',
      signal: controller.signal,
    });
    let data = null;
    try { data = await response.json(); } catch { data = null; }
    return { ok: response.ok, status: response.status, data };
  } catch (err) {
    log('warn', `request failed path=${redactedPath(apiPath)} error=${err?.name ?? 'Error'}`);
    return { ok: false, status: 0, data: null };
  } finally {
    if (timer) clearTimeout(timer);
  }
}

/** 成功返回 JSON；失败抛 ServerApiError。可恢复的 401 自动续期一次再重试。 */
async function apiFetch(apiPath, opts = {}) {
  let result = await rawFetch(apiPath, opts);
  let retried = false;
  const first = readError(result.data).code ?? statusToCode(result.status);
  if (result.status === 401 && first !== 'ACCOUNT_UNAVAILABLE' && isRefreshableUnauthorized(first)) {
    if (await refreshAuth()) {
      retried = true;
      result = await rawFetch(apiPath, opts);
    }
  }
  if (!result.ok) {
    const { code, message } = readError(result.data);
    const errCode = code ?? statusToCode(result.status);
    const publicCode = opts.redactErrorDetails ? statusToCode(result.status) : errCode;
    if (result.status === 401 && (errCode === 'ACCOUNT_UNAVAILABLE' || (retried && isRefreshableUnauthorized(errCode)))) {
      void expireSession(errCode === 'ACCOUNT_UNAVAILABLE' ? 'account-unavailable' : 'unknown');
    }
    log('warn', `not ok path=${redactedPath(apiPath)} status=${result.status} code=${publicCode}`);
    throw new ServerApiError(
      publicCode,
      result.status,
      opts.redactErrorDetails ? `请求失败 (${result.status})` : (message ?? `请求失败 (${result.status})`),
    );
  }
  if (result.data === null) throw new ServerApiError('EMPTY_RESPONSE', result.status, '响应体为空');
  return result.data;
}

// ── 凭据元数据（desktop credentialsStore.ts 的文件版）──────────────────────────────

let credentialsPath = null;
let credentialsState = null;

function loadCredentialsState() {
  credentialsState = null;
  if (!existsSync(credentialsPath)) return;
  try {
    const raw = JSON.parse(readFileSync(credentialsPath, 'utf8'));
    if (raw?.source === 'server' && typeof raw.endpoint === 'string') credentialsState = { source: 'server', endpoint: raw.endpoint };
    else if (raw?.source === 'manual') credentialsState = { source: 'manual' };
  } catch (err) {
    log('warn', `model-access-credentials.json unreadable (${err.message}); ignoring`);
  }
}

function persistCredentialsState(next) {
  credentialsState = next;
  try {
    if (next === null) {
      rmSync(credentialsPath, { force: true });
    } else {
      const tmp = `${credentialsPath}.tmp-${process.pid}`;
      writeFileSync(tmp, JSON.stringify(next), { mode: 0o600 });
      renameSync(tmp, credentialsPath);
    }
  } catch (err) {
    // 只影响下次冷启动首轮同步前的 endpoint 记忆，不阻断主流程。
    log('warn', `persist model-access credentials meta failed: ${err.message}`);
  }
}

const credentialsStore = {
  getSource: () => credentialsState?.source ?? null,
  getServerEndpoint: () => (credentialsState?.source === 'server' ? credentialsState.endpoint ?? null : null),
  setServerCredentials: (endpoint) => persistCredentialsState({ source: 'server', endpoint: endpoint.replace(/\/+$/, '') }),
  setManualSource: () => persistCredentialsState({ source: 'manual' }),
  clear: () => persistCredentialsState(null),
};

// ── 模型目录与 xd provider 投影 ──────────────────────────────────────────────

let gatewayModels = [];
let projectedPiModels = [];
let accountTier = null;
const changeListeners = new Set();

function notifyChange() {
  for (const fn of changeListeners) {
    try { fn(); } catch (err) { log('warn', `change listener failed: ${err?.message ?? err}`); }
  }
}

/** xd provider 或其模型清单变化（登录 / 登出 / 目录刷新 / key 变化）。返回取消订阅函数。 */
export function onXdChange(fn) {
  changeListeners.add(fn);
  return () => changeListeners.delete(fn);
}

function normalizeModelId(modelId) {
  return String(modelId).replace(/\[1m\]$/, '');
}

function nativeApiForRoute(modelId) {
  const resolved = resolveModelNativeApi(BUNDLED_CATALOG.modelRegistry, XD_PROVIDER_ID, modelId);
  if (resolved == null) return resolved;
  return PI_APIS.has(resolved) ? resolved : undefined;
}

function hintApi(gm) {
  const api = gm.perAgent?.pi?.wireProtocol;
  return PI_APIS.has(api) ? api : null;
}

// desktop resolveXdPiGatewayModelApi：Cindy 目录 > 随版本的 Pi 目录 > 网关提示。
// Tauri 没有下载的服务端目录，用随包目录做权威目录；也没有 Pi 二进制探测。
function resolvePiApi(gm) {
  if (isCatalogPiGatewayModelRetired(BUNDLED_CATALOG, gm.id)) return null;
  const nativeApi = nativeApiForRoute(gm.id);
  if (nativeApi) return nativeApi;
  const declared = resolveCatalogPiGatewayModelApi(BUNDLED_CATALOG, gm.id);
  if (declared !== undefined) return declared === null || PI_APIS.has(declared) ? declared : null;
  if ((BUNDLED_CATALOG.modelRegistry?.schemaVersion ?? 0) >= 3 || nativeApi === null) return hintApi(gm);
  const local = resolveBundledPiGatewayModelProfile(gm.id)?.api;
  if (local !== undefined) return PI_APIS.has(local) ? local : null;
  return hintApi(gm);
}

function isChatMode(mode) {
  return !mode || mode === 'chat' || mode === 'responses';
}

// desktop active-catalog.ts 的 xd 投影（只投 Pi）。解析不出 Pi 协议的模型不进清单：
// Tauri 没有启动后探测，留着只会选中后启动失败。
function projectPiModels(models) {
  const out = [];
  for (const gm of models) {
    if (!Array.isArray(gm.agents) || !gm.agents.includes('pi')) continue;
    const piApi = resolvePiApi(gm);
    if (!piApi) continue;
    const ov = gm.perAgent?.pi ?? {};
    const efforts = ov.efforts ?? gm.efforts ?? [];
    const intent = ov.defaultEffort ?? gm.defaultEffort ?? defaultEffortForCapabilities(efforts);
    const defaultEffort = efforts.length === 0 ? null : (clampEffortToSupported(intent, efforts) ?? null);
    const nativeApi = nativeApiForRoute(gm.id);
    const cost = effectiveGatewayModelCost(gm);
    const defaultEnabled = ov.defaultEnabled ?? gm.defaultEnabled;
    const supportsFastMode = ov.supportsFastMode ?? gm.supportsFastMode;
    out.push({
      ...(gm.displayPricing && gm.currency
        ? { displayPricing: gm.displayPricing, currency: gm.currency, ...(gm.costDiscount !== undefined ? { costDiscount: gm.costDiscount } : {}) }
        : {}),
      id: gm.id,
      ...(nativeApi !== undefined ? { nativeApi } : {}),
      ...(gm.availability ? { availability: gm.availability } : {}),
      name: gm.name,
      ...(gm.group !== undefined ? { group: gm.group } : {}),
      contextWindow: ov.contextWindow ?? gm.contextWindow,
      contextWindowMax: gm.contextWindow,
      ...(gm.maxOutputTokens !== undefined ? { maxOutput: gm.maxOutputTokens } : {}),
      contextWindowVerified: true,
      efforts,
      defaultEffort,
      ...(supportsFastMode !== undefined ? { supportsFastMode } : {}),
      ...(gm.mode !== undefined ? { mode: gm.mode } : {}),
      ...(gm.description !== undefined ? { description: gm.description } : {}),
      ...(gm.sortOrder !== undefined ? { sortOrder: gm.sortOrder } : {}),
      ...(defaultEnabled !== undefined ? { defaultEnabled } : {}),
      ...(gm.newSessionDefault !== undefined ? { newSessionDefault: gm.newSessionDefault } : {}),
      ...(gm.icon !== undefined ? { icon: gm.icon } : {}),
      ...(cost ? { cost } : {}),
      ...(gm.modalities !== undefined ? { modalities: gm.modalities } : {}),
      piApi,
    });
  }
  // desktop selectDefaultModels：需付费或非对话模型默认收起。
  const visible = new Set(models
    .filter((m) => m.availability !== 'requires_payment' && isChatMode(m.mode))
    .map((m) => m.id));
  return out
    .map((m, index) => ({ m: isChatMode(m.mode) && m.defaultEnabled !== false && !visible.has(m.id) ? { ...m, defaultEnabled: false } : m, index }))
    .sort((a, b) => (a.m.sortOrder ?? Number.MAX_SAFE_INTEGER) - (b.m.sortOrder ?? Number.MAX_SAFE_INTEGER) || a.index - b.index)
    .map(({ m }) => m);
}

function applyGatewayModels(models) {
  gatewayModels = models;
  projectedPiModels = projectPiModels(models);
  notifyChange();
}

/** CatalogModel → maker-core ModelDescriptor（desktop catalog-to-descriptors.ts toDescriptor）。 */
export function catalogModelToDescriptor(m) {
  const d = {
    id: m.id,
    displayName: m.name,
    contextWindow: m.contextWindow,
    efforts: m.efforts,
    defaultEffort: m.defaultEffort,
    effortsUnknown: m.effortsUnknown === true,
  };
  if (m.description !== undefined) d.description = m.description;
  if (m.effortDisplayNames !== undefined) d.effortDisplayNames = m.effortDisplayNames;
  if (m.supportsFastMode !== undefined) d.supportsFastMode = m.supportsFastMode;
  if (m.supportsUltrafastMode !== undefined) d.supportsUltrafastMode = m.supportsUltrafastMode;
  if (m.group !== undefined) d.group = m.group;
  if (m.sortOrder !== undefined) d.sortOrder = m.sortOrder;
  if (m.mode !== undefined) d.mode = m.mode;
  if (m.defaultEnabled !== undefined) d.defaultEnabled = m.defaultEnabled;
  if (m.newSessionDefault !== undefined) d.newSessionDefault = m.newSessionDefault;
  if (m.cost !== undefined) d.cost = m.cost;
  if (m.maxOutput !== undefined) d.maxOutputTokens = m.maxOutput;
  const supportsImageInput = m.supportsImageInput ?? (m.modalities !== undefined ? m.modalities.input.includes('image') : undefined);
  if (supportsImageInput !== undefined) d.supportsImageInput = supportsImageInput;
  return d;
}

/** 网关 key 与配套 endpoint 都在、且处于云端模式。 */
export function xdConnected() {
  return getAppMode() === 'cloud' && hasSecret(XD_KEY_NAME) && credentialsStore.getServerEndpoint() !== null;
}

/** 目录里的 xd provider（Provider 形状）；不在云端模式返回 null（desktop 本机模式同样不列 Cindy AI）。 */
export function getXdProvider() {
  if (getAppMode() !== 'cloud') return null;
  const bundled = BUNDLED_CATALOG.providers.find((p) => p.id === XD_PROVIDER_ID);
  if (!bundled) return null;
  const { embeddingModels: _e, embeddingDefaults: _d, imageModels: _i, videoModels: _v, audioModels: _a, ...rest } = bundled;
  return {
    ...rest,
    agents: ['pi'],
    routing: { pi: bundled.routing.pi },
    models: { pi: projectedPiModels },
  };
}

function findPiModel(modelId) {
  const id = normalizeModelId(modelId);
  return projectedPiModels.find((m) => m.id === id) ?? null;
}

/** Pi 网关块用：xd 模型的描述符；不是 xd 成员返回 undefined（maker-core 回落到公开描述符）。 */
export function resolveXdDescriptor(modelId) {
  const m = findPiModel(modelId);
  return m ? catalogModelToDescriptor(m) : undefined;
}

/** (xd, model) → 会话运行配置；未连接、不在清单或需付费返回 null。 */
export function resolveXdRunConfig(modelId) {
  if (!xdConnected()) return null;
  const m = findPiModel(modelId);
  if (!m || m.availability === 'requires_payment') return null;
  return { providerId: XD_PROVIDER_ID, providerName: 'Cindy AI', modelId: m.id, gateway: true, reasoning: m.efforts.length > 0 };
}

/** 第一个可选的 xd 模型（没有自定义 provider 时的默认）。 */
export function defaultXdRunConfig() {
  if (!xdConnected()) return null;
  const m = projectedPiModels.find((x) => x.availability !== 'requires_payment' && isChatMode(x.mode) && x.defaultEnabled !== false)
    ?? projectedPiModels.find((x) => x.availability !== 'requires_payment' && isChatMode(x.mode));
  return m ? resolveXdRunConfig(m.id) : null;
}

/** desktop resolvePiCindyGatewayModelSpec 的精简版（无二进制探测）。非 xd 成员返回 undefined。 */
export function resolvePiGatewayModelSpec(_providerId, modelId) {
  const id = normalizeModelId(modelId);
  const gm = gatewayModels.find((m) => m.id === id);
  if (!gm?.agents?.includes('pi')) return undefined;
  const api = resolvePiApi(gm);
  if (api == null) return api;
  const bundled = resolveBundledPiGatewayModelProfile(id);
  const profile = bundled?.api === api ? bundled : undefined;
  let compat = profile?.compat;
  if (api === 'openai-completions' && DEEPSEEK_REASONING_REPLAY_MODELS.has(id) && compat?.requiresReasoningContentOnAssistantMessages === undefined) {
    compat = { ...compat, requiresReasoningContentOnAssistantMessages: true };
  }
  if (api === 'anthropic-messages' && compat && GATEWAY_UNSUPPORTED_ANTHROPIC_COMPAT.some((k) => k in compat)) {
    compat = Object.fromEntries(Object.entries(compat).filter(([k]) => !GATEWAY_UNSUPPORTED_ANTHROPIC_COMPAT.includes(k)));
  }
  if (api === 'openai-completions' || api === 'anthropic-messages') compat = { ...compat, sendSessionAffinityHeaders: true };
  return {
    api,
    ...(compat ? { compat: structuredClone(compat) } : {}),
    ...(profile?.samplingParams ? { samplingParams: structuredClone(profile.samplingParams) } : {}),
    ...(profile?.thinkingLevelMap ? { thinkingLevelMap: { ...profile.thinkingLevelMap } } : {}),
  };
}

// ── 本地转发 ────────────────────────────────────────────────────────────────

let forwarder = null;
let lastRejection = { key: null, at: 0 };

/** Pi 网关块的 baseUrl（本地转发）。 */
export function gatewayEndpoint() {
  return forwarder?.url();
}

export function registerPiProxySession(sessionId, token, resolveProviderId, options) {
  if (!forwarder) throw new Error('Cindy AI gateway forwarder is not running');
  return forwarder.registerSession(sessionId, token, resolveProviderId, options);
}

function currentUpstream() {
  if (!xdConnected()) return null;
  const key = getSecretSync(XD_KEY_NAME)?.trim();
  const endpoint = credentialsStore.getServerEndpoint();
  return key && endpoint ? { key, endpoint } : null;
}

// ── 状态机装配 ───────────────────────────────────────────────────────────────

let sync = null;
let authGeneration = 0;
let lastAuthUserId = null;
let writingXdKey = false;
let modelsFlight = null;
let modelsFlightGen = -1;
let modelsAttempt = 0;
let lastModelsSucceededAttempt = 0;
let lastModelsStartedAt = 0;

function statusPayload() {
  return { ...sync.getStatus(), accountTier };
}

function broadcastStatus() {
  emitToRenderer(MODEL_ACCESS_STATUS_CHANNEL, statusPayload());
}

function setAccountTier(next) {
  if (accountTier === next) return;
  accountTier = next;
  broadcastStatus();
}

async function writeXdKey(key) {
  if (getAppMode() !== 'cloud') return false;
  writingXdKey = true;
  try {
    await setSecret(XD_KEY_NAME, key);
    return true;
  } catch (err) {
    log('warn', `save gateway key failed: ${err?.message ?? err}`);
    return false;
  } finally {
    writingXdKey = false;
  }
}

async function clearXdKey() {
  writingXdKey = true;
  try {
    await removeSecret(XD_KEY_NAME);
  } catch (err) {
    log('warn', `remove gateway key failed: ${err?.message ?? err}`);
  } finally {
    writingXdKey = false;
  }
  credentialsStore.clear();
}

async function runModelsSync(gen, attempt) {
  const previous = gatewayModels;
  const failed = (why) => {
    log('warn', `xd gateway models ${why} (keeping executable last valid list)`);
    if (gen !== authGeneration) return;
    setAccountTier(null);
    applyGatewayModels(modelsWithoutStalePaymentUpsell(previous));
  };
  let data;
  try {
    const { path: apiPath, options } = buildModelsSyncRequest(modelAccessBaseUrl);
    data = await withModelsSyncOverallDeadline(apiFetch(apiPath, { timeoutMs: options.timeoutMs, headers: options.headers }));
  } catch (err) {
    failed(`fetch failed: ${err?.code ?? err?.message ?? err}`);
    return;
  }
  if (gen !== authGeneration) return;
  const parsed = parseModelsSyncPayload(data);
  if (!parsed.ok) {
    failed(`response rejected: ${parsed.error}`);
    return;
  }
  applyGatewayModels(parsed.models);
  setAccountTier(parsed.accountTier);
  lastModelsSucceededAttempt = attempt;
  log('info', `xd gateway models synced: ${parsed.models.length} total, ${projectedPiModels.length} for pi`);
}

/** 同世代 single-flight；旧世代在途时等它结束再为当前账号补发。返回本次所等的 flight。 */
function scheduleModelsSync() {
  if (!lastAuthUserId) return null;
  const gen = authGeneration;
  if (modelsFlight) {
    if (modelsFlightGen === gen) return modelsFlight;
    return modelsFlight.then(() => scheduleModelsSync());
  }
  modelsFlightGen = gen;
  const attempt = ++modelsAttempt;
  lastModelsStartedAt = Date.now();
  const run = runModelsSync(gen, attempt)
    .catch((err) => log('warn', `xd gateway models sync threw: ${err?.message ?? err}`))
    .finally(() => { if (modelsFlight === run) modelsFlight = null; });
  modelsFlight = run;
  return run;
}

function requireCloud() {
  if (getAppMode() !== 'cloud') throw ipcError('PERMISSION_DENIED', 'Cindy AI requires a Cindy account.');
}

/** 设置页 / 模型选择器的显式刷新（maker:provider:models-refresh xd）：凭据没就绪先重试，再等一次新的 /models。 */
export async function refreshXdGatewayModels() {
  requireCloud();
  const status = sync.getStatus().state === 'ok' ? sync.getStatus() : await sync.retry();
  if (status.state !== 'ok') throw ipcError('MODEL_ACCESS_FAILED', 'Cindy AI credentials are not ready.');
  const gen = authGeneration;
  const minimum = modelsAttempt + 1;
  for (;;) {
    const flight = scheduleModelsSync();
    if (!flight) throw ipcError('MODEL_ACCESS_FAILED', 'Cindy AI model list refresh did not start.');
    const attempt = modelsAttempt;
    await flight;
    if (gen !== authGeneration) throw ipcError('MODEL_ACCESS_FAILED', 'Cindy AI account changed during model list refresh.');
    if (attempt < minimum) continue;
    if (lastModelsSucceededAttempt !== attempt) throw ipcError('MODEL_ACCESS_FAILED', 'Cindy AI model list refresh failed.');
    return;
  }
}

/** 代替 desktop 的窗口聚焦刷新：打开模型选择器 / 设置页时最多每 5 分钟拉一次。 */
export function foregroundRefreshXdModels() {
  if (!lastAuthUserId || !sync || sync.getStatus().state !== 'ok') return;
  if (Date.now() - lastModelsStartedAt < FOREGROUND_REFRESH_INTERVAL_MS) return;
  scheduleModelsSync();
}

function noteAuthState(info) {
  const userId = info.authenticated ? info.userId : null;
  if (userId !== lastAuthUserId) {
    authGeneration += 1;
    lastAuthUserId = userId;
  }
  sync.handleAuthChange({ isAuthenticated: info.authenticated, userId, realm: info.authenticated ? AUTH_REGION : null });
  if (getAppMode() !== 'cloud' && (hasSecret(XD_KEY_NAME) || credentialsStore.getSource() !== null)) {
    void clearXdKey().then(notifyChange);
  }
  notifyChange();
}

function mapServerError(err) {
  if (err?.statusCode === 503 || err?.code === 'MODEL_ACCESS_DISABLED') throw ipcError('MODEL_ACCESS_DISABLED', '模型访问服务未启用');
  if (err?.statusCode === 403 || err?.code === 'ORG_NOT_SUPPORTED') throw ipcError('MODEL_ACCESS_UNSUPPORTED', '当前企业未开通模型访问');
  throw ipcError('MODEL_ACCESS_FAILED', err instanceof Error ? err.message : String(err));
}

// ── 余额（desktop billing/index.ts 的 get-balance / get-credit-usage）────────────────

function requirePersonalAccount() {
  if (getAppMode() !== 'cloud') throw ipcError('PERMISSION_DENIED', 'Billing requires a personal Cindy account.');
  if (authInfo().membershipKind !== 'personal') throw ipcError('PERMISSION_DENIED', 'Billing is only available to personal accounts.');
}

function throwBalanceFetchError(err) {
  if (err instanceof ServerApiError) {
    if (err.statusCode === 401 || err.statusCode === 403) throw ipcError('PERMISSION_DENIED', 'balance request is not authorized');
    if (err.statusCode === 404) throw ipcError('NOT_FOUND', 'balance account is not provisioned');
    if (err.statusCode === 501) throw ipcError('UNSUPPORTED_CAPABILITY', 'balance is not supported for this account');
  }
  throw ipcError('MODEL_ACCESS_FAILED', 'balance service request failed');
}

function billingHandler(apiPath, projector, payloadMessage) {
  return async (...args) => {
    requirePersonalAccount();
    if (args.length > 0 && args[0] !== undefined) throw ipcError('INVALID_PARAMS', payloadMessage);
    let response;
    try {
      response = await apiFetch(apiPath, { timeoutMs: BILLING_REQUEST_TIMEOUT_MS, redactErrorDetails: true });
    } catch (err) {
      throwBalanceFetchError(err);
    }
    try {
      return projector(response);
    } catch {
      throw ipcError('INTERNAL', 'billing service response was invalid');
    }
  };
}

// ── 注册 ────────────────────────────────────────────────────────────────────

export function registerModelAccessIpc({ dataDir }) {
  credentialsPath = path.join(dataDir, 'model-access-credentials.json');
  loadCredentialsState();
  gatewayModels = [];
  projectedPiModels = [];
  accountTier = null;

  sync = createCredentialsSync({
    fetchCredentials: () => apiFetch(CREDENTIALS_PATH),
    rotateCredentials: () => apiFetch(`${CREDENTIALS_PATH}/rotate`, { method: 'POST' }),
    readXdKey: () => getSecretSync(XD_KEY_NAME),
    writeXdKey,
    store: credentialsStore,
    onStatusChange: (status) => {
      if (['failed', 'disabled', 'unsupported', 'idle'].includes(status.state)) accountTier = null;
      broadcastStatus();
      if (status.state === 'ok') {
        scheduleModelsSync();
        notifyChange();
      } else if (['failed', 'disabled', 'unsupported', 'idle'].includes(status.state)) {
        applyGatewayModels([]);
      }
    },
    log: { info: (m) => log('info', m), warn: (m) => log('warn', m) },
  });

  forwarder = startGatewayForwarder({
    getUpstream: currentUpstream,
    // 网关拒绝当前 key：同一个 key 5 分钟内最多静默重拉一次凭据。
    onRejected: (key) => {
      const now = Date.now();
      if (lastRejection.key === key && now - lastRejection.at < KEY_REJECTION_RESYNC_INTERVAL_MS) return;
      lastRejection = { key, at: now };
      log('warn', 'gateway rejected the current key; re-syncing credentials');
      void sync.resyncAfterKeyRejection().catch(() => undefined);
    },
  });

  const offAuth = onAuthChange(noteAuthState);
  // renderer 经通用 safe-storage 通道手填 / 删除网关 key 时，按 desktop 语义改来源标记。
  const offSecret = onSecretChange((name) => {
    if (name === null) { notifyChange(); return; }
    if (name !== XD_KEY_NAME || writingXdKey) {
      if (name === XD_KEY_NAME) notifyChange();
      return;
    }
    if (hasSecret(XD_KEY_NAME)) sync.noteManualKeySaved();
    else sync.noteManualKeyRemoved();
    notifyChange();
  });
  const initial = authInfo();
  if (initial.authenticated) noteAuthState(initial);
  else if (getAppMode() !== 'cloud' && (hasSecret(XD_KEY_NAME) || credentialsStore.getSource() !== null)) {
    // 上次退出云端模式时没来得及删（或钥匙串当时锁着），启动时补删。
    void clearXdKey().then(notifyChange);
  }

  registerInvoke('model-access:get-status', () => statusPayload());
  registerInvoke('model-access:retry', async () => {
    requireCloud();
    await sync.retry();
    return statusPayload();
  });
  registerInvoke('model-access:rotate', async () => {
    requireCloud();
    try {
      await sync.rotate();
    } catch (err) {
      mapServerError(err);
    }
    return statusPayload();
  });
  // 企业 BYOK 号池不在第一批范围：返回空闲、无 provider。
  registerInvoke('model-access:byok-status', () => ({ state: 'idle', providers: [] }));
  registerInvoke('model-access:byok-retry', () => ({ state: 'idle', providers: [] }));
  registerInvoke('billing:get-balance', billingHandler(BALANCE_PATH, projectModelAccessBalance, 'balance request does not accept a payload'));
  registerInvoke('billing:get-credit-usage', billingHandler(CREDIT_USAGE_PATH, projectModelAccessCreditUsage, 'credit usage request does not accept a payload'));

  return {
    ready: forwarder.ready,
    dispose() {
      offAuth();
      offSecret();
      void forwarder?.close();
    },
    _test: { forwarderSessions: () => forwarder?._test.sessionCount() ?? 0 },
  };
}
