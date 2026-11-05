// 下载官方 Node 运行时，放到 src-tauri/binaries/ 下，文件名带 Rust target triple（Tauri externalBin 要求）。
// 用法：node scripts/fetch-node.mjs aarch64-apple-darwin | x86_64-pc-windows-msvc
// 下载后按 nodejs.org 的 SHASUMS256.txt 校验压缩包。
import { execFileSync } from 'node:child_process';
import { createHash } from 'node:crypto';
import { mkdirSync, copyFileSync, chmodSync, existsSync, readFileSync, rmSync } from 'node:fs';
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
const sums = execFileSync('curl', ['-fsSL', `${base}/SHASUMS256.txt`]).toString();
const expected = sums.split('\n').map((l) => l.trim().split(/\s+/)).find(([, name]) => name === t.archive)?.[0];
const actual = createHash('sha256').update(readFileSync(archive)).digest('hex');
if (!expected || expected !== actual) {
  rmSync(archive, { force: true });
  throw new Error(`sha256 mismatch for ${t.archive}: expected ${expected ?? '(not listed)'}, got ${actual}`);
}
console.log(`sha256 ok: ${t.archive}`);
if (!existsSync(path.join(cache, t.dir, t.bin))) {
  execFileSync('tar', ['-xf', archive, '-C', cache], { stdio: 'inherit' });
}

const out = path.join(root, 'src-tauri', 'binaries');
mkdirSync(out, { recursive: true });
const dest = path.join(out, `cindy-node-${triple}${t.ext}`);
copyFileSync(path.join(cache, t.dir, t.bin), dest);
chmodSync(dest, 0o755);
// P5 体积：macOS 的官方 node 带本地符号表（111 MB → strip -x 后 90 MB），去掉后重新 ad-hoc 签名
// （arm64 必须有签名才能运行；打包时 Tauri 还会再签一次）。Windows 的 node.exe 符号在 pdb 里，不需要处理。
if (process.platform === 'darwin' && triple.endsWith('apple-darwin')) {
  execFileSync('strip', ['-x', dest], { stdio: ['ignore', 'ignore', 'inherit'] });
  execFileSync('codesign', ['--force', '--sign', '-', dest], { stdio: 'inherit' });
}
console.log('node runtime ->', dest);
