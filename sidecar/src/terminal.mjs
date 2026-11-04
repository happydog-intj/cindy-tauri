// 右侧栏终端：desktop main/terminal/{shellResolver,ptyManager,terminalPrefsStore}.ts + maker-ipc/terminal-handlers.ts。
// PTY 用 node-pty（N-API 预编译，随包放在 resources/sidecar/native/node_modules/）。
// 与 desktop 的差别：只有一个窗口，输出统一推给主 webview；没有独立子窗口的 owner 转移。
import { existsSync, statSync } from 'node:fs';
import { createRequire } from 'node:module';
import path from 'node:path';
import { emitToRenderer, registerInvoke } from './ipc.mjs';
import { readJson, writeJsonAtomic } from './settings/override-file.mjs';

const TERMINAL_NAME = 'xterm-256color';
const DEFAULT_COLS = 80;
const DEFAULT_ROWS = 24;
const MAX_SESSIONS = 32;
// 单次写入上限：粘贴大段文本也够用，防止异常调用把内存打满。
const MAX_WRITE_CHARS = 1024 * 1024;
export const SHELL_PREFS = ['auto', 'zsh', 'bash', 'fish', 'sh', 'pwsh', 'powershell', 'cmd', 'gitbash', 'wsl'];

// desktop ptyManager.ts ENV_KEYS_TO_STRIP：父进程带来的终端程序标记会让 shell 启动脚本输出 xterm.js 不认识的序列。
// CINDY_* 是本应用传给 sidecar 的内部变量（数据目录、冒烟开关等），不给用户的 shell。
const ENV_KEYS_TO_STRIP = [
  'TERMINFO', 'TERMINFO_DIRS', 'TERM_PROGRAM', 'TERM_PROGRAM_VERSION', 'TERM_SESSION_ID',
  'LC_TERMINAL', 'LC_TERMINAL_VERSION', 'ITERM_PROFILE', 'ITERM_SESSION_ID', 'ITERM_SHELL_INTEGRATION_INSTALLED',
  'VSCODE_INJECTION', 'VSCODE_PID', 'VSCODE_GIT_IPC_HANDLE', 'VSCODE_GIT_ASKPASS_NODE', 'VSCODE_GIT_ASKPASS_EXTRA_ARGS',
  'VSCODE_GIT_ASKPASS_MAIN', 'VSCODE_IPC_HOOK_CLI', 'VSCODE_NLS_CONFIG', 'VSCODE_CWD', 'GIT_ASKPASS',
  'ELECTRON_RUN_AS_NODE',
];

const fail = (code, msg) => {
  const err = new Error(`[${code}] ${msg}`);
  err.code = code;
  throw err;
};

// ── shell 解析（desktop shellResolver.ts）──────────────────────────
function joinFor(platform, dir, base) {
  return platform === 'win32' ? path.win32.join(dir, base) : path.posix.join(dir, base);
}

function isFile(p) {
  try { return existsSync(p) && statSync(p).isFile(); } catch { return false; }
}

export function whichSync(bin, platform = process.platform, env = process.env) {
  if (platform === 'win32') {
    const exts = (env.PATHEXT ?? '.EXE;.CMD;.BAT').split(';').map((e) => e.toLowerCase()).filter(Boolean);
    const lower = bin.toLowerCase();
    const tries = exts.some((e) => lower.endsWith(e)) ? [''] : ['', ...exts];
    for (const dir of (env.PATH ?? env.Path ?? '').split(';')) {
      if (!dir) continue;
      for (const ext of tries) {
        const c = joinFor(platform, dir, bin + ext);
        if (isFile(c)) return c;
      }
    }
    return null;
  }
  for (const dir of (env.PATH ?? '').split(':')) {
    if (!dir) continue;
    const c = joinFor(platform, dir, bin);
    if (isFile(c)) return c;
  }
  return null;
}

function findGitBashOnWindows(env) {
  const direct = whichSync('git-bash.exe', 'win32', env);
  if (direct) return direct;
  const git = whichSync('git.exe', 'win32', env);
  if (git) {
    const gitDir = path.win32.dirname(git);
    for (const c of [path.win32.join(gitDir, 'bash.exe'), path.win32.join(path.win32.dirname(gitDir), 'bin', 'bash.exe')]) {
      if (isFile(c)) return c;
    }
  }
  for (const key of ['ProgramFiles', 'ProgramFiles(x86)', 'LocalAppData']) {
    const r = env[key];
    if (!r) continue;
    const c = path.win32.join(r, 'Git', 'bin', 'bash.exe');
    if (isFile(c)) return c;
  }
  return null;
}

const NIX_NAMES = { zsh: 'zsh', bash: 'bash', fish: 'fish', sh: 'sh' };

function autoDetect(platform, env) {
  if (platform === 'win32') {
    const pwsh = whichSync('pwsh.exe', platform, env);
    if (pwsh) return { id: 'pwsh', command: pwsh, args: [], displayName: 'PowerShell' };
    const ps = whichSync('powershell.exe', platform, env);
    if (ps) return { id: 'powershell', command: ps, args: [], displayName: 'Windows PowerShell' };
    return { id: 'cmd', command: env.COMSPEC?.trim() || 'cmd.exe', args: [], displayName: 'Command Prompt' };
  }
  const shellEnv = env.SHELL?.trim();
  if (shellEnv && existsSync(shellEnv)) {
    const base = path.basename(shellEnv).toLowerCase();
    return { id: NIX_NAMES[base] ?? 'sh', command: shellEnv, args: [], displayName: base };
  }
  for (const id of ['zsh', 'bash', 'sh']) {
    if (existsSync(`/bin/${id}`)) return { id, command: `/bin/${id}`, args: [], displayName: id };
  }
  return { id: 'sh', command: '/bin/sh', args: [], displayName: 'sh' };
}

function byId(id, platform, env) {
  if (platform === 'win32') {
    switch (id) {
      case 'pwsh': { const p = whichSync('pwsh.exe', platform, env); return p ? { id, command: p, args: [], displayName: 'PowerShell' } : null; }
      case 'powershell': { const p = whichSync('powershell.exe', platform, env); return p ? { id, command: p, args: [], displayName: 'Windows PowerShell' } : null; }
      case 'cmd': return { id, command: env.COMSPEC?.trim() || 'cmd.exe', args: [], displayName: 'Command Prompt' };
      case 'gitbash': { const p = findGitBashOnWindows(env); return p ? { id, command: p, args: ['--login', '-i'], displayName: 'Git Bash' } : null; }
      case 'wsl': { const p = whichSync('wsl.exe', platform, env); return p ? { id, command: p, args: [], displayName: 'WSL' } : null; }
      default: return null;
    }
  }
  if (!NIX_NAMES[id]) return null;
  const p = whichSync(id, platform, env) ?? (id === 'fish' ? null : `/bin/${id}`);
  return p && existsSync(p) ? { id, command: p, args: [], displayName: id } : null;
}

export function resolveShellForCreate(pref, platform = process.platform, env = process.env) {
  if (pref == null || pref === 'auto') return autoDetect(platform, env);
  return byId(pref, platform, env) ?? autoDetect(platform, env);
}

export function probeAvailableShells(platform = process.platform, env = process.env) {
  const auto = autoDetect(platform, env).id;
  const ids = platform === 'win32' ? ['pwsh', 'powershell', 'cmd', 'gitbash', 'wsl'] : ['zsh', 'bash', 'fish', 'sh'];
  const out = [];
  for (const id of ids) {
    const r = byId(id, platform, env);
    if (r) out.push({ id: r.id, command: r.command, displayName: r.displayName, isAutoDetectTarget: r.id === auto });
  }
  return out;
}

// ── PTY 环境（desktop ptyManager.ts spawnSession）──────────────────
export function buildTerminalEnv(parent, platform) {
  const env = {};
  for (const [k, v] of Object.entries(parent)) {
    if (typeof v === 'string' && !k.startsWith('CINDY_')) env[k] = v;
  }
  // macOS 从 Finder 启动可能是 C locale，zsh 行编辑器需要 UTF-8 的 LC_CTYPE 才能输入中文。
  if (platform === 'darwin' && !env.LC_ALL?.trim()) {
    const loc = (env.LC_CTYPE?.trim() || env.LANG?.trim() || 'C').toUpperCase();
    if (loc === 'C' || loc === 'POSIX') env.LC_CTYPE = 'UTF-8';
  }
  for (const k of ENV_KEYS_TO_STRIP) delete env[k];
  env.TERM = TERMINAL_NAME;
  return env;
}

// node-pty 放在 sidecar.cjs 同级的 native/node_modules（打包后）或仓库 node_modules（开发 / e2e）。
let ptyModule = null;
function loadNodePty() {
  if (ptyModule) return ptyModule;
  const req = createRequire(__filename);
  const dirs = [path.join(path.dirname(__filename), 'native', 'node_modules')];
  if (process.env.CINDY_TAURI_NATIVE_DIR) dirs.unshift(process.env.CINDY_TAURI_NATIVE_DIR);
  let lastErr;
  for (const d of dirs) {
    try { ptyModule = req(path.join(d, 'node-pty')); return ptyModule; } catch (e) { lastErr = e; }
  }
  try { ptyModule = req('node-pty'); return ptyModule; } catch (e) { lastErr = e; }
  fail('UNSUPPORTED_CAPABILITY', `terminal backend unavailable: ${lastErr?.message ?? 'node-pty not found'}`);
}

const reqString = (v, name) => {
  if (typeof v !== 'string' || v.length === 0) fail('INVALID_PARAMS', `${name} must be a non-empty string`);
  return v;
};
const reqPosInt = (v, name) => {
  if (!Number.isInteger(v) || v < 1 || v > 10_000) fail('INVALID_PARAMS', `${name} must be a positive integer`);
  return v;
};
const optPosInt = (v) => (Number.isInteger(v) && v > 0 && v <= 10_000 ? v : undefined);

/**
 * 注册 terminal:* 通道。返回 { disposeAll, _test }，sidecar 退出时 disposeAll。
 * spawn 可注入（测试用），默认走 node-pty。
 */
export function registerTerminalIpc({ settingsDir, platform = process.platform, spawn } = {}) {
  const prefsFile = path.join(settingsDir, 'terminal-prefs.json');
  const sessions = new Map();
  const spawnPty = spawn ?? ((cmd, args, opts) => loadNodePty().spawn(cmd, args, opts));

  const getDefaultShellPref = () => {
    let raw;
    try { raw = readJson(prefsFile)?.defaultShellPref; } catch { raw = undefined; }
    return SHELL_PREFS.includes(raw) ? raw : 'auto';
  };

  function spawnSession({ id, cwd, cols, rows, shellPref }) {
    const resolved = resolveShellForCreate(shellPref, platform);
    let dir = cwd;
    try { if (!statSync(dir).isDirectory()) dir = null; } catch { dir = null; }
    if (!dir) dir = process.env.HOME || process.env.USERPROFILE || process.cwd();
    const pty = spawnPty(resolved.command, resolved.args, {
      name: TERMINAL_NAME,
      cols,
      rows,
      cwd: dir,
      env: buildTerminalEnv(process.env, platform),
      encoding: 'utf8',
      // Windows 11 一律 ConPTY（随包不带 winpty）。
      ...(platform === 'win32' ? { useConpty: true } : {}),
    });
    const s = { id, pty, resolved, cwd, cols, rows, shellPref, exit: null, pending: '', flushScheduled: false };
    s.dataSub = pty.onData((chunk) => emitToRenderer('terminal:data', { id, chunk }));
    s.exitSub = pty.onExit(({ exitCode, signal }) => {
      s.exit = { code: typeof exitCode === 'number' ? exitCode : null, signal: signal ? String(signal) : null };
      s.pending = '';
      emitToRenderer('terminal:exit', { id, exit: s.exit });
    });
    return s;
  }

  const meta = (s) => ({ shellId: s.resolved.id, shellDisplayName: s.resolved.displayName, pid: s.pty.pid });

  function create(params) {
    if (!params || typeof params !== 'object') fail('INVALID_PARAMS', 'create params must be an object');
    const id = reqString(params.id, 'id');
    const cwd = reqString(params.cwd, 'cwd');
    let shellPref = params.shellPref;
    if (shellPref !== undefined && shellPref !== null && !SHELL_PREFS.includes(shellPref)) fail('INVALID_PARAMS', `invalid shell pref: ${String(shellPref)}`);
    if (shellPref === undefined) shellPref = getDefaultShellPref();
    const existing = sessions.get(id);
    if (existing) {
      // desktop 的幂等 attach：切回同一任务时 renderer 会再调 create。
      if (existing.exit) fail('TERMINAL_SPAWN_FAILED', `terminal session already exists (exited): ${id}`);
      return meta(existing);
    }
    if (sessions.size >= MAX_SESSIONS) fail('RESOURCE_EXHAUSTED', `too many terminal sessions (max ${MAX_SESSIONS})`);
    try {
      const s = spawnSession({ id, cwd, cols: optPosInt(params.cols) ?? DEFAULT_COLS, rows: optPosInt(params.rows) ?? DEFAULT_ROWS, shellPref });
      sessions.set(id, s);
      return meta(s);
    } catch (err) {
      if (err?.code === 'UNSUPPORTED_CAPABILITY') throw err;
      const msg = String(err?.message ?? err);
      if (/ENOENT|not found|no such file/i.test(msg)) fail('TERMINAL_SHELL_NOT_FOUND', `shell binary unavailable: ${msg}`);
      fail('TERMINAL_SPAWN_FAILED', `failed to spawn pty: ${msg}`);
    }
  }

  function dispose(id) {
    const s = sessions.get(id);
    if (!s) return;
    sessions.delete(id);
    try { s.dataSub?.dispose(); s.exitSub?.dispose(); } catch { /* 已解绑 */ }
    if (!s.exit) {
      try { s.pty.kill(); } catch { /* 进程已退出 */ }
    }
  }

  registerInvoke('terminal:create', async (params) => create(params));
  registerInvoke('terminal:write', async (idArg, dataArg) => {
    const id = reqString(idArg, 'id');
    if (typeof dataArg !== 'string' || dataArg.length === 0) fail('INVALID_PARAMS', 'data must be a non-empty string');
    if (dataArg.length > MAX_WRITE_CHARS) fail('INVALID_PARAMS', 'data is too large');
    const s = sessions.get(id);
    if (!s) fail('TERMINAL_NOT_FOUND', `pty session not found: ${id}`);
    if (s.exit) return;
    // desktop：按键逐个到达，微任务内合并成一次写入。
    s.pending += dataArg;
    if (s.flushScheduled) return;
    s.flushScheduled = true;
    queueMicrotask(() => {
      s.flushScheduled = false;
      if (s.exit || !s.pending) return;
      const chunk = s.pending;
      s.pending = '';
      try { s.pty.write(chunk); } catch { /* 进程刚退出 */ }
    });
  });
  registerInvoke('terminal:resize', async (idArg, colsArg, rowsArg) => {
    const id = reqString(idArg, 'id');
    const cols = reqPosInt(colsArg, 'cols');
    const rows = reqPosInt(rowsArg, 'rows');
    const s = sessions.get(id);
    if (!s) fail('TERMINAL_NOT_FOUND', `pty session not found: ${id}`);
    if (s.exit || (s.cols === cols && s.rows === rows)) return;
    try { s.pty.resize(cols, rows); s.cols = cols; s.rows = rows; } catch { /* 进程刚退出 */ }
  });
  registerInvoke('terminal:dispose', async (idArg) => { dispose(reqString(idArg, 'id')); });
  registerInvoke('terminal:restart', async (idArg) => {
    const id = reqString(idArg, 'id');
    const old = sessions.get(id);
    if (!old) fail('TERMINAL_NOT_FOUND', `pty session not found: ${id}`);
    if (!old.exit) fail('PRECONDITION_FAILED', 'cannot restart a session that has not exited');
    dispose(id);
    return create({ id, cwd: old.cwd, cols: old.cols, rows: old.rows, shellPref: old.shellPref });
  });
  registerInvoke('terminal:list-available-shells', async () => probeAvailableShells(platform));
  registerInvoke('terminal:get-default-shell-pref', async () => getDefaultShellPref());
  registerInvoke('terminal:set-default-shell-pref', async (value) => {
    if (typeof value !== 'string' || !SHELL_PREFS.includes(value)) fail('INVALID_PARAMS', `invalid shell pref: ${String(value)}`);
    writeJsonAtomic(prefsFile, { defaultShellPref: value });
  });

  return {
    disposeAll: () => { for (const id of [...sessions.keys()]) dispose(id); },
    _test: { sessions },
  };
}
