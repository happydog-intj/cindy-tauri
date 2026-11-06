// 本机密钥存储：对齐 Electron safeStorage 的模型。
// - 32 字节主密钥放在系统钥匙串（macOS Keychain / Windows 凭据管理器），由 Rust 在启动后经
//   stdin RPC `secrets.unlock` 交给 sidecar，不经过环境变量，子进程拿不到；
// - 每个条目用 AES-256-GCM 加密（条目名作 AAD，防止密文挪到别的名字下），落 <dataDir>/secrets.v1.json；
// - 明文只在内存里按需解密，绝不写日志；
// - 主密钥换了（钥匙串条目被删后重建）时旧文件无法解密：改名保留为 .unrecoverable-*，从空开始。
import { createCipheriv, createDecipheriv, randomBytes } from 'node:crypto';
import { existsSync, readFileSync, renameSync } from 'node:fs';
import { mkdir, rename, writeFile } from 'node:fs/promises';
import path from 'node:path';
import { isRendererAccessibleSafeStorageKey } from '@cindy-desktop-shared/providerSecrets';
import { registerInvoke } from './ipc.mjs';

const CHECK_NAME = '__check__';
const CHECK_PLAIN = 'cindy-tauri-secret-store-v1';
const KEY_NAME_RE = /^[a-zA-Z0-9_-]{1,200}$/;
// 登录凭据只在 sidecar 内部使用（auth.mjs），renderer 的通用通道读写不到。
// desktop 的 isRendererAccessibleSafeStorageKey 不拦这些键（desktop 另走独立存储），这里自己补上。
const AUTH_ONLY_KEYS = new Set(['cindy_auth_session_v1', 'cindy_auth_accounts_v1', 'cindy_auth_refresh_token', 'refresh_token']);
const CUSTOM_PROVIDER_RUNTIME_KEY_RE = /^provider_key_[a-zA-Z0-9_-]+_(?:claude-code|codex|pi)$/;
const MAX_VALUE_CHARS = 64 * 1024;
const READY_TIMEOUT_MS = 15_000;

let filePath = null;
let masterKey = null;
// 'locked'：等 Rust 送主密钥；'ready'：可用；'unavailable'：钥匙串不可用或主密钥非法。
let state = 'locked';
let unavailableReason = null;
let entries = {};
let writeQueue = Promise.resolve();
const readyWaiters = new Set();
const changeListeners = new Set();

function ipcError(code, message) {
  return new Error(`[${code}] ${message}`);
}

function settle() {
  for (const fn of readyWaiters) fn();
  readyWaiters.clear();
}

function encrypt(name, plain) {
  const iv = randomBytes(12);
  const cipher = createCipheriv('aes-256-gcm', masterKey, iv);
  cipher.setAAD(Buffer.from(name, 'utf8'));
  const data = Buffer.concat([cipher.update(plain, 'utf8'), cipher.final()]);
  return { iv: iv.toString('base64'), tag: cipher.getAuthTag().toString('base64'), data: data.toString('base64') };
}

function decrypt(name, blob) {
  const decipher = createDecipheriv('aes-256-gcm', masterKey, Buffer.from(blob.iv, 'base64'));
  decipher.setAAD(Buffer.from(name, 'utf8'));
  decipher.setAuthTag(Buffer.from(blob.tag, 'base64'));
  return Buffer.concat([decipher.update(Buffer.from(blob.data, 'base64')), decipher.final()]).toString('utf8');
}

function isBlob(v) {
  return v && typeof v === 'object' && typeof v.iv === 'string' && typeof v.tag === 'string' && typeof v.data === 'string';
}

function load() {
  entries = {};
  if (!existsSync(filePath)) return;
  let raw;
  try {
    raw = JSON.parse(readFileSync(filePath, 'utf8'));
    if (!isBlob(raw?.check) || decrypt(CHECK_NAME, raw.check) !== CHECK_PLAIN) throw new Error('check entry mismatch');
  } catch (err) {
    const aside = `${filePath}.unrecoverable-${Date.now()}`;
    process.stderr.write(`[secret-store] existing secrets cannot be decrypted (${err.message}); moved to ${path.basename(aside)}\n`);
    try { renameSync(filePath, aside); } catch { /* 下一次写入会覆盖 */ }
    return;
  }
  for (const [name, blob] of Object.entries(raw.entries ?? {})) {
    if (KEY_NAME_RE.test(name) && isBlob(blob)) entries[name] = blob;
  }
}

function persist() {
  const snapshot = JSON.stringify({ version: 1, check: encrypt(CHECK_NAME, CHECK_PLAIN), entries }, null, 2);
  const task = writeQueue.then(async () => {
    await mkdir(path.dirname(filePath), { recursive: true });
    const tmp = `${filePath}.tmp-${process.pid}`;
    await writeFile(tmp, snapshot, { mode: 0o600 });
    await rename(tmp, filePath);
  });
  writeQueue = task.catch(() => {});
  return task;
}

function notify(name) {
  for (const fn of changeListeners) {
    try { fn(name); } catch { /* 监听方自己负责 */ }
  }
}

function assertName(name) {
  if (typeof name !== 'string' || !KEY_NAME_RE.test(name) || name === CHECK_NAME) throw ipcError('INVALID_PARAMS', 'invalid secret name');
}

/** 初始化存储位置。主密钥稍后由 unlock() 送达。 */
export function initSecretStore({ dataDir }) {
  filePath = path.join(dataDir, 'secrets.v1.json');
  masterKey = null;
  state = 'locked';
  unavailableReason = null;
  entries = {};
}

/** Rust 侧送来主密钥（64 位十六进制）。只接受一次，之后的调用一律拒绝。 */
export function unlock(params) {
  if (state !== 'locked') throw ipcError('PRECONDITION_FAILED', 'secret store already initialized');
  const hex = params?.key;
  if (typeof hex !== 'string' || !/^[0-9a-f]{64}$/.test(hex)) {
    markUnavailable('invalid master key');
    throw ipcError('INVALID_PARAMS', 'master key must be 64 lowercase hex chars');
  }
  masterKey = Buffer.from(hex, 'hex');
  load();
  state = 'ready';
  settle();
  notify(null);
  return { ok: true, entries: Object.keys(entries).length };
}

/** 钥匙串不可用（例如用户拒绝访问）。带 key 的 provider 显示为未连接，保存 key 时报错。 */
export function markUnavailable(reason) {
  if (state !== 'locked') return { ok: false };
  state = 'unavailable';
  unavailableReason = typeof reason === 'string' ? reason.slice(0, 300) : 'unknown';
  process.stderr.write(`[secret-store] unavailable: ${unavailableReason}\n`);
  settle();
  notify(null);
  return { ok: true };
}

export function secretStoreState() {
  return { state, reason: unavailableReason };
}

/** 等主密钥到位；超时或不可用返回 false。 */
export function whenReady(timeoutMs = READY_TIMEOUT_MS) {
  if (state !== 'locked') return Promise.resolve(state === 'ready');
  return new Promise((resolve) => {
    const done = () => { clearTimeout(t); readyWaiters.delete(done); resolve(state === 'ready'); };
    const t = setTimeout(done, timeoutMs);
    readyWaiters.add(done);
  });
}

async function requireReady() {
  if (!(await whenReady())) {
    throw ipcError('CREDENTIAL_STORAGE_UNAVAILABLE', 'system keychain is not available, so API keys cannot be saved or read');
  }
}

/** 同步判断：已解锁且存在该条目。 */
export function hasSecret(name) {
  return state === 'ready' && Object.hasOwn(entries, name);
}

/** 同步读取明文；未解锁、不存在或解密失败返回 null。 */
export function getSecretSync(name) {
  if (!hasSecret(name)) return null;
  try {
    return decrypt(name, entries[name]);
  } catch {
    return null;
  }
}

export async function setSecret(name, value) {
  assertName(name);
  if (typeof value !== 'string' || value.length === 0 || value.length > MAX_VALUE_CHARS) throw ipcError('INVALID_PARAMS', 'invalid secret value');
  await requireReady();
  entries[name] = encrypt(name, value);
  await persist();
  notify(name);
}

export async function removeSecret(name) {
  assertName(name);
  await requireReady();
  if (!Object.hasOwn(entries, name)) return false;
  delete entries[name];
  await persist();
  notify(name);
  return true;
}

export function onSecretChange(fn) {
  changeListeners.add(fn);
  return () => changeListeners.delete(fn);
}

export function isCustomProviderRuntimeKeyName(name) {
  return typeof name === 'string' && CUSTOM_PROVIDER_RUNTIME_KEY_RE.test(name);
}

// renderer 的通用 safeStorage 通道（desktop bootstrap-electron.ts safe-storage-*）：
// 键名规则和 main-only 黑名单与 desktop 相同；provider key 可回读，用于编辑表单回填。
function rendererKey(key) {
  return typeof key === 'string' && KEY_NAME_RE.test(key) && key !== CHECK_NAME
    && !AUTH_ONLY_KEYS.has(key.toLowerCase()) && !key.toLowerCase().startsWith('cindy_auth_')
    && isRendererAccessibleSafeStorageKey(key);
}

export function registerSecretIpc() {
  registerInvoke('safe-storage-store', async (key, value) => {
    if (!rendererKey(key) || typeof value !== 'string' || value.length > MAX_VALUE_CHARS) return false;
    if (!(await whenReady())) return false;
    if (value.length === 0) await removeSecret(key);
    else await setSecret(key, value);
    return true;
  });
  registerInvoke('safe-storage-read', async (key) => {
    if (!rendererKey(key)) return null;
    if (!(await whenReady())) {
      if (isCustomProviderRuntimeKeyName(key)) throw ipcError('INTERNAL', 'custom provider credential is unavailable');
      return null;
    }
    return getSecretSync(key);
  });
  registerInvoke('safe-storage-remove', async (key) => {
    if (!rendererKey(key)) return { success: false, error: 'invalid key' };
    if (!(await whenReady())) return { success: false, error: 'secure storage unavailable' };
    await removeSecret(key);
    return { success: true };
  });
}

export const secretMethods = {
  'secrets.unlock': unlock,
  'secrets.unavailable': (params) => markUnavailable(params?.reason),
};
