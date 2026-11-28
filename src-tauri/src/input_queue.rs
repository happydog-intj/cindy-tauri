//! 输入队列与 turn 生命周期（纯状态机）。
//!
//! 对应 sidecar/src/maker-ipc.mjs 的 `getState`、`pump`、`onTurnSettled`、`projection`
//! 以及 maker:input:* 通道，语义逐条保持：
//! - 同一个 clientId 重试不重复入队；
//! - 回合进行中或队列暂停时不派发；派发时领一个递增的 turn token；
//! - 只有「当前 token 的、非后台的、终态事件」才结束回合（`is_terminal_turn_event` 对齐 maker-core 的
//!   `isTerminalTurnEvent`：done、isRunning=false 的 status、终态 error）；
//! - 派发失败只在 token 仍是当前回合时清掉回合并记录 error；
//! - stop 默认清空队列，keepQueue 保留，pauseQueue 暂停；
//! - 会话关闭时回合清零。
//!
//! 不起线程、不碰 IO。外层根据返回的 `Dispatch` 去给 pi-rs 发 prompt。

#![allow(dead_code)]

use serde_json::{json, Value};

pub fn is_terminal_turn_event(event: &Value) -> bool {
    match event["type"].as_str() {
        Some("done") => true,
        Some("status") => event["data"]["isRunning"] == json!(false),
        Some("error") => {
            let data = &event["data"];
            if !data.is_object() {
                return true;
            }
            if let Some(b) = data["isTerminal"].as_bool() {
                return b;
            }
            if let Some(b) = data["willRetry"].as_bool() {
                return !b;
            }
            true
        }
        _ => false,
    }
}

/// 一次派发：外层按 `item` 给 pi-rs 发 prompt，并把 `token` 记作本回合的 turnAttemptToken。
#[derive(Debug, Clone, PartialEq)]
pub struct Dispatch {
    pub item: Value,
    pub token: i64,
}

#[derive(Default)]
pub struct InputQueue {
    pending: Vec<Value>,
    /// (clientId, token)
    active: Option<(String, i64)>,
    /// 当前回合的来源（队列项的 `origin`，如 goal）。maker-core 把它盖在回合内每个事件的 turnOrigin 上。
    active_origin: Option<Value>,
    turn_seq: i64,
    paused: bool,
    /// 正在被用户编辑的排队消息（`maker:input:set-edit-lock`）。队首被锁时不派发，保持顺序。
    edit_locks: Vec<String>,
    error: Option<String>,
}

impl InputQueue {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn turn_in_progress(&self) -> bool {
        self.active.is_some()
    }

    pub fn active_token(&self) -> Option<i64> {
        self.active.as_ref().map(|(_, t)| *t)
    }

    /// 只在回合进行中有值：收尾后 `active` 清空，残留的 origin 不能盖到下一个（非 goal）回合上。
    pub fn active_origin(&self) -> Option<&Value> {
        self.active.as_ref().and(self.active_origin.as_ref())
    }

    /// `maker:input:get-projection` / `maker:input:projection` 推送的形状。
    pub fn projection(&self, session_id: &str) -> Value {
        json!({
            "sessionId": session_id,
            "pendingQueue": self.pending,
            "turnInProgress": self.active.is_some(),
            "clearBoundaryMs": null,
            "continuationInFlightClientId": null,
            "continuationTurnClientId": null,
            "steeringQueueClientIds": [],
            "queuePaused": self.paused,
            "queueExpanded": false,
            "queueInteractionLocks": [],
            "queueEditLocks": self.edit_locks,
            "queueAbortPending": false,
            "error": self.error,
            "recovery": null,
            "errorRetryText": null,
            "credentialSwitchWait": null,
        })
    }

    /// 入队。返回 Err 表示参数不合法；同一 clientId 已在队列或正在跑则不重复入队。
    pub fn enqueue(&mut self, item: Value) -> Result<(), String> {
        let client_id = item["clientId"].as_str().ok_or("queued message requires clientId and text")?.to_string();
        if !item["text"].is_string() {
            return Err("queued message requires clientId and text".into());
        }
        let dup = self.active.as_ref().is_some_and(|(c, _)| *c == client_id)
            || self.pending.iter().any(|q| q["clientId"].as_str() == Some(client_id.as_str()));
        if !dup {
            self.pending.push(item);
        }
        Ok(())
    }

    /// 取下一条待派发项。回合进行中、暂停或队列为空时返回 None。
    pub fn next(&mut self) -> Option<Dispatch> {
        if self.active.is_some() || self.paused || self.pending.is_empty() {
            return None;
        }
        if self.pending[0]["clientId"].as_str().is_some_and(|c| self.edit_locks.iter().any(|l| l == c)) {
            return None;
        }
        let item = self.pending.remove(0);
        self.turn_seq += 1;
        let token = self.turn_seq;
        self.active = Some((item["clientId"].as_str().unwrap_or("").to_string(), token));
        self.active_origin = item["origin"].is_object().then(|| item["origin"].clone());
        self.error = None;
        Some(Dispatch { item, token })
    }

    /// 派发失败（建会话 / send 被拒）。只在 token 仍是当前回合时生效，避免误清后来的回合。
    pub fn dispatch_failed(&mut self, token: i64, message: &str) -> bool {
        if self.active_token() == Some(token) {
            self.active = None;
            self.error = Some(message.to_string());
            return true;
        }
        false
    }

    /// 一个 maker 事件到达。返回 true 表示它结束了当前回合（外层应落库、推投影、再 `next()`）。
    pub fn on_event(&mut self, event: &Value) -> bool {
        let Some(token) = self.active_token() else { return false };
        if event["turnScope"].as_str() == Some("background") {
            return false;
        }
        if event["turnAttemptToken"].as_i64() != Some(token) {
            return false;
        }
        if !is_terminal_turn_event(event) {
            return false;
        }
        if event["type"] == "error" {
            let msg = event["data"]["message"].as_str().unwrap_or("Agent error");
            self.error = Some(msg.to_string());
        }
        self.active = None;
        true
    }

    /// `maker:input:stop`。默认清空队列；keepQueue 保留；pauseQueue 暂停。
    pub fn stop(&mut self, keep_queue: bool, pause_queue: bool) {
        if !keep_queue {
            self.pending.clear();
        }
        if pause_queue {
            self.paused = true;
        }
    }

    /// `maker:abort-session`：不动排队，只收尾回合。
    pub fn clear_active(&mut self) {
        self.active = None;
    }

    pub fn resume(&mut self) {
        self.paused = false;
    }

    pub fn clear_error(&mut self) {
        self.error = None;
    }

    /// 取走所有待派发项（换模型重建时迁到新会话）。
    pub fn take_pending(&mut self) -> Vec<Value> {
        std::mem::take(&mut self.pending)
    }

    pub fn remove(&mut self, client_id: &str) {
        self.pending.retain(|q| q["clientId"].as_str() != Some(client_id));
        self.edit_locks.retain(|l| l != client_id);
    }

    /// 编辑锁：加锁只对仍在队列里的消息生效；解锁总是放行。
    pub fn set_edit_lock(&mut self, client_id: &str, locked: bool) {
        self.edit_locks.retain(|l| l != client_id);
        if locked && self.pending.iter().any(|q| q["clientId"].as_str() == Some(client_id)) {
            self.edit_locks.push(client_id.to_string());
        }
    }

    /// 用编辑后的内容替换排队中的同 clientId 消息（保持位置）。已派发或不存在则不动，由调用方对照 projection 判断。
    pub fn update_content(&mut self, client_id: &str, item: Value) -> Result<(), String> {
        if item["clientId"].as_str() != Some(client_id) || !item["text"].is_string() {
            return Err("replacement requires the same clientId and text".into());
        }
        if let Some(slot) = self.pending.iter_mut().find(|q| q["clientId"].as_str() == Some(client_id)) {
            *slot = item;
        }
        Ok(())
    }

    /// 会话关闭：回合与队列一并清零。
    pub fn on_closed(&mut self) -> bool {
        let had = self.active.is_some();
        self.active = None;
        self.pending.clear();
        had
    }

    /// 状态变为 closed 时 maker-ipc 只清回合，不清队列（关闭由 close-session 通道另行清队）。
    pub fn on_status_closed(&mut self) -> bool {
        let had = self.active.is_some();
        self.active = None;
        had
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(id: &str) -> Value {
        json!({ "clientId": id, "text": format!("msg {id}") })
    }

    fn done(token: i64) -> Value {
        json!({ "type": "done", "turnAttemptToken": token })
    }

    #[test]
    fn dispatches_one_at_a_time_in_order_with_increasing_tokens() {
        let mut q = InputQueue::new();
        q.enqueue(item("a")).unwrap();
        q.enqueue(item("b")).unwrap();
        let d1 = q.next().unwrap();
        assert_eq!((d1.item["clientId"].as_str().unwrap(), d1.token), ("a", 1));
        assert!(q.next().is_none(), "回合进行中不派发");
        assert!(q.projection("s")["turnInProgress"].as_bool().unwrap());
        assert!(q.on_event(&done(1)));
        let d2 = q.next().unwrap();
        assert_eq!((d2.item["clientId"].as_str().unwrap(), d2.token), ("b", 2));
    }

    #[test]
    fn duplicate_client_id_is_not_enqueued_again() {
        let mut q = InputQueue::new();
        q.enqueue(item("a")).unwrap();
        q.enqueue(item("a")).unwrap();
        assert_eq!(q.projection("s")["pendingQueue"].as_array().unwrap().len(), 1);
        q.next().unwrap();
        q.enqueue(item("a")).unwrap();
        assert!(q.projection("s")["pendingQueue"].as_array().unwrap().is_empty(), "正在跑的 clientId 重试也不入队");
        assert!(q.enqueue(json!({ "clientId": "x" })).is_err());
        assert!(q.enqueue(json!({ "text": "x" })).is_err());
    }

    #[test]
    fn only_current_token_foreground_terminal_events_settle_the_turn() {
        let mut q = InputQueue::new();
        q.enqueue(item("a")).unwrap();
        q.next().unwrap();
        assert!(!q.on_event(&done(99)), "别的 token");
        assert!(!q.on_event(&json!({ "type": "done", "turnAttemptToken": 1, "turnScope": "background" })), "后台回合");
        assert!(!q.on_event(&json!({ "type": "text", "turnAttemptToken": 1 })), "非终态");
        assert!(!q.on_event(&json!({ "type": "status", "turnAttemptToken": 1, "data": { "isRunning": true } })));
        assert!(q.turn_in_progress());
        assert!(q.on_event(&json!({ "type": "status", "turnAttemptToken": 1, "data": { "isRunning": false } })));
        assert!(!q.turn_in_progress());
        assert!(!q.on_event(&done(1)), "已收尾，不再响应");
    }

    #[test]
    fn terminal_error_records_message_but_retrying_error_does_not_settle() {
        let mut q = InputQueue::new();
        q.enqueue(item("a")).unwrap();
        q.next().unwrap();
        let retry = json!({ "type": "error", "turnAttemptToken": 1, "data": { "message": "boom", "willRetry": true } });
        assert!(!q.on_event(&retry));
        let fatal = json!({ "type": "error", "turnAttemptToken": 1, "data": { "message": "boom", "willRetry": false } });
        assert!(q.on_event(&fatal));
        assert_eq!(q.projection("s")["error"], "boom");
        q.enqueue(item("b")).unwrap();
        q.next().unwrap();
        assert!(q.projection("s")["error"].is_null(), "新回合清掉旧错误");
        assert!(is_terminal_turn_event(&json!({ "type": "error", "data": "x" })));
        assert!(is_terminal_turn_event(&json!({ "type": "error", "data": { "isTerminal": true, "willRetry": true } })));
    }

    #[test]
    fn dispatch_failure_only_clears_its_own_turn() {
        let mut q = InputQueue::new();
        q.enqueue(item("a")).unwrap();
        let d = q.next().unwrap();
        assert!(!q.dispatch_failed(d.token + 1, "stale"));
        assert!(q.turn_in_progress());
        assert!(q.dispatch_failed(d.token, "send not accepted: busy"));
        assert_eq!(q.projection("s")["error"], "send not accepted: busy");
        assert!(!q.turn_in_progress());
    }

    #[test]
    fn stop_pause_resume_and_remove() {
        let mut q = InputQueue::new();
        q.enqueue(item("a")).unwrap();
        q.enqueue(item("b")).unwrap();
        q.enqueue(item("c")).unwrap();
        q.remove("b");
        assert_eq!(q.projection("s")["pendingQueue"].as_array().unwrap().len(), 2);
        q.stop(true, true);
        assert!(q.projection("s")["queuePaused"].as_bool().unwrap());
        assert!(q.next().is_none(), "暂停不派发");
        q.resume();
        assert_eq!(q.next().unwrap().item["clientId"], "a");
        q.stop(false, false);
        assert!(q.projection("s")["pendingQueue"].as_array().unwrap().is_empty(), "默认清空队列");
        q.clear_active();
        assert!(!q.turn_in_progress());
    }

    #[test]
    fn closing_resets_turn_and_reports_whether_one_was_running() {
        let mut q = InputQueue::new();
        q.enqueue(item("a")).unwrap();
        q.enqueue(item("b")).unwrap();
        q.next().unwrap();
        assert!(q.on_status_closed());
        assert_eq!(q.projection("s")["pendingQueue"].as_array().unwrap().len(), 1, "status closed 只清回合");
        assert!(!q.on_closed());
        assert!(q.projection("s")["pendingQueue"].as_array().unwrap().is_empty());
    }

    #[test]
    fn edit_lock_holds_the_head_and_unlock_releases_it() {
        let mut q = InputQueue::new();
        q.enqueue(item("a")).unwrap();
        q.enqueue(item("b")).unwrap();
        q.set_edit_lock("a", true);
        assert_eq!(q.projection("s")["queueEditLocks"], json!(["a"]));
        assert!(q.next().is_none(), "队首被锁时不派发，也不能越过它发后面的");
        q.set_edit_lock("a", false);
        assert_eq!(q.next().unwrap().item["clientId"], "a");
        q.set_edit_lock("zzz", true);
        assert_eq!(q.projection("s")["queueEditLocks"], json!([]), "不在队列里的消息不能加锁");
    }

    #[test]
    fn update_content_replaces_in_place_and_ignores_dispatched_items() {
        let mut q = InputQueue::new();
        q.enqueue(item("a")).unwrap();
        q.enqueue(item("b")).unwrap();
        q.update_content("b", json!({ "clientId": "b", "text": "edited" })).unwrap();
        let p = q.projection("s");
        assert_eq!(p["pendingQueue"][1]["text"], "edited");
        assert_eq!(p["pendingQueue"][0]["text"], "msg a");
        assert!(q.update_content("b", json!({ "clientId": "other", "text": "x" })).is_err());
        let d = q.next().unwrap();
        q.update_content("a", json!({ "clientId": "a", "text": "late" })).unwrap();
        assert_eq!(d.item["text"], "msg a");
        assert_eq!(q.projection("s")["pendingQueue"].as_array().unwrap().len(), 1);
    }
}
