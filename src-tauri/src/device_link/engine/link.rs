//! 链路层：入站 link-open → link-accept、能力协商、恢复/重放、link-close、invoke-result / push 的发送入口。
use super::peer::{short, DuplicateOpenRepair, LinkConfirm, Phase, ResumePlan};
use super::send::is_unlinked_legacy;
use super::types::*;
use super::{Engine, TimerKind};
use crate::device_link::model_speed::encode_model_speed_payload;
use crate::device_link::transport::*;
use serde_json::{json, Map, Value};

const MAX_PENDING_INBOUND_LINK_OFFERS: usize = 64;
const MAX_UNLINKED_LEGACY_RESPONSE_IDS: usize = 128;

#[derive(Debug, Clone)]
pub struct InboundOffer {
    pub request_id: String,
    pub capabilities: Option<Vec<String>>,
    pub transport_stream_id: Option<String>,
    pub transport_base_seq: Option<u64>,
}

fn caps_of(payload: &Value) -> Option<Vec<String>> {
    payload.get("capabilities").and_then(Value::as_array).map(|a| a.iter().filter_map(|c| c.as_str().map(str::to_owned)).collect())
}

fn merge_caps(current: Option<Vec<String>>, additions: &[&str]) -> Vec<String> {
    let mut out = current.unwrap_or_default();
    for a in additions {
        if !out.iter().any(|c| c == a) {
            out.push((*a).to_owned());
        }
    }
    out
}

impl Engine {
    pub(crate) fn on_link_open_frame(&mut self, env: &Env) {
        let (Some(src), Some(id)) = (env.src.clone(), env.id.clone()) else { return };
        let payload = env.payload.clone().unwrap_or(Value::Null);
        let base = payload.get("transportBaseSeq").and_then(Value::as_f64).filter(|n| n.fract() == 0.0 && *n > 0.0).map(|n| n as u64);
        let offer = InboundOffer {
            request_id: id,
            capabilities: caps_of(&payload),
            transport_stream_id: payload.get("transportStreamId").and_then(Value::as_str).map(str::to_owned),
            transport_base_seq: base,
        };
        self.pending_offers.shift_remove(&src);
        self.pending_offers.insert(src, offer);
        while self.pending_offers.len() > MAX_PENDING_INBOUND_LINK_OFFERS {
            self.pending_offers.shift_remove_index(0);
        }
    }

    /// 返回 true = 吞掉（不交给应用层）。
    pub(crate) fn on_link_close_frame(&mut self, env: &Env) -> bool {
        let Some(src) = env.src.clone() else { return false };
        let reason = env.payload.as_ref().and_then(|p| p.get("reason")).and_then(Value::as_str).unwrap_or("");
        if reason == "transport-timeout" {
            // 对端对我们的可靠重试耗尽后的瞬时重置：保留 stream 与 pending，等对端重新 link-open
            self.mark_peer_link_down(&src);
            self.disarm(TimerKind::Retry, &src, "");
            return false;
        }
        self.pending_offers.shift_remove(&src);
        self.mark_peer_link_down(&src);
        {
            let p = self.peer(&src);
            p.explicitly_closed = true;
            p.unlinked_legacy_response_ids.clear();
        }
        self.disarm_all(TimerKind::TimeoutCloseNotify, &src);
        {
            let p = self.peer(&src);
            p.link_accepted_inbound = false;
            p.reliable = false;
            p.remote_stream_id = None;
            p.remote_base_seq = 1;
            p.receive.clear();
        }
        self.abandon_reliable_pending(&src, "control link closed by peer");
        false
    }

    pub(crate) fn on_invoke_frame(&mut self, env: &Env) {
        let (Some(src), Some(id)) = (env.src.clone(), env.id.clone()) else { return };
        if !is_unlinked_legacy(env) {
            return;
        }
        let p = self.peer(&src);
        if p.explicitly_closed && !p.link_ready() {
            p.unlinked_legacy_response_ids.retain(|x| x != &id);
            p.unlinked_legacy_response_ids.push(id);
            while p.unlinked_legacy_response_ids.len() > MAX_UNLINKED_LEGACY_RESPONSE_IDS {
                p.unlinked_legacy_response_ids.remove(0);
            }
        }
    }

    /// 被控端：回 link-accept。
    pub fn send_link_accept(&mut self, dst: &str, request_id: &str, payload: Value) -> Result<(), LinkError> {
        let offer = self.pending_offers.get(dst).filter(|o| o.request_id == request_id).cloned();
        let offer_caps = offer.as_ref().and_then(|o| o.capabilities.clone());
        let supports_reliable = offer_caps.as_ref().is_some_and(|c| c.iter().any(|x| x == CAP_RELIABLE_TRANSPORT));
        let supports_confirm = supports_reliable && offer_caps.as_ref().is_some_and(|c| c.iter().any(|x| x == CAP_RELIABLE_LINK_CONFIRM));
        self.peer(dst);
        if supports_reliable {
            self.drop_discardable_prefix(dst, false, "before link re-establishment replay");
        }
        let link_generation = self.peers.get(dst).unwrap().link_generation + 1;
        let mut body: Map<String, Value> = payload.as_object().cloned().unwrap_or_default();
        if supports_reliable {
            let mut additions = vec![CAP_RELIABLE_TRANSPORT];
            if supports_confirm {
                additions.push(CAP_RELIABLE_LINK_CONFIRM);
            }
            additions.push(CAP_TRANSPORT_ACK_V2);
            let merged = merge_caps(caps_of(&payload), &additions);
            body.insert("capabilities".into(), json!(merged));
            let p = self.peers.get(dst).unwrap();
            body.insert("transportStreamId".into(), json!(p.stream_id));
            body.insert("transportBaseSeq".into(), json!(p.base_seq()));
        }
        let env = Env::new("link-accept").id(request_id).to(dst).with(Value::Object(body));
        let accept_id = self.send_routed(&env, Some(link_generation))?;
        // markPeerRouteOnline
        self.peers.get_mut(dst).unwrap().link_generation = link_generation;
        if !supports_confirm {
            if let Some(id) = accept_id {
                self.ledger.settle(dst, &id);
            }
        }
        if let Some(o) = offer {
            self.pending_offers.shift_remove(dst);
            self.set_peer_capabilities(dst, o.capabilities.as_deref(), o.transport_stream_id.as_deref(), o.transport_base_seq);
        }
        {
            let p = self.peers.get_mut(dst).unwrap();
            p.link_accepted_inbound = true;
            p.duplicate_open_repair = None;
        }
        if supports_reliable {
            let resume = self.plan_send_resume(dst);
            self.commit_receive_ready(dst);
            if supports_confirm {
                self.begin_link_confirmation(dst, request_id, resume);
            } else {
                self.commit_send_resume(dst, resume);
            }
        }
        Ok(())
    }

    fn set_peer_capabilities(&mut self, dst: &str, caps: Option<&[String]>, remote_stream_id: Option<&str>, remote_base_seq: Option<u64>) {
        let has = |c: &str| caps.is_some_and(|l| l.iter().any(|x| x == c));
        let reliable = has(CAP_RELIABLE_TRANSPORT);
        let next_remote = if reliable { remote_stream_id.filter(|s| !s.is_empty()).map(str::to_owned) } else { None };
        let next_base = if reliable { remote_base_seq.filter(|b| *b > 0).unwrap_or(1) } else { 1 };
        {
            let p = self.peer(dst);
            p.supports_transport_timeout_close = has(CAP_TRANSPORT_TIMEOUT_CLOSE);
            p.accepts_ack_v2 = reliable && has(CAP_TRANSPORT_ACK_V2);
            if p.remote_stream_id != next_remote {
                p.receive.clear();
                p.duplicate_open_repair = None;
                p.confirmed_gap_repair = None;
            }
        }
        if self.peers.get(dst).unwrap().reliable && !reliable {
            self.abandon_reliable_pending(dst, "peer no longer supports reliable transport");
        }
        if !reliable {
            self.mark_peer_link_down(dst);
        } else {
            self.peers.get_mut(dst).unwrap().receive_ready = false;
        }
        let p = self.peers.get_mut(dst).unwrap();
        p.reliable = reliable;
        p.explicitly_closed = false;
        p.remote_stream_id = next_remote.clone();
        p.remote_base_seq = next_base;
        if let Some(rs) = next_remote {
            self.get_receive_stream(dst, &rs, next_base);
        }
    }

    fn plan_send_resume(&mut self, dst: &str) -> ResumePlan {
        let epoch = self.conn_epoch;
        let p = self.peer(dst);
        let was_ready = p.send_ready();
        let stream_changed = p.last_replay_remote_stream_id != p.remote_stream_id;
        let duplicate_open = was_ready && !stream_changed && p.last_replay_epoch == epoch;
        let had_prior = p.last_replay_remote_stream_id.is_some();
        let resumed_link = !was_ready || stream_changed;
        ResumePlan { duplicate_open, resumed_link, enter_recovery: resumed_link && (had_prior || !p.pending.is_empty() || p.recovery_needs_ack) }
    }

    fn commit_receive_ready(&mut self, dst: &str) {
        self.peer(dst).receive_ready = true;
        let ids: Vec<String> = self.peers.get(dst).unwrap().receive.keys().cloned().collect();
        for sid in ids {
            let s = self.stream(dst, &sid);
            // 与 applyReceiveStreamBase 一致
            let target = s.requested_base_seq.saturating_sub(1);
            if target > s.last_delivered_seq {
                let base = s.requested_base_seq;
                s.assemblies.retain(|k, _| *k >= base);
                s.ready.retain(|k, _| *k >= base);
                s.last_delivered_seq = target;
            }
            let next = self.stream(dst, &sid).last_delivered_seq + 1;
            if self.stream(dst, &sid).ready.contains_key(&next) {
                self.drain_transport_stream(dst, &sid);
            }
        }
    }

    fn begin_link_confirmation(&mut self, dst: &str, request_id: &str, resume: ResumePlan) {
        let prev = self.peers.get(dst).unwrap().pending_link_confirmation.clone();
        if resume.duplicate_open && self.peers.get(dst).unwrap().send_ready() {
            let p = self.peers.get_mut(dst).unwrap();
            let head = p.pending.values().next().map(|h| (h.seq, h.sent));
            let mut through = 0;
            for x in p.pending.values() {
                if !x.sent {
                    break;
                }
                through = x.seq;
            }
            p.duplicate_open_repair = match head {
                Some((seq, true)) => Some(DuplicateOpenRepair { request_id: request_id.to_owned(), head_seq: seq, through_seq: through }),
                _ => None,
            };
            let nonempty = !p.pending.is_empty();
            self.ledger.settle(dst, request_id);
            if nonempty {
                self.ensure_retry_timer(dst);
            }
            return;
        }
        self.disarm(TimerKind::LinkConfirm, dst, "");
        let (min_ack, timeout) = {
            let p = self.peers.get_mut(dst).unwrap();
            let previous = prev.map(|c| c.previous_send_phase).unwrap_or(if p.send_phase == Phase::Ready { Phase::Ready } else { Phase::Down });
            p.send_phase = Phase::AwaitingConfirm;
            let min_ack = p.base_seq().saturating_sub(1);
            p.pending_link_confirmation = Some(LinkConfirm { request_id: request_id.to_owned(), minimum_ack_seq: min_ack, resume, previous_send_phase: previous });
            let attempts = self.timing.transport_max_retry_attempts.max(1) as f64;
            let base = retransmit_base_ms(&p.rtt, self.timing.transport_retry_interval_ms as f64);
            (min_ack, ((self.timing.handshake_timeout_ms * 2) as f64).max(base * attempts) as u64)
        };
        let _ = min_ack;
        self.disarm(TimerKind::Retry, dst, "");
        let (n, bytes, sid) = {
            let p = self.peers.get(dst).unwrap();
            (p.pending.len(), p.pending_bytes, p.stream_id.clone())
        };
        self.log("info", format!("device-link recovery dst={} trigger=await-link-confirm pending={n}/{bytes} stream={} request={} conn={}", short(dst), short(&sid), short(request_id), self.conn_epoch));
        self.arm(TimerKind::LinkConfirm, dst, "", timeout);
    }

    pub(crate) fn commit_send_resume(&mut self, dst: &str, resume: ResumePlan) {
        self.disarm(TimerKind::LinkConfirm, dst, "");
        {
            let p = self.peer(dst);
            p.send_phase = Phase::Ready;
            p.pending_link_confirmation = None;
        }
        self.disarm_all(TimerKind::TimeoutCloseNotify, dst);
        if resume.duplicate_open {
            if !self.peers.get(dst).unwrap().pending.is_empty() {
                self.ensure_retry_timer(dst);
            }
            return;
        }
        let epoch = self.conn_epoch;
        {
            let p = self.peers.get_mut(dst).unwrap();
            p.last_replay_epoch = epoch;
            p.last_replay_remote_stream_id = p.remote_stream_id.clone();
            p.confirmed_gap_repair = None;
            if resume.enter_recovery {
                p.recovery_needs_ack = true;
                p.recovery_frames_sent = 0;
            }
        }
        self.replay_pending(dst, resume.resumed_link);
    }

    /// 被控端主动关链（撤销 / 关远控 / 退出）。
    pub fn close_link(&mut self, dst: &str, reason: &str) {
        self.disarm_all(TimerKind::TimeoutCloseNotify, dst);
        self.pending_offers.shift_remove(dst);
        if self.peers.contains_key(dst) {
            // 撤销入站方向：取消确认超时，恢复此前的发送阶段
            self.disarm(TimerKind::LinkConfirm, dst, "");
            let restore = self.peers.get_mut(dst).and_then(|p| p.pending_link_confirmation.take());
            if let Some(c) = restore {
                let p = self.peers.get_mut(dst).unwrap();
                if p.send_phase == Phase::AwaitingConfirm {
                    p.send_phase = c.previous_send_phase;
                    if p.send_phase == Phase::Ready && !p.pending.is_empty() {
                        self.ensure_retry_timer(dst);
                    }
                }
            }
            self.peers.get_mut(dst).unwrap().link_accepted_inbound = false;
        }
        if self.status != Status::Online {
            return;
        }
        let env = Env::new("link-close").to(dst).with(json!({ "reason": reason }));
        if let Err(e) = self.send_best_effort(&env) {
            self.log("debug", format!("link-close notification failed for {}: {}", short(dst), e.message));
        }
    }

    /// 被控端：回 invoke-result。
    pub fn send_invoke_result(&mut self, dst: &str, request_id: &str, payload: Value) -> Result<(), LinkError> {
        let env = Env::new("invoke-result").id(request_id).to(dst).with(payload);
        let (reliable, ready) = self.peers.get(dst).map_or((false, false), |p| (p.reliable, p.send_ready()));
        if reliable && !ready && self.status == Status::Online {
            // 死锁绕行：链路未就绪时结果先留可靠副本，再裸帧直发
            if let Err(err) = self.send_peer_envelope(&env, false) {
                if err.code != "BACKPRESSURE" {
                    return Err(err);
                }
                if let Err(direct) = self.send_best_effort(&env) {
                    return Err(if direct.code == "PAYLOAD_TOO_LARGE" { err } else { direct });
                }
                self.forget_unlinked(dst, request_id);
                return Ok(());
            }
            match self.send_best_effort(&env) {
                Ok(()) => self.forget_unlinked(dst, request_id),
                Err(e) => {
                    if e.code != "PAYLOAD_TOO_LARGE" {
                        self.log("debug", format!("unlinked result direct send failed; reliable copy retained: {}", e.message));
                    }
                }
            }
            return Ok(());
        }
        let allow = self.peers.get(dst).is_some_and(|p| p.unlinked_legacy_response_ids.iter().any(|x| x == request_id));
        self.send_peer_envelope(&env, allow)?;
        if allow {
            self.forget_unlinked(dst, request_id);
        }
        Ok(())
    }

    fn forget_unlinked(&mut self, dst: &str, id: &str) {
        if let Some(p) = self.peers.get_mut(dst) {
            p.unlinked_legacy_response_ids.retain(|x| x != id);
        }
    }

    /// 被控端：转发 push（fire-and-forget；失败由上层缓冲策略兜底）。
    pub fn send_push(&mut self, dst: &str, channel: &str, payload: Value, owner_stamp: Option<Value>) -> Result<(), LinkError> {
        let payload = encode_model_speed_payload(channel, payload);
        if self.status != Status::Online {
            return Ok(());
        }
        let mut body = Map::new();
        body.insert("channel".into(), json!(channel));
        body.insert("payload".into(), payload);
        if let Some(s) = owner_stamp {
            body.insert("ownerStamp".into(), s);
        }
        let env = Env::new("push").to(dst).with(Value::Object(body));
        if channel == TRANSPORT_ACK_CHANNEL {
            return self.send_best_effort(&env);
        }
        self.send_peer_envelope(&env, false).map(|_| ())
    }

    pub fn is_link_ready(&self, dst: &str) -> bool {
        self.peers.get(dst).is_some_and(|p| p.link_ready())
    }

    pub fn peer_reliable_queue_depth(&self, dst: &str) -> usize {
        self.peers.get(dst).map_or(0, |p| p.pending.len())
    }
}
