// model-config 的 provider 视图：内嵌数据表（完整注册表 + 随包 Pi 目录行）与黄金样例。
// 黄金：对一批受限配置（pi runtime、openai-chat、无头、无 OAuth）运行上游真实的 buildUserProvider + buildRegistry。
import { BUNDLED_CATALOG, buildRegistry, buildUserProvider, isModelSelectableForNewRoute, parseModelsListResponse } from '@cindy/model-providers';
import { PROVIDER_MODEL_CATALOG } from '../../../cindy/packages/model-providers/src/providerModelCatalog.ts';
import { registryEntryDefaults } from '../../../cindy/packages/model-providers/src/modelMetadataLayers.ts';

const reg: any = BUNDLED_CATALOG.modelRegistry;
const norm = (v: any) => JSON.parse(JSON.stringify(v === undefined ? null : v));

const entries = reg.models.map((e: any) => ({
  id: e.id, status: e.status ?? null, nativeApi: e.nativeApi === undefined ? { u: 1 } : e.nativeApi,
  mode: registryEntryDefaults(reg, e).mode ?? null, modelRef: e.modelRef ?? null,
  routes: e.routes.map((r: any) => ({ providerId: r.providerId, modelId: r.modelId })),
}));
const bases = (reg.baseModels ?? []).map((b: any) => ({ id: b.id, aliases: b.aliases, defaults: b.defaults }));
const rules = reg.nativeApiRules ?? [];
const rows: any[] = [];
const sigs = new Map<string, number>();
for (const [provider, list] of Object.entries(PROVIDER_MODEL_CATALOG.providers as Record<string, any[]>)) {
  for (const r of list) {
    // 上游按 JSON.stringify(row) 判断“同一条记录”；紧凑表丢了 execution 细节，所以把去重标识一并导出。
    const text = JSON.stringify(r);
    if (!sigs.has(text)) sigs.set(text, sigs.size);
    rows.push({
      sig: sigs.get(text),
      provider, id: r.id, name: r.name, upstream: r.upstream, contextWindow: r.contextWindow, maxOutput: r.maxOutput, modalities: r.modalities,
      supportsImageInput: r.supportsImageInput, supportsFastMode: r.supportsFastMode, reasoning: r.reasoning, thinkingToggle: r.thinkingToggle,
      efforts: r.efforts, defaultEffort: r.defaultEffort, cost: r.cost, api: r.execution.pi.api,
      offNull: r.execution.pi.thinkingLevelMap?.off === null,
    });
  }
}
const data = norm({ schemaVersion: reg.schemaVersion, entries, bases, rules, rows });

// ── 黄金：配置集合 ──
const upstreams = [...new Set(rows.map((r) => r.upstream))].filter((u) => !u.includes('{'));
const byUp = (u: string) => rows.filter((r) => r.upstream === u).map((r) => r.id);
const cfgs: any[] = [];
let n = 0;
const mk = (baseUrl: string, models: any[], o: any = {}) => {
  cfgs.push({ id: `c${n++}`, name: `C${n}`, auth: { method: o.auth ?? 'apiKey' }, runtimes: { pi: { baseUrl, wireProtocol: 'openai-chat', ...(o.modelsUrl ? { modelsUrl: o.modelsUrl } : {}), models } }, connected: o.connected ?? true });
};
const m = (id: string, extra: any = {}) => ({ id, name: extra.name ?? id, reasoning: false, ...extra });
for (const up of upstreams) {
  const ids = byUp(up);
  mk(up, ids.slice(0, 3).map((i) => m(i)));
  mk(up, [m(ids[0], { contextWindow: 123456, supportsImageInput: true, defaultEnabled: false }), m('zz-unknown-model')], { auth: 'none' });
  mk(up, [m('gpt-9-turbo'), m('gpt-5.5'), m('claude-opus-9'), m('deepseek-v9'), m('qwen9-max'), m('glm-9.9'), m('gemini-9-pro'), m('kimi-k9')]);
}
const hosts = ['https://relay.example.com/v1', 'https://relay.example.com', 'http://127.0.0.1:11434/v1', 'http://localhost:1234/v1', 'https://api.openai.com/v1/', 'https://generativelanguage.googleapis.com/v1beta/openai',
  'https://token-plan-cn.xiaomimimo.com/v1', 'https://token-plan-ams.xiaomimimo.com:8443/v1', 'https://api.deepseek.com', 'https://api.anthropic.com'];
const ids = ['my-model', 'gpt-5.5', 'gpt-5.4', 'gpt-5.4-mini', 'gpt-4o', 'gpt-4o-mini', 'claude-sonnet-5', 'claude-opus-5', 'claude-opus-4-8', 'deepseek-chat', 'deepseek-v4-pro', 'deepseek-v4-flash',
  'qwen3.8-max', 'qwen3-max', 'glm-5.3', 'glm-4.6', 'kimi-k3', 'gemini-3.5-flash', 'gemini-3-pro', 'grok-4.6', 'text-embedding-3-large', 'gpt-image-2', 'gpt-realtime-2', 'o4-mini', 'Qwen/Qwen3-8B', 'meta/llama-4-70b', 'a/b/c', 'x'.repeat(130), 'model[1m]', 'chatgpt/gpt-5.5', 'xai/grok-4.6'];
for (const h of hosts) {
  for (let i = 0; i < ids.length; i += 4) mk(h, ids.slice(i, i + 4).map((id) => m(id, i % 8 === 0 ? { contextWindow: 64000 + i } : {})));
  mk(h, ids.map((id) => m(id)), { auth: 'none', modelsUrl: h + '/models' });
}
mk('https://relay.example.com/v1', [m('gpt-5.5'), m('gpt-5.4'), m('gpt-5.3'), m('gpt-4.1'), m('gpt-4o')], { connected: false });
mk('http://127.0.0.1:1/v1', [m('foo')], { auth: 'none', modelsUrl: 'https://remote.example.com/models' });
mk('https://relay.example.com/v1', [m('foo', { name: 'Custom Name' }), m('bar', { name: '' }), m('baz', { defaultEnabled: true, supportsImageInput: false })]);
// 与注册表里的 provider id 同名的自定义配置（目录里有这些路由）。
for (const pid of ['deepseek', 'gemini', 'minimax-cn', 'zhipu-glm-cn', 'moonshot-kimi-cn']) {
  const c: any = { id: pid, name: pid, auth: { method: 'apiKey' }, runtimes: { pi: { baseUrl: 'https://relay.example.com/v1', wireProtocol: 'openai-chat', models: ['deepseek-v4-pro', 'gemini-3.5-flash', 'glm-5.3', 'kimi-k3', 'MiniMax-M3', 'minimax-m3'].map((id) => m(id)) } } };
  cfgs.push({ ...c, connected: true });
}
const connected: Record<string, boolean> = {};
const providers = cfgs.map(({ connected: cn, ...c }) => { connected[c.id] = cn; return buildUserProvider(c); });
const views = norm(buildRegistry({ version: 'x', providers } as any, connected));

// ── 黄金：新路由准入 ──
const sel: any[] = [];
for (const v of views) for (const mm of v.models.pi ?? []) sel.push({ model: { id: mm.id, group: mm.group, mode: mm.mode, disabled: mm.disabled, availability: mm.availability }, user: true, want: isModelSelectableForNewRoute(mm, { userProvider: true }) });
const probeIds = ['claude-x', 'gpt-4o', 'gpt-image-2', 'dall-e-3', 'sora-2', 'veo-3', 'whisper-1', 'tts-1', 'gpt-4o-mini-tts', 'text-embedding-3', 'embed-english-v3.0', 'cohere/embed-x', 'rerank-v3', 'omni-moderation-latest',
  'babbage-002', 'openai/babbage-002', 'gpt-realtime-2', 'gpt-4o-realtime-preview', 'gemini-omni-x', 'codex/gpt-5.4', 'openai-codex/gpt-5.5', 'chatgpt/gpt-5.5', 'xai/grok-4', 'x-ai/grok-4.5', 'grok-4', 'gemini-3', 'Qwen/Qwen3-Embedding-8B',
  'seedance-2', 'wan-t2v', 'ai-gateway-doc', 'llava-video-7b', 'gpt-4o-audio-preview', 'qwen3-asr-flash-realtime', 'elevenlabs/scribe-v1', 'elevenlabs/eleven-v3', 'foo', 'a/b/gpt-5', 'cohere/command-r', 'speech-02', 'A/B-TTS'];
for (const id of probeIds) for (const group of [undefined, 'custom:x', 'anthropic', 'image', 'tts', 'ungrouped', 'china']) for (const mode of [undefined, 'chat', 'responses', 'embedding', 'image_generation', 'audio_speech'])
  for (const extra of [{}, { disabled: true }, { availability: 'requires_payment' }, { status: 'retired' }]) for (const user of [false, true]) {
    const model: any = { id, ...(group ? { group } : {}), ...(mode ? { mode } : {}), ...extra };
    sel.push({ model, user, want: isModelSelectableForNewRoute(model, { userProvider: user }) });
  }
// ── 黄金：/models 响应解析 ──
const list = [
  { data: [{ id: 'a' }, { id: 'b', name: 'B' }, { id: 'a' }] }, { models: [{ slug: 'x' }, { key: 'k' }, { model_name: 'mn' }, { model_id: 'mi' }] }, ['s1', 's2', 's1'], { data: 'no' }, null, 5, {},
  { data: [{ id: 'c', context_length: 8000 }, { id: 'd', context_window: '16000' }, { id: 'e', max_context_length: 3.7 }, { id: 'f', max_input_tokens: -1 }, { id: 'g', context_length: '1e3' }] },
  { data: [{ id: 'h', display_name: 'H Name', name: 'ignored' }, { id: 'i', name: '' }, { id: 'j', limit: { context: 4096, output: 512 } }] },
  { models: [{ name: 'models/gemini-x', displayName: 'Gem X', supportedGenerationMethods: ['generateContent'], inputTokenLimit: 100000 }, { name: 'models/emb', supportedGenerationMethods: ['embedContent'] }] },
  { data: [{ id: 'r1', reasoning: { supportedEfforts: ['low', 'high', 'none'], defaultEffort: 'high' } }, { id: 'r2', default_reasoning_level: 'medium' }, { id: 'r3', reasoning: { supported_efforts: [{ effort: 'low' }, { effort: 'xhigh' }], default_effort: 'xhigh' } }] },
  { data: [{ id: 'm1', modalities: { input: ['text', 'image'], output: ['text'] } }, { id: 'm2', architecture: { input_modalities: ['text'], output_modalities: ['text'] } }, { id: 'm3', input_modalities: ['image', 'text'] }] },
  { data: [{ id: 'v1', type: 'language' }, { id: 'v2', type: 'image' }] }, { data: [{ id: 'el1', model_id: 'el1', can_do_text_to_speech: true }, { id: 'el2', model_id: 'el2', can_do_text_to_speech: false }] },
];
const listCases = list.flatMap((j) => [undefined, 'https://api.example.com/v1/models', 'https://ai-gateway.vercel.sh/v1/models', 'https://openrouter.ai/api/v1/models', 'https://openhubs.zhidemai.com/v1/models']
  .map((u) => ({ json: j, url: u ?? null, out: norm(parseModelsListResponse(j, u)) })));

console.log(JSON.stringify({ data, golden: { configs: norm(cfgs.map(({ connected: _c, ...c }) => c)), connected, views, selectable: norm(sel), listCases: norm(listCases) } }));
