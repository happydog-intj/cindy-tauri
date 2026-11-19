//! provider 视图：重放上游 buildUserProvider + buildRegistry 对 218 个受限配置的输出。
use super::view::*;
use serde_json::Value;
use std::collections::HashMap;

fn golden() -> Value {
    serde_json::from_str(include_str!("../provider_golden.json")).unwrap()
}

#[test]
fn views_match_upstream() {
    let g = golden();
    let configs = g["configs"].as_array().unwrap();
    let want = g["views"].as_array().unwrap();
    assert_eq!(configs.len(), want.len());
    let connected: HashMap<String, bool> = g["connected"].as_object().unwrap().iter().map(|(k, v)| (k.clone(), v.as_bool().unwrap())).collect();
    let built: Vec<Value> = configs.iter().map(build_user_provider).collect();
    let got = build_registry(&built, &connected);
    let mut bad = 0;
    for (i, (a, b)) in got.iter().zip(want).enumerate() {
        if a != b {
            bad += 1;
            if bad <= 3 {
                let (ma, mb) = (a["models"]["pi"].as_array().unwrap(), b["models"]["pi"].as_array().unwrap());
                for (x, y) in ma.iter().zip(mb) {
                    if x != y {
                        eprintln!("config #{i} {}\n got  {x}\n want {y}", configs[i]["runtimes"]["pi"]["baseUrl"]);
                        break;
                    }
                }
                if a["routing"] != b["routing"] || a["access"] != b["access"] {
                    eprintln!("config #{i} routing/access\n got  {} {}\n want {} {}", a["routing"], a["access"], b["routing"], b["access"]);
                }
            }
        }
    }
    assert_eq!(bad, 0, "{bad}/{} 个视图不一致", want.len());
}

#[test]
fn new_route_admission_matches_upstream() {
    let g = golden();
    let cases = g["selectable"].as_array().unwrap();
    assert!(cases.len() > 14_000, "{}", cases.len());
    let mut bad = vec![];
    for c in cases {
        let got = super::classify::is_model_selectable_for_new_route(&c["model"], c["user"].as_bool().unwrap());
        if got != c["want"].as_bool().unwrap() {
            bad.push(format!("{} user={} want={}", c["model"], c["user"], c["want"]));
        }
    }
    assert!(bad.is_empty(), "{} 个不一致，例如 {:?}", bad.len(), &bad[..bad.len().min(5)]);
}

#[test]
fn models_list_parsing_matches_upstream() {
    let g = golden();
    let cases = g["listCases"].as_array().unwrap();
    assert!(cases.len() >= 70, "{}", cases.len());
    let mut bad = vec![];
    for c in cases {
        let got = super::discovery::parse_models_list_response(&c["json"], c["url"].as_str());
        let got = got.map_or(Value::Null, Value::Array);
        if got != c["out"] {
            bad.push(format!("url={} json={}\n got  {}\n want {}", c["url"], c["json"], got, c["out"]));
        }
    }
    assert!(bad.is_empty(), "{} 个不一致:\n{}", bad.len(), bad.iter().take(3).cloned().collect::<Vec<_>>().join("\n"));
}

// ── 启动参数（迁自 scripts/rust-launch-test.mjs）──────────────────────────
mod launch {
    use crate::model_config::manager::{build_custom_launch, build_gateway_launch};
    use serde_json::{json, Value};

    fn gw_cfg() -> Value {
        json!({ "providerId": "xd", "providerName": "Cindy AI", "modelId": "claude-x", "gateway": true, "reasoning": true })
    }
    fn up() -> Option<(String, String)> {
        Some(("gw-secret".into(), "https://gw.example.com/".into()))
    }
    fn descriptor() -> Value {
        json!({ "displayName": "Claude X", "efforts": ["low", "high"], "supportsImageInput": true, "contextWindow": 200000, "maxOutputTokens": 32000, "cost": { "input": 3, "output": 15 } })
    }
    fn code(r: Result<Value, String>) -> String {
        r.unwrap_err()
    }

    #[test]
    fn gateway_anthropic_uses_the_root_and_both_auth_headers() {
        let r = build_gateway_launch(&gw_cfg(), Some(&json!({ "api": "anthropic-messages" })), Some(&descriptor()), up()).unwrap();
        assert_eq!(r["provider"]["baseUrl"], "https://gw.example.com");
        assert_eq!(r["provider"]["apiKeyHeaders"], json!(["authorization", "x-api-key"]));
        assert!(r["key"] == "gw-secret" && !r["provider"].to_string().contains("gw-secret"));
        assert!(r["providerId"] == "cindy" && r["keyKind"] == "gateway");
        let m = &r["provider"]["models"][0];
        assert!(m["name"] == "Claude X" && m["reasoning"] == true && m["contextWindow"] == 200000 && m["maxTokens"] == 32000 && m["cost"]["output"] == 15 && m["cost"]["cacheRead"] == 0, "{m}");
        assert_eq!(m["input"], json!(["text", "image"]));
    }

    #[test]
    fn gateway_openai_appends_v1_once_and_honours_text_only_and_compat() {
        let mut d = descriptor();
        d["supportsImageInput"] = json!(false);
        let a = build_gateway_launch(&gw_cfg(), Some(&json!({ "api": "openai-responses", "compat": { "x": 1 } })), Some(&d), up()).unwrap();
        assert_eq!(a["provider"]["baseUrl"], "https://gw.example.com/v1");
        assert_eq!(a["provider"]["models"][0]["input"], json!(["text"]));
        assert_eq!(a["provider"]["models"][0]["compat"]["x"], 1);
        let b = build_gateway_launch(&gw_cfg(), Some(&json!({ "api": "openai-completions" })), Some(&descriptor()), Some(("k".into(), "https://gw.example.com/v1".into()))).unwrap();
        assert_eq!(b["provider"]["baseUrl"], "https://gw.example.com/v1");
    }

    #[test]
    fn gateway_fails_closed() {
        assert!(code(build_gateway_launch(&gw_cfg(), Some(&json!({ "api": "anthropic-messages" })), None, None)).starts_with("[AUTH_INVALID]"));
        assert!(code(build_gateway_launch(&gw_cfg(), Some(&Value::Null), None, up())).starts_with("[MODEL_NOT_CONFIGURED]"));
        assert!(code(build_gateway_launch(&gw_cfg(), Some(&json!({ "api": null })), None, up())).starts_with("[MODEL_NOT_CONFIGURED]"));
    }

    fn custom_cfg() -> Value {
        json!({ "providerId": "my-llm", "providerName": "My LLM", "modelId": "m1", "baseUrl": "http://127.0.0.1:11434/v1", "api": "openai-completions", "authMethod": "apiKey", "reasoning": false, "contextLimit": 32000 })
    }

    #[test]
    fn custom_provider_key_goes_to_the_file_not_the_block() {
        let r = build_custom_launch(&custom_cfg(), |id| (id == "my-llm").then(|| "sk-user".to_string())).unwrap();
        assert!(r["key"] == "sk-user" && !r["provider"].to_string().contains("sk-user"));
        assert!(r["provider"].get("apiKey").is_none() && r["keyKind"] == "provider");
        assert_eq!(r["provider"]["models"][0]["contextWindow"], 32000);
        assert!(code(build_custom_launch(&custom_cfg(), |_| None)).starts_with("[AUTH_INVALID]"));
    }

    #[test]
    fn keyless_provider_uses_the_placeholder_and_reserved_ids_are_prefixed() {
        let mut cfg = custom_cfg();
        cfg["authMethod"] = json!("none");
        let k = build_custom_launch(&cfg, |_| panic!("keyless 不应读 key")).unwrap();
        assert!(k["key"].is_null() && k["keyKind"] == "none" && k["provider"]["apiKey"] == "cindy-tauri-keyless");
        cfg["providerId"] = json!("openai");
        assert_eq!(build_custom_launch(&cfg, |_| None).unwrap()["providerId"], "cindy-tauri-openai");
    }
}
