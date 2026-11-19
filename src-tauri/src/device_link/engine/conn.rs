//! 连接生命周期：start/stop、令牌、socket 事件、hello/hello-ack、心跳、重连、presence。
use super::types::*;
use super::{Engine, TimerKind};
use crate::device_link::transport::PROTOCOL_VERSION;
use serde_json::{json, Value};

const SHORT_LIVED_STREAK_LIMIT: u32 = 3;
const HANDSHAKE_TIMEOUT_WIDEN_AFTER: u32 = 2;
const DUPLICATE_CONNECTION_CLOSE_CODE: u16 = 4409;
const RELAY_TRY_AGAIN_LATER_CLOSE_CODE: u16 = 1013;

impl Engine {
    pub fn start(&mut self) {
        if !self.stopped {
            return;
        }
        self.stopped = false;
        self.reconnect_attempt = 0;
        self.congestion_streak = 0;
        self.connect("start");
    }

    /// 主动重建连接（换账号 / 令牌刷新）。
    pub fn restart_connection(&mut self, reason: &str) {
        if self.stopped {
            return;
        }
        self.reconnect_attempt = 0;
        self.disarm(TimerKind::Reconnect, "", "");
        self.reset_link_state_for_reconnect();
        self.connect(reason);
    }

    pub fn stop(&mut self) {
        // 延后 ACK 定时器与 stream 计数
        let keys: Vec<_> = self.timers.keys().filter(|k| k.kind == TimerKind::DelayedAck).cloned().collect();
        for k in keys {
            self.timers.remove(&k);
        }
        for p in self.peers.values_mut() {
            for s in p.receive.values_mut() {
                s.delayed_ack_armed = false;
                s.unacked_messages = 0;
                s.unacked_bytes = 0;
                s.unacked_non_push = false;
            }
        }
        if self.stopped {
            return;
        }
        self.stopped = true;
        self.clear_timers();
        self.handshake_streak = 0;
        self.clear_peer_transport();
        self.congestion_budget.reset();
        self.local_budget.reset();
        self.ws_buffered = 0;
        self.pending_offers.clear();
        self.pending_presence = None;
        self.conn_epoch += 1;
        self.awaiting_token_epoch = None;
        if self.ws_open {
            self.ws_open = false;
            self.push(Out::CloseSocket { code: 1000, reason: "client stopped".into() });
        }
        let online_for = self.online_since_wall.map(|t| self.wall_ms.saturating_sub(t));
        self.log("info", format!("device-link stopped by host (onlineForMs={})", online_for.map_or("never-online".into(), |v| v.to_string())));
        self.online_since_wall = None;
        self.short_lived_streak = 0;
        self.set_issue(None);
        self.set_status(Status::Stopped);
    }

    fn clear_timers(&mut self) {
        let keep: Vec<_> = self.timers.keys().filter(|k| matches!(k.kind, TimerKind::DelayedAck)).cloned().collect();
        let kept: Vec<_> = keep.into_iter().filter_map(|k| self.timers.remove(&k).map(|v| (k, v))).collect();
        self.timers.clear();
        for (k, v) in kept {
            self.timers.insert(k, v);
        }
    }

    pub(crate) fn set_status(&mut self, s: Status) {
        if self.status == s {
            return;
        }
        self.status = s;
        self.push(Out::Status(s));
    }

    pub(crate) fn set_issue(&mut self, issue: Option<Issue>) {
        if self.issue == issue {
            return;
        }
        // 同类别只更新内容，不重复通知
        if let (Some(prev), Some(next)) = (&self.issue, &issue) {
            if prev.kind == next.kind {
                self.issue = issue;
                return;
            }
        }
        self.issue = issue.clone();
        self.push(Out::Issue(issue));
    }

    fn effective_handshake_timeout(&self) -> u64 {
        if self.handshake_streak >= HANDSHAKE_TIMEOUT_WIDEN_AFTER { self.timing.handshake_timeout_ms * 2 } else { self.timing.handshake_timeout_ms }
    }

    fn connect(&mut self, reason: &str) {
        if self.stopped {
            return;
        }
        self.reset_legacy_inbound_queue();
        self.connection_started_ms = self.now_ms;
        self.set_status(Status::Connecting);
        self.conn_epoch += 1;
        let epoch = self.conn_epoch;
        self.last_socket_error = None;
        self.log("debug", format!("connecting (reason={reason})"));
        if self.ws_open {
            self.ws_open = false;
            let online_for = self.online_since_wall.map(|t| self.wall_ms.saturating_sub(t));
            self.log("info", format!("discarding live socket for reconnect (reason={reason}, onlineForMs={})", online_for.map_or("never-online".into(), |v| v.to_string())));
            self.online_since_wall = None;
            self.push(Out::CloseSocket { code: 1000, reason: "reconnecting".into() });
        }
        self.awaiting_token_epoch = Some(epoch);
        self.push(Out::RequestToken { epoch });
    }

    /// 运行时取到令牌（None = 无登录态或取令牌失败）。
    pub fn on_token(&mut self, epoch: u64, token: Option<String>) {
        if self.stopped || epoch != self.conn_epoch || self.awaiting_token_epoch != Some(epoch) {
            return;
        }
        self.awaiting_token_epoch = None;
        match token {
            None => {
                self.log("debug", "no access token; retrying with backoff".into());
                self.schedule_reconnect();
            }
            Some(token) => {
                self.arm_handshake(epoch);
                self.push(Out::OpenSocket { epoch, token });
            }
        }
    }

    /// 打开 socket 失败（连接被拒、TLS 错误、升级失败等）。
    pub fn on_open_failed(&mut self, epoch: u64, error: &str) {
        if self.stopped || epoch != self.conn_epoch {
            return;
        }
        self.last_socket_error = Some(error.to_owned());
        self.log("warn", format!("relay connection error {error}"));
        // 对应上游：error 后 close 事件统一在 close 里处理重连
        self.on_socket_closed(epoch, None, "");
    }

    pub fn on_socket_open(&mut self, epoch: u64) {
        if epoch != self.conn_epoch {
            return;
        }
        self.ws_open = true;
        self.arm_handshake(epoch);
        let hello = self.hello.clone();
        let env = Env::new("hello").with(hello);
        let _ = self.send_envelope(&env);
    }

    pub fn on_socket_error(&mut self, epoch: u64, message: &str) {
        if epoch != self.conn_epoch {
            return;
        }
        self.last_socket_error = Some(message.to_owned());
        self.log("warn", format!("relay connection error {message}"));
    }

    pub fn on_socket_closed(&mut self, epoch: u64, code: Option<u16>, reason: &str) {
        if epoch != self.conn_epoch {
            return;
        }
        self.ws_open = false;
        self.handle_disconnect(code, reason);
    }

    fn arm_handshake(&mut self, _epoch: u64) {
        let t = self.effective_handshake_timeout();
        self.arm(TimerKind::Handshake, "", "", t);
    }

    pub(crate) fn on_handshake_timeout(&mut self) {
        if self.stopped || self.status == Status::Online {
            return;
        }
        self.handshake_streak += 1;
        let t = self.effective_handshake_timeout();
        self.log("warn", format!("handshake not completed within {t}ms, forcing reconnect (streak={})", self.handshake_streak));
        self.drop_socket(1006, "handshake timeout");
    }

    /// 强制关掉当前 socket 并按断线处理。
    pub(crate) fn drop_socket(&mut self, code: u16, reason: &str) {
        if self.ws_open {
            self.push(Out::CloseSocket { code: 1000, reason: reason.to_owned() });
        }
        self.ws_open = false;
        self.conn_epoch += 1;
        self.handle_disconnect(Some(code), reason);
    }

    fn handle_disconnect(&mut self, code: Option<u16>, reason: &str) {
        let online_for = self.online_since_wall.map(|t| self.wall_ms.saturating_sub(t));
        self.log("info", format!("device-link disconnected (code={}, reason={}, onlineForMs={})", code.map_or("n/a".into(), |c| c.to_string()), if reason.is_empty() { "n/a" } else { reason }, online_for.map_or("never-online".into(), |v| v.to_string())));
        self.track_short_lived(online_for, code, reason);
        self.online_since_wall = None;
        self.clear_timers();
        self.last_ping_sent_ms = None;
        let socket_err = self.last_socket_error.clone();
        let kind = classify_connection_issue(code, reason, socket_err.as_deref());
        self.reset_link_state_for_reconnect();
        if self.stopped {
            return;
        }
        if code == Some(4410) {
            // 设备已被移除：不再重连
            self.stop_connection_only();
            self.set_issue(Some(Issue { kind: "device-removed", close_code: code, detail: None, at: self.wall_ms }));
            return;
        }
        if code == Some(DUPLICATE_CONNECTION_CLOSE_CODE) {
            self.log("warn", format!("relay replaced this device connection; keeping reconnect backoff warm{}", if reason.is_empty() { String::new() } else { format!(" ({reason})") }));
        }
        if code == Some(RELAY_TRY_AGAIN_LATER_CLOSE_CODE) {
            self.congestion_streak += 1;
            self.log("warn", format!("relay signalled congestion (close=1013); reconnect cool-down engaged (streak={})", self.congestion_streak));
        }
        if let Some(kind) = kind {
            let detail = if !reason.is_empty() { Some(reason.to_owned()) } else { socket_err };
            self.set_issue(Some(Issue { kind, close_code: code, detail, at: self.wall_ms }));
        }
        self.schedule_reconnect();
    }

    /// device-removed：等价上游 stopConnection（保留 issue）。
    fn stop_connection_only(&mut self) {
        self.stopped = true;
        self.clear_timers();
        self.clear_peer_transport();
        self.set_status(Status::Stopped);
    }

    fn track_short_lived(&mut self, online_for: Option<u64>, code: Option<u16>, reason: &str) {
        let Some(online) = online_for else { return };
        if self.stopped {
            return;
        }
        if online >= self.timing.reconnect_stable_reset_ms {
            self.short_lived_streak = 0;
            return;
        }
        self.short_lived_streak += 1;
        if self.short_lived_streak < SHORT_LIVED_STREAK_LIMIT {
            return;
        }
        self.log("warn", format!("device-link keeps dropping after handshake ({} in a row, lastOnlineForMs={online})", self.short_lived_streak));
        let detail = format!("{} short-lived connections; last {online}ms{}", self.short_lived_streak, if reason.is_empty() { String::new() } else { format!(" ({reason})") });
        self.set_issue(Some(Issue { kind: "unstable", close_code: code, detail: Some(detail), at: self.wall_ms }));
    }

    fn schedule_reconnect(&mut self) {
        if self.stopped || self.armed(TimerKind::Reconnect, "", "") {
            return;
        }
        let r = self.rand_f64();
        let delay = reconnect_delay_ms(self.reconnect_attempt, self.congestion_streak, &self.timing, r);
        self.reconnect_attempt += 1;
        self.set_status(Status::Connecting);
        self.log("debug", format!("scheduling device-link reconnect in {delay}ms (attempt={}, congestionStreak={})", self.reconnect_attempt, self.congestion_streak));
        self.arm(TimerKind::Reconnect, "", "", delay);
    }

    pub(crate) fn on_reconnect_timer(&mut self) {
        self.connect("backoff-reconnect");
    }

    fn arm_reconnect_stable(&mut self) {
        let t = self.timing.reconnect_stable_reset_ms;
        self.arm(TimerKind::ReconnectStable, "", "", t);
    }

    pub(crate) fn on_reconnect_stable(&mut self) {
        if self.stopped || self.status != Status::Online {
            return;
        }
        self.reconnect_attempt = 0;
        self.short_lived_streak = 0;
        if self.issue.as_ref().map(|i| i.kind) == Some("unstable") {
            self.set_issue(None);
        }
    }

    fn arm_congestion_stable(&mut self) {
        if self.congestion_streak == 0 {
            self.disarm(TimerKind::CongestionStable, "", "");
            return;
        }
        let t = self.timing.congestion_stable_reset_ms.max(1);
        self.arm(TimerKind::CongestionStable, "", "", t);
    }

    pub(crate) fn on_congestion_stable(&mut self) {
        if self.stopped || self.status != Status::Online {
            return;
        }
        self.congestion_streak = 0;
    }

    // ── 心跳 ──
    fn start_heartbeat(&mut self) {
        self.pong_misses = 0;
        self.last_ping_sent_ms = None;
        self.last_inbound_ms = self.now_ms;
        let t = self.timing.ping_interval_ms;
        self.arm(TimerKind::Ping, "", "", t);
    }

    pub(crate) fn on_ping_timer(&mut self) {
        let interval = self.timing.ping_interval_ms;
        self.arm(TimerKind::Ping, "", "", interval);
        if self.status != Status::Online {
            return;
        }
        let now = self.now_ms;
        if now - self.last_inbound_ms <= interval {
            self.pong_misses = 0;
        }
        self.pong_misses += 1;
        let idle_budget = interval * (self.timing.pong_miss_limit as u64 + 1);
        if self.pong_misses > self.timing.pong_miss_limit && now - self.last_inbound_ms >= idle_budget {
            self.log("warn", format!("heartbeat lost, forcing reconnect (misses={}, idleForMs={})", self.pong_misses, now - self.last_inbound_ms));
            self.drop_socket(1006, "heartbeat lost");
            return;
        }
        if self.send_envelope(&Env::new("ping")).is_ok() {
            self.last_ping_sent_ms = Some(now);
        }
    }

    // ── presence ──
    pub fn send_presence(&mut self, patch: serde_json::Map<String, Value>) {
        if self.status != Status::Online {
            return;
        }
        let cur = self.pending_presence.get_or_insert_with(Default::default);
        for (k, v) in patch {
            cur.insert(k, v);
        }
        self.flush_presence();
    }

    pub(crate) fn flush_presence(&mut self) {
        let Some(patch) = self.pending_presence.clone() else { return };
        if self.status != Status::Online {
            return;
        }
        match self.send_envelope(&Env::new("presence-set").with(Value::Object(patch))) {
            Ok(_) => {
                self.pending_presence = None;
            }
            Err(e) if e.code == "BACKPRESSURE" => {
                if !self.armed(TimerKind::PresenceRetry, "", "") {
                    let t = self.timing.presence_retry_interval_ms;
                    self.arm(TimerKind::PresenceRetry, "", "", t);
                }
            }
            Err(e) if e.code == "NOT_CONNECTED" => {
                self.pending_presence = None;
                self.disarm(TimerKind::PresenceRetry, "", "");
            }
            Err(_) => {}
        }
    }

    /// 连接世代切换时的链路层复位（对应上游 resetLinkStateForReconnect）。
    pub(crate) fn reset_link_state_for_reconnect(&mut self) {
        self.pending_presence = None;
        self.disarm(TimerKind::PresenceRetry, "", "");
        self.ledger.clear();
        let ids: Vec<String> = self.peers.keys().cloned().collect();
        for id in ids {
            self.mark_peer_link_down(&id);
            if let Some(p) = self.peers.get_mut(&id) {
                p.recovery_needs_ack = true;
                p.recovery_frames_sent = 0;
            }
            self.disarm(TimerKind::Retry, &id, "");
        }
        self.pending_offers.clear();
        self.reset_legacy_inbound_queue();
    }

    pub(crate) fn clear_peer_transport(&mut self) {
        let ids: Vec<String> = self.peers.keys().cloned().collect();
        for id in &ids {
            for k in [TimerKind::Retry, TimerKind::LinkConfirm, TimerKind::TimeoutCloseNotify] {
                self.disarm(k, id, "");
            }
        }
        self.peers.clear();
        self.ledger.clear();
    }

    pub(crate) fn reset_legacy_inbound_queue(&mut self) {}

    // ── hello-ack ──
    pub(crate) fn on_hello_ack(&mut self, payload: &Value) {
        let ver = payload.get("serverProtocolVersion").and_then(Value::as_f64);
        if let Some(v) = ver {
            if v != PROTOCOL_VERSION as f64 {
                self.log("error", format!("device-link protocol mismatch: server v{v}, client v{PROTOCOL_VERSION}; staying offline"));
                self.set_issue(Some(Issue { kind: "version-mismatch", close_code: None, detail: Some(format!("server v{v}, client v{PROTOCOL_VERSION}")), at: self.wall_ms }));
                self.drop_socket(4400, "protocol version mismatch");
                return;
            }
        }
        if self.issue.as_ref().map(|i| i.kind) != Some("unstable") {
            self.set_issue(None);
        }
        self.server_capabilities = payload.get("capabilities").and_then(Value::as_array).map(|a| a.iter().filter_map(|c| c.as_str().map(str::to_owned)).collect()).unwrap_or_default();
        if let Some(id) = payload.get("deviceId").and_then(Value::as_str).filter(|s| !s.is_empty()) {
            self.self_device_id = Some(id.to_owned());
        }
        self.disarm(TimerKind::Handshake, "", "");
        self.handshake_streak = 0;
        let was_online = self.status == Status::Online;
        self.set_status(Status::Online);
        if !was_online {
            self.online_since_wall = Some(self.wall_ms);
        }
        self.arm_reconnect_stable();
        self.arm_congestion_stable();
        self.start_heartbeat();
        if was_online {
            self.log("info", format!("duplicate hello-ack while already online (protocol=v{})", ver.unwrap_or(0.0)));
        } else {
            self.log("info", format!("device-link online (protocol=v{}, elapsedMs={})", ver.unwrap_or(0.0), self.now_ms.saturating_sub(self.connection_started_ms)));
        }
    }

    pub(crate) fn on_pong(&mut self) {
        self.last_ping_sent_ms = None;
        self.pong_misses = 0;
    }

    pub(crate) fn json_hello_default() -> Value {
        json!({})
    }
}
