// xd 网关模型投影的数据表与黄金样例。
// data：Rust 编译期嵌入的紧凑静态表（注册表里 xd 相关的条目、路由 API 声明、Pi 目录行）；
// golden：对一批网关模型运行 sidecar 里真实的 projectPiModels / resolvePiGatewayModelSpec 得到的输出。
import { BUNDLED_CATALOG, providerCatalogForPi } from '@cindy/model-providers';
import { registryEntryDefaults, findBaseModel } from '../../frontend/packages/model-providers/src/modelMetadataLayers.ts';
import { projectPiModels, resolvePiGatewayModelSpec, setGatewayModels } from './xd-reference.ts';
import { effectiveGatewayModelCost as cost } from '@cindy-desktop-shared/modelPriceQuote';
import {
  isCatalogPiGatewayModelRetired, resolveBundledPiGatewayModelProfile, resolveCatalogPiGatewayModelApi,
} from '@cindy-desktop-main/maker-host/pi-gateway-model-catalog.ts';
import { resolveModelNativeApi } from '@cindy/model-providers';

const reg: any = BUNDLED_CATALOG.modelRegistry;
const cat: any = BUNDLED_CATALOG;
const norm = (v: any) => JSON.parse(JSON.stringify(v === undefined ? null : v));

// 注册表条目：只留与 xd 有关的（路由里出现 xd），并预先算好 registryEntryDefaults(...).mode。
const entries = reg.models.filter((e: any) => e.routes.some((r: any) => r.providerId === 'xd')).map((e: any) => ({
  id: e.id, status: e.status ?? null,
  nativeApi: e.nativeApi === undefined ? { u: 1 } : e.nativeApi,
  mode: registryEntryDefaults(reg, e).mode ?? null,
  routes: e.routes.map((r: any) => ({ providerId: r.providerId, modelId: r.modelId })),
}));
const bases = (reg.baseModels ?? []).map((b: any) => ({ id: b.id, aliases: b.aliases, mode: b.defaults?.mode ?? null }));
const rules = (reg.nativeApiRules ?? []).filter((r: any) => r.providerId === 'xd');

// routedCatalogApis 需要的 provider / preset 的 Pi 模型 API 声明（只留被 xd 条目的非 xd 路由引用的）。
const wanted = new Set<string>();
for (const e of entries) for (const r of e.routes) if (r.providerId !== 'xd') wanted.add(r.providerId);
const piModel = (m: any) => ({ id: m.id, piApi: m.piApi ?? null, wire: m.route?.wireProtocol ?? null });
const providers: any = {};
for (const p of cat.providers) if (wanted.has(p.id)) providers[p.id] = { wire: p.routing?.pi?.wireProtocol ?? null, models: (p.models?.pi ?? []).map(piModel) };
const presets: any = {};
for (const p of cat.presets ?? []) if (wanted.has(p.id) && p.runtimes?.pi) presets[p.id] = { wire: p.runtimes.pi.wireProtocol ?? null, models: (p.runtimes.pi.models ?? []).map(piModel) };

// Pi 目录行（pi-gateway-model-catalog.ts 的 rows）：只留带 api 的字段。
const piRows = Object.values((providerCatalogForPi() as any).providers).flat().map((r: any) => ({
  provider: r.provider, id: r.id, api: r.api,
  ...(r.compat ? { compat: r.compat } : {}), ...(r.samplingParams ? { samplingParams: r.samplingParams } : {}),
  ...(r.thinkingLevelMap ? { thinkingLevelMap: r.thinkingLevelMap } : {}),
}));
const xdProvider = cat.providers.find((p: any) => p.id === 'xd');
const { embeddingModels: _e, embeddingDefaults: _d, imageModels: _i, videoModels: _v, audioModels: _a, ...xdRest } = xdProvider;

const data = { schemaVersion: reg.schemaVersion, entries, bases, rules, providers, presets, piRows, xdProvider: norm({ ...xdRest, agents: ['pi'], routing: { pi: xdProvider.routing.pi } }) };

// ── 黄金：网关模型 id 集合 ──
const ids = new Set<string>();
for (const e of entries) for (const r of e.routes) if (r.providerId === 'xd') ids.add(r.modelId);
for (const r of piRows) ids.add(`${r.provider}/${r.id}`);
for (const id of ['claude-opus-5', 'gpt-5.5', 'openai-codex/gpt-5.5:auto', 'codex/gpt-5.5:auto', 'deepseek/deepseek-v4-pro', 'deepseek/deepseek-flash', 'moonshot/kimi-k3',
  'qwen/qwen3.8-max', 'z-ai/glm-5.3', 'x-ai/grok-4.6', 'google/gemini-9', 'gemini-9', 'unknown/model', 'claude-sonnet-5[1m]', 'anthropic/claude-opus-5', 'gpt-5.4-mini',
  'codex/gpt-5.4', 'anthropic-claude/claude-x', 'qwen-x', '', 'a/b/c']) ids.add(id);
const idList = [...ids];

const resolveCases = idList.map((id) => ({
  id,
  nativeApi: ((v: any) => (v === undefined ? { u: 1 } : v))(resolveModelNativeApi(reg, 'xd', id)),
  retired: isCatalogPiGatewayModelRetired(BUNDLED_CATALOG, id),
  catalogApi: ((v: any) => (v === undefined ? { u: 1 } : v))(resolveCatalogPiGatewayModelApi(BUNDLED_CATALOG, id)),
  profile: norm(resolveBundledPiGatewayModelProfile(id) ?? null),
}));

// 网关模型样本：每个 id 一个带 pi 的模型，再叠几个形状变体。
const gm = (id: string, extra: any = {}) => ({
  id, name: id, agents: ['pi'], contextWindow: 200000, availability: 'available', efforts: ['low', 'high'], defaultEffort: 'high', ...extra,
});
const models: any[] = idList.map((id, i) => gm(id, i % 7 === 0 ? { perAgent: { pi: { wireProtocol: 'openai-completions', contextWindow: 100000 } } } : {}));
models.push(
  gm('hint/anthropic', { perAgent: { pi: { wireProtocol: 'anthropic-messages' } } }), gm('hint/bogus', { perAgent: { pi: { wireProtocol: 'bogus' } } }), gm('hint/none'),
  gm('x/pay', { availability: 'requires_payment', mode: 'chat' }), gm('x/embed', { mode: 'embedding' }), gm('x/resp', { mode: 'responses' }),
  gm('x/cost', { inputCostPerToken: 0.000003, outputCostPerToken: 0.000015, cacheReadInputTokenCost: 0.0000003, cacheCreationInputTokenCost: 0.00000375, costDiscount: 0.2 }),
  gm('x/cost0', { inputCostPerToken: 0, outputCostPerToken: 0 }), gm('x/costbad', { inputCostPerToken: -1, outputCostPerToken: 1 }), gm('x/discount', { inputCostPerToken: 1e-6, outputCostPerToken: 2e-6, costDiscount: 1 }),
  gm('x/discount2', { inputCostPerToken: 1e-6, outputCostPerToken: 2e-6, costDiscount: 1.5 }), gm('x/noeff', { efforts: [], defaultEffort: undefined }), gm('x/noeff2', { efforts: undefined, defaultEffort: undefined }),
  gm('x/clamp', { efforts: ['low', 'medium'], defaultEffort: 'xhigh' }), gm('x/clamp2', { efforts: ['high', 'max'], defaultEffort: 'minimal' }), gm('x/clamp3', { efforts: ['high'], defaultEffort: null }),
  gm('x/peff', { perAgent: { pi: { efforts: ['minimal', 'ultra'], defaultEffort: 'ultra' } } }), gm('x/pdis', { perAgent: { pi: { defaultEnabled: false, supportsFastMode: true } } }),
  gm('x/full', { group: 'g', maxOutputTokens: 4096, supportsFastMode: false, description: 'd', sortOrder: 3, defaultEnabled: true, newSessionDefault: ['pi'], icon: 'i', modalities: { input: ['text'], output: ['text'] },
    displayPricing: { items: [{ label: 'a', price: 1, unit: 'u', quantity: 1 }] }, currency: 'USD', costDiscount: 0.1 }),
  gm('x/dp-nocur', { displayPricing: { items: [{ label: 'a', price: 1, unit: 'u', quantity: 1 }] } }), gm('x/noagents', { agents: ['codex'] }), gm('x/sort1', { sortOrder: 1 }), gm('x/sort0', { sortOrder: 0 }),
  gm('x/sortneg', { sortOrder: -5 }), gm('x/paysort', { availability: 'requires_payment', sortOrder: 2, defaultEnabled: true }), gm('x/embedon', { mode: 'embedding', defaultEnabled: true }),
  gm('x/audio', { mode: 'audio_speech', defaultEnabled: true, agents: ['pi'] }), gm('x/dis', { defaultEnabled: false }),
);
setGatewayModels(models);
const project = norm(projectPiModels(models));
const specCases = idList.concat(['hint/anthropic', 'x/pay']).map((id) => ({ id, spec: norm(resolvePiGatewayModelSpec('xd', id) ?? null), specUndefined: resolvePiGatewayModelSpec('xd', id) === undefined }));
const costCases = models.map((m) => ({ id: m.id, cost: norm(cost(m)) }));

console.log(JSON.stringify({ data, golden: { ids: idList, resolveCases, models: norm(models), project, specCases, costCases } }));
