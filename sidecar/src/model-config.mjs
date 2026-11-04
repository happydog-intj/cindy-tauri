// 模型配置：自定义 provider（OpenAI Chat Completions 兼容、无 key）的持久化与 renderer IPC。
// - 配置落 <dataDir>/providers.json，只含 id / 名称 / baseUrl / 模型清单，不含任何 key 或请求头；
// - 带 key / OAuth / 自定义请求头的 provider 本里程碑不支持（安全存储未接入），一律返回 [NOT_IMPLEMENTED]；
// - 环境变量 CINDY_TAURI_SMOKE_MODEL_BASE_URL / CINDY_TAURI_SMOKE_MODEL_ID 存在时注入只读的 "Smoke"
//   provider（不落盘）并作为默认；
// - ProviderView 由 @cindy/model-providers 的 buildUserProvider + buildRegistry 生成，与 desktop 同形状。
import { existsSync, readFileSync } from 'node:fs';
import { mkdir, rename, writeFile } from 'node:fs/promises';
import path from 'node:path';
import {
  buildRegistry,
  buildUserProvider,
  isModelSelectableForNewRoute,
  parseModelsListResponse,
} from '@cindy/model-providers';
import { emitToRenderer, registerInvoke, registerSync } from './ipc.mjs';

// 本机模式 owner，与 boot-stubs 的 authState 一致。
const DATA_OWNER_ID = 'local-v1';
const OWNER_GENERATION = 1;
const SMOKE_PROVIDER_ID = 'smoke';
const PROVIDER_CHANGED = 'maker:provider:changed';
const FETCH_TIMEOUT_MS = 10_000;
const AGENTS = ['claude-code', 'codex', 'pi'];
const BUILTIN_REFRESHABLE = ['xd', 'anthropic', 'openai', 'xai'];
const AUTO_REFRESH_TRIGGERS = ['providers-open', 'model-selector-open'];
const PROVIDER_ID_RE = /^[a-z0-9_-]{1,64}$/;
const MODEL_ID_RE = /^[A-Za-z0-9._:/-]{1,128}$/;
const RESERVED_IDS = new Set(['anthropic', 'openai', 'xd', 'xai', 'cindy', SMOKE_PROVIDER_ID]);
const MAX_MODELS = 512;
const MAX_ORDER_ITEMS = 256;

let storePath = null;
// 持久化部分：{ version, providers: CustomProviderConfig[], providerOrder: string[] }
let store = { version: 1, providers: [], providerOrder: [] };
// renderer 镜像过来的模型可见性 override（desktop 也只放内存）。
let visibilityMirror = {};
let writeQueue = Promise.resolve();

function ipcError(code, message) {
  return new Error(`[${code}] ${message}`);
}

// 与 index.mjs validateModel 同一规则：HTTPS 或回环 HTTP，不能内嵌凭据。返回去掉尾部斜杠的 href，非法返回 null。
export function normalizeBaseUrl(raw) {
  if (typeof raw !== 'string' || raw.length === 0 || raw.length > 4096) return null;
  let url;
  try { url = new URL(raw.trim()); } catch { return null; }
  const loopback = ['127.0.0.1', 'localhost', '[::1]'].includes(url.hostname);
  if (url.username || url.password || url.search || url.hash) return null;
  if (!(url.protocol === 'https:' || (url.protocol === 'http:' && loopback))) return null;
  return url.href.replace(/\/+$/, '');
}

function smokeProvider() {
  const rawUrl = process.env.CINDY_TAURI_SMOKE_MODEL_BASE_URL;
  const modelId = process.env.CINDY_TAURI_SMOKE_MODEL_ID;
  if (!rawUrl || !modelId) return null;
  const baseUrl = normalizeBaseUrl(rawUrl);
  if (!baseUrl || !MODEL_ID_RE.test(modelId)) {
    process.stderr.write('[model-config] smoke model env ignored: invalid base URL or model id\n');
    return null;
  }
  return {
    id: SMOKE_PROVIDER_ID,
    name: 'Smoke',
    auth: { method: 'none' },
    runtimes: { pi: { baseUrl, wireProtocol: 'openai-chat', models: [{ id: modelId, name: modelId, reasoning: false }] } },
  };
}

// 全部 provider（Smoke 在前），按 providerOrder 排序；未出现在 order 里的保持原相对顺序排在后面。
function allConfigs() {
  const list = [];
  const smoke = smokeProvider();
  if (smoke) list.push(smoke);
  list.push(...store.providers);
  const rank = new Map(store.providerOrder.map((id, i) => [id, i]));
  return list
    .map((c, i) => ({ c, i }))
    .sort((a, b) => (rank.get(a.c.id) ?? MAX_ORDER_ITEMS + a.i) - (rank.get(b.c.id) ?? MAX_ORDER_ITEMS + b.i))
    .map(({ c }) => c);
}

function isConnected(config) {
  return config.auth?.method === 'none';
}

/** 当前目录的 ProviderView[]（与 desktop maker:provider:list 的 providers 同形状）。 */
export function getProviderViews() {
  const configs = allConfigs();
  const catalog = { version: 'cindy-tauri-local', providers: configs.map((c) => buildUserProvider(c)) };
  const connected = Object.fromEntries(configs.map((c) => [c.id, isConnected(c)]));
  return buildRegistry(catalog, connected);
}

/**
 * 供 A 的 maker:get-capabilities 用：pi 的 availableModels（ModelDescriptor[]）。
 * 口径对齐 desktop catalog-to-descriptors.ts deriveAvailableModels(catalog, 'pi')：
 * 只取已连接 provider、可用于新路由的模型，按 id 首见去重。
 */
export function getPiModelDescriptors() {
  const seen = new Set();
  const out = [];
  for (const provider of getProviderViews()) {
    if (!provider.connected || provider.routing?.pi?.disabled === true) continue;
    for (const m of provider.models?.pi ?? []) {
      if (seen.has(m.id) || !isModelSelectableForNewRoute(m, { userProvider: true })) continue;
      seen.add(m.id);
      const d = {
        id: m.id,
        displayName: m.name,
        contextWindow: m.contextWindow,
        efforts: m.efforts,
        defaultEffort: m.defaultEffort,
        effortsUnknown: m.effortsUnknown === true,
      };
      if (m.group !== undefined) d.group = m.group;
      if (m.defaultEnabled !== undefined) d.defaultEnabled = m.defaultEnabled;
      if (m.supportsImageInput !== undefined) d.supportsImageInput = m.supportsImageInput;
      out.push(d);
    }
  }
  return out;
}

function toRunConfig(config, model) {
  const rt = config.runtimes.pi;
  const out = {
    providerId: config.id,
    providerName: config.name,
    modelId: model.id,
    baseUrl: rt.baseUrl,
    api: 'openai-completions',
    apiKey: null,
    reasoning: false,
  };
  // 只透传用户 / 发现结果明确给出的窗口；目录兜底的 200000 不当作上限。
  if (Number.isSafeInteger(model.contextWindow) && model.contextWindow > 0) out.contextLimit = model.contextWindow;
  return out;
}

/** renderer 传来的 (providerId, modelId) → Pi 运行配置；找不到或不可用返回 null。 */
export function resolveModel(providerId, modelId) {
  if (typeof providerId !== 'string' || typeof modelId !== 'string') return null;
  const config = allConfigs().find((c) => c.id === providerId);
  if (!config || !isConnected(config) || !config.runtimes.pi) return null;
  const model = config.runtimes.pi.models.find((m) => m.id === modelId);
  return model ? toRunConfig(config, model) : null;
}

/** 默认 Pi 运行配置：Smoke 优先，其次按 provider 顺序第一个已连接 provider 的第一个模型；未配置返回 null。 */
export function getModelConfig() {
  const configs = allConfigs();
  const ordered = [...configs.filter((c) => c.id === SMOKE_PROVIDER_ID), ...configs.filter((c) => c.id !== SMOKE_PROVIDER_ID)];
  for (const config of ordered) {
    const model = isConnected(config) ? config.runtimes.pi?.models[0] : null;
    if (model) return toRunConfig(config, model);
  }
  return null;
}

// ── 持久化 ────────────────────────────────────────────────────────────

function loadStore() {
  if (!existsSync(storePath)) return;
  try {
    const raw = JSON.parse(readFileSync(storePath, 'utf8'));
    const providers = Array.isArray(raw?.providers)
      ? raw.providers.map((p) => { try { return sanitizeConfig(p); } catch { return null; } }).filter(Boolean)
      : [];
    const providerOrder = Array.isArray(raw?.providerOrder) ? raw.providerOrder.filter((id) => typeof id === 'string') : [];
    store = { version: 1, providers, providerOrder };
  } catch (err) {
    // 坏文件改名保留，不覆盖用户数据。
    const backup = `${storePath}.corrupt-${Date.now()}`;
    process.stderr.write(`[model-config] providers.json unreadable (${err.message}); moved to ${path.basename(backup)}\n`);
    rename(storePath, backup).catch(() => {});
  }
}

function persist() {
  const snapshot = JSON.stringify(store, null, 2);
  const task = writeQueue.then(async () => {
    await mkdir(path.dirname(storePath), { recursive: true });
    const tmp = `${storePath}.tmp-${process.pid}`;
    await writeFile(tmp, snapshot, { mode: 0o600 });
    await rename(tmp, storePath);
  });
  writeQueue = task.catch(() => {});
  return task;
}

function changed() {
  emitToRenderer(PROVIDER_CHANGED, {});
}

// ── 输入校验 ──────────────────────────────────────────────────────────

function hasAnyKey(keys) {
  if (keys === undefined || keys === null) return false;
  if (typeof keys !== 'object' || Array.isArray(keys)) throw ipcError('INVALID_PARAMS', 'invalid provider runtime keys');
  for (const [agent, value] of Object.entries(keys)) {
    if (!AGENTS.includes(agent) || typeof value !== 'string') throw ipcError('INVALID_PARAMS', 'invalid provider runtime keys');
  }
  return Object.values(keys).some((v) => v.length > 0);
}

const NOT_IMPLEMENTED_SECRET = 'providers with API keys, OAuth or custom headers need secure storage, which is not available in the Tauri build yet';

/** 把 renderer 传来的 CustomProviderConfig 收敛成只含 pi runtime、无凭据的规整形状；非法抛 IPC 错误。 */
function sanitizeConfig(input) {
  if (!input || typeof input !== 'object' || Array.isArray(input)) throw ipcError('INVALID_PARAMS', 'config must be an object');
  const { id, name, auth, runtimes } = input;
  if (typeof id !== 'string' || !PROVIDER_ID_RE.test(id)) throw ipcError('INVALID_PARAMS', 'id must match /^[a-z0-9_-]{1,64}$/');
  if (typeof name !== 'string' || !name.trim() || name.length > 120) throw ipcError('INVALID_PARAMS', 'name required');
  if (auth !== undefined && auth?.method !== 'none') throw ipcError('NOT_IMPLEMENTED', NOT_IMPLEMENTED_SECRET);
  if (!runtimes || typeof runtimes !== 'object' || Array.isArray(runtimes)) throw ipcError('INVALID_PARAMS', 'runtimes required');
  const agents = Object.keys(runtimes);
  if (agents.some((a) => !AGENTS.includes(a))) throw ipcError('INVALID_PARAMS', 'invalid runtime');
  if (agents.length !== 1 || agents[0] !== 'pi') throw ipcError('NOT_IMPLEMENTED', 'only the Pi runtime is supported in the Tauri build');
  const rt = runtimes.pi;
  if (!rt || typeof rt !== 'object') throw ipcError('INVALID_PARAMS', 'pi runtime required');
  if (rt.headers && Object.keys(rt.headers).length > 0) throw ipcError('NOT_IMPLEMENTED', NOT_IMPLEMENTED_SECRET);
  if (rt.wireProtocol !== undefined && rt.wireProtocol !== 'openai-chat') {
    throw ipcError('NOT_IMPLEMENTED', 'only OpenAI Chat Completions compatible endpoints are supported');
  }
  const baseUrl = normalizeBaseUrl(rt.baseUrl);
  if (!baseUrl) throw ipcError('INVALID_PARAMS', 'baseUrl must be HTTPS or loopback HTTP without credentials');
  let modelsUrl;
  if (typeof rt.modelsUrl === 'string' && rt.modelsUrl.trim()) {
    modelsUrl = normalizeBaseUrl(rt.modelsUrl);
    if (!modelsUrl) throw ipcError('INVALID_PARAMS', 'modelsUrl must be HTTPS or loopback HTTP without credentials');
  }
  if (!Array.isArray(rt.models) || rt.models.length > MAX_MODELS) throw ipcError('INVALID_PARAMS', 'models must be an array');
  const seen = new Set();
  const models = [];
  for (const m of rt.models) {
    if (!m || typeof m.id !== 'string' || !MODEL_ID_RE.test(m.id.trim())) throw ipcError('INVALID_PARAMS', 'invalid model id');
    const mid = m.id.trim();
    if (seen.has(mid)) continue;
    seen.add(mid);
    const mname = typeof m.name === 'string' && m.name.trim() ? m.name.trim().slice(0, 200) : mid;
    const model = { id: mid, name: mname, reasoning: false };
    if (Number.isSafeInteger(m.contextWindow) && m.contextWindow > 0) model.contextWindow = m.contextWindow;
    if (typeof m.defaultEnabled === 'boolean') model.defaultEnabled = m.defaultEnabled;
    if (typeof m.supportsImageInput === 'boolean') model.supportsImageInput = m.supportsImageInput;
    models.push(model);
  }
  const pi = { baseUrl, wireProtocol: 'openai-chat', models };
  if (modelsUrl) pi.modelsUrl = modelsUrl;
  return { id, name: name.trim(), auth: { method: 'none' }, runtimes: { pi } };
}

function assertOwnerScope(scope) {
  if (scope === undefined) return;
  if (!scope || typeof scope !== 'object' || Array.isArray(scope)) throw ipcError('INVALID_PARAMS', 'Invalid owner scope');
  if (scope.dataOwnerId !== DATA_OWNER_ID || scope.ownerGeneration !== OWNER_GENERATION) {
    throw ipcError('INTERNAL', 'active account changed during provider mutation');
  }
}

function assertOptions(options) {
  if (options === undefined) return;
  if (!options || typeof options !== 'object' || Array.isArray(options)) throw ipcError('INVALID_PARAMS', 'invalid custom provider options');
}

function assertMutable(id) {
  if (id === SMOKE_PROVIDER_ID) throw ipcError('PERMISSION_DENIED', 'the Smoke provider is managed by environment variables');
}

// ── /models 请求 ─────────────────────────────────────────────────────

function classifyStatus(status) {
  if (status === 401) return 'AUTH_INVALID';
  if (status === 403) return 'AUTH_FORBIDDEN';
  if (status === 404) return 'ENDPOINT_NOT_FOUND';
  if (status === 402) return 'QUOTA_EXCEEDED';
  if (status === 429 || status === 529) return 'RATE_LIMITED';
  if (status >= 500) return 'UPSTREAM_ERROR';
  return 'UNKNOWN';
}

// GET <baseUrl>/models（或显式 modelsUrl），10 秒超时，不跟随重定向，不带任何凭据。
async function requestModels(baseUrl, modelsUrl, signal) {
  const target = modelsUrl ?? `${baseUrl}/models`;
  const started = Date.now();
  const timeout = AbortSignal.timeout(FETCH_TIMEOUT_MS);
  try {
    const res = await fetch(target, {
      method: 'GET',
      headers: { accept: 'application/json' },
      redirect: 'error',
      signal: signal ? AbortSignal.any([signal, timeout]) : timeout,
    });
    const latencyMs = Date.now() - started;
    const text = await res.text();
    if (!res.ok) return { ok: false, code: classifyStatus(res.status), status: res.status, latencyMs };
    let json = null;
    try { json = JSON.parse(text); } catch { /* 非 JSON */ }
    return { ok: true, status: res.status, latencyMs, models: parseModelsListResponse(json, target) };
  } catch (err) {
    const latencyMs = Date.now() - started;
    if (signal?.aborted) return { ok: false, cancelled: true, latencyMs };
    const name = err?.name ?? '';
    if (timeout.aborted || name === 'TimeoutError') return { ok: false, code: 'TIMEOUT', latencyMs };
    return { ok: false, code: 'UPSTREAM_UNREACHABLE', latencyMs };
  }
}

function savedPiRoute(providerId) {
  const config = allConfigs().find((c) => c.id === providerId);
  const rt = config?.runtimes.pi;
  return rt ? { baseUrl: rt.baseUrl, modelsUrl: rt.modelsUrl ?? null } : null;
}

// ── IPC handler ──────────────────────────────────────────────────────

function listProviders() {
  return {
    dataOwnerId: DATA_OWNER_ID,
    ownerGeneration: OWNER_GENERATION,
    providers: getProviderViews(),
    providerOrder: allConfigs().map((c) => c.id),
    modelVisibilityOverrides: { ...visibilityMirror },
  };
}

async function createProvider(input, keys, options) {
  assertOptions(options);
  if (hasAnyKey(keys)) throw ipcError('NOT_IMPLEMENTED', NOT_IMPLEMENTED_SECRET);
  if (input && typeof input === 'object' && (RESERVED_IDS.has(input.id))) {
    throw ipcError('INVALID_PARAMS', `id '${input.id}' is reserved`);
  }
  const config = sanitizeConfig(input);
  if (store.providers.some((p) => p.id === config.id)) throw ipcError('ALREADY_EXISTS', `custom provider '${config.id}' already exists`);
  store.providers.push(config);
  await persist();
  changed();
  return { ok: true };
}

async function updateProvider(input, keys, options) {
  assertOptions(options);
  if (hasAnyKey(keys)) throw ipcError('NOT_IMPLEMENTED', NOT_IMPLEMENTED_SECRET);
  if (input && typeof input === 'object') assertMutable(input.id);
  const config = sanitizeConfig(input);
  const index = store.providers.findIndex((p) => p.id === config.id);
  if (index < 0) throw ipcError('NOT_FOUND', `custom provider '${config.id}' not found`);
  store.providers[index] = config;
  await persist();
  changed();
  return { ok: true };
}

async function deleteProvider(providerId, ownerScope, options) {
  assertOwnerScope(ownerScope);
  assertOptions(options);
  if (typeof providerId !== 'string' || !providerId) throw ipcError('INVALID_PARAMS', 'providerId required');
  assertMutable(providerId);
  const before = store.providers.length;
  store.providers = store.providers.filter((p) => p.id !== providerId);
  store.providerOrder = store.providerOrder.filter((id) => id !== providerId);
  if (store.providers.length !== before) {
    await persist();
    changed();
  }
  return { ok: true };
}

// desktop 只允许断开 API key 形态的连接；这里所有 provider 都是无鉴权，与 desktop 一致返回 INVALID_PARAMS。
function disconnectProvider(providerId, ownerScope, options) {
  assertOwnerScope(ownerScope);
  assertOptions(options);
  if (typeof providerId !== 'string' || !providerId) throw ipcError('INVALID_PARAMS', 'providerId required');
  throw ipcError('INVALID_PARAMS', 'API connection required');
}

const connectionTests = new Map();

async function testConnection(input) {
  if (!input || typeof input !== 'object') throw ipcError('INVALID_PARAMS', 'invalid test-connection input');
  if (input.requestId !== undefined && (typeof input.requestId !== 'string' || !/^[a-zA-Z0-9_-]{1,100}$/.test(input.requestId))) {
    throw ipcError('INVALID_PARAMS', 'invalid test-connection input');
  }
  if (input.kind === 'cancel') {
    connectionTests.get(input.requestId)?.abort();
    return { ok: false, latencyMs: 0, decision: 'cancelled' };
  }
  let route;
  let modelId;
  if (input.kind === 'saved') {
    if (typeof input.providerId !== 'string' || !AGENTS.includes(input.agent)) throw ipcError('INVALID_PARAMS', 'invalid test-connection input');
    const config = allConfigs().find((c) => c.id === input.providerId);
    const rt = input.agent === 'pi' ? config?.runtimes.pi : null;
    if (!rt) throw ipcError('INVALID_PARAMS', 'invalid saved provider connection');
    route = { baseUrl: rt.baseUrl, modelsUrl: rt.modelsUrl ?? null };
    modelId = rt.models[0]?.id ?? null;
  } else if (input.kind === 'adhoc') {
    const spec = input.spec;
    if (!spec || typeof spec !== 'object' || !AGENTS.includes(spec.agent)) throw ipcError('INVALID_PARAMS', 'invalid test-connection input');
    if ((typeof spec.apiKey === 'string' && spec.apiKey.length > 0) || spec.authMethod === 'oauth'
      || (spec.headers && Object.keys(spec.headers).length > 0)) {
      throw ipcError('NOT_IMPLEMENTED', NOT_IMPLEMENTED_SECRET);
    }
    if (spec.wireProtocol !== undefined && spec.wireProtocol !== 'openai-chat') {
      throw ipcError('NOT_IMPLEMENTED', 'only OpenAI Chat Completions compatible endpoints are supported');
    }
    const baseUrl = normalizeBaseUrl(spec.baseUrl);
    if (!baseUrl || typeof spec.modelId !== 'string' || !spec.modelId) throw ipcError('INVALID_PARAMS', 'invalid test-connection input');
    route = { baseUrl, modelsUrl: null };
    modelId = spec.modelId;
  } else {
    throw ipcError('INVALID_PARAMS', 'invalid test-connection input');
  }
  const controller = new AbortController();
  if (input.requestId) {
    if (connectionTests.has(input.requestId)) throw ipcError('PRECONDITION_FAILED', 'connection test already running');
    connectionTests.set(input.requestId, controller);
  }
  try {
    const r = await requestModels(route.baseUrl, route.modelsUrl, controller.signal);
    if (r.cancelled) return { ok: false, latencyMs: 0, decision: 'cancelled' };
    const selected = { baseUrl: route.baseUrl, wireProtocol: 'openai-chat' };
    if (!r.ok) return { ok: false, code: r.code, ...(r.status ? { status: r.status } : {}), latencyMs: r.latencyMs, decision: 'blocked' };
    // 端点能列出模型但不含所选模型时判为 MODEL_NOT_FOUND；列表不可解析时只认端点可达。
    if (modelId && Array.isArray(r.models) && r.models.length > 0 && !r.models.some((m) => m.id === modelId)) {
      return { ok: false, code: 'MODEL_NOT_FOUND', status: r.status, latencyMs: r.latencyMs, decision: 'blocked' };
    }
    return { ok: true, status: r.status, latencyMs: r.latencyMs, decision: 'success', ...(input.strategy === 'auto' ? { selected } : {}) };
  } finally {
    if (input.requestId) connectionTests.delete(input.requestId);
  }
}

async function fetchModels(input) {
  if (!input || typeof input !== 'object' || !AGENTS.includes(input.agent)) throw ipcError('INVALID_PARAMS', 'invalid models-fetch input');
  if (!['apiKey', 'oauth', 'none'].includes(input.authMethod)) throw ipcError('INVALID_PARAMS', 'invalid models-fetch input');
  if ((typeof input.apiKey === 'string' && input.apiKey.length > 0) || input.authMethod === 'oauth'
    || (input.headers && Object.keys(input.headers).length > 0)) {
    throw ipcError('NOT_IMPLEMENTED', NOT_IMPLEMENTED_SECRET);
  }
  let baseUrl = normalizeBaseUrl(input.baseUrl);
  let modelsUrl = null;
  if (typeof input.modelsUrl === 'string' && input.modelsUrl.trim()) {
    modelsUrl = normalizeBaseUrl(input.modelsUrl);
    if (!modelsUrl) throw ipcError('INVALID_PARAMS', 'invalid models-fetch input');
  }
  if (!baseUrl) throw ipcError('INVALID_PARAMS', 'invalid models-fetch input');
  // 已保存 provider：目标钉回已存端点，与 desktop 一致。
  if (typeof input.savedProviderId === 'string') {
    const saved = savedPiRoute(input.savedProviderId);
    if (saved) ({ baseUrl, modelsUrl } = saved);
  }
  const r = await requestModels(baseUrl, modelsUrl);
  if (!r.ok) return { ok: false, code: r.code, ...(r.status ? { status: r.status } : {}) };
  if (!r.models) return { ok: false, code: 'WIRE_INCOMPATIBLE', status: r.status };
  if (r.models.length === 0 && input.allowEmpty !== true) return { ok: false, code: 'MODEL_NOT_FOUND', status: r.status };
  return { ok: true, models: r.models };
}

async function setProviderOrder(input) {
  if (!input || typeof input !== 'object' || Array.isArray(input)) throw ipcError('INVALID_PARAMS', 'invalid provider order input');
  const { dataOwnerId, ownerGeneration, providerIds: ids } = input;
  if (Object.keys(input).length !== 3 || (dataOwnerId !== null && typeof dataOwnerId !== 'string')
    || !Number.isSafeInteger(ownerGeneration) || ownerGeneration < 0 || !Array.isArray(ids) || ids.length === 0
    || ids.length > MAX_ORDER_ITEMS || ids.some((id) => typeof id !== 'string' || !id || id.length > 256)
    || new Set(ids).size !== ids.length) {
    throw ipcError('INVALID_PARAMS', 'providerIds must be a bounded unique non-empty string[]');
  }
  assertOwnerScope({ dataOwnerId, ownerGeneration });
  const catalogIds = new Set(allConfigs().map((c) => c.id));
  if (ids.some((id) => !catalogIds.has(id))) throw ipcError('INVALID_PARAMS', 'providerIds must belong to the current provider catalog');
  if (JSON.stringify(store.providerOrder) !== JSON.stringify(ids)) {
    store.providerOrder = [...ids];
    await persist();
    changed();
  }
  return { ok: true };
}

// renderer 单向镜像的模型可见性快照；owner 不匹配的迟到快照直接丢弃（desktop 同语义）。
function syncModelVisibility(dataOwnerId, ownerGeneration, map, policy) {
  if (dataOwnerId !== DATA_OWNER_ID || ownerGeneration !== OWNER_GENERATION) return undefined;
  if (!map || typeof map !== 'object' || Array.isArray(map)) throw ipcError('INVALID_PARAMS', 'invalid visibility map');
  if (policy?.pending === true) return undefined;
  const next = Object.fromEntries(Object.entries(map).filter(([k, v]) => typeof k === 'string' && typeof v === 'boolean'));
  if (JSON.stringify(next) !== JSON.stringify(visibilityMirror)) {
    visibilityMirror = next;
    changed();
  }
  return undefined;
}

export function registerProviderIpc({ dataDir }) {
  storePath = path.join(dataDir, 'providers.json');
  store = { version: 1, providers: [], providerOrder: [] };
  visibilityMirror = {};
  loadStore();

  // 模型可见性 owner 认领：ipc.mjs 默认返回 null，preload 会兜底成 canWriteOwnerScoped:false，
  // renderer 的可见性迁移因此卡在 owner-pending，providers 快照永不提交，输入框显示
  // "Model Details Unavailable"。本机模式只有一个 owner，直接给出可写、无旧数据的认领结果。
  registerSync('maker:model-visibility:legacy-owner-claim-sync', () => ({
    dataOwnerId: DATA_OWNER_ID,
    ownerGeneration: OWNER_GENERATION,
    canWriteOwnerScoped: true,
    claimed: false,
    claimedByOtherOwner: false,
    canInitialize: false,
    profileOrigin: 'new',
  }));

  registerInvoke('maker:provider:list', listProviders);
  registerInvoke('maker:provider:presets', () => ({ presets: [] }));
  registerInvoke('maker:provider:custom:create', createProvider);
  registerInvoke('maker:provider:custom:update', updateProvider);
  registerInvoke('maker:provider:custom:delete', deleteProvider);
  registerInvoke('maker:provider:custom:disconnect', disconnectProvider);
  registerInvoke('maker:provider:test-connection', testConnection);
  registerInvoke('maker:provider:models-fetch', fetchModels);
  // 只接受内置 provider id（desktop 同口径）；本构建没有内置 provider，合法 id 直接回成功。
  registerInvoke('maker:provider:models-refresh', (providerId) => {
    if (!BUILTIN_REFRESHABLE.includes(providerId)) {
      throw ipcError('INVALID_PARAMS', `providerId must be one of: ${BUILTIN_REFRESHABLE.join(', ')}`);
    }
    return { ok: true, providerId };
  });
  // ModelSelector 打开时会调；没有内置动态清单，直接成功。
  registerInvoke('maker:provider:models-auto-refresh', (trigger) => {
    if (!AUTO_REFRESH_TRIGGERS.includes(trigger)) {
      throw ipcError('INVALID_PARAMS', `trigger must be one of: ${AUTO_REFRESH_TRIGGERS.join(', ')}`);
    }
    return { ok: true };
  });
  registerInvoke('maker:provider:order:set', setProviderOrder);
  registerInvoke('maker:provider:local-cli-scan', () => ({ detections: [] }));
  registerInvoke('maker:provider:detect', () => ({ candidates: [], reason: 'insufficient_evidence' }));
  registerInvoke('maker:model-visibility:sync', syncModelVisibility);
  registerInvoke('model-access:get-status', () => ({ state: 'idle', source: null, endpoint: null, accountTier: null }));
  registerInvoke('ghosts:models:list', () => []);
}
