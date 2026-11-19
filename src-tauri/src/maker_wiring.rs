//! Rust 会话宿主的生产接线：落库 / 启动参数回调、Tauri 事件推送、pi-rs 路径定位。
//!
//! 没有开关、没有回退：`MakerRegistry::owns` 为真的通道一律由 Rust 应答。
//! 找不到 pi-rs 时会话通道返回明确错误（见 `registry`）。

use crate::maker_host::{Emitter, HostBackend, HostFactory, HostSink};
use crate::maker_registry::MakerRegistry;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tauri::{AppHandle, Emitter as _, EventTarget};

/// 会话宿主的 `host.*` 回调：落库走本地库，模型 / 启动参数走模型宿主。会话宿主、goal 都只认 `HostBackend`。
pub struct DbBackend {
    pub db: Arc<crate::local_db::LocalDb>,
    pub models: Option<Arc<crate::model_config::host::ModelHost>>,
}
impl HostBackend for DbBackend {
    fn call(&self, method: &str, p: Value) -> Result<Value, String> {
        let sid = p["sessionId"].as_str().unwrap_or("");
        match method {
            "host.persist-event" => Ok(json!({ "persistId": self.db.persist_maker_event(sid, &p["event"]) })),
            "host.persist-user-message" => Ok(json!({ "clientId": self.db.persist_user_message(sid, p["clientId"].as_str().unwrap_or(""), &p["content"]) })),
            "host.persist-interaction-request" => Ok(json!({ "persistId": self.db.persist_interaction_request(sid, &p["request"]) })),
            "host.persist-interaction-resolved" => {
                self.db.persist_interaction_resolved(sid, p["requestId"].as_str().unwrap_or(""), &p["decision"]);
                Ok(Value::Null)
            }
            // 建会话 / 回合结束后的行更新：只写 patch 里有的字段（旧 sidecar 缓存整行写回会覆盖用户改过的值）。
            "host.upsert-session" => {
                let mut patch = p["patch"].as_object().cloned().unwrap_or_default();
                patch.insert("id".into(), json!(sid));
                self.db.upsert_session(&Value::Object(patch.clone()))?;
                // upsertSession 只管建行与基础字段；运行时选择要走 update 才落得进库。
                let extra: serde_json::Map<String, Value> =
                    ["effort", "fastMode"].iter().filter_map(|k| patch.get(*k).filter(|v| !v.is_null()).map(|v| (k.to_string(), v.clone()))).collect();
                if !extra.is_empty() && self.db.get_session(sid).is_some() {
                    self.db.update_session(sid, &Value::Object(extra))?;
                }
                Ok(Value::Null)
            }
            "host.session-resume-info" => {
                let s = self.db.get_session(sid);
                let f = |k: &str| s.as_ref().map(|s| s[k].clone()).unwrap_or(Value::Null);
                Ok(json!({ "workingDir": f("workingDir"), "sdkSessionId": f("sdkSessionId"), "providerId": f("providerId"), "model": f("model") }))
            }
            "host.sync-sdk-session-id" => {
                if let Some(id) = p["sdkSessionId"].as_str().filter(|s| !s.is_empty()) {
                    if let Some(row) = self.db.get_session(sid) {
                        if row["sdkSessionId"].as_str() != Some(id) {
                            self.db.update_session(sid, &json!({ "sdkSessionId": id }))?;
                        }
                    }
                }
                Ok(Value::Null)
            }
            "host.broadcast" => {
                crate::native::push_tap(p["channel"].as_str().unwrap_or(""), &p["payload"]);
                Ok(Value::Null)
            }
            "host.session-muted" => Ok(json!(self.db.session_muted(sid))),
            "host.local-db-file" => Ok(json!(self.db.file.to_string_lossy())),
            // 忙闲由注册表自己维护，不需要再同步给别处。
            "host.turn-state" => Ok(Value::Null),
            _ => match self.models.as_ref().and_then(|m| m.call(method, &p)) {
                Some(r) => r,
                None => Err(format!("[NOT_IMPLEMENTED] {method}")),
            },
        }
    }
}

/// 本地库的推送：推给 webview，同时转给设备互联旁路（手机端也要这份）。
pub struct AppPush(pub AppHandle);
impl crate::local_db::Push for AppPush {
    fn push(&self, channel: &str, payload: Value) {
        TauriEmitter(self.0.clone()).emit(channel, payload.clone());
        crate::native::push_tap(channel, &payload);
    }
}

/// 与 `IpcPush` 同口径：`cindy:ipc` 事件，payload 为 `{"v":[arg]}` 的 JSON 字符串。
pub struct TauriEmitter(pub AppHandle);
impl Emitter for TauriEmitter {
    fn emit(&self, channel: &str, arg: Value) {
        self.emit_args(channel, vec![arg]);
    }
    fn emit_args(&self, channel: &str, args: Vec<Value>) {
        let payload = serde_json::to_string(&json!({ "v": args })).unwrap_or_default();
        if let Err(e) = self.0.emit_to(EventTarget::webview("main"), "cindy:ipc", json!({ "channel": channel, "payload": payload })) {
            eprintln!("[maker-host] emit {channel} failed: {e}");
        }
    }
}

/// pi-rs 二进制：`CINDY_PI_RS` 覆盖 → 打包资源 `pi-rs/pi` → 开发目录 `resources/pi-rs/pi`。
pub fn locate_pi_rs(resource_dir: Option<&Path>) -> Option<PathBuf> {
    let file = if cfg!(windows) { "pi.exe" } else { "pi" };
    let mut cands: Vec<PathBuf> = Vec::new();
    if let Some(p) = std::env::var_os("CINDY_PI_RS") {
        cands.push(PathBuf::from(p));
    }
    if let Some(d) = resource_dir {
        cands.push(d.join("pi-rs").join(file));
    }
    cands.push(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../resources/pi-rs").join(file));
    cands.into_iter().find(|p| p.is_file())
}

/// goal 控制器的生产端口：库文件向本地库要一次，发送 / 中止 / 忙闲直接问注册表，推送走 sink（同时转给手机端）。
pub struct GoalWiring {
    pub backend: Arc<dyn HostBackend>,
    pub registry: std::sync::Weak<MakerRegistry>,
    pub sink: Arc<HostSink>,
}

impl crate::goal_host::GoalPorts for GoalWiring {
    fn db_path(&self) -> Result<PathBuf, String> {
        let v = self.backend.call("host.local-db-file", json!({}))?;
        v.as_str().map(PathBuf::from).ok_or_else(|| "[PRECONDITION_FAILED] local database is not ready".to_string())
    }
    fn in_turn(&self, session_id: &str) -> bool {
        self.registry.upgrade().and_then(|r| r.invoke("maker:session-in-turn", &[json!(session_id)]).ok()).and_then(|v| v.as_bool()).unwrap_or(false)
    }
    fn enqueue(&self, session_id: &str, item: Value) -> Result<(), String> {
        let reg = self.registry.upgrade().ok_or("registry gone")?;
        reg.invoke("maker:input:enqueue", &[json!(session_id), item]).map(|_| ())
    }
    fn abort(&self, session_id: &str) {
        if let Some(r) = self.registry.upgrade() {
            let _ = r.invoke("maker:abort-session", &[json!(session_id)]);
        }
    }
    fn emit(&self, channel: &str, payload: Value) {
        self.sink.push(channel, payload);
    }
}

/// 接上 /goal 控制器：事件从 sink 进，操作从注册表进。启动后 2 秒把还在跑的目标接着跑（等 IPC 与模型就绪）。
pub fn attach_goal(registry: &Arc<MakerRegistry>, backend: Arc<dyn HostBackend>, sink: Arc<HostSink>, resume_after: Option<std::time::Duration>) -> Arc<crate::goal_host::GoalController> {
    let goal = crate::goal_host::GoalController::new(Arc::new(GoalWiring { backend, registry: Arc::downgrade(registry), sink: sink.clone() }));
    sink.set_goal(goal.clone());
    registry.set_goal(goal.clone());
    if let Some(delay) = resume_after {
        let g = goal.clone();
        std::thread::spawn(move || {
            std::thread::sleep(delay);
            let n = g.resume_active_goals();
            if n > 0 {
                eprintln!("[goal] resumed {n} goal(s) after start");
            }
        });
    }
    goal
}

pub fn build(app: AppHandle, data_dir: PathBuf, pi_binary: PathBuf, db: Arc<crate::local_db::LocalDb>) -> Arc<MakerRegistry> {
    eprintln!("[maker-host] Rust 会话宿主已启用，pi-rs={}", pi_binary.display());
    let backend: Arc<dyn HostBackend> = Arc::new(DbBackend { db, models: crate::native::MODEL_HOST.get().cloned() });
    let sink = HostSink::new(backend.clone(), Arc::new(TauriEmitter(app)));
    let factory = Arc::new(HostFactory { backend: backend.clone(), sink: sink.clone(), data_dir, pi_binary });
    let registry = MakerRegistry::new(factory);
    attach_goal(&registry, backend, sink.clone(), Some(std::time::Duration::from_secs(2)));
    let weak = Arc::downgrade(&registry);
    // 进程自行退出（崩溃 / 被关）后从注册表摘掉，下次使用时重建。
    sink.set_on_exit(move |id| {
        if let Some(r) = weak.upgrade() {
            r.on_exited(id);
        }
    });
    registry
}

fn registry<R: tauri::Runtime>(app: &tauri::AppHandle<R>) -> Result<Arc<MakerRegistry>, String> {
    use tauri::Manager;
    app.try_state::<crate::MakerState>()
        .and_then(|m| m.0.clone())
        .ok_or_else(|| "[PRECONDITION_FAILED] pi-rs not found: session host unavailable".to_string())
}

/// 手机端 invoke 与网关 key 轮换用的会话宿主入口。与 Tauri 解耦，方便集成测试。
pub fn handle_native(reg: &MakerRegistry, method: &str, params: &Value) -> Option<Result<Value, String>> {
    match method {
        // 与 renderer 的 `maker:input:enqueue` 同路径。
        "maker.enqueue" => Some(reg.invoke("maker:input:enqueue", &[params["sessionId"].clone(), params["item"].clone()])),
        "maker.abort" => Some(reg.invoke("maker:abort-session", &[params["sessionId"].clone()])),
        // 网关 key 轮换：存活会话的 key 文件原地改写。
        "maker.gateway-key" => Some(Ok(json!({ "rotated": reg.rotate_gateway_key(params["key"].as_str().unwrap_or("")) }))),
        // 远程控制端（手机）的 invoke：整条转回注册表。
        "maker.invoke" => {
            let channel = params["channel"].as_str().unwrap_or("");
            if !MakerRegistry::owns(channel) {
                return Some(Err(format!("[NOT_IMPLEMENTED] {channel} is not owned by the Rust host")));
            }
            let args = params["args"].as_array().cloned().unwrap_or_default();
            Some(reg.invoke(channel, &args))
        }
        _ => None,
    }
}

pub fn native_enqueue<R: tauri::Runtime>(app: &tauri::AppHandle<R>, params: &Value) -> Result<Value, String> {
    let reg = registry(app)?;
    handle_native(&reg, "maker.enqueue", params).unwrap_or(Ok(Value::Null))
}

pub fn native_gateway_key<R: tauri::Runtime>(app: &tauri::AppHandle<R>, params: &Value) -> Result<Value, String> {
    let reg = registry(app)?;
    handle_native(&reg, "maker.gateway-key", params).unwrap_or(Ok(Value::Null))
}

pub fn native_invoke<R: tauri::Runtime>(app: &tauri::AppHandle<R>, params: &Value) -> Result<Value, String> {
    // 手机端调的本地库通道（会话列表等）归 Rust 本地库。
    if let (Some(ch), Some(db)) = (params["channel"].as_str(), crate::native::LOCAL_DB.get()) {
        if crate::local_db::LocalDb::owns(ch) {
            let args = params["args"].as_array().cloned().unwrap_or_default();
            return db.invoke(ch, &args);
        }
    }
    let reg = registry(app)?;
    handle_native(&reg, "maker.invoke", params).unwrap_or(Ok(Value::Null))
}

pub fn native_abort<R: tauri::Runtime>(app: &tauri::AppHandle<R>, params: &Value) -> Result<Value, String> {
    let reg = registry(app)?;
    handle_native(&reg, "maker.abort", params).unwrap_or(Ok(Value::Null))
}
