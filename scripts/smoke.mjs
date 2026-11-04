// 冒烟测试：启动已打包的应用（CINDY_TAURI_SMOKE=1），等待其写出结果文件，判断 sidecar 通信是否成功。
// 用法：node scripts/smoke.mjs <可执行文件路径> [--session]
// --session：先启动 scripts/mock-openai.mjs，用同一个临时数据目录连续启动应用两次
//   （CINDY_TAURI_SESSION_SMOKE=run 再 =verify），两次都 ok 才算通过。
//   CINDY_TAURI_SMOKE_LOG_DIR 指定时，mock 请求日志和各阶段结果写到该目录（CI 失败时上传）。
import { spawn } from 'node:child_process';
import { createWriteStream, existsSync, mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import readline from 'node:readline';
import { fileURLToPath } from 'node:url';

const args = process.argv.slice(2);
const session = args.includes('--session');
const exe = args.find((a) => !a.startsWith('--'));
if (!exe || !existsSync(exe)) {
  console.error(`executable not found: ${exe}`);
  process.exit(2);
}

// 启动一次应用，等结果文件；超时或崩溃返回 null。
async function runApp(extraEnv, label, timeoutMs = 660_000) {
  const out = path.join(os.tmpdir(), `cindy-tauri-smoke-${label}-${Date.now()}.json`);
  rmSync(out, { force: true });
  const child = spawn(exe, [], {
    env: { ...process.env, ...extraEnv, CINDY_TAURI_SMOKE: '1', CINDY_TAURI_SMOKE_OUT: out },
    stdio: 'inherit',
  });
  let exited = false;
  child.once('exit', () => { exited = true; });
  const deadline = Date.now() + timeoutMs;
  while (!existsSync(out) && Date.now() < deadline && !exited) {
    await new Promise((r) => setTimeout(r, 1000));
  }
  // 进程退出前可能刚写完结果，稍等一下再判断。
  if (!existsSync(out)) await new Promise((r) => setTimeout(r, 1000));
  if (!existsSync(out)) {
    if (child.exitCode === null) child.kill();
    return null;
  }
  // 等应用自己退出（smoke_finish 会 app.exit），避免第二次启动时数据库仍被占用。
  const exitDeadline = Date.now() + 15_000;
  while (child.exitCode === null && child.signalCode === null && Date.now() < exitDeadline) {
    await new Promise((r) => setTimeout(r, 200));
  }
  if (child.exitCode === null && child.signalCode === null) child.kill();
  // 结果文件可能还在写，解析失败时重试几次。
  for (let i = 0; i < 5; i++) {
    try {
      return JSON.parse(readFileSync(out, 'utf8'));
    } catch {
      await new Promise((r) => setTimeout(r, 300));
    }
  }
  return { ok: false, detail: 'result file is not valid JSON' };
}

// 启动 mock 模型，读 stdout 第一行的 {"port":N}；stderr 请求日志写到 logFile。
async function startMock(logFile) {
  const script = path.join(path.dirname(fileURLToPath(import.meta.url)), 'mock-openai.mjs');
  const child = spawn(process.execPath, [script], { stdio: ['ignore', 'pipe', 'pipe'] });
  const log = createWriteStream(logFile, { flags: 'a' });
  child.stderr.pipe(log);
  const port = await new Promise((resolve, reject) => {
    const t = setTimeout(() => reject(new Error('mock-openai did not report a port in 10s')), 10_000);
    child.once('exit', (code) => reject(new Error(`mock-openai exited early (${code})`)));
    readline.createInterface({ input: child.stdout }).once('line', (line) => {
      clearTimeout(t);
      try {
        resolve(JSON.parse(line).port);
      } catch {
        reject(new Error(`unexpected mock-openai output: ${line}`));
      }
    });
  });
  return { port, stop: () => { child.kill(); log.end(); } };
}

// 解析 detail 里的 sessionSmoke 字段。
function sessionDetail(result) {
  try {
    return JSON.parse(result.detail).sessionSmoke ?? null;
  } catch {
    return null;
  }
}

async function sessionSmoke() {
  const logDir = process.env.CINDY_TAURI_SMOKE_LOG_DIR
    ? path.resolve(process.env.CINDY_TAURI_SMOKE_LOG_DIR)
    : mkdtempSync(path.join(os.tmpdir(), 'cindy-tauri-smoke-logs-'));
  mkdirSync(logDir, { recursive: true });
  const mockLog = path.join(logDir, 'mock-openai.log');
  // 数据目录：可用 CINDY_TAURI_SMOKE_DATA_DIR 指定（例如复用已下载的 Pi），默认新建临时目录。
  const dataDir = process.env.CINDY_TAURI_SMOKE_DATA_DIR
    ? path.resolve(process.env.CINDY_TAURI_SMOKE_DATA_DIR)
    : mkdtempSync(path.join(os.tmpdir(), 'cindy-tauri-smoke-data-'));
  mkdirSync(dataDir, { recursive: true });
  console.log(`session smoke: dataDir=${dataDir} logs=${logDir}`);

  const mock = await startMock(mockLog);
  console.log(`session smoke: mock-openai on 127.0.0.1:${mock.port}`);
  // Pi 的 openai-completions provider 在 baseUrl 后直接拼 /chat/completions，所以不带 /v1。
  const env = {
    CINDY_TAURI_DATA_DIR: dataDir,
    CINDY_TAURI_SMOKE_MODEL_BASE_URL: `http://127.0.0.1:${mock.port}`,
    CINDY_TAURI_SMOKE_MODEL_ID: 'mock-model',
  };
  const phases = {};
  try {
    for (const phase of ['run', 'verify']) {
      const result = await runApp({ ...env, CINDY_TAURI_SESSION_SMOKE: phase }, phase);
      phases[phase] = result ?? { ok: false, detail: 'no result file (timeout or app crashed)' };
      writeFileSync(path.join(logDir, `smoke-${phase}.json`), JSON.stringify(phases[phase], null, 2));
      console.log(`smoke ${phase} result:`, JSON.stringify(phases[phase]));
      if (!phases[phase].ok) break;
    }
  } finally {
    mock.stop();
  }

  if (!phases.run?.ok || !phases.verify?.ok) {
    console.error('session smoke failed');
    return false;
  }
  // 交叉核对：verify 阶段的会话列表里必须有 run 阶段创建的会话。
  const runId = sessionDetail(phases.run)?.sessionId;
  const verifyIds = sessionDetail(phases.verify)?.sessionIds ?? [];
  if (!runId || !verifyIds.includes(runId)) {
    console.error(`session smoke failed: session ${runId} from run phase not listed after restart`);
    return false;
  }
  console.log(`session smoke ok: session ${runId} survived restart`);
  return true;
}

if (session) {
  process.exit((await sessionSmoke()) ? 0 : 1);
}

const result = await runApp({}, 'basic');
if (!result) {
  console.error('smoke failed: no result file (timeout or app crashed)');
  process.exit(1);
}
console.log('smoke result:', JSON.stringify(result));
process.exit(result.ok ? 0 : 1);
