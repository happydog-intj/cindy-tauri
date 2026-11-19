//! 布局存档校验（上游 apps/desktop/src/shared/layoutTree.ts 的 coerceLayout / validateLayout / createDefaultLayout）
//! 和项目排序归一化（packages/maker-shared/src/projectOrderSync.ts）。逐条对齐，黄金数据来自上游 TS 实际执行
//! （scripts/app-settings/layout-entry.ts → layout_golden.json）。上游变化后重跑 scripts/gen-app-settings-data.mjs。

use serde_json::{json, Map, Value};
use std::collections::HashSet;

const SCHEMA_VERSION: i64 = 1;
const MAX_TREE_DEPTH: usize = 8;
const MAX_PANEL_KIND_LENGTH: usize = 128;

pub fn create_default_layout() -> Value {
    json!({
        "schemaVersion": SCHEMA_VERSION,
        "sidebar": { "type": "pane", "id": "sessions", "panelKind": "session-list", "edge": "left" },
        "content": {
            "type": "split", "id": "root", "direction": "row",
            "children": [
                { "fraction": 0.5, "node": { "type": "pane", "id": "chat", "panelKind": "chat-main", "minWidth": 400 } },
                { "fraction": 0.5, "node": { "type": "pane", "id": "right", "panelKind": "right-tabs" } },
            ],
        },
        "float": [],
    })
}

/// JS `.length` 是 UTF-16 单元数。
fn utf16_len(s: &str) -> usize {
    s.encode_utf16().count()
}

fn is_valid_id(v: Option<&Value>) -> Option<&str> {
    v.and_then(Value::as_str).filter(|s| !s.is_empty() && utf16_len(s) <= 128)
}

fn is_valid_panel_kind(v: Option<&Value>) -> bool {
    v.and_then(Value::as_str).is_some_and(|s| !s.is_empty() && utf16_len(s) <= MAX_PANEL_KIND_LENGTH)
}

/// `Number.isFinite(x)`：只认 number 类型（字符串不转换）。JSON 里的数字必然有限，故等价于 is_number。
fn finite(v: &Value) -> Option<f64> {
    if v.is_number() { v.as_f64().filter(|f| f.is_finite()) } else { None }
}

fn bad<T>(reason: impl Into<String>) -> Result<T, String> {
    Err(reason.into())
}

struct Collected<'a> {
    ids: HashSet<String>,
    panes: Vec<&'a Map<String, Value>>,
}

fn validate_node_shape<'a>(node: Option<&'a Value>, depth: usize, c: &mut Collected<'a>) -> Result<(), String> {
    if depth > MAX_TREE_DEPTH {
        return bad(format!("tree deeper than {MAX_TREE_DEPTH}"));
    }
    let Some(node) = node.and_then(Value::as_object) else { return bad("node is not an object") };
    let Some(id) = is_valid_id(node.get("id")) else { return bad("node id invalid") };
    if !c.ids.insert(id.to_string()) {
        return bad(format!("duplicate node id: {id}"));
    }
    match node.get("type").and_then(Value::as_str) {
        Some("pane") => {
            if !is_valid_panel_kind(node.get("panelKind")) {
                return bad("panelKind invalid");
            }
            if node.get("collapsed").is_some_and(|v| !v.is_boolean()) {
                return bad("collapsed must be boolean");
            }
            if let Some(w) = node.get("minWidth") {
                if !finite(w).is_some_and(|f| f > 0.0) {
                    return bad("minWidth must be a positive finite number");
                }
            }
            if let Some(w) = node.get("preferredWidth") {
                if !finite(w).is_some_and(|f| (120.0..=16000.0).contains(&f)) {
                    return bad("preferredWidth must be between 120 and 16000");
                }
            }
            c.panes.push(node);
            Ok(())
        }
        Some("split") => {
            if !matches!(node.get("direction").and_then(Value::as_str), Some("row" | "column")) {
                return bad("split direction invalid");
            }
            let Some(children) = node.get("children").and_then(Value::as_array).filter(|a| a.len() >= 2) else {
                return bad("split needs at least 2 children");
            };
            let mut sum = 0.0;
            for child in children {
                let Some(child) = child.as_object() else { return bad("split child is not an object") };
                let Some(f) = child.get("fraction").and_then(finite).filter(|f| *f > 0.0) else {
                    return bad("child fraction must be a positive finite number");
                };
                sum += f;
                validate_node_shape(child.get("node"), depth + 1, c)?;
            }
            if (sum - 1.0).abs() > 0.01 {
                return bad(format!("split fractions must sum to ~1, got {}", js_number(sum)));
            }
            Ok(())
        }
        other => bad(format!("unknown node type: {}", other.map(String::from).unwrap_or_else(|| "undefined".into()))),
    }
}

/// 数字转字符串的 JS 形态（只用于 reason 文案）。整数不带小数点，其余用最短往返表示。
fn js_number(f: f64) -> String {
    if f.fract() == 0.0 && f.abs() < 1e21 { format!("{}", f as i64) } else { format!("{f}") }
}

/// 结构性不变量（与上游 validateLayout 的清单一一对应）。
pub fn validate_layout(input: &Value) -> Result<(), String> {
    let Some(input) = input.as_object() else { return bad("layout is not an object") };
    // JS 里 1.0 === 1：as_f64 比较同时接受 1 和 1.0。
    if input.get("schemaVersion").and_then(Value::as_f64) != Some(SCHEMA_VERSION as f64) {
        return bad(format!("schemaVersion must be {SCHEMA_VERSION}"));
    }
    if !input.get("float").and_then(Value::as_array).is_some_and(Vec::is_empty) {
        return bad("float must be an empty array in v1");
    }
    let Some(sidebar) = input.get("sidebar").and_then(Value::as_object).filter(|s| s.get("type").and_then(Value::as_str) == Some("pane")) else {
        return bad("sidebar must be a pane");
    };
    if sidebar.get("panelKind").and_then(Value::as_str) != Some("session-list") {
        return bad("sidebar must be session-list");
    }
    if sidebar.get("edge").and_then(Value::as_str) != Some("left") {
        return bad("sidebar edge must be 'left' in v1");
    }
    let Some(sidebar_id) = is_valid_id(sidebar.get("id")) else { return bad("sidebar id invalid") };
    let mut c = Collected { ids: HashSet::from([sidebar_id.to_string()]), panes: Vec::new() };
    validate_node_shape(input.get("content"), 1, &mut c)?;
    let kind = |p: &&Map<String, Value>| p.get("panelKind").and_then(Value::as_str).map(String::from);
    let chats: Vec<_> = c.panes.iter().filter(|p| kind(p).as_deref() == Some("chat-main")).collect();
    if chats.len() != 1 {
        return bad(format!("content must contain exactly one chat-main (got {})", chats.len()));
    }
    if chats[0].get("collapsed") == Some(&Value::Bool(true)) {
        return bad("chat-main must not be collapsed");
    }
    if c.panes.iter().any(|p| kind(p).as_deref() == Some("session-list")) {
        return bad("session-list must stay outside the content tree");
    }
    Ok(())
}

/// 来源不可信的数据 → 必然合法的布局。返回 (layout, fallback, reason)；永不失败。
pub fn coerce_layout(raw: &Value) -> (Value, bool, Option<String>) {
    if !raw.is_object() {
        return (create_default_layout(), true, Some("not an object".into()));
    }
    match validate_layout(raw) {
        Ok(()) => (raw.clone(), false, None),
        Err(reason) => (create_default_layout(), true, Some(reason)),
    }
}

// ── 项目排序 ──────────────────────────────────────────────

/// normalizeSyncedProjectOrderList：去重、trim、丢空串和非字符串，最多 10000 项。
pub fn normalize_order_list(v: Option<&Value>) -> Vec<String> {
    let Some(arr) = v.and_then(Value::as_array) else { return Vec::new() };
    let mut out: Vec<String> = Vec::new();
    let mut seen = HashSet::new();
    for entry in arr {
        let Some(s) = entry.as_str() else { continue };
        let key = crate::appearance_core::js_trim(s);
        if key.is_empty() || !seen.insert(key.to_string()) {
            continue;
        }
        out.push(key.to_string());
        if out.len() >= 10_000 {
            break;
        }
    }
    out
}

/// hostLocalProjectKeysOnly：只留 `local:` 前缀的键。
pub fn host_local_project_keys_only(v: Option<&Value>) -> Vec<String> {
    normalize_order_list(v).into_iter().filter(|k| k.starts_with("local:")).collect()
}

pub fn parse_order_mode(v: Option<&Value>) -> &'static str {
    if v.and_then(Value::as_str) == Some("custom") { "custom" } else { "activity" }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn golden() -> Value {
        serde_json::from_str(include_str!("layout_golden.json")).unwrap()
    }

    #[test]
    fn default_layout_matches_upstream() {
        assert_eq!(create_default_layout(), golden()["default"]);
        assert!(validate_layout(&create_default_layout()).is_ok());
    }

    #[test]
    fn coerce_matches_upstream_case_by_case() {
        let g = golden();
        let cases = g["coerceCases"].as_array().unwrap();
        assert_eq!(cases.len(), 53);
        let mut ok = 0;
        for (i, c) in cases.iter().enumerate() {
            // 黄金里 undefined 输入记作 {"__undef":true}；JSON 里没有 undefined，等价于“不是对象”
            let input = if c["input"].get("__undef").is_some() { Value::Null } else { c["input"].clone() };
            let (layout, fallback, reason) = coerce_layout(&input);
            let want = &c["output"];
            assert_eq!(fallback, want["fallback"].as_bool().unwrap(), "case {i}: {input}");
            assert_eq!(reason.as_deref(), want["reason"].as_str(), "case {i}");
            assert_eq!(&layout, &want["layout"], "case {i}");
            if !fallback {
                ok += 1;
            }
        }
        assert_eq!(ok, 12, "合法用例数与上游一致");
    }

    #[test]
    fn order_normalization_matches_upstream() {
        let g = golden();
        for c in g["orderCases"].as_array().unwrap() {
            let got = host_local_project_keys_only(Some(&c["input"]));
            let want: Vec<String> = c["keys"].as_array().unwrap().iter().map(|k| k.as_str().unwrap().to_string()).collect();
            assert_eq!(got, want, "{}", c["input"].to_string().chars().take(120).collect::<String>());
        }
        for c in g["modeCases"].as_array().unwrap() {
            let input = if c["input"].get("__undef").is_some() { None } else { Some(&c["input"]) };
            assert_eq!(parse_order_mode(input), c["mode"].as_str().unwrap(), "{}", c["input"]);
        }
    }
}
