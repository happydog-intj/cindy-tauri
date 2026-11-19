//! pi-rs 会话的启动准备：写 models.json / key 文件 / 权限档文件，拼出子进程参数与环境。
//!
//! 输入是 sidecar 的 `host.resolve-launch` 结果（provider 块 + 待写入的 key），不依赖 sidecar 本身，
//! 因此可以单测。密钥的处理：
//! - key 只写进 0600 的文件，pi-rs 用 `apiKeyFile` 现读，所以轮换只需改写文件；
//! - key 不进 models.json、不进环境变量、不进命令行（`ps` 看不到）；
//! - 文件放在会话的运行目录（`<data>/pi-runtime/<session>/`），关闭会话时一并删除。

#![allow(dead_code)]

use serde_json::{json, Value};
use std::path::{Path, PathBuf};

#[derive(Debug)]
pub struct LaunchPlan {
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
    pub cwd: PathBuf,
    pub runtime_dir: PathBuf,
    pub key_file: Option<PathBuf>,
}

/// 与 maker-ipc 的 SAFE_ID 同规则，防止 session id 把运行目录带出 data 目录。
fn safe_component(id: &str) -> bool {
    !id.is_empty() && id != "." && id != ".." && id.chars().all(|c| c.is_ascii_alphanumeric() || "_.:-".contains(c))
}

#[cfg(unix)]
fn write_private(path: &Path, content: &str) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut f = std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(path)?;
    f.write_all(content.as_bytes())
}

#[cfg(not(unix))]
fn write_private(path: &Path, content: &str) -> std::io::Result<()> {
    std::fs::write(path, content)
}

/// 轮换 key：原地改写，pi-rs 下一个请求就会读到新值。
pub fn rotate_key(key_file: &Path, key: &str) -> std::io::Result<()> {
    write_private(key_file, key)
}

/// 写出启动所需的全部文件并返回子进程参数。
///
/// `launch`：`{ providerId, modelId, keyKind, key, provider }`（见 sidecar pi-launch.mjs）。
/// `permission_mode`：ask / auto / bypassPermissions 等，写进权限档文件（pi-rs 每次现读）。
pub fn prepare(
    data_dir: &Path,
    session_id: &str,
    working_dir: &Path,
    launch: &Value,
    permission_mode: &str,
) -> Result<LaunchPlan, String> {
    if !safe_component(session_id) {
        return Err("[INVALID_PARAMS] invalid session id".into());
    }
    let provider_id = launch["providerId"].as_str().filter(|s| !s.is_empty()).ok_or("[MODEL_NOT_CONFIGURED] launch.providerId missing")?;
    let model_id = launch["modelId"].as_str().filter(|s| !s.is_empty()).ok_or("[MODEL_NOT_CONFIGURED] launch.modelId missing")?;
    if !launch["provider"].is_object() {
        return Err("[MODEL_NOT_CONFIGURED] launch.provider missing".into());
    }

    let runtime_dir = data_dir.join("pi-runtime").join(session_id);
    let agent_dir = runtime_dir.join("agent");
    let sessions_dir = data_dir.join("pi-sessions").join(session_id);
    for d in [&agent_dir, &sessions_dir] {
        std::fs::create_dir_all(d).map_err(|e| format!("create {}: {e}", d.display()))?;
    }
    std::fs::create_dir_all(working_dir).map_err(|e| format!("create {}: {e}", working_dir.display()))?;

    // key 文件（可选）：写 0600，provider 块里只引用路径。
    let mut provider = launch["provider"].clone();
    let key_file = match launch["key"].as_str().filter(|k| !k.is_empty()) {
        Some(key) => {
            let f = runtime_dir.join("provider.key");
            write_private(&f, key).map_err(|e| format!("write key file: {e}"))?;
            provider["apiKeyFile"] = json!(f.to_string_lossy());
            Some(f)
        }
        None => None,
    };
    // 防御：provider 块里不应再带明文 key 字段（sidecar 已剔除，这里兜底）。
    if key_file.is_some() {
        if let Some(o) = provider.as_object_mut() {
            o.remove("apiKey");
        }
    }

    let models = json!({ "providers": { provider_id: provider } });
    std::fs::write(agent_dir.join("models.json"), serde_json::to_string_pretty(&models).unwrap() + "\n")
        .map_err(|e| format!("write models.json: {e}"))?;

    // 权限档：pi-rs 每次现读，读不到按 ask（fail-closed）。
    let perm_file = runtime_dir.join("permission.json");
    std::fs::write(&perm_file, json!({ "mode": permission_mode }).to_string()).map_err(|e| format!("write permission file: {e}"))?;

    let mut args: Vec<String> = [
        "--mode", "rpc",
        // bridge 身份：让 pi-rs 进入权限门（不带 --no-approve）。pi-rs 不加载扩展内容，只认名字。
        "--extension", "cindy-bridge.ts",
        "--no-skills", "--no-context-files", "--no-prompt-templates",
    ].iter().map(|s| s.to_string()).collect();
    args.extend(["--session-dir".into(), sessions_dir.to_string_lossy().into(), "--provider".into(), provider_id.into(), "--model".into(), model_id.into()]);
    // 续接历史不走命令行：pi-rs 没有 --session 参数（传了会直接报 Unknown option 退出），
    // 由工厂在进程起来后发 `switch_session` RPC 命令。

    let env = vec![
        ("PI_CODING_AGENT_DIR".to_string(), agent_dir.to_string_lossy().into_owned()),
        ("CINDY_PI_PERMISSION_FILE".to_string(), perm_file.to_string_lossy().into_owned()),
        ("PI_OFFLINE".to_string(), "1".to_string()),
    ];
    Ok(LaunchPlan { args, env, cwd: working_dir.to_path_buf(), runtime_dir, key_file })
}

/// 切换权限档（`maker:set-permission-mode`）：改写文件即可，pi-rs 下次工具调用就按新档位。
pub fn set_permission_mode(runtime_dir: &Path, mode: &str) -> std::io::Result<()> {
    std::fs::write(runtime_dir.join("permission.json"), json!({ "mode": mode }).to_string())
}

/// 会话关闭：删掉运行目录（含 key 文件）。会话历史在 pi-sessions 里，保留。
pub fn cleanup(runtime_dir: &Path) {
    let _ = std::fs::remove_dir_all(runtime_dir);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("pi-launch-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn launch(key: Option<&str>) -> Value {
        json!({
            "providerId": "cindy", "modelId": "m1", "keyKind": "gateway", "key": key,
            "provider": { "name": "Cindy AI", "baseUrl": "https://gw.example.com", "api": "anthropic-messages",
                          "apiKeyHeaders": ["authorization", "x-api-key"], "apiKey": "LEAK-ME", "models": [{ "id": "m1" }] }
        })
    }

    #[test]
    fn writes_models_json_with_key_file_reference_and_no_plaintext_key() {
        let d = tmp("keys");
        let plan = prepare(&d, "s1", &d.join("work"), &launch(Some("gw-secret")), "ask").unwrap();
        let models = std::fs::read_to_string(plan.runtime_dir.join("agent/models.json")).unwrap();
        assert!(!models.contains("gw-secret"), "明文 key 不进 models.json");
        assert!(!models.contains("LEAK-ME"), "provider 里残留的 apiKey 被剔除");
        let v: Value = serde_json::from_str(&models).unwrap();
        let f = v["providers"]["cindy"]["apiKeyFile"].as_str().unwrap();
        assert_eq!(std::fs::read_to_string(f).unwrap(), "gw-secret");
        assert_eq!(plan.key_file.as_deref(), Some(Path::new(f)));
        assert_eq!(v["providers"]["cindy"]["apiKeyHeaders"], json!(["authorization", "x-api-key"]));
        assert!(!plan.args.iter().any(|a| a.contains("gw-secret")), "key 不在命令行");
        assert!(!plan.env.iter().any(|(_, v)| v.contains("gw-secret")), "key 不在环境变量");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(f).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "key 文件仅属主可读写");
        }
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn rotation_rewrites_in_place_and_cleanup_removes_secrets_but_keeps_history() {
        let d = tmp("rotate");
        let plan = prepare(&d, "s1", &d.join("work"), &launch(Some("k1")), "ask").unwrap();
        let kf = plan.key_file.clone().unwrap();
        rotate_key(&kf, "k2").unwrap();
        assert_eq!(std::fs::read_to_string(&kf).unwrap(), "k2");
        assert!(d.join("pi-sessions/s1").is_dir());
        cleanup(&plan.runtime_dir);
        assert!(!kf.exists() && !plan.runtime_dir.exists(), "关闭后 key 文件被删");
        assert!(d.join("pi-sessions/s1").is_dir(), "会话历史保留");
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn keyless_provider_keeps_its_placeholder_and_writes_no_key_file() {
        let d = tmp("keyless");
        let l = json!({ "providerId": "local", "modelId": "m", "keyKind": "none", "key": null,
                        "provider": { "baseUrl": "http://127.0.0.1:11434/v1", "api": "openai-completions", "apiKey": "cindy-tauri-keyless", "models": [{ "id": "m" }] } });
        let plan = prepare(&d, "s2", &d.join("w"), &l, "ask").unwrap();
        assert!(plan.key_file.is_none());
        let v: Value = serde_json::from_str(&std::fs::read_to_string(plan.runtime_dir.join("agent/models.json")).unwrap()).unwrap();
        assert_eq!(v["providers"]["local"]["apiKey"], "cindy-tauri-keyless");
        assert!(v["providers"]["local"].get("apiKeyFile").is_none());
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn args_env_and_permission_file_follow_the_wire_contract() {
        let d = tmp("args");
        let plan = prepare(&d, "s3", &d.join("w"), &launch(Some("k")), "bypassPermissions").unwrap();
        let a = plan.args.join(" ");
        assert!(a.contains("--mode rpc") && a.contains("--extension cindy-bridge.ts"), "bridge 模式");
        assert!(!a.contains("--no-approve"), "不能带 --no-approve，否则权限门被整段绕过");
        assert!(a.contains("--provider cindy --model m1"));
        assert!(!plan.args.iter().any(|x| x == "--session"), "pi-rs 不认 --session，续接走 switch_session RPC");
        let env: std::collections::HashMap<_, _> = plan.env.iter().cloned().collect();
        let perm = std::fs::read_to_string(&env["CINDY_PI_PERMISSION_FILE"]).unwrap();
        assert_eq!(serde_json::from_str::<Value>(&perm).unwrap()["mode"], "bypassPermissions");
        set_permission_mode(&plan.runtime_dir, "ask").unwrap();
        assert!(std::fs::read_to_string(&env["CINDY_PI_PERMISSION_FILE"]).unwrap().contains("\"ask\""));
        assert!(env["PI_CODING_AGENT_DIR"].ends_with("agent"));
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn rejects_unsafe_session_ids_and_incomplete_launches() {
        let d = tmp("bad");
        for id in ["", "..", "../etc", "a/b", "a b"] {
            assert!(prepare(&d, id, &d, &launch(None), "ask").unwrap_err().starts_with("[INVALID_PARAMS]"), "{id}");
        }
        assert!(prepare(&d, "ok", &d, &json!({ "providerId": "p" }), "ask").unwrap_err().starts_with("[MODEL_NOT_CONFIGURED]"));
        assert!(!d.join("pi-runtime").exists(), "非法请求不应创建任何目录");
        std::fs::remove_dir_all(&d).unwrap();
    }
}
