// 可选的 Rust 版 Pi（pi-rs）：用 CINDY_PI_RS=<二进制路径> 开启，默认不启用。
// pi-rs 只实现 Cindy 常规会话用到的 RPC 子集；会话依赖它没有的能力时一律回退到原版 Pi，
// 这样打开开关不会让任何功能静默变少。
import { existsSync, statSync } from 'node:fs';

// 原版 Pi 带、pi-rs 没有的能力。会话启动时 maker-core 会把这些写进参数/扩展，
// 判断依据是启动参数（见 maker-core agents/pi 的 args 组装）。
export const PI_RS_UNSUPPORTED = Object.freeze([
  'subagent',        // --extension 子代理扩展
  'skills',          // --skill / 受管技能目录
  'prompt-templates',// --prompt-template
  'mcp',             // MCP 桥
  'vision',          // vision / vision-locate 工具
  'ask-user-question', // ask_user_question 工具
  'native-provider-refresh', // 会话内刷新 provider
  'branch-switch',   // cindy-branch-switch 命令
  'remote',          // 远程设备托管会话
]);

export function piRsPath(env = process.env) {
  const p = env.CINDY_PI_RS;
  if (!p) return null;
  try { return existsSync(p) && statSync(p).isFile() ? p : null; } catch { return null; }
}

/**
 * 判断一次会话启动能否用 pi-rs。needs 是这次会话要用的能力集合（见 PI_RS_UNSUPPORTED），
 * 返回 { use, reason }。reason 用于日志，说明为什么回退。
 */
export function choosePiBinary({ original, needs = [], env = process.env }) {
  const rs = piRsPath(env);
  if (!rs) return { binary: original, use: false, reason: env.CINDY_PI_RS ? 'pi-rs binary not found' : 'disabled' };
  const blocked = needs.filter((n) => PI_RS_UNSUPPORTED.includes(n));
  if (blocked.length > 0) return { binary: original, use: false, reason: `needs ${blocked.join(',')}` };
  return { binary: rs, use: true, reason: 'ok' };
}
