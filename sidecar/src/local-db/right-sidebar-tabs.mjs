// 右侧栏页签持久化：local-db:right-sidebar-tabs:*，行为对齐 desktop main/localDb/ipc/rightSidebarTabs.ts。
// desktop 的 maker:right-sidebar-tabs:changed 只发给“其它” renderer（弹出的单页签窗口），
// Tauri 版只有主窗口一个 renderer，所以不广播。

import { randomUUID } from 'node:crypto';
import { registerInvoke } from '../ipc.mjs';

const fail = (code, message) => { throw new Error(`[${code}] ${message}`); };
const requireObject = (v, name) => {
  if (!v || typeof v !== 'object' || Array.isArray(v)) fail('INVALID_PARAMS', `${name} must be an object`);
  return v;
};
const requireString = (v, name) => {
  if (typeof v !== 'string' || !v) fail('INVALID_PARAMS', `${name} must be a non-empty string`);
  return v;
};
const requireInt = (v, name) => {
  if (typeof v !== 'number' || !Number.isInteger(v) || v < 0) fail('INVALID_PARAMS', `${name} must be a non-negative integer`);
  return v;
};

export const MAX_TABS_PER_SESSION = 20;
// shared/rightSidebarTabState.ts MAX_STATE_JSON_BYTES
export const MAX_STATE_JSON_BYTES = 16 * 1024;
const SINGLETON_TAB_KINDS = new Set(['subagents']);

function parseState(raw) {
  try {
    return JSON.parse(raw);
  } catch {
    return null;
  }
}

function rowToTab(row) {
  return {
    id: row.id,
    sessionId: row.session_id,
    kind: row.kind,
    position: row.position,
    state: parseState(row.state),
    isActive: row.is_active === 1,
    createdAt: row.created_at,
    updatedAt: row.updated_at,
  };
}

function serializeState(value) {
  if (value === undefined) return '{}';
  let json;
  try {
    json = JSON.stringify(value);
  } catch {
    fail('INVALID_PARAMS', 'tab state must be JSON-serializable');
  }
  if (typeof json !== 'string') fail('INVALID_PARAMS', 'tab state must be JSON-serializable');
  const bytes = Buffer.byteLength(json, 'utf8');
  if (bytes > MAX_STATE_JSON_BYTES) {
    fail('RIGHT_SIDEBAR_STATE_TOO_LARGE', `tab state JSON too large: ${bytes} bytes (limit ${MAX_STATE_JSON_BYTES})`);
  }
  return json;
}

const tooMany = (sessionId) =>
  fail('RIGHT_SIDEBAR_TOO_MANY_TABS', `session ${sessionId} already has ${MAX_TABS_PER_SESSION} tabs (limit reached)`);

export function registerRightSidebarTabsIpc(sql) {
  const sessionExists = (sid) => Boolean(sql.prepare('SELECT 1 FROM sessions WHERE id = ?').get(sid));
  const countTabs = (sid) => sql.prepare('SELECT COUNT(*) AS n FROM right_sidebar_tabs WHERE session_id = ?').get(sid).n;
  const tx = (fn) => {
    sql.exec('BEGIN IMMEDIATE');
    try {
      const out = fn();
      sql.exec('COMMIT');
      return out;
    } catch (err) {
      sql.exec('ROLLBACK');
      throw err;
    }
  };

  registerInvoke('local-db:right-sidebar-tabs:list', (payload) => {
    const sessionId = requireString(requireObject(payload, 'list payload').sessionId, 'sessionId');
    // 会话还没落库（新建对话的草稿）：返回空且不可持久化，renderer 只放内存。
    if (!sessionExists(sessionId)) return { tabs: [], activeTabId: null, persistable: false };
    const rows = sql.prepare('SELECT * FROM right_sidebar_tabs WHERE session_id = ? ORDER BY position ASC').all(sessionId);
    return {
      tabs: rows.map(rowToTab),
      activeTabId: rows.find((r) => r.is_active === 1)?.id ?? null,
      persistable: true,
    };
  });

  registerInvoke('local-db:right-sidebar-tabs:ensure-singleton', (payload) => {
    const obj = requireObject(payload, 'ensure singleton payload');
    const sessionId = requireString(obj.sessionId, 'sessionId');
    const kind = requireString(obj.kind, 'kind');
    if (!SINGLETON_TAB_KINDS.has(kind)) fail('INVALID_PARAMS', `${kind} is not a singleton tab kind`);
    const stateJson = serializeState(obj.state);
    if (!sessionExists(sessionId)) return { tab: null, created: false, persistable: false };
    return tx(() => {
      const find = () => sql.prepare('SELECT * FROM right_sidebar_tabs WHERE session_id = ? AND kind = ? LIMIT 1').get(sessionId, kind);
      const existing = find();
      if (existing) return { tab: rowToTab(existing), created: false, persistable: true };
      const count = countTabs(sessionId);
      if (count >= MAX_TABS_PER_SESSION) tooMany(sessionId);
      const now = Date.now();
      const id = `t_${randomUUID().replace(/-/g, '')}`;
      sql.prepare(`INSERT INTO right_sidebar_tabs (id, session_id, kind, position, state, is_active, created_at, updated_at)
        VALUES (?, ?, ?, ?, ?, 0, ?, ?) ON CONFLICT DO NOTHING`).run(id, sessionId, kind, count, stateJson, now, now);
      const canonical = find();
      if (!canonical) throw new Error(`failed to ensure ${kind} tab for session ${sessionId}`);
      return { tab: rowToTab(canonical), created: canonical.id === id, persistable: true };
    });
  });

  registerInvoke('local-db:right-sidebar-tabs:upsert', (payload) => {
    const obj = requireObject(payload, 'upsert payload');
    const id = requireString(obj.id, 'id');
    const sessionId = requireString(obj.sessionId, 'sessionId');
    const kind = requireString(obj.kind, 'kind');
    const position = requireInt(obj.position, 'position');
    const stateJson = serializeState(obj.state);
    tx(() => {
      const ids = sql.prepare('SELECT id FROM right_sidebar_tabs WHERE session_id = ?').all(sessionId).map((r) => r.id);
      if (!ids.includes(id) && ids.length >= MAX_TABS_PER_SESSION) tooMany(sessionId);
      const now = Date.now();
      // 会话不存在时外键约束会报错，与 desktop 一致（renderer 先看 list 的 persistable）。
      sql.prepare(`INSERT INTO right_sidebar_tabs (id, session_id, kind, position, state, is_active, created_at, updated_at)
        VALUES (?, ?, ?, ?, ?, 0, ?, ?)
        ON CONFLICT(id) DO UPDATE SET kind = excluded.kind, position = excluded.position, state = excluded.state, updated_at = excluded.updated_at`)
        .run(id, sessionId, kind, position, stateJson, now, now);
    });
    return { ok: true };
  });

  registerInvoke('local-db:right-sidebar-tabs:close', (payload) => {
    const id = requireString(requireObject(payload, 'close payload').id, 'id');
    const { changes } = sql.prepare('DELETE FROM right_sidebar_tabs WHERE id = ?').run(id);
    if (changes === 0) fail('NOT_FOUND', `tab ${id} not found`);
    return { ok: true };
  });

  registerInvoke('local-db:right-sidebar-tabs:setActive', (payload) => {
    const obj = requireObject(payload, 'setActive payload');
    const sessionId = requireString(obj.sessionId, 'sessionId');
    const targetId = obj.id === null ? null : requireString(obj.id, 'id');
    tx(() => {
      const now = Date.now();
      sql.prepare('UPDATE right_sidebar_tabs SET is_active = 0, updated_at = ? WHERE session_id = ?').run(now, sessionId);
      if (targetId !== null) {
        const { changes } = sql.prepare('UPDATE right_sidebar_tabs SET is_active = 1, updated_at = ? WHERE id = ? AND session_id = ?')
          .run(now, targetId, sessionId);
        if (changes === 0) fail('NOT_FOUND', `tab ${targetId} not found in session ${sessionId}`);
      }
    });
    return { ok: true };
  });

  registerInvoke('local-db:right-sidebar-tabs:reorder', (payload) => {
    const obj = requireObject(payload, 'reorder payload');
    const sessionId = requireString(obj.sessionId, 'sessionId');
    if (!Array.isArray(obj.orderedIds)) fail('INVALID_PARAMS', 'orderedIds must be an array');
    const orderedIds = obj.orderedIds.map((id, i) => {
      if (typeof id !== 'string' || id === '') fail('INVALID_PARAMS', `orderedIds[${i}] must be a non-empty string`);
      return id;
    });
    tx(() => {
      const now = Date.now();
      const stmt = sql.prepare('UPDATE right_sidebar_tabs SET position = ?, updated_at = ? WHERE id = ? AND session_id = ?');
      orderedIds.forEach((id, i) => stmt.run(i, now, id, sessionId));
    });
    return { ok: true };
  });
}
