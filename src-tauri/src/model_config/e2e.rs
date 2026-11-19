//! 迁自 scripts/session-e2e.mjs：带 API key 的自定义 provider 全流程（钥匙串、models-fetch、连接测试、重启、改端点、断开）。
//! 与生产同一套 Rust 管理器（Rig 装配），mock 模型进程提供 /keyed/* 带鉴权的接口。
use crate::rust_session_e2e::{env, start_mock_logged, temp, Rig};
use serde_json::{json, Value};

const API_KEY: &str = "sk-e2e-0123456789abcdef01234567";

fn keyed_config(base: &str) -> Value {
    json!({ "id": "keyed", "name": "Keyed", "auth": { "method": "apiKey" },
        "runtimes": { "pi": { "baseUrl": base, "wireProtocol": "openai-chat", "models": [{ "id": "mock-model", "name": "Mock (keyed)" }] } } })
}

fn keyed_view(r: &Rig) -> Option<Value> {
    r.ipc("maker:provider:list", json!([])).unwrap()["providers"].as_array().unwrap().iter().find(|p| p["id"] == "keyed").cloned()
}

#[test]
fn keyed_provider_lifecycle_with_secure_storage_and_restart() {
    let Some(e) = env() else { return };
    let (mut mock, port, log) = start_mock_logged(&e, &[("MOCK_API_KEY", API_KEY)]);
    let data = temp("keyed");
    let r = Rig::start(&e, port, &data);
    let keyed_url = format!("http://127.0.0.1:{port}/keyed");

    // 可选模型清单：smoke provider 的 mock-model 在里面。
    let models = r.models.call("host.pi-models", &json!({})).unwrap().unwrap();
    assert!(models.as_array().unwrap().iter().any(|m| m["id"] == "mock-model"), "{models}");

    // 没 key → AUTH_INVALID；带 key → 列出模型。
    let no_key = r.ipc("maker:provider:models-fetch", json!([{ "agent": "pi", "baseUrl": keyed_url, "authMethod": "apiKey" }])).unwrap();
    assert_eq!((no_key["ok"].clone(), no_key["code"].clone()), (json!(false), json!("AUTH_INVALID")), "{no_key}");
    let with_key = r.ipc("maker:provider:models-fetch", json!([{ "agent": "pi", "baseUrl": keyed_url, "authMethod": "apiKey", "apiKey": API_KEY }])).unwrap();
    assert!(with_key["ok"] == true && with_key["models"].as_array().unwrap().iter().any(|m| m["id"] == "mock-model"), "{with_key}");

    // 创建：key 进钥匙串，provider 变为已连接，渲染层读得回。
    assert_eq!(r.ipc("maker:provider:custom:create", json!([keyed_config(&keyed_url), { "pi": API_KEY }, {}])).unwrap()["ok"], true);
    assert_eq!(keyed_view(&r).unwrap()["connected"], true);
    assert_eq!(r.secrets.get("provider_key_keyed_pi").as_deref(), Some(API_KEY));
    let saved = r.ipc("maker:provider:test-connection", json!([{ "kind": "saved", "providerId": "keyed", "agent": "pi" }])).unwrap();
    assert_eq!(saved["ok"], true, "{saved}");
    let fetch_saved = r.ipc("maker:provider:models-fetch", json!([{ "agent": "pi", "baseUrl": keyed_url, "authMethod": "apiKey", "savedProviderId": "keyed" }])).unwrap();
    assert_eq!(fetch_saved["ok"], true, "{fetch_saved}");
    // 启动参数：自定义 provider 的 key 只在 key 字段里，provider 块不带 key。
    let launch = r.models.call("host.resolve-launch", &json!({ "providerId": "keyed", "model": "mock-model" })).unwrap().unwrap();
    assert_eq!((launch["keyKind"].clone(), launch["key"].clone(), launch["providerId"].clone()), (json!("provider"), json!(API_KEY), json!("keyed")));
    assert!(!launch["provider"].to_string().contains(API_KEY));

    // 改端点不带新 key → 旧 key 清掉；重新给 key → 恢复；断开 → 清 key、保留配置。
    r.ipc("maker:provider:custom:update", json!([keyed_config(&format!("http://localhost:{port}/keyed")), {}, {}])).unwrap();
    assert_eq!(keyed_view(&r).unwrap()["connected"], false);
    assert!(r.secrets.get("provider_key_keyed_pi").is_none());
    let err = r.models.call("host.resolve-launch", &json!({ "providerId": "keyed", "model": "mock-model" })).unwrap();
    assert!(err.is_err(), "未连接的 provider 不能启动: {err:?}");
    r.ipc("maker:provider:custom:update", json!([keyed_config(&keyed_url), { "pi": API_KEY }, {}])).unwrap();
    assert_eq!(keyed_view(&r).unwrap()["connected"], true);

    // 重启（同一数据目录，钥匙串重新解锁）后仍是已连接。
    let secrets_file_has_plaintext = std::fs::read_dir(&data).unwrap().filter_map(Result::ok).filter(|f| f.path().is_file()).any(|f| std::fs::read(f.path()).map(|b| String::from_utf8_lossy(&b).contains(API_KEY)).unwrap_or(false));
    assert!(!secrets_file_has_plaintext, "数据目录里不能出现明文 key");
    r.stop();
    let r = Rig::start(&e, port, &data);
    assert_eq!(keyed_view(&r).unwrap()["connected"], true, "重启后 key 仍在");

    let scope = json!({ "dataOwnerId": r.ipc("maker:provider:list", json!([])).unwrap()["dataOwnerId"], "ownerGeneration": r.ipc("maker:provider:list", json!([])).unwrap()["ownerGeneration"] });
    assert_eq!(r.ipc("maker:provider:custom:disconnect", json!(["keyed", scope, {}])).unwrap()["ok"], true);
    assert_eq!(keyed_view(&r).unwrap()["connected"], false);
    assert!(r.secrets.get("provider_key_keyed_pi").is_none());
    assert!(!log.lock().unwrap().iter().any(|l| l.contains(API_KEY)), "mock 日志不能出现 key");
    r.stop();
    let _ = mock.kill();
}
