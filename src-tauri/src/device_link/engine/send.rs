//! 发送侧：写 socket、路由代次账本、可靠传输队列、重发与恢复预算。
use super::peer::{short, Pending};
use super::types::*;
use super::{Engine, TimerKind};
use crate::device_link::transport::*;
use serde_json::{json, Value};

const SOCKET_DATA_HIGH_WATER_BYTES: usize = 512 * 1024;
const SOCKET_DATA_LOW_WATER_BYTES: usize = 128 * 1024;
const SOCKET_CONTROL_RESERVE_BYTES: usize = 64 * 1024;
const SOCKET_SEND_CREDIT_BYTES: usize = 64 * 1024;
const MAX_WEBSOCKET_BUFFERED_BYTES: usize = 8 * 1024 * 1024;
pub(crate) const MAX_FRAME_BYTES: usize = 2 * 1024 * 1024;
const RELIABLE_RETRY_BYTES_PER_INTERVAL: usize = 16 * 1024;
const RELIABLE_RETRY_MAX_SIZE_INTERVALS: usize = 60;
const MAX_REASSEMBLIES: u64 = 16;

pub(crate) fn is_reliable_kind(kind: &str) -> bool {
    matches!(kind, "invoke" | "invoke-result" | "push")
}

pub(crate) fn is_coalescible_push(env: &Env) -> bool {
    env.kind == "push" && matches!(env.channel(), Some("maker:event") | Some("maker:event:batch"))
}

const UNLINKED_LEGACY_INVOKE: &[&str] = &[
    "device-link:subscribe", "device-link:unsubscribe", "device-link:voice:dictionary:get", "maker:provider:list",
    "maker:get-capabilities", "maker:get-new-maker-defaults", "maker:list-active", "maker:list-available-agents",
    "maker:list-agent-commands", "maker:list-agent-skills", "local-db:sessions:list", "local-db:sessions:get",
    "local-db:history:messages", "local-db:messages:list", "local-db:messages:view", "local-db:messages:work-details",
    "local-db:messages:view-intent", "local-db:messages:around", "local-db:messages:around-client-id",
    "local-db:messages:estimatedSessionValue", "local-db:recent-workdirs:list", "local-db:sessions:interrupted-pending",
    "maker:git-safety:get",
];
const UNLINKED_LEGACY_PUSH: &[&str] = &["device-link:remote-control-consent", "device-link:plugin-sync:v1", "device-link:voice:dictionary:snapshot"];

pub(crate) fn is_unlinked_legacy(env: &Env) -> bool {
    match (env.kind.as_str(), env.channel()) {
        ("invoke", Some(c)) => UNLINKED_LEGACY_INVOKE.contains(&c),
        ("push", Some(c)) => UNLINKED_LEGACY_PUSH.contains(&c),
        _ => false,
    }
}

pub(crate) fn frame_envs(env: &Env, frames: Vec<Frame>) -> Vec<Env> {
    frames.into_iter().map(|f| Env { payload: Some(f.payload()), ..env.clone() }).collect()
}

pub(crate) fn measure_pub(frames: &[Env]) -> usize {
    frames.iter().map(Env::wire_bytes).sum()
}

pub(crate) fn encode_env_pub(env: &Env, stream_id: &str, seq: u64, base: u64) -> Result<Vec<Env>, String> {
    encode_reliable_frames(env.payload.as_ref(), stream_id, seq, base).map(|f| frame_envs(env, f))
}

impl Engine {
    pub(crate) fn peer_link_generation(&self, dst: &str) -> u64 {
        self.peers.get(dst).map_or(0, |p| p.link_generation)
    }

    /// 运行时回报 socket 发送缓冲（已交给写任务但尚未写完的字节数）。
    pub fn set_buffered(&mut self, bytes: usize) {
        self.ws_buffered = bytes;
    }

    pub(crate) fn assert_ws_capacity(&self, additional: usize) -> Result<(), LinkError> {
        if !self.ws_open {
            return Err(LinkError::new("NOT_CONNECTED", "no active connection"));
        }
        if self.ws_buffered + additional > MAX_WEBSOCKET_BUFFERED_BYTES {
            return Err(LinkError::new("BACKPRESSURE", "websocket send buffer is full"));
        }
        Ok(())
    }

    /// 写一个信封到当前 socket。
    pub(crate) fn send_envelope(&mut self, env: &Env) -> Result<(), LinkError> {
        if !self.ws_open {
            return Err(LinkError::new("NOT_CONNECTED", "no active connection"));
        }
        let text = env.to_text();
        if text.len() > MAX_FRAME_BYTES {
            return Err(LinkError::new("PAYLOAD_TOO_LARGE", format!("frame exceeds {MAX_FRAME_BYTES} bytes")));
        }
        self.assert_ws_capacity(text.len())?;
        self.ws_buffered += text.len();
        self.push(Out::Write(text));
        Ok(())
    }

    /// 带路由代次登记的发送。返回帧 id。
    pub(crate) fn send_routed(&mut self, env: &Env, link_generation: Option<u64>) -> Result<Option<String>, LinkError> {
        let Some(dst) = env.dst.clone() else {
            self.send_envelope(env)?;
            return Ok(env.id.clone());
        };
        let mut routed = env.clone();
        if routed.id.is_none() {
            routed.id = Some(self.new_id());
        }
        let id = routed.id.clone().unwrap();
        let generation = link_generation.unwrap_or_else(|| self.peer_link_generation(&dst));
        self.ledger.remember(&id, &dst, generation).map_err(|m| LinkError::new("BACKPRESSURE", m))?;
        if let Err(e) = self.send_envelope(&routed) {
            self.ledger.rollback(&id, &dst, generation);
            return Err(e);
        }
        Ok(Some(id))
    }

    pub(crate) fn send_best_effort(&mut self, env: &Env) -> Result<(), LinkError> {
        let id = self.send_routed(env, None)?;
        if let (Some(dst), Some(id)) = (&env.dst, id) {
            self.ledger.settle(dst, &id);
        }
        Ok(())
    }

    pub(crate) fn can_write_reliable_bytes(&mut self, bytes: usize) -> bool {
        let buffered = self.ws_buffered;
        let pressured = buffered > SOCKET_DATA_LOW_WATER_BYTES && (self.local_send_pressure || buffered + bytes > SOCKET_DATA_HIGH_WATER_BYTES);
        if pressured != self.local_send_pressure {
            self.local_send_pressure = pressured;
            if !pressured {
                self.local_budget.reset();
            }
            // 重发节奏随压力变化：重新排期所有 peer 的重发定时器
            let ids: Vec<String> = self.peers.iter().filter(|(id, _)| self.armed(TimerKind::Retry, id, "")).map(|(id, _)| id.clone()).collect();
            for id in ids {
                self.disarm(TimerKind::Retry, &id, "");
                self.ensure_retry_timer(&id);
            }
        }
        buffered + bytes <= SOCKET_DATA_HIGH_WATER_BYTES
            || (buffered <= SOCKET_DATA_LOW_WATER_BYTES && buffered + bytes <= MAX_WEBSOCKET_BUFFERED_BYTES - SOCKET_CONTROL_RESERVE_BYTES)
    }

    pub(crate) fn is_outside_receive_window(&self, dst: &str, seq: u64) -> bool {
        let base = self.peers.get(dst).map_or(1, |p| p.base_seq());
        seq as i64 - base as i64 >= MAX_REASSEMBLIES as i64
    }

    pub(crate) fn ready_reliable_peers(&self, target: &str) -> Vec<String> {
        self.peers.iter().filter(|(id, p)| p.reliable && p.send_ready() && (id.as_str() == target || !p.pending.is_empty())).map(|(id, _)| id.clone()).collect()
    }

    // ── 恢复预算 ──
    pub(crate) fn recovery_pass_budget(&self) -> usize {
        if self.timing.transport_retry_pass_budget >= 1 { self.timing.transport_retry_pass_budget } else { RETRY_PASS_BUDGET }
    }

    pub(crate) fn should_hold_recovery(&self, dst: &str, additional: usize) -> bool {
        let Some(p) = self.peers.get(dst) else { return false };
        if !p.recovery_needs_ack || !p.pending.values().any(|x| x.sent) {
            return false;
        }
        let remaining = self.recovery_pass_budget().saturating_sub(p.recovery_frames_sent);
        remaining == 0 || additional > remaining
    }

    pub(crate) fn note_recovery_frames(&mut self, dst: &str, frames: usize) {
        if let Some(p) = self.peers.get_mut(dst) {
            if p.recovery_needs_ack && frames > 0 {
                p.recovery_frames_sent += frames;
            }
        }
    }

    pub(crate) fn estimate_frame_count(bytes: usize) -> usize {
        bytes.div_ceil(MAX_CHUNK_BYTES).max(1)
    }

    // ── 重发定时器 ──
    pub(crate) fn ensure_retry_timer(&mut self, dst: &str) {
        if self.armed(TimerKind::Retry, dst, "") {
            return;
        }
        let interval = self.timing.transport_retry_interval_ms;
        let t = if self.congestion_streak > 0 || self.local_send_pressure { interval.min(250) } else { interval };
        self.arm(TimerKind::Retry, dst, "", t);
    }

    pub(crate) fn on_retry_timer(&mut self, dst: &str) {
        if !self.peers.contains_key(dst) {
            return;
        }
        self.ensure_retry_timer(dst);
        self.retry_pending(dst, false, false);
    }

    // ── 待确认队列的维护 ──
    pub(crate) fn remove_pending_entry(&mut self, dst: &str, seq: u64) {
        let Some(p) = self.peers.get_mut(dst) else { return };
        let Some(pending) = p.pending.remove(&seq) else { return };
        p.pending_bytes = p.pending_bytes.saturating_sub(pending.bytes);
        let empty = p.pending.is_empty();
        if let Some(id) = &pending.env.id {
            self.ledger.settle(dst, id);
        }
        if empty {
            self.disarm(TimerKind::Retry, dst, "");
        }
    }

    /// 丢掉队头连续的可丢弃前缀（push 与 skip 占位）。
    pub(crate) fn drop_discardable_prefix(&mut self, dst: &str, expired_only: bool, reason: &str) -> usize {
        let now = self.now_ms;
        let Some(p) = self.peers.get(dst) else { return 0 };
        let mut drop_seqs = Vec::new();
        for (seq, x) in &p.pending {
            let discardable = x.env.kind == "push" || x.env.payload.as_ref().is_some_and(is_skip_payload);
            if !discardable {
                break;
            }
            if expired_only && x.env.kind == "push" && now.saturating_sub(x.enqueued_at) < PENDING_PUSH_MAX_AGE_MS {
                break;
            }
            drop_seqs.push(*seq);
        }
        let n = drop_seqs.len();
        for s in drop_seqs {
            self.remove_pending_entry(dst, s);
        }
        if n > 0 {
            self.log("warn", format!("dropped {n} discardable pending frame(s) (best-effort push / transport-skip) for peer {} {reason}", short(dst)));
        }
        n
    }

    /// latest-wins 腾位：只丢队头的可驱逐 push / skip，直到放得下。
    pub(crate) fn drop_oldest_discardable_for_push(&mut self, dst: &str, reserved: usize) -> usize {
        let mut dropped = 0;
        loop {
            let Some(p) = self.peers.get(dst) else { break };
            if !(p.pending.len() >= MAX_PENDING_MESSAGES || p.pending_bytes + reserved > MAX_PENDING_BYTES) {
                break;
            }
            let Some((seq, head)) = p.pending.iter().next() else { break };
            if !head.env.payload.as_ref().is_some_and(is_skip_payload) && !is_coalescible_push(&head.env) {
                break;
            }
            let seq = *seq;
            self.remove_pending_entry(dst, seq);
            dropped += 1;
        }
        if dropped > 0 {
            if let Some(p) = self.peers.get_mut(dst) {
                p.push_admission_drop_count += dropped as u64;
            }
            self.log("warn", format!("dropped {dropped} oldest discardable pending frame(s) for peer {} latest-wins push admission", short(dst)));
        }
        dropped
    }

    pub(crate) fn abandon_reliable_pending(&mut self, dst: &str, message: &str) {
        let _ = message;
        self.disarm(TimerKind::Retry, dst, "");
        let ids: Vec<String> = self.peers.get(dst).map(|p| p.pending.values().filter_map(|x| x.env.id.clone()).collect()).unwrap_or_default();
        for id in ids {
            self.ledger.settle(dst, &id);
        }
        if let Some(p) = self.peers.get_mut(dst) {
            p.pending.clear();
            p.pending_bytes = 0;
        }
    }

    /// 原请求没有送达（relay 回了永久性帧错误）：把对应 seq 换成 skip 占位，避免稍后意外重放。
    pub(crate) fn drop_reliable_pending_for_request(&mut self, dst: &str, request_id: &str) {
        let Some(p) = self.peers.get(dst) else { return };
        let Some(seq) = p.pending.iter().find(|(_, x)| x.env.id.as_deref() == Some(request_id)).map(|(s, _)| *s) else { return };
        self.replace_pending_with_skip(dst, seq);
        self.send_skip_placeholder_now(dst, seq);
    }

    fn replace_pending_with_skip(&mut self, dst: &str, seq: u64) {
        let Some(p) = self.peers.get(dst) else { return };
        let Some(pending) = p.pending.get(&seq) else { return };
        let skip_env = Env { payload: Some(skip_payload()), ..pending.env.clone() };
        let skip_bytes = encode_env_pub(&skip_env, &p.stream_id, seq, seq).map(|f| measure_pub(&f)).unwrap_or(pending.bytes);
        let p = self.peers.get_mut(dst).unwrap();
        let pending = p.pending.get_mut(&seq).unwrap();
        p.pending_bytes = p.pending_bytes + skip_bytes - pending.bytes;
        pending.env = skip_env;
        pending.bytes = skip_bytes;
        pending.reset_send_round();
        pending.sent = false;
    }

    fn send_skip_placeholder_now(&mut self, dst: &str, seq: u64) {
        let ok = self.peers.get(dst).is_some_and(|p| p.reliable && p.send_ready()) && !self.stopped && self.status == Status::Online;
        if !ok {
            return;
        }
        if let Err(e) = self.send_reliable_frames(dst, seq) {
            self.log("debug", format!("reliable transport skip placeholder send failed for {}: {}", short(dst), e.message));
        }
    }
}
