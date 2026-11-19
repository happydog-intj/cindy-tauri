//! xd 目录投影：重放上游黄金样例（解析、网关模型成本、整份投影、协议配置）。
use super::xd_catalog::*;
use serde_json::{json, Value};

fn golden() -> Value {
    serde_json::from_str(include_str!("../xd_golden.json")).unwrap()
}

#[test]
fn native_api_and_profile_resolution() {
    let g = golden();
    let cases = g["resolveCases"].as_array().unwrap();
    assert!(cases.len() > 1500, "{}", cases.len());
    for c in cases {
        let id = c["id"].as_str().unwrap();
        assert_eq!(resolve_model_native_api(id).to_json_pub(), c["nativeApi"], "nativeApi {id}");
        assert_eq!(bundled_profile(id).unwrap_or(Value::Null), c["profile"], "profile {id}");
    }
}

#[test]
fn gateway_cost_matches() {
    let g = golden();
    let models = g["models"].as_array().unwrap();
    for (m, c) in models.iter().zip(g["costCases"].as_array().unwrap()) {
        assert_eq!(effective_gateway_model_cost(m).unwrap_or(Value::Null), c["cost"], "{}", m["id"]);
    }
}

#[test]
fn projection_matches() {
    let g = golden();
    let models: Vec<Value> = g["models"].as_array().unwrap().clone();
    let got = project_pi_models(&models);
    let want = g["project"].as_array().unwrap();
    assert_eq!(got.len(), want.len());
    for (a, b) in got.iter().zip(want) {
        assert_eq!(a, b, "{}", b["id"]);
    }
}

#[test]
fn spec_matches() {
    let g = golden();
    let models: Vec<Value> = g["models"].as_array().unwrap().clone();
    for c in g["specCases"].as_array().unwrap() {
        let id = c["id"].as_str().unwrap();
        let got = resolve_pi_gateway_model_spec(&models, id);
        match got {
            Err(()) => assert!(c["specUndefined"].as_bool().unwrap(), "{id}"),
            Ok(v) => {
                assert!(!c["specUndefined"].as_bool().unwrap(), "{id}");
                assert_eq!(v.unwrap_or(Value::Null), c["spec"], "{id}");
            }
        }
    }
}

#[test]
fn clamp_and_default_effort() {
    let e = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    assert_eq!(clamp_effort_to_supported("xhigh", &e(&["low", "medium"])), "medium");
    assert_eq!(clamp_effort_to_supported("minimal", &e(&["high", "max"])), "high");
    assert_eq!(default_effort_for_capabilities(&e(&["high", "low"])), Some("high".into()));
    assert_eq!(json!(default_effort_for_capabilities(&[])), Value::Null);
}
