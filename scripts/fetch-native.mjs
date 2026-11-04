// 把 sidecar 运行时需要的原生模块复制到 sidecar/dist/native/node_modules/，供 Tauri 作为资源打包。
// 用法：node scripts/fetch-native.mjs [目标三元组]，默认按当前机器。
// 只带目标平台的预编译文件，去掉调试符号、测试和 source map；node-pty 1.1.0 是 N-API 模块，
// 预编译文件与 Node 版本无关，sidecar 用的 Node 22 可以直接加载。
import { chmod, cp, mkdir, readdir, rm, stat } from 'node:fs/promises';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const TRIPLES = {
  'aarch64-apple-darwin': 'darwin-arm64',
  'x86_64-apple-darwin': 'darwin-x64',
  'x86_64-pc-windows-msvc': 'win32-x64',
  'aarch64-pc-windows-msvc': 'win32-arm64',
};
const triple = process.argv[2];
const prebuild = triple ? TRIPLES[triple] : `${process.platform}-${process.arch}`;
if (!prebuild) throw new Error(`unsupported target ${triple}`);

const destRoot = path.join(root, 'sidecar', 'dist', 'native', 'node_modules');
await rm(destRoot, { recursive: true, force: true });

// ── node-pty（右侧栏终端）──
const src = path.join(root, 'node_modules', 'node-pty');
const dest = path.join(destRoot, 'node-pty');
await stat(path.join(src, 'prebuilds', prebuild)).catch(() => {
  throw new Error(`node-pty has no prebuild for ${prebuild}; run npm ci first`);
});
const skip = (name) => name.endsWith('.map') || name.endsWith('.test.js') || name === 'testUtils.test.js';
await mkdir(dest, { recursive: true });
await cp(path.join(src, 'package.json'), path.join(dest, 'package.json'));
await cp(path.join(src, 'LICENSE'), path.join(dest, 'LICENSE'));
await cp(path.join(src, 'lib'), path.join(dest, 'lib'), {
  recursive: true,
  filter: (p) => !skip(path.basename(p)),
});
const prebuildDest = path.join(dest, 'prebuilds', prebuild);
await cp(path.join(src, 'prebuilds', prebuild), prebuildDest, {
  recursive: true,
  // Windows 11 只走 ConPTY：不带 winpty（pty.node / winpty*）和 .pdb。
  filter: (p) => {
    const name = path.basename(p);
    if (name.endsWith('.pdb')) return false;
    if (prebuild.startsWith('win32') && (name === 'pty.node' || name.startsWith('winpty'))) return false;
    return true;
  },
});
// npm 包里的 spawn-helper 没有执行位，不补的话 macOS 上 posix_spawnp 会失败。
if (prebuild.startsWith('darwin')) await chmod(path.join(prebuildDest, 'spawn-helper'), 0o755);

let bytes = 0;
const walk = async (d) => {
  for (const e of await readdir(d, { withFileTypes: true })) {
    const p = path.join(d, e.name);
    if (e.isDirectory()) await walk(p);
    else bytes += (await stat(p)).size;
  }
};
await walk(destRoot);
console.log(`native modules for ${prebuild} copied to ${destRoot} (${(bytes / 1024).toFixed(0)} KB)`);
