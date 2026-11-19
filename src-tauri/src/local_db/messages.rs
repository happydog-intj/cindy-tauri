//! 消息读写 + 落库原语 + 最近目录 / 项目别名，对应 index.mjs 的 store / listMessages / messages:* / recent-workdirs / project-aliases。

use super::mapper::{as_integer, message_to_camel, parse_date, slice16, utf16_len, iso, Row};
use super::persist::{NewRow, Store};
use super::sessions::{get_row};
use super::{fail, sql, to_session, LocalDb};
use rusqlite::Connection;
use serde_json::{json, Map, Value};

fn require_string(v: &Value, name: &str) -> Result<String, String> {
    match v.as_str().filter(|s| !s.is_empty()) {
        Some(s) => Ok(s.to_string()),
        None => fail("INVALID_PARAMS", format!("{name} must be a non-empty string")),
    }
}

pub fn get_message_row(conn: &Connection, sid: &str, client_id: &str) -> Result<Option<Row>, String> {
    sql::get(conn, "SELECT rowid, * FROM messages WHERE session_id = ? AND client_id = ?", &[json!(sid), json!(client_id)])
}

/// JSON.stringify(x ?? null)。
fn stringify(v: &Value) -> String {
    serde_json::to_string(v).unwrap_or_else(|_| "null".into())
}

pub struct StoreImpl<'a> {
    pub db: &'a LocalDb,
    pub conn: &'a Connection,
}

impl StoreImpl<'_> {
    fn patched_preview(&self, sid: &str) {
        if let Ok(Some(row)) = get_row(self.conn, sid) {
            let preview = to_session(&row, self.db.boot_at)["preview"].clone();
            self.db.out.push("local-db:sessions:patched", json!({ "sessionId": sid, "patch": { "preview": preview } }));
        }
    }
}

impl Store for StoreImpl<'_> {
    fn insert(&mut self, sid: &str, row: NewRow) -> Option<Value> {
        let sess = sql::get(self.conn, "SELECT agent_kind FROM sessions WHERE id = ?", &[json!(sid)]).ok().flatten()?; // 会话行未建：跳过（FK）
        if let Ok(Some(existing)) = get_message_row(self.conn, sid, &row.client_id) {
            return Some(message_to_camel(&existing));
        }
        let meta = row.agent_meta.as_ref().map(stringify).map(Value::String).unwrap_or(Value::Null);
        let r = sql::run(
            self.conn,
            "INSERT INTO messages (id, client_id, session_id, role, content, tool_use_id, agent_meta, agent_kind, created_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
            &[
                json!(self.db.env.uuid()), json!(row.client_id), json!(sid), json!(row.role), json!(stringify(&row.content)),
                row.tool_use_id.clone().map(Value::String).unwrap_or(Value::Null), meta, sess.get("agent_kind").cloned().unwrap_or(Value::Null), json!(row.created_at),
            ],
        );
        if let Err(e) = r {
            eprintln!("[local-db] insert message failed: {e}");
            return None;
        }
        let now = self.db.env.now_ms();
        let _ = sql::run(self.conn, "UPDATE sessions SET last_activity_at = ?, updated_at = ? WHERE id = ?", &[json!(now), json!(now), json!(sid)]);
        get_message_row(self.conn, sid, &row.client_id).ok().flatten().map(|r| message_to_camel(&r))
    }
    fn update_content(&mut self, sid: &str, client_id: &str, content: &Value) -> Option<Value> {
        let n = sql::run(self.conn, "UPDATE messages SET content = ? WHERE session_id = ? AND client_id = ?", &[json!(stringify(content)), json!(sid), json!(client_id)]).ok()?;
        if n == 0 {
            return None;
        }
        get_message_row(self.conn, sid, client_id).ok().flatten().map(|r| message_to_camel(&r))
    }
    fn latest_created_at(&mut self, sid: &str) -> Option<i64> {
        sql::get(self.conn, "SELECT MAX(created_at) AS t FROM messages WHERE session_id = ?", &[json!(sid)]).ok().flatten().and_then(|r| r["t"].as_i64())
    }
    fn provider_id_of(&mut self, sid: &str) -> Option<String> {
        sql::get(self.conn, "SELECT provider_id FROM sessions WHERE id = ?", &[json!(sid)]).ok().flatten().and_then(|r| r["provider_id"].as_str().map(String::from))
    }
    fn note_turn_ended(&mut self, sid: &str, at: i64) {
        let _ = sql::run(self.conn, "UPDATE sessions SET last_turn_ended_at = ? WHERE id = ?", &[json!(at), json!(sid)]);
        self.db.out.push("local-db:sessions:patched", json!({ "sessionId": sid, "patch": { "lastTurnEndedAt": at } }));
    }
    fn emit_error(&mut self, sid: &str, persist_id: &str) {
        self.db.out.push("local-db:session:error-persisted", json!({ "sessionId": sid, "clientId": persist_id }));
    }
    fn emit_row(&mut self, sid: &str, message: &Value) {
        self.db.out.push("local-db:messages:created", json!({ "sessionId": sid, "message": message }));
        if matches!(message["role"].as_str(), Some("user") | Some("assistant")) {
            self.patched_preview(sid);
        }
    }
    fn now_ms(&self) -> i64 {
        self.db.env.now_ms()
    }
    fn random_bytes(&mut self, n: usize) -> Vec<u8> {
        self.db.env.random_bytes(n)
    }
}

/// desktop agent-input-coordinator 在 onAccepted 写 user 行，同时推进 user_send_at / active_turn_started_at。
pub fn persist_user_message(db: &LocalDb, conn: &Connection, sid: &str, client_id: &str, content: &Value) -> Option<String> {
    if client_id.is_empty() {
        return None;
    }
    let now = db.env.now_ms();
    let content = if content.is_null() { json!("") } else { content.clone() };
    let mut st = StoreImpl { db, conn };
    let msg = st.insert(sid, NewRow { client_id: client_id.into(), role: "user".into(), content, tool_use_id: None, agent_meta: None, created_at: now })?;
    let _ = sql::run(conn, "UPDATE sessions SET user_send_at = ?, active_turn_started_at = ? WHERE id = ?", &[json!(now), json!(now), json!(sid)]);
    st.emit_row(sid, &msg);
    msg["clientId"].as_str().map(String::from)
}

fn cleared_at_of(conn: &Connection, sid: &str) -> Result<Option<i64>, String> {
    match sql::get(conn, "SELECT cleared_at FROM sessions WHERE id = ?", &[json!(sid)])? {
        None => fail("NOT_FOUND", "History is unavailable"),
        Some(r) => Ok(r["cleared_at"].as_i64()),
    }
}

/// messages:list：倒序取 limit 条（游标 before=消息 id / beforeTs），返回正序。
pub fn list_messages(conn: &Connection, sid: &str, opts: &Value) -> Result<Vec<Value>, String> {
    let cleared = cleared_at_of(conn, sid)?;
    let limit = opts.get("limit").and_then(as_integer).unwrap_or(100).clamp(1, 1000);
    let mut where_ = vec!["session_id = ?".to_string(), "rewind_at IS NULL".to_string()];
    let mut vals = vec![json!(sid)];
    if let Some(c) = cleared {
        where_.push("created_at > ?".into());
        vals.push(json!(c));
    }
    if let Some(b) = opts.get("before").and_then(Value::as_str).filter(|s| !s.is_empty()) {
        let a = sql::get(conn, "SELECT rowid, created_at FROM messages WHERE session_id = ? AND id = ?", &[json!(sid), json!(b)])?;
        let Some(a) = a else { return fail("NOT_FOUND", "History range changed") };
        where_.push("(created_at < ? OR (created_at = ? AND rowid < ?))".into());
        vals.extend([a["created_at"].clone(), a["created_at"].clone(), a["rowid"].clone()]);
    } else if let Some(ts) = opts.get("beforeTs").filter(|v| v.is_number()) {
        where_.push("created_at < ?".into());
        vals.push(ts.clone());
    }
    vals.push(json!(limit));
    let mut rows = sql::all(conn, &format!("SELECT rowid, * FROM messages WHERE {} ORDER BY created_at DESC, rowid DESC LIMIT ?", where_.join(" AND ")), &vals)?;
    rows.reverse();
    Ok(rows.iter().map(message_to_camel).collect())
}

pub fn list_value(_db: &LocalDb, conn: &Connection, sid: &Value, opts: &Value) -> Result<Value, String> {
    let sid = require_string(sid, "sessionId")?;
    let opts = if opts.is_null() { json!({}) } else { opts.clone() };
    Ok(Value::Array(list_messages(conn, &sid, &opts)?))
}

/// 一页最多读这么多行再交给 projectHistoryView 分组（desktop 扫描预算 2000 行）。
const HISTORY_PAGE_ROWS: usize = 400;

/// messages:view 的分页部分（投影由 sidecar 的 projectHistoryView 做）：
/// 返回 `{ page, hasMore, nextCursor, before }`，page 是正序消息行。
pub fn view_page(conn: &Connection, sid: &Value, opts: &Value) -> Result<Value, String> {
    let sid = require_string(sid, "sessionId")?;
    let before = opts.get("before").and_then(Value::as_str).filter(|s| !s.is_empty()).map(String::from);
    let mut o = json!({ "limit": HISTORY_PAGE_ROWS + 1 });
    if let Some(b) = &before {
        o["before"] = json!(b);
    }
    let rows = list_messages(conn, &sid, &o)?;
    let has_more = rows.len() > HISTORY_PAGE_ROWS;
    let mut page: Vec<Value> = if has_more { rows[1..].to_vec() } else { rows };
    // 不在 turn 中间切页：从第一条 user 行开始，更早的留给下一页。
    if has_more {
        if let Some(i) = page.iter().position(|r| r["role"] == "user").filter(|i| *i > 0) {
            page = page[i..].to_vec();
        }
    }
    let next = if has_more && !page.is_empty() { page[0]["id"].clone() } else { Value::Null };
    Ok(json!({ "page": page, "hasMore": has_more, "nextCursor": next, "before": before }))
}

/// messages:work-details：ref 的首尾消息（含）之间的行。
pub fn work_details(conn: &Connection, sid: &Value, r: &Value) -> Result<Value, String> {
    let sid = require_string(sid, "sessionId")?;
    cleared_at_of(conn, &sid)?;
    let pos = |id: &str| sql::get(conn, "SELECT rowid, created_at FROM messages WHERE session_id = ? AND id = ?", &[json!(sid), json!(id)]);
    let first = pos(&require_string(r.get("firstMessageId").unwrap_or(&Value::Null), "firstMessageId")?)?;
    let last = pos(&require_string(r.get("lastMessageId").unwrap_or(&Value::Null), "lastMessageId")?)?;
    let (Some(first), Some(last)) = (first, last) else { return fail("NOT_FOUND", "History range changed") };
    let rows = sql::all(
        conn,
        "SELECT rowid, * FROM messages WHERE session_id = ? AND rewind_at IS NULL AND (created_at > ? OR (created_at = ? AND rowid >= ?)) AND (created_at < ? OR (created_at = ? AND rowid <= ?)) ORDER BY created_at ASC, rowid ASC LIMIT 2000",
        &[json!(sid), first["created_at"].clone(), first["created_at"].clone(), first["rowid"].clone(), last["created_at"].clone(), last["created_at"].clone(), last["rowid"].clone()],
    )?;
    Ok(json!({ "version": 1, "messages": rows.iter().map(message_to_camel).collect::<Vec<_>>(), "hasMore": false, "nextCursor": null }))
}

pub fn create(db: &LocalDb, conn: &Connection, sid: &Value, body: &Value) -> Result<Value, String> {
    let sid = require_string(sid, "sessionId")?;
    let Some(b) = body.as_object().filter(|_| !body.is_null()) else { return fail("INVALID_PARAMS", "body required") };
    let client_id = require_string(b.get("clientId").unwrap_or(&Value::Null), "clientId")?;
    let role = require_string(b.get("role").unwrap_or(&Value::Null), "role")?;
    let at = match b.get("createdAt") {
        Some(Value::String(s)) => parse_date(s),
        _ => Some(db.env.now_ms()),
    }
    .unwrap_or_else(|| db.env.now_ms());
    let meta = b.get("agentMeta").filter(|v| super::mapper::truthy(v)).cloned();
    let tool_use_id = b.get("toolUseId").and_then(Value::as_str).map(String::from);
    let mut st = StoreImpl { db, conn };
    let msg = st.insert(&sid, NewRow { client_id, role, content: b.get("content").cloned().unwrap_or(Value::Null), tool_use_id, agent_meta: meta, created_at: at });
    let Some(msg) = msg else { return fail("NOT_FOUND", "Session 不存在") };
    st.emit_row(&sid, &msg);
    Ok(msg)
}

pub fn update_content_value(db: &LocalDb, conn: &Connection, sid: &Value, client_id: &Value, content: &Value) -> Result<Value, String> {
    let sid = require_string(sid, "sessionId")?;
    let cid = require_string(client_id, "clientId")?;
    let mut st = StoreImpl { db, conn };
    st.update_content(&sid, &cid, content).ok_or_else(|| "[NOT_FOUND] Message 不存在".to_string())
}

pub fn dismiss_error(db: &LocalDb, conn: &Connection, sid: &Value, client_id: &Value) -> Result<Value, String> {
    let sid = require_string(sid, "sessionId")?;
    let cid = require_string(client_id, "clientId")?;
    let row = get_message_row(conn, &sid, &cid)?;
    let Some(row) = row.filter(|r| r["role"] == "error") else { return fail("NOT_FOUND", "Message 不存在") };
    let content = serde_json::from_str::<Value>(row["content"].as_str().unwrap_or("")).ok();
    let mut obj: Map<String, Value> = match content {
        Some(Value::Object(o)) => o,
        _ => Map::new(),
    };
    obj.insert("dismissed".into(), json!(true));
    let mut st = StoreImpl { db, conn };
    Ok(st.update_content(&sid, &cid, &Value::Object(obj)).unwrap_or(Value::Null))
}

pub fn around(_db: &LocalDb, conn: &Connection, sid: &Value, col: &str, value: &Value, opts: &Value) -> Result<Value, String> {
    let sid = require_string(sid, "sessionId")?;
    let value = require_string(value, if col == "id" { "messageId" } else { "clientId" })?;
    let radius = opts.get("radius").and_then(as_integer).unwrap_or(20).clamp(1, 200);
    let a = sql::get(conn, &format!("SELECT rowid, created_at FROM messages WHERE session_id = ? AND {col} = ?"), &[json!(sid), json!(value)])?;
    let Some(a) = a else { return Ok(json!([])) };
    let mut older = sql::all(
        conn,
        "SELECT rowid, * FROM messages WHERE session_id = ? AND rewind_at IS NULL AND (created_at < ? OR (created_at = ? AND rowid <= ?)) ORDER BY created_at DESC, rowid DESC LIMIT ?",
        &[json!(sid), a["created_at"].clone(), a["created_at"].clone(), a["rowid"].clone(), json!(radius + 1)],
    )?;
    older.reverse();
    let newer = sql::all(
        conn,
        "SELECT rowid, * FROM messages WHERE session_id = ? AND rewind_at IS NULL AND (created_at > ? OR (created_at = ? AND rowid > ?)) ORDER BY created_at ASC, rowid ASC LIMIT ?",
        &[json!(sid), a["created_at"].clone(), a["created_at"].clone(), a["rowid"].clone(), json!(radius)],
    )?;
    Ok(Value::Array(older.iter().chain(newer.iter()).map(message_to_camel).collect()))
}

pub fn recent_list(conn: &Connection) -> Result<Value, String> {
    let rows = sql::all(conn, "SELECT path, last_used_at FROM recent_workdirs ORDER BY last_used_at DESC LIMIT 50", &[])?;
    Ok(Value::Array(rows.iter().map(|r| json!({ "path": r["path"], "lastUsedAt": r["last_used_at"].as_i64().and_then(iso) })).collect()))
}

pub fn recent_remove(db: &LocalDb, conn: &Connection, input: &Value) -> Result<Value, String> {
    let p = if input.is_string() { input.clone() } else { input.get("path").cloned().unwrap_or(Value::Null) };
    let p = require_string(&p, "path")?;
    sql::run(conn, "DELETE FROM recent_workdirs WHERE path = ?", &[json!(p)])?;
    db.out.push("local-db:recent-workdirs:changed", json!({}));
    Ok(json!({ "ok": true }))
}

pub fn aliases_list(conn: &Connection) -> Result<Value, String> {
    let rows = sql::all(conn, "SELECT project_key, alias, updated_at FROM project_aliases", &[])?;
    Ok(Value::Array(rows.iter().map(|r| json!({ "projectKey": r["project_key"], "alias": r["alias"], "updatedAt": r["updated_at"].as_i64().and_then(iso) })).collect()))
}

pub fn aliases_set(db: &LocalDb, conn: &Connection, input: &Value) -> Result<Value, String> {
    let key = require_string(input.get("projectKey").unwrap_or(&Value::Null), "projectKey")?;
    let alias = require_string(input.get("alias").unwrap_or(&Value::Null), "alias")?;
    if utf16_len(&alias) > 200 {
        return fail("INVALID_PARAMS", "alias too long");
    }
    sql::run(
        conn,
        "INSERT INTO project_aliases (project_key, alias, updated_at) VALUES (?, ?, ?) ON CONFLICT(project_key) DO UPDATE SET alias = excluded.alias, updated_at = excluded.updated_at",
        &[json!(key), json!(alias), json!(db.env.now_ms())],
    )?;
    db.out.push("local-db:project-aliases:changed", json!({}));
    Ok(json!({ "ok": true }))
}

pub fn aliases_delete(db: &LocalDb, conn: &Connection, key: &Value) -> Result<Value, String> {
    let key = require_string(key, "projectKey")?;
    sql::run(conn, "DELETE FROM project_aliases WHERE project_key = ?", &[json!(key)])?;
    db.out.push("local-db:project-aliases:changed", json!({}));
    Ok(json!({ "ok": true }))
}

#[allow(dead_code)]
fn _keep(s: &str) -> String {
    slice16(s, 1)
}
