// 准备 Tauri 前端：
// 1. 用 esbuild 编译 cindy 的 preload.ts，electron 模块换成 shim/electron.mjs，process 换成 shim/process.mjs，
//    输出 src-tauri/preload/cindy-preload.js（Rust 编译期 include_str! 嵌入）；
// 2. 复制 cindy renderer 构建产物（apps/desktop/.vite/renderer/main_window）到 dist/frontend/，
//    并把 P1 冒烟页 ui/ 放到 dist/frontend/__smoke/。
// 用法：node scripts/build-frontend.mjs [cindy 仓库目录]
// 需要先在 cindy 仓库里构建 renderer：pnpm --filter desktop exec vite build --config vite.renderer.config.ts
import { build } from 'esbuild';
import { cp, mkdir, readdir, readFile, rm, stat, writeFile } from 'node:fs/promises';
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

// P5 体积：HarmonyOS Sans SC 每个字重 82 个分片、约 4.6 MB，woff2 不可再压缩。
// renderer 只用到 400 / 500 / 600 / 700（typographyDiscipline 测试禁止 800 / 900，代码里没有 100 / 300），
// 删掉 Thin(100) / Light(300) / Black(900) 三个字重的 @font-face 和字体文件；万一请求到这些字重，浏览器按就近字重回退。
const PRUNED_FONT_FAMILY = 'HarmonyOS Sans SC';
const PRUNED_FONT_WEIGHTS = new Set(['100', '300', '900']);

async function pruneFontWeights(dir) {
  const assets = path.join(dir, 'assets');
  const names = await readdir(assets);
  const removed = new Set();
  let rules = 0;
  for (const name of names.filter((n) => n.endsWith('.css'))) {
    const file = path.join(assets, name);
    const css = await readFile(file, 'utf8');
    const next = css.replace(/@font-face\{[^}]*\}/g, (rule) => {
      const family = rule.match(/font-family:\s*["']?([^;"'}]+)["']?\s*[;}]/)?.[1]?.trim();
      const weight = rule.match(/font-weight:\s*(\d+)/)?.[1];
      if (family !== PRUNED_FONT_FAMILY || !PRUNED_FONT_WEIGHTS.has(weight)) return rule;
      for (const m of rule.matchAll(/url\(["']?\/?assets\/([^)"']+\.woff2)["']?\)/g)) removed.add(m[1]);
      rules += 1;
      return '';
    });
    if (next !== css) await writeFile(file, next);
  }
  // 其他 CSS / JS 不能再引用被删的文件，否则说明规则匹配漏了。
  const texts = await Promise.all(names.filter((n) => /\.(css|js|mjs|html)$/.test(n)).map((n) => readFile(path.join(assets, n), 'utf8')));
  const stillUsed = [...removed].filter((f) => texts.some((t) => t.includes(f)));
  if (stillUsed.length) throw new Error(`pruned font files still referenced: ${stillUsed.slice(0, 5).join(', ')}`);
  let bytes = 0;
  for (const f of removed) {
    bytes += (await stat(path.join(assets, f))).size;
    await rm(path.join(assets, f));
  }
  console.log(`fonts: removed ${rules} @font-face rules / ${removed.size} files (${(bytes / 1048576).toFixed(1)} MB) for ${PRUNED_FONT_FAMILY} weights ${[...PRUNED_FONT_WEIGHTS].join('/')}`);
}

await pruneFontWeights(frontendOut);

const { size } = await stat(preloadOut);
console.log(`preload: ${preloadOut} (${(size / 1024).toFixed(0)} KB)`);
console.log(`frontend: ${frontendOut}`);
