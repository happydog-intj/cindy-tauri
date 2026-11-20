//! 没有专属实现者的 renderer 通道：首屏默认值、降级通道、未实现的错误形状。
//! 原先由 Node sidecar 的 ipc.mjs / capture-degraded.mjs / maker-ipc.mjs 应答，现在在 Rust 里。
//! 返回值是未编码的 JSON 值；`UNDEFINED` 标记对应 JS 的 undefined（src/shared/ipc-codec.mjs 的 `u` 标签）。
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::sync::{Mutex, OnceLock};

/// 冒烟与排迁移优先级用的统计：未实现通道的次数、渲染层 error 日志（最多 50 条）。
#[derive(Default)]
struct Stats {
    missing: [BTreeMap<String, u64>; 3],
    renderer_errors: Vec<Value>,
}
static STATS: Mutex<Option<Stats>> = Mutex::new(None);

fn with_stats<R>(f: impl FnOnce(&mut Stats) -> R) -> R {
    let mut g = STATS.lock().unwrap_or_else(|e| e.into_inner());
    f(g.get_or_insert_with(Stats::default))
}

fn miss(kind: usize, channel: &str) {
    with_stats(|s| *s.missing[kind].entry(channel.to_string()).or_default() += 1);
}

/// `ipc_stats` 命令的返回值。
pub fn stats() -> Value {
    with_stats(|s| {
        let m = |i: usize| serde_json::to_value(&s.missing[i]).unwrap_or(Value::Null);
        json!({ "rendererErrors": s.renderer_errors, "missing": { "invoke": m(0), "send": m(1), "sendSync": m(2) } })
    })
}

pub fn undefined() -> Value {
    json!({ "__cindy_t": "u" })
}

static VERSION: OnceLock<String> = OnceLock::new();

pub fn init(version: &str) {
    let _ = VERSION.set(version.to_string());
}

fn version() -> &'static str {
    VERSION.get().map(String::as_str).unwrap_or("0.0.0")
}

fn is_undefined(v: Option<&Value>) -> bool {
    match v {
        None => true,
        Some(o) => o.get("__cindy_t").and_then(Value::as_str) == Some("u"),
    }
}

fn os_release() -> &'static str {
    static V: OnceLock<String> = OnceLock::new();
    V.get_or_init(|| {
        #[cfg(unix)]
        let out = std::process::Command::new("uname").arg("-r").output().ok().and_then(|o| String::from_utf8(o.stdout).ok()).map(|s| s.trim().to_string());
        // `ver` 输出形如 "Microsoft Windows [Version 10.0.22631.4602]"，取 Node os.release() 的三段式。
        #[cfg(windows)]
        let out = std::process::Command::new("cmd").args(["/C", "ver"]).output().ok().and_then(|o| String::from_utf8(o.stdout).ok()).and_then(|s| {
            let v = s.split("Version ").nth(1)?.trim_end_matches(|c: char| c == ']' || c.is_whitespace()).to_string();
            Some(v.split('.').take(3).collect::<Vec<_>>().join("."))
        });
        out.filter(|s| !s.is_empty()).unwrap_or_default()
    })
}

/// 首屏 sendSync 的默认值：形状与 desktop 主进程一致，内容是「没有任何用户数据」。`None` = undefined。
pub fn sync(channel: &str) -> Option<Value> {
    Some(match channel {
        "app-locale:get-preferred-system-locale-sync" => json!(crate::shell_policy::system_locale()),
        "get-app-display-version-info" => json!({ "display": format!("{} · tauri", version()), "detail": format!("{} (tauri)", version()) }),
        "client-endpoints:get-sync" => json!({ "websiteUrl": crate::auth::env::website_url() }),
        "get-os-release" => json!(os_release()),
        "get-app-version" => json!(version()),
        "ghosts:recommendations" => json!({ "ownerId": null, "sources": [], "recentIds": [], "newlyInstalledId": null }),
        "ghosts:list" => json!({ "ghosts": [] }),
        "ghosts:recent-usage" => json!({ "ids": [] }),
        "ghosts:unread" => json!({ "entries": [] }),
        "ghosts:errand-prefs" => json!({ "config": {}, "auxiliaryModelLabel": null }),
        "ghosts:workdir-prefs" => json!({ "disabled": [] }),
        "sidebar-settings:claim-renderer-legacy-owner-sync" | "sidebar-settings:load-snapshot-sync" => Value::Null,
        _ => {
            miss(2, channel);
            return None;
        }
    })
}

fn unsupported(what: &str) -> Result<Value, String> {
    Err(format!("[UNSUPPORTED_CAPABILITY] {what} is not available in the Tauri build"))
}

fn safe_id(v: Option<&Value>) -> bool {
    v.and_then(Value::as_str).is_some_and(|s| {
        !s.is_empty() && s.chars().count() <= 128 && s != "." && s != ".." && s.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | ':' | '-'))
    })
}

/// invoke 通道的兜底。截图 / 录屏 / 远程桌面不移植（DESIGN.md 9.7）：读取类通道回「关闭 / 不可用」，会改状态的入口报 UNSUPPORTED_CAPABILITY。
pub fn invoke(channel: &str, args: &[Value]) -> Result<Value, String> {
    let r = match channel {
        "remote-desktop:state" => {
            let check = args.first();
            if !is_undefined(check) && !check.is_some_and(Value::is_boolean) {
                return Err("[INVALID_PARAMS] Invalid Windows support status request".into());
            }
            let mut v = json!({ "enabled": false, "active": null, "overlay": false, "permissionGuide": false });
            if check == Some(&Value::Bool(true)) {
                v["windowsSupport"] = json!("unavailable");
            }
            Ok(v)
        }
        "remote-desktop:permissions" => Ok(json!({ "screenRecording": "unknown", "accessibility": "unknown" })),
        "remote-desktop:enable" | "remote-desktop:windows-support" | "remote-desktop:open-permission" => unsupported("remote desktop"),
        "remote-desktop:stop" | "remote-desktop:dismiss-permission-guide" => Ok(undefined()),
        "region-screenshot:compose-take" => Ok(Value::Null),
        // 渲染层换模型时总会先走这个通道。只有 Pi 一个引擎：同引擎 = no-op 成功，跨引擎不支持。
        "maker:switch-session-agent" => {
            if !safe_id(args.first()) {
                return Err("[INVALID_PARAMS] sessionId required".into());
            }
            let target = args.get(1).and_then(Value::as_str).unwrap_or("");
            if target != "pi" {
                return Err(format!("[UNSUPPORTED_CAPABILITY] agent {target} is not available in the Tauri build"));
            }
            Ok(json!({ "switched": false, "agentKind": "pi", "model": args.get(2).cloned().unwrap_or_else(undefined), "engineReady": true, "sameEngineRevision": 0 }))
        }
        _ => {
            miss(0, channel);
            Err(format!("[NOT_IMPLEMENTED] {channel} is not available in the Tauri build yet"))
        }
    };
    if let Err(e) = &r {
        // 渲染层多半只弹通用提示，这里记下通道和错误码（只取首行前 300 字，不含参数）。
        eprintln!("[ipc] invoke {channel} failed: {}", e.lines().next().unwrap_or("").chars().take(300).collect::<String>());
    }
    r
}

/// send 通道的兜底：未实现的直接丢弃，renderer 日志落到应用日志。
pub fn send(channel: &str, args: &[Value]) {
    if channel != "renderer:log" {
        miss(1, channel);
        return;
    }
    let s = |i: usize| args.get(i).and_then(Value::as_str).unwrap_or("");
    let level = s(0);
    if matches!(level, "error" | "fatal") {
        with_stats(|st| {
            if st.renderer_errors.len() < 50 {
                st.renderer_errors.push(json!({ "scope": s(1), "msg": s(2).chars().take(500).collect::<String>() }));
            }
        });
    }
    if matches!(level, "error" | "fatal" | "warn") {
        eprintln!("[renderer:{level}] {}: {}", s(1), s(2).chars().take(2000).collect::<String>());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_screen_defaults_and_unknown_channels() {
        init("1.2.3");
        assert_eq!(sync("get-app-version"), Some(json!("1.2.3")));
        assert_eq!(sync("get-app-display-version-info"), Some(json!({ "display": "1.2.3 · tauri", "detail": "1.2.3 (tauri)" })));
        assert_eq!(sync("ghosts:list"), Some(json!({ "ghosts": [] })));
        assert_eq!(sync("sidebar-settings:load-snapshot-sync"), Some(Value::Null));
        assert_eq!(sync("nope:sync"), None);
        assert!(sync("get-os-release").unwrap().as_str().is_some());
        assert!(sync("app-locale:get-preferred-system-locale-sync").unwrap().as_str().is_some());
    }

    #[test]
    fn degraded_capture_channels_keep_their_shapes() {
        assert_eq!(invoke("remote-desktop:state", &[]).unwrap(), json!({ "enabled": false, "active": null, "overlay": false, "permissionGuide": false }));
        assert_eq!(invoke("remote-desktop:state", &[json!(true)]).unwrap()["windowsSupport"], "unavailable");
        assert!(invoke("remote-desktop:state", &[json!("x")]).unwrap_err().contains("INVALID_PARAMS"));
        assert!(invoke("remote-desktop:enable", &[]).unwrap_err().contains("UNSUPPORTED_CAPABILITY"));
        assert_eq!(invoke("remote-desktop:stop", &[]).unwrap(), undefined());
        assert_eq!(invoke("region-screenshot:compose-take", &[]).unwrap(), Value::Null);
        assert!(invoke("whatever:else", &[]).unwrap_err().starts_with("[NOT_IMPLEMENTED] whatever:else"));
    }

    #[test]
    fn switching_to_the_same_engine_is_a_noop_and_other_engines_are_refused() {
        let ok = invoke("maker:switch-session-agent", &[json!("s1"), json!("pi"), json!("m")]).unwrap();
        assert_eq!(ok, json!({ "switched": false, "agentKind": "pi", "model": "m", "engineReady": true, "sameEngineRevision": 0 }));
        assert!(invoke("maker:switch-session-agent", &[json!("s1"), json!("codex")]).unwrap_err().contains("UNSUPPORTED_CAPABILITY"));
        assert!(invoke("maker:switch-session-agent", &[json!("../x"), json!("pi")]).unwrap_err().contains("INVALID_PARAMS"));
        assert!(invoke("maker:switch-session-agent", &[json!(".."), json!("pi")]).unwrap_err().contains("INVALID_PARAMS"));
    }
}
