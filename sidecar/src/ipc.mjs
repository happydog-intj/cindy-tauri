// renderer IPC 路由：接住 shim 转发过来的 Electron channel。
// - 已实现的 channel 放在 invokeHandlers / sendHandlers / syncHandlers 里；
// - 未实现的 invoke 返回 [NOT_IMPLEMENTED] 错误，send 直接丢弃，sendSync 返回 undefined；
// - 每个 channel 的调用次数都记下来（ipc.stats），用于排迁移优先级。
import os from 'node:os';
import { decode, encode } from '../../shared/ipc-codec.mjs';

const counts = { invoke: new Map(), send: new Map(), sendSync: new Map() };
const missing = { invoke: new Map(), send: new Map(), sendSync: new Map() };
// renderer 的 error 日志（最多 50 条），冒烟用来判断首屏是否真的可用。
const rendererErrors = [];

function bump(map, channel) {
  map.set(channel, (map.get(channel) ?? 0) + 1);
}

// 推送给 renderer：等价于主进程 webContents.send(channel, ...args)。
let pushFn = () => {};
export function setPush(fn) {
  pushFn = fn;
}
export function emitToRenderer(channel, ...args) {
  pushFn({ channel, payload: encode(args) });
}

const appVersion = process.env.CINDY_TAURI_APP_VERSION ?? '0.0.0';

// 首屏 sendSync 的默认值。形状与 desktop 主进程返回值一致，内容是“没有任何用户数据”的状态。
const syncHandlers = {
  'app-locale:get-preferred-system-locale-sync': () => preferredLocale(),
  'get-app-display-version-info': () => ({ display: `${appVersion} · tauri`, detail: `${appVersion} (tauri)` }),
  'client-endpoints:get-sync': () => ({ websiteUrl: '' }),
  'appearance-settings:get-sync': () => null,
  'get-os-release': () => os.release(),
  'get-app-version': () => appVersion,
  'auth:has-persisted-session-hint-sync': () => false,
  'local-themes:list-sync': () => ({ success: true, themes: [], diagnostics: [] }),
  'app-shortcuts:get': () => ({ overrides: {}, platform: process.platform, globalFailures: [] }),
  // layout 和 ghosts 由各自的 store 实现（后续批次）；这里先返回 null，preload 会走它自己的兜底。
  'ghosts:recommendations': () => ({ ownerId: null, sources: [], recentIds: [], newlyInstalledId: null }),
  'ghosts:list': () => ({ ghosts: [] }),
  'ghosts:recent-usage': () => ({ ids: [] }),
  'ghosts:unread': () => ({ entries: [] }),
  'ghosts:errand-prefs': () => ({ config: {}, auxiliaryModelLabel: null }),
  'ghosts:workdir-prefs': () => ({ disabled: [] }),
  'sidebar-settings:claim-renderer-legacy-owner-sync': () => null,
  'sidebar-settings:load-snapshot-sync': () => null,
  'maker:model-visibility:legacy-owner-claim-sync': () => null,
};

const invokeHandlers = {};
const sendHandlers = {
  // renderer 日志落到 sidecar stderr（最终进 Tauri 日志）。
  'renderer:log': (level, scope, msg) => {
    if ((level === 'error' || level === 'fatal') && rendererErrors.length < 50) {
      rendererErrors.push({ scope: String(scope), msg: String(msg).slice(0, 500) });
    }
    if (level === 'error' || level === 'fatal' || level === 'warn') {
      process.stderr.write(`[renderer:${level}] ${scope}: ${String(msg).slice(0, 2000)}\n`);
    }
  },
};

function preferredLocale() {
  const raw = Intl.DateTimeFormat().resolvedOptions().locale || 'en';
  if (/^zh[-_](TW|HK|MO|Hant)/i.test(raw)) return 'zh-TW';
  if (/^zh/i.test(raw)) return 'zh-CN';
  const base = raw.split(/[-_]/)[0];
  if (raw === 'pt-BR') return 'pt-BR';
  if (base === 'pt') return 'pt-PT';
  return ['en', 'ja', 'ko', 'es', 'fr', 'de'].includes(base) ? base : 'en';
}

export function registerInvoke(channel, fn) {
  invokeHandlers[channel] = fn;
}

export function registerSync(channel, fn) {
  syncHandlers[channel] = fn;
}

export function registerSend(channel, fn) {
  sendHandlers[channel] = fn;
}

function args(params) {
  const decoded = decode(params?.payload);
  return Array.isArray(decoded) ? decoded : [];
}

function channelOf(params) {
  const c = params?.channel;
  if (typeof c !== 'string' || !c || c.length > 200) throw new Error('[INVALID_PARAMS] channel is required');
  return c;
}

export const ipcMethods = {
  'ipc.invoke': async (params) => {
    const channel = channelOf(params);
    bump(counts.invoke, channel);
    const fn = invokeHandlers[channel];
    if (!fn) {
      bump(missing.invoke, channel);
      throw new Error(`[NOT_IMPLEMENTED] ${channel} is not available in the Tauri build yet`);
    }
    return encode(await fn(...args(params)));
  },
  'ipc.send': async (params) => {
    const channel = channelOf(params);
    bump(counts.send, channel);
    const fn = sendHandlers[channel];
    if (!fn) {
      bump(missing.send, channel);
      return null;
    }
    await fn(...args(params));
    return null;
  },
  'ipc.sendSync': async (params) => {
    const channel = channelOf(params);
    bump(counts.sendSync, channel);
    const fn = syncHandlers[channel];
    if (!fn) {
      bump(missing.sendSync, channel);
      return encode(undefined);
    }
    return encode(await fn(...args(params)));
  },
  'ipc.stats': () => {
    const top = (m) => [...m.entries()].sort((a, b) => b[1] - a[1]);
    const total = (m) => [...m.values()].reduce((a, b) => a + b, 0);
    return {
      rendererErrors,
      calls: { invoke: total(counts.invoke), send: total(counts.send), sendSync: total(counts.sendSync) },
      missing: {
        invoke: Object.fromEntries(top(missing.invoke)),
        send: Object.fromEntries(top(missing.send)),
        sendSync: Object.fromEntries(top(missing.sendSync)),
      },
    };
  },
};
