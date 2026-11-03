// 把 cindy-headless 的构建产物复制到 sidecar/dist/headless/，供 Tauri 作为资源打包。
// 用法：node scripts/fetch-headless.mjs [cindy 仓库目录]，默认读 CINDY_DIR 或 ../cindy。
// 需要先在 cindy 仓库里构建：pnpm --filter cindy-headless build
import { cp, mkdir, rm, stat } from 'node:fs/promises';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const cindyDir = path.resolve(process.argv[2] ?? process.env.CINDY_DIR ?? path.join(root, '..', 'cindy'));
const src = path.join(cindyDir, 'apps', 'cindy-headless');
const dest = path.join(root, 'sidecar', 'dist', 'headless');

const cli = path.join(src, 'dist', 'cli.cjs');
await stat(cli).catch(() => {
  throw new Error(`missing ${cli}; run "pnpm --filter cindy-headless build" in ${cindyDir} first`);
});

await rm(dest, { recursive: true, force: true });
await mkdir(dest, { recursive: true });
// source map 有 12MB，不随包。
await cp(cli, path.join(dest, 'cli.cjs'));
await cp(path.join(src, 'profiles'), path.join(dest, 'profiles'), { recursive: true });
// Agent 按需下载的版本锁定（url + sha256）。
await mkdir(path.join(dest, 'agents'), { recursive: true });
await cp(path.join(cindyDir, 'tools', 'pi', 'latest.json'), path.join(dest, 'agents', 'pi.json'));

const { size } = await stat(path.join(dest, 'cli.cjs'));
console.log(`cindy-headless copied to ${dest} (cli.cjs ${(size / 1024 / 1024).toFixed(1)} MB)`);
