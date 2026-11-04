// Sidecar 入口：通过 stdin/stdout 按行收发 JSON-RPC 风格的消息。
// 请求：{"id": 1, "method": "ping", "params": {}}
// 响应：{"id": 1, "result": {...}} 或 {"id": 1, "error": "..."}
import readline from 'node:readline';
import { format } from 'node:util';

import { execFile } from 'node:child_process';
import { existsSync, readFileSync } from 'node:fs';
import { copyFile, mkdir, mkdtemp, readFile, writeFile } from 'node:fs/promises';
import path from 'node:path';
import { agentStatus, installAgent } from './agents.mjs';
import { ipcMethods, setPush } from './ipc.mjs';
import { registerBootStubs } from './boot-stubs.mjs';
import { openLocalDb, registerLocalDbIpc } from './local-db/index.mjs';
import * as models from './model-config.mjs';
import { registerMakerIpc } from './maker-ipc.mjs';
import { initSecretStore, registerSecretIpc, secretMethods } from './secret-store.mjs';
import { handleNativeReply, setNativeWriter } from './native.mjs';
import { registerDesktopShellIpc } from './desktop-shell.mjs';
import { registerSettingsIpc } from './settings-ipc.mjs';
import { registerBrowserPanelIpc } from './browser-panel.mjs';

// stdout 只用于 JSON 协议，其他模块的 console.log/info 改写到 stderr。
for (const k of ['log', 'info', 'debug']) {
  console[k] = (...a) => process.stderr.write(format(...a) + '\n');
}

// 打包后的布局：sidecar.cjs 旁边是 headless/cli.cjs、headless/profiles/ 和 headless/agents/。
const headlessDir = path.join(__dirname, 'headless');
const headlessCli = path.join(headlessDir, 'cli.cjs');
const agentManifestDir = path.join(headlessDir, 'agents');

// Tauri 通过环境变量传入应用数据目录；未设置时（直接跑 sidecar）用当前目录下的 .cindy-data。
const dataDir = path.resolve(process.env.CINDY_TAURI_DATA_DIR ?? '.cindy-data');

// P2 验证阶段：headless.run 会在无沙箱、无审批的情况下执行 agent 命令，
// 只在冒烟或开发模式下开放，工作目录固定在数据目录下新建的临时文件夹。
const runEnabled = process.env.CINDY_TAURI_SMOKE === '1' || process.env.CINDY_TAURI_DEV === '1';

// 子进程环境：去掉可能泄露或覆盖路由的网关变量，只用 profile 里的自定义模型地址。
function headlessEnv(extra = {}) {
  const env = { ...process.env, ...extra };
  for (const k of ['CINDY_HEADLESS_API_KEY', 'ANTHROPIC_API_KEY', 'CINDY_HEADLESS_BASE_URL', 'ANTHROPIC_BASE_URL', 'CINDY_CODEX_API_KEY', 'CINDY_HEADLESS_CONFIG_FILE']) delete env[k];
  return env;
}

// 用当前 Node（随包的 cindy-node）以子进程运行 cindy-headless CLI，返回解析后的 JSON。
function runHeadless(args, { timeoutMs = 60_000, env, cwd, allowFailure = false } = {}) {
  if (!existsSync(headlessCli)) throw new Error(`cindy-headless not bundled: ${headlessCli}`);
  return new Promise((resolve, reject) => {
    execFile(
      process.execPath,
      ['--no-warnings', headlessCli, ...args],
      { timeout: timeoutMs, maxBuffer: 16 * 1024 * 1024, windowsHide: true, env: env ?? headlessEnv(), cwd },
      (err, stdout, stderr) => {
        // run 在 infra-* 状态下退出码为 1，但 stdout 仍是完整的结果 JSON。
        if (err && !(allowFailure && stdout.trim())) {
          reject(new Error(`cindy-headless ${args[0]} failed: ${(stderr || stdout || err.message).trim().slice(-2000)}`));
          return;
        }
        try {
          resolve(JSON.parse(stdout));
        } catch {
          reject(new Error(`cindy-headless ${args[0]} returned non-JSON output`));
        }
      },
    );
  });
}

// profile 只接受随包 profiles/ 下的相对路径，避免任意路径读取。
function bundledProfile(rel) {
  if (typeof rel !== 'string' || !rel) throw new Error('params.profile is required');
  const root = path.join(headlessDir, 'profiles');
  const full = path.resolve(root, rel);
  if (full !== root && !full.startsWith(root + path.sep)) throw new Error('profile must stay inside bundled profiles');
  return full;
}

// 模型配置校验：baseUrl 只允许 HTTPS 或本机 HTTP（与 cindy-headless 的规则一致），不接受内嵌凭据。
function validateModel(model) {
  if (!model || typeof model !== 'object') throw new Error('params.model is required');
  const { baseUrl, id } = model;
  if (typeof id !== 'string' || !/^[A-Za-z0-9._:/-]{1,128}$/.test(id)) throw new Error('params.model.id is invalid');
  let url;
  try { url = new URL(baseUrl); } catch { throw new Error('params.model.baseUrl is invalid'); }
  const loopback = ['127.0.0.1', 'localhost', '[::1]'].includes(url.hostname);
  if (url.username || url.password || !(url.protocol === 'https:' || (url.protocol === 'http:' && loopback))) {
    throw new Error('params.model.baseUrl must be HTTPS or loopback HTTP without credentials');
  }
  const contextWindow = Number.isInteger(model.contextWindow) && model.contextWindow > 0 ? model.contextWindow : 128000;
  const maxTokens = Number.isInteger(model.maxTokens) && model.maxTokens > 0 ? model.maxTokens : 8192;
  return { baseUrl: url.href.replace(/\/+$/, ''), id, contextWindow, maxTokens, reasoning: model.reasoning === true };
}

// 以随包的 cindy-production-pi BYOM 模板为基础，生成桌面端 profile。
// 桌面上没有容器，containerSandbox=false；临时工作目录不是 git 仓库，关闭 Pi 项目 skill 扫描。
async function writeDesktopProfile(dir, piBinary, model) {
  const templateDir = path.join(headlessDir, 'profiles', 'cindy-production-pi');
  const p = JSON.parse(await readFile(path.join(templateDir, 'profile.byom.example.json'), 'utf8'));
  delete p.parentProfile;
  delete p.changedDimensions;
  p.id = 'cindy-tauri-desktop-pi';
  p.agentBinaryPath = piBinary;
  p.supportedModelIds = [model.id];
  p.model = { provider: 'desktop-byom', requestedId: model.id, routeId: 'desktop-byom', contextLimit: model.contextWindow, maxOutputTokens: model.maxTokens };
  p.nativeProviders = [{
    id: 'desktop-byom',
    name: 'Desktop OpenAI-compatible provider',
    baseUrl: model.baseUrl,
    api: 'openai-completions',
    models: [{ id: model.id, contextWindow: model.contextWindow, maxTokens: model.maxTokens, reasoning: model.reasoning, input: ['text'] }],
  }];
  p.containerSandbox = false;
  p.piProjectSkills = { enabled: false, roots: [] };
  await copyFile(path.join(templateDir, 'prompt.md'), path.join(dir, 'prompt.md'));
  const file = path.join(dir, 'profile.json');
  await writeFile(file, JSON.stringify(p, null, 2));
  return file;
}

async function headlessRun(params) {
  if (!runEnabled) throw new Error('headless.run is only enabled in smoke or dev mode during P2');
  if (typeof params.task !== 'string' || !params.task.trim() || params.task.length > 16000) throw new Error('params.task is required (max 16000 chars)');
  const model = validateModel(params.model);
  const timeoutMs = Math.min(Math.max(Number(params.timeoutMs) || 180_000, 10_000), 600_000);
  const { binary } = await installAgent('pi', { manifestDir: agentManifestDir, dataDir });
  await mkdir(path.join(dataDir, 'runs'), { recursive: true });
  const runDir = await mkdtemp(path.join(dataDir, 'runs', 'run-'));
  const workDir = path.join(runDir, 'workspace');
  const outDir = path.join(runDir, 'output');
  await mkdir(workDir);
  await mkdir(outDir);
  const profile = await writeDesktopProfile(runDir, binary, model);
  const result = await runHeadless(
    ['run', '--profile', profile, '--task', params.task, '--working-dir', workDir, '--output-dir', outDir, '--timeout-ms', String(timeoutMs)],
    { timeoutMs: timeoutMs + 30_000, env: headlessEnv({ CINDY_HEADLESS_ALLOW_UNSANDBOXED_BYPASS: '1' }), cwd: runDir, allowFailure: true },
  );
  return {
    status: result.status,
    durationMs: result.durationMs,
    error: result.error ?? null,
    finalText: readFinalText(path.join(outDir, 'trace.jsonl')),
    runDir,
    workDir,
  };
}

// 从 trace.jsonl 里取最后一条最终文本。
function readFinalText(traceFile) {
  if (!existsSync(traceFile)) return null;
  let text = null;
  for (const line of readFileSync(traceFile, 'utf8').split('\n')) {
    if (!line.includes('"type":"text"')) continue;
    try {
      const ev = JSON.parse(line);
      if (ev.data?.isFinal) text = ev.data.text;
    } catch { /* 忽略损坏行 */ }
  }
  return text;
}

// Rust 专用方法（native.*）在 desktop-shell 注册后合并进来。
const rustOnly = {};

const handlers = {
  ping: () => ({ pong: true, at: new Date().toISOString() }),
  info: () => ({ node: process.version, platform: process.platform, arch: process.arch, pid: process.pid }),
  echo: (params) => params,
  'headless.version': () => runHeadless(['version']),
  'headless.capabilities': (params) => runHeadless(['capabilities', '--profile', bundledProfile(params.profile)]),
  'agent.status': (params) => agentStatus(params.kind ?? 'pi', { manifestDir: agentManifestDir, dataDir }),
  'agent.install': (params) => installAgent(params.kind ?? 'pi', { manifestDir: agentManifestDir, dataDir }),
  'headless.run': headlessRun,
  ...ipcMethods,
  // 只由 Rust 侧调用（sidecar_call 拒绝 webview 调 secrets.*）。
  ...secretMethods,
};
const isRustOnly = (method) => Object.hasOwn(rustOnly, method);

function reply(msg) {
  process.stdout.write(JSON.stringify(msg) + '\n');
}

// 没有 id 的推送消息，Rust 侧转发为 Tauri 事件 "cindy:ipc"。
setPush((push) => reply({ push }));
setNativeWriter((msg) => reply(msg));
registerBootStubs({ dataDir, agentManifestDir });
registerSettingsIpc({ dataDir, piStatus: () => agentStatus('pi', { manifestDir: agentManifestDir, dataDir }) });

// §9.5 会话链路：本地库（会话列表 / 历史）→ 模型配置 → maker 会话。注册顺序靠后者覆盖 boot stub。
const LOCAL_OWNER_ID = 'local-v1';
const db = openLocalDb({ dataDir, ownerId: LOCAL_OWNER_ID });
initSecretStore({ dataDir });
registerSecretIpc();
models.registerProviderIpc({ dataDir });
const makerIpc = registerMakerIpc({ dataDir, db, models, agentManifestDir });
registerLocalDbIpc(db, { isRunning: (sid) => makerIpc.isTurnRunning?.(sid) ?? false });
const shell = registerDesktopShellIpc({ dataDir, db, anySessionInTurn: () => makerIpc.anySessionInTurn?.() ?? false });
Object.assign(rustOnly, shell.nativeMethods);
registerBrowserPanelIpc({ dataDir });

const rl = readline.createInterface({ input: process.stdin });
const pending = new Set();

rl.on('line', (line) => {
  const task = handleLine(line);
  pending.add(task);
  task.finally(() => pending.delete(task));
});

async function handleLine(line) {
  let req;
  try {
    req = JSON.parse(line);
  } catch {
    reply({ id: null, error: 'invalid json' });
    return;
  }
  // Rust 对 sidecar → Rust 反向调用的回复。
  if (req && typeof req === 'object' && req.nativeReply) {
    handleNativeReply(req.nativeReply);
    return;
  }
  const handler = isRustOnly(req.method) ? rustOnly[req.method] : handlers[req.method];
  if (!handler) {
    reply({ id: req.id, error: `unknown method: ${req.method}` });
    return;
  }
  try {
    reply({ id: req.id, result: await handler(req.params ?? {}) });
  } catch (err) {
    reply({ id: req.id, error: String(err?.message ?? err) });
  }
}

// stdin 关闭后等待未完成的请求回复完再退出。
rl.on('close', async () => {
  await Promise.allSettled([...pending]);
  await Promise.resolve(makerIpc.shutdown?.()).catch(() => undefined);
  try { db.close(); } catch { /* 已关闭 */ }
  process.exit(0);
});
