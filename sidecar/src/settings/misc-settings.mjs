// 其余设置页通道：上下文压缩、Pi 扩展包 / Pi 内核、用量历史、Bot 模型链、辅助模型。
// 形状与校验对齐 desktop，逐项注明出处。
import path from 'node:path';
import { normalizeBotModelChain } from '@cindy-desktop-shared/botModelChain';
import { isValidAuxiliaryModelListInput, normalizeAuxiliaryModelList } from '@cindy-desktop-shared/auxiliaryModelSettings';
import { isUtilityModelProviderKind } from '@cindy-desktop-shared/utilityModelProfiles';
import { registerInvoke } from '../ipc.mjs';
import { createOverrideSettingsFile, overrideWire } from './override-file.mjs';
import { fail } from './maker-settings.mjs';
import { isCurrentOwner } from '../owner.mjs';


// desktop bootstrap-electron.ts:1507 assertCompactionMutationOwner + chat-embedding-owner-stamp.ts
function assertLocalOwner(raw) {
  const ok = raw && typeof raw === 'object' && !Array.isArray(raw)
    && (typeof raw.dataOwnerId === 'string' || raw.dataOwnerId === null)
    && Number.isSafeInteger(raw.ownerGeneration);
  if (!ok) fail('INVALID_PARAMS', 'compaction owner stamp required');
  if (!isCurrentOwner(raw.dataOwnerId, raw.ownerGeneration)) {
    fail('PRECONDITION_FAILED', 'Compaction setting belongs to a stale account session.');
  }
}

export function registerMiscSettingsIpc({ settingsDir, piStatus = () => null }) {
  const file = (name) => path.join(settingsDir, name);

  // ── 上下文压缩阈值 ── desktop maker-host/compaction-settings-store.ts（50–95，默认 90）+ bootstrap-electron.ts:5164-5216
  const clampPct = (v) => (typeof v === 'number' && Number.isFinite(v) ? Math.min(95, Math.max(50, Math.round(v))) : 90);
  function compaction(prefix, fileName, key, label) {
    const store = createOverrideSettingsFile({
      file: file(fileName),
      defaults: { [key]: 90 },
      normalize: (r) => ({ [key]: clampPct(r?.[key]) }),
    });
    const wire = () => {
      const s = store.readState();
      return { pct: s.value[key], isCustomized: s.isCustomized, defaultPct: s.defaults[key] };
    };
    registerInvoke(`maker:${prefix}:get-pct`, async () => store.read()[key]);
    registerInvoke(`maker:${prefix}:get-state`, async () => wire());
    registerInvoke(`maker:${prefix}:reset-pct`, async (owner) => {
      assertLocalOwner(owner);
      store.reset();
      return wire();
    });
    registerInvoke(`maker:${prefix}:set-pct`, async (pct, owner) => {
      if (typeof pct !== 'number' || !Number.isFinite(pct)) fail('INVALID_PARAMS', `${label} pct required (number)`);
      assertLocalOwner(owner);
      store.writePatch({ [key]: clampPct(pct) });
      return wire();
    });
  }
  compaction('compaction', 'compaction-settings.json', 'claudeCodeAutoCompactPct', 'compaction');
  compaction('pi-compaction', 'pi-compaction-settings.json', 'piAutoCompactPct', 'pi compaction');

  // ── Pi 扩展包 ── desktop maker-ipc/register.ts:8449-8490、maker-host/pi-package-store.ts:2323。
  // 包管理需要调 Pi 原生 `pi install/remove`，Tauri 版未接：list 回 desktop「无 Pi 二进制」形状，mutate 校验后拒绝。
  registerInvoke('maker:pi-packages:list', async () => ({ available: false, packages: [] }));
  registerInvoke('maker:pi-packages:mutate', async (raw) => {
    if (!raw || typeof raw !== 'object' || Array.isArray(raw)) fail('INVALID_PARAMS', 'payload must be an object');
    if (!['install', 'remove', 'update', 'set-enabled'].includes(raw.action)) {
      fail('INVALID_PARAMS', 'action must be one of install, remove, update, set-enabled');
    }
    if (typeof raw.source !== 'string' || raw.source.length > 2048) fail('INVALID_PARAMS', 'invalid Pi extension source');
    if (Object.prototype.hasOwnProperty.call(raw, 'confirmed')) fail('INVALID_PARAMS', 'Renderer confirmation is not accepted');
    if (raw.enabled !== undefined && typeof raw.enabled !== 'boolean') fail('INVALID_PARAMS', 'enabled must be a boolean');
    if (raw.mutationTarget !== undefined
      && (typeof raw.mutationTarget !== 'string' || !/^cindy-pi-package:[a-f0-9]{64}$/.test(raw.mutationTarget))) {
      fail('INVALID_PARAMS', 'invalid Pi extension mutation target');
    }
    fail('PRECONDITION_FAILED', 'Pi extension management is not available in this build');
  });

  // ── Pi 内核 ── desktop maker-ipc/pi-kernel.ts + shared/piKernel.ts PiKernelState。
  // Tauri 版的 Pi 版本由 headless 清单钉死（agents.mjs），不提供在线检查与切换。
  const emptyCheck = () => ({ release: null, checkedAt: null, error: false });
  const kernelState = () => {
    let currentVersion = null;
    try {
      const s = piStatus();
      if (s?.installed) currentVersion = s.version ?? null;
    } catch { /* 状态读取失败按未安装 */ }
    return { currentVersion, restartRequired: false, official: emptyCheck(), upstream: emptyCheck(), operation: null };
  };
  registerInvoke('maker:agent:pi-kernel-state', async (check = false) => {
    if (typeof check !== 'boolean') fail('INVALID_PARAMS', 'check must be boolean');
    return kernelState();
  });
  registerInvoke('maker:agent:pi-kernel-install', async (body) => {
    if (!body || typeof body !== 'object' || Array.isArray(body)) fail('INVALID_PARAMS', 'Invalid Pi install request');
    if ((body.source !== 'official' && body.source !== 'upstream') || typeof body.version !== 'string'
      || !/^\d+\.\d+\.\d+$/.test(body.version) || Object.keys(body).some((k) => !['source', 'version'].includes(k))) {
      fail('INVALID_PARAMS', 'Invalid Pi install target');
    }
    // desktop 中版本已变时报 PRECONDITION_FAILED version-changed；这里没有可安装的候选版本
    fail('PRECONDITION_FAILED', 'version-changed');
  });

  // ── 用量历史 ── desktop main/usage/usageHistory.ts:1219 emptyUsageHistoryPayload。
  // Tauri 版还没有日消费账本；币种按 global 区域（USD，regionalMoney.ts:76）。
  const zeroMoney = (kind = 'actual-cost') => ({
    amount: 0, currency: 'USD', approximate: kind === 'value-estimate', kind,
    ...(kind === 'value-estimate' ? { estimateReasons: ['subscription-value'] } : {}),
  });
  const todayKey = () => {
    const d = new Date();
    return `${d.getFullYear()}-${String(d.getMonth() + 1).padStart(2, '0')}-${String(d.getDate()).padStart(2, '0')}`;
  };
  registerInvoke('maker:usage:history', async () => ({
    generatedAt: Date.now(),
    todayKey: todayKey(),
    stale: false,
    estimatesPending: false,
    days: [],
    modelDaily: [],
    models: [],
    streak: { current: 0, longest: 0 },
    totals: {
      today: zeroMoney(),
      last30Days: zeroMoney(),
      last30DaysWithEstimatedValue: zeroMoney(),
      last30DaysEstimatedValue: zeroMoney('value-estimate'),
      todayTokens: 0,
      last30DaysTokens: 0,
    },
    anomaly: { isAnomalous: false, trailing7DayAvg: null },
  }));

  // ── Bot 模型链 ── desktop maker-host/bot-model-chain-settings-store.ts + localDb/ipc/bots.ts:2373-2398。
  // 未自定义时 desktop 按已连接 provider 推导默认链（defaultBotModelChain）；Tauri 版本地没有 Bot 运行时，默认链为空。
  const botChain = createOverrideSettingsFile({
    file: file('bot-model-chain-settings.json'),
    defaults: { modelChain: [] },
    normalize: (r) => ({ modelChain: normalizeBotModelChain(r?.modelChain) }),
  });
  const botWire = () => {
    const s = botChain.readState();
    return { modelChain: s.value.modelChain, isCustomized: s.isCustomized };
  };
  registerInvoke('local-db:bots:model-chain-settings-get', async () => botWire());
  registerInvoke('local-db:bots:model-chain-settings-set', async (raw) => {
    const body = raw && typeof raw === 'object' && !Array.isArray(raw) ? raw : {};
    const normalized = normalizeBotModelChain(body.modelChain);
    // desktop writeBotModelChainSettings 抛普通 Error；这里给出明确错误码
    if (normalized.length === 0) fail('INVALID_PARAMS', 'Bot model chain must contain at least one route');
    botChain.writePatch({ modelChain: normalized }, { preserveDefaults: true });
    return botWire();
  });
  registerInvoke('local-db:bots:model-chain-settings-reset', async () => {
    try { botChain.reset(); } catch { fail('INTERNAL', 'Could not restore Bot model defaults'); }
    return botWire();
  });

  // ── 辅助模型 ── desktop maker-ipc/auxiliary-model-settings.ts。
  // 候选项来自一次性文本路由（buildTextOneshotPinOptions），Tauri 版未接，options 为空；
  // 空列表即「跟随自动链」，与 desktop 默认一致。没有可路由的 catalog pin。
  const aux = createOverrideSettingsFile({
    file: file('auxiliary-model-settings.json'),
    defaults: { models: [] },
    normalize: (r) => ({ models: normalizeAuxiliaryModelList(r?.models) }),
  });
  const auxWire = () => ({ ...overrideWire(aux.readState()), options: [] });
  registerInvoke('maker:auxiliary-model-settings:get', async () => auxWire());
  registerInvoke('maker:auxiliary-model-settings:set', async (raw) => {
    if (!raw || typeof raw !== 'object' || Array.isArray(raw)) fail('INVALID_PARAMS', 'auxiliary model settings patch required');
    const keys = Object.keys(raw);
    if (keys.length === 0 || keys.some((k) => k !== 'models')) fail('INVALID_PARAMS', 'auxiliary model settings patch has invalid keys');
    const models = raw.models;
    if (!isValidAuxiliaryModelListInput(models)) fail('INVALID_PARAMS', 'models must be a unique list of at most 3 auxiliary refs');
    // 轻量 profile key 总是允许；catalog pin 只接受已持久化的（Tauri 版没有可路由的候选）
    const persisted = new Set(aux.read().models);
    if (models.some((m) => !isUtilityModelProviderKind(m) && !persisted.has(m))) fail('INVALID_PARAMS', 'models contains a catalog pin that is not currently routable');
    aux.writePatch({ models });
    return auxWire();
  });
}
