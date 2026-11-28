//! Maker 注册表：多会话 + `maker:*` 通道分发（Rust 接管的那一部分）。
//!
//! 与 terminal.rs 同模式：main.rs 在 `ipc_invoke` 里截走通道，调用 `MakerRegistry::invoke`，
//! 事件经 `SessionSink` 推到 renderer。会话怎么创建（写 models.json、拉起 pi-rs）交给 `SessionFactory`，
//! 注册表本身不关心，因此可以用假工厂单测。
//!
//! 本阶段接管的通道（其余仍由 sidecar 应答）：
//!   maker:input:get-projection / enqueue / stop / resume / clear-error / remove
//!   maker:get-pending-interactions / maker:resolve-interaction
//!   maker:abort-session / maker:close-session / maker:list-active / maker:session-in-turn / maker:any-session-in-turn
//! 会话由 `maker:create-session` 或 enqueue 时带的 createOpts 懒创建。
//! 同一通道只有一个所有者：`owns(channel)` 为真的通道 sidecar 不再应答（不双写）。

#![allow(dead_code)]

use crate::pi_proc::PiSession;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

pub fn fail(code: &str, msg: impl Into<String>) -> String {
    format!("[{code}] {}", msg.into())
}

/// 与 sidecar maker-ipc 的 SAFE_ID 同规则。
pub fn valid_session_id(v: &str) -> bool {
    !v.is_empty() && v.len() <= 128 && v != "." && v != ".." && v.chars().all(|c| c.is_ascii_alphanumeric() || "_.:-".contains(c))
}

/// 创建 / 恢复一个会话（拉起 pi-rs）。`opts` 是 renderer 的 createOpts，含 id / model / workingDir 等。
pub trait SessionFactory: Send + Sync + 'static {
    fn create(&self, session_id: &str, opts: &Value) -> Result<Arc<PiSession>, String>;
    /// 校验并规范化模型选择，返回 `(providerId, modelId)`。失败带错误码（如 MODEL_NOT_AVAILABLE）。
    fn resolve_selection(&self, _model: &str, provider: Option<&str>) -> Result<(String, String), String> {
        Ok((provider.unwrap_or("").to_string(), _model.to_string()))
    }
    /// 网关 key 轮换：改写存活网关会话的 key 文件，返回改写的会话数。
    fn rotate_gateway_key(&self, _key: &str) -> usize {
        0
    }
    /// 会话行里当前的来源（set-model 没带 providerId 时沿用它，与 sidecar 同口径）。
    fn current_provider(&self, _session_id: &str) -> Option<String> {
        None
    }
    /// 把选择写进会话行（下次建会话按库里的模型走）。
    fn persist_patch(&self, _session_id: &str, _patch: Value) {}
}

const PERMISSION_MODES: [&str; 6] = ["ask", "default", "acceptEdits", "plan", "auto", "bypassPermissions"];

const OWNED: [&str; 20] = [
    "maker:set-fast-mode",
    "maker:set-thinking-enabled",
    "maker:set-model",
    "maker:set-effort",
    "maker:create-session",
    "maker:send",
    "maker:set-permission-mode",
    "maker:input:get-projection",
    "maker:input:enqueue",
    "maker:input:stop",
    "maker:input:resume",
    "maker:input:clear-error",
    "maker:input:remove",
    "maker:input:set-edit-lock",
    "maker:input:update-content",
    "maker:get-pending-interactions",
    "maker:resolve-interaction",
    "maker:abort-session",
    "maker:close-session",
    "maker:session-in-turn",
];

pub struct MakerRegistry {
    sessions: Mutex<HashMap<String, Arc<PiSession>>>,
    /// 回合中收到的换模型请求：回合结束后的下一次取会话时重建（pi-rs 的模型表在启动时加载，不能原地换 provider）。
    stale: Mutex<std::collections::HashSet<String>>,
    factory: Arc<dyn SessionFactory>,
    me: std::sync::Weak<MakerRegistry>,
    /// /goal 控制器（接线时设置；未设置时 goal 通道返回明确错误）。
    goal: std::sync::OnceLock<Arc<crate::goal_host::GoalController>>,
}

fn session_id_arg(args: &[Value]) -> Result<String, String> {
    match args.first().and_then(Value::as_str) {
        Some(s) if valid_session_id(s) => Ok(s.to_string()),
        _ => Err(fail("INVALID_PARAMS", "sessionId required")),
    }
}

impl MakerRegistry {
    pub fn new(factory: Arc<dyn SessionFactory>) -> Arc<Self> {
        Arc::new_cyclic(|me| Self { sessions: Mutex::new(HashMap::new()), stale: Mutex::new(Default::default()), factory, me: me.clone(), goal: std::sync::OnceLock::new() })
    }

    /// 回合收尾后应用延期的换模型：关掉旧进程，按库里的新选择重建，并把队列里待派发的消息迁过去。
    /// 与 sidecar 同口径：先应用选择，再派发下一条，所以排队的消息用的是新模型。
    fn rebuild_after_turn(&self, id: &str) {
        let Some(old) = self.get(id) else { return };
        if !self.stale.lock().unwrap().contains(id) || old.turn_in_progress() {
            return;
        }
        // 暂停中的队列、或没有待派发项：交给下一次取会话时的懒重建。
        if old.projection()["queuePaused"] == json!(true) {
            return;
        }
        let pending = old.take_pending();
        if pending.is_empty() {
            return;
        }
        let Some(opts) = pending.iter().find_map(|i| i.get("createOpts").filter(|o| o.is_object()).cloned()) else {
            for it in pending {
                let _ = old.enqueue(it);
            }
            return;
        };
        self.stale.lock().unwrap().remove(id);
        self.sessions.lock().unwrap().remove(id);
        old.shutdown();
        match self.ensure(id, Some(&opts)) {
            Ok(created) => {
                for it in pending {
                    let _ = created.enqueue(it);
                }
            }
            Err(e) => eprintln!("[maker-registry] deferred set-model rebuild failed for {id}: {e}"),
        }
    }

    /// 网关 key 变了（sidecar 重新同步 / 轮换后通知）：存活会话的 key 文件原地改写。
    pub fn rotate_gateway_key(&self, key: &str) -> usize {
        self.factory.rotate_gateway_key(key)
    }

    /// 归 Rust 的通道清单（host.activate 时告诉 sidecar，远程控制端的 invoke 据此转回来）。
    pub fn owned_channels() -> Vec<&'static str> {
        let mut v: Vec<&'static str> = OWNED.to_vec();
        v.extend(["maker:list-active", "maker:any-session-in-turn"]);
        v.extend(crate::goal_host::GOAL_CHANNELS);
        v.extend(crate::local_db::LocalDb::owned_channels());
        v
    }

    /// 这条通道由 Rust 应答吗？
    pub fn owns(channel: &str) -> bool {
        OWNED.contains(&channel) || channel == "maker:list-active" || channel == "maker:any-session-in-turn" || crate::goal_host::owns(channel)
    }

    pub fn set_goal(&self, goal: Arc<crate::goal_host::GoalController>) {
        let _ = self.goal.set(goal);
    }

    pub fn goal(&self) -> Option<Arc<crate::goal_host::GoalController>> {
        self.goal.get().cloned()
    }

    fn get(&self, id: &str) -> Option<Arc<PiSession>> {
        self.sessions.lock().unwrap().get(id).cloned()
    }

    /// 取 live 会话；没有就用 createOpts 懒创建。无 createOpts 则 SESSION_NOT_FOUND。
    fn ensure(&self, id: &str, create_opts: Option<&Value>) -> Result<Arc<PiSession>, String> {
        if let Some(s) = self.get(id) {
            let is_stale = self.stale.lock().unwrap().contains(id);
            if !is_stale || s.turn_in_progress() {
                return Ok(s);
            }
            // 空闲且选择已变：关掉旧进程，下面按库里的新模型重建，历史由 switch_session 续上。
            self.stale.lock().unwrap().remove(id);
            self.sessions.lock().unwrap().remove(id);
            s.shutdown();
        }
        let Some(opts) = create_opts.filter(|o| o.is_object()) else {
            return Err(fail("SESSION_NOT_FOUND", format!("session {id} is not running and no createOpts were given")));
        };
        // 创建期间不持锁（拉起子进程可能慢）；创建完再登记，并发创建同 id 时后到者让位给先到者。
        let created = self.factory.create(id, opts)?;
        let mut g = self.sessions.lock().unwrap();
        if let Some(existing) = g.get(id) {
            let keep = existing.clone();
            drop(g);
            created.shutdown();
            return Ok(keep);
        }
        g.insert(id.to_string(), created.clone());
        Ok(created)
    }

    /// 进程退出后的清理（由 sink 的 exited 回调触发）：从表里摘掉，下次懒创建。
    pub fn on_exited(&self, id: &str) {
        self.sessions.lock().unwrap().remove(id);
    }

    pub fn any_in_turn(&self) -> bool {
        self.sessions.lock().unwrap().values().any(|s| s.turn_in_progress())
    }

    pub fn shutdown_all(&self) {
        // 先停 goal 控制器：否则它的定时器会在会话关掉后又拉起新一轮。
        if let Some(g) = self.goal.get() {
            g.dispose();
        }
        let all: Vec<Arc<PiSession>> = self.sessions.lock().unwrap().drain().map(|(_, s)| s).collect();
        for s in all {
            s.shutdown();
        }
    }

    /// `ipc.invoke("maker:*")` 的参数数组分发。返回值由 main.rs 编码。
    pub fn invoke(&self, channel: &str, args: &[Value]) -> Result<Value, String> {
        if crate::goal_host::owns(channel) {
            return match self.goal.get() {
                Some(g) => g.invoke(channel, args),
                None => Err(fail("PRECONDITION_FAILED", "goal host unavailable")),
            };
        }
        match channel {
            "maker:create-session" => {
                let opts = args.first().cloned().unwrap_or(Value::Null);
                if !opts.is_object() {
                    return Err(fail("INVALID_PARAMS", "createSession opts must be an object"));
                }
                if !opts["model"].as_str().is_some_and(|m| !m.is_empty()) {
                    return Err(fail("INVALID_PARAMS", "model required"));
                }
                let id = match opts["id"].as_str() {
                    Some(s) if valid_session_id(s) => s.to_string(),
                    Some(_) => return Err(fail("INVALID_PARAMS", "sessionId required")),
                    None => uuid::Uuid::new_v4().to_string(),
                };
                self.ensure(&id, Some(&opts))?;
                Ok(json!({
                    "sessionId": id,
                    "agentKind": "pi",
                    "workDir": opts["workingDir"],
                    "capabilities": Value::Null,
                    "usedProjectContext": false,
                }))
            }
            // 旧直发通道：走与 enqueue 相同的队列，保证回合 / 落库 / 审批语义只有一份。
            "maker:send" => {
                let id = session_id_arg(args)?;
                let text = args.get(1).and_then(Value::as_str).ok_or_else(|| fail("INVALID_PARAMS", "message required"))?;
                if let Some(s) = self.get(&id) {
                    if s.turn_in_progress() {
                        return Ok(json!({ "accepted": false, "reason": "turn-in-progress" }));
                    }
                }
                let create_opts = args.get(2).cloned().unwrap_or(Value::Null);
                let session = self.ensure(&id, Some(&create_opts))?;
                let client_id = format!("direct:{}", uuid::Uuid::new_v4());
                session.enqueue(json!({ "clientId": client_id, "text": text })).map_err(|e| fail("INVALID_PARAMS", e))?;
                Ok(json!({ "accepted": true }))
            }
            "maker:set-model" => {
                let id = session_id_arg(args)?;
                let model = args.get(1).and_then(Value::as_str).filter(|m| !m.is_empty()).ok_or_else(|| fail("INVALID_PARAMS", "model required"))?;
                let provider = match args.get(2) {
                    None | Some(Value::Null) => None,
                    Some(Value::String(p)) => Some(p.as_str()),
                    Some(_) => return Err(fail("INVALID_PARAMS", "invalid providerId")),
                };
                let selection = args.get(4).cloned().unwrap_or(Value::Null);
                if !selection.is_null() && !selection.is_object() {
                    return Err(fail("INVALID_PARAMS", "invalid selection"));
                }
                let current = if provider.is_none() { self.factory.current_provider(&id) } else { None };
                let (provider_id, model_id) = self.factory.resolve_selection(model, provider.or(current.as_deref()))?;
                let mut patch = json!({ "model": model_id, "providerId": provider_id });
                if let Some(e) = selection["effort"].as_str().filter(|e| !e.is_empty()) {
                    patch["effort"] = json!(e);
                }
                if let Some(f) = selection["fastMode"].as_bool() {
                    patch["fastMode"] = json!(f);
                }
                self.factory.persist_patch(&id, patch);
                let Some(live) = self.get(&id) else {
                    return Ok(json!({ "deferred": false })); // 未加载：下次懒创建按库里的模型走
                };
                self.stale.lock().unwrap().insert(id.clone());
                if live.turn_in_progress() {
                    let me = self.me.clone();
                    let sid = id.clone();
                    live.on_settled_rebuild(move || {
                        if let Some(r) = me.upgrade() {
                            r.rebuild_after_turn(&sid);
                        }
                    });
                    return Ok(json!({ "deferred": true, "superseded": false }));
                }
                // 空闲：立刻重建，renderer 马上看到新模型生效（失败留到下一次发送时报）。
                self.stale.lock().unwrap().remove(&id);
                self.sessions.lock().unwrap().remove(&id);
                live.shutdown();
                Ok(json!({ "deferred": false }))
            }
            "maker:set-effort" => {
                let id = session_id_arg(args)?;
                let effort = args.get(1).and_then(Value::as_str).filter(|e| !e.is_empty()).ok_or_else(|| fail("INVALID_PARAMS", "sessionId + effort required"))?;
                self.factory.persist_patch(&id, json!({ "effort": effort }));
                if let Some(s) = self.get(&id) {
                    // pi-rs 会按当前模型夹紧档位；不在目录里的档位不会被拒绝。
                    let _ = s.send(&json!({ "type": "set_thinking_level", "level": effort }));
                }
                Ok(Value::Null)
            }
            // pi-rs 没有 fast 档：只落库（与 sidecar 路径下 Pi 的表现一致），ultrafast 需要 Codex 运行时。
            "maker:set-fast-mode" => {
                let id = session_id_arg(args)?;
                match args.get(1) {
                    Some(Value::Bool(b)) => self.factory.persist_patch(&id, json!({ "fastMode": b })),
                    Some(Value::String(v)) if v == "ultrafast" => return Err(fail("NOT_SUPPORTED", "Ultrafast requires the Codex runtime")),
                    _ => return Err(fail("INVALID_PARAMS", "sessionId + enabled required")),
                }
                Ok(Value::Null)
            }
            // 关思考 = off；开思考沿用库里的档位，没有就 medium（pi-rs 会按模型夹紧）。
            "maker:set-thinking-enabled" => {
                let id = session_id_arg(args)?;
                let Some(on) = args.get(1).and_then(Value::as_bool) else {
                    return Err(fail("INVALID_PARAMS", "sessionId + enabled required"));
                };
                if let Some(s) = self.get(&id) {
                    let level = if on { "medium" } else { "off" };
                    let _ = s.send(&json!({ "type": "set_thinking_level", "level": level }));
                }
                Ok(Value::Null)
            }
            "maker:set-permission-mode" => {
                let id = session_id_arg(args)?;
                let mode = args.get(1).and_then(Value::as_str).unwrap_or("");
                if !PERMISSION_MODES.contains(&mode) {
                    return Err(fail("INVALID_PARAMS", "invalid permission mode"));
                }
                self.factory.persist_patch(&id, json!({ "permissionMode": mode }));
                if let Some(s) = self.get(&id) {
                    s.set_permission_mode(mode).map_err(|e| fail("INTERNAL", e.to_string()))?;
                }
                // 会话未加载时只需落库，下次创建按库里的档位启动（由 host.upsert-session 在建会话时写入）。
                Ok(Value::Null)
            }
            "maker:input:get-projection" => {
                let id = session_id_arg(args)?;
                let mut p = self.get(&id).map(|s| s.projection()).unwrap_or_else(|| empty_projection(&id));
                p["inputDeliveryVersion"] = json!(1);
                Ok(p)
            }
            "maker:input:enqueue" => {
                let id = session_id_arg(args)?;
                let item = args.get(1).cloned().unwrap_or(Value::Null);
                if !item.is_object() || !item["clientId"].is_string() || !item["text"].is_string() {
                    return Err(fail("INVALID_PARAMS", "queued message requires clientId and text"));
                }
                // 首条消息懒创建：失败直接返回错误，renderer 的 catch 路径会解析 [CODE]。
                let mut opts = item.get("createOpts").cloned().unwrap_or(Value::Null);
                if opts.is_object() && opts["workingDir"].as_str().map(str::is_empty).unwrap_or(true) {
                    if let Some(wd) = item["workingDir"].as_str() {
                        opts["workingDir"] = json!(wd);
                    }
                }
                let session = self.ensure(&id, Some(&opts))?;
                // `!cmd`：用户直接在会话目录里执行命令，不经模型。只认 renderer / 手机端的输入框（带附件的消息照常发给模型）。
                if let Some(cmd) = item["text"].as_str().and_then(crate::bang_cmd::parse) {
                    let has_attachments = ["images", "files"].iter().any(|k| item[*k].as_array().map(|a| !a.is_empty()).unwrap_or(false));
                    if !has_attachments && !item["origin"].is_object() {
                        return session.run_bang(&item, cmd.to_string()).map_err(|e| fail("INVALID_PARAMS", e.trim_start_matches("[INVALID_PARAMS] ")));
                    }
                }
                session.enqueue(item).map_err(|e| fail("INVALID_PARAMS", e))
            }
            "maker:input:stop" => {
                let id = session_id_arg(args)?;
                let o = args.get(1).cloned().unwrap_or(Value::Null);
                let keep = o["keepQueue"].as_bool().unwrap_or(false);
                let pause = o["pauseQueue"].as_bool().unwrap_or(false);
                Ok(self.get(&id).map(|s| s.stop(keep, pause)).unwrap_or_else(|| empty_projection(&id)))
            }
            "maker:input:resume" => {
                let id = session_id_arg(args)?;
                Ok(self.get(&id).map(|s| s.resume()).unwrap_or_else(|| empty_projection(&id)))
            }
            "maker:input:clear-error" => {
                let id = session_id_arg(args)?;
                Ok(self.get(&id).map(|s| s.clear_error()).unwrap_or_else(|| empty_projection(&id)))
            }
            "maker:input:remove" => {
                let id = session_id_arg(args)?;
                let cid = args.get(1).and_then(Value::as_str).unwrap_or("");
                Ok(self.get(&id).map(|s| s.remove_queued(cid)).unwrap_or_else(|| empty_projection(&id)))
            }
            "maker:input:set-edit-lock" => {
                let id = session_id_arg(args)?;
                let cid = args.get(1).and_then(Value::as_str).unwrap_or("");
                let locked = args.get(2).and_then(Value::as_bool).unwrap_or(false);
                Ok(self.get(&id).map(|s| s.set_edit_lock(cid, locked)).unwrap_or_else(|| empty_projection(&id)))
            }
            "maker:input:update-content" => {
                let id = session_id_arg(args)?;
                let cid = args.get(1).and_then(Value::as_str).unwrap_or("");
                let item = args.get(2).cloned().unwrap_or(Value::Null);
                match self.get(&id) {
                    Some(s) => s.update_content(cid, item).map_err(|e| fail("INVALID_PARAMS", e)),
                    None => Ok(empty_projection(&id)),
                }
            }
            "maker:get-pending-interactions" => {
                let Some(id) = args.first().and_then(Value::as_str).filter(|s| !s.is_empty()) else { return Ok(json!([])) };
                Ok(json!(self.get(id).map(|s| s.pending()).unwrap_or_default()))
            }
            "maker:resolve-interaction" => {
                let Some(request_id) = args.first().and_then(Value::as_str) else {
                    return Err(fail("INVALID_PARAMS", "requestId required"));
                };
                let decision = args.get(1).cloned().unwrap_or(Value::Null);
                if !decision.is_object() || !decision["kind"].is_string() {
                    return Err(fail("INVALID_PARAMS", "decision required"));
                }
                // requestId 全局唯一；逐会话尝试，谁登记了谁收尾。
                let all: Vec<Arc<PiSession>> = self.sessions.lock().unwrap().values().cloned().collect();
                let accepted = all.iter().any(|s| s.resolve(request_id, decision.clone()));
                Ok(json!({ "accepted": accepted }))
            }
            "maker:abort-session" => {
                let id = session_id_arg(args)?;
                if let Some(s) = self.get(&id) {
                    s.stop(true, false); // 不动排队，取消待审批，中止回合
                }
                Ok(Value::Null)
            }
            "maker:close-session" => {
                let id = session_id_arg(args)?;
                let s = self.sessions.lock().unwrap().remove(&id);
                if let Some(s) = s {
                    s.shutdown();
                }
                Ok(Value::Null)
            }
            "maker:list-active" => {
                let g = self.sessions.lock().unwrap();
                let mut rows: Vec<Value> = g.iter().map(|(id, s)| json!({ "sessionId": id, "agentKind": "pi", "isTurnRunning": s.turn_in_progress(), "pid": s.pid() })).collect();
                rows.sort_by(|a, b| a["sessionId"].as_str().cmp(&b["sessionId"].as_str()));
                Ok(json!(rows))
            }
            "maker:session-in-turn" => {
                let id = session_id_arg(args)?;
                Ok(json!(self.get(&id).map(|s| s.turn_in_progress()).unwrap_or(false)))
            }
            "maker:any-session-in-turn" => Ok(json!(self.any_in_turn())),
            other => Err(fail("NOT_IMPLEMENTED", format!("{other} is not handled by the Rust maker host"))),
        }
    }
}

/// 未加载的会话的空投影（形状同 InputQueue::projection）。
fn empty_projection(session_id: &str) -> Value {
    crate::input_queue::InputQueue::new().projection(session_id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pi_proc::SessionSink;
    use std::path::Path;

    struct NullSink;
    impl SessionSink for NullSink {
        fn maker_event(&self, _: &str, _: Value) {}
        fn interaction_request(&self, _: Value) {}
        fn interaction_dismissed(&self, _: Value) {}
        fn projection(&self, _: Value) {}
        fn exited(&self, _: &str, _: Option<i32>) {}
    }

    /// 假工厂：每个会话是一个 `cat` 进程（吞掉 stdin，不产生帧），并记录创建次数。
    struct CatFactory {
        created: Mutex<Vec<String>>,
    }
    impl SessionFactory for CatFactory {
        fn create(&self, id: &str, opts: &Value) -> Result<Arc<PiSession>, String> {
            if opts["fail"] == true {
                return Err(fail("MODEL_NOT_CONFIGURED", "model x is not configured"));
            }
            self.created.lock().unwrap().push(id.to_string());
            PiSession::spawn(id, Path::new("/bin/cat"), &[], &[], Path::new("/tmp"), Arc::new(NullSink)).map_err(|e| e.to_string())
        }
    }

    fn registry() -> (Arc<MakerRegistry>, Arc<CatFactory>) {
        let f = Arc::new(CatFactory { created: Mutex::new(Vec::new()) });
        (MakerRegistry::new(f.clone()), f)
    }

    fn item(id: &str) -> Value {
        json!({ "clientId": id, "text": "hi", "createOpts": { "model": "m" } })
    }

    #[test]
    fn ownership_covers_exactly_the_migrated_channels() {
        for c in [
            "maker:create-session", "maker:send", "maker:set-permission-mode", "maker:set-model", "maker:set-effort", "maker:set-fast-mode", "maker:set-thinking-enabled",
            "maker:input:enqueue", "maker:resolve-interaction", "maker:list-active", "maker:any-session-in-turn", "maker:close-session",
            "maker:goal:set", "maker:goal:get-status", "maker:list-desktop-commands", "maker:execute-desktop-command",
        ] {
            assert!(MakerRegistry::owns(c), "{c}");
        }
        for c in ["maker:set-plan-mode", "maker:get-session-tree", "maker:get-capabilities", "maker:switch-session-agent", "terminal:create"] {
            assert!(!MakerRegistry::owns(c), "{c} 仍归 sidecar");
        }
    }

    #[test]
    fn set_model_on_idle_session_rebuilds_on_next_use_and_defers_during_a_turn() {
        let (r, f) = registry();
        // 未加载：只落库，不建进程。
        assert_eq!(r.invoke("maker:set-model", &[json!("s0"), json!("m2"), json!("p")]).unwrap()["deferred"], false);
        assert!(f.created.lock().unwrap().is_empty());

        // 空闲的 live 会话：旧进程关闭，下一次使用时按新选择重建。
        r.invoke("maker:create-session", &[json!({ "id": "s1", "model": "m" })]).unwrap();
        assert_eq!(r.invoke("maker:set-model", &[json!("s1"), json!("m2"), json!("p")]).unwrap()["deferred"], false);
        assert!(!r.invoke("maker:session-in-turn", &[json!("s1")]).unwrap().as_bool().unwrap());
        r.invoke("maker:input:enqueue", &[json!("s1"), item("a")]).unwrap();
        assert_eq!(f.created.lock().unwrap().len(), 2, "换模型后重建了一次");

        // 回合中：延期，不动进程。
        let r2 = r.invoke("maker:set-model", &[json!("s1"), json!("m3"), json!("p")]).unwrap();
        assert_eq!(r2["deferred"], true);
        assert_eq!(f.created.lock().unwrap().len(), 2);
        r.shutdown_all();
    }

    #[test]
    fn set_model_and_set_effort_validate_their_arguments() {
        let (r, _f) = registry();
        assert!(r.invoke("maker:set-model", &[json!("s1")]).unwrap_err().starts_with("[INVALID_PARAMS]"));
        assert!(r.invoke("maker:set-model", &[json!("s1"), json!("m"), json!(5)]).unwrap_err().starts_with("[INVALID_PARAMS]"));
        assert!(r.invoke("maker:set-model", &[json!("s1"), json!("m"), Value::Null, Value::Null, json!([1])]).unwrap_err().starts_with("[INVALID_PARAMS]"));
        assert!(r.invoke("maker:set-effort", &[json!("s1")]).unwrap_err().starts_with("[INVALID_PARAMS]"));
        assert!(r.invoke("maker:set-effort", &[json!("s1"), json!("high")]).is_ok());
        assert!(r.invoke("maker:set-fast-mode", &[json!("s1"), json!("ultrafast")]).unwrap_err().starts_with("[NOT_SUPPORTED]"));
        assert!(r.invoke("maker:set-fast-mode", &[json!("s1"), json!("x")]).unwrap_err().starts_with("[INVALID_PARAMS]"));
        assert!(r.invoke("maker:set-fast-mode", &[json!("s1"), json!(true)]).is_ok());
        assert!(r.invoke("maker:set-thinking-enabled", &[json!("s1"), json!("yes")]).unwrap_err().starts_with("[INVALID_PARAMS]"));
        assert!(r.invoke("maker:set-thinking-enabled", &[json!("s1"), json!(false)]).is_ok());
    }

    #[test]
    fn enqueue_lazily_creates_once_and_requires_createopts() {
        let (r, f) = registry();
        let no_opts = r.invoke("maker:input:enqueue", &[json!("s1"), json!({ "clientId": "a", "text": "hi" })]).unwrap_err();
        assert!(no_opts.starts_with("[SESSION_NOT_FOUND]"), "{no_opts}");
        let p = r.invoke("maker:input:enqueue", &[json!("s1"), item("a")]).unwrap();
        assert_eq!(p["turnInProgress"], true);
        r.invoke("maker:input:enqueue", &[json!("s1"), item("b")]).unwrap();
        assert_eq!(f.created.lock().unwrap().len(), 1, "同一会话只创建一次");
        assert!(r.invoke("maker:session-in-turn", &[json!("s1")]).unwrap().as_bool().unwrap());
        assert!(r.invoke("maker:any-session-in-turn", &[]).unwrap().as_bool().unwrap());
        r.shutdown_all();
    }

    #[test]
    fn create_failure_propagates_the_coded_error_and_leaves_no_session() {
        let (r, _) = registry();
        let it = json!({ "clientId": "a", "text": "hi", "createOpts": { "fail": true } });
        let err = r.invoke("maker:input:enqueue", &[json!("s1"), it]).unwrap_err();
        assert!(err.starts_with("[MODEL_NOT_CONFIGURED]"), "{err}");
        assert_eq!(r.invoke("maker:list-active", &[]).unwrap(), json!([]));
    }

    #[cfg(unix)]
    #[test]
    fn bang_text_is_run_directly_but_attachments_and_goal_turns_go_to_the_model() {
        let (r, _) = registry();
        // 纯文本 !cmd：直接执行，不入队（队列里没有它，也没开回合）。
        let p = r.invoke("maker:input:enqueue", &[json!("s1"), json!({ "clientId": "a", "text": "!true", "createOpts": { "model": "m" } })]).unwrap();
        assert_eq!(p["turnInProgress"], false);
        assert!(p["pendingQueue"].as_array().unwrap().is_empty());
        // 带附件、goal 发起的回合：照常当作发给模型的消息（cat 吞掉 stdin，回合保持进行中）。
        let p = r.invoke("maker:input:enqueue", &[json!("s2"), json!({ "clientId": "a", "text": "!true", "images": [{ "x": 1 }], "createOpts": { "model": "m" } })]).unwrap();
        assert_eq!(p["turnInProgress"], true, "带图片的消息不当命令");
        let p = r.invoke("maker:input:enqueue", &[json!("s3"), json!({ "clientId": "a", "text": "!true", "origin": { "kind": "goal" }, "createOpts": { "model": "m" } })]).unwrap();
        assert_eq!(p["turnInProgress"], true, "goal 回合不当命令");
        // 回合进行中再发 !cmd：被拒绝，错误带 [INVALID_PARAMS] 码（只出现一次）。
        let err = r.invoke("maker:input:enqueue", &[json!("s2"), json!({ "clientId": "b", "text": "!true" })]).unwrap_err();
        assert!(err.starts_with("[INVALID_PARAMS] ") && !err["[INVALID_PARAMS] ".len()..].starts_with('['), "{err}");
        assert!(err.contains("模型正在回答"), "{err}");
    }

    #[test]
    fn sessions_are_isolated_and_listed() {
        let (r, _) = registry();
        r.invoke("maker:input:enqueue", &[json!("b"), item("1")]).unwrap();
        r.invoke("maker:input:enqueue", &[json!("a"), item("1")]).unwrap();
        let list = r.invoke("maker:list-active", &[]).unwrap();
        assert_eq!(list[0]["sessionId"], "a");
        assert_eq!(list[1]["sessionId"], "b");
        r.invoke("maker:input:stop", &[json!("a")]).unwrap();
        assert_eq!(r.invoke("maker:session-in-turn", &[json!("a")]).unwrap(), json!(false));
        assert_eq!(r.invoke("maker:session-in-turn", &[json!("b")]).unwrap(), json!(true), "停 a 不影响 b");
        r.shutdown_all();
    }

    #[test]
    fn unloaded_session_gets_an_empty_projection_and_unknown_ids_are_rejected() {
        let (r, _) = registry();
        let p = r.invoke("maker:input:get-projection", &[json!("ghost")]).unwrap();
        assert_eq!(p["turnInProgress"], false);
        assert_eq!(p["inputDeliveryVersion"], 1);
        assert_eq!(r.invoke("maker:get-pending-interactions", &[json!("ghost")]).unwrap(), json!([]));
        assert_eq!(r.invoke("maker:resolve-interaction", &[json!("r"), json!({ "kind": "permission", "behavior": "allow" })]).unwrap(), json!({ "accepted": false }));
        assert!(r.invoke("maker:input:get-projection", &[json!("../etc")]).unwrap_err().starts_with("[INVALID_PARAMS]"));
        assert!(r.invoke("maker:resolve-interaction", &[json!("r"), json!({})]).unwrap_err().starts_with("[INVALID_PARAMS]"));
        assert!(r.invoke("maker:input:enqueue", &[json!("s"), json!({ "clientId": "x" })]).unwrap_err().starts_with("[INVALID_PARAMS]"));
    }

    #[test]
    fn a_hung_process_is_aborted_by_the_watchdog_and_the_session_stays_usable() {
        use std::time::Duration;
        let (r, _f) = registry();
        let p = r.invoke("maker:input:enqueue", &[json!("s1"), item("a")]).unwrap();
        assert_eq!(p["turnInProgress"], true);
        // `cat` 读 stdin 但永不回应，等价于挂住的 pi-rs。
        let s = r.get("s1").unwrap();
        std::thread::sleep(Duration::from_millis(1200));
        s.check_stall(Duration::from_secs(1));
        assert!(!r.invoke("maker:session-in-turn", &[json!("s1")]).unwrap().as_bool().unwrap(), "卡死回合已收尾");
        // 会话仍可用：新消息能再次入队派发。
        let p = r.invoke("maker:input:enqueue", &[json!("s1"), item("b")]).unwrap();
        assert_eq!(p["turnInProgress"], true);
        r.shutdown_all();
    }

    #[test]
    fn close_removes_the_session_and_exit_cleanup_allows_recreate() {
        let (r, f) = registry();
        r.invoke("maker:input:enqueue", &[json!("s1"), item("a")]).unwrap();
        r.invoke("maker:close-session", &[json!("s1")]).unwrap();
        assert_eq!(r.invoke("maker:list-active", &[]).unwrap(), json!([]));
        r.invoke("maker:input:enqueue", &[json!("s1"), item("b")]).unwrap();
        r.on_exited("s1");
        r.invoke("maker:input:enqueue", &[json!("s1"), item("c")]).unwrap();
        assert_eq!(f.created.lock().unwrap().len(), 3, "关闭 / 退出后再来会重新创建");
        r.shutdown_all();
    }
}
