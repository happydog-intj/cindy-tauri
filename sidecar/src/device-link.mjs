// 设备互联（第一步：被控端）：手机 / 别的电脑经 device-link 中继看本机任务、发消息、审批。
// 对应 desktop main/device-link/{index,dispatch,broadcast-tap,settings-store}.ts 的被控端子集：
// - 登录后用 access token 连中继，退出 / 过期即断；token 刷新后重连。
// - 「允许被控」开关存在 <dataDir>/settings/device-link-settings.json（与设置页同一份文件），默认关闭。
// - 入站 link-open：开关关闭或控制端已撤销 → 拒绝；否则 link-accept。
// - 入站 invoke：只放行 REMOTE_INVOKE_ALLOWLIST 里本机已实现的通道，走本机同一份 IPC handler。
// - 推送：PUSH_FORWARD_ALLOWLIST 里的推送按 topicForPush 转给订阅了对应 topic 的控制端。
// 不做：文件传输、媒体、远程桌面、共享任务、provider share、直连（WebRTC）、事件批量、离线积压队列。
import os from 'node:os';
import { execFile } from 'node:child_process';
import WebSocket from 'ws';
import {
  computeAllowlistHash,
  DeviceLinkClient,
  DEVICE_LINK_CAPABILITY_BACKGROUND_LINK_V1,
  DEVICE_LINK_CAPABILITY_HISTORY_VIEW_V1,
  DL_SUBSCRIBE_CHANNEL,
  DL_UNSUBSCRIBE_CHANNEL,
  PUSH_FORWARD_ALLOWLIST,
  REMOTE_INVOKE_ALLOWLIST,
  topicForPush,
} from '@cindy/device-link';
import { authInfo, getAccessToken, onAuthChange, refresh as refreshAuth } from './auth.mjs';
import { deviceLinkBaseUrl } from './cloud-endpoints.mjs';
import { dispatchLocalInvoke, emitToRenderer, isInvokeImplemented, onRendererPush, registerInvoke } from './ipc.mjs';
import { ownerStamp } from './owner.mjs';
import { readJson, writeJsonAtomic } from './settings/override-file.mjs';

const APP_VERSION = process.env.CINDY_TAURI_APP_VERSION ?? '0.0.0';
const LEGACY_TOPIC = '*';
const LIST_TIMEOUT_MS = 15_000;
const MAX_TOPICS_PER_CONTROLLER = 256;

function log(level, msg) {
  process.stderr.write(`[device-link] ${level} ${msg}\n`);
}
const short = (id) => (typeof id === 'string' ? id.slice(0, 8) : '?');
const ipcError = (code, message) => new Error(`[${code}] ${message}`);

// 本机名：macOS 用「电脑名称」（与 desktop deviceName.ts 同源），其余用 hostname。
// 官方版在同一台电脑上用同一个名字，这里加「(Tauri)」后缀区分，避免手机上看到两台同名设备。
function readComputerName() {
  return new Promise((resolve) => {
    const fallback = () => resolve(os.hostname().replace(/\.local$/i, '') || 'Unknown Device');
    if (process.platform !== 'darwin') return fallback();
    execFile('scutil', ['--get', 'ComputerName'], { timeout: 500 }, (err, stdout) => {
      const name = !err && typeof stdout === 'string' ? stdout.trim() : '';
      if (name) resolve(name); else fallback();
    });
  });
}

function isTopic(value) {
  if (typeof value !== 'string') return false;
  if (value === 'sessions') return true;
  return value.startsWith('session:') && value.length > 'session:'.length;
}

// 控制端订阅表：controllerId → { name, topics:Set, capabilities:Set }
function createSubscriptions() {
  const byController = new Map();
  return {
    ensure(src, name, capabilities) {
      let entry = byController.get(src);
      if (!entry) {
        entry = { name: name ?? short(src), topics: new Set(), capabilities: new Set() };
        byController.set(src, entry);
      }
      if (name) entry.name = name;
      if (Array.isArray(capabilities)) for (const c of capabilities) if (typeof c === 'string') entry.capabilities.add(c);
      return entry;
    },
    subscribe(src, topics, name) {
      const entry = this.ensure(src, name);
      for (const t of topics) if (entry.topics.size < MAX_TOPICS_PER_CONTROLLER) entry.topics.add(t);
      if (topics.length > 0) entry.topics.delete(LEGACY_TOPIC);
    },
    unsubscribe(src, topics) {
      const entry = byController.get(src);
      if (entry) for (const t of topics) entry.topics.delete(t);
    },
    drop(src) { byController.delete(src); },
    clear() { byController.clear(); },
    controllers() { return [...byController.entries()]; },
    targets(topic) {
      const out = [];
      for (const [src, entry] of byController) {
        if (entry.topics.has(LEGACY_TOPIC) || entry.topics.has(topic)) out.push(src);
      }
      return out;
    },
  };
}

/**
 * registerDeviceLinkIpc({ settingsDir }) —— 被控端主机。远程调用本机未实现的通道直接回 NOT_IMPLEMENTED。
 * createWebSocket / wsUrl 只给测试注入。
 */
export function registerDeviceLinkIpc({ settingsDir, isImplemented = isInvokeImplemented, createWebSocket, wsUrl: wsUrlOverride }) {
  const settingsFile = `${settingsDir}/device-link-settings.json`;
  const readSettings = () => {
    let raw;
    try { raw = readJson(settingsFile); } catch { raw = undefined; }
    return raw && typeof raw === 'object' ? raw : {};
  };
  const remoteControlEnabled = () => readSettings().remoteControlEnabled === true;
  const revoked = () => new Set(Array.isArray(readSettings().revokedControllers) ? readSettings().revokedControllers : []);
  const writeSettings = (patch) => writeJsonAtomic(settingsFile, { ...readSettings(), ...patch });

  let deviceName = os.hostname();
  void readComputerName().then((n) => { deviceName = `${n} (Tauri)`; });

  const subscriptions = createSubscriptions();
  const accepted = new Set();
  let client = null;
  let lastStatus = 'stopped';
  let lastIssue = null;
  const seenRequests = new Map(); // `${src}\0${id}` → 到期时刻，重复 invoke 去重

  const wsUrl = () => wsUrlOverride ?? `${deviceLinkBaseUrl().replace(/^http/, 'ws')}/api/device-link/ws`;

  function controlledBy() {
    return subscriptions.controllers().filter(([src]) => accepted.has(src)).map(([deviceId, e]) => ({ deviceId, name: e.name }));
  }
  function emitControlled() {
    emitToRenderer('device-link:controlled-state', { controllers: controlledBy() });
  }

  function state() {
    const s = readSettings();
    return {
      remoteControlEnabled: s.remoteControlEnabled === true,
      keepAwake: s.keepAwake === true,
      linkStatus: lastStatus,
      connectionIssue: lastIssue,
      standby: false,
      controlledBy: controlledBy(),
      revokedControllers: Array.isArray(s.revokedControllers) ? s.revokedControllers : [],
      disabledControlDeviceIds: Array.isArray(s.disabledControlDeviceIds) ? s.disabledControlDeviceIds : [],
      unresponsiveDeviceIds: [],
    };
  }

  async function getToken() {
    if (!authInfo().authenticated) return null;
    let token = getAccessToken();
    if (!token) {
      await refreshAuth().catch(() => false);
      token = getAccessToken();
    }
    return token ?? null;
  }

  function hello() {
    return {
      deviceName,
      platform: process.platform,
      appVersion: APP_VERSION,
      remoteControlEnabled: remoteControlEnabled(),
      busy: false,
      capabilities: [],
    };
  }

  function result(src, id, payload) {
    try { client?.sendInvokeResult(src, id, payload); } catch (err) { log('warn', `invoke-result to ${short(src)} failed: ${err?.message ?? err}`); }
  }
  const failure = (code, message) => ({ ok: false, error: { code, message: `[${code}] ${message}` } });

  function errorCodeOf(err) {
    const msg = String(err?.message ?? err);
    const m = /^\[([A-Z0-9_]+)\]\s*(.*)$/s.exec(msg);
    return m ? { code: m[1], message: msg } : { code: 'INTERNAL', message: `[INTERNAL] ${msg}` };
  }

  function handleLinkOpen(env) {
    const src = env.src;
    if (!src || !env.id) return;
    if (!remoteControlEnabled()) {
      log('warn', `link-open from ${short(src)} rejected: remote control disabled locally`);
      return;
    }
    if (revoked().has(src)) {
      log('warn', `link-open from ${short(src)} rejected: access revoked`);
      client.closeLink(src, 'revoked', 'inbound');
      return;
    }
    const payload = env.payload && typeof env.payload === 'object' ? env.payload : {};
    const name = typeof payload.controllerName === 'string' && payload.controllerName.trim() ? payload.controllerName.trim().slice(0, 120) : null;
    const caps = Array.isArray(payload.capabilities) ? payload.capabilities : [];
    client.sendLinkAccept(src, env.id, {
      appVersion: APP_VERSION,
      allowlistHash: computeAllowlistHash(),
      capabilities: [DEVICE_LINK_CAPABILITY_HISTORY_VIEW_V1, DEVICE_LINK_CAPABILITY_BACKGROUND_LINK_V1],
    });
    const entry = subscriptions.ensure(src, name, caps);
    // 不声明 background-link 的老控制端靠全量订阅（desktop 同口径）。
    if (!caps.includes(DEVICE_LINK_CAPABILITY_BACKGROUND_LINK_V1) && entry.topics.size === 0) entry.topics.add(LEGACY_TOPIC);
    accepted.add(src);
    log('info', `controller ${short(src)} linked`);
    emitControlled();
  }

  function handleLinkClose(env) {
    const src = env.src;
    if (!src) return;
    const reason = env.payload && typeof env.payload === 'object' ? env.payload.reason : undefined;
    if (reason === 'transport-timeout') return; // 链路瞬断：保留订阅，等对端重开
    subscriptions.drop(src);
    accepted.delete(src);
    emitControlled();
  }

  function handleSubscription(src, payload) {
    if (!remoteControlEnabled()) return failure('REMOTE_DISABLED', 'remote control disabled');
    if (revoked().has(src)) return failure('ACCESS_REVOKED', 'access revoked by target device');
    const arg = Array.isArray(payload.args) ? payload.args[0] : undefined;
    const o = arg && typeof arg === 'object' ? arg : {};
    const topics = Array.isArray(o.topics) ? o.topics.filter(isTopic) : [];
    const name = typeof o.controllerName === 'string' && o.controllerName.trim() ? o.controllerName.trim().slice(0, 120) : null;
    if (payload.channel === DL_SUBSCRIBE_CHANNEL) {
      subscriptions.subscribe(src, topics, name);
      if (Array.isArray(o.capabilities)) subscriptions.ensure(src, null, o.capabilities);
    } else {
      subscriptions.unsubscribe(src, topics);
    }
    if (!accepted.has(src)) { accepted.add(src); emitControlled(); }
    return { ok: true, result: { ok: true } };
  }

  async function handleInvoke(env) {
    const src = env.src;
    const id = env.id;
    if (!src || !id) return;
    const now = Date.now();
    const key = `${src}\0${id}`;
    if ((seenRequests.get(key) ?? 0) > now) return; // 同一请求重发：结果已在路上
    seenRequests.set(key, now + 120_000);
    if (seenRequests.size > 2000) for (const [k, exp] of seenRequests) if (exp <= now) seenRequests.delete(k);

    const payload = env.payload && typeof env.payload === 'object' ? env.payload : {};
    const channel = typeof payload.channel === 'string' ? payload.channel : '';
    if (channel === DL_SUBSCRIBE_CHANNEL || channel === DL_UNSUBSCRIBE_CHANNEL) {
      result(src, id, handleSubscription(src, payload));
      return;
    }
    if (!remoteControlEnabled()) return result(src, id, failure('REMOTE_DISABLED', 'remote control disabled'));
    if (revoked().has(src)) return result(src, id, failure('ACCESS_REVOKED', 'access revoked by target device'));
    if (!REMOTE_INVOKE_ALLOWLIST.has(channel)) return result(src, id, failure('FORBIDDEN', `channel ${channel} is not remotely invokable`));
    if (!isImplemented(channel)) return result(src, id, failure('NOT_IMPLEMENTED', `${channel} is not available in the Tauri build yet`));
    try {
      const value = await dispatchLocalInvoke(channel, payload.args);
      result(src, id, { ok: true, result: value === undefined ? null : value });
    } catch (err) {
      const { code, message } = errorCodeOf(err);
      log('warn', `remote invoke ${channel} from ${short(src)} failed: ${code}`);
      result(src, id, { ok: false, error: { code, message: message.slice(0, 2000) } });
    }
  }

  function onFrame(env) {
    if (!env || typeof env !== 'object') return;
    if (env.kind === 'link-open') return handleLinkOpen(env);
    if (env.kind === 'link-close') return handleLinkClose(env);
    if (env.kind === 'invoke') {
      handleInvoke(env).catch((err) => log('warn', `invoke handling failed: ${err?.message ?? err}`));
    }
    return undefined;
  }

  // 推送转发：只发给已接入、订阅了对应 topic 的控制端。
  const offPush = onRendererPush((channel, args) => {
    if (!client || lastStatus !== 'online' || !PUSH_FORWARD_ALLOWLIST.has(channel)) return;
    const payload = args.length <= 1 ? args[0] : args;
    const topic = topicForPush(channel, payload);
    if (!topic) return;
    const stamp = ownerStamp();
    for (const dst of subscriptions.targets(topic)) {
      if (!accepted.has(dst)) continue;
      try { client.sendPush(dst, channel, payload, stamp.dataOwnerId ? stamp : undefined); } catch (err) { log('warn', `push ${channel} to ${short(dst)} failed: ${err?.message ?? err}`); }
    }
  });

  // ── 连接生命周期：登录在线、退出断开 ──
  function ensureClient() {
    if (client) return client;
    client = new DeviceLinkClient({
      getWsUrl: wsUrl,
      getToken,
      getHello: hello,
      createWebSocket: createWebSocket ?? ((url, headers) => new WebSocket(url, { headers })),
      timing: { pingIntervalMs: 15_000 },
      logger: {
        debug: () => {},
        info: (...a) => log('info', a.map(String).join(' ').slice(0, 300)),
        warn: (...a) => log('warn', a.map(String).join(' ').slice(0, 300)),
        error: (...a) => log('error', a.map(String).join(' ').slice(0, 300)),
      },
    });
    client.onStatusChange((status) => {
      lastStatus = status;
      log('info', `status ${status}`);
      emitToRenderer('device-link:status-changed', { status });
      if (status !== 'online') {
        // 中继断开后控制端会重新 link-open；订阅表在 link-close / 重开时重建。
        accepted.clear();
        emitControlled();
      }
    });
    client.onConnectionIssue((issue) => {
      lastIssue = issue ? { kind: issue.kind, ...(issue.closeCode !== undefined ? { closeCode: issue.closeCode } : {}), at: issue.at ?? Date.now() } : null;
      emitToRenderer('device-link:connection-issue', { issue: lastIssue });
    });
    client.onPresenceChanged((snap) => emitToRenderer('device-link:presence-changed', snap));
    client.onFrame(onFrame);
    return client;
  }

  let wantOnline = false;
  let lastUserId = null;
  function syncWithAuth(info = authInfo()) {
    const shouldBeOnline = info.authenticated === true;
    if (shouldBeOnline) {
      const c = ensureClient();
      if (!wantOnline) {
        wantOnline = true;
        lastUserId = info.userId;
        c.start();
      } else if (info.userId !== lastUserId) {
        // 换账号：重连，旧订阅作废。
        lastUserId = info.userId;
        subscriptions.clear();
        accepted.clear();
        c.restartConnection('account-changed');
      }
    } else if (wantOnline) {
      wantOnline = false;
      lastUserId = null;
      subscriptions.clear();
      accepted.clear();
      client?.stop();
      emitControlled();
    }
  }
  const offAuth = onAuthChange((info) => syncWithAuth(info));
  syncWithAuth();

  // ── renderer 通道（desktop shared/deviceLinkIpc.ts）──
  registerInvoke('device-link:get-state', async () => state());
  registerInvoke('device-link:set-enabled', async (enabled) => {
    if (typeof enabled !== 'boolean') throw ipcError('INVALID_PARAMS', 'enabled must be a boolean');
    writeSettings({ remoteControlEnabled: enabled });
    client?.sendPresence({ remoteControlEnabled: enabled });
    if (!enabled) {
      // 关闭被控：断开所有控制端（desktop toggle-off 同口径）。
      for (const src of accepted) { try { client?.closeLink(src, 'toggle-off', 'inbound'); } catch { /* 已断 */ } }
      subscriptions.clear();
      accepted.clear();
      emitControlled();
    }
    return state();
  });
  registerInvoke('device-link:list-devices', async () => {
    const token = await getToken();
    if (!token) throw ipcError('PRECONDITION_FAILED', 'Device Link requires a Cindy account.');
    const controller = new AbortController();
    const timer = setTimeout(() => controller.abort(), LIST_TIMEOUT_MS);
    let res;
    try {
      res = await fetch(`${deviceLinkBaseUrl()}/api/device-link/devices`, {
        headers: { Authorization: `Bearer ${token}`, Accept: 'application/json' },
        signal: controller.signal,
        cache: 'no-store',
      });
    } catch {
      throw ipcError('NETWORK_ERROR', 'device directory is unreachable');
    } finally {
      clearTimeout(timer);
    }
    if (!res.ok) throw ipcError(res.status === 401 ? 'PERMISSION_DENIED' : 'DEVICE_LINK_FAILED', `device directory request failed (${res.status})`);
    const body = await res.json().catch(() => null);
    const devices = Array.isArray(body?.devices) ? body.devices : [];
    const disabled = new Set(Array.isArray(readSettings().disabledControlDeviceIds) ? readSettings().disabledControlDeviceIds : []);
    const selfId = client?.getSelfDeviceId?.() ?? null;
    return {
      devices: devices.filter((d) => d && typeof d.deviceId === 'string').map((d) => ({
        ...d,
        isSelf: selfId ? d.deviceId === selfId : d.isSelf === true,
        controlEnabled: !disabled.has(d.deviceId),
      })),
    };
  });
  registerInvoke('device-link:revoke', async (deviceId) => {
    if (typeof deviceId !== 'string' || !deviceId) throw ipcError('INVALID_PARAMS', 'deviceId required');
    writeSettings({ revokedControllers: [...new Set([...revoked(), deviceId])] });
    try { client?.closeLink(deviceId, 'revoked', 'inbound'); } catch { /* 已断 */ }
    subscriptions.drop(deviceId);
    accepted.delete(deviceId);
    emitControlled();
    return state();
  });
  registerInvoke('device-link:restore', async (deviceId) => {
    if (typeof deviceId !== 'string' || !deviceId) throw ipcError('INVALID_PARAMS', 'deviceId required');
    const next = revoked();
    next.delete(deviceId);
    writeSettings({ revokedControllers: [...next] });
    return state();
  });
  registerInvoke('device-link:disconnect-all', async () => {
    for (const src of accepted) { try { client?.closeLink(src, 'user', 'inbound'); } catch { /* 已断 */ } }
    subscriptions.clear();
    accepted.clear();
    emitControlled();
    return state();
  });

  return {
    dispose() {
      offAuth();
      offPush();
      try { client?.stop(); } catch { /* 已停 */ }
    },
    _test: { state, subscriptions: () => subscriptions.controllers().map(([id, e]) => ({ id, topics: [...e.topics] })) },
  };
}
