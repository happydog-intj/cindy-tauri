// 首屏启动需要的最小 handler。返回值形状与 desktop 主进程一致，内容是“本机模式、没有任何用户数据”。
// 这些都是占位实现：真实持久化在“最小会话链路”里机制化替换。
import { randomUUID } from 'node:crypto';
import { existsSync, mkdirSync, readFileSync, writeFileSync } from 'node:fs';
import path from 'node:path';
import { agentStatus } from './agents.mjs';
import { coerceLayout } from '@cindy-desktop-shared/layoutTree';
import { registerInvoke, registerSync } from './ipc.mjs';

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
  registerInvoke('update-get-status', async () => ({ status: 'idle' }));
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

  return { deviceId: id, exists: existsSync(dataDir) };
}
