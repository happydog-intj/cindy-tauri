// 由 scripts/gen-app-settings-data.mjs 打包后运行：把上游 shared 模块的静态数据与黄金样例导出成 JSON。
import { APP_ICONS } from '@cindy/maker-shared/app-icons';
import { CLASSICAL_REPLY_STYLES } from '@cindy/maker-shared/classical-reply';
import { DEFAULT_APPEARANCE_SETTINGS, WALLPAPER_IDS, normalizeAppearanceSettings } from '@cindy-desktop-shared/appearanceSettings';
import { APP_SHORTCUT_DEFINITIONS, normalizeAppShortcutOverrides, findAppShortcutConflict, comboToElectronAccelerator, getEffectiveAppShortcuts } from '@cindy-desktop-shared/appShortcuts';
import { isSystemReservedShortcut } from '@cindy-desktop-shared/keyboardReserved';
import { WORKLOUDER_CODEX_AGENT_SLOT_COUNT, WORKLOUDER_CODEX_EMPTY_DEVICE_STATE, WORKLOUDER_MODELS, createWorkLouderCodexDefaultSettings } from '@cindy-desktop-shared/workLouderCodex';
import { GAMEPAD_FAMILIES, createXboxGamepadDefaultSettings, emptyGamepadDevice } from '@cindy-desktop-shared/xboxGamepad';

const PLATFORMS = ['darwin', 'win32', 'linux'];
const shortcuts = APP_SHORTCUT_DEFINITIONS.map((d) => ({
  id: d.id, scope: d.scope, rebindable: d.rebindable, hiddenInSettings: !!d.hiddenInSettings,
  yieldsToUserBindings: !!d.yieldsToUserBindings, menuBacked: !!d.menuBacked, platforms: d.platforms ?? null,
  defaults: Object.fromEntries(PLATFORMS.map((p) => [p, d.getDefaultCombos(p)])),
}));
const workLouder = Object.fromEntries(WORKLOUDER_MODELS.map((model) => {
  const settings = createWorkLouderCodexDefaultSettings(model);
  const slotCount = settings.layout.taskKeys?.length ?? WORKLOUDER_CODEX_AGENT_SLOT_COUNT;
  const agentSlots = Array.from({ length: slotCount }, (_, slot) => {
    const action = settings.customAgentKeys[slot] ?? null;
    const sessionId = action?.type === 'task' ? action.sessionId : null;
    return { slot, sessionId, title: null, action };
  });
  return [model, { connectionStatus: 'disabled', connectionReason: 'sdk-unavailable', devicePresent: null,
    device: { ...WORKLOUDER_CODEX_EMPTY_DEVICE_STATE }, settings, agentSlots, taskOptions: [], agentSlotCount: slotCount }];
}));
const xbox = Object.fromEntries(GAMEPAD_FAMILIES.map((family) => [family, {
  connectionStatus: 'disabled', devicePresent: null, deviceName: null,
  device: emptyGamepadDevice(family), settings: createXboxGamepadDefaultSettings(),
}]));

// ── 黄金样例 ──
const H = 'a'.repeat(64);
const appearanceInputs: unknown[] = [
  null, 5, [], {}, { uiSize: 99, codeSize: 3.6, terminalSize: 'x', windowZoom: 1.234, wallpaperOverlay: 0.337 },
  { uiFamily: '  Foo  ', codeFamily: 'x'.repeat(300), wallpaperId: 'custom', wallpaperMotion: 'dynamic', appIcon: APP_ICONS[1] ?? 'x' },
  { wallpaperId: 'nope', appIcon: 'nope', density: 9, motion: -3, wallpaperBlur: 7.6, wallpaperVisibility: 0.456, desktopVisibility: 2 },
  { customWallpaperUrl: `cindy-media://client-wallpaper/${H}.webp`, customWallpaperFallbackUrl: `cindy-media://client-wallpaper/${H}.mp4` },
  { customWallpaperUrl: 'http://x', customizedKeys: ['a', 'b'] }, { customizedKeys: [1] },
  { uiFontResource: { source: { instanceId: 'i', key: H }, reference: 'asset:abc', hash: H, family: `CindyPlugin_${H}` } },
  { uiFontResource: { source: { instanceId: 'i', key: 'bad' }, reference: 'asset:abc', hash: H, family: `CindyPlugin_${H}` } },
  { codeFontResource: { source: { themeId: 't', packageSha256: H }, reference: 'asset:abc', hash: H, family: `CindyTheme_${H}` } },
  { terminalFontResource: { source: { themeId: 't', packageSha256: H }, reference: 'asset:abc', hash: H, family: `CindyTheme_${H}` } },
  { bodyFontResource: { source: { instanceId: 'i', key: H }, reference: 'library:a/b.ttf', hash: H, family: `CindyPlugin_${H}` } },
  { bodyFontResource: { source: { instanceId: 'i', key: H }, reference: 'library:a/../b.ttf', hash: H, family: `CindyPlugin_${H}` } },
  { reading: { bodyLineHeight: 1.5, maxWidth: 800 }, contentStyle: { messageGap: 8 }, themeColors: { light: { text: '#112233' } } },
  { reading: { bodyLineHeight: 9 }, contentStyle: { nope: 1 }, themeColors: { light: { text: 'red' } } },
  { reading: {}, themeColors: {} },
  { bodyFont: { size: 14, weight: 500, style: 'italic', family: 'Foo Bar' } },
  { bodyFont: { size: 14, resource: 'asset:abc' } },
  { bodyFont: { size: 14, resource: 'asset:abc', family: 'x' } },
  { bodyFont: { size: 14, weight: 500.5 } }, { bodyFont: { family: 'bad;family' } },
  { uiFontTraits: { weight: 400, style: 'normal' } }, { uiFontTraits: { weight: 400, family: 'x' } },
  { codeFontTraits: { size: 12 } }, { codeFontTraits: { weight: 901 } },
  { uiFontTraits: { resource: 'library:x/y' } }, { bodyFont: { resource: 'cindy-media://blobs/' + H + '.png' } },
];
const appearance = appearanceInputs.map((input) => ({ input, output: normalizeAppearanceSettings(input) }));
const shortcutOverrideInputs: unknown[] = [
  null, [], {}, { overrides: 1 },
  { 'toggle-sidebar': { code: 'KeyB', meta: true }, 'nope': { code: 'KeyA' }, 'close-tab-or-window': { code: 'KeyQ', ctrl: true } },
  { 'toggle-sidebar': null, 'open-terminal': { code: 'ShiftLeft' }, 'zoom-in': { code: '  ' }, 'zoom-out': { code: 'KeyW', meta: true, key: 'w' } },
  { 'zoom-out': { code: 'KeyW', ctrl: true, key: 5 }, 'zoom-in': 3, 'switch-session-1': { code: 'KeyZ', alt: true } },
];
const comboCases: any[] = [];
const codes = ['KeyA','KeyB','KeyC','KeyD','KeyE','KeyF','KeyG','KeyH','KeyI','KeyJ','KeyK','KeyL','KeyM','KeyN','KeyO','KeyP','KeyQ','KeyR','KeyS','KeyT','KeyV','KeyW','KeyX','KeyZ','Digit0','Digit1','Digit5','Comma','Period','Semicolon','Slash','Minus','Equal','Backquote','Tab','Space','Enter','Escape','Home','Delete','F4','F5','F13','ArrowLeft','ArrowUp','ArrowDown','ArrowRight','Numpad0','PrintScreen','Pause'];
for (const code of codes) for (let m = 0; m < 16; m++) {
  const c = { code, meta: !!(m & 1), ctrl: !!(m & 2), alt: !!(m & 4), shift: !!(m & 8) };
  comboCases.push({
    combo: c,
    reserved: { mac: isSystemReservedShortcut(c, 'mac'), windows: isSystemReservedShortcut(c, 'windows'), other: isSystemReservedShortcut(c, 'other') },
    accel: Object.fromEntries(PLATFORMS.map((p) => [p, comboToElectronAccelerator(c, p)])),
  });
}
const conflictCases: any[] = [];
const idsAll = APP_SHORTCUT_DEFINITIONS.map((d) => d.id);
const sampleOverrides: any[] = [{}, { 'toggle-sidebar': { code: 'KeyB', meta: false, ctrl: true, alt: false, shift: false } }, { 'open-terminal': null }];
for (const p of PLATFORMS) for (const ov of sampleOverrides) for (const id of idsAll) for (const code of ['KeyB', 'KeyW', 'Digit1', 'F5', 'KeyF']) for (const m of [1, 3]) {
  const c = { code, meta: !!(m & 1) && p === 'darwin', ctrl: !!(m & 2) || (p !== 'darwin' && !!(m & 1)), alt: false, shift: !!(m & 4) };
  conflictCases.push({ platform: p, id, combo: c, overrides: ov, conflict: findAppShortcutConflict(id as any, c, ov, p) });
}
const shortcutOverrides = PLATFORMS.flatMap((p) => shortcutOverrideInputs.map((input) => ({ platform: p, input, output: normalizeAppShortcutOverrides(input, p) })));
const effective = PLATFORMS.map((p) => ({ platform: p, overrides: { 'toggle-sidebar': { code: 'Digit1', meta: p === 'darwin', ctrl: p !== 'darwin', alt: false, shift: false } },
  result: Object.fromEntries(getEffectiveAppShortcuts({ 'toggle-sidebar': { code: 'Digit1', meta: p === 'darwin', ctrl: p !== 'darwin', alt: false, shift: false } } as any, p)) }));

process.stdout.write(JSON.stringify({
  data: {
    appIcons: APP_ICONS, wallpaperIds: WALLPAPER_IDS, appearanceDefaults: DEFAULT_APPEARANCE_SETTINGS,
    classicalStyles: Object.fromEntries(Object.entries(CLASSICAL_REPLY_STYLES).map(([k, v]) => [k, v.map((s) => s.id)])),
    shortcuts, workLouder, xbox,
  },
  golden: { appearance, shortcutOverrides, comboCases, conflictCases, effective },
}));
