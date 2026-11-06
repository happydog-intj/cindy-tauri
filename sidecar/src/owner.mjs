// 应用会话（desktop main/appSessionState.ts 的单数据集版）。
// Tauri 只有一份本地数据：本机模式和云端登录都用 dataOwnerId='local-v1'，未登录为 null。
// 每次模式切换 generation 递增（renderer 据此区分“边界已提交”和“边界进行中”），持久化到
// <dataDir>/app-session.json，内容只有 mode / generation / 非敏感的用户展示信息。
import { existsSync, mkdirSync, readFileSync, renameSync, writeFileSync } from 'node:fs';
import path from 'node:path';

export const LOCAL_DATA_OWNER_ID = 'local-v1';
const MODES = new Set(['local', 'signed-out', 'cloud']);

let filePath = null;
let mode = 'local';
let generation = 1;
let cachedUser = null;
const listeners = new Set();

function sanitizeCachedUser(u) {
  if (!u || typeof u !== 'object' || typeof u.id !== 'string' || !u.id) return null;
  const s = (v) => (typeof v === 'string' ? v.slice(0, 500) : null);
  return { id: u.id.slice(0, 200), name: s(u.name) ?? '', email: s(u.email), membershipKind: u.membershipKind === 'org' ? 'org' : 'personal' };
}

/** 读入持久化的会话；没有文件（v0.0.1 升级上来）= 本机模式、generation 1，和旧行为一致。 */
export function initAppSession({ dataDir }) {
  filePath = path.join(dataDir, 'app-session.json');
  if (!existsSync(filePath)) return;
  try {
    const raw = JSON.parse(readFileSync(filePath, 'utf8'));
    if (MODES.has(raw?.mode)) mode = raw.mode;
    if (Number.isSafeInteger(raw?.generation) && raw.generation >= 1) generation = raw.generation;
    cachedUser = mode === 'cloud' ? sanitizeCachedUser(raw?.cachedUser) : null;
  } catch (err) {
    process.stderr.write(`[owner] app-session.json unreadable (${err.message}); using local mode\n`);
  }
}

function persist() {
  if (!filePath) return;
  mkdirSync(path.dirname(filePath), { recursive: true });
  const tmp = `${filePath}.tmp-${process.pid}`;
  writeFileSync(tmp, JSON.stringify({ version: 1, mode, generation, cachedUser }), { mode: 0o600 });
  renameSync(tmp, filePath);
}

export function getAppMode() {
  return mode;
}

export function getCachedUser() {
  return cachedUser;
}

export function currentDataOwnerId() {
  return mode === 'signed-out' ? null : LOCAL_DATA_OWNER_ID;
}

export function ownerStamp() {
  return { dataOwnerId: currentDataOwnerId(), ownerGeneration: generation };
}

export function isCurrentOwner(dataOwnerId, ownerGeneration) {
  return dataOwnerId === currentDataOwnerId() && ownerGeneration === generation;
}

/** 提交新模式：generation +1 并落盘。同模式只更新缓存的用户展示信息，不动 generation。 */
export function commitAppSession(nextMode, user = null) {
  if (!MODES.has(nextMode)) throw new Error(`invalid app mode: ${nextMode}`);
  const nextUser = nextMode === 'cloud' ? sanitizeCachedUser(user) : null;
  const changed = nextMode !== mode;
  if (changed) generation += 1;
  mode = nextMode;
  cachedUser = nextUser;
  persist();
  if (changed) for (const fn of listeners) { try { fn(ownerStamp()); } catch { /* 监听方自理 */ } }
  return ownerStamp();
}

export function onOwnerChange(fn) {
  listeners.add(fn);
  return () => listeners.delete(fn);
}
