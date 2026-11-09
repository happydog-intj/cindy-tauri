// 会话宿主：在 sidecar 里装配 maker-core 的 Maker + PiAgent（只支持 Pi）。
// 参照 cindy-headless host.ts createHeadlessMaker 与 desktop maker-host/pi-host.ts buildPiAgent。
// 模型来源：C 的自定义 OpenAI 兼容 provider，经 resolvePiNativeProviders 写进 Pi 的 models.json
// 原生 provider 块；登录后的 Cindy AI（xd）走 maker-core 的 cindy 网关块，baseUrl 指向本地转发
// （gateway-forwarder.mjs），Pi 进程里只有占位 key，真实网关 key 由转发注入。不接订阅。
// 带 key 的 provider：models.json 里只写 $CINDY_PI_KEY_<ID> 引用，key 经 env 交给 Pi 进程
// （maker-core 会把这些变量名列进 CINDY_PI_SECRET_ENV_NAMES，Pi 的 bash 子进程拿不到）。
import path from 'node:path';
import { mkdirSync } from 'node:fs';
import { Maker, PiAgent } from '@cindy/maker-core';

import { agentStatus, installAgent } from './agents.mjs';
import { piRsPath } from './pi-rs.mjs';
import { SqliteSessionStorage } from './maker-storage.mjs';
import {
  gatewayEndpoint,
  onXdChange,
  registerPiProxySession,
  resolvePiGatewayModelSpec,
  resolveXdDescriptor,
  XD_PROVIDER_ID,
  xdConnected,
} from './model-access.mjs';

// maker-core 认为是网关/订阅的 provider id，自定义 provider 不能用这些作 Pi 运行时 id。
const RESERVED_PI_PROVIDER_IDS = new Set(['cindy', 'xd', 'openai', 'anthropic', 'xai']);
// keyless provider 的占位 key：Pi 要求 provider 有 key 才可选，值不会发给任何上游。
const KEYLESS_PLACEHOLDER = 'cindy-tauri-keyless';

// maker-core 把 null / 'xd' / 'cindy' 当作 Cindy 网关路由。
function isGatewayProviderId(providerId) {
  return providerId === null || providerId === undefined || providerId === XD_PROVIDER_ID || providerId === 'cindy';
}

function stderrLogger(scope = 'maker') {
  const write = (level, message, context) => {
    if (level === 'trace' || (level === 'debug' && process.env.CINDY_TAURI_MAKER_DEBUG !== '1')) return;
    process.stderr.write(`${JSON.stringify({ level, scope, message, ...context })}\n`);
  };
  const logger = {
    trace: (m, c) => write('trace', m, c),
    debug: (m, c) => write('debug', m, c),
    info: (m, c) => write('info', m, c),
    warn: (m, c) => write('warn', m, c),
    error: (m, c) => write('error', m, c),
    fatal: (m, c) => write('fatal', m, c),
    child: (name) => stderrLogger(`${scope}:${name}`),
  };
  return logger;
}

// Pi 运行时 provider id 只允许 slug；保留字加前缀避开网关路由。
export function piRuntimeProviderId(providerId) {
  const slug = String(providerId).toLowerCase().replace(/[^a-z0-9_-]+/g, '-').replace(/^-+|-+$/g, '') || 'custom';
  return RESERVED_PI_PROVIDER_IDS.has(slug) ? `cindy-tauri-${slug}` : slug;
}

// 与 desktop pi-host.ts piNativeKeyEnvVar 同规则：CINDY_PI_KEY_<ID>，ID 规整成 [A-Z0-9_]。
export function piNativeKeyEnvVar(providerId) {
  return `CINDY_PI_KEY_${String(providerId).toUpperCase().replace(/[^A-Z0-9]+/g, '_')}`;
}

// C 的模型配置 → Pi 原生 provider 描述。apiKeyEnvVar 只是变量名，值由调用方放进 env。
export function toPiNativeProvider(cfg) {
  if (!cfg) return null;
  return {
    id: piRuntimeProviderId(cfg.providerId),
    ...(cfg.authMethod === 'apiKey' ? { apiKeyEnvVar: piNativeKeyEnvVar(cfg.providerId) } : {}),
    sourceProviderId: cfg.providerId,
    name: cfg.providerName || cfg.providerId,
    baseUrl: cfg.baseUrl,
    api: cfg.api || 'openai-completions',
    models: [{
      id: cfg.modelId,
      name: cfg.modelId,
      reasoning: cfg.reasoning === true,
      input: ['text'],
      ...(cfg.contextLimit ? { contextWindow: cfg.contextLimit } : {}),
    }],
  };
}

/**
 * 创建会话宿主。
 * models: C 的模块对象（getModelConfig / resolveModel）。
 * 返回 { maker, ensurePiInstalled, piStatus, resolveSessionModel, shutdown }。
 */
export function createMakerHost({ dataDir, models, agentManifestDir, logger = stderrLogger() }) {
  const piCtx = { manifestDir: agentManifestDir, dataDir };
  // 二进制路径在装好之前就确定（agents.mjs 的固定安装位置）；createSession 前再确保已安装。
  const status = agentStatus('pi', piCtx);
  // 实验开关：CINDY_PI_RS=<pi-rs 二进制> 让会话改用 Rust 版 Pi（内存约 10MB 对 300MB）。默认关闭。
  // maker-core 在构造 PiAgent 时固定二进制，不能按会话切换，所以由 pi-rs 自己判断：会话用到它没有的能力
  // （子代理/其他扩展、skill、MCP、vision、受邀者托管、原生 provider 适配）时，pi-rs 启动即 exec 原版 Pi，
  // 原版路径经 CINDY_PI_FALLBACK_BINARY 传入。bash 环境剔除密钥、ask_user_question、纯文本轮、分支切换由 pi-rs 自己实现。
  const rsBinary = piRsPath();
  if (rsBinary) {
    process.env.CINDY_PI_FALLBACK_BINARY = status.binary;
    logger.warn('using experimental pi-rs runtime', { binary: rsBinary, fallback: status.binary });
  }
  const agentHome = path.join(dataDir, 'pi-agent-home');
  // Pi 原生全局上下文目录也放进数据目录，避免读到用户 ~/.pi/agent 的配置。
  const globalContextHome = path.join(dataDir, 'pi-global-context');
  mkdirSync(agentHome, { recursive: true });
  mkdirSync(globalContextHome, { recursive: true });

  const resolveConfig = (providerId, modelId) => {
    if (providerId && modelId) {
      const hit = models.resolveModel(providerId, modelId);
      if (hit) return hit;
    }
    return null;
  };

  const auth = {
    // Cindy 网关：要有登录下发的网关 key（desktop DesktopPiAuthAdapter 同口径）；
    // 自定义 provider 的 key 由 resolvePiNativeProviders 现取，这里恒为已就绪。
    async getState(options) {
      const providerId = options?.providerId?.trim() || null;
      if (isGatewayProviderId(providerId)) {
        return xdConnected()
          ? { authenticated: true, identity: 'Cindy AI', authSource: 'api-key' }
          : { authenticated: false, errorReason: 'cindy_gateway_key_unavailable' };
      }
      return { authenticated: true, authSource: 'api-key' };
    },
    async triggerLogin() { return { authenticated: true, authSource: 'api-key' }; },
    async logout() {},
    // Pi 只拿占位值：网关请求经本地转发时才换成真实 key。
    async getAuthEnv() { return { CINDY_PI_API_KEY: KEYLESS_PLACEHOLDER }; },
  };

  // endpoint 用 getter：本地转发的端口在监听后才确定，Pi 每次写 models.json 时现读。
  const runtimeConfig = { userDataPath: dataDir };
  Object.defineProperty(runtimeConfig, 'endpoint', { get: () => gatewayEndpoint(), enumerable: true });

  const agent = new PiAgent({
    auth,
    runtimeConfig,
    binaryPath: rsBinary ?? status.binary,
    logger: logger.child('pi'),
    resolvePiAgentHome: () => agentHome,
    resolvePiGlobalContextHome: () => globalContextHome,
    // Cindy 网关成员的协议与 compat 由 model-access.mjs 解析；非成员返回 undefined，maker-core 不写进 cindy 块。
    resolvePiGatewayModelSpec: (providerId, modelId) => resolvePiGatewayModelSpec(providerId, modelId),
    resolvePiGatewayModelApi: (providerId, modelId) => {
      const spec = resolvePiGatewayModelSpec(providerId, modelId);
      return spec === null ? null : spec?.api;
    },
    resolvePiGatewayModelDescriptor: (_providerId, modelId) => resolveXdDescriptor(modelId) ?? null,
    resolvePiRuntimeModels: () => models.getPiModelDescriptors(),
    registerPiProxySession: (sessionId, token, resolveProviderId, options) =>
      registerPiProxySession(sessionId, token, resolveProviderId, options),
    resolveModelContextLimit: (providerId, modelId) => resolveConfig(providerId, modelId)?.contextLimit ?? null,
    resolvePiNativeProviders: async (ctx) => {
      await models.whenSecretsReady?.();
      const cfg = resolveConfig(ctx.providerId, ctx.model);
      // Cindy 网关模型不是原生 provider，由 maker-core 的 cindy 块承载。
      if (cfg?.gateway) return { providers: [], env: {} };
      const spec = toPiNativeProvider(cfg);
      if (!spec) return { providers: [], env: {} };
      if (!spec.apiKeyEnvVar) return { providers: [spec], env: {} };
      // 每次启动 / 恢复 Pi 时现取，断开连接后不会再用旧 key。取不到就抛错，maker-core 会按
      // “所选 provider 不可解析”结束本次启动，而不是带着空 key 发请求。
      const key = models.readProviderApiKey?.(cfg.providerId);
      if (!key) throw new Error(`[AUTH_INVALID] API key for provider ${cfg.providerId} is missing`);
      return { providers: [spec], env: { [spec.apiKeyEnvVar]: key } };
    },
  });

  const storage = new SqliteSessionStorage(path.join(dataDir, 'maker-sessions.sqlite'));
  const maker = new Maker({ agents: { pi: agent }, storage, logger });

  // desktop refreshCatalogDerivedModels：原地替换 Pi 的 availableModels，已有会话持有的是同一个数组。
  const syncCapabilities = () => {
    const list = agent.capabilities.availableModels;
    list.splice(0, list.length, ...models.getPiModelDescriptors());
  };
  const offXd = onXdChange(syncCapabilities);

  return {
    maker,
    piStatus: () => agentStatus('pi', piCtx),
    async ensurePiInstalled() {
      if (agentStatus('pi', piCtx).installed) return;
      logger.info('pi binary missing, installing on demand', { version: status.version });
      await installAgent('pi', piCtx);
    },
    // 钥匙串主密钥到位（或确认不可用）后再解析模型，避免启动瞬间带 key 的 provider 被当成未连接。
    async whenModelsReady() {
      await models.whenSecretsReady?.();
      syncCapabilities();
    },
    // 把 renderer 传来的 providerId/model 落到 C 的配置上；缺 providerId 时用当前默认配置。
    resolveSessionModel(providerId, modelId) {
      // renderer 选 Cindy AI 模型时 providerId 为空（null = 网关，与 maker-core 同口径）：先按 xd 找这个模型，
      // 找不到才退回默认模型；不能直接用默认，否则会落到第一个自定义 provider 上。
      const gatewayHit = !providerId && modelId ? models.resolveModel(XD_PROVIDER_ID, modelId) : null;
      const cfg = gatewayHit ?? resolveConfig(providerId, modelId) ?? (providerId ? null : models.getModelConfig());
      if (!cfg) throw new Error(`[MODEL_NOT_CONFIGURED] model ${providerId ?? '(default)'}/${modelId ?? ''} is not configured`);
      if (modelId && cfg.modelId !== modelId && providerId) {
        throw new Error(`[MODEL_NOT_CONFIGURED] model ${modelId} not found in provider ${providerId}`);
      }
      return cfg;
    },
    async shutdown() {
      offXd();
      await maker.shutdown({ reason: 'app-quit' }).catch(() => undefined);
      storage.close();
    },
  };
}
