//! 行 → camelCase 投影与预览，对应 sidecar/src/local-db/mapper.mjs（及其 JS 语义小工具）。
//! 行用 `Map<String, Value>` 表示（与 node:sqlite 返回的对象同形），移植时可以逐行对照 JS。

use regex::Regex;
use serde_json::{json, Map, Value};
use std::sync::OnceLock;

pub type Row = Map<String, Value>;

const PREVIEW_MAX_CHARS: usize = 140;
pub const PREVIEW_EXTRACT_CHARS: usize = 4096;

// ── JS 语义小工具 ──────────────────────────────────────────────

/// JS `\s` 的字符集（与 Rust 的 is_whitespace 在 U+0085 / U+FEFF 上不同）。
pub fn is_js_ws(c: char) -> bool {
    matches!(c, '\t' | '\n' | '\u{b}' | '\u{c}' | '\r' | ' ' | '\u{a0}' | '\u{1680}' | '\u{2028}' | '\u{2029}' | '\u{202f}' | '\u{205f}' | '\u{3000}' | '\u{feff}')
        || ('\u{2000}'..='\u{200a}').contains(&c)
}
pub fn js_trim(s: &str) -> &str {
    s.trim_matches(is_js_ws)
}
pub fn utf16_len(s: &str) -> usize {
    s.encode_utf16().count()
}
/// `s.slice(0, n)`（按 UTF-16 单元）。切在代理对中间时丢掉那半个字符（JS 会留下孤立代理）。
pub fn slice16(s: &str, n: usize) -> String {
    let units: Vec<u16> = s.encode_utf16().collect();
    if units.len() <= n {
        return s.to_string();
    }
    let mut end = n;
    if end > 0 && (0xd800..=0xdbff).contains(&units[end - 1]) {
        end -= 1;
    }
    String::from_utf16_lossy(&units[..end])
}
pub fn truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().map(|f| f != 0.0 && !f.is_nan()).unwrap_or(true),
        Value::String(s) => !s.is_empty(),
        _ => true,
    }
}
/// `String(v)`。
pub fn js_str(v: &Value) -> String {
    match v {
        Value::Null => "null".into(),
        Value::String(s) => s.clone(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => n.to_string(),
        Value::Array(a) => a.iter().map(|x| if x.is_null() { String::new() } else { js_str(x) }).collect::<Vec<_>>().join(","),
        Value::Object(_) => "[object Object]".into(),
    }
}
/// `Number.isInteger(v)`（含 5.0）。
pub fn as_integer(v: &Value) -> Option<i64> {
    let n = v.as_number()?;
    if let Some(i) = n.as_i64() {
        return Some(i);
    }
    let f = n.as_f64()?;
    (f.is_finite() && f.fract() == 0.0 && f.abs() < 9.007e15).then_some(f as i64)
}
pub fn as_finite(v: &Value) -> Option<f64> {
    v.as_f64().filter(|f| f.is_finite())
}
/// `new Date(ms).toISOString()`。
pub fn iso(ms: i64) -> Option<String> {
    chrono::DateTime::from_timestamp_millis(ms).map(|d| d.format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string())
}
/// `Date.parse(s)` 的常用子集：RFC3339，或只有日期（按 UTC）。
pub fn parse_date(s: &str) -> Option<i64> {
    if let Ok(d) = chrono::DateTime::parse_from_rfc3339(s) {
        return Some(d.timestamp_millis());
    }
    chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d").ok().map(|d| d.and_hms_opt(0, 0, 0).unwrap().and_utc().timestamp_millis())
}

fn ws_class() -> &'static str {
    r"[\t\n\x{b}\x{c}\r \x{a0}\x{1680}\x{2000}-\x{200a}\x{2028}\x{2029}\x{202f}\x{205f}\x{3000}\x{feff}]"
}

// ── 预览 ─────────────────────────────────────────────────────

struct PreviewRe {
    fence: Regex,
    image: Regex,
    link: Regex,
    tag: Regex,
    lead: Regex,
    marks: Regex,
    spaces: Regex,
}
fn re() -> &'static PreviewRe {
    static R: OnceLock<PreviewRe> = OnceLock::new();
    R.get_or_init(|| {
        let ws = ws_class();
        PreviewRe {
            fence: Regex::new(r"```[^\n]*\n?").unwrap(),
            image: Regex::new(r"!\[([^\]]*)\]\([^)]*\)").unwrap(),
            link: Regex::new(r"\[([^\]]*)\]\([^)]*\)").unwrap(),
            tag: Regex::new(r"<[^>]+>").unwrap(),
            lead: Regex::new(&format!(r"(?m)^{ws}{{0,3}}(#{{1,6}}|>|[-*+]|[0-9]+\.){ws}+")).unwrap(),
            marks: Regex::new(r"[*_~`]+").unwrap(),
            spaces: Regex::new(&format!("{ws}+")).unwrap(),
        }
    })
}

/// 简化版 computePlainPreview：去掉常见 markdown 记号，折叠空白，截到 140 个 UTF-16 单元。
pub fn plain_preview(text: Option<&str>) -> Option<String> {
    let text = text.filter(|t| !t.is_empty())?;
    let r = re();
    let s = r.fence.replace_all(text, " ");
    let s = r.image.replace_all(&s, "$1");
    let s = r.link.replace_all(&s, "$1");
    let s = r.tag.replace_all(&s, " ");
    let s = r.lead.replace_all(&s, "");
    let s = r.marks.replace_all(&s, "");
    let s = r.spaces.replace_all(&s, " ");
    let collapsed = js_trim(&s);
    if collapsed.is_empty() {
        return None;
    }
    Some(slice16(collapsed, PREVIEW_MAX_CHARS))
}

/// extractMessagePreview：assistant 取 JSON 字符串，user 取对象的 .text；解析失败（被截断）按原文。
pub fn extract_message_preview(raw: Option<&str>, role: Option<&str>) -> Option<String> {
    let raw = raw.filter(|r| !r.is_empty())?;
    let text: Option<String> = match serde_json::from_str::<Value>(raw) {
        Ok(Value::String(s)) => Some(s),
        Ok(Value::Object(o)) if role == Some("user") => o.get("text").and_then(Value::as_str).map(String::from),
        Ok(_) => None,
        Err(_) => Some(raw.to_string()),
    };
    plain_preview(text.as_deref())
}

// ── 行投影 ───────────────────────────────────────────────────

fn s(row: &Row, k: &str) -> Value {
    row.get(k).cloned().unwrap_or(Value::Null)
}
fn i(row: &Row, k: &str) -> i64 {
    row.get(k).and_then(Value::as_i64).unwrap_or(0)
}
fn flag(row: &Row, k: &str) -> bool {
    row.get(k).map(|v| as_finite(v).map(|f| f != 0.0).unwrap_or(false)).unwrap_or(false)
}
fn ms_iso(row: &Row, k: &str) -> Value {
    match row.get(k).and_then(Value::as_i64).and_then(iso) {
        Some(t) => Value::String(t),
        None => Value::Null,
    }
}

/// messageToCamel：content 先 JSON.parse，失败保留原串；agentMeta 解析失败为 null。
pub fn message_to_camel(row: &Row) -> Value {
    let raw = row.get("content").and_then(Value::as_str).unwrap_or("");
    let content = serde_json::from_str::<Value>(raw).unwrap_or_else(|_| Value::String(raw.to_string()));
    let agent_meta = row.get("agent_meta").and_then(Value::as_str).and_then(|t| serde_json::from_str::<Value>(t).ok()).unwrap_or(Value::Null);
    let mut out = Map::new();
    out.insert("id".into(), s(row, "id"));
    out.insert("clientId".into(), s(row, "client_id"));
    out.insert("sessionId".into(), s(row, "session_id"));
    out.insert("role".into(), s(row, "role"));
    out.insert("content".into(), content);
    out.insert("toolUseId".into(), s(row, "tool_use_id"));
    out.insert("agentMeta".into(), agent_meta);
    out.insert("agentKind".into(), s(row, "agent_kind"));
    out.insert("createdAt".into(), iso(i(row, "created_at")).map(Value::String).unwrap_or(Value::Null));
    if let Some(r) = row.get("rowid").filter(|v| v.is_number()) {
        out.insert("rowid".into(), r.clone());
    }
    Value::Object(out)
}

pub fn is_interrupted(row: &Row, boot_at: i64) -> bool {
    let started = match row.get("active_turn_started_at").and_then(Value::as_f64) {
        Some(v) if v > 0.0 => v,
        _ => return false,
    };
    let num = |k: &str| row.get(k).and_then(Value::as_f64).unwrap_or(0.0);
    row.get("status").and_then(Value::as_str) == Some("active") && started < boot_at as f64 && started > num("last_turn_ended_at") && started > num("cleared_at")
}

/// sessionToCamel（不含 preview，由调用方按最新消息算）。
pub fn session_to_camel(row: &Row, boot_at: i64) -> Row {
    let mut m = Map::new();
    let mut put = |k: &str, v: Value| {
        m.insert(k.to_string(), v);
    };
    put("id", s(row, "id"));
    put("userId", json!(""));
    put("title", s(row, "title"));
    put("workingDir", s(row, "working_dir"));
    put("projectDir", s(row, "project_dir"));
    put("workspaceKind", json!(if row.get("workspace_kind").and_then(Value::as_str) == Some("dialogue") { "dialogue" } else { "project" }));
    put("model", s(row, "model"));
    put("effort", s(row, "effort"));
    put("permissionMode", s(row, "permission_mode"));
    put("providerId", s(row, "provider_id"));
    put("status", s(row, "status"));
    put("sdkSessionId", s(row, "sdk_session_id"));
    put("totalTokenUsage", row.get("total_token_usage").cloned().filter(|v| !v.is_null()).unwrap_or(json!(0)));
    put("totalCostUsd", row.get("total_cost_usd").cloned().filter(|v| !v.is_null()).unwrap_or(json!(0)));
    put("totalMoney", json!({ "amount": 0, "currency": "USD", "approximate": false, "kind": "actual-cost" }));
    put("contextTokens", row.get("context_tokens").cloned().filter(|v| !v.is_null()).unwrap_or(json!(0)));
    put("contextWindow", row.get("context_window").cloned().filter(|v| !v.is_null()).unwrap_or(json!(0)));
    put("fastMode", json!(flag(row, "fast_mode")));
    put("planModeEnabled", json!(flag(row, "plan_mode_enabled")));
    put("clearedAt", ms_iso(row, "cleared_at"));
    put("pinnedAt", ms_iso(row, "pinned_at"));
    put("hiddenAt", ms_iso(row, "hidden_at"));
    put("notificationsMuted", json!(flag(row, "notifications_muted")));
    put("userSendAt", ms_iso(row, "user_send_at"));
    put("lastActivityAt", ms_iso(row, "last_activity_at"));
    put("agentKind", s(row, "agent_kind"));
    let source = row.get("source").and_then(Value::as_str).filter(|x| !x.is_empty()).unwrap_or("desktop");
    put("source", json!(source));
    for k in ["orcaRole", "parentSessionId", "forkedAtMessageId", "worktreePath", "remoteHostId", "agentDeviceId", "orcaRemoteLead"] {
        put(k, Value::Null);
    }
    put("usedProjectContext", json!(false));
    put("extraDirs", json!([]));
    put("writableDirs", json!([]));
    put("activeTurnStartedAt", s(row, "active_turn_started_at"));
    put("lastTurnEndedAt", s(row, "last_turn_ended_at"));
    put("createdAt", iso(i(row, "created_at")).map(Value::String).unwrap_or(Value::Null));
    put("updatedAt", iso(i(row, "updated_at")).map(Value::String).unwrap_or(Value::Null));
    put("_count", json!({ "messages": row.get("message_count").cloned().filter(|v| !v.is_null()).unwrap_or(json!(0)) }));
    put("preview", Value::Null);
    put("summary", s(row, "summary"));
    let interrupted = if is_interrupted(row, boot_at) { s(row, "active_turn_started_at") } else { Value::Null };
    put("interruptedTurnStartedAt", interrupted);
    m
}
