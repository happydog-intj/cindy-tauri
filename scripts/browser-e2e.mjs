// sidecar 级浏览器面板测试（不起 Tauri）：右侧栏页签持久化（local-db:right-sidebar-tabs:*）、
// rsb-browser-bridge / tauri-browser:popup 的弹窗归属、browser-backend 设置与降级形状、rsb-native-popup。
// 用法：node scripts/browser-e2e.mjs   （需要先 npm run sidecar:build，headless/ 已打包）
import { spawn } from 'node:child_process';
import { mkdtempSync } from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import readline from 'node:readline';
import { fileURLToPath } from 'node:url';
import { decode, encode } from '../shared/ipc-codec.mjs';

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const sidecarJs = path.join(root, 'sidecar', 'dist', 'sidecar.cjs');
const dataDir = mkdtempSync(path.join(os.tmpdir(), 'cindy-browser-e2e-'));

function startSidecar() {
  const child = spawn(process.execPath, [sidecarJs], {
    cwd: path.dirname(sidecarJs),
    env: { ...process.env, CINDY_TAURI_DATA_DIR: dataDir },
    stdio: ['pipe', 'pipe', 'pipe'],
  });
  const errLines = [];
  const pushes = [];
  const natives = [];
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
      natives.push(msg.native);
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
  return { call, invoke, stop, errLines, pushes, natives };
}

const steps = [];
function check(name, cond, detail) {
  steps.push({ name, ok: !!cond });
  console.log(`${cond ? 'PASS' : 'FAIL'} ${name}${detail !== undefined ? ` — ${typeof detail === 'string' ? detail : JSON.stringify(detail).slice(0, 300)}` : ''}`);
  if (!cond) throw new Error(`step failed: ${name}`);
}
const rejects = async (p) => { try { await p; return null; } catch (e) { return e.message; } };
const popups = (sc) => sc.pushes.filter((p) => p.channel === 'rsb:browser-popup').map((p) => p.args[0]);

let failed = false;
let sc = startSidecar();
try {
  await sc.call('ping', {});
  const T = 'local-db:right-sidebar-tabs';

  // ---------- 页签持久化 ----------
  const draft = await sc.invoke(`${T}:list`, { sessionId: 'not-a-session' });
  check('draft session not persistable', draft.persistable === false && draft.tabs.length === 0, draft);
  const row = await sc.invoke('local-db:sessions:create', { agentKind: 'pi', title: 'Browser E2E', workspaceKind: 'dialogue', model: 'm', permissionMode: 'ask' });
  const sid = row.id;
  const empty = await sc.invoke(`${T}:list`, { sessionId: sid });
  check('new session persistable', empty.persistable === true && empty.tabs.length === 0 && empty.activeTabId === null, empty);

  const browserState = { url: 'https://example.com/', title: 'Example' };
  await sc.invoke(`${T}:upsert`, { id: 'tab-a', sessionId: sid, kind: 'web-browser', position: 0, state: browserState });
  await sc.invoke(`${T}:upsert`, { id: 'tab-b', sessionId: sid, kind: 'web-browser', position: 1, state: { url: 'https://b.test/' } });
  await sc.invoke(`${T}:setActive`, { sessionId: sid, id: 'tab-b' });
  let list = await sc.invoke(`${T}:list`, { sessionId: sid });
  check('upsert + setActive', list.tabs.length === 2 && list.activeTabId === 'tab-b' && list.tabs[0].state.url === browserState.url, list);
  await sc.invoke(`${T}:upsert`, { id: 'tab-a', sessionId: sid, kind: 'web-browser', position: 0, state: { url: 'https://example.org/' } });
  list = await sc.invoke(`${T}:list`, { sessionId: sid });
  check('upsert updates state', list.tabs.find((t) => t.id === 'tab-a').state.url === 'https://example.org/' && list.tabs.length === 2);
  await sc.invoke(`${T}:reorder`, { sessionId: sid, orderedIds: ['tab-b', 'tab-a'] });
  list = await sc.invoke(`${T}:list`, { sessionId: sid });
  check('reorder', list.tabs.map((t) => t.id).join() === 'tab-b,tab-a', list.tabs.map((t) => t.id));

  const s1 = await sc.invoke(`${T}:ensure-singleton`, { sessionId: sid, kind: 'subagents', state: {} });
  const s2 = await sc.invoke(`${T}:ensure-singleton`, { sessionId: sid, kind: 'subagents', state: {} });
  check('ensure-singleton idempotent', s1.created === true && s2.created === false && s1.tab.id === s2.tab.id && /^t_[0-9a-f]{32}$/.test(s1.tab.id), { s1: s1.tab.id, s2: s2.tab.id });
  check('ensure-singleton rejects other kinds', /INVALID_PARAMS/.test(await rejects(sc.invoke(`${T}:ensure-singleton`, { sessionId: sid, kind: 'web-browser' })) ?? ''));
  check('state size limit', /RIGHT_SIDEBAR_STATE_TOO_LARGE/.test(await rejects(sc.invoke(`${T}:upsert`, { id: 'big', sessionId: sid, kind: 'web-browser', position: 3, state: { s: 'x'.repeat(17000) } })) ?? ''));
  for (let i = 0; i < 17; i++) {
    await sc.invoke(`${T}:upsert`, { id: `fill-${i}`, sessionId: sid, kind: 'web-browser', position: 3 + i, state: {} });
  }
  check('tab count limit', /RIGHT_SIDEBAR_TOO_MANY_TABS/.test(await rejects(sc.invoke(`${T}:upsert`, { id: 'one-more', sessionId: sid, kind: 'web-browser', position: 30, state: {} })) ?? ''));
  check('update allowed at limit', (await sc.invoke(`${T}:upsert`, { id: 'fill-0', sessionId: sid, kind: 'web-browser', position: 3, state: { url: 'https://c.test/' } }))?.ok === true);
  for (let i = 0; i < 17; i++) await sc.invoke(`${T}:close`, { id: `fill-${i}` });
  check('close missing tab', /NOT_FOUND/.test(await rejects(sc.invoke(`${T}:close`, { id: 'fill-0' })) ?? ''));
  check('setActive unknown tab', /NOT_FOUND/.test(await rejects(sc.invoke(`${T}:setActive`, { sessionId: sid, id: 'nope' })) ?? ''));
  check('upsert unknown session fails', !!(await rejects(sc.invoke(`${T}:upsert`, { id: 'orphan', sessionId: 'missing', kind: 'web-browser', position: 0, state: {} }))));

  // ---------- 弹窗归属 ----------
  check('report validates', /INVALID_PARAMS/.test(await rejects(sc.invoke('rsb-browser-bridge:report', { sessionId: sid, tabId: 'tab-a', webContentsId: -1 })) ?? ''));
  check('report', (await sc.invoke('rsb-browser-bridge:report', { sessionId: sid, tabId: 'tab-a', webContentsId: 41 }))?.ok === true);
  await sc.invoke('tauri-browser:popup', { webContentsId: 41, url: 'https://popup.test/a', disposition: 'foreground-tab' });
  let p = popups(sc).at(-1);
  check('popup carries opener', p?.url === 'https://popup.test/a' && p.openerTabId === 'tab-a' && p.openerSessionId === sid && p.disposition === 'foreground-tab', p);
  const nonHttp = await sc.invoke('tauri-browser:popup', { webContentsId: 41, url: 'file:///etc/passwd' });
  check('non-http popup dropped', nonHttp.ok === false && popups(sc).length === 1);
  // 弹窗先到、report 后到：等到 report 再路由。
  const late = sc.invoke('tauri-browser:popup', { webContentsId: 42, url: 'https://popup.test/late' });
  await new Promise((r) => setTimeout(r, 200));
  check('popup waits for report', popups(sc).length === 1);
  await sc.invoke('rsb-browser-bridge:report', { sessionId: sid, tabId: 'tab-b', webContentsId: 42 });
  await late;
  p = popups(sc).at(-1);
  check('late popup attributed', p?.url === 'https://popup.test/late' && p.openerTabId === 'tab-b', p);
  // stale release 不删新记录
  await sc.invoke('rsb-browser-bridge:release', { tabId: 'tab-b', webContentsId: 7 });
  await sc.invoke('tauri-browser:popup', { webContentsId: 42, url: 'https://popup.test/still' });
  check('stale release ignored', popups(sc).at(-1)?.openerTabId === 'tab-b');
  const snap = await sc.invoke('rsb-browser-bridge:snapshot', { liveTabIds: ['tab-a'] });
  check('snapshot drops dead tabs', snap.ok === true && snap.dropped.join() === 'tab-b' && snap.kept === 1 && Array.isArray(snap.pinnedTabIds), snap);
  check('snapshot validates', /INVALID_PARAMS/.test(await rejects(sc.invoke('rsb-browser-bridge:snapshot', { liveTabIds: [''] })) ?? ''));
  for (const [ch, arg] of [['set-active-session', { sessionId: sid }], ['set-foreground', { tabId: 'tab-a' }], ['set-foreground', { tabId: null }], ['tab-op-result', {}], ['release', { tabId: 'tab-a' }]]) {
    check(`bridge ${ch}`, (await sc.invoke(`rsb-browser-bridge:${ch}`, arg))?.ok === true);
  }
  check('set-foreground validates', /INVALID_PARAMS/.test(await rejects(sc.invoke('rsb-browser-bridge:set-foreground', { tabId: 3 })) ?? ''));
  check('screenshot degraded', /UNSUPPORTED_CAPABILITY/.test(await rejects(sc.invoke('rsb-browser-bridge:capture-screenshot', { tabId: 'tab-a' })) ?? ''));

  // ---------- 原生弹窗面 ----------
  check('native popup claim', (await sc.invoke('rsb-native-popup:claim', { surfaceId: 's', sessionId: sid, tabId: 'x' }))?.alive === false);
  check('native popup close', (await sc.invoke('rsb-native-popup:close', { surfaceId: 's' }))?.ok === true);

  // ---------- browser-backend ----------
  let st = await sc.invoke('browser-backend:get-state');
  check('backend default state', st.active === 'external' && st.systemDefault === 'external' && st.isOverride === false && st.useRealProfile === false, st);
  check('backend set-kind', (await sc.invoke('browser-backend:set-kind', { kind: 'rsb-webview' })).active === 'rsb-webview');
  check('backend set-kind validates', /INVALID_PARAMS/.test(await rejects(sc.invoke('browser-backend:set-kind', { kind: 'chrome' })) ?? ''));
  const health = await sc.invoke('browser-backend:get-health');
  check('backend health shape', health.active === 'rsb-webview' && health.status === 'error' && typeof health.canRecover === 'boolean' && health.reason === 'host-unavailable', health);
  check('backend recover shape', (await sc.invoke('browser-backend:recover')).ok === false);
  check('backend probe', (await sc.invoke('browser-backend:probe-source-read')).readable === false);
  check('real profile refused', /NO_CHROMIUM/.test(await rejects(sc.invoke('browser-backend:set-use-real-profile', { enabled: true })) ?? ''));
  check('real profile off', (await sc.invoke('browser-backend:set-use-real-profile', { enabled: false })).enabled === false);
  const store = await sc.invoke('browser-backend:open-extension-store');
  const opened = sc.natives.find((n) => n.method === 'open.url');
  check('extension store opens via native', store.ok === true && /^https:\/\/chromewebstore\.google\.com\//.test(opened?.params?.url ?? ''), opened);

  // ---------- 重启后 ----------
  await sc.stop();
  sc = startSidecar();
  await sc.call('ping', {});
  list = await sc.invoke(`${T}:list`, { sessionId: sid });
  check('tabs survive restart', list.tabs.map((t) => t.id).join() === `tab-b,tab-a,${s1.tab.id}` && list.activeTabId === 'tab-b', list.tabs.map((t) => t.id));
  st = await sc.invoke('browser-backend:get-state');
  check('backend kind survives restart', st.active === 'rsb-webview' && st.isOverride === true, st);
  check('backend reset', (await sc.invoke('browser-backend:reset')).active === 'external');
} catch (err) {
  failed = true;
  console.error(err);
  if (sc.errLines.length) console.error(sc.errLines.slice(-20).join('\n'));
} finally {
  await sc.stop();
}
console.log(JSON.stringify({ ok: !failed, dataDir, steps: steps.length }));
process.exit(failed ? 1 : 0);
