// 冒烟测试：启动已打包的应用（CINDY_TAURI_SMOKE=1），等待其写出结果文件，判断 sidecar 通信是否成功。
// 用法：node scripts/smoke.mjs <可执行文件路径>
import { spawn } from 'node:child_process';
import { existsSync, readFileSync, rmSync } from 'node:fs';
import os from 'node:os';
import path from 'node:path';

const exe = process.argv[2];
if (!exe || !existsSync(exe)) {
  console.error(`executable not found: ${exe}`);
  process.exit(2);
}

const out = path.join(os.tmpdir(), `cindy-tauri-smoke-${Date.now()}.json`);
rmSync(out, { force: true });

const child = spawn(exe, [], {
  env: { ...process.env, CINDY_TAURI_SMOKE: '1', CINDY_TAURI_SMOKE_OUT: out },
  stdio: 'inherit',
});

const deadline = Date.now() + 660_000;
while (!existsSync(out) && Date.now() < deadline) {
  await new Promise((r) => setTimeout(r, 1000));
}

if (child.exitCode === null) child.kill();

if (!existsSync(out)) {
  console.error('smoke failed: no result file (timeout or app crashed)');
  process.exit(1);
}

const result = JSON.parse(readFileSync(out, 'utf8'));
console.log('smoke result:', JSON.stringify(result));
process.exit(result.ok ? 0 : 1);
