// Sidecar 入口：通过 stdin/stdout 按行收发 JSON-RPC 风格的消息。
// 请求：{"id": 1, "method": "ping", "params": {}}
// 响应：{"id": 1, "result": {...}} 或 {"id": 1, "error": "..."}
import readline from 'node:readline';
import { execFile } from 'node:child_process';
import { existsSync } from 'node:fs';
import path from 'node:path';

// 打包后的布局：sidecar.cjs 旁边是 headless/cli.cjs 和 headless/profiles/。
const headlessDir = path.join(__dirname, 'headless');
const headlessCli = path.join(headlessDir, 'cli.cjs');

// 用当前 Node（随包的 cindy-node）以子进程运行 cindy-headless CLI，返回解析后的 JSON。
function runHeadless(args, timeoutMs = 60_000) {
  if (!existsSync(headlessCli)) throw new Error(`cindy-headless not bundled: ${headlessCli}`);
  return new Promise((resolve, reject) => {
    execFile(
      process.execPath,
      ['--no-warnings', headlessCli, ...args],
      { timeout: timeoutMs, maxBuffer: 16 * 1024 * 1024, windowsHide: true },
      (err, stdout, stderr) => {
        if (err) {
          reject(new Error(`cindy-headless ${args[0]} failed: ${(stderr || stdout || err.message).trim().slice(0, 2000)}`));
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

const handlers = {
  ping: () => ({ pong: true, at: new Date().toISOString() }),
  info: () => ({ node: process.version, platform: process.platform, arch: process.arch, pid: process.pid }),
  echo: (params) => params,
  'headless.version': () => runHeadless(['version']),
  'headless.capabilities': (params) => runHeadless(['capabilities', '--profile', bundledProfile(params.profile)]),
};

function reply(msg) {
  process.stdout.write(JSON.stringify(msg) + '\n');
}

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
  const handler = handlers[req.method];
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
  process.exit(0);
});
