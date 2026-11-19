//! 可靠传输发送：入队、写帧、重发、重放、重试耗尽后的止损。
use super::peer::{short, Pending, Phase};
use super::send::{encode_env_pub as encode_env, is_coalescible_push, is_reliable_kind, is_unlinked_legacy, measure_pub as measure};
use super::types::*;
use super::{Engine, TimerKind};
use crate::device_link::transport::*;

const RELIABLE_RETRY_BYTES_PER_INTERVAL: usize = 16 * 1024;
const RELIABLE_RETRY_MAX_SIZE_INTERVALS: usize = 60;
const SOCKET_SEND_CREDIT_BYTES: usize = 64 * 1024;

impl Engine {
    /// 对外发送一个业务信封（invoke-result / push / link-accept 以外的可靠三类走这里）。
    /// 返回 true = 进入了可靠队列。
    pub(crate) fn send_peer_envelope(&mut self, env: &Env, allow_closed_legacy_response: bool) -> Result<bool, LinkError> {
        let Some(dst) = env.dst.clone() else {
            self.send_best_effort(env)?;
            return Ok(false);
        };
        if !is_reliable_kind(&env.kind) {
            self.send_best_effort(env)?;
            return Ok(false);
        }
        let mut routed = env.clone();
        if routed.id.is_none() {
            routed.id = Some(self.new_id());
        }
        {
            let p = self.peer(&dst);
            if p.explicitly_closed && !p.send_ready() && !is_unlinked_legacy(env) && !allow_closed_legacy_response {
                return Err(LinkError::new("LINK_NOT_OPEN", "control link is closed"));
            }
            if !p.reliable {
                // 旧控制端：裸帧即时发送
                let id = routed.id.clone().unwrap();
                self.send_routed(&routed, None)?;
                self.ledger.settle(&dst, &id);
                return Ok(false);
            }
        }
        // 链路长期未就绪：放弃陈旧积压
        let (not_ready, oldest) = {
            let p = self.peers.get(&dst).unwrap();
            (!p.send_ready() && !p.pending.is_empty(), p.pending.values().next().map(|x| x.enqueued_at))
        };
        if not_ready {
            if let Some(oldest) = oldest {
                if self.now_ms.saturating_sub(oldest) > self.timing.stalled_link_pending_max_age_ms {
                    let n = self.peers.get(&dst).unwrap().pending.len();
                    self.log("warn", format!("abandoning {n} stalled reliable frame(s) for peer {} (link down > {}ms)", short(&dst), self.timing.stalled_link_pending_max_age_ms));
                    self.abandon_reliable_pending(&dst, "reliable link stalled; pending abandoned");
                }
            }
        }
        let (seq, stream_id, base) = {
            let p = self.peers.get(&dst).unwrap();
            (p.next_seq, p.stream_id.clone(), p.base_seq())
        };
        let frames = encode_env(&routed, &stream_id, seq, base).map_err(|e| LinkError::new("PAYLOAD_TOO_LARGE", e))?;
        let reserved = encode_env(&routed, &stream_id, seq, seq).map(|f| measure(&f)).map_err(|e| LinkError::new("PAYLOAD_TOO_LARGE", e))?;
        let has_capacity = |e: &Engine| {
            let p = e.peers.get(&dst).unwrap();
            p.pending.len() < MAX_PENDING_MESSAGES && p.pending_bytes + reserved <= MAX_PENDING_BYTES
        };
        let additional = frames.len().max(1);
        let previous_base = base;
        let wire = measure(&frames);
        let congestion_ok = {
            if self.congestion_streak == 0 {
                true
            } else {
                let peers = self.ready_reliable_peers(&dst);
                let now = self.now_ms as f64;
                self.congestion_budget.can_take(&dst, additional as i64, &peers, now)
            }
        };
        let will_send_now = {
            let ready = self.peers.get(&dst).unwrap().send_ready();
            ready && (!self.is_outside_receive_window(&dst, seq) || !has_capacity(self)) && !self.should_hold_recovery(&dst, additional) && congestion_ok
        };
        if will_send_now {
            self.assert_ws_capacity(wire)?;
        }
        if !has_capacity(self) {
            if routed.kind == "invoke-result" {
                self.drop_discardable_prefix(&dst, false, "to make room for invoke-result");
            } else if routed.kind == "push" && is_coalescible_push(&routed) {
                self.drop_oldest_discardable_for_push(&dst, reserved);
            } else {
                self.drop_discardable_prefix(&dst, true, "after pending push TTL expiry");
            }
        }
        if !has_capacity(self) {
            self.log("warn", format!("reliable send backlog full peer={}", short(&dst)));
            if routed.kind == "invoke-result" && self.try_send_result_past_stalled_window(&routed, &dst) {
                return Ok(false);
            }
            return Err(LinkError::new("BACKPRESSURE", format!("reliable transport buffer is full for peer {}", short(&dst))));
        }
        {
            let now = self.now_ms;
            let p = self.peers.get_mut(&dst).unwrap();
            p.pending.insert(seq, Pending { seq, env: routed.clone(), bytes: reserved, attempts: 0, last_sent_at: 0, sent: false, enqueued_at: now, first_sent_at: None });
            p.pending_bytes += reserved;
            p.next_seq = seq + 1;
        }
        if will_send_now {
            match self.send_reliable_frames(&dst, seq) {
                Ok(n) => self.note_recovery_frames(&dst, n),
                Err(e) => {
                    let sent = self.peers.get(&dst).and_then(|p| p.pending.get(&seq)).is_some_and(|x| x.sent);
                    if !sent {
                        let p = self.peers.get_mut(&dst).unwrap();
                        if let Some(x) = p.pending.remove(&seq) {
                            p.pending_bytes -= x.bytes;
                        }
                        if p.next_seq == seq + 1 {
                            p.next_seq = seq;
                        }
                        return Err(e);
                    }
                    self.log("debug", format!("reliable transport initial send interrupted for {}: {}", short(&dst), e.message));
                }
            }
        }
        if self.peers.get(&dst).is_some_and(|p| p.send_ready()) {
            if self.peers.get(&dst).unwrap().base_seq() != previous_base {
                self.retry_pending(&dst, false, true);
            }
            self.ensure_retry_timer(&dst);
        }
        Ok(true)
    }

    /// 窗口被队头卡住时，invoke-result 越过它直接发一份（受预算约束）。
    fn try_send_result_past_stalled_window(&mut self, env: &Env, dst: &str) -> bool {
        let Some(p) = self.peers.get(dst) else { return false };
        let head = p.pending.values().next();
        if self.status != Status::Online || !p.send_ready() {
            return false;
        }
        let Some(head) = head else { return false };
        if !head.sent || head.first_sent_at.is_none() || head.attempts < 2 {
            return false;
        }
        let bytes = measure(std::slice::from_ref(env));
        if bytes > super::send::MAX_FRAME_BYTES || !self.can_write_reliable_bytes(bytes) {
            return false;
        }
        let peers = self.ready_reliable_peers(dst);
        let now = self.now_ms as f64;
        let credits = bytes.div_ceil(SOCKET_SEND_CREDIT_BYTES) as i64;
        let use_congestion = self.congestion_streak > 0;
        let use_local = self.local_send_pressure;
        if use_congestion && !self.congestion_budget.can_take(dst, 1, &peers, now) {
            return false;
        }
        if use_local && !self.local_budget.can_take(dst, credits, &peers, now) {
            return false;
        }
        if use_congestion && !self.congestion_budget.take(dst, 1, &peers, now) {
            return false;
        }
        if use_local && !self.local_budget.take(dst, credits, &peers, now) {
            if use_congestion {
                self.congestion_budget.refund(dst, 1);
            }
            return false;
        }
        let gen = self.peer_link_generation(dst);
        match self.send_routed(env, Some(gen)) {
            Ok(id) => {
                if let Some(id) = id {
                    self.ledger.settle(dst, &id);
                }
                true
            }
            Err(_) => {
                if use_congestion {
                    self.congestion_budget.refund(dst, 1);
                }
                if use_local {
                    self.local_budget.refund(dst, credits);
                }
                false
            }
        }
    }

    /// 写出一条 pending 的全部帧。返回实际写出的帧数（0 = 本地被拦下，未计一次尝试）。
    pub(crate) fn send_reliable_frames(&mut self, dst: &str, seq: u64) -> Result<usize, LinkError> {
        let (env, stream_id, base, sent_flag, gen) = {
            let Some(p) = self.peers.get(dst) else { return Ok(0) };
            let Some(x) = p.pending.get(&seq) else { return Ok(0) };
            (x.env.clone(), p.stream_id.clone(), p.base_seq(), x.sent, p.link_generation)
        };
        if !sent_flag && self.is_outside_receive_window(dst, seq) {
            return Ok(0);
        }
        let frames = encode_env(&env, &stream_id, seq, base).map_err(|e| LinkError::new("PAYLOAD_TOO_LARGE", e))?;
        let wire = measure(&frames);
        if self.status != Status::Online {
            return Ok(0);
        }
        let peers = self.ready_reliable_peers(dst);
        let now = self.now_ms as f64;
        let use_congestion = self.congestion_streak > 0;
        if use_congestion && !self.congestion_budget.can_take(dst, frames.len() as i64, &peers, now) {
            return Ok(0);
        }
        self.assert_ws_capacity(wire)?;
        if !self.can_write_reliable_bytes(wire) {
            return Ok(0);
        }
        let use_local = self.local_send_pressure;
        let credits = wire.div_ceil(SOCKET_SEND_CREDIT_BYTES) as i64;
        if use_local && !self.local_budget.can_take(dst, credits, &peers, now) {
            return Ok(0);
        }
        if use_congestion && !self.congestion_budget.take(dst, frames.len() as i64, &peers, now) {
            return Ok(0);
        }
        if use_local && !self.local_budget.take(dst, credits, &peers, now) {
            if use_congestion {
                self.congestion_budget.refund(dst, frames.len() as i64);
            }
            return Ok(0);
        }
        let mut sent = 0usize;
        let mut written = 0usize;
        let mut first_err = None;
        if let Err(e) = self.assert_ws_capacity(wire) {
            first_err = Some(e);
        } else {
            for frame in &frames {
                match self.send_routed(frame, Some(gen)) {
                    Ok(_) => {
                        written += frame.wire_bytes();
                        if let Some(x) = self.peers.get_mut(dst).and_then(|p| p.pending.get_mut(&seq)) {
                            x.sent = true;
                        }
                        sent += 1;
                    }
                    Err(e) => {
                        first_err = Some(e);
                        break;
                    }
                }
            }
        }
        if use_congestion {
            self.congestion_budget.refund(dst, (frames.len() - sent) as i64);
        }
        if use_local {
            self.local_budget.refund(dst, credits - written.div_ceil(SOCKET_SEND_CREDIT_BYTES) as i64);
        }
        self.note_reliable_write(dst, seq, sent);
        if let Some(e) = first_err {
            if sent == 0 {
                return Err(e);
            }
            self.log("debug", format!("reliable transport send interrupted after {sent} frame(s) for seq={seq}: {}", e.message));
        }
        Ok(sent)
    }

    fn note_reliable_write(&mut self, dst: &str, seq: u64, sent: usize) {
        if sent == 0 {
            return;
        }
        let (now, wall) = (self.now_ms, self.wall_ms);
        if let Some(x) = self.peers.get_mut(dst).and_then(|p| p.pending.get_mut(&seq)) {
            x.sent = true;
            if x.attempts == 0 {
                x.first_sent_at = Some(now);
            }
            x.attempts += 1;
            x.last_sent_at = wall;
        }
    }

    /// 一趟重发（旧→新，帧预算限制每趟写出量）。
    pub(crate) fn retry_pending(&mut self, dst: &str, ignore_interval: bool, only_unsent: bool) {
        let Some(p) = self.peers.get(dst) else { return };
        if !p.reliable || !p.send_ready() || self.stopped || self.status != Status::Online {
            return;
        }
        let now = self.wall_ms;
        let budget = self.recovery_pass_budget();
        let mut frames_spent = 0usize;
        let seqs: Vec<u64> = p.pending.keys().copied().collect();
        let head_seq = seqs.first().copied();
        let head_bytes = head_seq.and_then(|s| p.pending.get(&s)).map(|x| x.bytes).unwrap_or(0);
        for seq in seqs {
            let Some(pe) = self.peers.get(dst).and_then(|p| p.pending.get(&seq)).cloned() else { continue };
            if only_unsent && pe.sent {
                continue;
            }
            let is_head = Some(seq) == head_seq;
            if !ignore_interval && !is_head && head_bytes > RELIABLE_RETRY_BYTES_PER_INTERVAL && pe.attempts >= 2.min(self.timing.transport_max_retry_attempts) {
                continue;
            }
            let size_intervals = RELIABLE_RETRY_MAX_SIZE_INTERVALS.min(pe.bytes.div_ceil(RELIABLE_RETRY_BYTES_PER_INTERVAL));
            let rtt = self.peers.get(dst).unwrap().rtt.clone();
            let delay = reliable_retry_delay_ms(&rtt, self.timing.transport_retry_interval_ms as f64, size_intervals as f64, pe.attempts, pe.bytes as f64);
            if pe.sent && !ignore_interval && (now.saturating_sub(pe.last_sent_at) as f64) < delay {
                continue;
            }
            if pe.attempts >= self.timing.transport_max_retry_attempts {
                self.handle_reliable_retry_exhausted(dst, seq);
                return;
            }
            let admitting_new = !pe.sent;
            let est = Self::estimate_frame_count(pe.bytes);
            if admitting_new && !is_head && self.should_hold_recovery(dst, est) {
                break;
            }
            if frames_spent > 0 && frames_spent + est > budget {
                break;
            }
            let sent_frames = match self.send_reliable_frames(dst, seq) {
                Ok(n) => n,
                Err(e) => {
                    self.log("debug", format!("reliable transport retry failed for {}: {}", short(dst), e.message));
                    break;
                }
            };
            if sent_frames == 0 {
                break;
            }
            frames_spent += sent_frames;
            let needs_ack = self.peers.get(dst).is_some_and(|p| p.recovery_needs_ack);
            if admitting_new || (needs_ack && ignore_interval) {
                self.note_recovery_frames(dst, sent_frames);
            }
            if frames_spent >= budget {
                break;
            }
        }
    }

    /// 链路重建后重放全部 pending。
    pub(crate) fn replay_pending(&mut self, dst: &str, resumed_link: bool) {
        let ok = self.peers.get(dst).is_some_and(|p| p.reliable && p.send_ready() && !p.pending.is_empty());
        if !ok {
            return;
        }
        self.drop_discardable_prefix(dst, false, "before link re-establishment replay");
        if self.peers.get(dst).unwrap().pending.is_empty() {
            return;
        }
        let epoch = self.conn_epoch;
        let p = self.peers.get_mut(dst).unwrap();
        if resumed_link || p.last_replay_epoch != epoch {
            p.last_replay_epoch = epoch;
            for x in p.pending.values_mut() {
                x.reset_send_round();
            }
        }
        self.retry_pending(dst, true, false);
        self.ensure_retry_timer(dst);
    }

    // ── 重试耗尽 ──
    pub(crate) fn handle_reliable_retry_exhausted(&mut self, dst: &str, seq: u64) {
        if self.stopped || self.status != Status::Online {
            return;
        }
        let Some(p) = self.peers.get(dst) else {
            self.force_reconnect_for_timeout(dst, seq);
            return;
        };
        if !p.link_accepted_inbound || !p.supports_transport_timeout_close {
            self.force_reconnect_for_timeout(dst, seq);
            return;
        }
        if self.is_relay_wide_stall(dst) {
            self.log("warn", format!("reliable transport ACK timeout on multiple silent peers; relay connection looks stalled, forcing reconnect dst={} seq={seq}", short(dst)));
            self.drop_socket(1006, "reliable transport stalled on all peers");
            return;
        }
        self.log("warn", format!("reliable transport ACK timeout; resetting peer link (relay connection kept alive) dst={} seq={seq}", short(dst)));
        self.mark_peer_link_down(dst);
        self.disarm(TimerKind::Retry, dst, "");
        self.notify_transport_timeout_close(dst, 1);
    }

    fn force_reconnect_for_timeout(&mut self, dst: &str, seq: u64) {
        self.log("warn", format!("reliable transport ACK timeout; forcing reconnect dst={} seq={seq}", short(dst)));
        self.drop_socket(1006, "reliable transport retry exhausted");
    }

    fn is_relay_wide_stall(&self, dst: &str) -> bool {
        let silent = |head: Option<&Pending>| head.is_some_and(|h| h.sent && h.first_sent_at.is_some_and(|f| self.last_peer_inbound_ms <= f));
        let own = self.peers.get(dst).and_then(|p| p.pending.values().next());
        if !silent(own) {
            return false;
        }
        let min_attempts = 3.min(self.timing.transport_max_retry_attempts);
        let mut stalled_others = 0;
        for (id, other) in &self.peers {
            if id == dst {
                continue;
            }
            if !other.reliable {
                if other.link_accepted_inbound {
                    return false;
                }
                continue;
            }
            if !other.send_ready() {
                continue;
            }
            let head = other.pending.values().next();
            if !silent(head) || head.unwrap().attempts < min_attempts {
                return false;
            }
            stalled_others += 1;
        }
        stalled_others > 0
    }

    pub(crate) fn notify_transport_timeout_close(&mut self, dst: &str, attempt: u32) {
        let env = Env::new("link-close").to(dst).with(serde_json::json!({ "reason": "transport-timeout" }));
        match self.send_best_effort(&env) {
            Ok(()) => self.disarm_all(TimerKind::TimeoutCloseNotify, dst),
            Err(e) => {
                self.log("debug", format!("transport-timeout link-close notification failed for {} (attempt {attempt}): {}", short(dst), e.message));
                if attempt >= self.timing.transport_max_retry_attempts {
                    self.disarm_all(TimerKind::TimeoutCloseNotify, dst);
                    return;
                }
                self.disarm_all(TimerKind::TimeoutCloseNotify, dst);
                let t = self.timing.transport_retry_interval_ms;
                self.arm(TimerKind::TimeoutCloseNotify, dst, &(attempt + 1).to_string(), t);
            }
        }
    }

    pub(crate) fn on_timeout_close_retry(&mut self, dst: &str, attempt: &str) {
        let attempt: u32 = attempt.parse().unwrap_or(1);
        if self.stopped || self.status != Status::Online {
            return;
        }
        let Some(p) = self.peers.get(dst) else { return };
        if p.send_ready() || p.explicitly_closed || !p.link_accepted_inbound || !p.supports_transport_timeout_close {
            return;
        }
        self.notify_transport_timeout_close(dst, attempt);
    }

    /// 链路确认超时：按重试耗尽处理。
    pub(crate) fn on_link_confirm_timeout(&mut self, dst: &str) {
        let Some(p) = self.peers.get(dst) else { return };
        let Some(c) = &p.pending_link_confirmation else { return };
        if p.send_phase != Phase::AwaitingConfirm || self.stopped || self.status != Status::Online {
            return;
        }
        let min = c.minimum_ack_seq;
        self.log("warn", format!("device-link link confirmation timeout for {} request={}; resetting peer link", short(dst), short(&c.request_id.clone())));
        self.handle_reliable_retry_exhausted(dst, min);
    }

    pub(crate) fn mark_peer_link_down(&mut self, dst: &str) {
        self.disarm(TimerKind::LinkConfirm, dst, "");
        if let Some(p) = self.peers.get_mut(dst) {
            p.duplicate_open_repair = None;
            p.confirmed_gap_repair = None;
            p.send_phase = Phase::Down;
            p.receive_ready = false;
            p.pending_link_confirmation = None;
        }
    }
}
