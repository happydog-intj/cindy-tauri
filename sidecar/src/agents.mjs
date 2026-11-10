// Agent 二进制解析。Pi 现在是随包的 Rust 实现（pi-rs），不再按需下载原版：
// 二进制在 sidecar.cjs 旁边的 pi-rs/ 目录（Tauri 资源），开发模式在 sidecar/dist/pi-rs/。CINDY_PI_RS 可覆盖路径（测试用）。
// agentStatus / installAgent 保持原来的返回形状，调用方不用改。
import { existsSync } from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { piRsPath, PI_RS_VERSION } from './pi-rs.mjs';

const here = typeof __dirname === 'string' ? __dirname : path.dirname(fileURLToPath(import.meta.url));

function platformKey() {
  return `${process.platform}-${process.arch}`;
}

function bundledBinary() {
  const override = piRsPath();
  if (override) return override;
  const file = process.platform === 'win32' ? 'pi.exe' : 'pi';
  // 打包后：<resources>/sidecar/sidecar.cjs 与 <resources>/sidecar/pi-rs/pi；开发/测试：sidecar/dist/pi-rs/pi
  for (const dir of [path.join(here, 'pi-rs'), path.join(here, '..', 'dist', 'pi-rs')]) {
    const p = path.join(dir, file);
    if (existsSync(p)) return p;
  }
  return path.join(here, 'pi-rs', file);
}

export function agentStatus(kind) {
  if (kind !== 'pi') throw new Error(`unsupported agent: ${kind}`);
  const binary = bundledBinary();
  return { kind, version: PI_RS_VERSION, platform: platformKey(), installed: existsSync(binary), binary, downloadBytes: 0 };
}

// 随包，没有可安装的东西；二进制缺失说明安装包不完整，直接报错而不是静默回退。
export async function installAgent(kind) {
  const status = agentStatus(kind);
  if (!status.installed) throw new Error(`bundled pi-rs is missing: ${status.binary}`);
  return { ...status, downloaded: false };
}
