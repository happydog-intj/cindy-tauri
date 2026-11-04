// Maker 侧「覆盖项」设置：协作 / 资源 / git 安全 / LSP / 静默加密重试 / 运行时兜底 / 聊天向量 /
// subagent 模型 / 视觉桥 / 记忆开关。形状、默认值、校验对齐 desktop，逐项注明出处。
// 这些设置只落盘，Tauri 版运行时（只有 Pi）暂不读取，见文件末尾 NOT_WIRED。
import path from 'node:path';
import {
  SUBAGENT_MODEL_SETTINGS_DEFAULTS,
  isValidSubagentModelIdInput,
  normalizeSubagentModelId,
  reconcileSubagentModelSettingsPatch,
} from '@cindy-desktop-shared/subagentModelSettings';
import { registerInvoke } from '../ipc.mjs';
import { createOverrideSettingsFile, overrideWire } from './override-file.mjs';

export function fail(code, msg) {
  const err = new Error(`[${code}] ${msg}`);
  err.code = code;
  throw err;
}
// desktop main/utils/ipcValidate.ts requireBoolean
export function requireBoolean(v, name) {
  if (typeof v !== 'boolean') fail('INVALID_PARAMS', `${name} must be a boolean`);
  return v;
}

const clamp = (v, min, max) => (Number.isFinite(v) ? Math.max(min, Math.min(max, v)) : min);
const boolOr = (v, d) => (typeof v === 'boolean' ? v : d);

export function registerMakerSettingsIpc({ settingsDir }) {
  const file = (name) => path.join(settingsDir, name);

  // ── 协作 ── desktop maker-host/collaboration-settings-store.ts:14-35，校验 maker-ipc/register.ts:2378-2414
  const COLLAB_DEFAULTS = { workerSoftLimit: 5, workerHardLimit: 8, workerIdleReleaseMinutes: 0 };
  const collab = createOverrideSettingsFile({
    file: file('collaboration-settings.json'),
    defaults: COLLAB_DEFAULTS,
    normalize: (r) => {
      const soft = clamp(typeof r?.workerSoftLimit === 'number' ? r.workerSoftLimit : 5, 1, 20);
      return {
        workerSoftLimit: soft,
        workerHardLimit: clamp(typeof r?.workerHardLimit === 'number' ? r.workerHardLimit : 8, soft, 20),
        workerIdleReleaseMinutes: typeof r?.workerIdleReleaseMinutes === 'number' ? clamp(r.workerIdleReleaseMinutes, 0, 120) : 0,
      };
    },
  });
  registerInvoke('maker:collaboration-settings:get', async () => overrideWire(collab.readState()));
  registerInvoke('maker:collaboration-settings:set', async (body) => {
    if (!body || typeof body.key !== 'string') fail('INVALID_PARAMS', 'key required');
    const { key, value } = body;
    if (!(key in COLLAB_DEFAULTS)) fail('INVALID_PARAMS', `unknown key: ${key}`);
    if (typeof value !== 'number' || !Number.isInteger(value)) fail('INVALID_PARAMS', `${key} must be an integer`);
    const cur = collab.read();
    if (key === 'workerIdleReleaseMinutes') {
      if (value < 0) fail('INVALID_PARAMS', `${key} must be >= 0`);
      if (value > 120) fail('INVALID_PARAMS', `${key} must be <= 120`);
    } else {
      if (value < 1) fail('INVALID_PARAMS', `${key} must be >= 1`);
      if (value > 20) fail('INVALID_PARAMS', `${key} must be <= 20`);
      if (key === 'workerSoftLimit' && value > cur.workerHardLimit) fail('INVALID_PARAMS', 'workerSoftLimit must be <= workerHardLimit');
      if (key === 'workerHardLimit' && value < cur.workerSoftLimit) fail('INVALID_PARAMS', 'workerHardLimit must be >= workerSoftLimit');
    }
    return overrideWire(collab.writePatch({ [key]: value }));
  });
  registerInvoke('maker:collaboration-settings:reset', async () => {
    collab.reset();
    return overrideWire(collab.readState());
  });

  // ── Agent 资源 ── desktop maker-host/agent-resource-settings-store.ts、maker-ipc/agent-resource-settings-ipc.ts
  const PRIORITIES = ['normal', 'low', 'lowest'];
  const resource = createOverrideSettingsFile({
    file: file('agent-resource-settings.json'),
    defaults: { maxConcurrentCommands: 0, processPriority: 'normal', capToolchainThreads: false },
    normalize: (r) => ({
      maxConcurrentCommands: typeof r?.maxConcurrentCommands === 'number' && Number.isFinite(r.maxConcurrentCommands)
        ? Math.max(0, Math.min(64, Math.floor(r.maxConcurrentCommands))) : 0,
      processPriority: r?.processPriority === 'low' || r?.processPriority === 'lowest' ? r.processPriority : 'normal',
      capToolchainThreads: r?.capToolchainThreads === true,
    }),
  });
  registerInvoke('maker:agent-resource-settings:get', async () => overrideWire(resource.readState()));
  registerInvoke('maker:agent-resource-settings:set', async (body) => {
    if (!body || typeof body.key !== 'string') fail('INVALID_PARAMS', 'key required');
    const { key, value } = body;
    if (key === 'maxConcurrentCommands') {
      if (typeof value !== 'number' || !Number.isInteger(value)) fail('INVALID_PARAMS', `${key} must be an integer`);
      if (value < 0) fail('INVALID_PARAMS', `${key} must be >= 0`);
      if (value > 64) fail('INVALID_PARAMS', `${key} must be <= 64`);
    } else if (key === 'processPriority') {
      if (!PRIORITIES.includes(value)) fail('INVALID_PARAMS', `${key} must be one of ${PRIORITIES.join('/')}`);
    } else if (key === 'capToolchainThreads') {
      if (typeof value !== 'boolean') fail('INVALID_PARAMS', `${key} must be a boolean`);
    } else {
      fail('INVALID_PARAMS', `unknown key: ${key}`);
    }
    return overrideWire(resource.writePatch({ [key]: value }));
  });
  registerInvoke('maker:agent-resource-settings:reset', async () => {
    resource.reset();
    return overrideWire(resource.readState());
  });

  // ── Git 安全 ── desktop maker-host/git-safety-settings-store.ts、bootstrap-electron.ts:5346-5360（wire 见 :10861）
  const GIT_MODES = ['off', 'existing-git', 'all-projects'];
  const git = createOverrideSettingsFile({
    file: file('git-safety-settings.json'),
    defaults: { mode: 'existing-git' },
    normalize: (r) => {
      const legacy = typeof r?.autoSnapshotEnabled === 'boolean' ? (r.autoSnapshotEnabled ? 'all-projects' : 'off') : undefined;
      const mode = legacy !== undefined && (r.mode === undefined || r.mode === 'existing-git')
        ? legacy : GIT_MODES.includes(r?.mode) ? r.mode : 'existing-git';
      return { mode };
    },
    mergeOverrides: ({ patch, next, overrides }) => {
      // 同 desktop mergeOverrides：显式选过的 mode 即使等于默认值也记为自定义
      if ('mode' in patch) { overrides.mode = next.mode; delete overrides.autoSnapshotEnabled; }
      return overrides;
    },
  });
  const gitWire = () => {
    const s = git.readState();
    return {
      mode: s.value.mode,
      autoSnapshotEnabled: s.value.mode !== 'off',
      autoInitProjectGit: s.value.mode === 'all-projects',
      isCustomized: s.isCustomized,
      defaultMode: s.defaults.mode,
      defaultAutoSnapshotEnabled: s.defaults.mode !== 'off',
    };
  };
  registerInvoke('maker:git-safety:get', async () => gitWire());
  registerInvoke('maker:git-safety:set', async (mode) => {
    if (typeof mode === 'boolean') mode = mode ? 'all-projects' : 'off';
    if (!GIT_MODES.includes(mode)) fail('INVALID_PARAMS', 'git safety mode required');
    git.writePatch({ mode });
    return gitWire();
  });
  registerInvoke('maker:git-safety:reset', async () => {
    git.reset();
    return gitWire();
  });

  // ── LSP 模式 ── desktop maker-host/lsp-mode-store.ts（存整份对象）、bootstrap-electron.ts:5275-5285
  const lsp = createOverrideSettingsFile({
    file: file('lsp-mode-settings.json'),
    defaults: { enabled: false },
    normalize: (r) => ({ enabled: boolOr(r?.enabled, false) }),
  });
  registerInvoke('maker:lsp-mode:get', async () => lsp.read());
  registerInvoke('maker:lsp-mode:set', async (enabled) => {
    if (typeof enabled !== 'boolean') fail('INVALID_PARAMS', 'lsp mode enabled required (boolean)');
    lsp.writePatch({ enabled }, { preserveDefaults: true });
    return { effective: 'next-session' };
  });

  // ── 单开关类 ── wire 为 {enabled, isCustomized, defaultEnabled}（desktop bootstrap-electron.ts:10815/10824/10851）
  function toggle({ name, fileName, defaultEnabled, setMessage, effective }) {
    const store = createOverrideSettingsFile({
      file: file(fileName),
      defaults: { enabled: defaultEnabled },
      normalize: (r) => ({ enabled: boolOr(r?.enabled, defaultEnabled) }),
    });
    const wire = () => {
      const s = store.readState();
      return { enabled: s.value.enabled, isCustomized: s.isCustomized, defaultEnabled: s.defaults.enabled };
    };
    const withEffective = (w) => (effective ? { ...w, effective } : w);
    registerInvoke(`maker:${name}:get`, async () => wire());
    registerInvoke(`maker:${name}:set`, async (enabled) => {
      if (typeof enabled !== 'boolean') fail('INVALID_PARAMS', setMessage);
      store.writePatch({ enabled });
      return withEffective(wire());
    });
    registerInvoke(`maker:${name}:reset`, async () => {
      store.reset();
      return withEffective(wire());
    });
  }
  // desktop silent-encrypted-retry-store.ts（默认 true）、bootstrap-electron.ts:5126-5144
  toggle({ name: 'silent-encrypted-retry', fileName: 'silent-encrypted-retry-settings.json', defaultEnabled: true,
    setMessage: 'silent encrypted retry enabled required (boolean)', effective: 'immediate' });
  // desktop session-runtime-fallback-store.ts（默认 false）、bootstrap-electron.ts:5146-5161
  toggle({ name: 'session-runtime-fallback', fileName: 'session-runtime-fallback-settings.json', defaultEnabled: false,
    setMessage: 'session runtime fallback enabled required (boolean)', effective: 'immediate' });

  // ── 聊天向量 ── desktop chat-embedding-settings-store.ts + bootstrap-electron.ts:5290-5345。
  // 本机模式下 isChatEmbeddingAvailable() 为 false（需要云端组织账号），wire 恒为关闭；打开时同 desktop 报 UNSUPPORTED_CAPABILITY。
  const chatEmbeddingWire = () => ({ enabled: false, isCustomized: false, defaultEnabled: false });
  registerInvoke('maker:chat-embedding:get', async () => chatEmbeddingWire());
  registerInvoke('maker:chat-embedding:set', async (enabled) => {
    if (typeof enabled !== 'boolean') fail('INVALID_PARAMS', 'chat embedding enabled required (boolean)');
    if (enabled) fail('UNSUPPORTED_CAPABILITY', 'Chat embedding is not available for this account or region.');
    return chatEmbeddingWire();
  });
  registerInvoke('maker:chat-embedding:reset', async () => chatEmbeddingWire());

  // ── Subagent 模型 ── desktop maker-host/subagent-model-settings-store.ts、bootstrap-electron.ts:5028-5105 / :10713 / :10790
  // Tauri 版没有 Codex 进程，codexRestartDeferred 恒为 false（同 desktop 无 maker 时的分支）。
  const SUBAGENT_KEYS = ['claudeCode', 'claudeCodeProviderId', 'codexSmartSubagentRouting'];
  const subagent = createOverrideSettingsFile({
    file: file('subagent-model-settings.json'),
    defaults: SUBAGENT_MODEL_SETTINGS_DEFAULTS,
    normalize: (r) => {
      const claudeCode = normalizeSubagentModelId(r?.claudeCode);
      return {
        claudeCode,
        claudeCodeProviderId: claudeCode === null ? null : normalizeSubagentModelId(r?.claudeCodeProviderId),
        codexSmartSubagentRouting: r?.codexSmartSubagentRouting === true,
      };
    },
    mergeOverrides: ({ patch, next, defaults, overrides }) => {
      const out = Object.fromEntries(Object.entries(overrides).filter(([k]) => SUBAGENT_KEYS.includes(k)));
      for (const k of Object.keys(patch)) {
        if (next[k] === defaults[k]) delete out[k];
        else out[k] = next[k];
      }
      return out;
    },
  });
  registerInvoke('maker:subagent-model-settings:get', async () => overrideWire(subagent.readState()));
  registerInvoke('maker:subagent-model-settings:set', async (raw) => {
    if (!raw || typeof raw !== 'object' || Array.isArray(raw)) fail('INVALID_PARAMS', 'subagent model settings patch required (object)');
    const patch = {};
    for (const key of ['claudeCode', 'claudeCodeProviderId']) {
      if (!(key in raw)) continue;
      if (!isValidSubagentModelIdInput(raw[key])) fail('INVALID_PARAMS', `subagent model ${key} must be a valid string or null`);
      patch[key] = normalizeSubagentModelId(raw[key]);
    }
    if ('codexSmartSubagentRouting' in raw) {
      if (typeof raw.codexSmartSubagentRouting !== 'boolean') fail('INVALID_PARAMS', 'subagent codexSmartSubagentRouting must be boolean');
      patch.codexSmartSubagentRouting = raw.codexSmartSubagentRouting;
    }
    const parsed = reconcileSubagentModelSettingsPatch(patch, subagent.read());
    return { ...overrideWire(subagent.writePatch(parsed)), codexRestartDeferred: false };
  });
  registerInvoke('maker:subagent-model-settings:reset', async () => {
    subagent.reset();
    return { ...overrideWire(subagent.readState()), codexRestartDeferred: false };
  });

  // ── 视觉桥 ── desktop vision-bridge/vision-bridge-settings-store.ts、bootstrap-electron.ts:5108-5122、patch 解析 :10733
  const backendRef = (r) => {
    if (!r || typeof r !== 'object') return null;
    const providerId = typeof r.providerId === 'string' ? r.providerId.trim() : '';
    const modelId = typeof r.modelId === 'string' ? r.modelId.trim() : '';
    return providerId && modelId ? { providerId, modelId } : null;
  };
  const vision = createOverrideSettingsFile({
    file: file('vision-bridge-settings.json'),
    defaults: { enabled: false, targetModels: [], primary: null, fallback: null },
    normalize: (r) => ({
      enabled: boolOr(r?.enabled, false),
      targetModels: Array.isArray(r?.targetModels)
        ? r.targetModels.map((m) => (typeof m === 'string' ? m.trim() : '')).filter(Boolean) : [],
      primary: backendRef(r?.primary),
      fallback: backendRef(r?.fallback),
    }),
  });
  registerInvoke('maker:vision-bridge-settings:get', async () => overrideWire(vision.readState()));
  registerInvoke('maker:vision-bridge-settings:set', async (raw) => {
    if (!raw || typeof raw !== 'object' || Array.isArray(raw)) fail('INVALID_PARAMS', 'vision bridge settings patch required (object)');
    const patch = {};
    if ('enabled' in raw) {
      if (typeof raw.enabled !== 'boolean') fail('INVALID_PARAMS', 'vision bridge enabled must be boolean');
      patch.enabled = raw.enabled;
    }
    if ('targetModels' in raw) {
      if (!Array.isArray(raw.targetModels)) fail('INVALID_PARAMS', 'vision bridge targetModels must be a string array');
      const trimmed = raw.targetModels.map((m) => (typeof m === 'string' ? m.trim() : ''));
      if (trimmed.some((m) => !m)) fail('INVALID_PARAMS', 'vision bridge targetModels must be a non-blank string array');
      patch.targetModels = trimmed;
    }
    for (const key of ['primary', 'fallback']) {
      if (!(key in raw)) continue;
      const v = raw[key];
      if (v === null) { patch[key] = null; continue; }
      if (!v || typeof v !== 'object' || Array.isArray(v) || typeof v.providerId !== 'string' || typeof v.modelId !== 'string') {
        fail('INVALID_PARAMS', `vision bridge ${key} must be { providerId, modelId } or null`);
      }
      const providerId = v.providerId.trim();
      const modelId = v.modelId.trim();
      if (!providerId || !modelId) fail('INVALID_PARAMS', `vision bridge ${key} providerId/modelId must not be blank`);
      patch[key] = { providerId, modelId };
    }
    return overrideWire(vision.writePatch(patch));
  });
  registerInvoke('maker:vision-bridge-settings:reset', async () => {
    vision.reset();
    return overrideWire(vision.readState());
  });

  // ── 记忆 ── desktop maker-host/memory-settings-store.ts（默认全 true）、maker-ipc/register.ts:25888-26035、bootstrap-electron.ts:4916-4940
  // Tauri 版的 Maker 没有挂 MakerMemoryManager（maker-host.mjs:122），所以：
  // - Pi：PiAgent.getMemoryStatus 实际返回 enabled=false（manager 缺省），这里直接返回同形状；
  // - claude-code / codex：未注册，同 desktop requireAgent 抛错 → renderer 显示「不支持」；
  // - Cindy Memory 开关（maker）：设置照常落盘，manager 不存在时 set-enabled / reset 同 desktop 报 MAKER_MEMORY_NOT_READY。
  const memory = createOverrideSettingsFile({
    file: file('memory-settings.json'),
    defaults: { maker: true, claudeCode: true, codex: true, pi: true },
    normalize: (r) => ({
      maker: boolOr(r?.maker, true), claudeCode: boolOr(r?.claudeCode, true), codex: boolOr(r?.codex, true), pi: boolOr(r?.pi, true),
    }),
  });
  const requireAgentKind = (kind) => {
    if (kind !== 'claude-code' && kind !== 'codex' && kind !== 'pi') {
      fail('INVALID_PARAMS', `agentKind required (claude-code | codex | pi), got ${String(kind)}`);
    }
    // desktop packages/maker-core/src/maker.ts:1924 requireAgent 的报错文案
    if (kind !== 'pi') throw new Error(`Agent '${kind}' is not registered (available: pi)`);
  };
  registerInvoke('maker:memory:get-settings', async () => memory.read());
  registerInvoke('maker:memory:get-settings-state', async () => overrideWire(memory.readState()));
  registerInvoke('maker:memory:preserve-legacy-maker-disabled', async (legacy) => {
    // desktop memory-settings-store.ts preserveLegacyMakerMemoryDisabled
    const state = memory.readState();
    if (state.customizedKeys.includes('maker')) return state.value;
    if (legacy === true) return memory.writePatch({ maker: true }, { preserveDefaults: true }).value;
    if (legacy === false) return memory.writePatch({ maker: false }).value;
    return state.value;
  });
  registerInvoke('maker:memory:get', async (kind) => {
    requireAgentKind(kind);
    // packages/maker-core/src/agents/pi/index.ts:8759：无 manager 时 enabled=false，无 stats
    return { enabled: false, source: memory.readState().customizedKeys.includes('pi') ? 'host-runtime' : 'agent-default' };
  });
  registerInvoke('maker:memory:set', async (kind, enabled) => {
    requireAgentKind(kind);
    if (typeof enabled !== 'boolean') fail('INVALID_PARAMS', 'enabled required (boolean)');
    const s = memory.writePatch({ pi: enabled });
    return { effective: 'next-session', isCustomized: s.isCustomized, customizedKeys: s.customizedKeys, defaults: s.defaults };
  });
  registerInvoke('maker:memory:reset', async (kind) => {
    requireAgentKind(kind);
    return { removedEntries: 0 };
  });
  registerInvoke('maker:memory:reset-settings', async () => {
    memory.reset();
    return { ...overrideWire(memory.readState()), codexRestartDeferred: false };
  });
  registerInvoke('maker:maker-memory:set-enabled', async (enabled) => {
    if (typeof enabled !== 'boolean') fail('INVALID_PARAMS', 'enabled required (boolean)');
    fail('MAKER_MEMORY_NOT_READY', 'maker memory not initialized');
  });
  registerInvoke('maker:maker-memory:reset', async () => fail('MAKER_MEMORY_NOT_READY', 'maker memory not initialized'));
}
