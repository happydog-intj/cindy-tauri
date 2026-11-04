// 把 sidecar 打成单个 CJS 文件，供 Tauri 资源和 dev 模式使用。
import { build } from 'esbuild';
import { fileURLToPath } from 'node:url';
import path from 'node:path';

const here = path.dirname(fileURLToPath(import.meta.url));
// 直接复用 cindy desktop 的纯函数模块（shared/ 下无 electron 依赖的部分）。
const cindyDir = path.resolve(process.env.CINDY_DIR ?? path.join(here, '..', '..', 'cindy'));
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
  define: { 'import.meta.url': '__u' },
  banner: { js: 'const __u=require("node:url").pathToFileURL(__filename).href;' },
  // 原生模块不打包：sidecar 只用 node:sqlite。
  external: ['better-sqlite3', 'sharp'],
  logLevel: 'info',
});
