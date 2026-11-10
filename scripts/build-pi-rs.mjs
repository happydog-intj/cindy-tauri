// 编译 pi-rs（Rust 版 Pi）并放到 sidecar/dist/pi-rs/，供 Tauri 作为资源随包。
// 用法：node scripts/build-pi-rs.mjs [目标三元组]，默认按当前机器。需要 Rust 工具链。
import { execFileSync } from 'node:child_process';
import { chmodSync, copyFileSync, mkdirSync, rmSync, statSync } from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const triple = process.argv[2];
const win = triple ? triple.includes('windows') : process.platform === 'win32';
const manifest = path.join(root, 'pi-rs', 'Cargo.toml');
const args = ['build', '--release', '--locked', '--manifest-path', manifest];
if (triple) args.push('--target', triple);
execFileSync('cargo', args, { stdio: 'inherit' });

const built = path.join(root, 'pi-rs', 'target', ...(triple ? [triple] : []), 'release', win ? 'pi.exe' : 'pi');
const outDir = path.join(root, 'sidecar', 'dist', 'pi-rs');
rmSync(outDir, { recursive: true, force: true });
mkdirSync(outDir, { recursive: true });
const dest = path.join(outDir, win ? 'pi.exe' : 'pi');
copyFileSync(built, dest);
if (!win) chmodSync(dest, 0o755);
console.log(`pi-rs → ${path.relative(root, dest)} (${(statSync(dest).size / 1048576).toFixed(1)} MB)`);
