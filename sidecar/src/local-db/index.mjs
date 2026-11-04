// 本地库入口：openLocalDb 打开 <dataDir>/cindy-tauri-<ownerId>.db（node:sqlite），
// registerLocalDbIpc 注册 renderer 会话列表 / 历史渲染需要的 local-db:* 通道。
// 形状对齐 cindy apps/desktop/src/main/localDb/ipc/{sessions,messages,recentWorkdirs,projectAliases}.ts；
// 导入、分享、worktree、远程主机、置顶摘要生成等不在本里程碑内。

import { DatabaseSync } from 'node:sqlite';
import { mkdirSync } from 'node:fs';
import path from 'node:path';
import { randomUUID } from 'node:crypto';
import { projectHistoryView } from '@cindy/maker-shared/message-window';

import { emitToRenderer, registerInvoke } from '../ipc.mjs';
import { migrate } from './schema.mjs';
import { messageToCamel, sessionToCamel, extractMessagePreview, PREVIEW_EXTRACT_CHARS } from './mapper.mjs';
import { createEventPersister } from './persist.mjs';
import { registerRightSidebarTabsIpc } from './right-sidebar-tabs.mjs';

const fail = (code, message) => { throw new Error(`[${code}] ${message}`); };
const requireString = (v, name) => {
  if (typeof v !== 'string' || !v) fail('INVALID_PARAMS', `${name} must be a non-empty string`);
  return v;
};
const SESSION_ID_RE = /^[A-Za-z0-9_-]{1,128}$/;
const AGENT_KINDS = new Set(['cc', 'codex', 'pi']);
// 一页最多读这么多行再交给 projectHistoryView 分组（desktop 扫描预算 2000 行）。
const HISTORY_PAGE_ROWS = 400;

const pad = (n) => String(n).padStart(2, '0');
/** 对齐 desktop dialogue 工作区：<dataDir>/dialogues/<YYYY-MM-DD>/<sessionId>。 */
export function dialogueDir(dataDir, sessionId, at = Date.now()) {
  const d = new Date(at);
  return path.join(dataDir, 'dialogues', `${d.getFullYear()}-${pad(d.getMonth() + 1)}-${pad(d.getDate())}`, sessionId);
}

const VISIBLE = 'm.session_id = s.id AND m.rewind_at IS NULL AND (s.cleared_at IS NULL OR m.created_at > s.cleared_at)';
const LATEST = `FROM messages m WHERE ${VISIBLE} AND m.role IN ('user','assistant') ORDER BY m.created_at DESC, m.rowid DESC LIMIT 1`;
const SESSION_SELECT = `SELECT s.*,
  (SELECT COUNT(*) FROM messages m WHERE ${VISIBLE}) AS message_count,
  (SELECT substr(m.content, 1, ${PREVIEW_EXTRACT_CHARS}) ${LATEST}) AS latest_content,
  (SELECT m.role ${LATEST}) AS latest_role
  FROM sessions s`;

export function openLocalDb({ dataDir, ownerId }) {
  if (typeof ownerId !== 'string' || !/^[A-Za-z0-9_.-]{1,64}$/.test(ownerId)) fail('INVALID_PARAMS', 'invalid ownerId');
  mkdirSync(dataDir, { recursive: true });
  const file = path.join(dataDir, `cindy-tauri-${ownerId}.db`);
  const sql = new DatabaseSync(file);
  sql.exec('PRAGMA journal_mode = WAL; PRAGMA busy_timeout = 5000; PRAGMA foreign_keys = ON;');
  migrate(sql);
  const bootAt = Date.now();

  const toSession = (row) => {
    if (!row) return null;
    const preview = extractMessagePreview(row.latest_content, row.latest_role);
    return { ...sessionToCamel(row, bootAt), preview };
  };
  const getRow = (id) => sql.prepare(`${SESSION_SELECT} WHERE s.id = ?`).get(id);
  const getMessageRow = (sid, clientId) =>
    sql.prepare('SELECT rowid, * FROM messages WHERE session_id = ? AND client_id = ?').get(sid, clientId);

  const patchedPreview = (sid) => {
    const row = getRow(sid);
    if (row) emitToRenderer('local-db:sessions:patched', { sessionId: sid, patch: { preview: toSession(row).preview } });
  };

  // ── 同步写库原语（persist.mjs 的 store 接口） ──
  const store = {
    insert(sid, row) {
      const sess = sql.prepare('SELECT agent_kind FROM sessions WHERE id = ?').get(sid);
      if (!sess) return null; // 会话行未建：跳过（FK）
      const existing = getMessageRow(sid, row.clientId);
      if (existing) return messageToCamel(existing);
      sql.prepare(`INSERT INTO messages (id, client_id, session_id, role, content, tool_use_id, agent_meta, agent_kind, created_at)
        VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)`).run(
        randomUUID(), row.clientId, sid, row.role, JSON.stringify(row.content ?? null), row.toolUseId ?? null,
        row.agentMeta ? JSON.stringify(row.agentMeta) : null, sess.agent_kind ?? null, row.createdAt ?? Date.now(),
      );
      const now = Date.now();
      sql.prepare('UPDATE sessions SET last_activity_at = ?, updated_at = ? WHERE id = ?').run(now, now, sid);
      return messageToCamel(getMessageRow(sid, row.clientId));
    },
    updateContent(sid, clientId, content) {
      const r = sql.prepare('UPDATE messages SET content = ? WHERE session_id = ? AND client_id = ?')
        .run(JSON.stringify(content ?? null), sid, clientId);
      return r.changes ? messageToCamel(getMessageRow(sid, clientId)) : null;
    },
    latestCreatedAt(sid) {
      return sql.prepare('SELECT MAX(created_at) AS t FROM messages WHERE session_id = ?').get(sid)?.t ?? null;
    },
    providerIdOf(sid) {
      return sql.prepare('SELECT provider_id FROM sessions WHERE id = ?').get(sid)?.provider_id ?? null;
    },
    noteTurnEnded(sid, at) {
      sql.prepare('UPDATE sessions SET last_turn_ended_at = ? WHERE id = ?').run(at, sid);
      emitToRenderer('local-db:sessions:patched', { sessionId: sid, patch: { lastTurnEndedAt: at } });
    },
    emitError(sid, persistId) {
      emitToRenderer('local-db:session:error-persisted', { sessionId: sid, clientId: persistId });
    },
    emitRow(sid, message) {
      emitToRenderer('local-db:messages:created', { sessionId: sid, message });
      if (message.role === 'user' || message.role === 'assistant') patchedPreview(sid);
    },
  };
  const persister = createEventPersister(store);

  function touchRecentWorkdir(p, at) {
    sql.prepare('INSERT INTO recent_workdirs (path, last_used_at) VALUES (?, ?) ON CONFLICT(path) DO UPDATE SET last_used_at = excluded.last_used_at').run(p, at);
  }

  // ── 会话写入（sessions.ts create 的校验子集）──
  function createSessionRow(body = {}) {
    const now = Date.now();
    const id = body.id === undefined || body.id === null ? randomUUID() : String(body.id);
    if (!SESSION_ID_RE.test(id)) fail('INVALID_PARAMS', 'invalid session id');
    if (body.agentKind !== undefined && !AGENT_KINDS.has(body.agentKind)) fail('INVALID_PARAMS', `invalid agentKind: ${String(body.agentKind)}`);
    if (body.title !== undefined && (typeof body.title !== 'string' || body.title.length > 200)) {
      fail('INVALID_PARAMS', 'title must be a string no longer than 200 characters');
    }
    if (body.workspaceKind !== undefined && body.workspaceKind !== 'project' && body.workspaceKind !== 'dialogue') {
      fail('INVALID_PARAMS', `invalid workspaceKind: ${String(body.workspaceKind)}`);
    }
    if (body.remoteHostId || body.agentDeviceId || body.orcaRole) {
      fail('NOT_IMPLEMENTED', 'remote / orca sessions are not available in the Tauri build yet');
    }
    const existing = getRow(id);
    if (existing) return toSession(existing);
    const workspaceKind = body.workspaceKind ?? 'project';
    let workingDir = typeof body.workingDir === 'string' && body.workingDir.trim() ? body.workingDir : null;
    if (workspaceKind === 'dialogue' && !workingDir) {
      workingDir = dialogueDir(dataDir, id, now);
      mkdirSync(workingDir, { recursive: true });
    }
    sql.prepare(`INSERT INTO sessions (id, title, working_dir, project_dir, workspace_kind, model, effort, permission_mode,
        status, agent_kind, provider_id, plan_mode_enabled, fast_mode, created_at, updated_at, last_activity_at)
      VALUES (?, ?, ?, ?, ?, ?, ?, ?, 'active', ?, ?, ?, ?, ?, ?, ?)`).run(
      id, body.title || 'New Maker', workingDir, typeof body.projectDir === 'string' ? body.projectDir : null, workspaceKind,
      typeof body.model === 'string' && body.model ? body.model : 'claude-sonnet-4-6',
      typeof body.effort === 'string' && body.effort ? body.effort : 'high',
      typeof body.permissionMode === 'string' && body.permissionMode ? body.permissionMode : 'ask',
      body.agentKind ?? 'cc', typeof body.providerId === 'string' ? body.providerId : null,
      body.planModeEnabled ? 1 : 0, body.fastMode ? 1 : 0, now, now, now,
    );
    if (workspaceKind === 'project' && workingDir) touchRecentWorkdir(body.projectDir || workingDir, now);
    if (body.provisional !== true) emitToRenderer('local-db:sessions:created', { sessionId: id });
    return toSession(getRow(id));
  }

  // maker-ipc 的 upsertSession(row)：camelCase，字段见 maker-ipc.mjs upsertRow。
  function upsertSession(row) {
    if (!row || typeof row.id !== 'string') return;
    if (!getRow(row.id)) {
      createSessionRow({
        id: row.id, agentKind: row.agentKind ?? 'pi', title: row.title || 'New Maker', workingDir: row.workDir,
        workspaceKind: 'dialogue', model: row.model, permissionMode: row.permissionMode ?? undefined, providerId: row.providerId,
      });
      return;
    }
    const sets = [];
    const vals = [];
    const put = (col, v) => { if (v !== undefined && v !== null && v !== '') { sets.push(`${col} = ?`); vals.push(v); } };
    put('agent_kind', row.agentKind);
    put('title', row.title);
    put('working_dir', row.workDir);
    put('provider_id', row.providerId);
    put('model', row.model);
    put('permission_mode', row.permissionMode);
    put('status', row.status);
    put('updated_at', row.updatedAt ?? Date.now());
    sql.prepare(`UPDATE sessions SET ${sets.join(', ')} WHERE id = ?`).run(...vals, row.id);
  }

  // updateSessionInDb 的白名单子集（重命名 / 置顶 / 归档 / 模型档位）。
  const PATCH_COLUMNS = {
    title: 'title', model: 'model', effort: 'effort', permissionMode: 'permission_mode', status: 'status',
    providerId: 'provider_id', sdkSessionId: 'sdk_session_id', summary: 'summary',
    fastMode: 'fast_mode', planModeEnabled: 'plan_mode_enabled', notificationsMuted: 'notifications_muted',
    pinnedAt: 'pinned_at', hiddenAt: 'hidden_at', clearedAt: 'cleared_at', contextTokens: 'context_tokens',
    contextWindow: 'context_window', totalTokenUsage: 'total_token_usage', totalCostUsd: 'total_cost_usd',
  };
  const DATE_KEYS = new Set(['pinnedAt', 'hiddenAt', 'clearedAt']);
  function updateSession(id, patch) {
    if (!getRow(id)) fail('NOT_FOUND', 'Session 不存在');
    if (patch.workingDir !== undefined || patch.agentKind !== undefined) {
      fail('NOT_IMPLEMENTED', 'moving or switching agent is not available in the Tauri build yet');
    }
    if (patch.title !== undefined && (typeof patch.title !== 'string' || patch.title.length > 200)) fail('INVALID_PARAMS', 'invalid title');
    if (patch.notificationsMuted !== undefined && typeof patch.notificationsMuted !== 'boolean') {
      fail('INVALID_PARAMS', 'notificationsMuted must be a boolean');
    }
    const sets = [];
    const vals = [];
    for (const [k, col] of Object.entries(PATCH_COLUMNS)) {
      if (!(k in patch)) continue;
      let v = patch[k];
      if (typeof v === 'boolean') v = v ? 1 : 0;
      else if (DATE_KEYS.has(k) && v !== null) v = typeof v === 'number' ? v : Date.parse(v);
      if (v !== null && typeof v !== 'string' && !(typeof v === 'number' && Number.isFinite(v))) fail('INVALID_PARAMS', `invalid ${k}`);
      sets.push(`${col} = ?`);
      vals.push(v);
    }
    sets.push('updated_at = ?');
    vals.push(Date.now());
    sql.prepare(`UPDATE sessions SET ${sets.join(', ')} WHERE id = ?`).run(...vals, id);
    const updated = toSession(getRow(id));
    emitToRenderer('local-db:sessions:patched', { sessionId: id, patch: { ...patch, updatedAt: updated.updatedAt } });
    return updated;
  }

  // ── 读 ──
  function listSessions(limit = 20, status, options = {}) {
    const cap = Math.min(Math.max(Number.isInteger(limit) ? limit : 20, 1), 500);
    const where = [];
    const vals = [];
    if (typeof status === 'string' && status) { where.push('s.status = ?'); vals.push(status); }
    else where.push("s.status != 'deleted'");
    if (Array.isArray(options?.ids)) {
      const ids = options.ids.filter((x) => typeof x === 'string').slice(0, 500);
      if (!ids.length) return [];
      where.push(`s.id IN (${ids.map(() => '?').join(',')})`);
      vals.push(...ids);
    }
    const b = options?.before;
    if (b && typeof b.updatedAt === 'number' && typeof b.id === 'string') {
      where.push('(s.updated_at < ? OR (s.updated_at = ? AND s.id < ?))');
      vals.push(b.updatedAt, b.updatedAt, b.id);
    }
    return sql.prepare(`${SESSION_SELECT} WHERE ${where.join(' AND ')} ORDER BY s.updated_at DESC, s.id DESC LIMIT ?`)
      .all(...vals, cap).map(toSession);
  }

  function clearedAtOf(sid) {
    const row = sql.prepare('SELECT cleared_at FROM sessions WHERE id = ?').get(sid);
    if (!row) fail('NOT_FOUND', 'History is unavailable');
    return row.cleared_at ?? null;
  }

  /** messages:list：倒序取 limit 条（游标 before=消息 id / beforeTs），返回正序。 */
  function listMessages(sid, opts = {}) {
    const clearedAt = clearedAtOf(sid);
    const limit = Math.min(Math.max(Number.isInteger(opts?.limit) ? opts.limit : 100, 1), 1000);
    const where = ['session_id = ?', 'rewind_at IS NULL'];
    const vals = [sid];
    if (clearedAt !== null) { where.push('created_at > ?'); vals.push(clearedAt); }
    if (typeof opts?.before === 'string' && opts.before) {
      const a = sql.prepare('SELECT rowid, created_at FROM messages WHERE session_id = ? AND id = ?').get(sid, opts.before);
      if (!a) fail('NOT_FOUND', 'History range changed');
      where.push('(created_at < ? OR (created_at = ? AND rowid < ?))');
      vals.push(a.created_at, a.created_at, a.rowid);
    } else if (typeof opts?.beforeTs === 'number') {
      where.push('created_at < ?');
      vals.push(opts.beforeTs);
    }
    return sql.prepare(`SELECT rowid, * FROM messages WHERE ${where.join(' AND ')} ORDER BY created_at DESC, rowid DESC LIMIT ?`)
      .all(...vals, limit).reverse().map(messageToCamel);
  }

  /** messages:view：historyViewReader.page 的简化版；分组直接复用 maker-shared projectHistoryView（非 lazy）。 */
  function historyView(sid, opts = {}, running = () => false) {
    const before = typeof opts?.before === 'string' && opts.before ? opts.before : undefined;
    const rows = listMessages(sid, { limit: HISTORY_PAGE_ROWS + 1, ...(before ? { before } : {}) });
    const hasMore = rows.length > HISTORY_PAGE_ROWS;
    let page = hasMore ? rows.slice(1) : rows;
    // 不在 turn 中间切页：从第一条 user 行开始，更早的留给下一页。
    if (hasMore) {
      const firstUser = page.findIndex((r) => r.role === 'user');
      if (firstUser > 0) page = page.slice(firstUser);
    }
    const items = projectHistoryView(page, !before && running(sid), false);
    return { version: 1, items, hasMore, nextCursor: hasMore && page.length ? page[0].id : null };
  }

  /** messages:work-details：ref 的首尾消息（含）之间的行。 */
  function workDetails(sid, ref) {
    clearedAtOf(sid);
    const pos = (id) => sql.prepare('SELECT rowid, created_at FROM messages WHERE session_id = ? AND id = ?').get(sid, id);
    const first = pos(requireString(ref?.firstMessageId, 'firstMessageId'));
    const last = pos(requireString(ref?.lastMessageId, 'lastMessageId'));
    if (!first || !last) fail('NOT_FOUND', 'History range changed');
    const messages = sql.prepare(`SELECT rowid, * FROM messages WHERE session_id = ? AND rewind_at IS NULL
        AND (created_at > ? OR (created_at = ? AND rowid >= ?)) AND (created_at < ? OR (created_at = ? AND rowid <= ?))
        ORDER BY created_at ASC, rowid ASC LIMIT 2000`)
      .all(sid, first.created_at, first.created_at, first.rowid, last.created_at, last.created_at, last.rowid).map(messageToCamel);
    return { version: 1, messages, hasMore: false, nextCursor: null };
  }

  return {
    sql,
    file,
    bootAt,
    // brief 约定接口
    upsertSession,
    persistMakerEvent: (sid, event) => persister.persistMakerEvent(sid, event),
    getSession: (id) => toSession(getRow(id)),
    listSessions,
    // desktop agent-input-coordinator 在 onAccepted 写 user 行，同时推进 user_send_at / active_turn_started_at。
    persistUserMessage(sid, { clientId, content, agentMeta }) {
      if (typeof clientId !== 'string' || !clientId) return null;
      const now = Date.now();
      const msg = store.insert(sid, { clientId, role: 'user', content: content ?? '', agentMeta: agentMeta ?? null, createdAt: now });
      if (!msg) return null;
      sql.prepare('UPDATE sessions SET user_send_at = ?, active_turn_started_at = ? WHERE id = ?').run(now, now, sid);
      store.emitRow(sid, msg);
      return msg.clientId;
    },
    persistInteractionRequest: (sid, req) => persister.persistInteractionRequest(sid, req),
    persistInteractionResolved: (sid, requestId, decision) => persister.persistInteractionResolved(sid, requestId, decision),
    _internal: { createSessionRow, updateSession, listMessages, historyView, workDetails, getMessageRow, store },
    close() { sql.close(); },
  };
}

export function registerLocalDbIpc(db, { isRunning = () => false } = {}) {
  const { sql } = db;
  const I = db._internal;

  // ── right sidebar tabs ──
  registerRightSidebarTabsIpc(sql);

  // ── sessions ──
  registerInvoke('local-db:sessions:list', (limit, status, options) => db.listSessions(limit, status, options ?? {}));
  registerInvoke('local-db:sessions:create', (body) => I.createSessionRow(body && typeof body === 'object' ? body : {}));
  registerInvoke('local-db:sessions:get', (id) => {
    const s = db.getSession(requireString(id, 'id'));
    if (!s) fail('NOT_FOUND', 'Session 不存在');
    return s;
  });
  registerInvoke('local-db:sessions:get-many', (ids) => {
    if (!Array.isArray(ids) || ids.length > 200) fail('INVALID_PARAMS', 'sessionIds must be an array of at most 200 ids');
    return db.listSessions(200, undefined, { ids });
  });
  registerInvoke('local-db:sessions:restore-if-archived', (id) => db.getSession(requireString(id, 'id')));
  registerInvoke('local-db:sessions:update', (id, patch) => {
    if (!patch || typeof patch !== 'object' || Array.isArray(patch)) fail('INVALID_PARAMS', 'patch must be an object');
    return I.updateSession(requireString(id, 'id'), patch);
  });
  registerInvoke('local-db:sessions:touchUserSend', (id, atMs) => {
    const at = typeof atMs === 'number' && Number.isFinite(atMs) ? atMs : Date.now();
    sql.prepare('UPDATE sessions SET user_send_at = ?, last_activity_at = ?, updated_at = ? WHERE id = ?')
      .run(at, at, at, requireString(id, 'id'));
  });
  registerInvoke('local-db:sessions:set-pinned-card-summaries', (enabled) => {
    if (typeof enabled !== 'boolean') fail('INVALID_PARAMS', 'enabled must be a boolean');
  });
  // 中断判定同 mapper.isInterrupted：本进程启动前开始、没收尾的 turn。
  const interruptedRows = () => sql.prepare(`SELECT id, active_turn_started_at FROM sessions WHERE status = 'active'
      AND active_turn_started_at IS NOT NULL AND active_turn_started_at < ?
      AND active_turn_started_at > COALESCE(last_turn_ended_at, 0) AND active_turn_started_at > COALESCE(cleared_at, 0)`).all(db.bootAt);
  registerInvoke('local-db:sessions:interrupted-pending', () => interruptedRows().map((r) => r.id));
  registerInvoke('local-db:sessions:interrupted-resume-candidates', () =>
    interruptedRows().map((r) => ({ sessionId: r.id, interruptedAt: r.active_turn_started_at, activity: null })));
  registerInvoke('local-db:sessions:error-tail-pending', () => sql.prepare(`SELECT s.id FROM sessions s WHERE s.status = 'active' AND (
      SELECT CASE WHEN m.role = 'error' AND COALESCE(json_extract(m.content, '$.dismissed'), 0) != 1 THEN 1 ELSE 0 END
      FROM messages m WHERE m.session_id = s.id AND m.rewind_at IS NULL ORDER BY m.created_at DESC, m.rowid DESC LIMIT 1) = 1`)
    .all().map((r) => r.id));
  registerInvoke('local-db:sessions:ack-interrupted', (id) => {
    sql.prepare('UPDATE sessions SET last_turn_ended_at = ? WHERE id = ?').run(Date.now(), requireString(id, 'id'));
    return { ok: true };
  });
  registerInvoke('local-db:sessions:dismiss-pending-alerts', (ids) => {
    if (!Array.isArray(ids) || ids.length > 500) fail('INVALID_PARAMS', 'sessionIds must be an array');
    const now = Date.now();
    for (const id of ids) sql.prepare('UPDATE sessions SET last_turn_ended_at = ? WHERE id = ?').run(now, requireString(id, 'sessionId'));
    return { dismissed: ids.length, processed: ids, failed: [] };
  });

  // ── messages ──
  registerInvoke('local-db:messages:list', (sid, opts) => I.listMessages(requireString(sid, 'sessionId'), opts ?? {}));
  registerInvoke('local-db:messages:view', (sid, opts) => I.historyView(requireString(sid, 'sessionId'), opts ?? {}, isRunning));
  registerInvoke('local-db:messages:work-details', (sid, ref) => I.workDetails(requireString(sid, 'sessionId'), ref));
  registerInvoke('local-db:messages:create', (sid, body) => {
    const sessionId = requireString(sid, 'sessionId');
    if (!body || typeof body !== 'object') fail('INVALID_PARAMS', 'body required');
    requireString(body.clientId, 'clientId');
    requireString(body.role, 'role');
    const at = typeof body.createdAt === 'string' ? Date.parse(body.createdAt) : Date.now();
    const msg = I.store.insert(sessionId, {
      clientId: body.clientId, role: body.role, content: body.content, toolUseId: body.toolUseId ?? null,
      agentMeta: body.agentMeta ?? null, createdAt: Number.isFinite(at) ? at : Date.now(),
    });
    if (!msg) fail('NOT_FOUND', 'Session 不存在');
    I.store.emitRow(sessionId, msg);
    return msg;
  });
  registerInvoke('local-db:messages:updateContent', (sid, clientId, content) => {
    const msg = I.store.updateContent(requireString(sid, 'sessionId'), requireString(clientId, 'clientId'), content);
    if (!msg) fail('NOT_FOUND', 'Message 不存在');
    return msg;
  });
  registerInvoke('local-db:messages:dismiss-error', (sid, clientId) => {
    const row = I.getMessageRow(requireString(sid, 'sessionId'), requireString(clientId, 'clientId'));
    if (!row || row.role !== 'error') fail('NOT_FOUND', 'Message 不存在');
    let content = {};
    try { content = JSON.parse(row.content) ?? {}; } catch { /* 原内容损坏时只写 dismissed */ }
    return I.store.updateContent(sid, clientId, { ...content, dismissed: true });
  });
  const aroundBy = (sid, col, value, opts) => {
    const radius = Math.min(Math.max(Number.isInteger(opts?.radius) ? opts.radius : 20, 1), 200);
    const a = sql.prepare(`SELECT rowid, created_at FROM messages WHERE session_id = ? AND ${col} = ?`).get(sid, value);
    if (!a) return [];
    const older = sql.prepare(`SELECT rowid, * FROM messages WHERE session_id = ? AND rewind_at IS NULL
        AND (created_at < ? OR (created_at = ? AND rowid <= ?)) ORDER BY created_at DESC, rowid DESC LIMIT ?`)
      .all(sid, a.created_at, a.created_at, a.rowid, radius + 1).reverse();
    const newer = sql.prepare(`SELECT rowid, * FROM messages WHERE session_id = ? AND rewind_at IS NULL
        AND (created_at > ? OR (created_at = ? AND rowid > ?)) ORDER BY created_at ASC, rowid ASC LIMIT ?`)
      .all(sid, a.created_at, a.created_at, a.rowid, radius);
    return [...older, ...newer].map(messageToCamel);
  };
  registerInvoke('local-db:messages:around', (sid, id, opts) => aroundBy(requireString(sid, 'sessionId'), 'id', requireString(id, 'messageId'), opts));
  registerInvoke('local-db:messages:around-client-id', (sid, cid, opts) =>
    aroundBy(requireString(sid, 'sessionId'), 'client_id', requireString(cid, 'clientId'), opts));
  registerInvoke('local-db:messages:estimatedSessionValue', () => ({
    totalMoney: { amount: 0, currency: 'USD', approximate: false, kind: 'actual-cost' }, turns: [],
  }));

  // ── recent-workdirs / project-aliases ──
  registerInvoke('local-db:recent-workdirs:list', () =>
    sql.prepare('SELECT path, last_used_at FROM recent_workdirs ORDER BY last_used_at DESC LIMIT 50').all()
      .map((r) => ({ path: r.path, lastUsedAt: new Date(r.last_used_at).toISOString() })));
  registerInvoke('local-db:recent-workdirs:remove', (input) => {
    const p = typeof input === 'string' ? input : input?.path;
    sql.prepare('DELETE FROM recent_workdirs WHERE path = ?').run(requireString(p, 'path'));
    emitToRenderer('local-db:recent-workdirs:changed', {});
    return { ok: true };
  });
  registerInvoke('local-db:project-aliases:list', () =>
    sql.prepare('SELECT project_key, alias, updated_at FROM project_aliases').all()
      .map((r) => ({ projectKey: r.project_key, alias: r.alias, updatedAt: new Date(r.updated_at).toISOString() })));
  registerInvoke('local-db:project-aliases:set', (input) => {
    const key = requireString(input?.projectKey, 'projectKey');
    const alias = requireString(input?.alias, 'alias');
    if (alias.length > 200) fail('INVALID_PARAMS', 'alias too long');
    sql.prepare(`INSERT INTO project_aliases (project_key, alias, updated_at) VALUES (?, ?, ?)
      ON CONFLICT(project_key) DO UPDATE SET alias = excluded.alias, updated_at = excluded.updated_at`).run(key, alias, Date.now());
    emitToRenderer('local-db:project-aliases:changed', {});
    return { ok: true };
  });
  registerInvoke('local-db:project-aliases:delete', (key) => {
    sql.prepare('DELETE FROM project_aliases WHERE project_key = ?').run(requireString(key, 'projectKey'));
    emitToRenderer('local-db:project-aliases:changed', {});
    return { ok: true };
  });
}
