// sidecar 级设置页通道测试（不起 Tauri）：每个读取类通道都能返回、不报 NOT_IMPLEMENTED；
// 一组代表性的写入能校验参数、持久化到 <dataDir>/settings/，重启 sidecar 后还在，reset 能恢复默认。
// 用法：node scripts/settings-e2e.mjs   （需要先 npm run sidecar:build，headless/ 已打包）
import { spawn } from 'node:child_process';
import { randomBytes } from 'node:crypto';
import { existsSync, mkdtempSync, readdirSync } from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import readline from 'node:readline';
import { fileURLToPath } from 'node:url';
import { decode, encode } from '../shared/ipc-codec.mjs';

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const sidecarJs = path.join(root, 'sidecar', 'dist', 'sidecar.cjs');
const dataDir = mkdtempSync(path.join(os.tmpdir(), 'cindy-settings-e2e-'));
const OWNER = { dataOwnerId: 'local-v1', ownerGeneration: 1 };

function startSidecar() {
  const child = spawn(process.execPath, [sidecarJs], {
    cwd: path.dirname(sidecarJs),
    env: { ...process.env, CINDY_TAURI_DATA_DIR: dataDir },
    stdio: ['pipe', 'pipe', 'pipe'],
  });
  const errLines = [];
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
    if (msg.native && msg.native.id !== undefined) {
      child.stdin.write(JSON.stringify({ nativeReply: { id: msg.native.id, result: null } }) + '\n');
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
  const sendSync = async (channel, ...a) => decode(await call('ipc.sendSync', { channel, payload: encode(a) }));
  const stop = () => new Promise((resolve) => { child.once('exit', resolve); child.stdin.end(); });
  return { call, invoke, sendSync, stop, errLines };
}

const steps = [];
function check(name, cond, detail) {
  steps.push({ name, ok: !!cond });
  console.log(`${cond ? 'PASS' : 'FAIL'} ${name}${detail !== undefined ? ` — ${typeof detail === 'string' ? detail : JSON.stringify(detail).slice(0, 300)}` : ''}`);
  if (!cond) throw new Error(`step failed: ${name}`);
}
const rejects = async (p) => { try { await p; return null; } catch (e) { return e.message; } };

// 设置页各分区首屏会读的通道。
const READS = [
  'appearance-settings:get', 'local-themes:list', 'menubar:get-state', 'login-item:get', 'device-link:get-state',
  'update-channel-settings-get', 'update-auto-settings-get', 'terminal:list-available-shells',
  'terminal:get-default-shell-pref', 'worklouder-codex:get-state', 'codex-micro-guard:get-state', 'xbox-gamepad:get-state',
  'maker:collaboration-settings:get', 'maker:agent-resource-settings:get', 'maker:git-safety:get', 'maker:lsp-mode:get',
  'maker:silent-encrypted-retry:get', 'maker:session-runtime-fallback:get', 'maker:chat-embedding:get',
  'maker:subagent-model-settings:get', 'maker:vision-bridge-settings:get', 'maker:memory:get-settings',
  'maker:memory:get-settings-state', 'maker:compaction:get-state', 'maker:pi-compaction:get-state', 'maker:pi-packages:list',
  'maker:usage:history', 'local-db:bots:model-chain-settings-get', 'maker:auxiliary-model-settings:get',
  'window-behavior:get-windows-close-behavior',
];
const SYNC_READS = ['appearance-settings:get-sync', 'local-themes:list-sync', 'app-shortcuts:get', 'voice-input:data:get',
  'voice-input:history:get', 'voice-input:get-system-permissions-cached', 'voice-input:get-readiness-cached'];

let failed = false;
let sc = startSidecar();
try {
  await sc.call('ping', {});
  await sc.call('secrets.unlock', { key: randomBytes(32).toString('hex') });

  for (const ch of READS) {
    const err = await rejects(sc.invoke(ch));
    check(`read ${ch}`, err === null, err ?? undefined);
  }
  // desktop 本机模式（无 Cindy 账号）同样拒绝。
  check('device list needs account', (await rejects(sc.invoke('device-link:list-devices')))?.includes('PRECONDITION_FAILED'));
  for (const ch of SYNC_READS) {
    const v = await sc.sendSync(ch);
    check(`sync read ${ch}`, v !== undefined && v !== null, typeof v);
  }
  const kernel = await sc.invoke('maker:agent:pi-kernel-state', false);
  check('pi kernel state', kernel && 'currentVersion' in kernel && kernel.restartRequired === false, kernel);

  // 写入：参数校验 + 持久化
  const appearance = await sc.invoke('appearance-settings:get');
  check('appearance has defaults', typeof appearance === 'object' && appearance !== null, Object.keys(appearance ?? {}));
  const collab = await sc.invoke('maker:collaboration-settings:set', { key: 'workerSoftLimit', value: 3 });
  check('collaboration set', collab.workerSoftLimit === 3 && collab.isCustomized === true, collab);
  check('collaboration soft > hard rejected', (await rejects(sc.invoke('maker:collaboration-settings:set', { key: 'workerSoftLimit', value: 15 })))?.includes('INVALID_PARAMS'));
  check('collaboration rejects junk', (await rejects(sc.invoke('maker:collaboration-settings:set', 'nope')))?.includes('INVALID_PARAMS'));
  const ap = await sc.invoke('appearance-settings:set-patch', { uiSize: 15 });
  check('appearance patch', ap.uiSize === 15, ap);
  check('appearance unknown field rejected', (await rejects(sc.invoke('appearance-settings:set-patch', { uiFontSize: 15 })))?.includes('INVALID_PARAMS'));
  const pct = await sc.invoke('maker:compaction:set-pct', 70, OWNER);
  check('compaction set', pct.pct === 70, pct);
  check('compaction needs owner stamp', (await rejects(sc.invoke('maker:compaction:set-pct', 70)))?.includes('INVALID_PARAMS'));
  check('compaction stale owner rejected', (await rejects(sc.invoke('maker:compaction:set-pct', 70, { dataOwnerId: 'x', ownerGeneration: 9 })))?.includes('PRECONDITION_FAILED'));
  const retry = await sc.invoke('maker:silent-encrypted-retry:set', false);
  check('toggle set', retry.enabled === false && retry.isCustomized === true, retry);
  check('toggle rejects non-boolean', (await rejects(sc.invoke('maker:silent-encrypted-retry:set', 'yes')))?.includes('INVALID_PARAMS'));
  await sc.invoke('window-behavior:set-windows-close-behavior', 'quit');
  // 语音输入：词典增删改、历史记录（sync）
  const added = await sc.invoke('voice-input:dictionary:add-entry', 'Cindy Tauri');
  check('voice dictionary add', added.dictionaryEntries.some((e) => e.text === 'Cindy Tauri'), added.dictionaryEntries);
  const dupe = await sc.invoke('voice-input:dictionary:add-entry', 'cindy tauri');
  check('voice dictionary dedupe', dupe.dictionaryEntries.length === added.dictionaryEntries.length);
  const entryId = added.dictionaryEntries.find((e) => e.text === 'Cindy Tauri').id;
  const renamed = await sc.invoke('voice-input:dictionary:rename-entry', { entryId, text: 'Cindy' });
  check('voice dictionary rename', renamed.dictionaryEntries.some((e) => e.id === entryId && e.text === 'Cindy'));
  const imported = await sc.invoke('voice-input:dictionary:import-entries', ['alpha', 'beta', 'alpha']);
  check('voice dictionary import', imported.dictionaryEntries.length === 3, imported.dictionaryEntries.map((e) => e.text));
  const histId = await sc.sendSync('voice-input:history:record', '  你好 Cindy  ');
  check('voice history record', typeof histId === 'string', histId);
  check('voice history dedupe', (await sc.sendSync('voice-input:history:record', '你好 Cindy')) === histId);
  const settingsUpd = await sc.invoke('voice-input:settings:update', { composerLongPressEnabled: false });
  check('voice settings override', settingsUpd.composerLongPressEnabledOverride === false || settingsUpd.composerLongPressEnabled === false, settingsUpd.composerLongPressEnabled);
  const files = readdirSync(path.join(dataDir, 'settings'));
  check('settings persisted under settings/', files.includes('collaboration-settings.json') && files.includes('compaction-settings.json'), files);

  // 重启后仍在
  await sc.stop();
  sc = startSidecar();
  await sc.call('ping', {});
  check('collaboration survives restart', (await sc.invoke('maker:collaboration-settings:get')).workerSoftLimit === 3);
  check('compaction survives restart', (await sc.invoke('maker:compaction:get-state')).pct === 70);
  check('appearance survives restart', (await sc.sendSync('appearance-settings:get-sync')).uiSize === 15);
  check('toggle survives restart', (await sc.invoke('maker:silent-encrypted-retry:get')).enabled === false);
  const voice = await sc.sendSync('voice-input:data:get');
  check('voice data survives restart', voice.settings.dictionaryEntries.length === 3 && voice.history[0]?.text === '你好 Cindy', { n: voice.settings.dictionaryEntries.length, h: voice.history[0] });
  const delRes = await sc.invoke('voice-input:dictionary:delete-entries', [entryId]);
  check('voice dictionary delete', !delRes.dictionaryEntries.some((e) => e.id === entryId));
  await sc.sendSync('voice-input:history:delete', histId);
  check('voice history delete', (await sc.sendSync('voice-input:history:get')).length === 0);
  check('close behavior survives restart', (await sc.invoke('window-behavior:get-windows-close-behavior')) === 'quit');

  // reset 回默认
  const reset = await sc.invoke('maker:collaboration-settings:reset');
  check('collaboration reset', reset.workerSoftLimit === 5 && reset.isCustomized === false, reset);
  const toggleReset = await sc.invoke('maker:silent-encrypted-retry:reset');
  check('toggle reset', toggleReset.enabled === true && toggleReset.isCustomized === false, toggleReset);
  check('no stray data files', !existsSync(path.join(dataDir, 'settings', 'undefined.json')));
} catch (err) {
  failed = true;
  console.error(err);
  if (sc.errLines.length) console.error(sc.errLines.slice(-20).join('\n'));
} finally {
  await sc.stop();
}
console.log(JSON.stringify({ ok: !failed, dataDir, steps: steps.length }));
process.exit(failed ? 1 : 0);
