// 编译 pi-rs（Rust 版 Pi）并放到 resources/pi-rs/，供 Tauri 作为资源随包。
// 用法：node scripts/build-pi-rs.mjs [目标三元组]，默认按当前机器。需要 Rust 工具链。
import { execFileSync } from 'node:child_process';
import { chmodSync, copyFileSync, mkdirSync, renameSync, rmSync, statSync } from 'node:fs';
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
const outDir = path.join(root, 'resources', 'pi-rs');
rmSync(outDir, { recursive: true, force: true });
mkdirSync(outDir, { recursive: true });
const dest = path.join(outDir, win ? 'pi.exe' : 'pi');
// 先写临时文件再改名：就地覆盖正在被系统缓存签名的可执行文件，macOS 之后会按旧签名校验新内容，
// 把 pi 在启动瞬间 SIGKILL（Code Signature Invalid），会话就表现为"输入后没反应"。
const tmp = `${dest}.tmp-${process.pid}`;
copyFileSync(built, tmp);
if (!win) chmodSync(tmp, 0o755);
renameSync(tmp, dest);
console.log(`pi-rs → ${path.relative(root, dest)} (${(statSync(dest).size / 1048576).toFixed(1)} MB)`);
