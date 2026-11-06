// 把 sidecar 打成单个 CJS 文件，供 Tauri 资源和 dev 模式使用。
import { build } from 'esbuild';
import { fileURLToPath } from 'node:url';
import path from 'node:path';
import { readFileSync } from 'node:fs';

const here = path.dirname(fileURLToPath(import.meta.url));
// 直接复用 cindy desktop 的纯函数模块（shared/ 下无 electron 依赖的部分）。
const cindyDir = path.resolve(process.env.CINDY_DIR ?? path.join(here, '..', '..', 'cindy'));

// 主进程侧文案（托盘 / 通知 / 原生对话框）：只抽 main-strings-keys.json 列出的 key，
// 生成 virtual:cindy-main-strings = { [locale]: { [key]: string } }。
const localesDir = path.join(cindyDir, 'apps', 'desktop', 'src', 'renderer', 'i18n', 'locales');
const MAIN_LOCALES = ['zh-CN', 'zh-TW', 'en', 'ja', 'ko', 'es', 'fr', 'de', 'pt-PT', 'pt-BR', 'zh-Hant-x-lzh', 'zh-Hans-x-lzh'];
const mainStringsPlugin = {
  name: 'cindy-main-strings',
  setup(b) {
    b.onResolve({ filter: /^virtual:cindy-main-strings$/ }, (a) => ({ path: a.path, namespace: 'cindy-main-strings' }));
    b.onLoad({ filter: /.*/, namespace: 'cindy-main-strings' }, () => {
      const keys = JSON.parse(readFileSync(path.join(here, 'src', 'main-strings-keys.json'), 'utf8'));
      const out = {};
      for (const loc of MAIN_LOCALES) {
        const bundle = JSON.parse(readFileSync(path.join(localesDir, loc, 'common.json'), 'utf8'));
        const picked = {};
        for (const key of keys) {
          const v = key.split('.').reduce((o, k) => (o && typeof o === 'object' ? o[k] : undefined), bundle);
          if (typeof v === 'string') picked[key] = v;
        }
        out[loc] = picked;
      }
      if (Object.keys(out.en).length !== keys.length) {
        throw new Error(`main strings missing in en: ${keys.filter((k) => !(k in out.en)).join(', ')}`);
      }
      return { contents: JSON.stringify(out), loader: 'json' };
    });
  },
};
// desktop src/main 只按文件放行（DESIGN.md §9.12）：sidecar 通过 @cindy-desktop-main/<相对路径>
// 引用下面这些无 Electron 依赖的纯模块，跟随上游自动同步；其余 src/main 文件一律构建失败。
// 名单要扩大须先征得同意。
const desktopMainDir = path.join(cindyDir, 'apps', 'desktop', 'src', 'main');
const DESKTOP_MAIN_ALLOWLIST = [
  'authHostedCallback.ts',
  'authRefreshBackoff.ts',
  'model-access/credentialsSync.ts',
  'model-access/modelsSyncRefresh.ts',
  'maker-host/pi-gateway-model-catalog.ts',
  'billing/projection.ts',
];
// 名单内文件的纯传递依赖（运行时 import，不算新增引用点）。
const DESKTOP_MAIN_TRANSITIVE = ['model-access/authSessionIdentity.ts', 'billing/paymentRedirect.ts'];
const allowedMain = new Set([...DESKTOP_MAIN_ALLOWLIST, ...DESKTOP_MAIN_TRANSITIVE].map((f) => path.join(desktopMainDir, f)));
const desktopMainPlugin = {
  name: 'cindy-desktop-main-allowlist',
  setup(b) {
    b.onResolve({ filter: /^@cindy-desktop-main\// }, (a) => {
      const rel = a.path.slice('@cindy-desktop-main/'.length);
      if (!DESKTOP_MAIN_ALLOWLIST.includes(rel)) {
        return { errors: [{ text: `desktop src/main file not in allowlist: ${rel}` }] };
      }
      return { path: path.join(desktopMainDir, rel) };
    });
    const esc = desktopMainDir.replace(/[.*+?^${}()|[\]\\]/g, '\\$&');
    b.onLoad({ filter: new RegExp(`^${esc}[\\\\/]`) }, (a) => {
      if (!allowedMain.has(a.path)) {
        return { errors: [{ text: `desktop src/main file not in allowlist: ${path.relative(desktopMainDir, a.path)}` }] };
      }
      return undefined;
    });
  },
};
await build({
  alias: { '@cindy-desktop-shared': path.join(cindyDir, 'apps', 'desktop', 'src', 'shared') },
  entryPoints: [path.join(here, 'src/index.mjs')],
  bundle: true,
  platform: 'node',
  format: 'cjs',
  target: 'node22',
  outfile: path.join(here, 'dist/sidecar.cjs'),
  // maker-core 及其依赖从 cindy 的 node_modules 解析。
  nodePaths: [
    path.join(cindyDir, 'apps', 'cindy-headless', 'node_modules'),
    path.join(cindyDir, 'apps', 'desktop', 'node_modules'),
    path.join(cindyDir, 'node_modules'),
  ],
  // maker-core 内置的 Pi 扩展源码 / prompt 以 .md 文本形式 import。
  loader: { '.md': 'text' },
  // CJS 里没有 import.meta.url，用 banner 里的 __u 顶替。
  // desktop shared/brandRegion.ts 从 import.meta.env 读构建区域；Tauri 只做 global 区。
  define: { 'import.meta.url': '__u', 'import.meta.env': '{"VITE_CINDY_AUTH_REGION":"global"}' },
  banner: { js: 'const __u=require("node:url").pathToFileURL(__filename).href;' },
  // 原生模块不打包：sidecar 只用 node:sqlite。
  external: ['better-sqlite3', 'sharp', 'node-pty'],
  plugins: [mainStringsPlugin, desktopMainPlugin],
  logLevel: 'info',
});
