//! 右侧栏页签，对应 sidecar/src/local-db/right-sidebar-tabs.mjs。
//! desktop 的 maker:right-sidebar-tabs:changed 只发给“其它” renderer；Tauri 只有主窗口一个 renderer，所以不广播。

use super::mapper::{as_integer, Row};
use super::{fail, sql, LocalDb};
use rusqlite::Connection;
use serde_json::{json, Value};

pub const MAX_TABS_PER_SESSION: i64 = 20;
/// shared/rightSidebarTabState.ts MAX_STATE_JSON_BYTES
pub const MAX_STATE_JSON_BYTES: usize = 16 * 1024;

fn require_object<'a>(v: &'a Value, name: &str) -> Result<&'a serde_json::Map<String, Value>, String> {
    match v.as_object() {
        Some(o) => Ok(o),
        None => fail("INVALID_PARAMS", format!("{name} must be an object")),
    }
}
fn require_string(v: Option<&Value>, name: &str) -> Result<String, String> {
    match v.and_then(Value::as_str).filter(|s| !s.is_empty()) {
        Some(s) => Ok(s.to_string()),
        None => fail("INVALID_PARAMS", format!("{name} must be a non-empty string")),
    }
}
fn require_int(v: Option<&Value>, name: &str) -> Result<i64, String> {
    match v.and_then(as_integer).filter(|n| *n >= 0) {
        Some(n) => Ok(n),
        None => fail("INVALID_PARAMS", format!("{name} must be a non-negative integer")),
    }
}

fn row_to_tab(row: &Row) -> Value {
    let state = row["state"].as_str().and_then(|t| serde_json::from_str::<Value>(t).ok()).unwrap_or(Value::Null);
    json!({
        "id": row["id"], "sessionId": row["session_id"], "kind": row["kind"], "position": row["position"], "state": state,
        "isActive": row["is_active"] == json!(1), "createdAt": row["created_at"], "updatedAt": row["updated_at"],
    })
}

fn serialize_state(v: Option<&Value>) -> Result<String, String> {
    let Some(v) = v.filter(|v| !crate::voice_settings::is_undef(v)) else { return Ok("{}".into()) };
    let json = serde_json::to_string(v).map_err(|_| "[INVALID_PARAMS] tab state must be JSON-serializable".to_string())?;
    if json.len() > MAX_STATE_JSON_BYTES {
        return fail("RIGHT_SIDEBAR_STATE_TOO_LARGE", format!("tab state JSON too large: {} bytes (limit {MAX_STATE_JSON_BYTES})", json.len()));
    }
    Ok(json)
}

fn too_many<T>(sid: &str) -> Result<T, String> {
    fail("RIGHT_SIDEBAR_TOO_MANY_TABS", format!("session {sid} already has {MAX_TABS_PER_SESSION} tabs (limit reached)"))
}

fn session_exists(conn: &Connection, sid: &str) -> Result<bool, String> {
    Ok(sql::get(conn, "SELECT 1 AS x FROM sessions WHERE id = ?", &[json!(sid)])?.is_some())
}

fn tx<T>(conn: &Connection, f: impl FnOnce() -> Result<T, String>) -> Result<T, String> {
    conn.execute_batch("BEGIN IMMEDIATE").map_err(|e| e.to_string())?;
    match f() {
        Ok(v) => {
            conn.execute_batch("COMMIT").map_err(|e| e.to_string())?;
            Ok(v)
        }
        Err(e) => {
            let _ = conn.execute_batch("ROLLBACK");
            Err(e)
        }
    }
}

pub fn invoke(db: &LocalDb, conn: &Connection, op: &str, payload: &Value) -> Result<Value, String> {
    match op {
        "list" => {
            let o = require_object(payload, "list payload")?;
            let sid = require_string(o.get("sessionId"), "sessionId")?;
            // 会话还没落库（新建对话的草稿）：返回空且不可持久化，renderer 只放内存。
            if !session_exists(conn, &sid)? {
                return Ok(json!({ "tabs": [], "activeTabId": null, "persistable": false }));
            }
            let rows = sql::all(conn, "SELECT * FROM right_sidebar_tabs WHERE session_id = ? ORDER BY position ASC", &[json!(sid)])?;
            let active = rows.iter().find(|r| r["is_active"] == json!(1)).map(|r| r["id"].clone()).unwrap_or(Value::Null);
            Ok(json!({ "tabs": rows.iter().map(row_to_tab).collect::<Vec<_>>(), "activeTabId": active, "persistable": true }))
        }
        "ensure-singleton" => {
            let o = require_object(payload, "ensure singleton payload")?;
            let sid = require_string(o.get("sessionId"), "sessionId")?;
            let kind = require_string(o.get("kind"), "kind")?;
            if kind != "subagents" {
                return fail("INVALID_PARAMS", format!("{kind} is not a singleton tab kind"));
            }
            let state_json = serialize_state(o.get("state"))?;
            if !session_exists(conn, &sid)? {
                return Ok(json!({ "tab": null, "created": false, "persistable": false }));
            }
            tx(conn, || {
                let find = || sql::get(conn, "SELECT * FROM right_sidebar_tabs WHERE session_id = ? AND kind = ? LIMIT 1", &[json!(sid), json!(kind)]);
                if let Some(e) = find()? {
                    return Ok(json!({ "tab": row_to_tab(&e), "created": false, "persistable": true }));
                }
                let count = sql::get(conn, "SELECT COUNT(*) AS n FROM right_sidebar_tabs WHERE session_id = ?", &[json!(sid)])?.and_then(|r| r["n"].as_i64()).unwrap_or(0);
                if count >= MAX_TABS_PER_SESSION {
                    return too_many(&sid);
                }
                let now = db.env.now_ms();
                let id = format!("t_{}", db.env.uuid().replace('-', ""));
                sql::run(
                    conn,
                    "INSERT INTO right_sidebar_tabs (id, session_id, kind, position, state, is_active, created_at, updated_at) VALUES (?, ?, ?, ?, ?, 0, ?, ?) ON CONFLICT DO NOTHING",
                    &[json!(id), json!(sid), json!(kind), json!(count), json!(state_json), json!(now), json!(now)],
                )?;
                let Some(c) = find()? else { return Err(format!("failed to ensure {kind} tab for session {sid}")) };
                Ok(json!({ "tab": row_to_tab(&c), "created": c["id"] == json!(id), "persistable": true }))
            })
        }
        "upsert" => {
            let o = require_object(payload, "upsert payload")?;
            let id = require_string(o.get("id"), "id")?;
            let sid = require_string(o.get("sessionId"), "sessionId")?;
            let kind = require_string(o.get("kind"), "kind")?;
            let position = require_int(o.get("position"), "position")?;
            let state_json = serialize_state(o.get("state"))?;
            tx(conn, || {
                let ids: Vec<Value> = sql::all(conn, "SELECT id FROM right_sidebar_tabs WHERE session_id = ?", &[json!(sid)])?.into_iter().map(|r| r["id"].clone()).collect();
                if !ids.contains(&json!(id)) && ids.len() as i64 >= MAX_TABS_PER_SESSION {
                    return too_many(&sid);
                }
                let now = db.env.now_ms();
                // 会话不存在时外键约束会报错，与 desktop 一致（renderer 先看 list 的 persistable）。
                sql::run(
                    conn,
                    "INSERT INTO right_sidebar_tabs (id, session_id, kind, position, state, is_active, created_at, updated_at) VALUES (?, ?, ?, ?, ?, 0, ?, ?) ON CONFLICT(id) DO UPDATE SET kind = excluded.kind, position = excluded.position, state = excluded.state, updated_at = excluded.updated_at",
                    &[json!(id), json!(sid), json!(kind), json!(position), json!(state_json), json!(now), json!(now)],
                )?;
                Ok(())
            })?;
            Ok(json!({ "ok": true }))
        }
        "close" => {
            let o = require_object(payload, "close payload")?;
            let id = require_string(o.get("id"), "id")?;
            if sql::run(conn, "DELETE FROM right_sidebar_tabs WHERE id = ?", &[json!(id)])? == 0 {
                return fail("NOT_FOUND", format!("tab {id} not found"));
            }
            Ok(json!({ "ok": true }))
        }
        "setActive" => {
            let o = require_object(payload, "setActive payload")?;
            let sid = require_string(o.get("sessionId"), "sessionId")?;
            let target = match o.get("id") {
                Some(Value::Null) => None,
                other => Some(require_string(other, "id")?),
            };
            tx(conn, || {
                let now = db.env.now_ms();
                sql::run(conn, "UPDATE right_sidebar_tabs SET is_active = 0, updated_at = ? WHERE session_id = ?", &[json!(now), json!(sid)])?;
                if let Some(t) = &target {
                    let n = sql::run(conn, "UPDATE right_sidebar_tabs SET is_active = 1, updated_at = ? WHERE id = ? AND session_id = ?", &[json!(now), json!(t), json!(sid)])?;
                    if n == 0 {
                        return fail("NOT_FOUND", format!("tab {t} not found in session {sid}"));
                    }
                }
                Ok(())
            })?;
            Ok(json!({ "ok": true }))
        }
        "reorder" => {
            let o = require_object(payload, "reorder payload")?;
            let sid = require_string(o.get("sessionId"), "sessionId")?;
            let Some(arr) = o.get("orderedIds").and_then(Value::as_array) else { return fail("INVALID_PARAMS", "orderedIds must be an array") };
            let mut ids = Vec::new();
            for (i, v) in arr.iter().enumerate() {
                match v.as_str().filter(|s| !s.is_empty()) {
                    Some(s) => ids.push(s.to_string()),
                    None => return fail("INVALID_PARAMS", format!("orderedIds[{i}] must be a non-empty string")),
                }
            }
            tx(conn, || {
                let now = db.env.now_ms();
                for (i, id) in ids.iter().enumerate() {
                    sql::run(conn, "UPDATE right_sidebar_tabs SET position = ?, updated_at = ? WHERE id = ? AND session_id = ?", &[json!(i), json!(now), json!(id), json!(sid)])?;
                }
                Ok(())
            })?;
            Ok(json!({ "ok": true }))
        }
        other => fail("NOT_IMPLEMENTED", format!("local-db:right-sidebar-tabs:{other} is not available")),
    }
}
