//! 本地库（会话 / 消息 / 最近目录 / 项目别名 / 右侧栏页签）：Rust 持有 SQLite 连接并直接应答 `local-db:*`，
//! 逻辑逐段移植自 sidecar/src/local-db/{index,persist,right-sidebar-tabs,mapper,schema}.mjs。
//! 唯一的锁在 `Inner`：连接与落库器状态一起持有，保证“落库顺序 = 事件顺序”。
//! `messages:view` 的投影在 history_view/（上游 projectHistoryView 的移植）。

pub mod mapper;
pub mod persist;
pub mod schema;
pub mod sql;
mod messages;
mod sessions;
mod tabs;
#[cfg(test)]
mod tests;

use mapper::Row;
use rusqlite::Connection;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

pub fn fail<T>(code: &str, msg: impl AsRef<str>) -> Result<T, String> {
    Err(format!("[{code}] {}", msg.as_ref()))
}

/// 时钟 / 随机数 / 本地日期（测试里固定）。
pub trait Env: Send + Sync + 'static {
    fn now_ms(&self) -> i64;
    fn uuid(&self) -> String;
    fn random_bytes(&self, n: usize) -> Vec<u8>;
    /// 本地日历日期 (年, 月, 日)。
    fn local_date(&self, at_ms: i64) -> (i32, u32, u32);
}

pub struct SystemEnv;
impl Env for SystemEnv {
    fn now_ms(&self) -> i64 {
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0)
    }
    fn uuid(&self) -> String {
        uuid::Uuid::new_v4().to_string()
    }
    fn random_bytes(&self, n: usize) -> Vec<u8> {
        let mut out = Vec::with_capacity(n);
        while out.len() < n {
            out.extend_from_slice(uuid::Uuid::new_v4().as_bytes());
        }
        out.truncate(n);
        out
    }
    fn local_date(&self, at_ms: i64) -> (i32, u32, u32) {
        use chrono::Datelike;
        let d = chrono::DateTime::from_timestamp_millis(at_ms).unwrap_or_default().with_timezone(&chrono::Local);
        (d.year(), d.month(), d.day())
    }
}

/// 推给 renderer（同时转给设备互联的手机端），对应 sidecar 的 emitToRenderer。
pub trait Push: Send + Sync + 'static {
    fn push(&self, channel: &str, payload: Value);
}

pub struct Inner {
    pub(crate) conn: Connection,
    pub(crate) persister: persist::Persister,
}

pub struct LocalDb {
    pub(crate) inner: Mutex<Inner>,
    pub file: PathBuf,
    pub(crate) data_dir: PathBuf,
    pub(crate) boot_at: i64,
    pub(crate) env: Arc<dyn Env>,
    pub(crate) out: Arc<dyn Push>,
}

pub const VISIBLE: &str = "m.session_id = s.id AND m.rewind_at IS NULL AND (s.cleared_at IS NULL OR m.created_at > s.cleared_at)";

pub fn session_select() -> String {
    let latest = format!("FROM messages m WHERE {VISIBLE} AND m.role IN ('user','assistant') ORDER BY m.created_at DESC, m.rowid DESC LIMIT 1");
    format!(
        "SELECT s.*, (SELECT COUNT(*) FROM messages m WHERE {VISIBLE}) AS message_count, (SELECT substr(m.content, 1, {}) {latest}) AS latest_content, (SELECT m.role {latest}) AS latest_role FROM sessions s",
        mapper::PREVIEW_EXTRACT_CHARS
    )
}

/// 会话行 → renderer 的 Session（preview 取最新 user / assistant 消息）。
pub(crate) fn to_session(row: &Row, boot_at: i64) -> Value {
    let mut m = mapper::session_to_camel(row, boot_at);
    let p = mapper::extract_message_preview(row.get("latest_content").and_then(Value::as_str), row.get("latest_role").and_then(Value::as_str));
    m.insert("preview".into(), p.map(Value::String).unwrap_or(Value::Null));
    Value::Object(m)
}

impl LocalDb {
    /// 打开 `<dataDir>/cindy-tauri-<ownerId>.db` 并迁移。
    pub fn open(data_dir: &Path, owner_id: &str, env: Arc<dyn Env>, out: Arc<dyn Push>) -> Result<Self, String> {
        let ok = !owner_id.is_empty() && owner_id.len() <= 64 && owner_id.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-'));
        if !ok {
            return fail("INVALID_PARAMS", "invalid ownerId");
        }
        std::fs::create_dir_all(data_dir).map_err(|e| e.to_string())?;
        let file = data_dir.join(format!("cindy-tauri-{owner_id}.db"));
        let conn = Connection::open(&file).map_err(|e| e.to_string())?;
        conn.execute_batch("PRAGMA journal_mode = WAL; PRAGMA busy_timeout = 5000; PRAGMA foreign_keys = ON;").map_err(|e| e.to_string())?;
        schema::migrate(&conn)?;
        let boot_at = env.now_ms();
        Ok(Self { inner: Mutex::new(Inner { conn, persister: persist::Persister::default() }), file, data_dir: data_dir.to_path_buf(), boot_at, env, out })
    }

    pub fn owns(channel: &str) -> bool {
        channel.starts_with("local-db:right-sidebar-tabs:") || OWNED.contains(&channel)
    }

    /// 归 Rust 的通道清单（host.activate 时告诉 sidecar，手机端的 invoke 据此转回来）。
    pub fn owned_channels() -> Vec<&'static str> {
        let mut v: Vec<&'static str> = OWNED.to_vec();
        v.extend(TAB_CHANNELS);
        v
    }

    /// 应答一条 `local-db:*` / `maker:auto-title` 调用。
    pub fn invoke(&self, channel: &str, args: &[Value]) -> Result<Value, String> {
        let args: Vec<Value> = args.iter().map(|v| if crate::voice_settings::is_undef(v) { Value::Null } else { v.clone() }).collect();
        let a = |i: usize| args.get(i).cloned().unwrap_or(Value::Null);
        let mut g = self.inner.lock().unwrap();
        let g = &mut *g;
        if let Some(rest) = channel.strip_prefix("local-db:right-sidebar-tabs:") {
            return tabs::invoke(self, &g.conn, rest, &a(0));
        }
        match channel {
            "local-db:sessions:list" => sessions::list_value(self, &g.conn, &a(0), &a(1), &a(2)),
            "local-db:sessions:create" => sessions::create_value(self, &g.conn, &a(0)),
            "local-db:sessions:get" => sessions::get_value(self, &g.conn, &a(0)),
            "local-db:sessions:get-many" => sessions::get_many(self, &g.conn, &a(0)),
            "local-db:sessions:restore-if-archived" => sessions::restore_if_archived(self, &g.conn, &a(0)),
            "local-db:sessions:update" => sessions::update_value(self, &g.conn, &a(0), &a(1)),
            "maker:auto-title" => sessions::auto_title(self, &g.conn, &a(0)),
            "local-db:sessions:touchUserSend" => sessions::touch_user_send(self, &g.conn, &a(0), &a(1)),
            "local-db:sessions:set-pinned-card-summaries" => sessions::set_pinned_card_summaries(&a(0)),
            "local-db:sessions:interrupted-pending" => sessions::interrupted_pending(self, &g.conn),
            "local-db:sessions:interrupted-resume-candidates" => sessions::interrupted_candidates(self, &g.conn),
            "local-db:sessions:error-tail-pending" => sessions::error_tail_pending(&g.conn),
            "local-db:sessions:ack-interrupted" => sessions::ack_interrupted(self, &g.conn, &a(0)),
            "local-db:sessions:dismiss-pending-alerts" => sessions::dismiss_pending_alerts(self, &g.conn, &a(0)),
            "local-db:messages:list" => messages::list_value(self, &g.conn, &a(0), &a(1)),
            "local-db:messages:work-details" => messages::work_details(&g.conn, &a(0), &a(1)),
            "local-db:messages:create" => messages::create(self, &g.conn, &a(0), &a(1)),
            "local-db:messages:updateContent" => messages::update_content_value(self, &g.conn, &a(0), &a(1), &a(2)),
            "local-db:messages:dismiss-error" => messages::dismiss_error(self, &g.conn, &a(0), &a(1)),
            "local-db:messages:around" => messages::around(self, &g.conn, &a(0), "id", &a(1), &a(2)),
            "local-db:messages:around-client-id" => messages::around(self, &g.conn, &a(0), "client_id", &a(1), &a(2)),
            "local-db:messages:estimatedSessionValue" => Ok(json!({ "totalMoney": { "amount": 0, "currency": "USD", "approximate": false, "kind": "actual-cost" }, "turns": [] })),
            "local-db:recent-workdirs:list" => messages::recent_list(&g.conn),
            "local-db:recent-workdirs:remove" => messages::recent_remove(self, &g.conn, &a(0)),
            "local-db:project-aliases:list" => messages::aliases_list(&g.conn),
            "local-db:project-aliases:set" => messages::aliases_set(self, &g.conn, &a(0)),
            "local-db:project-aliases:delete" => messages::aliases_delete(self, &g.conn, &a(0)),
            other => fail("NOT_IMPLEMENTED", format!("{other} is not a local-db channel")),
        }
    }

    // ── Rust 会话宿主的落库入口（原 host.persist-* / host.upsert-session）──
    pub fn persist_maker_event(&self, sid: &str, event: &Value) -> Option<String> {
        let mut g = self.inner.lock().unwrap();
        let Inner { conn, persister } = &mut *g;
        let mut st = messages::StoreImpl { db: self, conn };
        persister.persist_maker_event(&mut st, sid, event)
    }
    pub fn persist_user_message(&self, sid: &str, client_id: &str, content: &Value) -> Option<String> {
        let g = self.inner.lock().unwrap();
        messages::persist_user_message(self, &g.conn, sid, client_id, content)
    }
    pub fn persist_interaction_request(&self, sid: &str, req: &Value) -> Option<String> {
        let mut g = self.inner.lock().unwrap();
        let Inner { conn, persister } = &mut *g;
        let mut st = messages::StoreImpl { db: self, conn };
        persister.persist_interaction_request(&mut st, sid, req)
    }
    pub fn persist_interaction_resolved(&self, sid: &str, request_id: &str, decision: &Value) -> Option<Value> {
        let mut g = self.inner.lock().unwrap();
        let Inner { conn, persister } = &mut *g;
        let mut st = messages::StoreImpl { db: self, conn };
        persister.persist_interaction_resolved(&mut st, sid, request_id, decision)
    }
    pub fn upsert_session(&self, row: &Value) -> Result<(), String> {
        let g = self.inner.lock().unwrap();
        sessions::upsert(self, &g.conn, row)
    }
    pub fn get_session(&self, id: &str) -> Option<Value> {
        let g = self.inner.lock().unwrap();
        sessions::get_row(&g.conn, id).ok().flatten().map(|r| to_session(&r, self.boot_at))
    }
    /// 会话行上的运行时字段补丁（host.upsert-session 的 effort / fastMode、sdkSessionId 同步）。
    pub fn update_session(&self, id: &str, patch: &Value) -> Result<Value, String> {
        let g = self.inner.lock().unwrap();
        sessions::update(self, &g.conn, &json!(id), patch)
    }
    /// messages:view 的分页（sidecar 负责 projectHistoryView 投影）。
    pub fn view_page(&self, sid: &Value, opts: &Value) -> Result<Value, String> {
        let g = self.inner.lock().unwrap();
        messages::view_page(&g.conn, sid, opts)
    }
    pub fn session_muted(&self, id: &str) -> bool {
        self.get_session(id).map(|s| s["notificationsMuted"] == json!(true)).unwrap_or(false)
    }
}

const TAB_CHANNELS: [&str; 6] = [
    "local-db:right-sidebar-tabs:list", "local-db:right-sidebar-tabs:ensure-singleton", "local-db:right-sidebar-tabs:upsert",
    "local-db:right-sidebar-tabs:close", "local-db:right-sidebar-tabs:setActive", "local-db:right-sidebar-tabs:reorder",
];

const OWNED: &[&str] = &[
    "local-db:sessions:list", "local-db:sessions:create", "local-db:sessions:get", "local-db:sessions:get-many",
    "local-db:sessions:restore-if-archived", "local-db:sessions:update", "maker:auto-title", "local-db:sessions:touchUserSend",
    "local-db:sessions:set-pinned-card-summaries", "local-db:sessions:interrupted-pending",
    "local-db:sessions:interrupted-resume-candidates", "local-db:sessions:error-tail-pending", "local-db:sessions:ack-interrupted",
    "local-db:sessions:dismiss-pending-alerts", "local-db:messages:list", "local-db:messages:create",
    "local-db:messages:updateContent", "local-db:messages:dismiss-error", "local-db:messages:around",
    "local-db:messages:around-client-id", "local-db:messages:estimatedSessionValue", "local-db:recent-workdirs:list",
    "local-db:recent-workdirs:remove", "local-db:project-aliases:list", "local-db:project-aliases:set",
    "local-db:project-aliases:delete", "local-db:messages:work-details",
];
