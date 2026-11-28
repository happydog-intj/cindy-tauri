//! 本地库表结构与迁移，对应 sidecar/src/local-db/schema.mjs（迁移只往末尾追加，不改已发布条目）。

use rusqlite::Connection;

const MIGRATIONS: [&str; 4] = [
    r#"
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
  "#,
    r#"
  CREATE TABLE IF NOT EXISTS right_sidebar_tabs (
    id TEXT PRIMARY KEY,
    session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
    kind TEXT NOT NULL,
    position INTEGER NOT NULL,
    is_active INTEGER NOT NULL DEFAULT 0,
    state TEXT NOT NULL DEFAULT '{}',
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
  );
  CREATE INDEX IF NOT EXISTS right_sidebar_tabs_session_idx ON right_sidebar_tabs(session_id, position);
  CREATE UNIQUE INDEX IF NOT EXISTS right_sidebar_tabs_subagents_singleton_idx
    ON right_sidebar_tabs(session_id) WHERE kind = 'subagents';
  CREATE UNIQUE INDEX IF NOT EXISTS right_sidebar_tabs_bot_artifacts_singleton_idx
    ON right_sidebar_tabs(session_id) WHERE kind = 'bot-artifacts';
  "#,
    r#"
  CREATE TABLE IF NOT EXISTS session_goals (
    session_id TEXT PRIMARY KEY REFERENCES sessions(id) ON DELETE CASCADE,
    objective TEXT NOT NULL,
    status TEXT NOT NULL DEFAULT 'active',
    budget_tokens INTEGER,
    max_turns INTEGER,
    no_progress_limit INTEGER,
    turns_used INTEGER NOT NULL DEFAULT 0,
    tokens_used INTEGER NOT NULL DEFAULT 0,
    no_progress_streak INTEGER NOT NULL DEFAULT 0,
    usage_reset_at INTEGER,
    last_reason TEXT,
    agent_kind TEXT NOT NULL,
    started_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
  );
  "#,
    // 附加只读目录与附加可写目录（JSON 数组）。
    r#"
  ALTER TABLE sessions ADD COLUMN extra_dirs TEXT NOT NULL DEFAULT '[]';
  ALTER TABLE sessions ADD COLUMN writable_dirs TEXT NOT NULL DEFAULT '[]';
  "#,
];

/// 迁移 4 加的两列：从 Electron 导入来的库可能已经有了，重复 ADD COLUMN 会报错，已有的跳过。
fn migration_sql(db: &Connection, v: usize) -> String {
    if v != 3 {
        return MIGRATIONS[v].to_string();
    }
    let has = |col: &str| {
        db.prepare("SELECT 1 FROM pragma_table_info('sessions') WHERE name = ?1")
            .and_then(|mut s| s.exists([col]))
            .unwrap_or(false)
    };
    ["extra_dirs", "writable_dirs"]
        .iter()
        .filter(|c| !has(c))
        .map(|c| format!("ALTER TABLE sessions ADD COLUMN {c} TEXT NOT NULL DEFAULT '[]';"))
        .collect::<Vec<_>>()
        .join("\n")
}

pub const SCHEMA_VERSION: i64 = MIGRATIONS.len() as i64;

pub fn migrate(db: &Connection) -> Result<(), String> {
    let current: i64 = db.query_row("PRAGMA user_version", [], |r| r.get(0)).map_err(|e| e.to_string())?;
    if current > SCHEMA_VERSION {
        return Err(format!("[PRECONDITION_FAILED] local db schema v{current} is newer than this build (v{SCHEMA_VERSION})"));
    }
    for v in current..SCHEMA_VERSION {
        db.execute_batch("BEGIN").map_err(|e| e.to_string())?;
        let r = db
            .execute_batch(&migration_sql(db, v as usize))
            .and_then(|_| db.execute_batch(&format!("PRAGMA user_version = {}", v + 1)))
            .and_then(|_| db.execute_batch("COMMIT"));
        if let Err(e) = r {
            let _ = db.execute_batch("ROLLBACK");
            return Err(e.to_string());
        }
    }
    Ok(())
}
