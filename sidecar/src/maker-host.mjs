// 会话宿主：在 sidecar 里装配 maker-core 的 Maker + PiAgent（只支持 Pi）。
// 参照 cindy-headless host.ts createHeadlessMaker 与 desktop maker-host/pi-host.ts buildPiAgent。
// 模型来源只有 C 的自定义 OpenAI 兼容 provider（无 key），经 resolvePiNativeProviders 写进
// Pi 的 models.json 原生 provider 块；不接 Cindy 网关、不接订阅。
import path from 'node:path';
import { mkdirSync } from 'node:fs';
import { Maker, PiAgent } from '@cindy/maker-core';

import { agentStatus, installAgent } from './agents.mjs';
import { SqliteSessionStorage } from './maker-storage.mjs';

// maker-core 认为是网关/订阅的 provider id，自定义 provider 不能用这些作 Pi 运行时 id。
const RESERVED_PI_PROVIDER_IDS = new Set(['cindy', 'xd', 'openai', 'anthropic', 'xai']);
// keyless provider 的占位 key：Pi 要求 provider 有 key 才可选，值不会发给任何上游。
const KEYLESS_PLACEHOLDER = 'cindy-tauri-keyless';

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

// C 的模型配置 → Pi 原生 provider 描述。
export function toPiNativeProvider(cfg) {
  if (!cfg) return null;
  if (cfg.apiKey) {
    // 这一里程碑不接安全存储，有 key 的 provider 一律拒绝，避免 key 写进 env/日志。
    throw new Error('[NOT_IMPLEMENTED] provider with api key requires secure storage (not wired yet)');
  }
  return {
    id: piRuntimeProviderId(cfg.providerId),
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
    // 只支持无 key 自定义 provider：鉴权恒为已就绪，key 用占位值。
    async getState() { return { authenticated: true, authSource: 'api-key' }; },
    async triggerLogin() { return { authenticated: true, authSource: 'api-key' }; },
    async logout() {},
    async getAuthEnv() { return { CINDY_PI_API_KEY: KEYLESS_PLACEHOLDER }; },
  };

  const agent = new PiAgent({
    auth,
    runtimeConfig: { userDataPath: dataDir },
    binaryPath: status.binary,
    logger: logger.child('pi'),
    resolvePiAgentHome: () => agentHome,
    resolvePiGlobalContextHome: () => globalContextHome,
    resolvePiGatewayModelApi: () => 'openai-completions',
    resolveModelContextLimit: (providerId, modelId) => resolveConfig(providerId, modelId)?.contextLimit ?? null,
    resolvePiNativeProviders: async (ctx) => {
      const spec = toPiNativeProvider(resolveConfig(ctx.providerId, ctx.model));
      return { providers: spec ? [spec] : [], env: {} };
    },
  });

  const storage = new SqliteSessionStorage(path.join(dataDir, 'maker-sessions.sqlite'));
  const maker = new Maker({ agents: { pi: agent }, storage, logger });

  return {
    maker,
    piStatus: () => agentStatus('pi', piCtx),
    async ensurePiInstalled() {
      if (agentStatus('pi', piCtx).installed) return;
      logger.info('pi binary missing, installing on demand', { version: status.version });
      await installAgent('pi', piCtx);
    },
    // 把 renderer 传来的 providerId/model 落到 C 的配置上；缺 providerId 时用当前默认配置。
    resolveSessionModel(providerId, modelId) {
      const cfg = resolveConfig(providerId, modelId) ?? (providerId ? null : models.getModelConfig());
      if (!cfg) throw new Error(`[MODEL_NOT_CONFIGURED] model ${providerId ?? '(default)'}/${modelId ?? ''} is not configured`);
      if (modelId && cfg.modelId !== modelId && providerId) {
        throw new Error(`[MODEL_NOT_CONFIGURED] model ${modelId} not found in provider ${providerId}`);
      }
      return cfg;
    },
    async shutdown() {
      await maker.shutdown({ reason: 'app-quit' }).catch(() => undefined);
      storage.close();
    },
  };
}
