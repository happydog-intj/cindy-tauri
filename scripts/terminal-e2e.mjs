// sidecar 级终端测试（不起 Tauri）：真实 node-pty 起 shell，覆盖 terminal:* 全部通道。
// 用法：node scripts/terminal-e2e.mjs   （需要先 npm run sidecar:build、npm run native:fetch，headless/ 已打包）
import { spawn } from 'node:child_process';
import { mkdtempSync, readFileSync } from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import readline from 'node:readline';
import { fileURLToPath } from 'node:url';
import { decode, encode } from '../shared/ipc-codec.mjs';

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const sidecarJs = path.join(root, 'sidecar', 'dist', 'sidecar.cjs');
const dataDir = mkdtempSync(path.join(os.tmpdir(), 'cindy-terminal-e2e-'));
const workDir = mkdtempSync(path.join(os.tmpdir(), 'cindy-terminal-cwd-'));
const isWin = process.platform === 'win32';

function startSidecar() {
  const child = spawn(process.execPath, [sidecarJs], {
    cwd: path.dirname(sidecarJs),
    env: { ...process.env, CINDY_TAURI_DATA_DIR: dataDir },
    stdio: ['pipe', 'pipe', 'pipe'],
  });
  const errLines = [];
  const pushes = [];
  readline.createInterface({ input: child.stderr }).on('line', (l) => errLines.push(l));
  let seq = 0;
  const waiters = new Map();
  child.once('exit', (code) => {
    if (waiters.size === 0) return;
    console.error(`sidecar exited early (code ${code})\n${errLines.slice(-30).join('\n')}`);
    process.exit(1);
  });
  readline.createInterface({ input: child.stdout }).on('line', (line) => {
    const msg = JSON.parse(line);
    if (msg.native) {
      if (msg.native.id !== undefined) child.stdin.write(JSON.stringify({ nativeReply: { id: msg.native.id, result: null } }) + '\n');
      return;
    }
    if (msg.push) {
      pushes.push({ channel: msg.push.channel, args: decode(msg.push.payload) });
      return;
    }
    const w = waiters.get(msg.id);
    if (!w) return;
    waiters.delete(msg.id);
    if (msg.error) w.reject(new Error(msg.error)); else w.resolve(msg.result);
  });
  const call = (method, params) => new Promise((resolve, reject) => {
    const id = ++seq;
    waiters.set(id, { resolve, reject });
    child.stdin.write(JSON.stringify({ id, method, params }) + '\n');
  });
  const invoke = async (channel, ...a) => decode(await call('ipc.invoke', { channel, payload: encode(a) }));
  const stop = () => new Promise((resolve) => { child.once('exit', resolve); child.stdin.end(); });
  return { call, invoke, stop, errLines, pushes, child };
}

const steps = [];
function check(name, cond, detail) {
  steps.push({ name, ok: !!cond });
  console.log(`${cond ? 'PASS' : 'FAIL'} ${name}${detail !== undefined ? ` — ${typeof detail === 'string' ? detail : JSON.stringify(detail).slice(0, 300)}` : ''}`);
  if (!cond) throw new Error(`step failed: ${name}`);
}
const rejects = async (p) => { try { await p; return null; } catch (e) { return e.message; } };
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
// 去掉 ANSI / OSC 控制序列和回车，便于在输出里找标记。
const plain = (s) => s
  .replace(/\x1b\][^\x07\x1b]*(\x07|\x1b\\)/g, '')
  .replace(/\x1b\[[0-?]*[ -/]*[@-~]/g, '')
  .replace(/\x1b[()][0-9A-B]/g, '')
  .replace(/\x1b[=>78]/g, '')
  .replace(/\r/g, '');
const output = (sc, id, from = 0) => plain(sc.pushes.slice(from)
  .filter((p) => p.channel === 'terminal:data' && p.args[0]?.id === id)
  .map((p) => p.args[0].chunk).join(''));
async function waitFor(fn, what, ms = 20_000) {
  const end = Date.now() + ms;
  for (;;) {
    const v = fn();
    if (v) return v;
    if (Date.now() > end) throw new Error(`timeout waiting for ${what}`);
    await sleep(50);
  }
}
const alive = (pid) => { try { process.kill(pid, 0); return true; } catch { return false; } };

// 各平台的命令：输出里的标记不出现在键入的命令本身，避免把回显当成结果。
const cmd = isWin
  ? {
    marker: "Write-Output ('mark' + (6*7))\r",
    env: "Write-Output ('v=' + [bool]$env:CINDY_TAURI_DATA_DIR)\r",
    envOk: 'v=False',
    cwd: "Write-Output ('leaf=' + (Split-Path -Leaf (Get-Location).Path))\r",
    size: "Write-Output ('size=' + [Console]::WindowWidth + 'x' + [Console]::WindowHeight)\r",
    exit: 'exit 3\r',
  }
  : {
    marker: 'echo mark$((6*7))\r',
    env: 'echo "v=${CINDY_TAURI_DATA_DIR:-none}"\r',
    envOk: 'v=none',
    cwd: 'echo "leaf=${PWD##*/}"\r',
    size: 'echo "size=$(tput cols)x$(tput lines)"\r',
    exit: 'exit 3\r',
  };

let failed = false;
let sc = startSidecar();
try {
  await sc.call('ping', {});

  // ---------- shell 列表与偏好 ----------
  const shells = await sc.invoke('terminal:list-available-shells');
  check('available shells listed', Array.isArray(shells) && shells.length > 0 && shells.some((s) => s.isAutoDetectTarget), shells.map((s) => s.id));
  check('default pref is auto', (await sc.invoke('terminal:get-default-shell-pref')) === 'auto');
  check('invalid pref rejected', /INVALID_PARAMS/.test(await rejects(sc.invoke('terminal:set-default-shell-pref', 'nope')) ?? ''));
  const pick = isWin ? (shells.find((s) => s.id === 'pwsh') ?? shells.find((s) => s.id === 'powershell')) : shells.find((s) => s.isAutoDetectTarget);
  check('picked a test shell', !!pick, pick?.id);
  await sc.invoke('terminal:set-default-shell-pref', pick.id);
  check('pref persisted', (await sc.invoke('terminal:get-default-shell-pref')) === pick.id
    && JSON.parse(readFileSync(path.join(dataDir, 'settings', 'terminal-prefs.json'), 'utf8')).defaultShellPref === pick.id);

  // ---------- 参数校验 ----------
  check('create without id rejected', /INVALID_PARAMS/.test(await rejects(sc.invoke('terminal:create', { cwd: workDir })) ?? ''));
  check('create with bad pref rejected', /INVALID_PARAMS/.test(await rejects(sc.invoke('terminal:create', { id: 'x', cwd: workDir, shellPref: 'bogus' })) ?? ''));
  check('write to unknown rejected', /TERMINAL_NOT_FOUND/.test(await rejects(sc.invoke('terminal:write', 'ghost', 'ls\r')) ?? ''));
  check('resize to unknown rejected', /TERMINAL_NOT_FOUND/.test(await rejects(sc.invoke('terminal:resize', 'ghost', 80, 24)) ?? ''));
  check('dispose unknown is no-op', (await sc.invoke('terminal:dispose', 'ghost')) === undefined);

  // ---------- 创建与 I/O ----------
  const id = 'tab-1';
  const meta = await sc.invoke('terminal:create', { id, cwd: workDir, cols: 100, rows: 30 });
  check('create returns shell meta', meta.shellId === pick.id && Number.isInteger(meta.pid) && meta.pid > 0, meta);
  check('pty process alive', alive(meta.pid));
  const again = await sc.invoke('terminal:create', { id, cwd: workDir, cols: 100, rows: 30 });
  check('create is idempotent (re-attach)', again.pid === meta.pid, again);
  check('restart before exit rejected', /PRECONDITION_FAILED/.test(await rejects(sc.invoke('terminal:restart', id)) ?? ''));

  let mark = sc.pushes.length;
  await sc.invoke('terminal:write', id, cmd.marker);
  await waitFor(() => output(sc, id, mark).includes('mark42'), 'marker output');
  check('command output streamed', true);

  mark = sc.pushes.length;
  await sc.invoke('terminal:write', id, cmd.env);
  await waitFor(() => output(sc, id, mark).includes(cmd.envOk), 'env output');
  check('CINDY_* env not leaked to shell', true, cmd.envOk);

  mark = sc.pushes.length;
  await sc.invoke('terminal:write', id, cmd.cwd);
  // 只比最后一段：macOS 有 /var → /private/var，Windows 临时目录可能是 8.3 短路径。
  const leaf = path.basename(workDir);
  await waitFor(() => output(sc, id, mark).includes(`leaf=${leaf}`), 'cwd output');
  check('shell started in cwd', true, leaf);

  await sc.invoke('terminal:resize', id, 120, 40);
  mark = sc.pushes.length;
  await sc.invoke('terminal:write', id, cmd.size);
  const sizeOut = await waitFor(() => output(sc, id, mark).match(/size=(\d+)x(\d+)/), 'size output');
  check('resize reaches the pty', sizeOut[1] === '120' && sizeOut[2] === '40', sizeOut[0]);
  check('resize with bad cols rejected', /INVALID_PARAMS/.test(await rejects(sc.invoke('terminal:resize', id, 0, 40)) ?? ''));

  // 按键逐个到达时合并写入，顺序不乱。
  if (!isWin) {
    mark = sc.pushes.length;
    for (const ch of 'echo "中文-$((2+3))"\r') await sc.invoke('terminal:write', id, ch);
    await waitFor(() => output(sc, id, mark).includes('中文-5'), 'utf-8 output');
    check('per-key writes and UTF-8 round trip', true);
  }

  // ---------- 退出与重启 ----------
  await sc.invoke('terminal:write', id, cmd.exit);
  const exitPush = await waitFor(() => sc.pushes.find((p) => p.channel === 'terminal:exit' && p.args[0]?.id === id), 'exit push');
  check('exit pushed with code', exitPush.args[0].exit.code === 3, exitPush.args[0]);
  check('write after exit is ignored', (await sc.invoke('terminal:write', id, 'ls\r')) === undefined);
  check('create on exited session rejected', /TERMINAL_SPAWN_FAILED/.test(await rejects(sc.invoke('terminal:create', { id, cwd: workDir })) ?? ''));
  const re = await sc.invoke('terminal:restart', id);
  check('restart spawns a new shell', re.pid !== meta.pid && alive(re.pid), re);
  mark = sc.pushes.length;
  await sc.invoke('terminal:write', id, cmd.marker);
  await waitFor(() => output(sc, id, mark).includes('mark42'), 'marker after restart');
  check('restarted shell accepts input', true);

  // ---------- dispose 与退出清理 ----------
  await sc.invoke('terminal:dispose', id);
  check('disposed session gone', /TERMINAL_NOT_FOUND/.test(await rejects(sc.invoke('terminal:write', id, 'ls\r')) ?? ''));
  await waitFor(() => !alive(re.pid), 'disposed pty exit', 10_000);
  check('dispose kills the shell', true);

  const t2 = await sc.invoke('terminal:create', { id: 'tab-2', cwd: path.join(workDir, 'missing-dir') });
  check('missing cwd falls back to home', alive(t2.pid), t2);
  await sc.stop();
  await waitFor(() => !alive(t2.pid), 'pty exit after sidecar shutdown', 10_000);
  check('sidecar shutdown kills remaining shells', true);
} catch (err) {
  failed = true;
  console.error(err.stack ?? err);
  console.error(sc.errLines.slice(-20).join('\n'));
} finally {
  try { sc.child.kill(); } catch { /* 已退出 */ }
}

const passed = steps.filter((s) => s.ok).length;
console.log(`terminal e2e: ${passed}/${steps.length} steps passed${failed ? ' (FAILED)' : ''}`);
process.exit(failed ? 1 : 0);
