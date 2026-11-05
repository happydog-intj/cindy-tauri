// 首屏启动需要的最小 handler。返回值形状与 desktop 主进程一致，内容是“本机模式、没有任何用户数据”。
// 这些都是占位实现：真实持久化在“最小会话链路”里机制化替换。
import { randomUUID } from 'node:crypto';
import { existsSync, mkdirSync, readFileSync, writeFileSync } from 'node:fs';
import path from 'node:path';
import { agentStatus } from './agents.mjs';
import { coerceLayout } from '@cindy-desktop-shared/layoutTree';
import { hostLocalProjectKeysOnly, parseSyncedProjectOrderMode } from '@cindy/maker-shared/project-order-sync';
import { emitToRenderer, registerInvoke, registerSync } from './ipc.mjs';

// desktop main/profile/profileRegistryModel.ts 的 LOCAL_PROFILE_DATA_OWNER_ID。
const LOCAL_DATA_OWNER_ID = 'local-v1';

function deviceId(dataDir) {
  const file = path.join(dataDir, 'device-id');
  try {
    const v = readFileSync(file, 'utf8').trim();
    if (/^[0-9a-f-]{36}$/.test(v)) return v;
  } catch {}
  const v = randomUUID();
  mkdirSync(dataDir, { recursive: true });
  writeFileSync(file, v);
  return v;
}

export function registerBootStubs({ dataDir, agentManifestDir }) {
  const id = deviceId(dataDir);
  const authState = () => ({
    user: null,
    mode: 'local',
    dataOwnerId: LOCAL_DATA_OWNER_ID,
    ownerGeneration: 1,
    canEnterApp: true,
    isAuthenticated: false,
    isCanary: false,
    deviceId: id,
    hasAccountDeletionReceipt: false,
    accountDeletionRestored: false,
    credentialStoreUnavailable: false,
  });

  // 这一版只支持 Pi；Claude Code / Codex 标成 skipped。Pi 未下载时也放行（renderer 里 Pi 是可选 agent）。
  registerInvoke('check-environment', async () => {
    let pi = { status: 'skipped' };
    try {
      const s = agentStatus('pi', { manifestDir: agentManifestDir, dataDir });
      if (s.installed) pi = { status: 'passed', path: s.binary };
    } catch (e) {
      pi = { status: 'failed', error: String(e?.message ?? e) };
    }
    return {
      claudeCode: { status: 'passed' },
      codex: { status: 'skipped' },
      pi,
      ripgrep: { status: 'skipped' },
      allPassed: true,
      platform: process.platform,
    };
  });
  registerInvoke('auth:initialize', async () => authState());
  registerInvoke('auth:get-login-state', async () => null);
  registerInvoke('check-app-update', async () => ({ hasUpdate: false, action: 'none' }));
  registerInvoke('analytics:settings-get', async () => ({
    privacyConsentAccepted: true,
    analyticsEnabled: false,
    analyticsEnabledCustomized: true,
    allowed: false,
  }));
  registerInvoke('legacy-migration:get-state', async () => ({ phase: null }));
  registerInvoke('local-db:maintenance:startup-progress', async () => null);
  registerInvoke('maker:memory:get-settings', async () => ({ maker: false, claudeCode: false, codex: false, pi: false }));
  registerInvoke('maker:git-safety:get', async () => ({
    mode: 'off',
    autoSnapshotEnabled: false,
    autoInitProjectGit: false,
    isCustomized: false,
    defaultMode: 'off',
    defaultAutoSnapshotEnabled: false,
  }));
  registerInvoke('maker:chat-embedding:get', async () => ({ enabled: false, isCustomized: false, defaultEnabled: false }));
  registerInvoke('maker:silent-encrypted-retry:get', async () => ({ enabled: false, isCustomized: false, defaultEnabled: false }));
  registerInvoke('maker:lsp-mode:get', async () => ({ enabled: false }));
  registerInvoke('worktree:list-all', async () => []);
  registerInvoke('local-db:bots:list', async () => []);
  // 真实 DB 在会话链路里接入；这里只放行 LocalDbGate。
  registerInvoke('local-db:ensure-ready', async () => ({ ready: true }));
  registerSync('accessibility-support:get-sync', () => true);
  // 语音输入未迁移：报告麦克风不可用，renderer 会跳过预热。
  registerSync('voice-input:get-microphone-permission-cached', () => ({
    ok: false,
    status: 'unknown',
    error: 'voice input is not available in the Tauri build yet',
  }));

  // 布局存档：<dataDir>/layout.v1.json，读写都过 desktop 的 coerceLayout 校验。
  const layoutFile = path.join(dataDir, 'layout.v1.json');
  const readLayout = () => {
    try {
      return coerceLayout(JSON.parse(readFileSync(layoutFile, 'utf8'))).layout;
    } catch {
      return coerceLayout(null).layout;
    }
  };
  registerSync('layout:get', () => ({ layout: readLayout() }));
  registerInvoke('layout:set', async (raw) => {
    const r = coerceLayout(raw);
    if (r.fallback) throw new Error(`[INVALID_PARAMS] invalid layout: ${r.reason}`);
    writeFileSync(layoutFile, JSON.stringify(r.layout));
    return { layout: r.layout, persisted: true };
  });
  registerInvoke('layout:reset', async () => {
    const layout = coerceLayout(null).layout;
    writeFileSync(layoutFile, JSON.stringify(layout));
    return { layout, persisted: true };
  });

  // 侧栏项目排序：desktop main/projectOrderStore.ts 的单 owner 版，存 <dataDir>/project-order.v1.json。
  const orderFile = path.join(dataDir, 'project-order.v1.json');
  const ownerStamp = { dataOwnerId: LOCAL_DATA_OWNER_ID, ownerGeneration: 1 };
  const normalizeOrder = (raw) => {
    const v = raw && typeof raw === 'object' ? raw : {};
    return {
      manualProjectOrder: hostLocalProjectKeysOnly(v.manualProjectOrder).filter((k) => k.length <= 4096),
      projectOrder: parseSyncedProjectOrderMode(v.projectOrder),
    };
  };
  const orderSnapshot = () => {
    let saved = null;
    try { saved = JSON.parse(readFileSync(orderFile, 'utf8')); } catch { /* 未自定义 */ }
    return { authoritative: saved !== null, available: true, ...normalizeOrder(saved), ownerStamp };
  };
  registerInvoke('sidebar-settings:get-project-order', async () => orderSnapshot());
  registerInvoke('sidebar-settings:apply-project-order', async (req) => {
    if (!req || typeof req !== 'object' || req.dataOwnerId !== ownerStamp.dataOwnerId || req.ownerGeneration !== ownerStamp.ownerGeneration) {
      throw new Error('[PRECONDITION_FAILED] active account changed during project order mutation');
    }
    writeFileSync(orderFile, JSON.stringify(normalizeOrder(req)));
    const snapshot = orderSnapshot();
    emitToRenderer('sidebar-settings:project-order-changed', snapshot, ownerStamp);
    return snapshot;
  });

  return { deviceId: id, exists: existsSync(dataDir) };
}
