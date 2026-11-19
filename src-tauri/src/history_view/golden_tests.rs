//! 与上游 TS 输出逐项对照（history_view_golden.json 由 scripts/gen-app-settings-data.mjs 生成）。
use super::{payload::has_visible_history_result, project::*, text::*};
use serde_json::Value;

fn golden() -> Value {
    serde_json::from_str(include_str!("../history_view_golden.json")).unwrap()
}

#[test]
fn projection_matches_upstream() {
    let g = golden();
    let cases = g["cases"].as_array().unwrap();
    let mut bad = Vec::new();
    for c in cases {
        let rows = c["rows"].as_array().unwrap();
        let got = Value::Array(project_history_view(rows, c["streaming"].as_bool().unwrap(), c["lazy"].as_bool().unwrap()));
        if serde_json::to_string(&got).unwrap() != serde_json::to_string(&c["out"]).unwrap() {
            bad.push(c["name"].as_str().unwrap().to_owned());
        }
    }
    assert!(bad.is_empty(), "{} / {} 个用例不一致: {:?}", bad.len(), cases.len(), bad);
}

fn table(g: &Value, k: &str) -> Vec<Value> {
    g["fn"][k].as_array().unwrap().clone()
}

#[test]
fn small_functions_match_upstream() {
    let g = golden();
    for c in table(&g, "visible") {
        assert_eq!(has_visible_history_result(&c["in"]), c["out"].as_bool().unwrap(), "visible {}", c["in"]);
    }
    for c in table(&g, "completes") {
        assert_eq!(completes_history_turn(&serde_json::json!({ "agentMeta": c["in"] })), c["out"].as_bool().unwrap(), "completes {}", c["in"]);
    }
    for c in table(&g, "detail") {
        assert_eq!(is_history_detail_tool(c["in"].as_str().unwrap()), c["out"].as_bool().unwrap(), "detail {}", c["in"]);
    }
    for c in table(&g, "delivery") {
        assert_eq!(is_delivery_prose_text(c["in"].as_str().unwrap()), c["out"].as_bool().unwrap(), "delivery {}", c["in"]);
    }
    for c in table(&g, "continuation") {
        assert_eq!(is_continuation_message(&c["in"]), c["out"].as_bool().unwrap(), "continuation {}", c["in"]);
    }
}
