// 准备 Tauri 前端：
// 1. 用 esbuild 编译 cindy 的 preload.ts，electron 模块换成 shim/electron.mjs，process 换成 shim/process.mjs，
//    输出 src-tauri/preload/cindy-preload.js（Rust 编译期 include_str! 嵌入）；
// 2. 复制 cindy renderer 构建产物（apps/desktop/.vite/renderer/main_window）到 dist/frontend/，
//    并把 P1 冒烟页 ui/ 放到 dist/frontend/__smoke/。
// 用法：node scripts/build-frontend.mjs [cindy 仓库目录]
// 需要先在 cindy 仓库里构建 renderer：pnpm --filter desktop exec vite build --config vite.renderer.config.ts
import { build } from 'esbuild';
import { cp, mkdir, rm, stat, writeFile } from 'node:fs/promises';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const cindyDir = path.resolve(process.argv[2] ?? process.env.CINDY_DIR ?? path.join(root, '..', 'cindy'));
const desktopDir = path.join(cindyDir, 'apps', 'desktop');
const preloadSrc = path.join(desktopDir, 'src', 'preload', 'preload.ts');
const rendererDist = path.join(desktopDir, '.vite', 'renderer', 'main_window');
const preloadOut = path.join(root, 'src-tauri', 'preload', 'cindy-preload.js');
const frontendOut = path.join(root, 'dist', 'frontend');

await stat(preloadSrc).catch(() => {
  throw new Error(`missing ${preloadSrc}; pass the cindy repo path`);
});
await stat(path.join(rendererDist, 'index.html')).catch(() => {
  throw new Error(`missing renderer build at ${rendererDist}; run vite build --config vite.renderer.config.ts in ${desktopDir}`);
});

// preload 在每个页面（包括冒烟页）加载前执行；只有主 renderer 需要它，冒烟页里 electronAPI 用不到但无害。
const result = await build({
  entryPoints: [path.join(root, 'shim', 'entry.mjs')],
  bundle: true,
  format: 'iife',
  platform: 'browser',
  target: ['safari16', 'chrome110'],
  alias: {
    electron: path.join(root, 'shim', 'electron.mjs'),
    __CINDY_PRELOAD__: preloadSrc,
  },
  inject: [path.join(root, 'shim', 'process.mjs')],
  // preload.ts 里的类型导入会跟进 main/ 下的模块，esbuild 会把纯类型导入删掉；这里只兜底 node: 内置模块。
  external: [],
  define: { 'import.meta.hot': 'undefined' },
  legalComments: 'none',
  minify: true,
  metafile: true,
  outfile: preloadOut,
  logLevel: 'warning',
});

const nodeImports = Object.keys(result.metafile.inputs).filter((f) => f.startsWith('node:') || f.includes('/main/'));
if (nodeImports.length) {
  console.warn('preload bundle pulled in main/ or node: modules:', nodeImports.slice(0, 10));
}

await rm(frontendOut, { recursive: true, force: true });
await mkdir(frontendOut, { recursive: true });
await cp(rendererDist, frontendOut, { recursive: true });
await cp(path.join(root, 'ui'), path.join(frontendOut, '__smoke'), { recursive: true });
await writeFile(path.join(frontendOut, '.cindy-source'), `${cindyDir}\n`);

const { size } = await stat(preloadOut);
console.log(`preload: ${preloadOut} (${(size / 1024).toFixed(0)} KB)`);
console.log(`frontend: ${frontendOut}`);
