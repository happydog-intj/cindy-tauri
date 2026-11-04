// 应用级设置：外观 / 快捷键 / 本地主题 / 窗口行为（菜单栏、开机启动、防休眠）/ 更新通道 /
// 终端 shell 偏好 / 外设（Work Louder、手柄）。形状与校验对齐 desktop，逐项注明出处。
import path from 'node:path';
import {
  DEFAULT_APPEARANCE_SETTINGS,
  WALLPAPER_IDS,
  clampAppearanceCodeSize,
  clampAppearanceTerminalSize,
  clampAppearanceUiSize,
  clampAppearanceWallpaperBlur,
  clampAppearanceWallpaperOverlay,
  clampAppearanceWallpaperVisibility,
  clampAppearanceWindowZoom,
  normalizeAppearanceSettings,
  normalizeCustomWallpaperUrl,
} from '@cindy-desktop-shared/appearanceSettings';
import {
  comboToElectronAccelerator,
  findAppShortcutConflict,
  getAppShortcutDefinition,
  isAppShortcutAvailableOnPlatform,
  isAppShortcutComboBindable,
  isAppShortcutId,
  isGlobalAppShortcutComboUsable,
  normalizeAppShortcutCombo,
  normalizeAppShortcutOverrides,
} from '@cindy-desktop-shared/appShortcuts';
import { isSystemReservedShortcut } from '@cindy-desktop-shared/keyboardReserved';
import { supportsBetaUpdateChannel } from '@cindy-desktop-shared/updateChannelCapability';
import { WORKLOUDER_CODEX_AGENT_SLOT_COUNT, WORKLOUDER_CODEX_EMPTY_DEVICE_STATE, WORKLOUDER_MODELS, createWorkLouderCodexDefaultSettings } from '@cindy-desktop-shared/workLouderCodex';
import { GAMEPAD_FAMILIES, createXboxGamepadDefaultSettings, emptyGamepadDevice } from '@cindy-desktop-shared/xboxGamepad';
import { isClassicalReplyStyle } from '@cindy/maker-shared/classical-reply';
import { emitToRenderer, registerInvoke, registerSend, registerSync } from '../ipc.mjs';
import { readJson, writeJsonAtomic, createOverrideSettingsFile } from './override-file.mjs';
import { fail, requireBoolean } from './maker-settings.mjs';

export function registerAppSettingsIpc({ settingsDir, platform = process.platform, arch = process.arch }) {
  const file = (name) => path.join(settingsDir, name);

  // ── 外观 ── desktop main/appearance-settings-store.ts（16KB 上限、坏文件保留）+ appearance-settings-ipc.ts
  const appearance = createOverrideSettingsFile({
    file: file('appearance-settings.json'),
    defaults: DEFAULT_APPEARANCE_SETTINGS,
    normalize: normalizeAppearanceSettings,
    maxBytes: 16 * 1024,
    preserveUnreadableFile: true,
  });
  // desktop main/custom-wallpaper-settings.ts：只存 {url}
  const customWallpaper = createOverrideSettingsFile({
    file: file('custom-wallpaper.json'),
    defaults: { url: '' },
    normalize: (r) => ({ url: normalizeCustomWallpaperUrl(r?.url) }),
    maxBytes: 4096,
    preserveUnreadableFile: true,
  });
  // desktop appearance-settings-store.ts:33 readAppearanceSettings
  const readAppearance = () => {
    const value = appearance.read();
    const customWallpaperUrl = customWallpaper.read().url;
    return { ...value, customWallpaperUrl, wallpaperId: value.wallpaperId === 'custom' && !customWallpaperUrl ? 'none' : value.wallpaperId };
  };
  const broadcastAppearance = (settings) => emitToRenderer('appearance-settings:changed', settings);
  registerSync('appearance-settings:get-sync', () => readAppearance());
  registerInvoke('appearance-settings:get', async () => {
    // desktop :get 返回 OverrideSettingsState 并顺带广播一次
    const state = { ...appearance.readState(), value: readAppearance() };
    broadcastAppearance(state.value);
    return state;
  });
  registerInvoke('appearance-settings:set-patch', async (raw) => {
    appearance.writePatch(parseAppearancePatch(raw));
    const settings = readAppearance();
    broadcastAppearance(settings);
    return settings;
  });
  registerInvoke('appearance-settings:reset', async () => {
    appearance.reset();
    const settings = readAppearance();
    broadcastAppearance(settings);
    return settings;
  });
  // 自定义壁纸依赖 cindy-media:// 协议与 sharp 转码，Tauri 版暂无；返回 null 即 desktop「用户取消」形状。
  registerInvoke('appearance-settings:import-wallpaper', async () => null);
  registerInvoke('appearance-settings:remove-wallpaper', async () => {
    customWallpaper.reset();
    const settings = readAppearance();
    broadcastAppearance(settings);
    return settings;
  });

  // ── 本地主题 ── desktop main/local-themes/loader.ts：目录 ~/.cindy/themes。
  // 主题 JSON 解析与品牌图检测依赖 nativeImage，暂未移植：只回空列表，保持与 sync 版一致。
  const emptyThemes = () => ({ success: true, themes: [], diagnostics: [] });
  registerSync('local-themes:list-sync', () => emptyThemes());
  registerInvoke('local-themes:list', async () => emptyThemes());

  // ── 快捷键 ── desktop main/app-shortcuts/AppShortcutStore.ts + index.ts:84-170
  const shortcutsFile = file('app-shortcuts.v1.json');
  let shortcutOverrides = null;
  const loadShortcuts = () => {
    if (shortcutOverrides) return shortcutOverrides;
    try {
      const parsed = readJson(shortcutsFile);
      shortcutOverrides = normalizeAppShortcutOverrides(parsed && typeof parsed === 'object' ? parsed.overrides : undefined, platform);
    } catch (err) {
      process.stderr.write(`[settings] app shortcut overrides read failed, using defaults: ${err?.message ?? err}\n`);
      shortcutOverrides = {};
    }
    return shortcutOverrides;
  };
  const replaceShortcuts = (next) => {
    try {
      writeJsonAtomic(shortcutsFile, { version: 1, overrides: next });
    } catch (err) {
      throw new Error(`[INTERNAL] failed to save app shortcut overrides: ${err?.message ?? err}`);
    }
    shortcutOverrides = next;
    emitToRenderer('app-shortcuts:changed', { overrides: { ...next }, globalFailures: [] });
  };
  const platformFamily = platform === 'darwin' ? 'mac' : platform === 'win32' ? 'windows' : 'other';
  // desktop AppShortcutStore.setOverride，返回拒绝原因或 null
  const setShortcutOverride = (id, combo) => {
    if (!isAppShortcutId(id)) return 'unknown-id';
    const def = getAppShortcutDefinition(id);
    if (!def.rebindable) return 'not-rebindable';
    if (!isAppShortcutAvailableOnPlatform(id, platform)) return 'platform-unavailable';
    const current = loadShortcuts();
    if (combo === null) {
      replaceShortcuts({ ...current, [id]: null });
      return null;
    }
    const normalized = normalizeAppShortcutCombo(combo);
    if (!normalized) return 'invalid-combo';
    if (!isAppShortcutComboBindable(normalized)) return 'not-bindable';
    if (isSystemReservedShortcut(normalized, platformFamily)) return 'system-reserved';
    if (def.menuBacked && platform === 'darwin' && comboToElectronAccelerator(normalized, 'darwin') === null) return 'menu-inexpressible';
    if (def.scope === 'global' && !isGlobalAppShortcutComboUsable(normalized, platform)) return 'global-unusable';
    if (findAppShortcutConflict(id, normalized, current, platform)) return 'conflict';
    replaceShortcuts({ ...current, [id]: normalized });
    return null;
  };
  // desktop app-shortcuts/index.ts:138-158
  const REJECTION_MESSAGES = {
    'unknown-id': (id) => `unknown app shortcut id: ${String(id)}`,
    'not-rebindable': (id) => `app shortcut is not rebindable: ${String(id)}`,
    'platform-unavailable': (id) => `app shortcut not available on this platform: ${String(id)}`,
    'invalid-combo': () => 'invalid shortcut combo',
    'not-bindable': () => 'shortcut combo not bindable (bare keys limited to F1-F24 / Shift+non-printable)',
    'system-reserved': () => 'shortcut combo is reserved by the operating system',
    'menu-inexpressible': () => 'shortcut combo cannot be expressed as a menu accelerator on macOS',
    'global-unusable': () => 'shortcut combo cannot be used as a system-wide shortcut',
    conflict: () => 'shortcut combo conflicts with another shortcut in an overlapping scope',
  };
  // globalFailures：Tauri 版不注册系统级快捷键，恒为空
  registerSync('app-shortcuts:get', () => ({ overrides: { ...loadShortcuts() }, platform, globalFailures: [] }));
  registerInvoke('app-shortcuts:set-override', async (id, combo) => {
    const rejection = setShortcutOverride(id, combo);
    if (rejection) fail('INVALID_PARAMS', REJECTION_MESSAGES[rejection](id));
    return { overrides: { ...loadShortcuts() } };
  });
  registerInvoke('app-shortcuts:clear-override', async (id) => {
    const current = loadShortcuts();
    if (isAppShortcutId(id) && id in current) {
      const next = { ...current };
      delete next[id];
      replaceShortcuts(next);
    }
    return { overrides: { ...loadShortcuts() } };
  });
  registerInvoke('app-shortcuts:reset-all', async () => {
    if (Object.keys(loadShortcuts()).length > 0) replaceShortcuts({});
    return { overrides: {} };
  });
  // 录制期间 desktop 暂停菜单 accelerator；Tauri 版无原生菜单快捷键，直接吞掉
  registerSend('app-shortcuts:set-recording', () => {});

  // ── 语言 ── app-menu:set-locale 已由 desktop-shell.mjs 注册（含 setMainLocale）。
  // desktop bootstrap-electron.ts:3190：非目录内的风格静默忽略；Tauri 版暂无回复脚注消费方。
  let classicalStyle = null;
  registerInvoke('app-locale:set-classical-style', async (style) => {
    if (isClassicalReplyStyle(style)) classicalStyle = style;
    return { ok: true };
  });

  // ── 菜单栏额度 ── desktop main/menubar/service.ts:106：无服务时 get/set 都回 {available:false, enabled:false}
  registerInvoke('menubar:get-state', async () => ({ available: false, enabled: false }));
  registerInvoke('menubar:set-enabled', async (enabled) => {
    requireBoolean(enabled, 'enabled');
    return { available: false, enabled: false };
  });

  // ── 开机启动 ── desktop main/login-item-settings.ts：非 win32/darwin 报 platform，未打包报 development。
  // Tauri 版还没接 autostart 插件，按「开发构建」形状回不可用。
  const loginItemState = () => ({
    available: false,
    unavailableReason: platform !== 'win32' && platform !== 'darwin' ? 'platform' : 'development',
    enabled: false,
    requiresApproval: false,
  });
  registerInvoke('login-item:get', async () => loginItemState());
  registerInvoke('login-item:set', async (enabled) => {
    requireBoolean(enabled, 'enabled');
    fail('UNSUPPORTED_CAPABILITY', 'Login startup requires an installed Windows or macOS app');
  });

  // ── 设备互联（只保留本地防休眠）── desktop main/device-link/settings-store.ts + ipc.ts:316-356
  const deviceLinkFile = file('device-link-settings.json');
  const DEVICE_LINK_DEFAULTS = {
    remoteDesktopEnabled: false, remoteControlEnabled: false, keepAwake: false,
    revokedControllers: [], disabledControlDeviceIds: [], lastKnownDeviceNames: {},
  };
  const readDeviceLink = () => {
    let raw;
    try { raw = readJson(deviceLinkFile); } catch { raw = undefined; }
    const r = raw && typeof raw === 'object' ? raw : {};
    return {
      ...DEVICE_LINK_DEFAULTS,
      remoteDesktopEnabled: r.remoteDesktopEnabled === true,
      remoteControlEnabled: r.remoteControlEnabled === true,
      keepAwake: r.keepAwake === true,
      revokedControllers: Array.isArray(r.revokedControllers) ? r.revokedControllers : [],
      disabledControlDeviceIds: Array.isArray(r.disabledControlDeviceIds) ? r.disabledControlDeviceIds : [],
      lastKnownDeviceNames: r.lastKnownDeviceNames && typeof r.lastKnownDeviceNames === 'object' ? r.lastKnownDeviceNames : {},
    };
  };
  registerInvoke('device-link:get-state', async () => ({
    remoteControlEnabled: false,
    keepAwake: readDeviceLink().keepAwake,
    linkStatus: 'stopped',
    connectionIssue: null,
    standby: false,
    controlledBy: [],
    revokedControllers: [],
    disabledControlDeviceIds: [],
    unresponsiveDeviceIds: [],
  }));
  registerInvoke('device-link:set-keep-awake', async (enabled) => {
    requireBoolean(enabled, 'enabled');
    writeJsonAtomic(deviceLinkFile, { ...readDeviceLink(), keepAwake: enabled });
    emitToRenderer('device-link:keep-awake-changed', { keepAwake: enabled });
    return { keepAwake: enabled };
  });
  // desktop ipc.ts:1706 gated()：无账号时 requireDeviceLinkCapability 抛 PERMISSION_DENIED；
  // 这里用允许的错误码表达同一前置条件，renderer 按 listFailed 处理。
  registerInvoke('device-link:list-devices', async () => fail('PRECONDITION_FAILED', 'Device Link requires a Cindy account.'));

  // ── 更新通道 ── desktop main/updateChannelStore.ts + updateService.ts:2386-2450
  const betaSupported = supportsBetaUpdateChannel(platform, arch);
  const channel = createOverrideSettingsFile({
    file: file('update-channel-settings.json'),
    defaults: { enableBeta: false, orgDefaultEnableBeta: false },
    normalize: (r) => ({ enableBeta: r?.enableBeta === true, orgDefaultEnableBeta: r?.orgDefaultEnableBeta === true }),
  });
  const channelWire = () => {
    const s = channel.readState();
    const custom = s.customizedKeys.includes('enableBeta');
    return { enableBeta: betaSupported && (custom ? s.value.enableBeta : s.value.orgDefaultEnableBeta), isCustomized: custom };
  };
  const broadcastChannel = () => emitToRenderer('update-channel-settings', channelWire());
  registerInvoke('update-channel-settings-get', async () => channelWire());
  registerInvoke('update-channel-settings-set', async (payload) => {
    if (!betaSupported) fail('INVALID_PARAMS', 'This build does not support the beta update channel');
    if (!payload || typeof payload !== 'object') fail('INVALID_PARAMS', 'update channel settings payload required');
    if (typeof payload.enableBeta !== 'boolean') fail('INVALID_PARAMS', 'enableBeta required (boolean)');
    try {
      channel.writePatch({ enableBeta: payload.enableBeta }, { preserveDefaults: true });
    } catch {
      fail('INTERNAL', 'failed to write update channel settings');
    }
    broadcastChannel();
    return channelWire();
  });
  registerInvoke('update-channel-settings-reset', async () => {
    try { channel.reset(); } catch { fail('INTERNAL', 'failed to reset update channel settings'); }
    broadcastChannel();
    return channelWire();
  });
  // Tauri 版没有 beta manifest，探测恒为不可用
  registerInvoke('update-channel-probe-beta', async () => ({ available: false }));
  // desktop main/auto-update-settings-store.ts（默认 false）+ updateService.ts:2358-2379
  const autoUpdate = createOverrideSettingsFile({
    file: file('auto-update-settings.json'),
    defaults: { autoRelaunchOnIdle: false },
    normalize: (r) => ({ autoRelaunchOnIdle: typeof r?.autoRelaunchOnIdle === 'boolean' ? r.autoRelaunchOnIdle : false }),
  });
  const autoUpdateWire = () => {
    const s = autoUpdate.readState();
    return { autoRelaunchOnIdle: s.value.autoRelaunchOnIdle, isCustomized: s.isCustomized, defaultAutoRelaunchOnIdle: s.defaults.autoRelaunchOnIdle };
  };
  registerInvoke('update-auto-settings-get', async () => autoUpdateWire());
  registerInvoke('update-auto-settings-set', async (payload) => {
    if (!payload || typeof payload !== 'object') fail('INVALID_PARAMS', 'auto update settings payload required');
    if (typeof payload.autoRelaunchOnIdle !== 'boolean') fail('INVALID_PARAMS', 'autoRelaunchOnIdle required (boolean)');
    autoUpdate.writePatch({ autoRelaunchOnIdle: payload.autoRelaunchOnIdle });
    return autoUpdateWire();
  });
  registerInvoke('update-auto-settings-reset', async () => {
    autoUpdate.reset();
    return autoUpdateWire();
  });

  // ── 外设 ── Tauri 版没有 HID / 手柄宿主：按 desktop 「SDK 不可用」快照返回默认设置。
  // desktop main/worklouder-codex/accessories.ts projectModel（live.connectionStatus='unavailable' 且 deviceEnabled=false 分支）
  registerInvoke('worklouder-codex:get-state', async () => Object.fromEntries(WORKLOUDER_MODELS.map((model) => {
    const settings = createWorkLouderCodexDefaultSettings(model);
    const slotCount = settings.layout.taskKeys?.length ?? WORKLOUDER_CODEX_AGENT_SLOT_COUNT;
    const agentSlots = Array.from({ length: slotCount }, (_, slot) => {
      const action = settings.customAgentKeys[slot] ?? null;
      const sessionId = action?.type === 'task' ? action.sessionId : null;
      return { slot, sessionId, title: null, action };
    });
    return [model, {
      connectionStatus: 'disabled',
      connectionReason: 'sdk-unavailable',
      devicePresent: null,
      device: { ...WORKLOUDER_CODEX_EMPTY_DEVICE_STATE },
      settings,
      agentSlots,
      taskOptions: [],
      agentSlotCount: slotCount,
    }];
  })));
  // desktop main/worklouder-codex/CodexMicroGuardService.ts:113 非 darwin 形状（Tauri 版无 Codex 进程可保护）
  registerInvoke('codex-micro-guard:get-state', async () => ({ supported: false, enabled: false, status: 'unsupported' }));
  // desktop main/xbox-gamepad/controller.ts:42 createSlot + snapshot
  registerInvoke('xbox-gamepad:get-state', async () => Object.fromEntries(GAMEPAD_FAMILIES.map((family) => [family, {
    connectionStatus: 'disabled',
    devicePresent: null,
    deviceName: null,
    device: emptyGamepadDevice(family),
    settings: createXboxGamepadDefaultSettings(),
  }])));
}

// desktop main/appearance-settings-ipc.ts parsePatch（customWallpaperUrl 只读，不接受写入）
function parseAppearancePatch(raw) {
  if (!raw || typeof raw !== 'object' || Array.isArray(raw)) fail('INVALID_PARAMS', 'appearance patch must be an object');
  const allowed = new Set(['uiFamily', 'codeFamily', 'uiSize', 'codeSize', 'terminalFamily', 'terminalSize', 'windowZoom',
    'wallpaperId', 'wallpaperOverlay', 'wallpaperVisibility', 'wallpaperBlur', 'wallpaperMotion']);
  for (const key of Object.keys(raw)) {
    if (!allowed.has(key)) fail('INVALID_PARAMS', `unknown appearance field: ${key}`);
  }
  const family = (v, name) => {
    if (typeof v !== 'string' || v.length > 256) fail('INVALID_PARAMS', `${name} must be a string up to 256 characters`);
    return v.trim();
  };
  const num = (v, name, clamp) => {
    if (typeof v !== 'number' || !Number.isFinite(v)) fail('INVALID_PARAMS', `${name} must be a finite number`);
    return clamp(v);
  };
  const patch = {};
  for (const k of ['uiFamily', 'codeFamily', 'terminalFamily']) if (k in raw) patch[k] = family(raw[k], k);
  if ('uiSize' in raw) patch.uiSize = num(raw.uiSize, 'uiSize', clampAppearanceUiSize);
  if ('codeSize' in raw) patch.codeSize = num(raw.codeSize, 'codeSize', clampAppearanceCodeSize);
  if ('terminalSize' in raw) patch.terminalSize = num(raw.terminalSize, 'terminalSize', clampAppearanceTerminalSize);
  if ('windowZoom' in raw) patch.windowZoom = num(raw.windowZoom, 'windowZoom', clampAppearanceWindowZoom);
  if ('wallpaperId' in raw) {
    if (typeof raw.wallpaperId !== 'string' || !WALLPAPER_IDS.includes(raw.wallpaperId)) fail('INVALID_PARAMS', 'wallpaperId is not supported');
    patch.wallpaperId = raw.wallpaperId;
  }
  if ('wallpaperOverlay' in raw) patch.wallpaperOverlay = num(raw.wallpaperOverlay, 'wallpaperOverlay', clampAppearanceWallpaperOverlay);
  if ('wallpaperMotion' in raw) {
    if (raw.wallpaperMotion !== 'static' && raw.wallpaperMotion !== 'dynamic') fail('INVALID_PARAMS', 'wallpaperMotion is not supported');
    patch.wallpaperMotion = raw.wallpaperMotion;
  }
  if ('wallpaperVisibility' in raw) {
    patch.wallpaperVisibility = raw.wallpaperVisibility === null ? null : num(raw.wallpaperVisibility, 'wallpaperVisibility', clampAppearanceWallpaperVisibility);
  }
  if ('wallpaperBlur' in raw) {
    patch.wallpaperBlur = raw.wallpaperBlur === null ? null : num(raw.wallpaperBlur, 'wallpaperBlur', clampAppearanceWallpaperBlur);
  }
  return patch;
}


