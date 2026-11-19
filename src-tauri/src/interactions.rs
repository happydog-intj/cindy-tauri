//! 待处理交互表（审批 / 提问 / 计划）。
//!
//! 对应 sidecar/src/maker-ipc.mjs 的 `pending` Map、`settleInteraction`、
//! `cancelInteractionsForSession` 与 `defaultDecision`，语义逐条保持：
//! - permission 带 10 分钟超时，ask_user_question / plan_review 不超时；
//! - resolve 时 decision.kind 必须与登记的 kind 一致，否则 accepted=false；
//! - 取消 / 超时 / 中止都以 `default_decision` 收尾，并发出 `maker:interaction-dismissed`；
//! - 只有 reason=="resolved" 的提问和计划才把 decision 带进 dismissed 事件。
//!
//! 纯状态机：时间由调用方传入，不起线程、不碰 IO，宿主在 pi-rs 的
//! `extension_ui_request` 与 Tauri 事件之间接线即可。

#![allow(dead_code)]

use serde_json::{json, Value};
use std::collections::HashMap;
use std::time::{Duration, Instant};

pub const PERMISSION_TIMEOUT: Duration = Duration::from_secs(10 * 60);

struct Entry {
    session_id: String,
    kind: String,
    request: Value,
    persist_id: Option<String>,
    deadline: Option<Instant>,
}

/// 一次收尾的结果：`decision` 回给 pi-rs，`dismissed` 作为 `maker:interaction-dismissed` 推给 renderer。
#[derive(Debug, Clone, PartialEq)]
pub struct Settled {
    pub session_id: String,
    pub request_id: String,
    pub decision: Value,
    pub dismissed: Value,
}

#[derive(Default)]
pub struct InteractionTable {
    pending: HashMap<String, Entry>,
}

pub fn default_decision(kind: &str, reason: &str) -> Value {
    match kind {
        "ask_user_question" => json!({ "kind": kind, "answers": {}, "dismissed": true }),
        "plan_review" => json!({ "kind": kind, "behavior": "deny", "reason": reason, "dismissed": true }),
        _ => json!({ "kind": "permission", "behavior": "deny", "reason": reason }),
    }
}

impl InteractionTable {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn len(&self) -> usize {
        self.pending.len()
    }

    /// 登记一条请求（要先登记再广播，避免 renderer 回得太快）。request 必须带 requestId 与 kind。
    pub fn register(&mut self, session_id: &str, request: Value, persist_id: Option<String>, now: Instant) -> Result<(), String> {
        let request_id = request["requestId"].as_str().ok_or("requestId required")?.to_string();
        let kind = request["kind"].as_str().ok_or("kind required")?.to_string();
        let deadline = (kind == "permission").then(|| now + PERMISSION_TIMEOUT);
        self.pending.insert(request_id, Entry { session_id: session_id.to_string(), kind, request, persist_id, deadline });
        Ok(())
    }

    /// renderer 的决策。kind 不匹配或请求不存在时返回 None（对应 `{accepted:false}`）。
    pub fn resolve(&mut self, request_id: &str, decision: Value) -> Option<Settled> {
        let entry = self.pending.get(request_id)?;
        if decision["kind"].as_str() != Some(entry.kind.as_str()) {
            return None;
        }
        self.settle(request_id, decision, "resolved")
    }

    /// 中止 / 关闭会话：该会话的全部待处理项以默认决策收尾。
    pub fn cancel_session(&mut self, session_id: &str, reason: &str) -> Vec<Settled> {
        let ids: Vec<String> = self.pending.iter().filter(|(_, e)| e.session_id == session_id).map(|(id, _)| id.clone()).collect();
        self.settle_all(ids, reason)
    }

    /// 单条被 pi-rs 取消（对应 abort signal）。
    pub fn abort_request(&mut self, request_id: &str) -> Option<Settled> {
        let kind = self.pending.get(request_id)?.kind.clone();
        self.settle(request_id, default_decision(&kind, "session_aborted"), "session_aborted")
    }

    /// 到期的 permission 以 timeout 收尾。宿主定时调用即可。
    pub fn expire(&mut self, now: Instant) -> Vec<Settled> {
        let ids: Vec<String> = self.pending.iter().filter(|(_, e)| e.deadline.is_some_and(|d| now >= d)).map(|(id, _)| id.clone()).collect();
        self.settle_all(ids, "timeout")
    }

    /// `maker:get-pending-interactions`：重连时重建审批面板。
    pub fn snapshot(&self, session_id: &str) -> Vec<Value> {
        let mut rows: Vec<&Entry> = self.pending.values().filter(|e| e.session_id == session_id).collect();
        rows.sort_by(|a, b| a.request["requestId"].as_str().cmp(&b.request["requestId"].as_str()));
        rows.into_iter()
            .map(|e| match &e.persist_id {
                Some(pid) => json!({ "request": e.request, "persistId": pid }),
                None => json!({ "request": e.request }),
            })
            .collect()
    }

    pub fn persist_id(&self, request_id: &str) -> Option<&str> {
        self.pending.get(request_id)?.persist_id.as_deref()
    }

    fn settle_all(&mut self, ids: Vec<String>, reason: &str) -> Vec<Settled> {
        let mut out = Vec::new();
        for id in ids {
            let Some(kind) = self.pending.get(&id).map(|e| e.kind.clone()) else { continue };
            if let Some(s) = self.settle(&id, default_decision(&kind, reason), reason) {
                out.push(s);
            }
        }
        out.sort_by(|a, b| a.request_id.cmp(&b.request_id));
        out
    }

    fn settle(&mut self, request_id: &str, decision: Value, reason: &str) -> Option<Settled> {
        let entry = self.pending.remove(request_id)?;
        let denied = decision["behavior"] == "deny" || decision["dismissed"].as_bool().unwrap_or(false);
        let carry = reason == "resolved" && (entry.kind == "ask_user_question" || entry.kind == "plan_review");
        let mut dismissed = json!({
            "sessionId": entry.session_id,
            "requestId": request_id,
            "reason": reason,
            "resolvedAs": if denied { "deny" } else { "allow" },
        });
        if carry {
            dismissed["decision"] = decision.clone();
        }
        Some(Settled { session_id: entry.session_id, request_id: request_id.to_string(), decision, dismissed })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn perm(id: &str) -> Value {
        json!({ "requestId": id, "kind": "permission", "tool": "write" })
    }
    fn ask(id: &str) -> Value {
        json!({ "requestId": id, "kind": "ask_user_question", "questions": [] })
    }

    #[test]
    fn resolve_requires_matching_kind_and_removes_entry() {
        let t0 = Instant::now();
        let mut t = InteractionTable::new();
        t.register("s1", perm("r1"), None, t0).unwrap();
        assert!(t.resolve("r1", json!({ "kind": "plan_review", "behavior": "allow" })).is_none());
        assert_eq!(t.len(), 1);
        let s = t.resolve("r1", json!({ "kind": "permission", "behavior": "allow" })).unwrap();
        assert_eq!(s.dismissed["resolvedAs"], "allow");
        assert_eq!(s.dismissed["reason"], "resolved");
        assert!(s.dismissed.get("decision").is_none(), "permission 不带 decision");
        assert!(t.resolve("r1", json!({ "kind": "permission", "behavior": "allow" })).is_none(), "不能重复收尾");
    }

    #[test]
    fn resolved_question_carries_decision_and_dismissal_reads_as_deny() {
        let t0 = Instant::now();
        let mut t = InteractionTable::new();
        t.register("s1", ask("q1"), Some("p-1".into()), t0).unwrap();
        let answered = t.resolve("q1", json!({ "kind": "ask_user_question", "answers": { "a": "b" } })).unwrap();
        assert_eq!(answered.dismissed["resolvedAs"], "allow");
        assert_eq!(answered.dismissed["decision"]["answers"]["a"], "b");

        t.register("s1", ask("q2"), None, t0).unwrap();
        let skipped = t.resolve("q2", json!({ "kind": "ask_user_question", "answers": {}, "dismissed": true })).unwrap();
        assert_eq!(skipped.dismissed["resolvedAs"], "deny");
    }

    #[test]
    fn cancel_session_uses_default_decisions_only_for_that_session() {
        let t0 = Instant::now();
        let mut t = InteractionTable::new();
        t.register("s1", perm("a"), None, t0).unwrap();
        t.register("s1", ask("b"), None, t0).unwrap();
        t.register("s2", perm("c"), None, t0).unwrap();
        let out = t.cancel_session("s1", "session_aborted");
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].decision, json!({ "kind": "permission", "behavior": "deny", "reason": "session_aborted" }));
        assert_eq!(out[1].decision, json!({ "kind": "ask_user_question", "answers": {}, "dismissed": true }));
        assert!(out.iter().all(|s| s.dismissed["resolvedAs"] == "deny" && s.dismissed.get("decision").is_none()));
        assert_eq!(t.len(), 1);
        assert_eq!(t.snapshot("s2").len(), 1);
    }

    #[test]
    fn only_permission_times_out() {
        let t0 = Instant::now();
        let mut t = InteractionTable::new();
        t.register("s1", perm("p"), None, t0).unwrap();
        t.register("s1", ask("q"), None, t0).unwrap();
        assert!(t.expire(t0 + PERMISSION_TIMEOUT - Duration::from_secs(1)).is_empty());
        let out = t.expire(t0 + PERMISSION_TIMEOUT);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].request_id, "p");
        assert_eq!(out[0].dismissed["reason"], "timeout");
        assert_eq!(t.len(), 1, "提问不超时");
    }

    #[test]
    fn snapshot_includes_persist_id_when_present() {
        let t0 = Instant::now();
        let mut t = InteractionTable::new();
        t.register("s1", ask("q"), Some("p-9".into()), t0).unwrap();
        t.register("s1", perm("r"), None, t0).unwrap();
        let snap = t.snapshot("s1");
        assert_eq!(snap.len(), 2);
        assert_eq!(snap[0]["request"]["requestId"], "q");
        assert_eq!(snap[0]["persistId"], "p-9");
        assert!(snap[1].get("persistId").is_none());
        assert!(t.snapshot("nobody").is_empty());
        assert!(t.register("s1", json!({ "kind": "permission" }), None, t0).is_err());
    }

    #[test]
    fn abort_request_closes_a_single_entry() {
        let t0 = Instant::now();
        let mut t = InteractionTable::new();
        t.register("s1", perm("a"), None, t0).unwrap();
        t.register("s1", perm("b"), None, t0).unwrap();
        let s = t.abort_request("a").unwrap();
        assert_eq!(s.dismissed["reason"], "session_aborted");
        assert_eq!(t.len(), 1);
        assert!(t.abort_request("a").is_none());
    }
}
