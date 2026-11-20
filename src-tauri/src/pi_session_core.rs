//! pi-rs 会话的帧处理核心（无进程、无 IO，可单测）。
//!
//! 输入：pi-rs 往 stdout 写的每一帧；renderer 的审批决策。
//! 输出：`Action` 列表，由外层的子进程包装器执行（写 stdin / 推 Tauri 事件）。
//!
//! 帧分流规则（与 pi-rs/baseline/permission-test.mjs 冻结的协议一致）：
//! - `response`：命令应答，不翻译；
//! - `extension_ui_request` 标题 `cindy:turn-change-capture`：任何模式都必须立刻回 `ok`，否则权限门挂起；
//! - `extension_ui_request` 标题 `cindy:permission`：payload 在 `placeholder`（JSON 字符串），登记进交互表并通知 renderer；
//! - 其余帧交给事件翻译器（与 TS 翻译器经 maker-freeze 逐帧对齐）。

#![allow(dead_code)]

use crate::input_queue::{Dispatch, InputQueue};
use crate::interactions::{InteractionTable, Settled};
use crate::maker_translate::{translate_event, MakerCtx};
use serde_json::{json, Value};
use std::time::Instant;

#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    /// 写一行 JSON 到 pi-rs 的 stdin。
    SendToPi(Value),
    /// 推 `maker:event`。
    MakerEvent(Value),
    /// 推 `maker:interaction-request`。
    InteractionRequest(Value),
    /// 推 `maker:interaction-dismissed`。
    InteractionDismissed(Value),
    /// 推 `maker:input:projection`（队列 / 回合状态变了）。
    Projection(Value),
    /// 用户消息落库（派发给 pi-rs 的同时写 user 行，与 sidecar 的 onAccepted 同口径）。
    UserMessage { client_id: String, content: String },
}

/// 回合进行中连续这么久没有任何帧（且没有待审批、没有在跑的工具）就判定卡死。
/// pi-rs 在等模型首包 / 流式输出时也会发 status / 文本增量，所以静默这么久只会是进程挂住或网络黑洞。
pub const STALL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(180);

pub struct SessionCore {
    pub session_id: String,
    ctx: MakerCtx,
    table: InteractionTable,
    queue: InputQueue,
    /// 最近一次收到任意帧的时间（回合开始时也重置）。
    last_frame: Option<Instant>,
    /// 正在执行的工具数：长时间工具（编译 / 安装）期间没有帧，不能算卡死。
    tools_running: usize,
}

/// 决策 → pi-rs 的 `extension_ui_response`。allow / user-deny:<理由> / cancelled。
pub fn decision_to_response(request_id: &str, decision: &Value) -> Value {
    let dismissed = decision["dismissed"].as_bool().unwrap_or(false);
    if dismissed {
        return json!({ "type": "extension_ui_response", "id": request_id, "cancelled": true });
    }
    // 提问：答案以 {问题: 答案} 的 JSON 文本回给 pi-rs（宿主发来的 dismissed 已在上面转成 cancelled）。
    if decision["kind"] == "ask_user_question" {
        let answers = if decision["answers"].is_object() { decision["answers"].clone() } else { json!({}) };
        return json!({ "type": "extension_ui_response", "id": request_id, "value": answers.to_string() });
    }
    if decision["behavior"] == "deny" {
        let reason = decision["reason"].as_str().unwrap_or("denied");
        return json!({ "type": "extension_ui_response", "id": request_id, "value": format!("user-deny:{reason}") });
    }
    json!({ "type": "extension_ui_response", "id": request_id, "value": "allow" })
}

impl SessionCore {
    pub fn new(session_id: &str, logger: Box<dyn Fn(&str) + Send>) -> Self {
        Self { session_id: session_id.to_string(), ctx: MakerCtx::new(logger), table: InteractionTable::new(), queue: InputQueue::new(), last_frame: None, tools_running: 0 }
    }

    pub fn pending(&self, session_id: &str) -> Vec<Value> {
        self.table.snapshot(session_id)
    }

    /// 看门狗：外层定时调用。回合进行中、无待审批、无在跑工具且静默超时 → 以终止性 error 收尾回合并让 pi-rs 中止。
    /// 返回空表示没卡死。收尾后队列里的下一条由外层 `after_settled` 接着派发。
    pub fn check_stall(&mut self, now: Instant, timeout: std::time::Duration) -> Vec<Action> {
        let Some(token) = self.queue.active_token() else { return Vec::new() };
        if self.table.len() > 0 || self.tools_running > 0 {
            return Vec::new();
        }
        let Some(last) = self.last_frame else { return Vec::new() };
        if now.saturating_duration_since(last) < timeout {
            return Vec::new();
        }
        (self.ctx.logger)(&format!("stall: no frames for {}s, aborting turn {token}", timeout.as_secs()));
        // 先登记再发 abort：pi 的 aborted 终态可能先于 RPC 回执到达，不能被当成上游中断。
        self.ctx.mark_host_abort_requested();
        let mut out = vec![Action::SendToPi(json!({ "type": "abort" }))];
        let ev = json!({
            "type": "error",
            "data": { "message": "The agent stopped responding and the turn was aborted. You can retry.", "isTerminal": true, "code": "AGENT_STALLED" },
            "source": "pi",
            "turnAttemptToken": token,
        });
        // 与正常终态同路径：队列收尾 + 推事件 + 推投影。
        self.queue.on_event(&ev);
        self.tools_running = 0;
        out.push(Action::MakerEvent(ev));
        out.push(Action::Projection(self.queue.projection(&self.session_id)));
        out
    }

    pub fn on_frame(&mut self, frame: &Value, now: Instant) -> Vec<Action> {
        self.last_frame = Some(now);
        match frame["type"].as_str() {
            Some("response") => self.on_response(frame),
            Some("extension_ui_request") => self.on_ui_request(frame, now),
            _ => self.on_agent_frame(frame),
        }
    }

    /// 翻译后的事件：盖当前回合的 turnAttemptToken（session.ts 同规则：非后台、当前回合、已有的不覆盖），
    /// 终态事件收尾回合并推投影。
    fn on_agent_frame(&mut self, frame: &Value) -> Vec<Action> {
        match frame["type"].as_str() {
            Some("tool_execution_start") => self.tools_running += 1,
            Some("tool_execution_end") => self.tools_running = self.tools_running.saturating_sub(1),
            _ => {}
        }
        let mut out = Vec::new();
        let mut settled = false;
        for mut ev in translate_event(frame, &mut self.ctx) {
            if let Some(token) = self.queue.active_token() {
                if ev["turnScope"].as_str() != Some("background") && ev.get("turnAttemptToken").is_none() {
                    ev["turnAttemptToken"] = json!(token);
                }
            }
            // 回合来源（goal 等）：session.ts 同规则，非后台、已有的不覆盖。goal 控制器靠它区分自己发起的回合。
            if ev["turnScope"].as_str() != Some("background") && ev.get("turnOrigin").is_none() {
                if let Some(origin) = self.queue.active_origin() {
                    ev["turnOrigin"] = origin.clone();
                }
            }
            if self.queue.on_event(&ev) {
                settled = true;
            }
            out.push(Action::MakerEvent(ev));
        }
        if settled {
            out.push(Action::Projection(self.queue.projection(&self.session_id)));
        }
        out
    }

    // ── 输入队列 ────────────────────────────────────────────────────

    pub fn projection(&self) -> Value {
        self.queue.projection(&self.session_id)
    }

    /// `maker:input:enqueue`。入队后尝试派发，返回要执行的动作（含 prompt 命令）。
    pub fn enqueue(&mut self, item: Value) -> Result<Vec<Action>, String> {
        self.queue.enqueue(item)?;
        Ok(self.pump())
    }

    #[cfg(test)]
    pub fn queue_for_test(&mut self) -> &mut InputQueue {
        &mut self.queue
    }

    /// 取下一条并生成发给 pi-rs 的 prompt。`id` 带 token，便于对应 response。
    pub fn pump(&mut self) -> Vec<Action> {
        let Some(Dispatch { item, token }) = self.queue.next() else { return Vec::new() };
        // 回合开始计一次「最近有动静」，否则上一回合结束后的静默会让新回合一派发就被判卡死。
        self.last_frame = Some(Instant::now());
        self.tools_running = 0;
        let text = item["text"].as_str().unwrap_or("");
        let content = item["persistedContent"].as_str().unwrap_or(text).to_string();
        let mut out = Vec::new();
        // goal 发起的回合（带 origin）不写 user 行，与 sidecar 路径一致。
        if !item["origin"].is_object() {
            out.push(Action::UserMessage { client_id: item["clientId"].as_str().unwrap_or("").to_string(), content });
        }
        out.push(Action::Projection(self.queue.projection(&self.session_id)));
        // prompt RPC 发出前登记其尚未到达的 agent_start：此时来的 Stop 针对的是这一代回合。
        self.ctx.mark_host_turn_start_pending();
        out.push(Action::SendToPi(json!({ "id": format!("turn-{token}"), "type": "prompt", "message": text })));
        out
    }

    /// 回合结束后继续派发下一条（外层在收到 Projection 且回合已收尾时调用）。
    pub fn after_settled(&mut self) -> Vec<Action> {
        if self.queue.turn_in_progress() {
            return Vec::new();
        }
        self.pump()
    }

    /// pi-rs 对 prompt 的 response 失败：回合以 error 收尾（只作用于该 token）。
    pub fn on_response(&mut self, frame: &Value) -> Vec<Action> {
        if frame["success"] != json!(false) {
            return Vec::new();
        }
        let Some(token) = frame["id"].as_str().and_then(|i| i.strip_prefix("turn-")).and_then(|t| t.parse::<i64>().ok()) else {
            return Vec::new();
        };
        let msg = frame["error"].as_str().unwrap_or("send not accepted");
        if self.queue.dispatch_failed(token, msg) {
            vec![Action::Projection(self.queue.projection(&self.session_id))]
        } else {
            Vec::new()
        }
    }

    /// `maker:input:stop` / `maker:abort-session`。返回给 pi-rs 的 abort 与审批收尾。
    pub fn stop(&mut self, keep_queue: bool, pause_queue: bool) -> Vec<Action> {
        self.queue.stop(keep_queue, pause_queue);
        let mut out = self.cancel_all("session_aborted");
        if self.queue.turn_in_progress() {
            self.ctx.mark_host_abort_requested();
            out.push(Action::SendToPi(json!({ "type": "abort" })));
            self.queue.clear_active();
        }
        out.push(Action::Projection(self.queue.projection(&self.session_id)));
        out
    }

    pub fn take_pending(&mut self) -> Vec<Value> {
        self.queue.take_pending()
    }

    pub fn resume(&mut self) -> Vec<Action> {
        self.queue.resume();
        let mut out = vec![Action::Projection(self.queue.projection(&self.session_id))];
        out.extend(self.pump());
        out
    }

    pub fn remove_queued(&mut self, client_id: &str) -> Vec<Action> {
        self.queue.remove(client_id);
        vec![Action::Projection(self.queue.projection(&self.session_id))]
    }

    pub fn clear_error(&mut self) -> Vec<Action> {
        self.queue.clear_error();
        vec![Action::Projection(self.queue.projection(&self.session_id))]
    }

    pub fn turn_in_progress(&self) -> bool {
        self.queue.turn_in_progress()
    }

    /// 进程退出：回合清零，所有悬挂审批以拒绝收尾。
    pub fn on_exit(&mut self) -> Vec<Action> {
        let mut out = self.cancel_all("session_closed");
        if self.queue.on_status_closed() {
            out.push(Action::Projection(self.queue.projection(&self.session_id)));
        }
        out
    }

    fn on_ui_request(&mut self, frame: &Value, now: Instant) -> Vec<Action> {
        let id = frame["id"].as_str().unwrap_or("").to_string();
        match frame["title"].as_str() {
            Some("cindy:turn-change-capture") => {
                vec![Action::SendToPi(json!({ "type": "extension_ui_response", "id": id, "value": "ok" }))]
            }
            Some("cindy:permission") => {
                // 解析失败按拒绝处理（fail-closed）：宁可让这次工具调用失败，也不放行没读懂的请求。
                let payload: Value = match frame["placeholder"].as_str().map(serde_json::from_str) {
                    Some(Ok(v)) => v,
                    _ => {
                        return vec![Action::SendToPi(json!({
                            "type": "extension_ui_response", "id": id, "value": "user-deny:unreadable permission request"
                        }))]
                    }
                };
                let mut request = if payload.is_object() { payload } else { json!({ "payload": payload }) };
                request["requestId"] = json!(id);
                request["kind"] = json!("permission");
                if self.table.register(&self.session_id, request.clone(), None, now).is_err() {
                    return vec![Action::SendToPi(json!({
                        "type": "extension_ui_response", "id": id, "value": "user-deny:invalid permission request"
                    }))];
                }
                vec![Action::InteractionRequest(json!({ "sessionId": self.session_id, "request": request }))]
            }
            Some("cindy:ask-user-question") => {
                let questions = frame["placeholder"].as_str()
                    .and_then(|p| serde_json::from_str::<Value>(p).ok())
                    .and_then(|v| v["questions"].as_array().cloned())
                    .filter(|q| !q.is_empty());
                let Some(questions) = questions else {
                    return vec![Action::SendToPi(json!({ "type": "extension_ui_response", "id": id, "cancelled": true }))];
                };
                let request = json!({ "requestId": id, "kind": "ask_user_question", "questions": questions });
                if self.table.register(&self.session_id, request.clone(), None, now).is_err() {
                    return vec![Action::SendToPi(json!({ "type": "extension_ui_response", "id": id, "cancelled": true }))];
                }
                vec![Action::InteractionRequest(json!({ "sessionId": self.session_id, "request": request }))]
            }
            // 其它 UI 请求（通知 / 状态栏等）本阶段没有 renderer 对应物：不应答、不翻译。
            _ => Vec::new(),
        }
    }

    /// renderer 的决策（`maker:resolve-interaction`）。返回 None 即 `{accepted:false}`。
    pub fn resolve(&mut self, request_id: &str, decision: Value) -> Option<Vec<Action>> {
        let settled = self.table.resolve(request_id, decision)?;
        Some(self.settled_actions(vec![settled]))
    }

    /// 中止 / 关闭 / 超时：待处理项以默认决策收尾，pi-rs 立刻收到拒绝，不会卡在等待里。
    pub fn cancel_all(&mut self, reason: &str) -> Vec<Action> {
        let settled = self.table.cancel_session(&self.session_id.clone(), reason);
        self.settled_actions(settled)
    }

    pub fn expire(&mut self, now: Instant) -> Vec<Action> {
        let settled = self.table.expire(now);
        self.settled_actions(settled)
    }

    fn settled_actions(&self, settled: Vec<Settled>) -> Vec<Action> {
        let mut out = Vec::new();
        for s in settled {
            out.push(Action::SendToPi(decision_to_response(&s.request_id, &s.decision)));
            out.push(Action::InteractionDismissed(s.dismissed));
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::interactions::PERMISSION_TIMEOUT;

    fn core() -> SessionCore {
        SessionCore::new("s1", Box::new(|_| {}))
    }
    fn perm_frame(id: &str) -> Value {
        json!({ "type": "extension_ui_request", "id": id, "method": "input", "title": "cindy:permission",
                "placeholder": json!({ "tool": "write", "path": "/tmp/x" }).to_string() })
    }

    #[test]
    fn turn_change_capture_is_acked_immediately() {
        let mut c = core();
        let f = json!({ "type": "extension_ui_request", "id": "c1", "method": "confirm", "title": "cindy:turn-change-capture" });
        assert_eq!(c.on_frame(&f, Instant::now()), vec![Action::SendToPi(json!({ "type": "extension_ui_response", "id": "c1", "value": "ok" }))]);
    }

    #[test]
    fn permission_request_registers_then_allow_answers_pi() {
        let mut c = core();
        let acts = c.on_frame(&perm_frame("p1"), Instant::now());
        assert_eq!(acts.len(), 1);
        let Action::InteractionRequest(v) = &acts[0] else { panic!("expected request") };
        assert_eq!(v["request"]["requestId"], "p1");
        assert_eq!(v["request"]["tool"], "write");
        assert_eq!(c.pending("s1").len(), 1);

        let out = c.resolve("p1", json!({ "kind": "permission", "behavior": "allow" })).unwrap();
        assert_eq!(out[0], Action::SendToPi(json!({ "type": "extension_ui_response", "id": "p1", "value": "allow" })));
        let Action::InteractionDismissed(d) = &out[1] else { panic!("expected dismissed") };
        assert_eq!(d["resolvedAs"], "allow");
        assert!(c.pending("s1").is_empty());
        assert!(c.resolve("p1", json!({ "kind": "permission", "behavior": "allow" })).is_none());
    }

    #[test]
    fn deny_carries_reason_and_wrong_kind_is_rejected() {
        let mut c = core();
        c.on_frame(&perm_frame("p1"), Instant::now());
        assert!(c.resolve("p1", json!({ "kind": "plan_review", "behavior": "allow" })).is_none());
        let out = c.resolve("p1", json!({ "kind": "permission", "behavior": "deny", "reason": "no way" })).unwrap();
        assert_eq!(out[0], Action::SendToPi(json!({ "type": "extension_ui_response", "id": "p1", "value": "user-deny:no way" })));
    }

    #[test]
    fn abort_denies_every_pending_request_so_pi_never_hangs() {
        let mut c = core();
        c.on_frame(&perm_frame("p1"), Instant::now());
        c.on_frame(&perm_frame("p2"), Instant::now());
        let out = c.cancel_all("session_aborted");
        let sends: Vec<&Value> = out.iter().filter_map(|a| if let Action::SendToPi(v) = a { Some(v) } else { None }).collect();
        assert_eq!(sends.len(), 2);
        assert!(sends.iter().all(|v| v["value"] == "user-deny:session_aborted"));
        assert!(c.pending("s1").is_empty());
    }

    #[test]
    fn permission_times_out_into_a_denial() {
        let mut c = core();
        let t0 = Instant::now();
        c.on_frame(&perm_frame("p1"), t0);
        assert!(c.expire(t0).is_empty());
        let out = c.expire(t0 + PERMISSION_TIMEOUT);
        assert_eq!(out[0], Action::SendToPi(json!({ "type": "extension_ui_response", "id": "p1", "value": "user-deny:timeout" })));
    }

    #[test]
    fn unreadable_permission_payload_fails_closed() {
        let mut c = core();
        let f = json!({ "type": "extension_ui_request", "id": "p9", "title": "cindy:permission", "placeholder": "{not json" });
        let acts = c.on_frame(&f, Instant::now());
        let Action::SendToPi(v) = &acts[0] else { panic!("expected immediate denial") };
        assert!(v["value"].as_str().unwrap().starts_with("user-deny:"));
        assert!(c.pending("s1").is_empty());
    }

    #[test]
    fn dispatching_a_turn_also_persists_the_user_message() {
        let mut c = SessionCore::new("s1", Box::new(|_| {}));
        let acts = c.enqueue(json!({ "clientId": "c1", "text": "hi", "persistedContent": "{\"text\":\"hi\"}" })).unwrap();
        assert_eq!(acts[0], Action::UserMessage { client_id: "c1".into(), content: "{\"text\":\"hi\"}".into() });
        let acts = c.enqueue(json!({ "clientId": "c2", "text": "later" })).unwrap();
        assert!(acts.is_empty(), "排队中的消息尚未派发，不落 user 行");
        let mut g = SessionCore::new("s2", Box::new(|_| {}));
        let acts = g.enqueue(json!({ "clientId": "g1", "text": "continue", "origin": { "kind": "goal" } })).unwrap();
        assert!(!acts.iter().any(|a| matches!(a, Action::UserMessage { .. })), "goal 回合不写 user 行");
    }

    #[test]
    fn goal_turn_events_carry_turn_origin_and_the_next_turn_does_not_inherit_it() {
        let mut c = SessionCore::new("s1", Box::new(|_| {}));
        let delta = || json!({ "type": "message_update", "assistantMessageEvent": { "type": "text_delta", "delta": "hi" } });
        let origins = |acts: &[Action]| -> Vec<Value> {
            acts.iter().filter_map(|a| if let Action::MakerEvent(e) = a { Some(e.get("turnOrigin").cloned().unwrap_or(Value::Null)) } else { None }).collect()
        };
        c.enqueue(json!({ "clientId": "g1", "text": "go", "origin": { "kind": "goal", "goalSessionId": "s1" } })).unwrap();
        let o = origins(&c.on_frame(&delta(), Instant::now()));
        assert!(!o.is_empty() && o.iter().all(|x| x["kind"] == "goal"), "goal 回合内的事件都带 turnOrigin: {o:?}");
        // 回合收尾（done）后下一条普通消息的事件不应继承 goal 的 origin。
        let token = c.queue_for_test().active_token().unwrap();
        assert!(c.queue_for_test().on_event(&json!({ "type": "done", "data": {}, "turnAttemptToken": token })), "done 应收尾回合");
        c.enqueue(json!({ "clientId": "u1", "text": "hi" })).unwrap();
        let o = origins(&c.on_frame(&delta(), Instant::now()));
        assert!(!o.is_empty() && o.iter().all(|x| x.is_null()), "普通回合不继承 goal 的 origin: {o:?}");
    }

    #[test]
    fn stalled_turn_is_aborted_with_a_terminal_error_and_frees_the_queue() {
        use std::time::Duration;
        let mut c = SessionCore::new("s1", Box::new(|_| {}));
        let t0 = Instant::now();
        c.enqueue(json!({ "clientId": "a", "text": "hi" })).unwrap();
        c.enqueue(json!({ "clientId": "b", "text": "next" })).unwrap();
        // 静默未超时：不动。
        assert!(c.check_stall(t0 + Duration::from_secs(10), Duration::from_secs(60)).is_empty());
        // 超时：abort + 终止性 error（带当前 token）+ 投影，回合已收尾。
        let acts = c.check_stall(t0 + Duration::from_secs(120), Duration::from_secs(60));
        assert!(acts.iter().any(|a| matches!(a, Action::SendToPi(v) if v["type"] == "abort")));
        let Some(Action::MakerEvent(ev)) = acts.iter().find(|a| matches!(a, Action::MakerEvent(_))) else { panic!("缺 error 事件") };
        assert_eq!(ev["type"], "error");
        assert_eq!(ev["data"]["isTerminal"], true);
        assert_eq!(ev["data"]["code"], "AGENT_STALLED");
        assert_eq!(ev["turnAttemptToken"], 1);
        assert!(!c.projection()["turnInProgress"].as_bool().unwrap(), "回合已收尾");
        // 收尾后队列里的下一条可以接着派发；没有活动回合时看门狗不再触发。
        assert!(c.check_stall(t0 + Duration::from_secs(500), Duration::from_secs(60)).is_empty(), "已收尾，看门狗不重复触发");
        let next = c.after_settled();
        assert!(next.iter().any(|a| matches!(a, Action::SendToPi(v) if v["type"] == "prompt" && v["message"] == "next")), "下一条 b 继续派发");
        // 新回合的计时从派发时刻起，不继承上一回合的静默。
        assert!(c.check_stall(Instant::now(), Duration::from_secs(60)).is_empty());
    }

    #[test]
    fn user_stop_settles_as_cancelled_and_a_pi_side_abort_without_stop_is_a_failure() {
        let aborted = || {
            let m = json!({ "role": "assistant", "content": [], "model": "m", "api": "openai-completions", "stopReason": "aborted", "errorMessage": "Request was aborted",
                            "usage": { "input": 1, "output": 0, "cacheRead": 0, "cacheWrite": 0, "totalTokens": 1 }, "timestamp": 1 });
            vec![json!({ "type": "agent_start" }), json!({ "type": "message_start", "message": m }), json!({ "type": "message_end", "message": m }),
                 json!({ "type": "agent_end", "messages": [] }), json!({ "type": "agent_settled", "aborted": true })]
        };
        let done_status = |acts: &[Action]| acts.iter().find_map(|a| match a { Action::MakerEvent(e) if e["type"] == "done" => Some(e["data"]["status"].clone()), _ => None });
        // 用户 Stop：登记发生在 abort RPC 之前，aborted 终态收成 cancelled，且没有终止性 error。
        let mut c = SessionCore::new("s1", Box::new(|_| {}));
        c.enqueue(json!({ "clientId": "a", "text": "hi" })).unwrap();
        c.on_frame(&json!({ "type": "agent_start" }), Instant::now());
        c.stop(false, false);
        let acts: Vec<Action> = aborted().iter().flat_map(|f| c.on_frame(f, Instant::now())).collect();
        assert!(!acts.iter().any(|a| matches!(a, Action::MakerEvent(e) if e["type"] == "error")), "Stop 不产生错误: {acts:?}");
        // Stop 清掉了活动回合，done 带不上 token；这里只验证翻译层的判定，用 status 取值。
        assert_eq!(done_status(&acts), Some(json!("cancelled")));
        // 没有 Stop 的同样帧流：Pi 自己的请求失败，是 failed。
        let mut c = SessionCore::new("s1", Box::new(|_| {}));
        c.enqueue(json!({ "clientId": "a", "text": "hi" })).unwrap();
        let acts: Vec<Action> = aborted().iter().flat_map(|f| c.on_frame(f, Instant::now())).collect();
        assert_eq!(done_status(&acts), Some(json!("failed")));
    }

    #[test]
    fn watchdog_waits_for_running_tools_and_pending_approvals() {
        use std::time::Duration;
        let mut c = SessionCore::new("s1", Box::new(|_| {}));
        let t0 = Instant::now();
        c.enqueue(json!({ "clientId": "a", "text": "hi" })).unwrap();
        // 长时间工具：start 之后没有帧也不算卡死；end 之后恢复计时。
        c.on_frame(&json!({ "type": "tool_execution_start", "toolCallId": "t1", "toolName": "bash", "args": {} }), t0);
        assert!(c.check_stall(t0 + Duration::from_secs(900), Duration::from_secs(60)).is_empty(), "工具在跑");
        c.on_frame(&json!({ "type": "tool_execution_end", "toolCallId": "t1", "toolName": "bash", "result": {}, "isError": false }), t0 + Duration::from_secs(900));
        assert!(c.check_stall(t0 + Duration::from_secs(910), Duration::from_secs(60)).is_empty(), "刚有动静");
        // 待审批：等人，不算卡死。
        let req = json!({ "type": "extension_ui_request", "id": "r1", "method": "input", "title": "cindy:permission",
            "placeholder": json!({ "toolName": "bash", "toolInput": {}, "toolUseId": "t2", "requestId": "r1", "kind": "permission" }).to_string() });
        c.on_frame(&req, t0 + Duration::from_secs(910));
        assert!(c.check_stall(t0 + Duration::from_secs(5000), Duration::from_secs(60)).is_empty(), "等待审批");
    }

    #[test]
    fn ask_user_question_registers_a_question_card_and_answers_pi_with_json() {
        let mut c = core();
        let frame = json!({ "type": "extension_ui_request", "id": "q1", "method": "input", "title": "cindy:ask-user-question",
            "placeholder": json!({ "questions": [{ "question": "删哪一个?", "options": [{ "label": "A" }, { "label": "B" }] }] }).to_string() });
        let acts = c.on_frame(&frame, Instant::now());
        let Action::InteractionRequest(v) = &acts[0] else { panic!("expected request") };
        assert_eq!(v["request"]["kind"], "ask_user_question");
        assert_eq!(v["request"]["requestId"], "q1");
        assert_eq!(v["request"]["questions"][0]["question"], "删哪一个?");
        assert_eq!(c.pending("s1").len(), 1);

        let out = c.resolve("q1", json!({ "kind": "ask_user_question", "answers": { "删哪一个?": "A" } })).unwrap();
        let Action::SendToPi(resp) = &out[0] else { panic!("expected response") };
        assert_eq!(resp["id"], "q1");
        let got: Value = serde_json::from_str(resp["value"].as_str().unwrap()).unwrap();
        assert_eq!(got, json!({ "删哪一个?": "A" }));
    }

    #[test]
    fn ask_user_question_with_no_questions_is_cancelled_instead_of_hanging() {
        let mut c = core();
        let frame = json!({ "type": "extension_ui_request", "id": "q2", "method": "input", "title": "cindy:ask-user-question", "placeholder": "{}" });
        assert_eq!(c.on_frame(&frame, Instant::now()), vec![Action::SendToPi(json!({ "type": "extension_ui_response", "id": "q2", "cancelled": true }))]);
        assert!(c.pending("s1").is_empty());
    }

    #[test]
    fn dismissed_decision_maps_to_cancelled_and_responses_are_ignored() {
        assert_eq!(
            decision_to_response("q", &json!({ "kind": "ask_user_question", "answers": {}, "dismissed": true })),
            json!({ "type": "extension_ui_response", "id": "q", "cancelled": true })
        );
        assert!(core().on_frame(&json!({ "type": "response", "id": "x", "success": true }), Instant::now()).is_empty());
    }
}
