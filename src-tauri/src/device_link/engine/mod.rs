//! 无 IO 的 device-link 被控端引擎：所有输入（帧、定时器、令牌、socket 事件）都是方法调用，
//! 输出是 `Out` 事件队列。运行时（`runtime.rs`）负责 websocket 与定时器。
//! 对应上游 `packages/device-link/src/client.ts` 里被控端用到的部分。
//!
//! 刻意不含：直连（WebRTC）、共享任务 / provider share 作用域、relay 帧签名、
//! 控制端发起 invoke、推送压缩（被控端不声明 push-deflate 能力，因此不会收到压缩推送）。
pub mod peer;
pub mod types;

mod conn;
pub use link::InboundOffer;
mod link;
mod recv;
mod send;
mod tx;

#[cfg(test)]
mod tests;

use indexmap::IndexMap;
use peer::{short, Peer, RouteLedger, SendBudget};
use serde_json::Value;
use std::collections::{BTreeMap, VecDeque};
pub use types::*;

pub struct Engine {
    pub timing: Timing,
    pub(crate) out: VecDeque<Out>,
    pub(crate) timers: BTreeMap<TimerKey, u64>,
    pub(crate) now_ms: u64,
    pub(crate) wall_ms: u64,
    pub(crate) status: Status,
    pub(crate) stopped: bool,
    pub(crate) conn_epoch: u64,
    pub(crate) ws_open: bool,
    pub(crate) peers: IndexMap<String, Peer>,
    pub(crate) ledger: RouteLedger,
    pub(crate) pending_offers: IndexMap<String, link::InboundOffer>,
    pub(crate) pending_presence: Option<serde_json::Map<String, Value>>,
    pub(crate) hello: Value,
    pub(crate) server_capabilities: Vec<String>,
    pub(crate) self_device_id: Option<String>,
    pub(crate) issue: Option<Issue>,
    pub(crate) reconnect_attempt: u32,
    pub(crate) congestion_streak: u32,
    pub(crate) handshake_streak: u32,
    pub(crate) short_lived_streak: u32,
    pub(crate) online_since_wall: Option<u64>,
    pub(crate) connection_started_ms: u64,
    pub(crate) last_inbound_ms: u64,
    pub(crate) last_peer_inbound_ms: u64,
    pub(crate) last_ping_sent_ms: Option<u64>,
    pub(crate) pong_misses: u32,
    pub(crate) last_socket_error: Option<String>,
    pub(crate) congestion_budget: SendBudget,
    pub(crate) local_budget: SendBudget,
    pub(crate) ws_buffered: usize,
    pub(crate) local_send_pressure: bool,
    pub(crate) rng: u64,
    pub(crate) id_counter: u64,
    /// 无登录态等待令牌的连接尝试。
    pub(crate) awaiting_token_epoch: Option<u64>,
}

impl Engine {
    pub fn new(timing: Timing, seed: u64) -> Self {
        Engine {
            timing,
            out: VecDeque::new(),
            timers: BTreeMap::new(),
            now_ms: 0,
            wall_ms: 0,
            status: Status::Stopped,
            stopped: true,
            conn_epoch: 0,
            ws_open: false,
            peers: IndexMap::new(),
            ledger: RouteLedger::default(),
            pending_offers: IndexMap::new(),
            pending_presence: None,
            hello: Value::Null,
            server_capabilities: Vec::new(),
            self_device_id: None,
            issue: None,
            reconnect_attempt: 0,
            congestion_streak: 0,
            handshake_streak: 0,
            short_lived_streak: 0,
            online_since_wall: None,
            connection_started_ms: 0,
            last_inbound_ms: 0,
            last_peer_inbound_ms: 0,
            last_ping_sent_ms: None,
            pong_misses: 0,
            last_socket_error: None,
            congestion_budget: SendBudget::default(),
            local_budget: SendBudget::default(),
            ws_buffered: 0,
            local_send_pressure: false,
            rng: seed | 1,
            id_counter: 0,
            awaiting_token_epoch: None,
        }
    }

    /// 运行时在每次输入前推进时钟：`mono` 单调毫秒，`wall` 墙钟毫秒。
    pub fn set_clock(&mut self, mono: u64, wall: u64) {
        self.now_ms = mono;
        self.wall_ms = wall;
    }

    pub fn drain(&mut self) -> Vec<Out> {
        self.out.drain(..).collect()
    }

    pub fn status(&self) -> Status {
        self.status
    }

    pub fn self_device_id(&self) -> Option<&str> {
        self.self_device_id.as_deref()
    }

    pub fn issue(&self) -> Option<&Issue> {
        self.issue.as_ref()
    }

    pub fn set_hello(&mut self, hello: Value) {
        self.hello = hello;
    }

    pub(crate) fn log(&mut self, level: &'static str, msg: String) {
        self.out.push_back(Out::Log(level, msg));
    }

    pub(crate) fn push(&mut self, o: Out) {
        self.out.push_back(o);
    }

    /// 不加密的伪随机（退避抖动与请求 id 的盐）。
    pub(crate) fn rand_f64(&mut self) -> f64 {
        // xorshift64*
        let mut x = self.rng;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.rng = x;
        (x.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 11) as f64 / (1u64 << 53) as f64
    }

    /// 生成请求 / stream id（UUID v4 形状；只要求不撞）。
    pub(crate) fn new_id(&mut self) -> String {
        self.id_counter += 1;
        let a = (self.rand_f64() * (u64::MAX as f64)) as u64 ^ self.id_counter.rotate_left(17);
        let b = (self.rand_f64() * (u64::MAX as f64)) as u64 ^ self.wall_ms;
        let mut bytes = [0u8; 16];
        bytes[..8].copy_from_slice(&a.to_be_bytes());
        bytes[8..].copy_from_slice(&b.to_be_bytes());
        bytes[6] = (bytes[6] & 0x0f) | 0x40;
        bytes[8] = (bytes[8] & 0x3f) | 0x80;
        let h: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
        format!("{}-{}-{}-{}-{}", &h[0..8], &h[8..12], &h[12..16], &h[16..20], &h[20..32])
    }

    pub(crate) fn arm(&mut self, kind: TimerKind, peer: &str, stream: &str, after_ms: u64) {
        self.timers.insert(TimerKey { kind, peer: peer.to_owned(), stream: stream.to_owned() }, self.now_ms + after_ms);
    }

    pub(crate) fn disarm(&mut self, kind: TimerKind, peer: &str, stream: &str) {
        self.timers.remove(&TimerKey { kind, peer: peer.to_owned(), stream: stream.to_owned() });
    }

    pub(crate) fn disarm_all(&mut self, kind: TimerKind, peer: &str) {
        let keys: Vec<_> = self.timers.keys().filter(|k| k.kind == kind && k.peer == peer).cloned().collect();
        for k in keys {
            self.timers.remove(&k);
        }
    }

    pub(crate) fn armed(&self, kind: TimerKind, peer: &str, stream: &str) -> bool {
        self.timers.contains_key(&TimerKey { kind, peer: peer.to_owned(), stream: stream.to_owned() })
    }

    /// 下一个到期时刻（毫秒，单调时钟）；运行时据此 sleep。
    pub fn next_deadline(&self) -> Option<u64> {
        self.timers.values().copied().min()
    }

    /// 触发所有已到期的定时器。
    pub fn tick(&mut self) {
        loop {
            let due: Option<TimerKey> = self.timers.iter().filter(|(_, at)| **at <= self.now_ms).min_by_key(|(_, at)| **at).map(|(k, _)| k.clone());
            let Some(key) = due else { break };
            self.timers.remove(&key);
            self.fire(key);
        }
    }

    fn fire(&mut self, key: TimerKey) {
        let TimerKey { kind, peer, stream } = key;
        match kind {
            TimerKind::Reconnect => self.on_reconnect_timer(),
            TimerKind::Handshake => self.on_handshake_timeout(),
            TimerKind::Ping => self.on_ping_timer(),
            TimerKind::PresenceRetry => self.flush_presence(),
            TimerKind::ReconnectStable => self.on_reconnect_stable(),
            TimerKind::CongestionStable => self.on_congestion_stable(),
            TimerKind::Retry => self.on_retry_timer(&peer),
            TimerKind::LinkConfirm => self.on_link_confirm_timeout(&peer),
            TimerKind::TimeoutCloseNotify => self.on_timeout_close_retry(&peer, &stream),
            TimerKind::DelayedAck => self.on_delayed_ack(&peer, &stream),
        }
    }

    pub(crate) fn peer(&mut self, dst: &str) -> &mut Peer {
        if !self.peers.contains_key(dst) {
            let sid = self.new_id();
            self.peers.insert(dst.to_owned(), Peer::new(sid, self.conn_epoch));
        }
        self.peers.get_mut(dst).unwrap()
    }

    pub(crate) fn has_server_capability(&self, c: &str) -> bool {
        self.server_capabilities.iter().any(|x| x == c)
    }

    pub fn short(id: &str) -> String {
        short(id)
    }
}
