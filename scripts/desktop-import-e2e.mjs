// sidecar 级“从桌面版导入”测试（不起 Tauri，不碰本机真实数据）：
// 在临时目录里造一个 Cindy 桌面版 profile（与 desktop schema 同列的 SQLite 库、Pi 会话文件、dialogue 工作区、
// Electron safeStorage 格式的 .enc），通过 CINDY_TAURI_DESKTOP_PROFILE_DIRS 指给 sidecar，然后检查：
// 扫描结果、导入后的任务 / 消息 / 页签 / provider / API key、路径改写、只读（源文件哈希不变）、幂等、
// 钥匙串拒绝时只导入配置、导入的任务能用 mock 模型继续对话（resume 走复制过来的 Pi 会话文件）。
// 用法：node scripts/desktop-import-e2e.mjs   （需要先 npm run sidecar:build，headless/ 已打包）
import { spawn } from 'node:child_process';
import { createCipheriv, createHash, pbkdf2Sync, randomBytes, randomUUID } from 'node:crypto';
import { existsSync, mkdirSync, mkdtempSync, readdirSync, readFileSync, statSync, writeFileSync } from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import readline from 'node:readline';
import { DatabaseSync } from 'node:sqlite';
import { fileURLToPath } from 'node:url';
import { decode, encode } from '../shared/ipc-codec.mjs';

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const sidecarJs = path.join(root, 'sidecar', 'dist', 'sidecar.cjs');
const work = mkdtempSync(path.join(os.tmpdir(), 'cindy-import-e2e-'));
const profileDir = path.join(work, 'CindyGlobal');
const dataDir = process.env.CINDY_E2E_DATA_DIR || path.join(work, 'tauri-data');
const USER_ID = 'cmfixtureuser0000000000001';
const OWNER_KEY = createHash('sha256').update(USER_ID).digest('hex').slice(0, 20);
// 假的 safeStorage 口令 / key 与 API key，只存在于本进程和临时目录。
const MAC_PASSWORD = randomBytes(12).toString('base64');
const WIN_KEY = randomBytes(32);
const API_KEY = `sk-import-${randomBytes(10).toString('hex')}`;
const masterKey = randomBytes(32).toString('hex');

// ── 造桌面版 profile ──────────────────────────────────────────────
// 列定义摘自 desktop localDb schema（sessions 只取一部分列，额外放几列 Tauri 版没有的列验证会被丢弃）。
const DESKTOP_SCHEMA = `
CREATE TABLE sessions (
  id text PRIMARY KEY NOT NULL, title text DEFAULT 'New CCS' NOT NULL, working_dir text,
  model text DEFAULT 'claude-sonnet-4-6' NOT NULL, effort text DEFAULT 'high' NOT NULL,
  permission_mode text DEFAULT 'ask' NOT NULL, status text DEFAULT 'active' NOT NULL, sdk_session_id text,
  total_token_usage integer DEFAULT 0 NOT NULL, total_cost_usd real DEFAULT 0 NOT NULL,
  context_tokens integer DEFAULT 0 NOT NULL, context_window integer DEFAULT 0 NOT NULL,
  fast_mode integer DEFAULT false NOT NULL, cleared_at integer, pinned_at integer,
  created_at integer NOT NULL, updated_at integer NOT NULL, agent_kind text DEFAULT 'cc' NOT NULL,
  user_send_at integer, parent_session_id text, source text DEFAULT 'desktop' NOT NULL,
  extra_dirs text DEFAULT '[]' NOT NULL, orca_role text, workspace_kind text DEFAULT 'project' NOT NULL,
  remote_host_id text, summary text, provider_id text, plan_mode_enabled integer DEFAULT false NOT NULL,
  active_turn_started_at integer, active_turn_pid integer, last_turn_ended_at integer,
  writable_dirs text DEFAULT '[]' NOT NULL, agent_device_id text, hidden_at integer,
  last_activity_at integer, notifications_muted integer DEFAULT false NOT NULL, project_dir text
);
CREATE TABLE messages (
  id text PRIMARY KEY NOT NULL, client_id text NOT NULL, session_id text NOT NULL, role text NOT NULL,
  content text NOT NULL, tool_use_id text, created_at integer NOT NULL, agent_meta text, rewind_at integer, agent_kind text,
  FOREIGN KEY (session_id) REFERENCES sessions(id) ON DELETE cascade
);
CREATE TABLE right_sidebar_tabs (
  id text PRIMARY KEY NOT NULL, session_id text NOT NULL, kind text NOT NULL, position integer NOT NULL,
  is_active integer DEFAULT false NOT NULL, state text DEFAULT '{}' NOT NULL, created_at integer NOT NULL, updated_at integer NOT NULL
);
CREATE TABLE custom_providers (
  id text PRIMARY KEY NOT NULL, name text NOT NULL, runtimes text DEFAULT '{}' NOT NULL, sort_order integer DEFAULT 0 NOT NULL,
  created_at integer NOT NULL, updated_at integer NOT NULL, auth text
);
CREATE TABLE recent_workdirs (path text PRIMARY KEY NOT NULL, last_used_at integer NOT NULL);
CREATE TABLE project_aliases (project_key text PRIMARY KEY NOT NULL, alias text NOT NULL, updated_at integer NOT NULL);
`;

// Electron safeStorage（Chromium os_crypt v10）加密，与真实 Electron 42 输出格式一致。
function safeStorageEncrypt(plain) {
  if (process.platform === 'win32') {
    const nonce = randomBytes(12);
    const c = createCipheriv('aes-256-gcm', WIN_KEY, nonce);
    const body = Buffer.concat([c.update(plain, 'utf8'), c.final()]);
    return Buffer.concat([Buffer.from('v10'), nonce, body, c.getAuthTag()]).toString('base64');
  }
  const key = pbkdf2Sync(MAC_PASSWORD, 'saltysalt', 1003, 16, 'sha1');
  const c = createCipheriv('aes-128-cbc', key, Buffer.alloc(16, 0x20));
  return Buffer.concat([Buffer.from('v10'), c.update(plain, 'utf8'), c.final()]).toString('base64');
}

let mockPort = 0;
function buildProfile() {
  mkdirSync(profileDir, { recursive: true });
  const sql = new DatabaseSync(path.join(profileDir, `cindy-${USER_ID}.db`));
  sql.exec('PRAGMA journal_mode = WAL');
  sql.exec(DESKTOP_SCHEMA);
  const base = Date.parse('2026-10-01T08:00:00Z');
  const dialogues = path.join(profileDir, 'owners', OWNER_KEY, 'dialogues', '2026-10-01');
  const piSessions = path.join(profileDir, 'pi-agent-home', 'sessions');
  mkdirSync(piSessions, { recursive: true });
  const insS = sql.prepare(`INSERT INTO sessions (id, title, working_dir, model, permission_mode, status, sdk_session_id, created_at,
    updated_at, agent_kind, workspace_kind, provider_id, active_turn_started_at, remote_host_id, orca_role, last_activity_at, project_dir)
    VALUES (?, ?, ?, ?, 'ask', ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)`);
  const insM = sql.prepare('INSERT INTO messages (id, client_id, session_id, role, content, created_at, agent_kind) VALUES (?, ?, ?, ?, ?, ?, ?)');
  const ids = {};
  const mk = (name, { kind = 'pi', status = 'active', workspace = 'dialogue', remote = null, orca = null, project = null, bigFile = false, activeTurn = null } = {}) => {
    const id = randomUUID();
    ids[name] = id;
    const at = base + Object.keys(ids).length * 60_000;
    let wd = project;
    if (workspace === 'dialogue') {
      wd = path.join(dialogues, id);
      mkdirSync(wd, { recursive: true });
      writeFileSync(path.join(wd, 'notes.md'), `# ${name}\n`);
    }
    // Pi 会话文件：首行 header 的 cwd 指向桌面版工作区。
    const piId = randomUUID();
    const piFile = path.join(piSessions, `2026-10-01T08-00-00-000Z_${piId}.jsonl`);
    writeFileSync(piFile, [
      JSON.stringify({ type: 'session', version: 3, id: piId, timestamp: new Date(at).toISOString(), cwd: wd }),
      JSON.stringify({ type: 'model_change', id: 'm1', parentId: null, timestamp: new Date(at).toISOString(), provider: 'gw', modelId: 'mock-model' }),
      JSON.stringify({ type: 'message', id: 'u1', parentId: 'm1', timestamp: new Date(at).toISOString(), message: { role: 'user', content: [{ type: 'text', text: `hello ${name}` }], timestamp: at } }),
      JSON.stringify({ type: 'message', id: 'a1', parentId: 'u1', timestamp: new Date(at).toISOString(), message: { role: 'assistant', content: [{ type: 'text', text: `reply ${name}` }], api: 'openai-completions', provider: 'gw', model: 'mock-model', usage: { input: 1, output: 1, cacheRead: 0, cacheWrite: 0, totalTokens: 2, cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0, total: 0 } }, stopReason: 'stop', timestamp: at } }),
    ].join('\n') + '\n');
    insS.run(id, `Task ${name}`, wd, 'mock-model', status, piFile, at, at, kind, workspace, 'gw', activeTurn, remote, orca, at, project);
    insM.run(randomUUID(), `${name}-u1`, id, 'user', JSON.stringify({ text: `hello ${name}` }), at, kind);
    insM.run(randomUUID(), `${name}-a1`, id, 'assistant', JSON.stringify([{ type: 'text', text: `reply ${name}` }]), at + 1, kind);
    return id;
  };
  mk('dialogue', { activeTurn: base });
  mk('archived', { status: 'archived' });
  mk('project', { workspace: 'project', project: path.join(work, 'some-project') });
  mk('claude', { kind: 'cc' });
  mk('remote', { remote: 'host-1' });
  mk('team', { orca: 'lead' });
  sql.prepare(`INSERT INTO right_sidebar_tabs (id, session_id, kind, position, is_active, state, created_at, updated_at)
    VALUES (?, ?, 'browser', 0, 1, ?, ?, ?)`).run(randomUUID(), ids.dialogue, JSON.stringify({ url: 'https://example.com/' }), base, base);
  sql.prepare('INSERT INTO recent_workdirs (path, last_used_at) VALUES (?, ?)').run(path.join(work, 'some-project'), base);
  sql.prepare('INSERT INTO project_aliases (project_key, alias, updated_at) VALUES (?, ?, ?)').run(path.join(work, 'some-project'), 'Some Project', base);
  // provider：keyed（apiKey，有 .enc）、open（none）、nokey（apiKey，没有 .enc）、oauth（不支持）、cc-only（无 Pi runtime）。
  const insP = sql.prepare('INSERT INTO custom_providers (id, name, runtimes, sort_order, created_at, updated_at, auth) VALUES (?, ?, ?, ?, ?, ?, ?)');
  const pi = (suffix) => JSON.stringify({ pi: { baseUrl: `http://127.0.0.1:${mockPort}${suffix}`, wireProtocol: 'openai-chat', models: [{ id: 'mock-model', name: 'Mock', discoveredMetadata: { name: 'Mock' } }] } });
  insP.run('keyed', 'Keyed Gateway', pi('/keyed'), 0, base, base, JSON.stringify({ method: 'apiKey' }));
  insP.run('gw', 'Open Gateway', pi(''), 1, base, base, JSON.stringify({ method: 'none' }));
  insP.run('nokey', 'No Key', pi('/keyed'), 2, base, base, null);
  insP.run('oauthy', 'OAuth Thing', pi(''), 3, base, base, JSON.stringify({ method: 'oauth', oauth: { flow: 'device-code' } }));
  insP.run('cconly', 'CC Only', JSON.stringify({ 'claude-code': { baseUrl: 'https://example.com' } }), 4, base, base, null);
  sql.close();
  const ss = path.join(profileDir, 'safe-storage');
  mkdirSync(ss, { recursive: true });
  writeFileSync(path.join(ss, `owner_${OWNER_KEY}_provider_key_keyed_pi.enc`), safeStorageEncrypt(API_KEY));
  writeFileSync(path.join(profileDir, 'Local State'), JSON.stringify({
    os_crypt: { encrypted_key: Buffer.concat([Buffer.from('DPAPI'), Buffer.from('fake-dpapi-blob')]).toString('base64') },
  }));
  return ids;
}

// skip：桌面版运行中（有 WAL）时，SQLite 读者会更新 -shm 里的读标记，这是 SQLite 自身协调，不算改动。
function hashTree(dir, skip = null) {
  const h = createHash('sha256');
  const walk = (d) => {
    for (const name of readdirSync(d).sort()) {
      if (skip && skip.test(name)) continue;
      const p = path.join(d, name);
      const st = statSync(p);
      if (st.isDirectory()) walk(p);
      else { h.update(path.relative(dir, p)); h.update(readFileSync(p)); }
    }
  };
  walk(dir);
  return h.digest('hex');
}

// 数据目录里任何文件都不能出现明文 key（跳过 Pi 安装目录）。
function findPlaintext(dir, needle, hits = []) {
  for (const name of readdirSync(dir)) {
    const p = path.join(dir, name);
    const st = statSync(p);
    if (st.isDirectory()) { if (name !== 'agents') findPlaintext(p, needle, hits); continue; }
    if (st.size > 20 * 1024 * 1024) continue;
    if (readFileSync(p).includes(needle)) hits.push(path.relative(dir, p));
  }
  return hits;
}

// ── mock 模型 + sidecar ───────────────────────────────────────────
function startMock() {
  const child = spawn(process.execPath, [path.join(root, 'scripts', 'mock-openai.mjs')], {
    stdio: ['ignore', 'pipe', 'pipe'], env: { ...process.env, MOCK_API_KEY: API_KEY },
  });
  const logLines = [];
  readline.createInterface({ input: child.stderr }).on('line', (l) => logLines.push(l));
  return new Promise((resolve) => {
    readline.createInterface({ input: child.stdout }).once('line', (l) => resolve({ child, port: JSON.parse(l).port, logLines }));
  });
}

// 扮演 Rust：desktopImport.safeStorageKey 按 keyMode 回假材料 / 拒绝；dialog.message 记录并回 nativeAnswer。
function startSidecar({ keyMode = 'ok', prompt = false, dir = dataDir } = {}) {
  const child = spawn(process.execPath, [sidecarJs], {
    cwd: path.dirname(sidecarJs),
    env: {
      ...process.env, CINDY_TAURI_DATA_DIR: dir, CINDY_TAURI_DESKTOP_PROFILE_DIRS: profileDir,
      CINDY_TAURI_DESKTOP_IMPORT_PROMPT: prompt ? '1' : '0',
    },
    stdio: ['pipe', 'pipe', 'pipe'],
  });
  const errLines = [];
  readline.createInterface({ input: child.stderr }).on('line', (l) => { errLines.push(l); if (process.env.E2E_VERBOSE) console.error('  [sc]', l); });
  let seq = 0;
  const waiters = new Map();
  const pushes = [];
  const natives = [];
  const listeners = new Set();
  const state = { dialogAnswers: [] };
  readline.createInterface({ input: child.stdout }).on('line', (line) => {
    const msg = JSON.parse(line);
    if (msg.native) {
      natives.push(msg.native);
      const { id, method, params } = msg.native;
      if (id === undefined) return;
      let reply = { id, result: null };
      if (method === 'desktopImport.safeStorageKey') {
        if (keyMode === 'deny') reply = { id, error: '[PERMISSION_DENIED] user denied keychain access' };
        else if (process.platform === 'win32') {
          reply = params?.encryptedKey === Buffer.from('fake-dpapi-blob').toString('hex')
            ? { id, result: { scheme: 'win-v10', key: WIN_KEY.toString('hex') } }
            : { id, error: '[INVALID_PARAMS] unexpected encryptedKey' };
        } else reply = { id, result: { scheme: 'mac-v10', password: MAC_PASSWORD } };
      } else if (method === 'dialog.message') {
        reply = { id, result: state.dialogAnswers.shift() ?? 0 };
      }
      child.stdin.write(JSON.stringify({ nativeReply: reply }) + '\n');
      for (const fn of listeners) fn({ native: msg.native });
      return;
    }
    if (msg.push) {
      const p = { channel: msg.push.channel, args: decode(msg.push.payload) };
      pushes.push(p);
      for (const fn of listeners) fn(p);
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
  const waitFor = (pred, ms = 120_000) => new Promise((resolve, reject) => {
    const t = setTimeout(() => { listeners.delete(fn); reject(new Error('wait timeout')); }, ms);
    const fn = (p) => { if (pred(p)) { clearTimeout(t); listeners.delete(fn); resolve(p); } };
    listeners.add(fn);
  });
  const stop = () => new Promise((resolve) => { child.once('exit', resolve); child.stdin.end(); });
  return { call, invoke, waitFor, pushes, natives, state, stop, errLines };
}

const steps = [];
function check(name, cond, detail) {
  steps.push({ name, ok: !!cond });
  console.log(`${cond ? 'PASS' : 'FAIL'} ${name}${detail !== undefined ? ` — ${typeof detail === 'string' ? detail : JSON.stringify(detail).slice(0, 300)}` : ''}`);
  if (!cond) throw new Error(`step failed: ${name}`);
}
const rejects = async (p) => { try { await p; return null; } catch (e) { return e.message; } };

const mock = await startMock();
mockPort = mock.port;
const ids = buildProfile();
const sourceHash = hashTree(profileDir);
let failed = false;
let sc;
try {
  // 1) 扫描
  sc = startSidecar();
  await sc.call('secrets.unlock', { key: masterKey });
  const scan = await sc.invoke('desktop-import:scan');
  const cand = scan.candidates?.[0];
  check('scan finds desktop profile', scan.candidates?.length === 1 && cand.ownerId === USER_ID, scan.candidates);
  check('scan counts importable Pi sessions only', cand.sessions === 3, cand);
  check('scan counts providers', cand.providers === 5, cand.providers);
  check('no marker before import', scan.marker === null);
  check('run rejects unknown profile path', (await rejects(sc.invoke('desktop-import:run', { profileDir: '/etc', dbFile: 'cindy-x.db' })))?.includes('NOT_FOUND'));

  // 2) 导入
  const report = await sc.invoke('desktop-import:run', { profileDir: cand.profileDir, dbFile: cand.dbFile });
  check('import ok', report.ok === true, { sessions: report.sessions, messages: report.messages, providers: report.providers, keys: report.keys });
  check('imported 3 sessions', report.sessions.imported === 3 && report.sessions.existing === 0, report.sessions);
  check('imported 6 messages', report.messages === 6, report.messages);
  check('imported browser tab', report.tabs === 1, report.tabs);
  check('imported recent workdir and alias', report.recent_workdirs === 1 && report.project_aliases === 1, report);
  check('imported 3 Pi providers', report.providers.imported === 3, report.providers);
  check('unsupported providers skipped with reason', report.providers.skipped.length === 2
    && report.providers.skipped.some((p) => p.id === 'oauthy') && report.providers.skipped.some((p) => p.id === 'cconly'), report.providers.skipped);
  check('API key decrypted for keyed', report.keys.imported === 1 && report.keys.missing.length === 1 && report.keys.missing[0] === 'nokey', report.keys);
  check('Pi session files copied', report.piSessions.copied === 3, report.piSessions);
  check('dialogue workdirs copied', report.workdirs.copied === 2, report.workdirs);
  check('local db backed up first', report.backup && existsSync(report.backup), report.backup);
  check('safe storage key asked once', sc.natives.filter((n) => n.method === 'desktopImport.safeStorageKey').length === 1);
  const created = sc.pushes.filter((p) => p.channel === 'local-db:sessions:created').map((p) => p.args[0].sessionId);
  check('renderer told about new sessions', created.length === 3, created.length);

  // 3) 导入后的数据
  const list = await sc.invoke('local-db:sessions:list', 50);
  const byId = Object.fromEntries(list.map((x) => [x.id, x]));
  check('session list has imported active tasks', byId[ids.dialogue] && byId[ids.project], list.map((x) => x.title));
  check('excluded sessions not imported', !byId[ids.claude] && !byId[ids.remote] && !byId[ids.team]);
  const archived = await sc.invoke('local-db:sessions:get', ids.archived);
  check('archived session keeps status', archived.status === 'archived', archived.status);
  const d = await sc.invoke('local-db:sessions:get', ids.dialogue);
  const expectedDir = path.join(dataDir, 'dialogues', '2026-10-01', ids.dialogue);
  check('dialogue workdir rewritten', d.workingDir === expectedDir && existsSync(path.join(expectedDir, 'notes.md')), d.workingDir);
  check('pi session path rewritten', d.sdkSessionId?.startsWith(path.join(dataDir, 'pi-agent-home', 'sessions')) && existsSync(d.sdkSessionId), d.sdkSessionId);
  const head = JSON.parse(readFileSync(d.sdkSessionId, 'utf8').split('\n')[0]);
  check('pi session header cwd rewritten', head.cwd === expectedDir, head.cwd);
  const p = await sc.invoke('local-db:sessions:get', ids.project);
  check('project workdir kept', p.workingDir === path.join(work, 'some-project'), p.workingDir);
  const pending = await sc.invoke('local-db:sessions:interrupted-pending');
  check('desktop in-flight turn not flagged as interrupted', !pending.some((x) => x.sessionId === ids.dialogue), pending);
  const msgs = await sc.invoke('local-db:messages:list', ids.dialogue, { limit: 10 });
  check('messages readable', msgs.length === 2 && msgs.some((m) => m.role === 'assistant'), msgs.map((m) => m.role));
  const tabs = await sc.invoke('local-db:right-sidebar-tabs:list', { sessionId: ids.dialogue });
  check('tab readable', tabs.tabs.length === 1 && tabs.tabs[0].kind === 'browser' && tabs.persistable === true, tabs);
  const providers = (await sc.invoke('maker:provider:list')).providers;
  const view = (id) => providers.find((x) => x.id === id);
  check('keyed provider connected', view('keyed')?.connected === true, view('keyed'));
  check('none-auth provider connected', view('gw')?.connected === true);
  check('provider without key needs reconnect', view('nokey') && view('nokey').connected === false);
  check('renderer reads imported key', (await sc.invoke('safe-storage-read', 'provider_key_keyed_pi')) === API_KEY);
  const providersJson = readFileSync(path.join(dataDir, 'providers.json'), 'utf8');
  check('providers.json has no key', !providersJson.includes(API_KEY));
  const marker = (await sc.invoke('desktop-import:scan')).marker;
  check('marker written', marker?.status === 'imported' && marker.sessions === 3, marker);

  // 4) 幂等：再导一次什么都不变，也不再要钥匙串
  const again = await sc.invoke('desktop-import:run', { profileDir: cand.profileDir, dbFile: cand.dbFile });
  check('re-import skips existing', again.ok && again.sessions.imported === 0 && again.sessions.existing === 3 && again.providers.imported === 0 && again.providers.exists === 3, { s: again.sessions, p: again.providers });
  check('re-import does not ask keychain', sc.natives.filter((n) => n.method === 'desktopImport.safeStorageKey').length === 1);
  check('re-import keeps message count', (await sc.invoke('local-db:messages:list', ids.dialogue, { limit: 10 })).length === 2);

  // 5) 导入的任务继续对话：resume 用复制过来的 Pi 会话文件，keyed 会话带上导入的 key
  const createOpts = { agentKind: 'pi', model: 'mock-model', providerId: 'keyed', permissionMode: 'ask', workingDir: d.workingDir,
    workspaceKind: 'dialogue', resumeSessionId: d.sdkSessionId };
  const before = mock.logLines.filter((l) => l.includes('/keyed/chat/completions') && l.includes('"auth":"ok"')).length;
  await sc.invoke('maker:input:enqueue', ids.dialogue, {
    clientId: 'c1', text: 'continue', persistedContent: JSON.stringify({ text: 'continue' }), model: 'mock-model', effort: 'medium',
    permissionMode: 'ask', workingDir: d.workingDir, chatMessage: { clientId: 'c1', role: 'user', content: 'continue' }, createOpts,
  });
  const end = await sc.waitFor((x) => x.channel === 'maker:event' && x.args[0]?.sessionId === ids.dialogue && ['done', 'error'].includes(x.args[0].event.type), 180_000);
  const authed = mock.logLines.filter((l) => l.includes('/keyed/chat/completions') && l.includes('"auth":"ok"'));
  check('imported task continues with imported key', end.args[0].event.type === 'done' && authed.length > before, end.args[0].event.type === 'error' ? end.args[0].event : `${authed.length - before} authed`);
  const lastReq = authed.at(-1) ?? '';
  check('resumed turn carries desktop history', /"messages":"[^"]*user,assistant,user/.test(lastReq), lastReq.slice(0, 300));
  await sc.stop();
  sc = null;

  check('desktop profile unchanged', hashTree(profileDir) === sourceHash);
  check('no plaintext key in data dir', findPlaintext(dataDir, API_KEY).length === 0, findPlaintext(dataDir, API_KEY));

  // 6) 钥匙串被拒：配置照样导入，key 记为缺失；首次启动询问流程（导入 / 不再提醒）
  // 模拟桌面版正在运行：开着写连接、关掉自动 checkpoint，新任务只在 WAL 里。
  const writer = new DatabaseSync(path.join(profileDir, `cindy-${USER_ID}.db`));
  writer.exec('PRAGMA journal_mode = WAL; PRAGMA wal_autocheckpoint = 0;');
  const liveId = randomUUID();
  writer.prepare(`INSERT INTO sessions (id, title, created_at, updated_at, agent_kind, workspace_kind) VALUES (?, 'Live', ?, ?, 'pi', 'project')`)
    .run(liveId, Date.now(), Date.now());
  check('fixture has uncheckpointed WAL', statSync(path.join(profileDir, `cindy-${USER_ID}.db-wal`)).size > 0);
  const liveHash = hashTree(profileDir, /-shm$/);
  const dataDir2 = path.join(work, 'tauri-data-2');
  sc = startSidecar({ dir: dataDir2, keyMode: 'deny', prompt: true });
  sc.state.dialogAnswers.push(1, 0); // “导入”，然后关掉结果对话框
  await sc.call('secrets.unlock', { key: masterKey });
  await sc.invoke('app-menu:set-locale', 'zh-CN');
  const ask = await sc.waitFor((x) => x.native?.method === 'dialog.message', 20_000);
  check('first launch asks in Chinese', ask.native.params.title === '从 Cindy 桌面版导入' && ask.native.params.buttons?.length === 3
    && ask.native.params.message.includes('4 个任务'), ask.native.params);
  const result = await sc.waitFor((x) => x.native?.method === 'dialog.message' && x.native.params.title !== ask.native.params.title, 60_000);
  check('result dialog reports import (incl. WAL-only task)', result.native.params.title === '导入完成' && result.native.params.message.includes('4 个任务'), result.native.params);
  check('result dialog explains missing keys', result.native.params.detail.includes('API key'), result.native.params.detail);
  const prov2 = (await sc.invoke('maker:provider:list')).providers;
  check('denied keychain: config imported, not connected', prov2.find((x) => x.id === 'keyed')?.connected === false);
  check('WAL-only task imported', !!(await sc.invoke('local-db:sessions:get', liveId))?.id);
  await sc.stop();
  check('running desktop profile unchanged', hashTree(profileDir, /-shm$/) === liveHash);
  writer.close();
  const sourceHash3 = hashTree(profileDir);
  sc = startSidecar({ dir: dataDir2, prompt: true });
  await sc.call('secrets.unlock', { key: masterKey });
  await sc.invoke('app-menu:set-locale', 'zh-CN');
  await new Promise((r) => setTimeout(r, 4500));
  check('no second prompt after import', !sc.natives.some((n) => n.method === 'dialog.message'));
  await sc.stop();

  const dataDir3 = path.join(work, 'tauri-data-3');
  sc = startSidecar({ dir: dataDir3, prompt: true });
  sc.state.dialogAnswers.push(2); // “不再提醒”
  await sc.call('secrets.unlock', { key: masterKey });
  await sc.invoke('app-menu:set-locale', 'en');
  const ask3 = await sc.waitFor((x) => x.native?.method === 'dialog.message', 20_000);
  check('prompt follows locale', ask3.native.params.title === 'Import from Cindy desktop', ask3.native.params.title);
  await new Promise((r) => setTimeout(r, 500));
  const m3 = (await sc.invoke('desktop-import:scan')).marker;
  check("don't ask again stored", m3?.status === 'dismissed' && (await sc.invoke('local-db:sessions:list', 50)).length === 0, m3);
  await sc.stop();
  sc = null;
  check('desktop profile still unchanged', hashTree(profileDir) === sourceHash3);
} catch (err) {
  failed = true;
  console.error(err);
  if (sc?.errLines.length) console.error(sc.errLines.slice(-30).join('\n'));
} finally {
  if (sc) await sc.stop();
  mock.child.kill();
}
console.log(JSON.stringify({ ok: !failed, work, steps: steps.length }));
process.exit(failed ? 1 : 0);
