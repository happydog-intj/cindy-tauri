//! 会话行：建 / 读 / 列表 / 更新 / 自动起名 / 中断与告警，对应 index.mjs（原 sessions.ts 的校验子集）。

use super::mapper::{as_finite, as_integer, js_str, js_trim, parse_date, slice16, utf16_len};
use super::sql;
use super::{fail, session_select, to_session, LocalDb, Row};
use rusqlite::Connection;
use serde_json::{json, Map, Value};

pub const DEFAULT_DRAFT_TITLE: &str = "New Maker";
const AUTO_TITLE_MAX_CHARS: usize = 40;
const AGENT_KINDS: [&str; 3] = ["cc", "codex", "pi"];

fn require_string(v: &Value, name: &str) -> Result<String, String> {
    match v.as_str().filter(|s| !s.is_empty()) {
        Some(s) => Ok(s.to_string()),
        None => fail("INVALID_PARAMS", format!("{name} must be a non-empty string")),
    }
}

pub fn get_row(conn: &Connection, id: &str) -> Result<Option<Row>, String> {
    sql::get(conn, &format!("{} WHERE s.id = ?", session_select()), &[json!(id)])
}

fn require_session(conn: &Connection, id: &Value, name: &str) -> Result<Row, String> {
    let id = require_string(id, name)?;
    get_row(conn, &id)?.ok_or_else(|| "[NOT_FOUND] Session 不存在".to_string())
}

/// `dialogueDir`：<dataDir>/dialogues/<YYYY-MM-DD>/<sessionId>。
fn dialogue_dir(db: &LocalDb, session_id: &str, at: i64) -> std::path::PathBuf {
    let (y, m, d) = db.env.local_date(at);
    db.data_dir.join("dialogues").join(format!("{y:04}-{m:02}-{d:02}")).join(session_id)
}

pub fn get_value(db: &LocalDb, conn: &Connection, id: &Value) -> Result<Value, String> {
    let row = require_session(conn, id, "id")?;
    Ok(to_session(&row, db.boot_at))
}

pub fn list_value(db: &LocalDb, conn: &Connection, limit: &Value, status: &Value, options: &Value) -> Result<Value, String> {
    Ok(Value::Array(list(db, conn, limit, status, options)?))
}

pub fn list(db: &LocalDb, conn: &Connection, limit: &Value, status: &Value, options: &Value) -> Result<Vec<Value>, String> {
    let cap = as_integer(limit).unwrap_or(20).clamp(1, 500);
    let mut where_: Vec<String> = Vec::new();
    let mut vals: Vec<Value> = Vec::new();
    match status.as_str().filter(|s| !s.is_empty()) {
        Some(s) => {
            where_.push("s.status = ?".into());
            vals.push(json!(s));
        }
        None => where_.push("s.status != 'deleted'".into()),
    }
    let opts = options.as_object();
    if let Some(ids) = opts.and_then(|o| o.get("ids")).and_then(Value::as_array) {
        let ids: Vec<String> = ids.iter().filter_map(Value::as_str).take(500).map(String::from).collect();
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        where_.push(format!("s.id IN ({})", vec!["?"; ids.len()].join(",")));
        vals.extend(ids.into_iter().map(Value::String));
    }
    if let Some(b) = opts.and_then(|o| o.get("before")).and_then(Value::as_object) {
        if let (Some(n), Some(id)) = (b.get("updatedAt").and_then(Value::as_i64), b.get("id").and_then(Value::as_str)) {
            where_.push("(s.updated_at < ? OR (s.updated_at = ? AND s.id < ?))".into());
            vals.push(json!(n));
            vals.push(json!(n));
            vals.push(json!(id));
        }
    }
    vals.push(json!(cap));
    let rows = sql::all(conn, &format!("{} WHERE {} ORDER BY s.updated_at DESC, s.id DESC LIMIT ?", session_select(), where_.join(" AND ")), &vals)?;
    Ok(rows.iter().map(|r| to_session(r, db.boot_at)).collect())
}

pub fn get_many(db: &LocalDb, conn: &Connection, ids: &Value) -> Result<Value, String> {
    let arr = match ids.as_array() {
        Some(a) if a.len() <= 200 => a,
        _ => return fail("INVALID_PARAMS", "sessionIds must be an array of at most 200 ids"),
    };
    Ok(Value::Array(list(db, conn, &json!(200), &Value::Null, &json!({ "ids": arr }))?))
}

pub fn restore_if_archived(db: &LocalDb, conn: &Connection, id: &Value) -> Result<Value, String> {
    let row = require_session(conn, id, "id")?;
    Ok(to_session(&row, db.boot_at))
}

fn touch_recent_workdir(conn: &Connection, p: &str, at: i64) -> Result<(), String> {
    sql::run(
        conn,
        "INSERT INTO recent_workdirs (path, last_used_at) VALUES (?, ?) ON CONFLICT(path) DO UPDATE SET last_used_at = excluded.last_used_at",
        &[json!(p), json!(at)],
    )?;
    Ok(())
}

fn validate_session_id(id: &str) -> Result<(), String> {
    let ok = !id.is_empty() && id.len() <= 128 && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
    if ok { Ok(()) } else { fail("INVALID_PARAMS", "invalid session id") }
}

pub fn create(db: &LocalDb, conn: &Connection, body: &Value) -> Result<Value, String> {
    let body = match body {
        Value::Object(o) => o.clone(),
        _ => Map::new(),
    };
    let now = db.env.now_ms();
    let id = match body.get("id") {
        None | Some(Value::Null) => db.env.uuid(),
        Some(v) => js_str(v),
    };
    validate_session_id(&id)?;
    if let Some(k) = body.get("agentKind").filter(|v| !v.is_null()) {
        let k = js_str(k);
        if !AGENT_KINDS.contains(&k.as_str()) {
            return fail("INVALID_PARAMS", format!("invalid agentKind: {k}"));
        }
    }
    if let Some(t) = body.get("title").filter(|v| !v.is_null()) {
        let len = t.as_str().map(utf16_len).unwrap_or(usize::MAX);
        if !t.is_string() || len > 200 {
            return fail("INVALID_PARAMS", "title must be a string no longer than 200 characters");
        }
    }
    if let Some(w) = body.get("workspaceKind").filter(|v| !v.is_null()) {
        let w = js_str(w);
        if w != "project" && w != "dialogue" {
            return fail("INVALID_PARAMS", format!("invalid workspaceKind: {w}"));
        }
    }
    for k in ["remoteHostId", "agentDeviceId", "orcaRole"] {
        // IPC 里值为 undefined 的键会编码成 {"__cindy_t":"u"} 对象，不能当成有值（否则普通本地任务也被拒）。
        if body.get(k).filter(|v| !crate::voice_settings::is_undef(v)).map(super::mapper::truthy).unwrap_or(false) {
            return fail("NOT_IMPLEMENTED", "remote / orca sessions are not available in the Tauri build yet");
        }
    }
    if let Some(row) = get_row(conn, &id)? {
        return Ok(to_session(&row, db.boot_at));
    }
    let workspace_kind = body.get("workspaceKind").and_then(Value::as_str).unwrap_or("project").to_string();
    let mut working_dir = match body.get("workingDir").and_then(Value::as_str) {
        Some(w) if !js_trim(w).is_empty() => Some(w.to_string()),
        _ => None,
    };
    if workspace_kind == "dialogue" && working_dir.is_none() {
        let d = dialogue_dir(db, &id, now);
        let _ = std::fs::create_dir_all(&d);
        working_dir = Some(d.to_string_lossy().into_owned());
    }
    let pick = |k: &str, dflt: &str| body.get(k).and_then(Value::as_str).filter(|s| !s.is_empty()).unwrap_or(dflt).to_string();
    let title = body.get("title").and_then(Value::as_str).filter(|s| !s.is_empty()).unwrap_or(DEFAULT_DRAFT_TITLE).to_string();
    let project_dir = body.get("projectDir").and_then(Value::as_str).map(String::from);
    let agent_kind = pick("agentKind", "cc");
    let model = pick("model", "claude-sonnet-4-6");
    let effort = pick("effort", "high");
    let permission_mode = pick("permissionMode", "ask");
    let provider_id = body.get("providerId").and_then(Value::as_str).map(String::from);
    let plan = super::mapper::truthy(body.get("planModeEnabled").unwrap_or(&Value::Null));
    let fast = super::mapper::truthy(body.get("fastMode").unwrap_or(&Value::Null));
    sql::run(
        conn,
        "INSERT INTO sessions (id, title, working_dir, project_dir, workspace_kind, model, effort, permission_mode, status, agent_kind, provider_id, plan_mode_enabled, fast_mode, created_at, updated_at, last_activity_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, 'active', ?, ?, ?, ?, ?, ?, ?)",
        &[
            json!(id), json!(title), working_dir.clone().map(Value::String).unwrap_or(Value::Null), project_dir.clone().map(Value::String).unwrap_or(Value::Null),
            json!(workspace_kind), json!(model), json!(effort), json!(permission_mode), json!(agent_kind),
            provider_id.clone().map(Value::String).unwrap_or(Value::Null), json!(plan as i64), json!(fast as i64), json!(now), json!(now), json!(now),
        ],
    )?;
    if workspace_kind == "project" {
        if let Some(w) = &working_dir {
            touch_recent_workdir(conn, project_dir.as_deref().unwrap_or(w), now)?;
        }
    }
    if body.get("provisional") != Some(&json!(true)) {
        db.out.push("local-db:sessions:created", json!({ "sessionId": id }));
    }
    Ok(to_session(&get_row(conn, &id)?.unwrap(), db.boot_at))
}

pub fn create_value(db: &LocalDb, conn: &Connection, body: &Value) -> Result<Value, String> {
    create(db, conn, body)
}

/// maker-ipc 的 upsertSession(row)：camelCase，字段见 maker-ipc.mjs upsertRow。
pub fn upsert(db: &LocalDb, conn: &Connection, row: &Value) -> Result<(), String> {
    let row = match row.as_object() {
        Some(o) => o,
        None => return Ok(()),
    };
    let rid = match row.get("id").and_then(Value::as_str) {
        Some(s) => s.to_string(),
        None => return Ok(()),
    };
    if get_row(conn, &rid)?.is_none() {
        let mut body = Map::new();
        body.insert("id".into(), json!(rid));
        body.insert("agentKind".into(), json!(row.get("agentKind").and_then(Value::as_str).unwrap_or("pi")));
        body.insert("title".into(), json!(row.get("title").and_then(Value::as_str).filter(|s| !s.is_empty()).unwrap_or(DEFAULT_DRAFT_TITLE)));
        if let Some(w) = row.get("workDir") {
            body.insert("workingDir".into(), w.clone());
        }
        body.insert("workspaceKind".into(), json!("dialogue"));
        for k in ["model", "permissionMode", "providerId"] {
            if let Some(v) = row.get(k) {
                body.insert(k.into(), v.clone());
            }
        }
        create(db, conn, &Value::Object(body))?;
        return Ok(());
    }
    let mut sets: Vec<String> = Vec::new();
    let mut vals: Vec<Value> = Vec::new();
    for (col, key) in [("agent_kind", "agentKind"), ("title", "title"), ("working_dir", "workDir"), ("provider_id", "providerId"), ("model", "model"), ("permission_mode", "permissionMode"), ("status", "status")] {
        match row.get(key) {
            Some(v) if !v.is_null() && v.as_str() != Some("") => {
                sets.push(format!("{col} = ?"));
                vals.push(v.clone());
            }
            _ => {}
        }
    }
    let updated = row.get("updatedAt").cloned().unwrap_or(json!(db.env.now_ms()));
    sets.push("updated_at = ?".into());
    vals.push(updated);
    vals.push(json!(rid));
    sql::run(conn, &format!("UPDATE sessions SET {} WHERE id = ?", sets.join(", ")), &vals)?;
    Ok(())
}

const PATCH_COLUMNS: [(&str, &str); 18] = [
    ("title", "title"), ("model", "model"), ("effort", "effort"), ("permissionMode", "permission_mode"), ("status", "status"),
    ("providerId", "provider_id"), ("sdkSessionId", "sdk_session_id"), ("summary", "summary"), ("fastMode", "fast_mode"),
    ("planModeEnabled", "plan_mode_enabled"), ("notificationsMuted", "notifications_muted"), ("pinnedAt", "pinned_at"),
    ("hiddenAt", "hidden_at"), ("clearedAt", "cleared_at"), ("contextTokens", "context_tokens"), ("contextWindow", "context_window"),
    ("totalTokenUsage", "total_token_usage"), ("totalCostUsd", "total_cost_usd"),
];
const DATE_KEYS: [&str; 3] = ["pinnedAt", "hiddenAt", "clearedAt"];

pub fn update(db: &LocalDb, conn: &Connection, id: &Value, patch: &Value) -> Result<Value, String> {
    let id = require_string(id, "id")?;
    if get_row(conn, &id)?.is_none() {
        return fail("NOT_FOUND", "Session 不存在");
    }
    let p = patch.as_object().ok_or_else(|| "[INVALID_PARAMS] patch must be an object".to_string())?;
    if p.contains_key("agentKind") {
        return fail("NOT_IMPLEMENTED", "switching agent is not available in the Tauri build yet");
    }
    // 切换工作目录：任务转成项目任务。目录必须是已存在的绝对路径；下次发送时 pi-rs 按新目录重建（调用方负责关掉旧进程）。
    let new_working_dir = match p.get("workingDir") {
        None => None,
        Some(v) => {
            let s = v.as_str().map(str::trim).filter(|s| !s.is_empty());
            let Some(s) = s else { return fail("INVALID_PARAMS", "workingDir must be a non-empty string") };
            let path = std::path::Path::new(s);
            if !path.is_absolute() {
                return fail("INVALID_PARAMS", "workingDir must be an absolute path");
            }
            if !path.is_dir() {
                return fail("INVALID_PARAMS", format!("workingDir is not a directory: {s}"));
            }
            let trimmed = s.trim_end_matches('/');
            Some(if trimmed.is_empty() { "/".to_string() } else { trimmed.to_string() })
        }
    };
    if let Some(t) = p.get("title") {
        let len = t.as_str().map(utf16_len).unwrap_or(usize::MAX);
        if !t.is_string() || len > 200 {
            return fail("INVALID_PARAMS", "invalid title");
        }
    }
    if let Some(m) = p.get("notificationsMuted") {
        if !m.is_boolean() {
            return fail("INVALID_PARAMS", "notificationsMuted must be a boolean");
        }
    }
    let mut sets: Vec<String> = Vec::new();
    let mut vals: Vec<Value> = Vec::new();
    // 附加目录：字符串数组，库里存 JSON 文本。校验（存在、去重、上限、授权）在调用方做，这里只挡类型错误。
    for (k, col) in [("extraDirs", "extra_dirs"), ("writableDirs", "writable_dirs")] {
        let Some(v) = p.get(k) else { continue };
        if !v.as_array().is_some_and(|a| a.len() <= 64 && a.iter().all(|d| d.as_str().is_some_and(|d| !d.is_empty() && d.len() <= 4096))) {
            return fail("INVALID_PARAMS", format!("{k} must be a string array"));
        }
        sets.push(format!("{col} = ?"));
        vals.push(json!(v.to_string()));
    }
    for (k, col) in PATCH_COLUMNS {
        let Some(v) = p.get(k) else { continue };
        let mut v = v.clone();
        if let Some(b) = v.as_bool() {
            v = json!(b as i64);
        } else if DATE_KEYS.contains(&k) && !v.is_null() {
            v = match &v {
                Value::Number(_) => v,
                Value::String(s) => json!(parse_date(s).unwrap_or(i64::MIN)),
                _ => v,
            };
        }
        if !v.is_null() && !v.is_string() && as_finite(&v).is_none() {
            return fail("INVALID_PARAMS", format!("invalid {k}"));
        }
        sets.push(format!("{col} = ?"));
        vals.push(v);
    }
    if let Some(wd) = &new_working_dir {
        sets.push("working_dir = ?".into());
        vals.push(json!(wd));
        sets.push("workspace_kind = ?".into());
        vals.push(json!("project"));
    }
    sets.push("updated_at = ?".into());
    vals.push(json!(db.env.now_ms()));
    vals.push(json!(id));
    sql::run(conn, &format!("UPDATE sessions SET {} WHERE id = ?", sets.join(", ")), &vals)?;
    let updated = to_session(&get_row(conn, &id)?.unwrap(), db.boot_at);
    let mut echoed = p.clone();
    if let Some(wd) = &new_working_dir {
        echoed.insert("workingDir".into(), json!(wd));
        echoed.insert("workspaceKind".into(), json!("project"));
    }
    echoed.insert("updatedAt".into(), updated["updatedAt"].clone());
    db.out.push("local-db:sessions:patched", json!({ "sessionId": id, "patch": Value::Object(echoed) }));
    Ok(updated)
}

pub fn update_value(db: &LocalDb, conn: &Connection, id: &Value, patch: &Value) -> Result<Value, String> {
    update(db, conn, id, patch)
}

/// normalizeAutoTitle：与 @cindy/maker-shared/session-title 逐字一致。
pub fn normalize_auto_title(text: &str) -> String {
    let collapsed: String = {
        let mut out = String::new();
        let mut in_ws = false;
        for c in text.chars() {
            if super::mapper::is_js_ws(c) {
                if !in_ws {
                    out.push(' ');
                }
                in_ws = true;
            } else {
                out.push(c);
                in_ws = false;
            }
        }
        out
    };
    let trimmed = js_trim(&collapsed);
    let units: Vec<u16> = trimmed.encode_utf16().collect();
    let head = if units.len() > AUTO_TITLE_MAX_CHARS { String::from_utf16_lossy(&units[..AUTO_TITLE_MAX_CHARS]) } else { trimmed.to_string() };
    js_trim(&head).to_string()
}

pub fn auto_title(db: &LocalDb, conn: &Connection, req: &Value) -> Result<Value, String> {
    let req = match req.as_object() {
        Some(o) => o,
        None => return fail("INVALID_PARAMS", "auto-title request required"),
    };
    let session_id = require_string(req.get("sessionId").unwrap_or(&Value::Null), "sessionId")?;
    let text = match req.get("text") {
        Some(Value::String(s)) => s.clone(),
        _ => return fail("INVALID_PARAMS", "invalid text"),
    };
    let row = sql::get(conn, "SELECT title FROM sessions WHERE id = ?", &[json!(session_id)])?;
    let Some(row) = row else { return Ok(json!({ "applied": false, "done": false })) };
    if row.get("title").and_then(Value::as_str) != Some(DEFAULT_DRAFT_TITLE) {
        return Ok(json!({ "applied": false, "done": true }));
    }
    let title = normalize_auto_title(&slice16(&text, 2000));
    if title.is_empty() || title == DEFAULT_DRAFT_TITLE {
        return Ok(json!({ "applied": false, "done": false }));
    }
    let changes = sql::run(conn, "UPDATE sessions SET title = ? WHERE id = ? AND title = ?", &[json!(title), json!(session_id), json!(DEFAULT_DRAFT_TITLE)])?;
    if changes != 1 {
        return Ok(json!({ "applied": false, "done": true }));
    }
    db.out.push("local-db:sessions:patched", json!({ "sessionId": session_id, "patch": { "title": title } }));
    // 附件合成的描述（isUserText=false）只是占位，等用户打字再换。
    Ok(json!({ "applied": true, "done": req.get("isUserText").map(|v| v != &json!(false)).unwrap_or(true) }))
}

pub fn touch_user_send(db: &LocalDb, conn: &Connection, id: &Value, at: &Value) -> Result<Value, String> {
    let at = at.as_f64().filter(|f| f.is_finite()).map(|f| f as i64).unwrap_or_else(|| db.env.now_ms());
    let id = require_string(id, "id")?;
    sql::run(conn, "UPDATE sessions SET user_send_at = ?, last_activity_at = ?, updated_at = ? WHERE id = ?", &[json!(at), json!(at), json!(at), json!(id)])?;
    Ok(Value::Null)
}

pub fn set_pinned_card_summaries(enabled: &Value) -> Result<Value, String> {
    if !enabled.is_boolean() {
        return fail("INVALID_PARAMS", "enabled must be a boolean");
    }
    Ok(Value::Null)
}

fn interrupted_rows(conn: &Connection, boot_at: i64) -> Result<Vec<Row>, String> {
    sql::all(
        conn,
        "SELECT id, active_turn_started_at FROM sessions WHERE status = 'active' AND active_turn_started_at IS NOT NULL AND active_turn_started_at < ? AND active_turn_started_at > COALESCE(last_turn_ended_at, 0) AND active_turn_started_at > COALESCE(cleared_at, 0)",
        &[json!(boot_at)],
    )
}

pub fn interrupted_pending(db: &LocalDb, conn: &Connection) -> Result<Value, String> {
    Ok(Value::Array(interrupted_rows(conn, db.boot_at)?.iter().map(|r| r["id"].clone()).collect()))
}

pub fn interrupted_candidates(db: &LocalDb, conn: &Connection) -> Result<Value, String> {
    Ok(Value::Array(
        interrupted_rows(conn, db.boot_at)?.iter().map(|r| json!({ "sessionId": r["id"], "interruptedAt": r["active_turn_started_at"], "activity": null })).collect(),
    ))
}

pub fn error_tail_pending(conn: &Connection) -> Result<Value, String> {
    let rows = sql::all(
        conn,
        "SELECT s.id FROM sessions s WHERE s.status = 'active' AND (SELECT CASE WHEN m.role = 'error' AND COALESCE(json_extract(m.content, '$.dismissed'), 0) != 1 THEN 1 ELSE 0 END FROM messages m WHERE m.session_id = s.id AND m.rewind_at IS NULL ORDER BY m.created_at DESC, m.rowid DESC LIMIT 1) = 1",
        &[],
    )?;
    Ok(Value::Array(rows.iter().map(|r| r["id"].clone()).collect()))
}

pub fn ack_interrupted(db: &LocalDb, conn: &Connection, id: &Value) -> Result<Value, String> {
    let id = require_string(id, "id")?;
    sql::run(conn, "UPDATE sessions SET last_turn_ended_at = ? WHERE id = ?", &[json!(db.env.now_ms()), json!(id)])?;
    Ok(json!({ "ok": true }))
}

pub fn dismiss_pending_alerts(db: &LocalDb, conn: &Connection, ids: &Value) -> Result<Value, String> {
    let arr = match ids.as_array() {
        Some(a) if a.len() <= 500 => a.clone(),
        _ => return fail("INVALID_PARAMS", "sessionIds must be an array"),
    };
    let now = db.env.now_ms();
    for id in &arr {
        let id = require_string(id, "sessionId")?;
        sql::run(conn, "UPDATE sessions SET last_turn_ended_at = ? WHERE id = ?", &[json!(now), json!(id)])?;
    }
    Ok(json!({ "dismissed": arr.len(), "processed": arr, "failed": [] }))
}
