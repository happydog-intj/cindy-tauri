// 下载官方 Node 运行时，放到 src-tauri/binaries/ 下，文件名带 Rust target triple（Tauri externalBin 要求）。
// 用法：node scripts/fetch-node.mjs aarch64-apple-darwin | x86_64-pc-windows-msvc
// 注意：P1 暂未校验 SHASUM256，正式版需要补充。
import { execFileSync } from 'node:child_process';
import { mkdirSync, copyFileSync, chmodSync, existsSync } from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const NODE_VERSION = '22.22.0';
const here = path.dirname(fileURLToPath(import.meta.url));
const root = path.resolve(here, '..');
const triple = process.argv[2];
const base = `https://nodejs.org/dist/v${NODE_VERSION}`;

const targets = {
  'aarch64-apple-darwin': {
    dir: `node-v${NODE_VERSION}-darwin-arm64`,
    archive: `node-v${NODE_VERSION}-darwin-arm64.tar.gz`,
    bin: 'bin/node',
    ext: '',
  },
  'x86_64-pc-windows-msvc': {
    dir: `node-v${NODE_VERSION}-win-x64`,
    archive: `node-v${NODE_VERSION}-win-x64.zip`,
    bin: 'node.exe',
    ext: '.exe',
  },
};

const t = targets[triple];
if (!t) throw new Error(`unsupported triple: ${triple}`);

const cache = path.join(root, '.cache');
mkdirSync(cache, { recursive: true });
const archive = path.join(cache, t.archive);
if (!existsSync(archive)) {
  execFileSync('curl', ['-fsSL', '-o', archive, `${base}/${t.archive}`], { stdio: 'inherit' });
}
if (!existsSync(path.join(cache, t.dir, t.bin))) {
  execFileSync('tar', ['-xf', archive, '-C', cache], { stdio: 'inherit' });
}

const out = path.join(root, 'src-tauri', 'binaries');
mkdirSync(out, { recursive: true });
const dest = path.join(out, `cindy-node-${triple}${t.ext}`);
copyFileSync(path.join(cache, t.dir, t.bin), dest);
chmodSync(dest, 0o755);
console.log('node runtime ->', dest);
