// 冻结的参考实现：sidecar/src/model-access.mjs 里 xd 模型投影的纯逻辑原样拷贝（sidecar 迁完会删），
// 只给黄金样例生成器用（scripts/app-settings/xdcatalog-entry.ts）。不要在这里改行为。
import { BUNDLED_CATALOG, clampEffortToSupported, defaultEffortForCapabilities, resolveModelNativeApi } from '@cindy/model-providers';
import { effectiveGatewayModelCost } from '@cindy-desktop-shared/modelPriceQuote';
import {
  isCatalogPiGatewayModelRetired, resolveBundledPiGatewayModelProfile, resolveCatalogPiGatewayModelApi,
} from '@cindy-desktop-main/maker-host/pi-gateway-model-catalog.ts';

const XD_PROVIDER_ID = 'xd';
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

let gatewayModels: any[] = [];
function normalizeModelId(modelId) {
  return String(modelId).replace(/\[1m\]$/, '');
}

function nativeApiForRoute(modelId: any) {
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


export { projectPiModels };
export function setGatewayModels(models: any[]) { gatewayModels = models; }
