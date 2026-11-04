// sidecar 级桌面外壳测试（不起 Tauri）：本脚本扮演 Rust，接 {native:...} 反向调用并回 nativeReply，
// 校验通知 / 角标 / 对话框 / 打开链接 / 关闭行为 / 托盘文案的 IPC 逻辑。
// 用法：node scripts/desktop-shell-e2e.mjs   （需要先 npm run sidecar:build）
import { spawn } from 'node:child_process';
import { randomBytes } from 'node:crypto';
import { mkdtempSync, readFileSync, writeFileSync } from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import readline from 'node:readline';
import { fileURLToPath } from 'node:url';
import { decode, encode } from '../shared/ipc-codec.mjs';

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const sidecarJs = path.join(root, 'sidecar', 'dist', 'sidecar.cjs');
const dataDir = mkdtempSync(path.join(os.tmpdir(), 'cindy-shell-e2e-'));
const platform = process.platform;

// 反向调用的预设回复：method → (params) => result | Error
const nativeReplies = {
  'dialog.pickFolder': () => dataDir,
  'dialog.pickFile': () => path.join(dataDir, 'page.html'),
  'dialog.message': () => 1,
  'open.url': () => null,
  'open.path': () => null,
  reveal: () => null,
};

const child = spawn(process.execPath, [sidecarJs], {
  cwd: path.dirname(sidecarJs),
  env: { ...process.env, CINDY_TAURI_DATA_DIR: dataDir },
  stdio: ['pipe', 'pipe', 'pipe'],
});
const errLines = [];
// sidecar 提前退出时不要挂在等回复上。
child.once('exit', (code) => {
  if (waiters.size === 0) return;
  console.error(`sidecar exited early (code ${code})\n${errLines.slice(-30).join('\n')}`);
  process.exit(1);
});
readline.createInterface({ input: child.stderr }).on('line', (l) => { errLines.push(l); if (process.env.E2E_VERBOSE) console.error('  [sc]', l); });
let seq = 0;
const waiters = new Map();
const pushes = [];
const natives = [];
readline.createInterface({ input: child.stdout }).on('line', (line) => {
  const msg = JSON.parse(line);
  if (msg.push) { pushes.push({ channel: msg.push.channel, args: decode(msg.push.payload) }); return; }
  if (msg.native) {
    natives.push(msg.native);
    const { id, method, params } = msg.native;
    if (id === undefined) return;
    const fn = nativeReplies[method];
    const reply = fn ? { id, result: fn(params) } : { id, error: `[NOT_IMPLEMENTED] ${method}` };
    child.stdin.write(JSON.stringify({ nativeReply: reply }) + '\n');
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
const send = (channel, ...a) => call('ipc.send', { channel, payload: encode(a) });
const rejects = async (p) => { try { await p; return null; } catch (e) { return e.message; } };
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const lastNative = (method) => natives.filter((n) => n.method === method).at(-1);

const steps = [];
function check(name, cond, detail) {
  steps.push({ name, ok: !!cond });
  console.log(`${cond ? 'PASS' : 'FAIL'} ${name}${detail !== undefined ? ` — ${typeof detail === 'string' ? detail : JSON.stringify(detail)}` : ''}`);
  if (!cond) throw new Error(`step failed: ${name}`);
}

let failed = false;
try {
  await call('ping', {});
  await call('secrets.unlock', { key: randomBytes(32).toString('hex') });

  // 托盘文案：启动即推，切到中文后重推。
  check('tray labels pushed at start', lastNative('tray.setLabels')?.params?.quit, lastNative('tray.setLabels')?.params);
  await invoke('app-menu:set-locale', 'zh-CN');
  await sleep(50);
  const zhLabels = lastNative('tray.setLabels')?.params;
  check('tray labels follow locale', zhLabels?.quit === '退出 Cindy', zhLabels);
  await invoke('app-menu:set-locale', 'xx-bogus');
  check('unsupported locale ignored', lastNative('tray.setLabels')?.params?.quit === '退出 Cindy');

  // 通知
  const before = natives.length;
  await invoke('notification:show-session-event', { sessionId: 'abcdef1234567890', title: '  ', kind: 'done' });
  await sleep(50);
  const toast = natives.slice(before).find((n) => n.method === 'notify');
  check('toast title falls back to session id', toast?.params?.title === 'Cindy · abcdef12', toast?.params);
  check('toast body localized', toast?.params?.body && toast.params.body !== 'settings.notifications.sessionEvent.done', toast?.params?.body);
  check('toast carries session id for click', toast?.params?.sessionId === 'abcdef1234567890');
  check('badge counts attention', lastNative('badge.set')?.params?.count === 1, lastNative('badge.set')?.params);
  check('invalid kind rejected', (await rejects(invoke('notification:show-session-event', { sessionId: 'x', title: '', kind: 'boom' })))?.includes('INVALID_PARAMS'));
  await invoke('notification:set-desktop-enabled', false);
  const n0 = natives.filter((n) => n.method === 'notify').length;
  await invoke('notification:show-session-event', { sessionId: 's2', title: 'T', kind: 'error' });
  await sleep(50);
  check('desktop disabled → no toast', natives.filter((n) => n.method === 'notify').length === n0);
  await invoke('notification:set-desktop-enabled', true);
  await call('native.notificationClicked', { sessionId: 's2' });
  check('notification click → deep-link:navigate', pushes.some((p) => p.channel === 'deep-link:navigate' && p.args[0]?.id === 's2'));
  await invoke('notification:clear-session-attention', 's2', 'explicit');
  check('attention cleared broadcast', pushes.some((p) => p.channel === 'notification:session-attention-cleared' && p.args[0]?.intent === 'explicit'));
  await invoke('notification:set-app-attention-count', { dataOwnerId: 'local-v1', ownerGeneration: 1, count: 7, sessionIds: ['a'] });
  await sleep(50);
  check('projected attention count → badge', lastNative('badge.set')?.params?.count === 7);
  check('attention snapshot needs owner stamp', (await rejects(invoke('notification:set-app-attention-count', { count: 1, sessionIds: [] })))?.includes('INVALID_PARAMS'));

  // 打开链接
  check('open https', (await invoke('shell:open-external', 'https://example.com/x')).success === true);
  check('open javascript: denied', (await invoke('shell:open-external', 'javascript:alert(1)')).success === false);
  check('open file: denied', (await invoke('shell:open-external', 'file:///etc/passwd')).success === false);
  check('billing mailto allowed', (await invoke('shell:open-external', 'mailto:xd-billing@xd.com?subject=hi')).success === true);
  check('other mailto denied', (await invoke('shell:open-external', 'mailto:a@b.com')).success === false);
  const html = path.join(dataDir, 'page.html');
  writeFileSync(html, '<p>hi</p>');
  check('open html in browser', (await invoke('shell:open-file-in-browser', html)).success === true);
  check('open txt in browser denied', (await invoke('shell:open-file-in-browser', path.join(dataDir, 'secrets.v1.json'))).errorCode === 'BROWSER_FILE_UNSUPPORTED_TYPE');
  if (platform !== 'win32') {
    check('system path denied', (await invoke('shell:open-file-in-browser', '/etc/x.html')).errorCode === 'BROWSER_FILE_PATH_NOT_ALLOWED');
  }
  check('show item in folder', (await invoke('shell:show-item-in-folder', { filePath: html })).success === true);
  check('show missing item', (await invoke('shell:show-item-in-folder', { filePath: path.join(dataDir, 'nope') })).success === false);
  check('open logs dir', (await invoke('app:open-logs-dir')).success === true && lastNative('open.path')?.params?.path === path.join(dataDir, 'logs'));

  // 对话框
  check('pick directory', (await invoke('dialog:show-open-directory', {})).path === dataDir);
  check('legacy pick directory', (await invoke('show-open-directory-dialog')).path === dataDir);
  check('pick resource → kind file', (await invoke('dialog:show-open-resource', {})).kind === 'file');
  check('relative defaultPath rejected', (await rejects(invoke('dialog:show-open-file', { defaultPath: 'rel' })))?.includes('INVALID_PARAMS'));
  const withFilter = await invoke('dialog:show-open-file', { filters: [{ name: 'HTML', extensions: ['html'] }] });
  check('pick file with filter', withFilter.path === html && lastNative('dialog.pickFile')?.params?.filters?.[0]?.extensions?.[0] === 'html');

  // 窗口关闭行为
  check('close behavior unset', (await invoke('window-behavior:get-windows-close-behavior')) === null);
  check('invalid close behavior rejected', (await rejects(invoke('window-behavior:set-windows-close-behavior', 'nope')))?.includes('INVALID_PARAMS'));
  await invoke('window-behavior:set-windows-close-behavior', 'tray');
  const stored = JSON.parse(readFileSync(path.join(dataDir, 'window-behavior-settings.json'), 'utf8'));
  check('close behavior persisted', stored.windowsCloseBehavior === 'tray', stored);
  const closeAction = await call('native.mainWindowCloseRequested', {});
  check('close request action', closeAction.action === (platform === 'win32' ? 'tray' : 'quit'), closeAction);
  // 托盘退出：没有任务在跑 → 直接退出，不弹确认。
  const msgCount = natives.filter((n) => n.method === 'dialog.message').length;
  check('tray quit when idle', (await call('native.trayQuit', {})).quit === true && natives.filter((n) => n.method === 'dialog.message').length === msgCount);
  check('native.* not reachable via ipc', (await rejects(invoke('native.trayQuit')))?.includes('NOT_IMPLEMENTED'));
  check('no unknown native methods', natives.every((n) => ['tray.setLabels', 'notify', 'badge.set', ...Object.keys(nativeReplies)].includes(n.method)), [...new Set(natives.map((n) => n.method))]);
} catch (err) {
  failed = true;
  console.error(err);
  if (errLines.length) console.error(errLines.slice(-20).join('\n'));
} finally {
  child.stdin.end();
}
console.log(JSON.stringify({ ok: !failed, dataDir, steps: steps.length }));
process.exit(failed ? 1 : 0);
