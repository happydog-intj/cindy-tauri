//! Rust 会话宿主的生产接线：`SessionFactory`（建会话）与 `SessionSink`（事件出口）。
//!
//! 依赖两样外部能力，都抽象成 trait，所以能用假实现单测：
//! - `HostBackend`：向 sidecar 要启动参数 / 落库 / goal 回调（生产实现是 `sidecar.call("host.*")`）；
//! - `Emitter`：向 renderer 推 `maker:*` 事件（生产实现是 Tauri 事件）。
//!
//! 落库顺序：reader 线程逐帧回调，sink 同步调用 `host.persist-event` 后再推给 renderer，
//! 所以同一会话里落库与推送顺序天然与事件顺序一致（对应 maker-ipc 的 eventChain 串行）。

#![allow(dead_code)]

use crate::maker_registry::{fail, SessionFactory};
use crate::pi_launch::{self, LaunchPlan};
use crate::pi_proc::{PiSession, SessionSink};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// sidecar 里 `host.*` 方法的调用面。
pub trait HostBackend: Send + Sync + 'static {
    fn call(&self, method: &str, params: Value) -> Result<Value, String>;
}

/// 向 renderer 推事件。channel 形如 `maker:event`，arg 是单个对象。
pub trait Emitter: Send + Sync + 'static {
    fn emit(&self, channel: &str, arg: Value);
    /// 多参数推送（desktop 的 `webContents.send(channel, a, b)`）。默认只推第一个参数；需要多参数的通道由 TauriEmitter 实现。
    fn emit_args(&self, channel: &str, args: Vec<Value>) {
        self.emit(channel, args.into_iter().next().unwrap_or(Value::Null));
    }
}

pub struct HostSink {
    backend: Arc<dyn HostBackend>,
    emitter: Arc<dyn Emitter>,
    /// 退出回调：让注册表摘掉会话（下次懒创建）。
    on_exit: Mutex<Option<Box<dyn Fn(&str) + Send + Sync>>>,
    /// 会话运行目录：退出时清理（key 文件在里面）。
    runtime_dirs: Mutex<HashMap<String, PathBuf>>,
    /// 用网关 key 的会话（key 文件在运行目录里）：网关 key 轮换时要改写，401 时要上报。
    gateway_sessions: Mutex<std::collections::HashSet<String>>,
    /// 会话 id → pi-rs 的 --session-dir（历史 jsonl 所在）。
    history_dirs: Mutex<HashMap<String, PathBuf>>,
    /// /goal 控制器：每个 maker 事件和回合收尾都要告诉它。
    goal: std::sync::OnceLock<Arc<crate::goal_host::GoalController>>,
}

impl HostSink {
    pub fn new(backend: Arc<dyn HostBackend>, emitter: Arc<dyn Emitter>) -> Arc<Self> {
        Arc::new(Self { backend, emitter, on_exit: Mutex::new(None), runtime_dirs: Mutex::new(HashMap::new()), gateway_sessions: Mutex::new(Default::default()), history_dirs: Mutex::new(HashMap::new()), goal: std::sync::OnceLock::new() })
    }

    pub fn set_goal(&self, g: Arc<crate::goal_host::GoalController>) {
        let _ = self.goal.set(g);
    }

    pub fn set_on_exit(&self, f: impl Fn(&str) + Send + Sync + 'static) {
        *self.on_exit.lock().unwrap() = Some(Box::new(f));
    }

    fn remember_gateway(&self, session_id: &str) {
        self.gateway_sessions.lock().unwrap().insert(session_id.to_string());
    }

    /// 网关 key 变了：把新 key 写进所有存活网关会话的 key 文件（原地改写，pi-rs 每个请求现读）。返回改写的会话数。
    pub fn rotate_gateway_key(&self, key: &str) -> usize {
        let ids: Vec<String> = self.gateway_sessions.lock().unwrap().iter().cloned().collect();
        let dirs = self.runtime_dirs.lock().unwrap();
        let mut n = 0;
        for id in ids {
            if let Some(dir) = dirs.get(&id) {
                match pi_launch::rotate_key(&dir.join("provider.key"), key) {
                    Ok(()) => n += 1,
                    Err(e) => eprintln!("[maker-host] rotate gateway key for {id} failed: {e}"),
                }
            }
        }
        n
    }

    fn remember_runtime(&self, session_id: &str, dir: PathBuf) {
        self.runtime_dirs.lock().unwrap().insert(session_id.to_string(), dir);
    }

    /// 落库失败不影响事件流：记日志后继续推（对应 maker-ipc 里 persistMakerEvent 的 try/catch）。
    /// 推给 webview，并让 sidecar 的设备互联旁路也收到一份（手机端订阅的会话事件、审批请求都靠它）。
    pub fn push(&self, channel: &str, arg: Value) {
        self.emitter.emit(channel, arg.clone());
        self.persist("host.broadcast", json!({ "channel": channel, "payload": arg }));
    }

    fn persist(&self, method: &str, params: Value) -> Option<Value> {
        match self.backend.call(method, params) {
            Ok(v) => Some(v),
            Err(e) => {
                eprintln!("[maker-host] {method} failed: {e}");
                None
            }
        }
    }
}

impl SessionSink for HostSink {
    fn maker_event(&self, session_id: &str, event: Value) {
        let persist_id = self
            .persist("host.persist-event", json!({ "sessionId": session_id, "event": event }))
            .and_then(|v| v["persistId"].as_str().map(String::from));
        // 网关回 401：告诉 sidecar 静默重拉凭据（它自己限频）。原来由本地转发器在转发时发现。
        if event["type"] == "error" && event["data"]["errorStatus"] == 401 && self.gateway_sessions.lock().unwrap().contains(session_id) {
            self.persist("host.gateway-rejected", json!({}));
        }
        // goal 控制器靠事件判断回合收尾 / 工具使用（落库之后，与原 sidecar 的监听顺序一致）。
        if let Some(g) = self.goal.get() {
            g.on_event(session_id, &event);
        }
        // 独立文本（扩展通知 / `!cmd` 输出）只经落库行通道到达渲染层；再发 maker:event 会被当成活跃助手流，
        // 盖掉或错放下一条模型回复。与上游 sessionEventPreparation 的 standaloneText 分支一致。
        if event["standaloneText"] == true {
            return;
        }
        let mut arg = json!({ "sessionId": session_id, "event": event });
        if let Some(pid) = persist_id {
            arg["persistId"] = json!(pid);
        }
        self.push("maker:event", arg);
    }

    fn interaction_request(&self, payload: Value) {
        let sid = payload["sessionId"].as_str().unwrap_or("").to_string();
        let mut arg = payload.clone();
        if let Some(v) = self.persist("host.persist-interaction-request", json!({ "sessionId": sid, "request": payload["request"] })) {
            if let Some(pid) = v["persistId"].as_str() {
                arg["persistId"] = json!(pid);
            }
        }
        self.push("maker:interaction-request", arg);
    }

    fn interaction_dismissed(&self, payload: Value) {
        // 提问 / 计划的最终状态回写（permission 不建行，sidecar 侧返回 null）。
        let decision = payload.get("decision").cloned().unwrap_or_else(|| json!({ "dismissed": payload["resolvedAs"] == "deny" }));
        self.persist(
            "host.persist-interaction-resolved",
            json!({ "sessionId": payload["sessionId"], "requestId": payload["requestId"], "decision": decision }),
        );
        self.push("maker:interaction-dismissed", payload);
    }

    fn projection(&self, payload: Value) {
        // 回合收尾（turnInProgress=false）→ 通知 goal 控制器并更新行的 updatedAt。
        // 忙闲同步给 sidecar（自动更新的忙闲判断、goal 的 isSessionInTurn 都看它）。
        if let Some(in_turn) = payload["turnInProgress"].as_bool() {
            self.persist("host.turn-state", json!({ "sessionId": payload["sessionId"], "inTurn": in_turn }));
        }
        if payload["turnInProgress"] == json!(false) {
            let sid = payload["sessionId"].clone();
            // Pi 会话文件路径落库（重启续接用）：每个会话独占一个目录，取最新的 jsonl。
            let dir = sid.as_str().and_then(|id| self.history_dirs.lock().unwrap().get(id).cloned());
            if let Some(file) = dir.as_deref().and_then(latest_session_file) {
                self.persist("host.sync-sdk-session-id", json!({ "sessionId": sid, "sdkSessionId": file.to_string_lossy() }));
            }
            if let (Some(g), Some(id)) = (self.goal.get(), sid.as_str()) {
                g.note_session_idle(id);
            }
            self.persist("host.upsert-session", json!({ "sessionId": sid, "patch": {} }));
        }
        self.push("maker:input:projection", payload);
    }

    fn user_message(&self, session_id: &str, client_id: &str, content: &str) {
        self.persist("host.persist-user-message", json!({ "sessionId": session_id, "clientId": client_id, "content": content }));
    }

    fn exited(&self, session_id: &str, _code: Option<i32>) {
        self.history_dirs.lock().unwrap().remove(session_id);
        self.gateway_sessions.lock().unwrap().remove(session_id);
        self.push("maker:status-changed", json!({ "sessionId": session_id, "status": "closed" }));
        if let Some(dir) = self.runtime_dirs.lock().unwrap().remove(session_id) {
            pi_launch::cleanup(&dir);
        }
        if let Some(f) = self.on_exit.lock().unwrap().as_ref() {
            f(session_id);
        }
    }
}

/// 目录里修改时间最新的 `.jsonl`。
pub fn latest_session_file(dir: &Path) -> Option<PathBuf> {
    std::fs::read_dir(dir)
        .ok()?
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().is_some_and(|x| x == "jsonl"))
        .filter_map(|e| Some((e.metadata().ok()?.modified().ok()?, e.path())))
        .max_by_key(|(t, _)| *t)
        .map(|(_, p)| p)
}

pub struct HostFactory {
    pub backend: Arc<dyn HostBackend>,
    pub sink: Arc<HostSink>,
    pub data_dir: PathBuf,
    pub pi_binary: PathBuf,
}

fn str_opt<'a>(v: &'a Value, k: &str) -> Option<&'a str> {
    v[k].as_str().filter(|s| !s.is_empty())
}

/// 与 maker-ipc 的 dialogueDir 同规则：没有工作目录的会话落到 dialogues/<日期>/<id>。
fn dir_list(v: &Value) -> Vec<String> {
    v.as_array().map(|a| a.iter().filter_map(|d| d.as_str().map(String::from)).collect()).unwrap_or_default()
}

fn dialogue_dir(data_dir: &Path, session_id: &str, today: &str) -> PathBuf {
    data_dir.join("dialogues").join(today).join(session_id)
}

fn today_local() -> String {
    // 只需要一个稳定的目录名；用 UTC 日期避免引入时区依赖。
    let secs = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0) as i64;
    date_from_unix(secs)
}

fn date_from_unix(secs: i64) -> String {
    let days = secs.div_euclid(86_400);
    // 民用历法换算（Howard Hinnant）。
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    format!("{:04}-{:02}-{:02}", if m <= 2 { y + 1 } else { y }, m, d)
}

impl SessionFactory for HostFactory {
    fn create(&self, session_id: &str, opts: &Value) -> Result<Arc<PiSession>, String> {
        str_opt(opts, "model").ok_or_else(|| fail("INVALID_PARAMS", "model required"))?;

        // 续接：库里记的工作目录、Pi 会话文件与模型选择优先于 renderer 传来的旧快照（重启后 createOpts 常常过期）。
        let resume = self.backend.call("host.session-resume-info", json!({ "sessionId": session_id })).unwrap_or(Value::Null);
        // 模型以库里的为准：set-model 已把选择写进会话行，入队项里的 createOpts 是建会话那刻的快照。
        let (model, provider) = match str_opt(&resume, "model") {
            Some(saved) => (saved, resume["providerId"].as_str().filter(|s| !s.is_empty())),
            None => (str_opt(opts, "model").unwrap_or_default(), opts["providerId"].as_str().filter(|s| !s.is_empty())),
        };
        let working_dir = str_opt(&resume, "workingDir")
            .or_else(|| str_opt(opts, "workingDir"))
            .map(PathBuf::from)
            .unwrap_or_else(|| dialogue_dir(&self.data_dir, session_id, &today_local()));
        let history_dir = self.data_dir.join("pi-sessions").join(session_id);
        // 库里没有记录时，用会话目录里最新的历史文件兜底。
        let resume_file = str_opt(opts, "resumeSessionId")
            .or_else(|| str_opt(&resume, "sdkSessionId"))
            .map(String::from)
            .or_else(|| latest_session_file(&history_dir).map(|p| p.to_string_lossy().into_owned()));

        // 启动参数与 key 由 sidecar 解析（model-access / secret store 留在那边）。失败直接带错误码返回。
        let launch = self
            .backend
            .call("host.resolve-launch", json!({ "providerId": provider, "model": model }))
            .map_err(|e| if e.starts_with('[') { e } else { fail("INTERNAL", e) })?;

        let mode = str_opt(opts, "permissionMode").unwrap_or("ask");
        let plan: LaunchPlan = pi_launch::prepare(&self.data_dir, session_id, &working_dir, &launch, mode)?;
        self.sink.remember_runtime(session_id, plan.runtime_dir.clone());
        if launch["keyKind"] == "gateway" {
            self.sink.remember_gateway(session_id);
        }
        self.sink.history_dirs.lock().unwrap().insert(session_id.to_string(), history_dir);

        let session = PiSession::spawn(session_id, &self.pi_binary, &plan.args, &plan.env, &plan.cwd, self.sink.clone() as Arc<dyn SessionSink>)
            .map_err(|e| {
                pi_launch::cleanup(&plan.runtime_dir);
                fail("INTERNAL", format!("spawn pi-rs failed ({}): {e}", self.pi_binary.display()))
            })?;

        session.set_runtime_dir(plan.runtime_dir.clone());
        // 附加目录：库里记着的授权要在新进程里重新生效（重启、换模型、切工作目录后重建都走这里）。
        let (extra, writable) = (dir_list(&resume["extraDirs"]), dir_list(&resume["writableDirs"]));
        if !extra.is_empty() || !writable.is_empty() {
            if let Err(e) = session.set_dirs(&extra, &writable) {
                eprintln!("[maker-host] apply extra dirs failed for {session_id}: {e}");
            }
        }
        // 续接历史：进程起来后发 switch_session（pi-rs 已有此命令；命令行没有 --session）。
        // 文件不存在 / 读不了时 pi-rs 会回失败应答，会话仍以空历史可用，不让续接失败拖垮建会话。
        if let Some(file) = resume_file.filter(|f| f != "<pending>") {
            let _ = session.send(&json!({ "id": "resume", "type": "switch_session", "sessionPath": file }));
        }
        // 行记录：与 maker-ipc createSession 末尾的 upsertRow 同字段。
        let _ = self.backend.call(
            "host.upsert-session",
            json!({ "sessionId": session_id, "patch": {
                "agentKind": "pi",
                "title": opts["title"].as_str().unwrap_or(""),
                "workDir": plan.cwd.to_string_lossy(),
                "providerId": launch["providerId"],
                "model": launch["modelId"],
                "permissionMode": mode,
                "status": "active",
            }}),
        );
        Ok(session)
    }

    /// 换模型的校验与规范化：复用 resolve-launch（同一套 provider / 模型 / 网关规则，未配置会直接带错误码失败）。
    fn resolve_selection(&self, model: &str, provider: Option<&str>) -> Result<(String, String), String> {
        let launch = self
            .backend
            .call("host.resolve-launch", json!({ "providerId": provider, "model": model }))
            .map_err(|e| if e.starts_with('[') { e } else { fail("INTERNAL", e) })?;
        let p = launch["providerId"].as_str().unwrap_or_default().to_string();
        let m = launch["modelId"].as_str().unwrap_or(model).to_string();
        Ok((p, m))
    }

    fn rotate_gateway_key(&self, key: &str) -> usize {
        self.sink.rotate_gateway_key(key)
    }

    fn current_provider(&self, session_id: &str) -> Option<String> {
        let info = self.backend.call("host.session-resume-info", json!({ "sessionId": session_id })).ok()?;
        info["providerId"].as_str().filter(|s| !s.is_empty()).map(String::from)
    }

    fn session_dirs(&self, session_id: &str) -> Value {
        let info = self.backend.call("host.session-resume-info", json!({ "sessionId": session_id })).unwrap_or(Value::Null);
        json!({ "extraDirs": info["extraDirs"], "writableDirs": info["writableDirs"], "workingDir": info["workingDir"] })
    }

    fn save_dirs(&self, session_id: &str, patch: Value) -> Result<(), String> {
        self.backend.call("host.update-session", json!({ "sessionId": session_id, "patch": patch })).map(|_| ())
    }

    fn persist_patch(&self, session_id: &str, patch: Value) {
        if let Err(e) = self.backend.call("host.upsert-session", json!({ "sessionId": session_id, "patch": patch })) {
            eprintln!("[maker-host] persist patch failed for {session_id}: {e}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct FakeBackend {
        calls: Mutex<Vec<(String, Value)>>,
        launch: Mutex<Option<Value>>,
        fail_persist_event: Mutex<bool>,
    }
    impl HostBackend for FakeBackend {
        fn call(&self, method: &str, params: Value) -> Result<Value, String> {
            self.calls.lock().unwrap().push((method.to_string(), params.clone()));
            match method {
                "host.resolve-launch" => self.launch.lock().unwrap().clone().ok_or_else(|| "[AUTH_INVALID] Cindy AI is not signed in".to_string()),
                "host.persist-event" if *self.fail_persist_event.lock().unwrap() => Err("db locked".into()),
                "host.persist-event" => Ok(json!({ "persistId": "pid-1" })),
                "host.persist-interaction-request" => Ok(json!({ "persistId": "pi-9" })),
                "host.session-resume-info" => Ok(json!({ "workingDir": null, "sdkSessionId": null })),
                _ => Ok(Value::Null),
            }
        }
    }
    impl FakeBackend {
        fn methods(&self) -> Vec<String> {
            self.calls.lock().unwrap().iter().map(|(m, _)| m.clone()).collect()
        }
    }

    #[derive(Default)]
    struct FakeEmitter(Mutex<Vec<(String, Value)>>);
    impl Emitter for FakeEmitter {
        fn emit(&self, channel: &str, arg: Value) {
            self.0.lock().unwrap().push((channel.to_string(), arg));
        }
    }

    fn rig() -> (Arc<HostSink>, Arc<FakeBackend>, Arc<FakeEmitter>) {
        let b = Arc::new(FakeBackend::default());
        let e = Arc::new(FakeEmitter::default());
        (HostSink::new(b.clone(), e.clone()), b, e)
    }

    #[test]
    fn events_are_persisted_before_they_are_pushed_and_carry_persist_id() {
        let (sink, b, e) = rig();
        sink.maker_event("s1", json!({ "type": "text", "data": { "text": "hi" } }));
        assert_eq!(b.methods(), vec!["host.persist-event", "host.broadcast"], "先落库，再推送并转给设备互联");
        let calls = b.calls.lock().unwrap();
        assert_eq!(calls[1].1["channel"], "maker:event");
        assert_eq!(calls[1].1["payload"]["persistId"], "pid-1", "手机端收到的事件同样带 persistId");
        let out = e.0.lock().unwrap();
        assert_eq!(out[0].0, "maker:event");
        assert_eq!(out[0].1["persistId"], "pid-1");
        assert_eq!(out[0].1["event"]["type"], "text");
    }

    #[test]
    fn persistence_failure_does_not_drop_the_event() {
        let (sink, b, e) = rig();
        *b.fail_persist_event.lock().unwrap() = true;
        sink.maker_event("s1", json!({ "type": "done" }));
        let out = e.0.lock().unwrap();
        assert_eq!(out.len(), 1, "落库失败也要推给 renderer");
        assert!(out[0].1.get("persistId").is_none());
    }

    #[test]
    fn interaction_request_gets_a_persist_id_and_resolution_is_written_back() {
        let (sink, b, e) = rig();
        sink.interaction_request(json!({ "sessionId": "s1", "request": { "requestId": "r1", "kind": "ask_user_question" } }));
        sink.interaction_dismissed(json!({ "sessionId": "s1", "requestId": "r1", "reason": "resolved", "resolvedAs": "allow", "decision": { "answers": { "a": "b" } } }));
        let out = e.0.lock().unwrap();
        assert_eq!(out[0].1["persistId"], "pi-9");
        assert_eq!(out[1].0, "maker:interaction-dismissed");
        let calls = b.calls.lock().unwrap();
        let resolved = calls.iter().find(|(m, _)| m == "host.persist-interaction-resolved").unwrap();
        assert_eq!(resolved.1["decision"]["answers"]["a"], "b");
        assert_eq!(resolved.1["requestId"], "r1");
    }

    #[test]
    fn settled_turn_syncs_the_newest_history_file_for_restart_resume() {
        let (sink, b, _e) = rig();
        let dir = std::env::temp_dir().join(format!("cindy-hist-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("old.jsonl"), "x").unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(dir.join("new.jsonl"), "y").unwrap();
        std::fs::write(dir.join("note.txt"), "z").unwrap();
        sink.history_dirs.lock().unwrap().insert("s1".into(), dir.clone());
        sink.projection(json!({ "sessionId": "s1", "turnInProgress": false }));
        let calls = b.calls.lock().unwrap();
        let sync = calls.iter().find(|(m, _)| m == "host.sync-sdk-session-id").expect("应同步历史文件路径");
        assert!(sync.1["sdkSessionId"].as_str().unwrap().ends_with("new.jsonl"));
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(latest_session_file(&dir), None);
    }

    #[test]
    fn settled_projection_notifies_goal_and_touches_the_row_but_running_one_does_not() {
        let (sink, b, e) = rig();
        sink.projection(json!({ "sessionId": "s1", "turnInProgress": true }));
        assert_eq!(b.methods(), vec!["host.turn-state", "host.broadcast"], "进行中只同步忙闲，不通知 goal、不动行");
        assert_eq!(b.calls.lock().unwrap()[0].1["inTurn"], true);
        b.calls.lock().unwrap().clear();
        sink.projection(json!({ "sessionId": "s1", "turnInProgress": false }));
        assert_eq!(b.methods(), vec!["host.turn-state", "host.upsert-session", "host.broadcast"]);
        assert_eq!(e.0.lock().unwrap().len(), 2, "两条投影都推给 renderer");
    }

    #[test]
    fn exit_pushes_closed_cleans_runtime_dir_and_notifies_registry() {
        let (sink, _, e) = rig();
        let dir = std::env::temp_dir().join(format!("host-exit-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("provider.key"), "secret").unwrap();
        sink.remember_runtime("s1", dir.clone());
        let seen = Arc::new(Mutex::new(Vec::<String>::new()));
        let s2 = seen.clone();
        sink.set_on_exit(move |id| s2.lock().unwrap().push(id.to_string()));
        sink.exited("s1", Some(0));
        assert!(!dir.exists(), "退出后 key 文件随运行目录删除");
        assert_eq!(*seen.lock().unwrap(), vec!["s1".to_string()]);
        assert_eq!(e.0.lock().unwrap()[0].1["status"], "closed");
    }

    #[test]
    fn gateway_key_rotation_rewrites_only_live_gateway_sessions_and_a_401_asks_for_a_resync() {
        let (sink, b, _) = rig();
        let base = std::env::temp_dir().join(format!("host-rotate-{}", std::process::id()));
        let (gw, other) = (base.join("gw"), base.join("other"));
        for d in [&gw, &other] {
            std::fs::create_dir_all(d).unwrap();
            std::fs::write(d.join("provider.key"), "old").unwrap();
        }
        sink.remember_runtime("gw", gw.clone());
        sink.remember_gateway("gw");
        sink.remember_runtime("other", other.clone()); // 自定义 provider：不是网关会话，key 不能被网关 key 覆盖
        assert_eq!(sink.rotate_gateway_key("new-key"), 1);
        assert_eq!(std::fs::read_to_string(gw.join("provider.key")).unwrap(), "new-key");
        assert_eq!(std::fs::read_to_string(other.join("provider.key")).unwrap(), "old");

        // 网关会话里的 401 → 上报；非网关会话、非 401 → 不上报。
        let err = |status: i64| json!({ "type": "error", "data": { "message": "x", "errorStatus": status } });
        sink.maker_event("other", err(401));
        sink.maker_event("gw", err(500));
        assert!(!b.methods().contains(&"host.gateway-rejected".to_string()));
        sink.maker_event("gw", err(401));
        assert!(b.methods().contains(&"host.gateway-rejected".to_string()), "网关会话的 401 触发重新同步");

        // 退出后不再被改写。
        sink.exited("gw", Some(0));
        assert_eq!(sink.rotate_gateway_key("newer"), 0);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn factory_fails_closed_with_a_coded_error_and_creates_nothing() {
        let (sink, b, _) = rig();
        let data = std::env::temp_dir().join(format!("host-factory-{}", std::process::id()));
        let f = HostFactory { backend: b.clone(), sink, data_dir: data.clone(), pi_binary: PathBuf::from("/nonexistent/pi") };
        let err = f.create("s1", &json!({ "model": "m", "providerId": "xd" })).err().unwrap();
        assert!(err.starts_with("[AUTH_INVALID]"), "{err}");
        assert!(!data.join("pi-runtime").exists(), "解析失败不应创建运行目录");
        assert!(f.create("s1", &json!({})).err().unwrap().starts_with("[INVALID_PARAMS]"));
    }

    #[test]
    fn model_switch_validates_through_resolve_launch_and_persists_the_row_patch() {
        let (sink, b, _) = rig();
        let f = HostFactory { backend: b.clone(), sink, data_dir: std::env::temp_dir(), pi_binary: PathBuf::from("/nonexistent/pi") };
        // 未登录 / 未配置：带错误码失败，不落库。
        let err = f.resolve_selection("m2", Some("xd")).unwrap_err();
        assert!(err.starts_with("[AUTH_INVALID]"), "{err}");
        assert!(!b.methods().contains(&"host.upsert-session".to_string()));
        // 解析成功：返回规范化后的 (provider, model)。
        *b.launch.lock().unwrap() = Some(json!({ "providerId": "pp", "modelId": "mm", "provider": {} }));
        assert_eq!(f.resolve_selection("m2", None).unwrap(), ("pp".to_string(), "mm".to_string()));
        f.persist_patch("s1", json!({ "model": "mm", "providerId": "pp" }));
        let calls = b.calls.lock().unwrap();
        let (_, params) = calls.iter().find(|(m, _)| m == "host.upsert-session").unwrap();
        assert_eq!(params["patch"]["model"], "mm");
    }

    #[test]
    fn factory_cleans_up_when_spawn_fails() {
        let (sink, b, _) = rig();
        *b.launch.lock().unwrap() = Some(json!({
            "providerId": "p", "modelId": "m", "keyKind": "provider", "key": "sk-1",
            "provider": { "baseUrl": "http://127.0.0.1:1", "api": "openai-completions", "models": [{ "id": "m" }] }
        }));
        let data = std::env::temp_dir().join(format!("host-spawnfail-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&data);
        let f = HostFactory { backend: b, sink, data_dir: data.clone(), pi_binary: PathBuf::from("/nonexistent/pi") };
        let err = f.create("s1", &json!({ "model": "m", "workingDir": data.join("w").to_string_lossy() })).err().unwrap();
        assert!(err.starts_with("[INTERNAL]"), "{err}");
        assert!(!data.join("pi-runtime/s1").exists(), "启动失败后 key 文件不残留");
        let _ = std::fs::remove_dir_all(&data);
    }

    #[test]
    fn dialogue_dir_and_date_helpers() {
        assert_eq!(dialogue_dir(Path::new("/d"), "s", "2026-11-17"), PathBuf::from("/d/dialogues/2026-11-17/s"));
        assert_eq!(date_from_unix(0), "1970-01-01");
        assert_eq!(date_from_unix(951_782_400), "2000-02-29", "闰日");
        assert_eq!(date_from_unix(1_700_000_000), "2023-11-14");
        assert_eq!(date_from_unix(1_798_761_599), "2026-12-31");
        assert_eq!(date_from_unix(1_798_761_600), "2027-01-01");
        let t = today_local();
        assert_eq!(t.len(), 10);
        assert!(t.starts_with("20"));
    }
}
