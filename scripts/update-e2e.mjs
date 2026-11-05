// sidecar 级自动更新测试（不起 Tauri）：本脚本扮演 Rust，回 update.check / download / install / idleSeconds，
// 校验 update.mjs 的状态推送、手动检查结果、下载进度、失败回退、superseding、空闲自动重启和重启前忙闲探测。
// 用法：node scripts/update-e2e.mjs   （需要先 npm run sidecar:build，headless/ 已打包）
import { spawn } from 'node:child_process';
import { randomBytes } from 'node:crypto';
import { mkdtempSync } from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import readline from 'node:readline';
import { fileURLToPath } from 'node:url';
import { decode, encode } from '../shared/ipc-codec.mjs';

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const sidecarJs = path.join(root, 'sidecar', 'dist', 'sidecar.cjs');
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

// rust：method → async (params, sc) => result；抛错即回 error。
function startSidecar({ version = '0.0.1', mode = 'manual', env = {}, rust = {} } = {}) {
  const dataDir = mkdtempSync(path.join(os.tmpdir(), 'cindy-update-e2e-'));
  const child = spawn(process.execPath, [sidecarJs], {
    cwd: path.dirname(sidecarJs),
    // CINDY_TAURI_SMOKE=1 才接受下面的计时覆盖；不设 SMOKE_OUT，Rust 侧的冒烟逻辑与本测试无关。
    env: { ...process.env, CINDY_TAURI_DATA_DIR: dataDir, CINDY_TAURI_APP_VERSION: version, CINDY_TAURI_UPDATES: mode, CINDY_TAURI_SMOKE: '1', ...env },
    stdio: ['pipe', 'pipe', 'pipe'],
  });
  const errLines = [];
  readline.createInterface({ input: child.stderr }).on('line', (l) => { errLines.push(l); if (process.env.E2E_VERBOSE) console.error('  [sc]', l); });
  let seq = 0;
  const waiters = new Map();
  const pushes = [];
  const natives = [];
  child.once('exit', (code) => {
    if (waiters.size === 0) return;
    console.error(`sidecar exited early (code ${code})\n${errLines.slice(-30).join('\n')}`);
    process.exit(1);
  });
  const sc = {
    pushes,
    natives,
    call: (method, params) => new Promise((resolve, reject) => {
      const id = ++seq;
      waiters.set(id, { resolve, reject });
      child.stdin.write(JSON.stringify({ id, method, params }) + '\n');
    }),
    invoke: async (channel, ...a) => decode(await sc.call('ipc.invoke', { channel, payload: encode(a) })),
    send: (channel, ...a) => sc.call('ipc.send', { channel, payload: encode(a) }),
    statuses: () => pushes.filter((p) => p.channel === 'update-status').map((p) => p.args[0]),
    nativeCount: (m) => natives.filter((n) => n.method === m).length,
    stop: () => { child.stdin.end(); return new Promise((r) => child.once('exit', r)); },
  };
  readline.createInterface({ input: child.stdout }).on('line', async (line) => {
    const msg = JSON.parse(line);
    if (msg.push) { pushes.push({ channel: msg.push.channel, args: decode(msg.push.payload) }); return; }
    if (msg.native) {
      natives.push(msg.native);
      const { id, method, params } = msg.native;
      if (id === undefined) return;
      const fn = rust[method];
      let reply;
      try {
        reply = fn ? { id, result: (await fn(params, sc)) ?? null } : { id, error: `[NOT_IMPLEMENTED] ${method}` };
      } catch (e) {
        reply = { id, error: String(e.message ?? e) };
      }
      child.stdin.write(JSON.stringify({ nativeReply: reply }) + '\n');
      return;
    }
    const w = waiters.get(msg.id);
    if (!w) return;
    waiters.delete(msg.id);
    if (msg.error) w.reject(new Error(msg.error)); else w.resolve(msg.result);
  });
  return sc;
}

const steps = [];
function check(name, cond, detail) {
  steps.push({ name, ok: !!cond });
  console.log(`${cond ? 'PASS' : 'FAIL'} ${name}${detail !== undefined ? ` — ${typeof detail === 'string' ? detail : JSON.stringify(detail).slice(0, 300)}` : ''}`);
  if (!cond) throw new Error(`step failed: ${name}`);
}
async function waitUntil(pred, ms = 5000) {
  const end = Date.now() + ms;
  while (!pred()) {
    if (Date.now() > end) return false;
    await sleep(25);
  }
  return true;
}

// Rust 侧可调的「发布」：available 时给版本和说明；download 先推进度再完成。
function fakeRelease() {
  const state = { latest: null, checkError: null, downloadError: null, installError: null, idle: 0 };
  const rust = {
    'update.check': async () => {
      if (state.checkError) throw new Error(state.checkError);
      return state.latest ? { available: true, version: state.latest, body: `notes for ${state.latest}`, date: '2026-11-05 00:00:00 +00:00' } : { available: false };
    },
    'update.download': async (params, sc) => {
      if (params.version !== state.latest) throw new Error(`unexpected version ${params.version}`);
      for (const received of [0, 500, 1000]) {
        await sc.call('native.updateProgress', { received, total: 1000 });
      }
      if (state.downloadError) throw new Error(state.downloadError);
      return { version: params.version, size: 1000 };
    },
    'update.install': async () => {
      if (state.installError) throw new Error(state.installError);
      // 真实 Rust 安装成功后会结束进程，不会回复；测试里回 null。
      return null;
    },
    'update.idleSeconds': async () => state.idle,
  };
  return { state, rust };
}

let failed = false;
try {
  // ── 1. 占位版本 0.0.0：完全不检查 ──
  {
    const sc = startSidecar({ version: '0.0.0' });
    await sc.call('secrets.unlock', { key: randomBytes(32).toString('hex') });
    const r = await sc.invoke('update-check-now');
    check('versionless build reports idle', r?.result === 'idle', r);
    check('versionless build never calls update.check', sc.nativeCount('update.check') === 0);
    await sc.stop();
  }

  // ── 2. 手动检查：无更新 / 检查失败 / 下载失败 / 下载成功 / 安装失败 ──
  {
    const { state, rust } = fakeRelease();
    const sc = startSidecar({ rust });
    await sc.call('secrets.unlock', { key: randomBytes(32).toString('hex') });
    check('initial status idle', (await sc.invoke('update-get-status'))?.status === 'idle');

    let r = await sc.invoke('update-check-now');
    check('no update → idle', r?.result === 'idle', r);
    check('pushed checking then idle', sc.statuses().map((s) => s.status).join(',') === 'checking,idle', sc.statuses());

    state.checkError = 'network down';
    r = await sc.invoke('update-check-now');
    check('check failure → manifest_failed', r?.result === 'manifest_failed', r);
    check('status back to idle after check failure', (await sc.invoke('update-get-status'))?.status === 'idle');
    state.checkError = null;

    state.latest = '0.0.2';
    state.downloadError = 'signature mismatch';
    const before = sc.pushes.length;
    r = await sc.invoke('update-check-now');
    check('download failure → download_failed', r?.result === 'download_failed', r);
    const failPush = sc.pushes.slice(before).find((p) => p.channel === 'app-update-progress' && p.args[0]?.failed);
    check('download failure broadcasts failed progress', !!failPush, failPush);
    check('status back to idle after download failure', (await sc.invoke('update-get-status'))?.status === 'idle');
    state.downloadError = null;

    const mark = sc.statuses().length;
    r = await sc.invoke('update-check-now');
    check('download ok → ready', r?.result === 'ready', r);
    const seq = sc.statuses().slice(mark);
    check('downloading pushes carry version and progress', seq.some((s) => s.status === 'downloading' && s.version === '0.0.2' && s.progress === 50), seq);
    check('progress reaches 100', seq.some((s) => s.status === 'downloading' && s.progress === 100), seq);
    check('ready push carries version', seq.at(-1)?.status === 'ready' && seq.at(-1)?.version === '0.0.2', seq.at(-1));
    const st = await sc.invoke('update-get-status');
    check('get-status reports ready version', st?.status === 'ready' && st?.version === '0.0.2', st);

    const checks = sc.nativeCount('update.check');
    r = await sc.invoke('update-check-now');
    check('check while ready short-circuits', r?.result === 'ready' && sc.nativeCount('update.check') === checks, r);

    const notes = await sc.invoke('release-notes:fetch', '0.0.2');
    check('release notes from latest.json body', notes?.topics?.[0]?.text === 'notes for 0.0.2', notes);
    check('unknown version has no notes', (await sc.invoke('release-notes:fetch', '9.9.9')) === null);
    check('release notes index unsupported → null', (await sc.invoke('release-notes:fetch-index')) === null);

    const summary = await sc.invoke('update-relaunch:blocking-summary');
    check('idle app is not busy', summary?.busy === false && Array.isArray(summary?.categories), summary);
    check('blocking-activity false', (await sc.invoke('update-relaunch:blocking-activity', { silent: true })) === false);

    // 自动重启默认关闭：等两个轮询周期也不应安装。
    await sleep(500);
    check('auto relaunch off by default', sc.nativeCount('update.install') === 0);

    state.installError = 'ShellExecuteW failed';
    await sc.send('update-relaunch', 'dark');
    check('relaunch calls update.install', await waitUntil(() => sc.nativeCount('update.install') === 1));
    const installCall = sc.natives.find((n) => n.method === 'update.install');
    check('install targets ready version', installCall?.params?.version === '0.0.2', installCall?.params);
    check('install failure → updater_spawn_failed', await waitUntil(() => sc.statuses().at(-1)?.errorCode === 'updater_spawn_failed'), sc.statuses().at(-1));
    const auto = await sc.invoke('update-relaunch-auto', 'dark');
    check('startup auto relaunch refused when not ready', auto?.accepted === false, auto);
    await sc.stop();
  }

  // ── 3. 后台轮询：首查 → ready → 再查到更新版本 → superseding → ready；空闲时自动重启 ──
  {
    const { state, rust } = fakeRelease();
    state.latest = '0.0.2';
    state.idle = 5;
    const sc = startSidecar({
      mode: 'auto',
      rust,
      env: {
        CINDY_TAURI_UPDATE_FIRST_CHECK_MS: '100',
        CINDY_TAURI_UPDATE_POLL_MS: '400',
        CINDY_TAURI_UPDATE_AUTO_RELAUNCH_MS: '150',
        CINDY_TAURI_UPDATE_IDLE_SECONDS: '600',
      },
    });
    await sc.call('secrets.unlock', { key: randomBytes(32).toString('hex') });
    check('background check reaches ready', await waitUntil(() => sc.statuses().some((s) => s.status === 'ready' && s.version === '0.0.2')), sc.statuses());
    state.latest = '0.0.3';
    check('newer release → superseding keeps old version', await waitUntil(() => sc.statuses().some((s) => s.status === 'superseding' && s.version === '0.0.2')), sc.statuses().slice(-3));
    check('superseding ends ready on new version', await waitUntil(() => sc.statuses().at(-1)?.status === 'ready' && sc.statuses().at(-1)?.version === '0.0.3'), sc.statuses().at(-1));

    const set = await sc.invoke('update-auto-settings-set', { autoRelaunchOnIdle: true });
    check('enable auto relaunch on idle', set?.autoRelaunchOnIdle === true, set);
    await sleep(600);
    check('user active → no auto relaunch', sc.nativeCount('update.install') === 0 && sc.nativeCount('update.idleSeconds') > 0);
    state.idle = 3600;
    check('idle → auto relaunch installs', await waitUntil(() => sc.nativeCount('update.install') === 1, 3000));
    const installCall = sc.natives.find((n) => n.method === 'update.install');
    check('auto relaunch installs newest version', installCall?.params?.version === '0.0.3', installCall?.params);
    await sc.stop();
  }

  // ── 4. off 模式（开发包）：不检查，接口仍可用 ──
  {
    const { rust } = fakeRelease();
    const sc = startSidecar({ mode: 'off', rust });
    await sc.call('secrets.unlock', { key: randomBytes(32).toString('hex') });
    const r = await sc.invoke('update-check-now');
    check('off mode → idle without native calls', r?.result === 'idle' && sc.natives.filter((n) => n.method.startsWith('update.')).length === 0, r);
    await sc.stop();
  }
} catch (err) {
  failed = true;
  console.error(err.message);
}
const passed = steps.filter((s) => s.ok).length;
console.log(`\nupdate e2e: ${passed}/${steps.length} passed${failed ? ' (FAILED)' : ''}`);
process.exit(failed ? 1 : 0);
