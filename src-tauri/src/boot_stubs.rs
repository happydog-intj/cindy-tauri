//! 首屏启动需要的最小 handler（原 sidecar boot-stubs.mjs）：返回值形状与 desktop 主进程一致，
//! 内容是“本机模式、没有任何用户数据”的占位；真正有状态的两块是布局存档和侧栏项目排序。
//! - 布局：<dataDir>/layout.v1.json，读写都过 layout_core（上游 coerceLayout）；
//! - 项目排序：<dataDir>/project-order.v1.json（desktop main/projectOrderStore.ts 的单 owner 版）；
//! - 设备 id：<dataDir>/device-id，登录时作为 auth-client 的 deviceId，刷新 token 与它绑定，不能随意重置。

use crate::layout_core::{coerce_layout, create_default_layout, host_local_project_keys_only, parse_order_mode};
use crate::maker_host::Emitter;
use crate::owner::Owner;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

pub const INVOKE_CHANNELS: &[&str] = &[
    "check-environment",
    "check-app-update",
    "analytics:settings-get",
    "legacy-migration:get-state",
    "local-db:maintenance:startup-progress",
    "worktree:list-all",
    "local-db:bots:list",
    "local-db:ensure-ready",
    "layout:set",
    "layout:reset",
    "sidebar-settings:get-project-order",
    "sidebar-settings:apply-project-order",
];
pub const SYNC_CHANNELS: &[&str] = &["accessibility-support:get-sync", "voice-input:get-microphone-permission-cached", "layout:get"];

fn fail<T>(code: &str, msg: impl AsRef<str>) -> Result<T, String> {
    Err(format!("[{code}] {}", msg.as_ref()))
}

/// 设备 id：文件里是合法的 36 位 uuid 就沿用，否则新建。
pub fn device_id(data_dir: &Path) -> String {
    let file = data_dir.join("device-id");
    if let Ok(text) = std::fs::read_to_string(&file) {
        let v = text.trim();
        // /^[0-9a-f-]{36}$/
        if v.len() == 36 && v.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b) || b == b'-') {
            return v.to_string();
        }
    }
    let id = uuid::Uuid::new_v4().to_string();
    let _ = std::fs::create_dir_all(data_dir);
    let _ = std::fs::write(&file, &id);
    id
}

pub struct BootStubs {
    layout_file: PathBuf,
    order_file: PathBuf,
    device_id: String,
    pi_binary: Option<PathBuf>,
    owner: Arc<Owner>,
    emitter: Arc<dyn Emitter>,
    /// 布局 / 排序的读改写串行化。
    write_lock: Mutex<()>,
}

fn write_json(file: &Path, v: &Value) -> Result<(), String> {
    if let Some(dir) = file.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    std::fs::write(file, v.to_string()).map_err(|e| e.to_string())
}

impl BootStubs {
    pub fn new(data_dir: &Path, pi_binary: Option<PathBuf>, owner: Arc<Owner>, emitter: Arc<dyn Emitter>) -> Self {
        Self {
            layout_file: data_dir.join("layout.v1.json"),
            order_file: data_dir.join("project-order.v1.json"),
            device_id: device_id(data_dir),
            pi_binary,
            owner,
            emitter,
            write_lock: Mutex::new(()),
        }
    }

    pub fn device_id(&self) -> &str {
        &self.device_id
    }

    pub fn owns(channel: &str) -> bool {
        INVOKE_CHANNELS.contains(&channel)
    }
    pub fn owns_sync(channel: &str) -> bool {
        SYNC_CHANNELS.contains(&channel)
    }

    fn read_layout(&self) -> Value {
        let raw = std::fs::read_to_string(&self.layout_file).ok().and_then(|t| serde_json::from_str::<Value>(&t).ok());
        coerce_layout(&raw.unwrap_or(Value::Null)).0
    }

    fn order_snapshot(&self) -> Value {
        let saved = std::fs::read_to_string(&self.order_file).ok().and_then(|t| serde_json::from_str::<Value>(&t).ok());
        let norm = normalize_order(saved.as_ref());
        json!({
            "authoritative": saved.is_some(), "available": true,
            "manualProjectOrder": norm["manualProjectOrder"], "projectOrder": norm["projectOrder"],
            "ownerStamp": self.owner.stamp(),
        })
    }

    fn check_environment(&self) -> Value {
        // 这一版只支持 Pi；Claude Code / Codex 标成 skipped。Pi 未下载时也放行（renderer 里 Pi 是可选 agent）。
        let pi = match &self.pi_binary {
            Some(p) if p.is_file() => json!({ "status": "passed", "path": p.to_string_lossy() }),
            _ => json!({ "status": "skipped" }),
        };
        json!({
            "claudeCode": { "status": "passed" }, "codex": { "status": "skipped" }, "pi": pi,
            "ripgrep": { "status": "skipped" }, "allPassed": true, "platform": node_platform(),
        })
    }

    pub fn invoke(&self, channel: &str, args: &[Value]) -> Option<Result<Value, String>> {
        Some(match channel {
            "check-environment" => Ok(self.check_environment()),
            "check-app-update" => Ok(json!({ "hasUpdate": false, "action": "none" })),
            "analytics:settings-get" => Ok(json!({ "privacyConsentAccepted": true, "analyticsEnabled": false, "analyticsEnabledCustomized": true, "allowed": false })),
            "legacy-migration:get-state" => Ok(json!({ "phase": null })),
            "local-db:maintenance:startup-progress" => Ok(Value::Null),
            "worktree:list-all" | "local-db:bots:list" => Ok(json!([])),
            // 真实 DB 在会话链路里接入；这里只放行 LocalDbGate。
            "local-db:ensure-ready" => Ok(json!({ "ready": true })),
            "layout:set" => {
                let _g = self.write_lock.lock().unwrap();
                let (layout, fallback, reason) = coerce_layout(args.first().unwrap_or(&Value::Null));
                if fallback {
                    return Some(fail("INVALID_PARAMS", format!("invalid layout: {}", reason.unwrap_or_default())));
                }
                write_json(&self.layout_file, &layout).map(|_| json!({ "layout": layout, "persisted": true }))
            }
            "layout:reset" => {
                let _g = self.write_lock.lock().unwrap();
                let layout = create_default_layout();
                write_json(&self.layout_file, &layout).map(|_| json!({ "layout": layout, "persisted": true }))
            }
            "sidebar-settings:get-project-order" => Ok(self.order_snapshot()),
            "sidebar-settings:apply-project-order" => {
                let _g = self.write_lock.lock().unwrap();
                let req = args.first().unwrap_or(&Value::Null);
                if !req.is_object() || !self.owner.is_current(req.get("dataOwnerId").unwrap_or(&Value::Null), req.get("ownerGeneration").unwrap_or(&Value::Null)) {
                    return Some(fail("PRECONDITION_FAILED", "active account changed during project order mutation"));
                }
                if let Err(e) = write_json(&self.order_file, &normalize_order(Some(req))) {
                    return Some(Err(e));
                }
                let snapshot = self.order_snapshot();
                // 与 desktop projectOrderStore 一致：第二个参数是 owner 戳。
                self.emitter.emit_args("sidebar-settings:project-order-changed", vec![snapshot.clone(), self.owner.stamp()]);
                Ok(snapshot)
            }
            _ => return None,
        })
    }

    pub fn sync(&self, channel: &str) -> Option<Value> {
        Some(match channel {
            "accessibility-support:get-sync" => json!(true),
            // 语音输入未迁移：报告麦克风不可用，renderer 会跳过预热。
            "voice-input:get-microphone-permission-cached" => {
                json!({ "ok": false, "status": "unknown", "error": "voice input is not available in the Tauri build yet" })
            }
            "layout:get" => json!({ "layout": self.read_layout() }),
            _ => return None,
        })
    }
}

fn normalize_order(raw: Option<&Value>) -> Value {
    let v = raw.filter(|v| v.is_object()).unwrap_or(&Value::Null);
    let manual: Vec<String> = host_local_project_keys_only(v.get("manualProjectOrder")).into_iter().filter(|k| k.encode_utf16().count() <= 4096).collect();
    json!({ "manualProjectOrder": manual, "projectOrder": parse_order_mode(v.get("projectOrder")) })
}

/// Node 的 process.platform 取值。
fn node_platform() -> &'static str {
    if cfg!(target_os = "macos") {
        "darwin"
    } else if cfg!(windows) {
        "win32"
    } else {
        "linux"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Rec(Mutex<Vec<(String, Vec<Value>)>>);
    impl Emitter for Rec {
        fn emit(&self, c: &str, a: Value) {
            self.0.lock().unwrap().push((c.into(), vec![a]));
        }
        fn emit_args(&self, c: &str, a: Vec<Value>) {
            self.0.lock().unwrap().push((c.into(), a));
        }
    }
    fn rig(n: &str) -> (BootStubs, Arc<Rec>, Arc<Owner>, PathBuf) {
        let d = std::env::temp_dir().join(format!("boot-stubs-{n}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        let owner = Arc::new(Owner::new(&d));
        let rec = Arc::new(Rec(Mutex::new(vec![])));
        (BootStubs::new(&d, None, owner.clone(), rec.clone()), rec, owner, d)
    }
    fn call(b: &BootStubs, ch: &str, a: Value) -> Result<Value, String> {
        b.invoke(ch, &[a]).unwrap()
    }

    #[test]
    fn layout_roundtrip_and_validation() {
        let (b, _r, _o, d) = rig("layout");
        assert_eq!(b.sync("layout:get").unwrap(), json!({ "layout": create_default_layout() }));
        let mut l = create_default_layout();
        l["content"]["children"][0]["fraction"] = json!(0.6);
        l["content"]["children"][1]["fraction"] = json!(0.4);
        let r = call(&b, "layout:set", l.clone()).unwrap();
        assert_eq!(r, json!({ "layout": l, "persisted": true }));
        assert_eq!(b.sync("layout:get").unwrap()["layout"], l);
        let e = call(&b, "layout:set", json!({ "schemaVersion": 2 })).unwrap_err();
        assert_eq!(e, "[INVALID_PARAMS] invalid layout: schemaVersion must be 1");
        assert_eq!(b.sync("layout:get").unwrap()["layout"], l, "非法写入不改存档");
        // 存档改坏 → 读出默认
        std::fs::write(d.join("layout.v1.json"), "{broken").unwrap();
        assert_eq!(b.sync("layout:get").unwrap()["layout"], create_default_layout());
        assert_eq!(call(&b, "layout:reset", json!(null)).unwrap()["persisted"], true);
        assert_eq!(b.sync("layout:get").unwrap()["layout"], create_default_layout());
    }

    #[test]
    fn project_order_owner_check_and_push() {
        let (b, rec, owner, d) = rig("order");
        let s = call(&b, "sidebar-settings:get-project-order", json!(null)).unwrap();
        assert_eq!(s["authoritative"], false);
        assert_eq!(s["projectOrder"], "activity");
        assert_eq!(s["ownerStamp"], owner.stamp());
        let stamp = owner.stamp();
        let req = json!({ "dataOwnerId": stamp["dataOwnerId"], "ownerGeneration": stamp["ownerGeneration"], "projectOrder": "custom", "manualProjectOrder": ["local:/a", "device:x:/b", "local:/a", " local:/c "] });
        let snap = call(&b, "sidebar-settings:apply-project-order", req.clone()).unwrap();
        assert_eq!((snap["authoritative"].clone(), snap["projectOrder"].clone(), snap["manualProjectOrder"].clone()), (json!(true), json!("custom"), json!(["local:/a", "local:/c"])));
        let pushed = rec.0.lock().unwrap().clone();
        assert_eq!(pushed.len(), 1);
        assert_eq!(pushed[0].0, "sidebar-settings:project-order-changed");
        assert_eq!(pushed[0].1, vec![snap.clone(), owner.stamp()], "第二个参数是 owner 戳");
        assert!(std::fs::read_to_string(d.join("project-order.v1.json")).unwrap().contains("custom"));
        // owner 换代后旧请求被拒；非对象请求也被拒
        owner.commit(crate::owner::Mode::SignedOut, None).unwrap();
        let e = call(&b, "sidebar-settings:apply-project-order", req).unwrap_err();
        assert_eq!(e, "[PRECONDITION_FAILED] active account changed during project order mutation");
        assert!(call(&b, "sidebar-settings:apply-project-order", json!(5)).unwrap_err().contains("PRECONDITION_FAILED"));
        assert_eq!(rec.0.lock().unwrap().len(), 1);
        // 超长 key 被丢弃
        owner.commit(crate::owner::Mode::Local, None).unwrap();
        let st = owner.stamp();
        let long = format!("local:/{}", "k".repeat(4100));
        let snap = call(&b, "sidebar-settings:apply-project-order", json!({ "dataOwnerId": st["dataOwnerId"], "ownerGeneration": st["ownerGeneration"], "manualProjectOrder": [long, "local:/ok"] })).unwrap();
        assert_eq!(snap["manualProjectOrder"], json!(["local:/ok"]));
    }

    #[test]
    fn placeholders_and_device_id() {
        let (b, _r, _o, d) = rig("misc");
        assert_eq!(call(&b, "check-app-update", json!(null)).unwrap(), json!({ "hasUpdate": false, "action": "none" }));
        assert_eq!(call(&b, "analytics:settings-get", json!(null)).unwrap()["allowed"], false);
        assert_eq!(call(&b, "legacy-migration:get-state", json!(null)).unwrap(), json!({ "phase": null }));
        assert_eq!(call(&b, "local-db:ensure-ready", json!(null)).unwrap(), json!({ "ready": true }));
        assert_eq!(call(&b, "worktree:list-all", json!(null)).unwrap(), json!([]));
        let env = call(&b, "check-environment", json!(null)).unwrap();
        assert_eq!((env["pi"].clone(), env["allPassed"].clone()), (json!({ "status": "skipped" }), json!(true)));
        assert_eq!(b.sync("accessibility-support:get-sync").unwrap(), json!(true));
        assert_eq!(b.sync("voice-input:get-microphone-permission-cached").unwrap()["ok"], false);
        assert!(b.invoke("nope", &[]).is_none() && b.sync("nope").is_none());
        // pi 在 → passed
        let bin = d.join("pi");
        std::fs::write(&bin, "x").unwrap();
        let b2 = BootStubs::new(&d, Some(bin.clone()), Arc::new(Owner::new(&d)), Arc::new(Rec(Mutex::new(vec![]))));
        assert_eq!(call(&b2, "check-environment", json!(null)).unwrap()["pi"], json!({ "status": "passed", "path": bin.to_string_lossy() }));
        // 设备 id：稳定、格式合法；文件被改坏则重建
        let id = b.device_id().to_string();
        assert_eq!(id.len(), 36);
        assert_eq!(device_id(&d), id);
        std::fs::write(d.join("device-id"), "garbage").unwrap();
        let fresh = device_id(&d);
        assert_ne!(fresh, id);
        assert_eq!(std::fs::read_to_string(d.join("device-id")).unwrap(), fresh);
    }
}
