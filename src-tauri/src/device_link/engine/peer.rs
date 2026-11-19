//! 每个对端（控制端）的可靠传输状态。对应上游 client.ts 的 PeerTransportState 及其附属结构（被控端用到的部分）。
use super::super::transport::RetransmitEstimate;
use super::types::Env;
use indexmap::IndexMap;
use std::collections::BTreeMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    Down,
    AwaitingConfirm,
    Ready,
}

#[derive(Debug, Clone)]
pub struct Pending {
    pub seq: u64,
    /// 逻辑信封；重放时按当前最早 pending seq 刷新 baseSeq。
    pub env: Env,
    /// 按 baseSeq=seq 预留的最坏 wire 大小。
    pub bytes: usize,
    pub attempts: u32,
    pub last_sent_at: u64,
    pub sent: bool,
    pub enqueued_at: u64,
    pub first_sent_at: Option<u64>,
}

impl Pending {
    pub fn reset_send_round(&mut self) {
        self.attempts = 0;
        self.last_sent_at = 0;
        self.first_sent_at = None;
    }
}

#[derive(Debug, Clone)]
pub struct ResumePlan {
    pub duplicate_open: bool,
    pub resumed_link: bool,
    pub enter_recovery: bool,
}

#[derive(Debug, Clone)]
pub struct LinkConfirm {
    pub request_id: String,
    pub minimum_ack_seq: u64,
    pub resume: ResumePlan,
    /// 取值 Down 或 Ready。
    pub previous_send_phase: Phase,
}

#[derive(Debug, Clone)]
pub struct DuplicateOpenRepair {
    pub request_id: String,
    pub head_seq: u64,
    pub through_seq: u64,
}

#[derive(Debug, Clone)]
pub struct GapRepair {
    pub awaiting_seq: u64,
    pub through_seq: u64,
}

#[derive(Debug, Clone)]
pub struct Assembly {
    pub received_at: u64,
    pub kind: String,
    pub id: Option<String>,
    pub src: Option<String>,
    pub dst: Option<String>,
    pub total: usize,
    pub total_bytes: usize,
    pub chunks: BTreeMap<usize, String>,
    pub bytes: usize,
}

#[derive(Debug, Clone)]
pub struct Ready {
    pub env: Env,
    pub json: String,
}

#[derive(Debug, Clone)]
pub struct RecvStream {
    pub last_delivered_seq: u64,
    pub requested_base_seq: u64,
    pub assemblies: BTreeMap<u64, Assembly>,
    pub ready: BTreeMap<u64, Ready>,
    pub buffered_bytes: usize,
    /// transport-ack-v2：延后 ACK 是否已排期（定时器在引擎的 timers 里）。
    pub delayed_ack_armed: bool,
    pub unacked_messages: u64,
    pub unacked_bytes: u64,
    pub unacked_non_push: bool,
}

impl RecvStream {
    pub fn new(base_seq: u64) -> Self {
        RecvStream {
            last_delivered_seq: base_seq.saturating_sub(1),
            requested_base_seq: base_seq,
            assemblies: BTreeMap::new(),
            ready: BTreeMap::new(),
            buffered_bytes: 0,
            delayed_ack_armed: false,
            unacked_messages: 0,
            unacked_bytes: 0,
            unacked_non_push: false,
        }
    }
}

#[derive(Debug)]
pub struct Peer {
    pub stream_id: String,
    pub remote_stream_id: Option<String>,
    pub remote_base_seq: u64,
    pub next_seq: u64,
    pub reliable: bool,
    pub send_phase: Phase,
    pub receive_ready: bool,
    pub pending_link_confirmation: Option<LinkConfirm>,
    pub explicitly_closed: bool,
    pub link_accepted_inbound: bool,
    pub supports_transport_timeout_close: bool,
    pub accepts_ack_v2: bool,
    pub unlinked_legacy_response_ids: Vec<String>,
    pub pending: BTreeMap<u64, Pending>,
    pub pending_bytes: usize,
    pub receive: IndexMap<String, RecvStream>,
    pub highest_ack_seq: u64,
    pub last_replay_epoch: u64,
    pub last_replay_remote_stream_id: Option<String>,
    pub duplicate_open_repair: Option<DuplicateOpenRepair>,
    pub confirmed_gap_repair: Option<GapRepair>,
    pub last_duplicate_open_repair_seq: u64,
    pub last_duplicate_open_repair_at: Option<u64>,
    pub recovery_needs_ack: bool,
    pub recovery_frames_sent: usize,
    pub rtt: RetransmitEstimate,
    pub push_admission_drop_count: u64,
    pub link_generation: u64,
}

impl Peer {
    pub fn new(stream_id: String, conn_epoch: u64) -> Self {
        Peer {
            stream_id,
            remote_stream_id: None,
            remote_base_seq: 1,
            next_seq: 1,
            reliable: false,
            send_phase: Phase::Down,
            receive_ready: false,
            pending_link_confirmation: None,
            explicitly_closed: false,
            link_accepted_inbound: false,
            supports_transport_timeout_close: false,
            accepts_ack_v2: false,
            unlinked_legacy_response_ids: Vec::new(),
            pending: BTreeMap::new(),
            pending_bytes: 0,
            receive: IndexMap::new(),
            highest_ack_seq: 0,
            last_replay_epoch: conn_epoch,
            last_replay_remote_stream_id: None,
            duplicate_open_repair: None,
            confirmed_gap_repair: None,
            last_duplicate_open_repair_seq: 0,
            last_duplicate_open_repair_at: None,
            recovery_needs_ack: false,
            recovery_frames_sent: 0,
            rtt: RetransmitEstimate::default(),
            push_admission_drop_count: 0,
            link_generation: 0,
        }
    }
    pub fn send_ready(&self) -> bool {
        self.send_phase == Phase::Ready
    }
    pub fn link_ready(&self) -> bool {
        self.send_ready() && self.receive_ready
    }
    /// 当前最早仍可能重放的 seq；无 pending 时是下一个 seq。
    pub fn base_seq(&self) -> u64 {
        self.pending.keys().next().copied().unwrap_or(self.next_seq)
    }
}

/// 路由代次账本：relay-error 没有物理发送标识，靠「发送时记下的 link generation」把迟到错误归属到正确的链路代。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Run {
    pub generation: u64,
    pub count: u64,
}

pub const MAX_IDS_PER_PEER: usize = 1_024;
pub const MAX_IDS_TOTAL: usize = 16_384;
pub const MAX_RUNS_PER_ID: usize = 256;
pub const MAX_SETTLED_PER_PEER: usize = 1_024;
pub const MAX_SETTLED_TOTAL: usize = 16_384;

type Ledger = IndexMap<String, IndexMap<String, Vec<Run>>>;

#[derive(Debug, Default)]
pub struct RouteLedger {
    active: Ledger,
    settled: Ledger,
}

impl RouteLedger {
    pub fn clear(&mut self) {
        self.active.clear();
        self.settled.clear();
    }

    /// Err = 账本满（对应 BACKPRESSURE）。
    pub fn remember(&mut self, id: &str, device: &str, generation: u64) -> Result<(), String> {
        let has_runs = self.active.get(device).map_or(false, |m| m.contains_key(id));
        if !has_runs {
            let total: usize = self.active.values().map(|m| m.len()).sum();
            let per = self.active.get(device).map_or(0, |m| m.len());
            if per >= MAX_IDS_PER_PEER || total >= MAX_IDS_TOTAL {
                return Err(format!("route attempt history is full for peer {}", short(device)));
            }
            self.active.entry(device.to_owned()).or_default().insert(id.to_owned(), vec![Run { generation, count: 1 }]);
            return Ok(());
        }
        let runs = self.active.get_mut(device).and_then(|m| m.get_mut(id)).unwrap();
        if let Some(last) = runs.last_mut() {
            if last.generation == generation {
                last.count += 1;
                return Ok(());
            }
        }
        if runs.len() >= MAX_RUNS_PER_ID {
            return Err(format!("route generation history is full for peer {}", short(device)));
        }
        runs.push(Run { generation, count: 1 });
        Ok(())
    }

    /// 取走该 id 最早的一次尝试的代次（先查已结算，再查在途）。
    pub fn consume(&mut self, id: &str, device: Option<&str>) -> Option<u64> {
        for settled in [true, false] {
            let ledger = if settled { &mut self.settled } else { &mut self.active };
            let dev = match device {
                Some(d) => ledger.get(d).filter(|m| m.contains_key(id)).map(|_| d.to_owned()),
                None => ledger.iter().find(|(_, m)| m.contains_key(id)).map(|(k, _)| k.clone()),
            };
            let Some(dev) = dev else { continue };
            let attempts = ledger.get_mut(&dev).unwrap();
            let runs = attempts.get_mut(id).unwrap();
            let first = runs.first_mut()?;
            let generation = first.generation;
            first.count -= 1;
            if first.count == 0 {
                runs.remove(0);
            }
            if runs.is_empty() {
                attempts.shift_remove(id);
            }
            if attempts.is_empty() {
                ledger.shift_remove(&dev);
            }
            return Some(generation);
        }
        None
    }

    pub fn discard_before(&mut self, device: &str, min_generation: u64) {
        for ledger in [&mut self.active, &mut self.settled] {
            let Some(attempts) = ledger.get_mut(device) else { continue };
            let ids: Vec<String> = attempts.keys().cloned().collect();
            for id in ids {
                let runs = attempts.get_mut(&id).unwrap();
                runs.retain(|r| r.generation >= min_generation);
                if runs.is_empty() {
                    attempts.shift_remove(&id);
                }
            }
            if attempts.is_empty() {
                ledger.shift_remove(device);
            }
        }
    }

    pub fn discard_for_id(&mut self, device: &str, id: &str) {
        for ledger in [&mut self.active, &mut self.settled] {
            if let Some(attempts) = ledger.get_mut(device) {
                attempts.shift_remove(id);
                if attempts.is_empty() {
                    ledger.shift_remove(device);
                }
            }
        }
    }

    /// 把在途记录移到已结算（不占发送额度，只保留近期代次供迟到 relay-error 归属）。
    pub fn settle(&mut self, device: &str, id: &str) {
        let Some(active) = self.active.get_mut(device) else { return };
        let Some(runs) = active.shift_remove(id) else { return };
        if active.is_empty() {
            self.active.shift_remove(device);
        }
        let settled = self.settled.entry(device.to_owned()).or_default();
        let mut merged = settled.shift_remove(id).unwrap_or_default();
        for run in runs {
            match merged.last_mut() {
                Some(l) if l.generation == run.generation => l.count += run.count,
                _ => merged.push(run),
            }
        }
        while merged.len() > MAX_RUNS_PER_ID {
            merged.remove(0);
        }
        settled.insert(id.to_owned(), merged);
        self.trim_settled();
    }

    fn trim_settled(&mut self) {
        let devices: Vec<String> = self.settled.keys().cloned().collect();
        for d in &devices {
            let m = self.settled.get_mut(d).unwrap();
            while m.len() > MAX_SETTLED_PER_PEER {
                m.shift_remove_index(0);
            }
            if m.is_empty() {
                self.settled.shift_remove(d);
            }
        }
        let mut total: usize = self.settled.values().map(|m| m.len()).sum();
        if total <= MAX_SETTLED_TOTAL {
            return;
        }
        let devices: Vec<String> = self.settled.keys().cloned().collect();
        for d in devices {
            let m = self.settled.get_mut(&d).unwrap();
            while !m.is_empty() && total > MAX_SETTLED_TOTAL {
                m.shift_remove_index(0);
                total -= 1;
            }
            if m.is_empty() {
                self.settled.shift_remove(&d);
            }
            if total <= MAX_SETTLED_TOTAL {
                break;
            }
        }
    }

    pub fn rollback(&mut self, id: &str, device: &str, generation: u64) {
        let Some(attempts) = self.active.get_mut(device) else { return };
        let Some(runs) = attempts.get_mut(id) else { return };
        match runs.last_mut() {
            Some(l) if l.generation == generation => {
                l.count -= 1;
                if l.count == 0 {
                    runs.pop();
                }
            }
            _ => return,
        }
        if runs.is_empty() {
            attempts.shift_remove(id);
        }
        if attempts.is_empty() {
            self.active.shift_remove(device);
        }
    }

    #[cfg(test)]
    pub fn active_len(&self) -> usize {
        self.active.values().map(|m| m.len()).sum()
    }
}

pub fn short(id: &str) -> String {
    id.chars().take(8).collect()
}

/// 拥塞 / 本地发送额度：按 250ms 窗口、每窗口 8 帧在多个 peer 间轮转（上游 CongestionSendBudget）。
#[derive(Debug)]
pub struct SendBudget {
    window: i64,
    spent: i64,
    turn: i64,
    by_peer: std::collections::HashMap<String, i64>,
}

impl Default for SendBudget {
    fn default() -> Self {
        SendBudget { window: -1, spent: 0, turn: -1, by_peer: Default::default() }
    }
}

impl SendBudget {
    pub fn reset(&mut self) {
        *self = SendBudget::default();
    }
    pub fn refund(&mut self, peer: &str, unsent: i64) {
        self.spent = (self.spent - unsent).max(0);
        let e = self.by_peer.entry(peer.to_owned()).or_insert(0);
        *e = (*e - unsent).max(0);
    }
    pub fn take(&mut self, peer: &str, frames: i64, peers: &[String], now: f64) -> bool {
        if !self.can_take(peer, frames, peers, now) {
            return false;
        }
        self.spent += frames;
        *self.by_peer.entry(peer.to_owned()).or_insert(0) += frames;
        true
    }
    pub fn can_take(&mut self, peer: &str, frames: i64, peers: &[String], now: f64) -> bool {
        let window = (now / 250.0).floor() as i64;
        if window != self.window {
            self.spent = if self.window >= 0 && window > self.window { (self.spent - 8 * (window - self.window)).max(0) } else { 0 };
            if self.spent < 8 {
                self.turn += 1;
            }
            self.window = window;
            self.by_peer.clear();
        }
        let index = peers.iter().position(|p| p == peer).map_or(-1, |i| i as i64);
        let count = (peers.len() as i64).max(1);
        let offset = ((index.max(0) - self.turn % count) + count) % count;
        if offset >= 8 {
            return false;
        }
        let share = ((8 / count.min(8)) as i64).max(1);
        let used = *self.by_peer.get(peer).unwrap_or(&0);
        if frames > share {
            if offset != 0 || used != 0 || self.spent >= 8 {
                return false;
            }
        } else if self.spent + frames > 8 || used + frames > share {
            return false;
        }
        true
    }
}
