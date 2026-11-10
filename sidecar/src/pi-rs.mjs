// Pi 是随包的 Rust 实现（pi-rs，源码在仓库的 pi-rs/ 目录）。
// CINDY_PI_RS=<二进制路径> 可覆盖随包路径，用于测试和调试。
import { existsSync, statSync } from 'node:fs';

// 随包 pi-rs 的版本号，与 pi-rs/Cargo.toml 保持一致（scripts/pi-rs-unit.mjs 校验）。
export const PI_RS_VERSION = '0.1.0';

export function piRsPath(env = process.env) {
  const p = env.CINDY_PI_RS;
  if (!p) return null;
  try { return existsSync(p) && statSync(p).isFile() ? p : null; } catch { return null; }
}
