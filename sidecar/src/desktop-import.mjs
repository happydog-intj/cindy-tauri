// P4 数据迁移：把 Cindy 桌面版（Electron）的数据导入 Tauri 版。
// 范围：任务（sessions，仅 Pi、本机）、消息、右侧栏页签、最近工作目录 / 项目别名、自定义模型服务及其 API key。
// 原则：
// - 桌面版数据只读：数据库先用 SQLite backup 拍快照再读，文件只复制不移动；
// - 幂等：同 id 的任务 / provider 已存在就跳过，不覆盖；可以重复执行；
// - 写本地库前先备份 <dataDir>/backups/，整批导入在一个事务里；
// - API key：Electron safeStorage 的口令 / 密钥由 Rust 取（macOS 钥匙串会弹授权框），这里解密后写进本机 secret store，
//   明文不落盘、不写日志。
import { createHash } from 'node:crypto';
import { existsSync, readdirSync, readFileSync, statSync } from 'node:fs';
import { constants as fsConstants } from 'node:fs';
import { copyFile, cp, mkdir, readFile, rm, writeFile, rename, opendir } from 'node:fs/promises';
import os from 'node:os';
import path from 'node:path';
import { DatabaseSync, backup } from 'node:sqlite';

import { emitToRenderer, registerInvoke } from './ipc.mjs';
import { callNative } from './native.mjs';
import { getMainLocale, onMainLocaleChange } from './main-locale.mjs';
import { whenReady as whenSecretsReady } from './secret-store.mjs';
import { createDecryptor, decryptEncFile } from './desktop-import/chromium-safe-storage.mjs';

// 桌面版 userData 目录名（desktop main/regionUserData.ts：global=CindyGlobal，cn=Cindy）。
const PROFILE_NAMES = ['CindyGlobal', 'Cindy'];
// desktop BRAND_IDENTITY.dbFilePrefix + 用户 id。
const DB_FILE_RE = /^cindy-([A-Za-z0-9_-]{1,64})\.db$/;
// 单个 dialogue 工作区超过这个大小就不复制（新建空目录，在报告里列出原路径）。
const MAX_WORKDIR_COPY_BYTES = 1024 * 1024 * 1024;
const MARKER = 'desktop-import.json';
// 只导入 Tauri 版能继续使用的任务：本机 Pi 任务，排除远程主机 / 设备 / 团队 / IM 来源。
const EXCLUDE_IF_SET = ['remote_host_id', 'agent_device_id', 'orca_role', 'feishu_open_id', 'im_bot_context_id'];
const PROVIDER_ID_RE = /^[a-z0-9_-]{1,64}$/;

const fail = (code, message) => { throw new Error(`[${code}] ${message}`); };

// desktop main/appSessionState.ts dataOwnerStorageKey。
export function ownerStorageKey(ownerId) {
  return createHash('sha256').update(ownerId).digest('hex').slice(0, 20);
}

const STRINGS = {
  'zh-CN': {
    title: '从 Cindy 桌面版导入',
    message: '在这台电脑上找到 Cindy 桌面版的数据：{sessions} 个任务、{providers} 个自定义模型服务。要导入到这里吗？',
    detail: '只读取桌面版数据，不会修改或删除它。导入 API key 时，macOS 可能会请求访问钥匙串项目“Cindy Safe Storage”，请点“允许”。',
    later: '以后再说', import: '导入', never: '不再提醒',
    doneTitle: '导入完成',
    done: '已导入 {sessions} 个任务、{messages} 条消息、{providers} 个模型服务。',
    notes: '需要留意：', keyMissing: '{n} 个模型服务的 API key 没有导入，请在 设置 → 模型服务 里重新填写。',
    workdirs: '{n} 个任务的工作文件超过 1 GB，没有复制，这些任务继续使用桌面版目录里的原文件。',
    providerSkipped: '{n} 个模型服务暂不支持（OAuth、自定义请求头或非 Pi 运行时）。',
    failedTitle: '导入失败',
    failed: '没有完成导入：{error}。桌面版数据没有改动，下次启动时会再询问。',
    ok: '好',
  },
  'zh-TW': {
    title: '從 Cindy 桌面版匯入',
    message: '在這台電腦上找到 Cindy 桌面版的資料：{sessions} 個任務、{providers} 個自訂模型服務。要匯入到這裡嗎？',
    detail: '只讀取桌面版資料，不會修改或刪除它。匯入 API key 時，macOS 可能會要求存取鑰匙圈項目「Cindy Safe Storage」，請按「允許」。',
    later: '以後再說', import: '匯入', never: '不再提醒',
    doneTitle: '匯入完成',
    done: '已匯入 {sessions} 個任務、{messages} 則訊息、{providers} 個模型服務。',
    notes: '需要留意：', keyMissing: '{n} 個模型服務的 API key 沒有匯入，請在 設定 → 模型服務 重新填寫。',
    workdirs: '{n} 個任務的工作檔案超過 1 GB，沒有複製，這些任務繼續使用桌面版目錄裡的原檔案。',
    providerSkipped: '{n} 個模型服務暫不支援（OAuth、自訂請求標頭或非 Pi 執行環境）。',
    failedTitle: '匯入失敗',
    failed: '沒有完成匯入：{error}。桌面版資料沒有改動，下次啟動時會再詢問。',
    ok: '好',
  },
  en: {
    title: 'Import from Cindy desktop',
    message: 'Found Cindy desktop data on this computer: {sessions} tasks and {providers} custom model providers. Import them here?',
    detail: 'The desktop data is only read, never changed or deleted. When importing API keys, macOS may ask for access to the keychain item "Cindy Safe Storage"; choose Allow.',
    later: 'Not now', import: 'Import', never: "Don't ask again",
    doneTitle: 'Import complete',
    done: 'Imported {sessions} tasks, {messages} messages and {providers} model providers.',
    notes: 'Please note:', keyMissing: 'API keys for {n} providers were not imported; enter them again in Settings → Model Providers.',
    workdirs: 'Working files for {n} tasks are larger than 1 GB and were not copied; those tasks keep using the originals in the desktop folder.',
    providerSkipped: '{n} providers are not supported yet (OAuth, custom headers or a non-Pi runtime).',
    failedTitle: 'Import failed',
    failed: 'The import did not finish: {error}. Desktop data was not changed; you will be asked again next launch.',
    ok: 'OK',
  },
};
function s(key, vars = {}) {
  const loc = getMainLocale();
  const table = STRINGS[loc] ?? (loc?.startsWith('zh') ? STRINGS['zh-CN'] : STRINGS.en);
  return (table[key] ?? STRINGS.en[key]).replace(/\{(\w+)\}/g, (_, k) => String(vars[k] ?? ''));
}

function columnsOf(sql, table, schema = 'main') {
  return sql.prepare(`SELECT name FROM pragma_table_info(?, ?)`).all(table, schema).map((r) => r.name);
}
function tableExists(sql, table) {
  return !!sql.prepare("SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?").get(table);
}
const quote = (c) => `"${c.replaceAll('"', '""')}"`;

/** 一个 desktop 库里可导入任务的 WHERE 条件（按实际存在的列拼）。 */
function importableWhere(cols) {
  const where = ["agent_kind = 'pi'", "status != 'deleted'"];
  for (const c of EXCLUDE_IF_SET) if (cols.includes(c)) where.push(`(${quote(c)} IS NULL OR ${quote(c)} = '')`);
  return where.join(' AND ');
}

function parseJson(text) {
  try { return JSON.parse(text); } catch { return null; }
}

// Electron app.getPath('appData')：macOS ~/Library/Application Support，Windows %APPDATA%（Roaming）。
function appDataRoot() {
  if (process.platform === 'darwin') return path.join(os.homedir(), 'Library', 'Application Support');
  if (process.platform === 'win32') return process.env.APPDATA || path.join(os.homedir(), 'AppData', 'Roaming');
  return process.env.XDG_CONFIG_HOME || path.join(os.homedir(), '.config');
}

const removeDb = async (file) => {
  for (const ext of ['', '-wal', '-shm', '-journal']) await rm(file + ext, { force: true });
};

/**
 * 给桌面版库拍一份一致的快照到 dst，源目录里不留任何新文件：
 * - 没有未合并的 WAL（桌面版没在运行）：直接克隆复制主库文件，quick_check 通过就用；
 * - 有 WAL（桌面版可能正在运行，-shm 已存在）：用只读连接走 SQLite backup，拿到一致快照。
 */
export async function snapshotDb(src, dst) {
  await removeDb(dst);
  const wal = `${src}-wal`;
  let walBytes = 0;
  try { walBytes = statSync(wal).size; } catch { /* 没有 WAL */ }
  if (walBytes === 0) {
    await copyFile(src, dst, fsConstants.COPYFILE_FICLONE);
    let ok = false;
    try {
      const c = new DatabaseSync(dst);
      try { ok = c.prepare('PRAGMA quick_check').get()?.quick_check === 'ok'; } finally { c.close(); }
    } catch { ok = false; }
    if (ok) return;
    await removeDb(dst);
  }
  const conn = new DatabaseSync(src, { readOnly: true });
  try {
    conn.exec('PRAGMA busy_timeout = 5000');
    await backup(conn, dst);
  } finally {
    conn.close();
  }
}

/** 找出这台电脑上的桌面版 profile（只读快照，不弹任何授权）。tmpDir 放临时快照。 */
export async function scanDesktopProfiles(tmpDir) {
  const dirs = process.env.CINDY_TAURI_DESKTOP_PROFILE_DIRS
    ? process.env.CINDY_TAURI_DESKTOP_PROFILE_DIRS.split(path.delimiter).filter(Boolean)
    : PROFILE_NAMES.map((n) => path.join(appDataRoot(), n));
  const candidates = [];
  for (const dir of dirs) {
    let names;
    try { names = readdirSync(dir); } catch { continue; }
    for (const name of names) {
      const m = DB_FILE_RE.exec(name);
      if (!m) continue;
      const file = path.join(dir, name);
      const snap = path.join(tmpDir, `scan-${process.pid}-${Date.now()}.db`);
      let sql;
      try {
        await mkdir(tmpDir, { recursive: true });
        await snapshotDb(file, snap);
        sql = new DatabaseSync(snap, { readOnly: true });
        if (!tableExists(sql, 'sessions')) continue;
        const cols = columnsOf(sql, 'sessions');
        const sessions = sql.prepare(`SELECT COUNT(*) AS n FROM sessions WHERE ${importableWhere(cols)}`).get().n;
        const providers = tableExists(sql, 'custom_providers')
          ? sql.prepare('SELECT COUNT(*) AS n FROM custom_providers').get().n : 0;
        candidates.push({ profileDir: dir, dbFile: name, ownerId: m[1], sessions, providers, modifiedAt: statSync(file).mtimeMs });
      } catch (err) {
        process.stderr.write(`[desktop-import] cannot read ${name}: ${err.message}\n`);
      } finally {
        try { sql?.close(); } catch { /* 已关 */ }
        await removeDb(snap);
      }
    }
  }
  // 最近用过的账号排前面。
  return candidates.sort((a, b) => b.modifiedAt - a.modifiedAt);
}

/** 目录总大小，超过 limit 提前停止并返回 Infinity。 */
async function dirSizeWithin(dir, limit) {
  let total = 0;
  const stack = [dir];
  while (stack.length) {
    const d = stack.pop();
    let h;
    try { h = await opendir(d); } catch { continue; }
    for await (const e of h) {
      const p = path.join(d, e.name);
      if (e.isDirectory()) stack.push(p);
      else if (e.isFile()) {
        try { total += statSync(p).size; } catch { /* 并发删除 */ }
        if (total > limit) return Infinity;
      }
    }
  }
  return total;
}

/** 桌面版 dialogue 工作区 → Tauri 版同结构路径；不在 profile 的 dialogues/ 下返回 null（项目目录原样保留）。 */
function relocateDialogue(profileDir, dataDir, workingDir) {
  if (typeof workingDir !== 'string' || !workingDir) return null;
  const rel = path.relative(profileDir, workingDir);
  if (!rel || rel.startsWith('..') || path.isAbsolute(rel)) return null;
  const parts = rel.split(path.sep);
  const i = parts.indexOf('dialogues');
  if (i < 0 || i === parts.length - 1) return null;
  return path.join(dataDir, 'dialogues', ...parts.slice(i + 1));
}

/** Pi 会话文件（sdk_session_id）在 profile 的 pi-agent-home/sessions 下：复制到本机 agentHome，并改写首行 cwd。 */
async function copyPiSession(profileDir, dataDir, sdkPath, newCwd) {
  if (typeof sdkPath !== 'string' || !sdkPath.endsWith('.jsonl')) return { path: null, status: 'missing' };
  const srcRoot = path.join(profileDir, 'pi-agent-home', 'sessions');
  const rel = path.relative(srcRoot, sdkPath);
  if (!rel || rel.startsWith('..') || path.isAbsolute(rel)) {
    // 不在桌面版目录里（例如用户自定义的 agent home）：只要文件还在就原样引用。
    return existsSync(sdkPath) ? { path: sdkPath, status: 'external' } : { path: null, status: 'missing' };
  }
  if (!existsSync(sdkPath)) return { path: null, status: 'missing' };
  const dst = path.join(dataDir, 'pi-agent-home', 'sessions', rel);
  if (existsSync(dst)) return { path: dst, status: 'exists' };
  await mkdir(path.dirname(dst), { recursive: true });
  const text = await readFile(sdkPath, 'utf8');
  const nl = text.indexOf('\n');
  const head = parseJson(nl < 0 ? text : text.slice(0, nl));
  let out = text;
  if (newCwd && head && head.type === 'session' && typeof head.cwd === 'string') {
    out = JSON.stringify({ ...head, cwd: newCwd }) + (nl < 0 ? '' : text.slice(nl));
  }
  const tmp = `${dst}.tmp-${process.pid}`;
  await writeFile(tmp, out, { mode: 0o600 });
  await rename(tmp, dst);
  return { path: dst, status: 'copied' };
}

async function readMarker(dataDir) {
  try { return JSON.parse(await readFile(path.join(dataDir, 'import', MARKER), 'utf8')); } catch { return null; }
}
async function writeMarker(dataDir, body) {
  const dir = path.join(dataDir, 'import');
  await mkdir(dir, { recursive: true });
  const file = path.join(dir, MARKER);
  await writeFile(`${file}.tmp`, JSON.stringify(body, null, 2));
  await rename(`${file}.tmp`, file);
}

// 向 Rust 要 safeStorage 密钥的参数：macOS 只需钥匙串条目（Rust 侧固定为 "Cindy Safe Storage" / "Cindy Key"）；
// Windows 读 profile 的 Local State，把 os_crypt.encrypted_key 去掉 'DPAPI' 前缀后以 hex 交给 Rust 做 DPAPI 解包。
function safeStorageKeyRequest(profileDir) {
  if (process.platform !== 'win32') return {};
  const state = parseJson(readFileSync(path.join(profileDir, 'Local State'), 'utf8'));
  const b64 = state?.os_crypt?.encrypted_key;
  if (typeof b64 !== 'string' || !b64) fail('NOT_FOUND', 'Local State has no os_crypt key');
  const raw = Buffer.from(b64, 'base64');
  if (raw.subarray(0, 5).toString('latin1') !== 'DPAPI') fail('INVALID_STATE', 'unexpected os_crypt key format');
  return { encryptedKey: raw.subarray(5).toString('hex') };
}

export function registerDesktopImportIpc({ dataDir, db, models, platform = process.platform }) {
  let running = null;
  let secretsReady = false;

  async function readProviderKeys(candidate, providers) {
    // 只有确实有 key 文件时才向 Rust 要 safeStorage 密钥（macOS 会弹钥匙串授权框）。
    const dir = path.join(candidate.profileDir, 'safe-storage');
    const owner = ownerStorageKey(candidate.ownerId);
    const files = new Map();
    for (const p of providers) {
      const names = [`owner_${owner}_provider_key_${p.id}_pi.enc`, `provider_key_${p.id}_pi.enc`];
      const hit = names.map((n) => path.join(dir, n)).find((f) => existsSync(f));
      if (hit) files.set(p.id, hit);
    }
    const keys = new Map();
    if (files.size === 0) return { keys, unavailable: null };
    let material;
    try {
      material = await callNative('desktopImport.safeStorageKey', safeStorageKeyRequest(candidate.profileDir), { timeoutMs: 0 });
    } catch (err) {
      return { keys, unavailable: String(err?.message ?? err) };
    }
    const decrypt = createDecryptor(material);
    if (!decrypt) return { keys, unavailable: 'unsupported safe storage key' };
    for (const [id, file] of files) {
      const plain = decryptEncFile(readFileSync(file, 'utf8'), decrypt);
      if (plain) keys.set(id, plain);
    }
    return { keys, unavailable: null };
  }

  async function importProviders(snap, candidate, report) {
    if (!tableExists(snap, 'custom_providers')) return;
    const rows = snap.prepare('SELECT * FROM custom_providers ORDER BY sort_order ASC, created_at ASC').all();
    const inputs = [];
    for (const row of rows) {
      const runtimes = parseJson(row.runtimes) ?? {};
      const auth = row.auth ? parseJson(row.auth) : undefined;
      if (!PROVIDER_ID_RE.test(String(row.id)) || !runtimes.pi) {
        report.providers.skipped.push({ id: String(row.id), reason: 'no Pi runtime' });
        continue;
      }
      inputs.push({ id: row.id, name: row.name, ...(auth ? { auth } : {}), runtimes: { pi: runtimes.pi } });
    }
    // 已经存在的 provider 不导入，也就不需要它的 key（避免无谓的钥匙串授权框）。
    const needKey = inputs.filter((p) => (p.auth?.method ?? 'apiKey') === 'apiKey' && !models.hasCustomProvider(p.id));
    let keys = new Map();
    if (needKey.length && !secretsReady) {
      report.keys.unavailable = 'local secure storage is unavailable';
    } else if (needKey.length) {
      const r = await readProviderKeys(candidate, needKey);
      keys = r.keys;
      if (r.unavailable) report.keys.unavailable = r.unavailable;
    }
    for (const input of inputs) {
      const usesKey = (input.auth?.method ?? 'apiKey') === 'apiKey';
      try {
        const r = await models.importProvider(input, usesKey ? keys.get(input.id) ?? null : null);
        if (r === 'exists') { report.providers.exists += 1; continue; }
        report.providers.imported += 1;
        if (usesKey) {
          if (keys.has(input.id)) report.keys.imported += 1;
          else report.keys.missing.push(input.id);
        }
      } catch (err) {
        report.providers.skipped.push({ id: input.id, reason: String(err?.message ?? err).slice(0, 200) });
      }
    }
  }

  async function importSessions(snapPath, snap, candidate, report) {
    const sql = db.sql;
    const srcCols = columnsOf(snap, 'sessions');
    const dstCols = columnsOf(sql, 'sessions');
    const cols = dstCols.filter((c) => srcCols.includes(c));
    // 目标列 NOT NULL 而源值为 NULL 时用列默认值（没有默认值的时间列用当前时间）。
    const info = new Map(sql.prepare("SELECT name, \"notnull\" AS nn, dflt_value AS d FROM pragma_table_info('sessions')").all().map((r) => [r.name, r]));
    const valueSql = (c) => {
      const i = info.get(c);
      if (!i?.nn) return '?';
      return `COALESCE(?, ${i.d ?? (c.endsWith('_at') ? Math.floor(Date.now()) : "''")})`;
    };
    const rows = snap.prepare(`SELECT ${cols.map(quote).join(', ')} FROM sessions WHERE ${importableWhere(srcCols)}`).all();
    const exists = sql.prepare('SELECT 1 FROM sessions WHERE id = ?');
    const fresh = rows.filter((r) => !exists.get(r.id));
    report.sessions.existing = rows.length - fresh.length;

    // 先复制文件（工作区、Pi 会话文件），再在一个事务里写库。复制只往新路径写，不覆盖已有文件。
    for (const row of fresh) {
      const newDir = row.workspace_kind === 'dialogue' ? relocateDialogue(candidate.profileDir, dataDir, row.working_dir) : null;
      if (newDir) {
        const src = row.working_dir;
        const size = existsSync(src) ? await dirSizeWithin(src, MAX_WORKDIR_COPY_BYTES) : 0;
        if (size === Infinity) {
          // 太大不复制：任务继续用桌面版目录里的原文件（Pi 会话里的路径也都指向那里）。
          report.workdirs.notCopied.push({ sessionId: row.id, path: src });
        } else {
          await mkdir(newDir, { recursive: true });
          if (size > 0) {
            await cp(src, newDir, { recursive: true, force: false, errorOnExist: false, mode: fsConstants.COPYFILE_FICLONE });
            report.workdirs.copied += 1;
          }
          row.working_dir = newDir;
        }
      }
      const moved = newDir && row.working_dir === newDir ? newDir : null;
      const sdk = await copyPiSession(candidate.profileDir, dataDir, row.sdk_session_id, moved);
      if (row.sdk_session_id) report.piSessions[sdk.status] = (report.piSessions[sdk.status] ?? 0) + 1;
      row.sdk_session_id = sdk.path;
      // 桌面版中断的回合不再续跑，避免导入后弹“任务被中断”。
      if ('active_turn_started_at' in row) row.active_turn_started_at = null;
    }
    if (fresh.length === 0) return [];

    // 本地库先整库备份。
    const backupDir = path.join(dataDir, 'backups');
    await mkdir(backupDir, { recursive: true });
    const backupFile = path.join(backupDir, `${path.basename(db.file, '.db')}.pre-import-${Date.now()}.db`);
    await backup(sql, backupFile);
    report.backup = backupFile;

    sql.prepare('ATTACH DATABASE ? AS src').run(snapPath);
    try {
      sql.exec('BEGIN IMMEDIATE');
      try {
        sql.exec('CREATE TEMP TABLE IF NOT EXISTS import_ids (id TEXT PRIMARY KEY); DELETE FROM temp.import_ids;');
        const ins = sql.prepare(`INSERT OR IGNORE INTO main.sessions (${cols.map(quote).join(', ')}) VALUES (${cols.map(valueSql).join(', ')})`);
        const mark = sql.prepare('INSERT OR IGNORE INTO temp.import_ids (id) VALUES (?)');
        for (const row of fresh) {
          const r = ins.run(...cols.map((c) => row[c] ?? null));
          if (r.changes) { mark.run(row.id); report.sessions.imported += 1; }
        }
        const copyTable = (table) => {
          if (!tableExists(snap, table)) return 0;
          const tc = columnsOf(sql, table).filter((c) => columnsOf(sql, table, 'src').includes(c));
          const list = tc.map(quote).join(', ');
          return Number(sql.prepare(`INSERT OR IGNORE INTO main.${table} (${list}) SELECT ${list} FROM src.${table}
            WHERE session_id IN (SELECT id FROM temp.import_ids)`).run().changes);
        };
        report.messages = copyTable('messages');
        report.tabs = copyTable('right_sidebar_tabs');
        // 最近工作目录 / 项目别名是锦上添花：列对不上就跳过（单条语句失败不影响事务里其它写入）。
        for (const table of ['recent_workdirs', 'project_aliases']) {
          if (!tableExists(snap, table)) continue;
          try {
            const tc = columnsOf(sql, table).filter((c) => columnsOf(sql, table, 'src').includes(c));
            const list = tc.map(quote).join(', ');
            report[table] = Number(sql.prepare(`INSERT OR IGNORE INTO main.${table} (${list}) SELECT ${list} FROM src.${table}`).run().changes);
          } catch (err) {
            process.stderr.write(`[desktop-import] ${table} skipped: ${err.message}\n`);
          }
        }
        sql.exec('COMMIT');
      } catch (err) {
        sql.exec('ROLLBACK');
        throw err;
      }
    } finally {
      sql.exec('DETACH DATABASE src');
    }
    return sql.prepare('SELECT id FROM temp.import_ids').all().map((r) => r.id);
  }

  async function runImport(candidate) {
    const report = {
      ok: false, source: { profileDir: candidate.profileDir, dbFile: candidate.dbFile },
      sessions: { imported: 0, existing: 0 }, messages: 0, tabs: 0,
      providers: { imported: 0, exists: 0, skipped: [] }, keys: { imported: 0, missing: [], unavailable: null },
      workdirs: { copied: 0, notCopied: [] }, piSessions: {}, backup: null,
    };
    // 先等本机 secret store 就绪，否则 key 无处可写（不可用时只导入配置）。
    secretsReady = await whenSecretsReady();
    const importDir = path.join(dataDir, 'import');
    await mkdir(importDir, { recursive: true });
    const snapPath = path.join(importDir, `snapshot-${process.pid}-${Date.now()}.db`);
    let snap;
    try {
      await snapshotDb(path.join(candidate.profileDir, candidate.dbFile), snapPath);
      snap = new DatabaseSync(snapPath, { readOnly: true });
      const imported = await importSessions(snapPath, snap, candidate, report);
      await importProviders(snap, candidate, report);
      for (const sessionId of imported) emitToRenderer('local-db:sessions:created', { sessionId });
      report.ok = true;
      await writeMarker(dataDir, { status: 'imported', at: new Date().toISOString(), source: report.source,
        sessions: report.sessions.imported, providers: report.providers.imported });
      return report;
    } finally {
      try { snap?.close(); } catch { /* 已关 */ }
      await removeDb(snapPath);
    }
  }

  const tmpDir = path.join(dataDir, 'import');
  const scan = () => scanDesktopProfiles(tmpDir);

  async function resolveCandidate(req) {
    const list = await scan();
    if (req === undefined || req === null) return list[0] ?? fail('NOT_FOUND', 'no Cindy desktop data found');
    if (typeof req !== 'object' || typeof req.profileDir !== 'string' || typeof req.dbFile !== 'string') {
      fail('INVALID_PARAMS', 'profileDir and dbFile are required');
    }
    // 只接受扫描出来的候选，不让 renderer 指定任意路径。
    return list.find((c) => c.profileDir === req.profileDir && c.dbFile === req.dbFile) ?? fail('NOT_FOUND', 'unknown desktop profile');
  }

  function start(candidate) {
    if (running) fail('BUSY', 'an import is already running');
    running = runImport(candidate).finally(() => { running = null; });
    return running;
  }

  registerInvoke('desktop-import:scan', async () => ({
    platform,
    candidates: (await scan()).map(({ modifiedAt: _m, ...c }) => c),
    marker: await readMarker(dataDir),
  }));
  registerInvoke('desktop-import:run', async (req) => start(await resolveCandidate(req)));
  registerInvoke('desktop-import:dismiss', async () => {
    await writeMarker(dataDir, { status: 'dismissed', at: new Date().toISOString() });
    return { ok: true };
  });

  // ── 首次启动询问 ──
  async function promptOnce() {
    if (await readMarker(dataDir)) return;
    const candidate = (await scan()).find((c) => c.sessions > 0 || c.providers > 0);
    if (!candidate) return;
    const index = await callNative('dialog.message', {
      kind: 'info',
      title: s('title'),
      message: s('message', { sessions: candidate.sessions, providers: candidate.providers }),
      detail: s('detail'),
      buttons: [s('later'), s('import'), s('never')],
    }, { timeoutMs: 0 }).catch(() => 0);
    if (index === 2) {
      await writeMarker(dataDir, { status: 'dismissed', at: new Date().toISOString() });
      return;
    }
    if (index !== 1) return;
    try {
      const r = await start(candidate);
      const notes = [];
      if (r.keys.missing.length) notes.push(s('keyMissing', { n: r.keys.missing.length }));
      if (r.workdirs.notCopied.length) notes.push(s('workdirs', { n: r.workdirs.notCopied.length }));
      if (r.providers.skipped.length) notes.push(s('providerSkipped', { n: r.providers.skipped.length }));
      await callNative('dialog.message', {
        kind: 'info', title: s('doneTitle'),
        message: s('done', { sessions: r.sessions.imported, messages: r.messages, providers: r.providers.imported }),
        detail: notes.length ? `${s('notes')}\n${notes.join('\n')}` : '',
        buttons: [s('ok')],
      }, { timeoutMs: 0 }).catch(() => 0);
    } catch (err) {
      process.stderr.write(`[desktop-import] failed: ${err?.message ?? err}\n`);
      await callNative('dialog.message', {
        kind: 'error', title: s('failedTitle'),
        message: s('failed', { error: String(err?.message ?? err).slice(0, 300) }),
        buttons: [s('ok')],
      }, { timeoutMs: 0 }).catch(() => 0);
    }
  }

  // 只有 Rust 正式启动（非冒烟）时才打开 CINDY_TAURI_DESKTOP_IMPORT_PROMPT=1；直接跑 sidecar 的测试脚本不扫描本机数据。
  // renderer 起来并同步语言（app-menu:set-locale）后再问，对话框才有父窗口、文案语言也对。
  const autoPrompt = process.env.CINDY_TAURI_DESKTOP_IMPORT_PROMPT === '1';
  if (autoPrompt) {
    let fired = false;
    const fire = () => {
      if (fired) return;
      fired = true;
      off();
      clearTimeout(timer);
      // 让首屏先渲染完。
      setTimeout(() => { promptOnce().catch((err) => process.stderr.write(`[desktop-import] prompt failed: ${err?.message ?? err}\n`)); }, 3000).unref?.();
    };
    const off = onMainLocaleChange(fire);
    const timer = setTimeout(fire, 30_000);
    timer.unref?.();
  }

  return { scan, run: async (req) => start(await resolveCandidate(req)), _test: { promptOnce } };
}
