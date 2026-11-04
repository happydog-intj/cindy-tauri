// 本地库表结构与版本迁移。
// 字段取自 cindy apps/desktop/src/main/localDb/schema.ts 的 sessions / messages / recent_workdirs /
// project_aliases 子集，列名与 desktop 一致（snake_case），只保留会话列表和历史渲染用得到的列。
// 版本号存在 PRAGMA user_version；新增迁移只往 MIGRATIONS 末尾追加，不改已发布的条目。

const MIGRATIONS = [
  // v1：初始表
  `
  CREATE TABLE IF NOT EXISTS sessions (
    id TEXT PRIMARY KEY,
    title TEXT NOT NULL DEFAULT 'New Maker',
    working_dir TEXT,
    project_dir TEXT,
    workspace_kind TEXT NOT NULL DEFAULT 'project',
    model TEXT NOT NULL DEFAULT 'claude-sonnet-4-6',
    effort TEXT NOT NULL DEFAULT 'high',
    permission_mode TEXT NOT NULL DEFAULT 'ask',
    status TEXT NOT NULL DEFAULT 'active',
    sdk_session_id TEXT,
    total_token_usage INTEGER NOT NULL DEFAULT 0,
    total_cost_usd REAL NOT NULL DEFAULT 0,
    context_tokens INTEGER NOT NULL DEFAULT 0,
    context_window INTEGER NOT NULL DEFAULT 0,
    fast_mode INTEGER NOT NULL DEFAULT 0,
    plan_mode_enabled INTEGER NOT NULL DEFAULT 0,
    cleared_at INTEGER,
    pinned_at INTEGER,
    hidden_at INTEGER,
    notifications_muted INTEGER NOT NULL DEFAULT 0,
    user_send_at INTEGER,
    last_activity_at INTEGER,
    agent_kind TEXT NOT NULL DEFAULT 'cc',
    source TEXT NOT NULL DEFAULT 'desktop',
    provider_id TEXT,
    active_turn_started_at INTEGER,
    last_turn_ended_at INTEGER,
    summary TEXT,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
  );
  CREATE INDEX IF NOT EXISTS idx_sessions_updated ON sessions(updated_at);
  CREATE TABLE IF NOT EXISTS messages (
    id TEXT PRIMARY KEY,
    client_id TEXT NOT NULL,
    session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
    role TEXT NOT NULL,
    content TEXT NOT NULL,
    tool_use_id TEXT,
    agent_meta TEXT,
    agent_kind TEXT,
    created_at INTEGER NOT NULL,
    rewind_at INTEGER
  );
  CREATE UNIQUE INDEX IF NOT EXISTS idx_messages_session_client ON messages(session_id, client_id);
  CREATE INDEX IF NOT EXISTS idx_messages_session_created ON messages(session_id, created_at);
  CREATE TABLE IF NOT EXISTS recent_workdirs (
    path TEXT PRIMARY KEY,
    last_used_at INTEGER NOT NULL
  );
  CREATE TABLE IF NOT EXISTS project_aliases (
    project_key TEXT PRIMARY KEY,
    alias TEXT NOT NULL,
    updated_at INTEGER NOT NULL
  );
  `,
];

export const SCHEMA_VERSION = MIGRATIONS.length;

export function migrate(db) {
  const current = db.prepare('PRAGMA user_version').get().user_version;
  if (current > SCHEMA_VERSION) {
    throw new Error(`[PRECONDITION_FAILED] local db schema v${current} is newer than this build (v${SCHEMA_VERSION})`);
  }
  for (let v = current; v < SCHEMA_VERSION; v++) {
    db.exec('BEGIN');
    try {
      db.exec(MIGRATIONS[v]);
      db.exec(`PRAGMA user_version = ${v + 1}`);
      db.exec('COMMIT');
    } catch (err) {
      db.exec('ROLLBACK');
      throw err;
    }
  }
}
