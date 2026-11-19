//! 接收侧：入站帧校验与分发、可靠帧重组与有序交付、ACK。
use super::peer::{short, Assembly, Phase, Ready, RecvStream};
use super::send::is_reliable_kind;
use super::types::*;
use super::{Engine, TimerKind};
use crate::device_link::transport::*;
use serde_json::Value;
use std::collections::BTreeMap;

const MAX_REASSEMBLIES: usize = 16;
const RELIABLE_ACK_BATCH_MESSAGES: u64 = 2;
const RELIABLE_ACK_BATCH_BYTES: u64 = 256 * 1024;
const KNOWN_KINDS: &[&str] = &["hello", "hello-ack", "presence-set", "presence-changed", "ping", "pong", "notify", "link-open", "link-accept", "link-close", "invoke", "invoke-result", "push", "relay-error"];
const ROUTED: &[&str] = &["link-open", "link-accept", "link-close", "invoke", "invoke-result", "push"];

fn is_record(v: &Value) -> bool {
    v.is_object()
}

fn str_nonempty(v: Option<&Value>) -> bool {
    v.and_then(Value::as_str).is_some_and(|s| !s.is_empty())
}

fn finite_num(v: Option<&Value>) -> bool {
    v.and_then(Value::as_f64).is_some_and(f64::is_finite)
}

/// 入站信封的轻量活性校验（上游 isValidInboundEnvelope）。
pub(crate) fn valid_for_heartbeat(env: &Env) -> bool {
    if is_reliable_kind(&env.kind) {
        if let Some(payload) = &env.payload {
            if let Some(frame) = parse_transport_payload(payload) {
                if env.kind != "invoke" {
                    return true;
                }
                if !env.src.as_deref().is_some_and(|s| !s.is_empty()) || !env.id.as_deref().is_some_and(|s| !s.is_empty()) {
                    return false;
                }
                if frame.meta.segment.is_some() {
                    return true;
                }
                return match serde_json::from_str::<Value>(&frame.data) {
                    Ok(inner) => is_skip_payload(&inner) || (inner.get("channel").is_some_and(Value::is_string) && inner.get("args").is_some_and(Value::is_array)),
                    Err(_) => false,
                };
            }
        }
    }
    let payload = env.payload.as_ref();
    let rec = payload.filter(|p| is_record(p));
    match env.kind.as_str() {
        "ping" | "pong" => payload.map_or(true, Value::is_null),
        "hello-ack" => rec.is_some_and(|p| finite_num(p.get("serverProtocolVersion")) && str_nonempty(p.get("deviceId")) && str_nonempty(p.get("userId"))),
        "presence-changed" => rec.is_some_and(|p| str_nonempty(p.get("deviceId")) && p.get("online").is_some_and(Value::is_boolean)),
        "relay-error" => rec.is_some_and(|p| p.get("code").is_some_and(Value::is_string) && p.get("message").is_some_and(Value::is_string)),
        "invoke" => env.src.as_deref().is_some_and(|s| !s.is_empty()) && env.id.as_deref().is_some_and(|s| !s.is_empty()) && rec.is_some_and(|p| p.get("channel").is_some_and(Value::is_string) && p.get("args").is_some_and(Value::is_array)),
        "invoke-result" => match rec {
            Some(p) if p.get("ok").is_some_and(Value::is_boolean) => {
                if p.get("ok") == Some(&Value::Bool(true)) {
                    true
                } else {
                    p.get("error").is_some_and(|e| e.is_object() && e.get("code").is_some_and(Value::is_string) && e.get("message").is_some_and(Value::is_string))
                }
            }
            _ => false,
        },
        "push" => rec.is_some_and(|p| p.get("channel").is_some_and(Value::is_string) && p.as_object().unwrap().contains_key("payload")),
        "link-open" => rec.is_some_and(|p| p.get("controllerName").is_some_and(Value::is_string) && finite_num(p.get("protocolVersion")) && p.get("appVersion").is_some_and(Value::is_string)),
        "link-accept" => rec.is_some_and(|p| p.get("appVersion").is_some_and(Value::is_string) && p.get("allowlistHash").is_some_and(Value::is_string)),
        "link-close" => rec.is_some_and(|p| p.get("reason").is_some_and(Value::is_string)),
        "presence-set" => rec.is_some_and(|p| p.get("remoteControlEnabled").map_or(true, Value::is_boolean) && p.get("busy").map_or(true, Value::is_boolean)),
        "notify" => rec.is_some_and(|p| ["category", "title", "deepLink", "collapseId"].iter().all(|k| p.get(*k).is_some_and(Value::is_string))),
        "hello" => rec.is_some(),
        _ => false,
    }
}

impl Engine {
    /// 运行时收到一个文本帧。
    pub fn on_message(&mut self, epoch: u64, raw: &str) {
        if epoch != self.conn_epoch {
            return;
        }
        let Ok(parsed) = serde_json::from_str::<Value>(raw) else {
            self.log("warn", "dropping unparseable frame".into());
            return;
        };
        let known = parsed.get("v").and_then(Value::as_f64) == Some(1.0) && parsed.get("kind").and_then(Value::as_str).is_some_and(|k| KNOWN_KINDS.contains(&k));
        let Some(env) = Env::from_value(&parsed).filter(|_| known) else {
            self.log("warn", "dropping invalid device-link frame".into());
            return;
        };
        // 被控端不声明 sharedTask / providerShare 作用域，带作用域的帧不是发给我们的
        if env.extra.contains_key("sharedTask") || env.extra.contains_key("providerShare") {
            self.log("warn", "dropping invalid device-link routing scope".into());
            return;
        }
        let valid = valid_for_heartbeat(&env);
        if !valid && env.kind != "invoke" && env.kind != "link-close" {
            self.log("warn", format!("dropping invalid device-link frame kind={}", env.kind));
            return;
        }
        if valid {
            self.last_inbound_ms = self.now_ms;
            if env.src.is_some() && ROUTED.contains(&env.kind.as_str()) {
                self.last_peer_inbound_ms = self.last_inbound_ms;
            }
        }
        if let Some(ack) = parse_transport_ack(&parsed) {
            if let Some(src) = env.src.clone() {
                self.handle_transport_ack(&src, &ack);
            }
            return;
        }
        if self.ingest_transport_envelope(&env) {
            return;
        }
        self.dispatch_envelope(env);
    }

    /// 业务帧交给应用层；其余在引擎内处理。
    pub(crate) fn dispatch_envelope(&mut self, env: Env) -> bool {
        match env.kind.as_str() {
            "hello-ack" => {
                let payload = env.payload.clone().unwrap_or(Value::Null);
                self.on_hello_ack(&payload);
                true
            }
            "pong" => {
                self.on_pong();
                true
            }
            "presence-changed" => {
                if let Some(p) = env.payload.clone() {
                    self.push(Out::PresenceChanged(p));
                }
                true
            }
            "relay-error" => {
                self.on_relay_error(&env);
                self.push(Out::App(env));
                true
            }
            "invoke-result" | "link-accept" => {
                // 被控端不发起请求，没有等待配对的 pending；这类帧原样交给应用层（与上游未配对分支一致）
                self.push(Out::App(env));
                true
            }
            _ => {
                match env.kind.as_str() {
                    "link-open" => self.on_link_open_frame(&env),
                    "link-close" => {
                        if self.on_link_close_frame(&env) {
                            return true;
                        }
                    }
                    "invoke" => self.on_invoke_frame(&env),
                    _ => {}
                }
                self.push(Out::App(env));
                true
            }
        }
    }

    fn on_relay_error(&mut self, env: &Env) {
        let payload = env.payload.clone().unwrap_or(Value::Null);
        let code = payload.get("code").and_then(Value::as_str).unwrap_or("").to_owned();
        let message = payload.get("message").and_then(Value::as_str).unwrap_or("").to_owned();
        let dst = payload.get("dst").and_then(Value::as_str).map(str::to_owned);
        let route_device = dst.clone();
        let remembered = env.id.as_ref().and_then(|id| self.ledger.consume(id, route_device.as_deref()));
        let released_or_settled = env.id.is_some() && remembered.is_none();
        let route_generation = remembered.or_else(|| route_device.as_deref().map(|d| self.peer_link_generation(d))).unwrap_or(0);
        let stale = route_device.as_deref().is_some_and(|d| released_or_settled || route_generation < self.peer_link_generation(d));
        let terminal = code == "REMOTE_DISABLED" || code == "DEVICE_OFFLINE";
        if let Some(d) = dst.as_deref() {
            if terminal && !stale {
                self.mark_peer_link_down(d);
                if let Some(p) = self.peers.get_mut(d) {
                    p.recovery_needs_ack = true;
                    p.recovery_frames_sent = 0;
                }
                self.disarm(TimerKind::Retry, d, "");
            } else if let (Some(id), true) = (&env.id, code == "BAD_REQUEST" || code == "PAYLOAD_TOO_LARGE") {
                self.drop_reliable_pending_for_request(d, id);
            }
        }
        if code == "VERSION_MISMATCH" {
            self.set_issue(Some(Issue { kind: "version-mismatch", close_code: None, detail: Some(message.clone()), at: self.wall_ms }));
        }
        self.log("warn", format!("relay-error: [{code}] {message}{}", dst.map_or(String::new(), |d| format!(" dst={}", short(&d)))));
    }

    // ── 入站可靠帧 ──
    /// 返回 true = 已由传输层处理（不再走 dispatch）。
    fn ingest_transport_envelope(&mut self, env: &Env) -> bool {
        let Some(payload) = &env.payload else { return false };
        let Some(parsed) = parse_transport_payload(payload) else { return false };
        let Some(src) = env.src.clone() else { return false };
        if !is_reliable_kind(&env.kind) {
            return false;
        }
        let (reliable, receive_ready, remote_stream) = {
            let p = self.peer(&src);
            (p.reliable, p.receive_ready, p.remote_stream_id.clone())
        };
        if !reliable || !receive_ready {
            self.log("debug", format!("dropping reliable payload before link is ready from {}", short(&src)));
            return true;
        }
        if let Some(rs) = &remote_stream {
            if rs != &parsed.meta.stream_id {
                self.log("debug", format!("dropping stale reliable stream from {} expected={} got={}", short(&src), short(rs), short(&parsed.meta.stream_id)));
                return true;
            }
        }
        if remote_stream.is_none() {
            self.peer(&src).remote_stream_id = Some(parsed.meta.stream_id.clone());
        }
        let meta = parsed.meta.clone();
        let remote_base = self.peer(&src).remote_base_seq;
        self.get_receive_stream(&src, &meta.stream_id, remote_base.max(meta.base_seq));
        let is_skip = meta.segment.is_none() && serde_json::from_str::<Value>(&parsed.data).map(|v| is_skip_payload(&v)).unwrap_or(false);
        let last = self.stream(&src, &meta.stream_id).last_delivered_seq;
        if meta.seq <= last {
            self.send_transport_ack(&src, &meta.stream_id, last, None, false);
            return true;
        }
        if meta.seq > last + MAX_SEQUENCE_WINDOW {
            self.log("warn", format!("dropping reliable payload reason=receive_window src={} seq={}", short(&src), meta.seq));
            self.send_transport_ack(&src, &meta.stream_id, last, None, false);
            return true;
        }
        if self.stream(&src, &meta.stream_id).ready.contains_key(&meta.seq) && !is_skip {
            self.send_transport_ack(&src, &meta.stream_id, last, None, false);
            self.drain_transport_stream(&src, &meta.stream_id);
            return true;
        }
        match &meta.segment {
            None => {
                let bytes = byte_length(&parsed.data);
                if is_skip {
                    self.remove_receive_entry(&src, &meta.stream_id, meta.seq);
                } else if self.stream(&src, &meta.stream_id).assemblies.contains_key(&meta.seq) {
                    self.log("warn", format!("dropping reliable payload with changed segment shape seq={}", meta.seq));
                    self.send_transport_ack(&src, &meta.stream_id, last, None, false);
                    return true;
                }
                let rejection = if bytes > MAX_CHUNK_BYTES { Some("chunk_too_large") } else if !self.ensure_receive_capacity(&src, &meta.stream_id, meta.seq, bytes, false) { Some("receive_capacity_exceeded") } else { None };
                if let Some(r) = rejection {
                    self.log("warn", format!("dropping reliable payload reason={r} src={} seq={} incomingBytes={bytes}", short(&src), meta.seq));
                    self.send_transport_ack(&src, &meta.stream_id, last, None, false);
                    return true;
                }
                if serde_json::from_str::<Value>(&parsed.data).is_err() {
                    self.log("warn", format!("dropping invalid reliable payload seq={}", meta.seq));
                    return true;
                }
                let s = self.stream(&src, &meta.stream_id);
                s.ready.insert(meta.seq, Ready { env: env.clone(), json: parsed.data.clone() });
                s.buffered_bytes += bytes;
            }
            Some(seg) => {
                if seg.total_bytes > MAX_REASSEMBLY_BYTES {
                    self.log("warn", format!("dropping oversized reliable reassembly seq={}", meta.seq));
                    return true;
                }
                let now = self.now_ms;
                let exists = self.stream(&src, &meta.stream_id).assemblies.contains_key(&meta.seq);
                let mut assembly = if exists {
                    self.stream(&src, &meta.stream_id).assemblies.get(&meta.seq).cloned().unwrap()
                } else {
                    Assembly { received_at: now, kind: env.kind.clone(), id: env.id.clone(), src: env.src.clone(), dst: env.dst.clone(), total: seg.total, total_bytes: seg.total_bytes, chunks: BTreeMap::new(), bytes: 0 }
                };
                if assembly.kind != env.kind || assembly.id != env.id || assembly.src != env.src || assembly.dst != env.dst || assembly.total != seg.total || assembly.total_bytes != seg.total_bytes {
                    let s = self.stream(&src, &meta.stream_id);
                    s.buffered_bytes = s.buffered_bytes.saturating_sub(assembly.bytes);
                    s.assemblies.remove(&meta.seq);
                    self.log("warn", format!("dropping reliable payload with changed segment metadata seq={}", meta.seq));
                    self.send_transport_ack(&src, &meta.stream_id, last, None, false);
                    return true;
                }
                if !assembly.chunks.contains_key(&seg.index) {
                    let bytes = byte_length(&parsed.data);
                    let rejection = if bytes > MAX_CHUNK_BYTES {
                        Some("chunk_too_large")
                    } else if assembly.bytes + bytes > assembly.total_bytes {
                        Some("declared_size_exceeded")
                    } else if !self.ensure_receive_capacity(&src, &meta.stream_id, meta.seq, bytes, true) {
                        Some("receive_capacity_exceeded")
                    } else {
                        None
                    };
                    if let Some(r) = rejection {
                        self.log("warn", format!("dropping reliable payload reason={r} src={} seq={} segment={}/{}", short(&src), meta.seq, seg.index, seg.total));
                        self.remove_receive_entry(&src, &meta.stream_id, meta.seq);
                        return true;
                    }
                    assembly.chunks.insert(seg.index, parsed.data.clone());
                    assembly.bytes += bytes;
                    self.stream(&src, &meta.stream_id).buffered_bytes += bytes;
                }
                if assembly.chunks.len() == assembly.total {
                    let json: String = (0..assembly.total).map(|i| assembly.chunks.get(&i).cloned().unwrap_or_default()).collect();
                    self.stream(&src, &meta.stream_id).assemblies.remove(&meta.seq);
                    let ok = byte_length(&json) == assembly.total_bytes && serde_json::from_str::<Value>(&json).is_ok();
                    if !ok {
                        let s = self.stream(&src, &meta.stream_id);
                        s.buffered_bytes = s.buffered_bytes.saturating_sub(assembly.bytes);
                        self.log("warn", format!("dropping invalid reliable reassembly seq={}", meta.seq));
                        self.send_transport_ack(&src, &meta.stream_id, last, None, false);
                        return true;
                    }
                    self.stream(&src, &meta.stream_id).ready.insert(meta.seq, Ready { env: env.clone(), json });
                } else {
                    self.stream(&src, &meta.stream_id).assemblies.insert(meta.seq, assembly);
                }
            }
        }
        self.drain_transport_stream(&src, &meta.stream_id);
        true
    }

    pub(crate) fn stream(&mut self, src: &str, stream_id: &str) -> &mut RecvStream {
        self.peer(src).receive.entry(stream_id.to_owned()).or_insert_with(|| RecvStream::new(1))
    }

    pub(crate) fn get_receive_stream(&mut self, src: &str, stream_id: &str, base_seq: u64) {
        let p = self.peer(src);
        if let Some(s) = p.receive.get_mut(stream_id) {
            s.requested_base_seq = s.requested_base_seq.max(base_seq);
            Self::apply_base(s);
            return;
        }
        if p.receive.len() >= MAX_REASSEMBLIES {
            p.receive.shift_remove_index(0);
        }
        p.receive.insert(stream_id.to_owned(), RecvStream::new(base_seq));
    }

    fn apply_base(s: &mut RecvStream) {
        let target = s.requested_base_seq.saturating_sub(1);
        if target <= s.last_delivered_seq {
            return;
        }
        let base = s.requested_base_seq;
        let old_a: Vec<u64> = s.assemblies.keys().copied().filter(|k| *k < base).collect();
        for k in old_a {
            if let Some(a) = s.assemblies.remove(&k) {
                s.buffered_bytes = s.buffered_bytes.saturating_sub(a.bytes);
            }
        }
        let old_r: Vec<u64> = s.ready.keys().copied().filter(|k| *k < base).collect();
        for k in old_r {
            if let Some(r) = s.ready.remove(&k) {
                s.buffered_bytes = s.buffered_bytes.saturating_sub(byte_length(&r.json));
            }
        }
        s.last_delivered_seq = target;
    }

    fn remove_receive_entry(&mut self, src: &str, stream_id: &str, seq: u64) {
        let s = self.stream(src, stream_id);
        if let Some(a) = s.assemblies.remove(&seq) {
            s.buffered_bytes = s.buffered_bytes.saturating_sub(a.bytes);
        }
        if let Some(r) = s.ready.remove(&seq) {
            s.buffered_bytes = s.buffered_bytes.saturating_sub(byte_length(&r.json));
        }
    }

    fn ensure_receive_capacity(&mut self, src: &str, stream_id: &str, seq: u64, additional: usize, assembling: bool) -> bool {
        let fits = |s: &RecvStream| {
            let has_slot = s.ready.contains_key(&seq) || s.assemblies.contains_key(&seq);
            let slots = s.ready.len() + s.assemblies.len() + usize::from(!has_slot);
            let assemblies = s.assemblies.len() + usize::from(assembling && !s.assemblies.contains_key(&seq));
            slots as u64 <= MAX_SEQUENCE_WINDOW && assemblies <= MAX_REASSEMBLIES && s.buffered_bytes + additional <= MAX_REASSEMBLY_BYTES
        };
        if fits(self.stream(src, stream_id)) {
            return true;
        }
        if seq != self.stream(src, stream_id).last_delivered_seq + 1 {
            return false;
        }
        loop {
            if fits(self.stream(src, stream_id)) {
                return true;
            }
            let s = self.stream(src, stream_id);
            let max_future = s.ready.keys().chain(s.assemblies.keys()).copied().filter(|k| *k > seq).max();
            let Some(f) = max_future else { return false };
            self.remove_receive_entry(src, stream_id, f);
        }
    }

    fn is_stream_active(&self, src: &str, stream_id: &str) -> bool {
        self.peers.get(src).is_some_and(|p| p.reliable && p.receive_ready && p.remote_stream_id.as_deref() == Some(stream_id) && p.receive.contains_key(stream_id))
    }

    /// 按 seq 连续交付；只有应用层接受后才推进累计 ACK。
    pub(crate) fn drain_transport_stream(&mut self, src: &str, stream_id: &str) {
        let delay_ack = self.peers.get(src).is_some_and(|p| p.accepts_ack_v2);
        let mut delivered_since = 0u64;
        let mut bytes_since = 0u64;
        let mut last_ack_sent = self.stream(src, stream_id).last_delivered_seq;
        let mut sent_ack = false;
        loop {
            Self::apply_base(self.stream(src, stream_id));
            let mut progressed = false;
            loop {
                let nxt = self.stream(src, stream_id).last_delivered_seq + 1;
                if !self.stream(src, stream_id).ready.contains_key(&nxt) {
                    break;
                }
                if !self.is_stream_active(src, stream_id) {
                    break;
                }
                let next = self.stream(src, stream_id).last_delivered_seq + 1;
                let ready = self.stream(src, stream_id).ready.get(&next).cloned().unwrap();
                let delivered_bytes = byte_length(&ready.json) as u64;
                let Ok(payload) = serde_json::from_str::<Value>(&ready.json) else {
                    self.log("warn", format!("dropping reliable payload decode failure seq={next}"));
                    break;
                };
                let logical = Env { payload: Some(payload.clone()), ..ready.env.clone() };
                let handled = if is_skip_payload(&payload) { true } else { self.dispatch_envelope(logical.clone()) };
                if !handled {
                    Self::apply_base(self.stream(src, stream_id));
                    if self.stream(src, stream_id).last_delivered_seq >= next {
                        continue;
                    }
                    self.log("warn", format!("reliable payload handler failed seq={next}; waiting for retry"));
                    break;
                }
                let s = self.stream(src, stream_id);
                s.ready.remove(&next);
                s.buffered_bytes = s.buffered_bytes.saturating_sub(delivered_bytes as usize);
                s.last_delivered_seq = next;
                Self::apply_base(s);
                delivered_since += 1;
                bytes_since += delivered_bytes;
                s.unacked_messages += 1;
                s.unacked_bytes += delivered_bytes;
                if logical.kind != "push" {
                    s.unacked_non_push = true;
                }
                let delay_this = delay_ack && !s.unacked_non_push;
                let (um, ub) = (s.unacked_messages, s.unacked_bytes);
                let flush = if delay_this { um >= DELAYED_ACK_MESSAGES || ub >= DELAYED_ACK_BYTES } else { delivered_since >= RELIABLE_ACK_BATCH_MESSAGES || bytes_since >= RELIABLE_ACK_BATCH_BYTES };
                if flush && self.is_stream_active(src, stream_id) {
                    let seq = self.stream(src, stream_id).last_delivered_seq;
                    if self.send_transport_ack(src, stream_id, seq, None, false) {
                        last_ack_sent = seq;
                        sent_ack = true;
                    }
                    delivered_since = 0;
                    bytes_since = 0;
                }
                progressed = true;
            }
            let _ = progressed;
            break;
        }
        if self.is_stream_active(src, stream_id) {
            let last = self.stream(src, stream_id).last_delivered_seq;
            if !sent_ack || last != last_ack_sent {
                let (non_push, um) = {
                    let s = self.stream(src, stream_id);
                    (s.unacked_non_push, s.unacked_messages)
                };
                if delay_ack && !non_push && um > 0 {
                    self.schedule_delayed_ack(src, stream_id);
                } else {
                    self.send_transport_ack(src, stream_id, last, None, false);
                }
            }
        }
    }

    pub(crate) fn schedule_delayed_ack(&mut self, src: &str, stream_id: &str) {
        if self.stream(src, stream_id).delayed_ack_armed {
            return;
        }
        self.stream(src, stream_id).delayed_ack_armed = true;
        self.arm(TimerKind::DelayedAck, src, stream_id, DELAYED_ACK_MS);
    }

    pub(crate) fn on_delayed_ack(&mut self, src: &str, stream_id: &str) {
        if !self.peers.get(src).is_some_and(|p| p.receive.contains_key(stream_id)) {
            return;
        }
        self.stream(src, stream_id).delayed_ack_armed = false;
        if self.stopped || !self.is_stream_active(src, stream_id) {
            return;
        }
        let last = self.stream(src, stream_id).last_delivered_seq;
        self.send_transport_ack(src, stream_id, last, None, true);
    }

    /// 发累计 ACK（best-effort）。`from_delay_timer` 标记它覆盖过被延后的消息。
    pub(crate) fn send_transport_ack(&mut self, dst: &str, stream_id: &str, ack_seq: u64, link_request_id: Option<&str>, from_delay_timer: bool) -> bool {
        let accepts_v2 = self.peer(dst).accepts_ack_v2;
        // 被控端不发起 link-open，没有「出站链路确认 ACK」状态：只在调用方给了 link id 时带上
        let effective_link = link_request_id.map(str::to_owned);
        let covers_delayed = self.peers.get(dst).and_then(|p| p.receive.get(stream_id)).is_some_and(|s| s.delayed_ack_armed) || from_delay_timer;
        let compact = link_request_id.is_none() && accepts_v2;
        let ack = make_transport_ack(dst, stream_id, ack_seq, effective_link.as_deref(), compact, covers_delayed);
        let Some(env) = Env::from_value(&ack) else { return false };
        match self.send_best_effort(&env) {
            Ok(()) => {
                // 清掉被这条 ACK 覆盖的延后计数
                if let Some(s) = self.peers.get_mut(dst).and_then(|p| p.receive.get_mut(stream_id)) {
                    if ack_seq >= s.last_delivered_seq {
                        let armed = s.delayed_ack_armed;
                        s.delayed_ack_armed = false;
                        s.unacked_messages = 0;
                        s.unacked_bytes = 0;
                        s.unacked_non_push = false;
                        if armed {
                            self.disarm(TimerKind::DelayedAck, dst, stream_id);
                        }
                    }
                }
                true
            }
            Err(e) => {
                self.log("debug", format!("reliable transport ACK send failed dst={} stream={} ack={ack_seq}: {}", short(dst), short(stream_id), e.message));
                false
            }
        }
    }

    // ── 收到对端 ACK ──
    pub(crate) fn handle_transport_ack(&mut self, src: &str, ack: &Ack) {
        let Some(p) = self.peers.get(src) else { return };
        if !p.reliable {
            return;
        }
        if ack.stream_prefix && !p.accepts_ack_v2 {
            return;
        }
        if p.stream_id != ack.stream_id && !(ack.stream_prefix && p.stream_id.starts_with(&ack.stream_id)) {
            return;
        }
        let ack_seq = ack.ack_seq;
        let same_link = |expected: &str, received: Option<&String>, prefix: bool| received.is_some_and(|r| r == expected || (prefix && r.chars().count() >= 8 && expected.starts_with(r.as_str())));
        let confirm = p.pending_link_confirmation.clone();
        if let Some(c) = confirm {
            let (phase, next_seq, sid, gen) = {
                let p = self.peers.get(src).unwrap();
                (p.send_phase, p.next_seq, p.stream_id.clone(), p.link_generation)
            };
            if phase == Phase::AwaitingConfirm && same_link(&c.request_id, ack.link_request_id.as_ref(), ack.stream_prefix) && ack_seq >= c.minimum_ack_seq && ack_seq + 1 <= next_seq {
                self.log("info", format!("device-link recovery dst={} trigger=link-confirm-ack stream={} request={} ack={ack_seq}", short(src), short(&sid), short(&c.request_id)));
                self.ledger.discard_before(src, gen);
                self.ledger.discard_for_id(src, &c.request_id);
                self.commit_send_resume(src, c.resume);
            }
        }
        if !self.peers.get(src).is_some_and(|p| p.send_ready()) {
            return;
        }
        self.repair_confirmed_receive_gap(src, ack_seq, ack.link_request_id.as_ref(), ack.stream_prefix);
        let (next_seq, highest) = {
            let p = self.peers.get(src).unwrap();
            (p.next_seq, p.highest_ack_seq)
        };
        if ack_seq + 1 > next_seq || ack_seq <= highest {
            return;
        }
        self.peers.get_mut(src).unwrap().highest_ack_seq = ack_seq;
        let acknowledged_at = self.now_ms;
        let mut rtt_sample: Option<f64> = None;
        let mut delivery_start = acknowledged_at;
        let mut delivery_bytes = 0usize;
        let mut delivery_valid = true;
        let seqs: Vec<u64> = self.peers.get(src).unwrap().pending.keys().copied().take_while(|s| *s <= ack_seq).collect();
        for seq in seqs {
            let x = self.peers.get(src).unwrap().pending.get(&seq).cloned().unwrap();
            if let Some(id) = &x.env.id {
                self.ledger.discard_for_id(src, id);
            }
            if !ack.delayed && x.first_sent_at.is_some() && x.bytes <= 16 * 1024 {
                rtt_sample = Some((self.now_ms - x.first_sent_at.unwrap()) as f64);
            }
            if x.first_sent_at.is_some() && x.attempts == 1 {
                delivery_start = delivery_start.min(x.first_sent_at.unwrap());
                delivery_bytes += x.bytes;
            } else {
                delivery_valid = false;
            }
            let p = self.peers.get_mut(src).unwrap();
            p.pending.remove(&seq);
            p.pending_bytes -= x.bytes;
        }
        if delivery_valid && !ack.delayed {
            let p = self.peers.get_mut(src).unwrap();
            update_delivery_estimate(&mut p.rtt, delivery_bytes as f64, (acknowledged_at - delivery_start) as f64);
        }
        if let Some(s) = rtt_sample {
            update_retransmit_estimate(&mut self.peers.get_mut(src).unwrap().rtt, s);
        }
        let needs = self.peers.get(src).unwrap().recovery_needs_ack;
        if needs {
            let p = self.peers.get_mut(src).unwrap();
            p.confirmed_gap_repair = None;
            p.recovery_needs_ack = false;
            p.recovery_frames_sent = 0;
            self.retry_pending(src, false, false);
        } else {
            let repair = self.peers.get(src).unwrap().confirmed_gap_repair.clone();
            if let Some(r) = repair {
                if ack_seq >= r.awaiting_seq {
                    self.send_confirmed_gap_head(src, r.through_seq);
                }
            }
            self.retry_pending(src, false, true);
        }
        if self.peers.get(src).unwrap().pending.is_empty() {
            self.disarm(TimerKind::Retry, src, "");
        }
    }

    fn repair_confirmed_receive_gap(&mut self, dst: &str, ack_seq: u64, link: Option<&String>, prefix: bool) {
        let Some(repair) = self.peers.get(dst).and_then(|p| p.duplicate_open_repair.clone()) else { return };
        let matches = link.is_some_and(|r| r == &repair.request_id || (prefix && r.chars().count() >= 8 && repair.request_id.starts_with(r.as_str())));
        if !matches {
            return;
        }
        self.peers.get_mut(dst).unwrap().duplicate_open_repair = None;
        if self.peers.get(dst).unwrap().confirmed_gap_repair.is_some() {
            return;
        }
        let p = self.peers.get(dst).unwrap();
        let head = p.pending.values().next();
        let now = self.now_ms;
        let recently = p.last_duplicate_open_repair_at.is_some_and(|t| now - t < self.timing.transport_retry_interval_ms);
        let Some(head) = head else { return };
        if !head.sent || head.seq != repair.head_seq || ack_seq + 1 != head.seq || ack_seq < p.highest_ack_seq || recently {
            return;
        }
        self.send_confirmed_gap_head(dst, repair.through_seq);
    }

    fn send_confirmed_gap_head(&mut self, dst: &str, through_seq: u64) {
        self.peers.get_mut(dst).unwrap().confirmed_gap_repair = None;
        let Some(head) = self.peers.get(dst).and_then(|p| p.pending.values().next().cloned()) else { return };
        let frame_count = Self::estimate_frame_count(head.bytes);
        let last_repair = self.peers.get(dst).unwrap().last_duplicate_open_repair_seq;
        if !head.sent || head.seq > through_seq || last_repair >= head.seq || head.attempts >= self.timing.transport_max_retry_attempts || frame_count > self.recovery_pass_budget() || self.should_hold_recovery(dst, frame_count) {
            return;
        }
        match self.send_reliable_frames(dst, head.seq) {
            Ok(0) => {}
            Ok(frames) => {
                let now = self.now_ms;
                let p = self.peers.get_mut(dst).unwrap();
                p.last_duplicate_open_repair_seq = head.seq;
                p.last_duplicate_open_repair_at = Some(now);
                if head.seq < through_seq {
                    p.confirmed_gap_repair = Some(super::peer::GapRepair { awaiting_seq: head.seq, through_seq });
                }
                self.note_recovery_frames(dst, frames);
            }
            Err(e) => self.log("debug", format!("device-link confirmed gap repair deferred dst={}: {}", short(dst), e.message)),
        }
    }
}
