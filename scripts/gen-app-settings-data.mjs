// 生成 src-tauri/src/app_settings_data.json（静态表）、app_settings_golden.json 与 voice_input_golden.json（黄金样例）。
// 全部由上游 cindy 的 shared TS 实际执行得到；上游改动后重跑并提交。
// 用法：node scripts/gen-app-settings-data.mjs   （CINDY_DIR 指向 cindy 仓库，默认 vendor/cindy）
import { build } from 'esbuild';
import { execFileSync } from 'node:child_process';
import { mkdirSync, mkdtempSync, writeFileSync } from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const cindyDir = path.resolve(process.env.CINDY_DIR ?? path.join(root, 'vendor', 'cindy'));
const tmp = mkdtempSync(path.join(os.tmpdir(), 'app-settings-gen-'));
const outfile = path.join(tmp, 'gen.cjs');
await build({
  entryPoints: [path.join(root, 'scripts', 'app-settings', 'entry.ts')],
  bundle: true, platform: 'node', format: 'cjs', outfile, logLevel: 'error',
  alias: { '@cindy-desktop-shared': path.join(cindyDir, 'apps', 'desktop', 'src', 'shared') },
  nodePaths: [path.join(cindyDir, 'apps', 'desktop', 'node_modules'), path.join(cindyDir, 'node_modules')],
  define: { 'import.meta.env': '{"VITE_CINDY_AUTH_REGION":"global"}' },
});
const { data, golden } = JSON.parse(execFileSync(process.execPath, [outfile], { maxBuffer: 1 << 28 }).toString());
const dst = path.join(root, 'src-tauri', 'src');
writeFileSync(path.join(dst, 'app_settings_data.json'), JSON.stringify(data));
writeFileSync(path.join(dst, 'app_settings_golden.json'), JSON.stringify(golden));
// 语音输入数据（固定时钟 + 固定 id 下运行上游 voiceInputData.ts）
const voiceOut = path.join(tmp, 'voice.cjs');
await build({
  entryPoints: [path.join(root, 'scripts', 'app-settings', 'voice-entry.ts')],
  bundle: true, platform: 'node', format: 'cjs', outfile: voiceOut, logLevel: 'error',
  alias: { '@cindy-desktop-shared': path.join(cindyDir, 'apps', 'desktop', 'src', 'shared') },
  nodePaths: [path.join(cindyDir, 'apps', 'desktop', 'node_modules'), path.join(cindyDir, 'node_modules')],
  define: { 'import.meta.env': '{"VITE_CINDY_AUTH_REGION":"global"}' },
});
writeFileSync(path.join(dst, 'voice_input_golden.json'), execFileSync(process.execPath, [voiceOut], { maxBuffer: 1 << 28 }));
// 布局与项目排序（上游 layoutTree.ts / projectOrderSync.ts）
const layoutOut = path.join(tmp, 'layout.cjs');
await build({
  entryPoints: [path.join(root, 'scripts', 'app-settings', 'layout-entry.ts')],
  bundle: true, platform: 'node', format: 'cjs', outfile: layoutOut, logLevel: 'error',
  alias: {
    '@cindy-desktop-shared': path.join(cindyDir, 'apps', 'desktop', 'src', 'shared'),
    '@cindy/maker-shared/project-order-sync': path.join(cindyDir, 'packages', 'maker-shared', 'src', 'projectOrderSync.ts'),
  },
  nodePaths: [path.join(cindyDir, 'apps', 'desktop', 'node_modules'), path.join(cindyDir, 'node_modules')],
  define: { 'import.meta.env': '{"VITE_CINDY_AUTH_REGION":"global"}' },
});
writeFileSync(path.join(dst, 'layout_golden.json'), execFileSync(process.execPath, [layoutOut], { maxBuffer: 1 << 28 }));
// 账号登录纯逻辑（auth-client types/email/sessionRealm、authIpc、authRefreshBackoff、authHostedCallback）
const cindyPkg = (n) => path.join(cindyDir, 'packages', n, 'src');
const authOut = path.join(tmp, 'auth.cjs');
await build({
  entryPoints: [path.join(root, 'scripts', 'app-settings', 'auth-entry.ts')],
  bundle: true, platform: 'node', format: 'cjs', outfile: authOut, logLevel: 'error', target: 'node22',
  alias: {
    '@cindy/auth-client': path.join(cindyPkg('auth-client'), 'index.ts'),
    '@cindy-desktop-shared': path.join(cindyDir, 'apps', 'desktop', 'src', 'shared'),
    '@cindy-desktop-main': path.join(cindyDir, 'apps', 'desktop', 'src', 'main'),
  },
  nodePaths: [path.join(cindyDir, 'apps', 'desktop', 'node_modules'), path.join(cindyDir, 'node_modules')],
  define: { 'import.meta.env': '{"VITE_CINDY_AUTH_REGION":"global"}' },
});
writeFileSync(path.join(dst, 'auth_golden.json'), execFileSync(process.execPath, [authOut], { maxBuffer: 1 << 28 }));
// 余额 / 额度用量投影（billing/projection.ts）
const billingOut = path.join(tmp, 'billing.cjs');
await build({
  entryPoints: [path.join(root, 'scripts', 'app-settings', 'billing-entry.ts')],
  bundle: true, platform: 'node', format: 'cjs', outfile: billingOut, logLevel: 'error', target: 'node22',
  alias: { '@cindy-desktop-main': path.join(cindyDir, 'apps', 'desktop', 'src', 'main') },
  nodePaths: [path.join(cindyDir, 'apps', 'desktop', 'node_modules'), path.join(cindyDir, 'node_modules')],
});
writeFileSync(path.join(dst, 'billing_golden.json'), execFileSync(process.execPath, [billingOut], { maxBuffer: 1 << 28 }));
// 凭据同步状态机（model-access/credentialsSync.ts）
const credsyncOut = path.join(tmp, 'credsync.cjs');
await build({
  entryPoints: [path.join(root, 'scripts', 'app-settings', 'credsync-entry.ts')],
  bundle: true, platform: 'node', format: 'cjs', outfile: credsyncOut, logLevel: 'error', target: 'node22',
  alias: { '@cindy-desktop-main': path.join(cindyDir, 'apps', 'desktop', 'src', 'main'), '@cindy/auth-client': path.join(cindyPkg('auth-client'), 'index.ts') },
  nodePaths: [path.join(cindyDir, 'apps', 'desktop', 'node_modules'), path.join(cindyDir, 'node_modules')],
});
writeFileSync(path.join(dst, 'credsync_golden.json'), execFileSync(process.execPath, [credsyncOut], { maxBuffer: 1 << 28 }));
// /models v5 响应校验（model-access/modelsSyncRefresh.ts + model-providers 校验器）
const msOut = path.join(tmp, 'models-sync.cjs');
await build({
  entryPoints: [path.join(root, 'scripts', 'app-settings', 'models-sync-entry.ts')],
  bundle: true, platform: 'node', format: 'cjs', outfile: msOut, logLevel: 'error', target: 'node22',
  alias: { '@cindy-desktop-main': path.join(cindyDir, 'apps', 'desktop', 'src', 'main'), '@cindy-desktop-shared': path.join(cindyDir, 'apps', 'desktop', 'src', 'shared') },
  nodePaths: [path.join(cindyDir, 'apps', 'desktop', 'node_modules'), path.join(cindyDir, 'node_modules')],
  loader: { '.md': 'text' }, define: { 'import.meta.env': '{"VITE_CINDY_AUTH_REGION":"global"}' },
});
writeFileSync(path.join(dst, 'models_sync_golden.json'), execFileSync(process.execPath, [msOut], { maxBuffer: 1 << 28 }));
// xd 网关模型投影的数据表（嵌入 Rust）与黄金样例
const xdOut = path.join(tmp, 'xdcatalog.cjs');
await build({
  entryPoints: [path.join(root, 'scripts', 'app-settings', 'xdcatalog-entry.ts')],
  bundle: true, platform: 'node', format: 'cjs', outfile: xdOut, logLevel: 'error', target: 'node22',
  alias: {
    '@cindy-desktop-main': path.join(cindyDir, 'apps', 'desktop', 'src', 'main'), '@cindy-desktop-shared': path.join(cindyDir, 'apps', 'desktop', 'src', 'shared'),
    '@cindy/auth-client': path.join(cindyPkg('auth-client'), 'index.ts'),
  },
  nodePaths: [path.join(cindyDir, 'apps', 'desktop', 'node_modules'), path.join(cindyDir, 'node_modules')],
  loader: { '.md': 'text' }, define: { 'import.meta.env': '{"VITE_CINDY_AUTH_REGION":"global"}', 'import.meta.url': '__u' },
  banner: { js: 'const __u=require("node:url").pathToFileURL(__filename).href;' },
  external: ['better-sqlite3', 'sharp'],
});
const xd = JSON.parse(execFileSync(process.execPath, [xdOut], { maxBuffer: 1 << 29 }));
mkdirSync(path.join(dst, 'model_access'), { recursive: true });
writeFileSync(path.join(dst, 'model_access', 'xd_catalog.json'), JSON.stringify(xd.data));
writeFileSync(path.join(dst, 'xd_golden.json'), JSON.stringify(xd.golden));
// provider 视图（model-config）：完整注册表 + 随包 Pi 目录行 + 黄金样例
const pvOut = path.join(tmp, 'provider.cjs');
await build({
  entryPoints: [path.join(root, 'scripts', 'app-settings', 'provider-entry.ts')],
  bundle: true, platform: 'node', format: 'cjs', outfile: pvOut, logLevel: 'error', target: 'node22',
  alias: {
    '@cindy-desktop-main': path.join(cindyDir, 'apps', 'desktop', 'src', 'main'), '@cindy-desktop-shared': path.join(cindyDir, 'apps', 'desktop', 'src', 'shared'),
    '@cindy/auth-client': path.join(cindyPkg('auth-client'), 'index.ts'),
  },
  nodePaths: [path.join(cindyDir, 'apps', 'desktop', 'node_modules'), path.join(cindyDir, 'node_modules')],
  loader: { '.md': 'text' }, define: { 'import.meta.env': '{"VITE_CINDY_AUTH_REGION":"global"}', 'import.meta.url': '__u' },
  banner: { js: 'const __u=require("node:url").pathToFileURL(__filename).href;' },
  external: ['better-sqlite3', 'sharp'],
});
const pv = JSON.parse(execFileSync(process.execPath, [pvOut], { maxBuffer: 1 << 29 }));
mkdirSync(path.join(dst, 'model_config'), { recursive: true });
writeFileSync(path.join(dst, 'model_config', 'provider_data.json'), JSON.stringify(pv.data));
writeFileSync(path.join(dst, 'provider_golden.json'), JSON.stringify(pv.golden));
console.log(`wrote app_settings_data.json / app_settings_golden.json (shortcuts=${data.shortcuts.length}, conflictCases=${golden.conflictCases.length})`);
// device-link 被控端：允许列表数据 + 协议黄金样例（allowlist / topics / transport）
const dlOut = path.join(tmp, 'dlink.cjs');
await build({
  entryPoints: [path.join(root, 'scripts', 'app-settings', 'dlink-entry.ts')],
  bundle: true, platform: 'node', format: 'cjs', outfile: dlOut, logLevel: 'error', target: 'node22',
  alias: { '@cindy/device-link-protocol': path.join(cindyPkg('device-link-protocol'), 'index.ts') },
  nodePaths: [path.join(cindyDir, 'apps', 'desktop', 'node_modules'), path.join(cindyDir, 'node_modules')],
  define: { 'import.meta.url': '__u' }, banner: { js: 'const __u=require("node:url").pathToFileURL(__filename).href;' },
});
const dl = JSON.parse(execFileSync(process.execPath, [dlOut], { maxBuffer: 1 << 29 }));
mkdirSync(path.join(dst, 'device_link'), { recursive: true });
writeFileSync(path.join(dst, 'device_link', 'allowlist.json'), JSON.stringify(dl.data));
writeFileSync(path.join(dst, 'device_link_golden.json'), JSON.stringify(dl.golden));

// projectHistoryView：会话脚本 × 上游投影结果
const hvOut = path.join(tmp, 'history.cjs');
await build({
  entryPoints: [path.join(root, 'scripts', 'app-settings', 'history-entry.ts')],
  bundle: true, platform: 'node', format: 'cjs', outfile: hvOut, logLevel: 'error', target: 'node22',
  nodePaths: [path.join(cindyDir, 'apps', 'desktop', 'node_modules'), path.join(cindyDir, 'node_modules')],
  loader: { '.md': 'text' },
  define: { 'import.meta.url': '__u', 'import.meta.env': '{}' }, banner: { js: 'const __u=require("node:url").pathToFileURL(__filename).href;' },
});
const hv = JSON.parse(execFileSync(process.execPath, [hvOut], { maxBuffer: 1 << 29 }));
mkdirSync(path.join(dst, 'history_view'), { recursive: true });
writeFileSync(path.join(dst, 'history_view_golden.json'), JSON.stringify(hv.golden));
console.log(`wrote history_view_golden.json (cases=${hv.golden.cases.length})`);
