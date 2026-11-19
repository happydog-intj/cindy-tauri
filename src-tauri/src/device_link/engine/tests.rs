//! 引擎行为测试：用脚本化的「控制端」驱动状态机。
use super::*;
use crate::device_link::transport::*;
use serde_json::{json, Value};

const ME: &str = "desktop-1";
const PHONE: &str = "phone-1";

struct H {
    e: Engine,
    t: u64,
    epoch: u64,
    writes: Vec<Value>,
    app: Vec<Env>,
    statuses: Vec<Status>,
}

impl H {
    fn new() -> Self {
        let mut e = Engine::new(Timing::default(), 7);
        e.set_hello(json!({ "deviceName": "t", "platform": "darwin", "appVersion": "1", "remoteControlEnabled": true, "busy": false, "capabilities": [] }));
        H { e, t: 1_000, epoch: 0, writes: vec![], app: vec![], statuses: vec![] }
    }
    fn pump(&mut self) -> Vec<Out> {
        let outs = self.e.drain();
        for o in &outs {
            match o {
                Out::Write(s) => self.writes.push(serde_json::from_str(s).unwrap()),
                Out::App(env) => self.app.push(env.clone()),
                Out::Status(s) => self.statuses.push(*s),
                Out::RequestToken { epoch } => self.epoch = *epoch,
                _ => {}
            }
            // 模拟 socket 写完：缓冲清零
        }
        self.e.set_buffered(0);
        outs
    }
    fn advance(&mut self, ms: u64) {
        self.t += ms;
        self.e.set_clock(self.t, 1_700_000_000_000 + self.t);
        self.e.tick();
        self.pump();
    }
    fn online(&mut self) {
        self.e.set_clock(self.t, 1_700_000_000_000 + self.t);
        self.e.start();
        let outs = self.pump();
        assert!(outs.iter().any(|o| matches!(o, Out::RequestToken { .. })));
        self.e.on_token(self.epoch, Some("tok".into()));
        let outs = self.pump();
        assert!(outs.iter().any(|o| matches!(o, Out::OpenSocket { token, .. } if token == "tok")));
        self.e.on_socket_open(self.epoch);
        self.pump();
        assert_eq!(self.writes.last().unwrap()["kind"], "hello");
        self.recv(json!({ "v": 1, "kind": "hello-ack", "payload": { "serverProtocolVersion": 1, "deviceId": ME, "userId": "u1", "capabilities": [] } }));
        assert_eq!(self.e.status(), Status::Online);
        self.writes.clear();
    }
    fn recv(&mut self, v: Value) {
        self.e.set_clock(self.t, 1_700_000_000_000 + self.t);
        self.e.on_message(self.epoch, &v.to_string());
        self.pump();
    }
    fn link_open(&mut self, reliable: bool, id: &str) {
        let mut caps = vec![];
        if reliable {
            caps.extend([CAP_RELIABLE_TRANSPORT, CAP_RELIABLE_LINK_CONFIRM, CAP_TRANSPORT_TIMEOUT_CLOSE, CAP_TRANSPORT_ACK_V2]);
        }
        let mut payload = json!({ "controllerName": "P", "protocolVersion": 1, "appVersion": "9", "capabilities": caps });
        if reliable {
            payload["transportStreamId"] = json!("phone-stream-0001");
            payload["transportBaseSeq"] = json!(1);
        }
        self.recv(json!({ "v": 1, "kind": "link-open", "id": id, "src": PHONE, "dst": ME, "payload": payload }));
        self.e.set_clock(self.t, 1_700_000_000_000 + self.t);
        self.e.send_link_accept(PHONE, id, json!({ "appVersion": "1", "allowlistHash": "abc", "capabilities": [CAP_HISTORY_VIEW_V1] })).unwrap();
        self.pump();
    }
    /// 控制端发来一个可靠 invoke（单帧）。
    fn reliable_invoke(&mut self, seq: u64, id: &str, channel: &str) {
        let payload = json!({ "channel": channel, "args": [] });
        let frames = encode_reliable_frames(Some(&payload), "phone-stream-0001", seq, 1).unwrap();
        for f in frames {
            self.recv(json!({ "v": 1, "kind": "invoke", "id": id, "src": PHONE, "dst": ME, "payload": f.payload() }));
        }
    }
    fn last_accept(&self) -> &Value {
        self.writes.iter().rev().find(|w| w["kind"] == "link-accept").expect("link-accept written")
    }
    fn acks(&self) -> Vec<Ack> {
        self.writes.iter().filter_map(parse_transport_ack).collect()
    }
    fn sent_reliable(&self, kind: &str) -> Vec<Frame> {
        self.writes.iter().filter(|w| w["kind"] == kind).filter_map(|w| parse_transport_payload(&w["payload"])).collect()
    }
    /// 控制端回链路确认 ACK（携带 link request id）。
    fn confirm(&mut self, link_id: &str, ack_seq: u64, stream: &str) {
        let ack = make_transport_ack(ME, stream, ack_seq, Some(link_id), false, false);
        let mut v = ack;
        v["src"] = json!(PHONE);
        self.recv(v);
    }
    fn plain_ack(&mut self, stream: &str, ack_seq: u64) {
        let mut v = make_transport_ack(ME, stream, ack_seq, None, false, false);
        v["src"] = json!(PHONE);
        self.recv(v);
    }
}

#[test]
fn start_requests_token_connects_and_goes_online() {
    let mut h = H::new();
    h.online();
    assert_eq!(h.statuses, vec![Status::Connecting, Status::Online]);
    assert_eq!(h.e.self_device_id(), Some(ME));
}

#[test]
fn no_token_retries_with_backoff() {
    let mut h = H::new();
    h.e.set_clock(h.t, 0);
    h.e.start();
    h.pump();
    h.e.on_token(h.epoch, None);
    h.pump();
    assert_eq!(h.e.status(), Status::Connecting);
    let at = h.e.next_deadline().expect("reconnect armed");
    assert!(at - h.t >= 700 && at - h.t <= 1000, "first backoff 1s * [0.7,1.0], got {}", at - h.t);
    h.advance(at - h.t);
    // 重连时重新要令牌
    assert!(h.e.drain().is_empty());
}

#[test]
fn protocol_version_mismatch_closes_and_reports_issue() {
    let mut h = H::new();
    h.e.start();
    let outs = h.pump();
    assert!(outs.iter().any(|o| matches!(o, Out::RequestToken { .. })));
    h.e.on_token(h.epoch, Some("t".into()));
    h.pump();
    h.e.on_socket_open(h.epoch);
    h.pump();
    h.recv(json!({ "v": 1, "kind": "hello-ack", "payload": { "serverProtocolVersion": 2, "deviceId": ME, "userId": "u" } }));
    assert_ne!(h.e.status(), Status::Online);
    assert_eq!(h.e.issue().map(|i| i.kind), Some("version-mismatch"));
}

#[test]
fn close_codes_classify_into_issues() {
    for (code, reason, kind) in [(4410u16, "", "device-removed"), (4409, "", "replaced"), (4429, "", "too-many-connections"), (4400, "bad Version", "version-mismatch")] {
        assert_eq!(classify_connection_issue(Some(code), reason, None), Some(kind));
    }
    assert_eq!(classify_connection_issue(None, "HTTP 401 Unauthorized", None), Some("auth-failed"));
    assert_eq!(classify_connection_issue(None, "", Some("Unexpected server response: 401")), Some("auth-failed"));
    assert_eq!(classify_connection_issue(Some(1006), "x4010", None), None);
}

#[test]
fn device_removed_stops_reconnecting() {
    let mut h = H::new();
    h.online();
    h.e.on_socket_closed(h.epoch, Some(4410), "");
    h.pump();
    assert_eq!(h.e.status(), Status::Stopped);
    assert_eq!(h.e.issue().map(|i| i.kind), Some("device-removed"));
    assert!(h.e.next_deadline().is_none());
}

#[test]
fn disconnect_schedules_reconnect_and_new_connection_gets_fresh_epoch() {
    let mut h = H::new();
    h.online();
    let old = h.epoch;
    h.e.on_socket_closed(h.epoch, Some(1006), "");
    h.pump();
    assert_eq!(h.e.status(), Status::Connecting);
    let at = h.e.next_deadline().unwrap();
    h.advance(at - h.t);
    assert!(h.epoch > old, "reconnect requested a token for a newer epoch");
    // 旧世代的迟到事件被忽略
    h.e.on_socket_closed(old, Some(1006), "late");
    assert_eq!(h.e.status(), Status::Connecting);
}

#[test]
fn heartbeat_pings_and_drops_a_silent_connection() {
    let mut h = H::new();
    h.online();
    h.advance(20_000);
    assert!(h.writes.iter().any(|w| w["kind"] == "ping"));
    // 持续沉默：超过 pongMissLimit 后强制重连
    for _ in 0..6 {
        h.advance(20_000);
    }
    assert_eq!(h.e.status(), Status::Connecting);
}

#[test]
fn presence_set_is_sent_and_coalesced() {
    let mut h = H::new();
    h.online();
    let mut p = serde_json::Map::new();
    p.insert("remoteControlEnabled".into(), json!(false));
    h.e.send_presence(p);
    h.pump();
    let w = h.writes.last().unwrap();
    assert_eq!(w["kind"], "presence-set");
    assert_eq!(w["payload"]["remoteControlEnabled"], false);
}

#[test]
fn legacy_link_accept_has_no_transport_fields() {
    let mut h = H::new();
    h.online();
    h.link_open(false, "req-1");
    let a = h.last_accept();
    assert_eq!(a["id"], "req-1");
    assert_eq!(a["dst"], PHONE);
    assert!(a["payload"].get("capabilities").is_none() || a["payload"]["capabilities"] == json!([CAP_HISTORY_VIEW_V1]));
    assert!(a["payload"].get("transportStreamId").is_none());
    // 旧控制端：invoke-result / push 是裸帧
    h.writes.clear();
    h.e.send_invoke_result(PHONE, "r1", json!({ "ok": true, "result": 1 })).unwrap();
    h.pump();
    let w = &h.writes[0];
    assert_eq!(w["kind"], "invoke-result");
    assert_eq!(w["payload"], json!({ "ok": true, "result": 1 }));
}

#[test]
fn reliable_link_accept_advertises_transport_and_waits_for_confirmation() {
    let mut h = H::new();
    h.online();
    h.link_open(true, "link-request-0001");
    let a = h.last_accept().clone();
    let caps: Vec<&str> = a["payload"]["capabilities"].as_array().unwrap().iter().filter_map(Value::as_str).collect();
    for c in [CAP_HISTORY_VIEW_V1, CAP_RELIABLE_TRANSPORT, CAP_RELIABLE_LINK_CONFIRM, CAP_TRANSPORT_ACK_V2] {
        assert!(caps.contains(&c), "missing {c} in {caps:?}");
    }
    let stream = a["payload"]["transportStreamId"].as_str().unwrap().to_owned();
    assert_eq!(a["payload"]["transportBaseSeq"], 1);
    // 链路确认之前：push 只入队不写出；结果走「先留可靠副本、再裸帧直发」的防死锁路径
    h.writes.clear();
    h.e.send_push(PHONE, "maker:event", json!({ "sessionId": "s" }), None).unwrap();
    h.e.send_invoke_result(PHONE, "res-1", json!({ "ok": true, "result": 1 })).unwrap();
    h.pump();
    assert!(h.writes.iter().all(|w| parse_transport_payload(&w["payload"]).is_none()), "no transport frame before confirmation: {:?}", h.writes);
    assert!(h.writes.iter().any(|w| w["kind"] == "invoke-result" && w["payload"]["ok"] == true), "bare result sent");
    assert_eq!(h.e.peer_reliable_queue_depth(PHONE), 2);
    // 确认 ACK 到达 → push 被丢弃（尽力而为），结果按 seq 重放
    h.writes.clear();
    h.confirm("link-request-0001", 0, &stream);
    assert!(h.sent_reliable("push").is_empty(), "discardable push is not replayed");
    let results = h.sent_reliable("invoke-result");
    assert_eq!(results.len(), 1, "{:?}", h.writes);
    assert_eq!(results[0].meta.stream_id, stream);
    assert!(h.e.is_link_ready(PHONE));
    // 重放的是 skip 占位(seq1) + 结果(seq2)，累计 ACK 到 2 清空队列
    h.plain_ack(&stream, results[0].meta.seq);
    assert_eq!(h.e.peer_reliable_queue_depth(PHONE), 0);
}

#[test]
fn confirmation_timeout_resets_the_link_and_forces_reconnect() {
    let mut h = H::new();
    h.online();
    h.link_open(true, "link-request-0002");
    // 没有任何确认：超过 max(2*握手超时, 退避*次数) 后按重试耗尽处理 → 无 inbound/timeout-close 能力时整连接重连
    h.advance(31_000);
    // 对端声明了 transport-timeout-close：只重置该 peer 的链路并通知，relay 连接保持
    assert_eq!(h.e.status(), Status::Online);
    assert!(!h.e.is_link_ready(PHONE));
    assert!(h.writes.iter().any(|w| w["kind"] == "link-close" && w["payload"]["reason"] == "transport-timeout"), "{:?}", h.writes);
}

#[test]
fn inbound_reliable_invoke_is_delivered_in_order_and_acked() {
    let mut h = H::new();
    h.online();
    h.link_open(true, "link-request-0003");
    let stream = h.last_accept()["payload"]["transportStreamId"].as_str().unwrap().to_owned();
    h.confirm("link-request-0003", 0, &stream);
    h.writes.clear();
    // seq 2 先到：缓存不交付
    h.reliable_invoke(2, "i2", "maker:b");
    assert!(h.app.iter().all(|e| e.id.as_deref() != Some("i2")));
    h.reliable_invoke(1, "i1", "maker:a");
    let ids: Vec<_> = h.app.iter().filter(|e| e.kind == "invoke").map(|e| e.id.clone().unwrap()).collect();
    assert_eq!(ids, vec!["i1", "i2"]);
    // 交付的是解包后的逻辑 payload
    let first = h.app.iter().find(|e| e.id.as_deref() == Some("i1")).unwrap();
    assert_eq!(first.payload.as_ref().unwrap()["channel"], "maker:a");
    // 累计 ACK 推进到 2
    let last = h.acks().pop().expect("ack written");
    assert_eq!(last.ack_seq, 2);
    // 重复帧：不重复交付，重申 ACK
    let before = h.app.len();
    h.reliable_invoke(1, "i1", "maker:a");
    assert_eq!(h.app.len(), before);
}

#[test]
fn oversized_invoke_result_is_segmented_and_reassembled_by_peer_view() {
    let mut h = H::new();
    h.online();
    h.link_open(true, "link-request-0004");
    let stream = h.last_accept()["payload"]["transportStreamId"].as_str().unwrap().to_owned();
    h.confirm("link-request-0004", 0, &stream);
    h.writes.clear();
    let big = "x".repeat(MAX_CHUNK_BYTES * 2 + 100);
    h.e.send_invoke_result(PHONE, "big-1", json!({ "ok": true, "result": big })).unwrap();
    h.pump();
    let frames = h.sent_reliable("invoke-result");
    assert!(frames.len() >= 2, "segmented: {}", frames.len());
    assert!(frames.iter().all(|f| f.meta.seq == 1 && f.meta.segment.is_some()));
    let joined: String = frames.iter().map(|f| f.data.as_str()).collect();
    let back: Value = serde_json::from_str(&joined).unwrap();
    assert_eq!(back["result"].as_str().unwrap().len(), MAX_CHUNK_BYTES * 2 + 100);
    h.plain_ack(&stream, 1);
    assert_eq!(h.e.peer_reliable_queue_depth(PHONE), 0);
}

#[test]
fn unacked_result_is_retransmitted_then_link_is_reset() {
    let mut h = H::new();
    h.online();
    h.link_open(true, "link-request-0005");
    let stream = h.last_accept()["payload"]["transportStreamId"].as_str().unwrap().to_owned();
    h.confirm("link-request-0005", 0, &stream);
    h.writes.clear();
    h.e.send_invoke_result(PHONE, "r-1", json!({ "ok": true, "result": 1 })).unwrap();
    h.pump();
    assert_eq!(h.sent_reliable("invoke-result").len(), 1);
    // 2s 间隔重发，直到 5 次后整连接重连（没有 inbound transport-timeout 能力的对端：此例对端声明了该能力 → 只重置该 peer）
    let mut sends = 1;
    for _ in 0..40 {
        h.advance(2_000);
        let n = h.sent_reliable("invoke-result").len();
        sends = n;
        if h.e.status() != Status::Online || !h.e.is_link_ready(PHONE) {
            break;
        }
    }
    assert!(sends >= 5, "retransmitted up to the attempt limit, got {sends}");
    // 对端声明了 transport-timeout-close：只重置 peer 链路并通知，relay 连接保持
    assert_eq!(h.e.status(), Status::Online);
    assert!(!h.e.is_link_ready(PHONE));
    assert!(h.writes.iter().any(|w| w["kind"] == "link-close" && w["payload"]["reason"] == "transport-timeout"));
    // 对端重新 link-open：未确认的结果按原 seq 重放
    h.writes.clear();
    h.link_open(true, "link-request-0006");
    let stream2 = h.last_accept()["payload"]["transportStreamId"].as_str().unwrap().to_owned();
    assert_eq!(stream2, stream, "stream id survives a link reset so the peer can dedupe");
    assert_eq!(h.last_accept()["payload"]["transportBaseSeq"], 1);
    h.confirm("link-request-0006", 0, &stream);
    let again = h.sent_reliable("invoke-result");
    assert_eq!(again.len(), 1);
    assert_eq!(again[0].meta.seq, 1);
}

#[test]
fn close_link_notifies_peer_and_drops_state() {
    let mut h = H::new();
    h.online();
    h.link_open(false, "req-9");
    h.writes.clear();
    h.e.close_link(PHONE, "revoked");
    h.pump();
    let w = h.writes.last().unwrap();
    assert_eq!(w["kind"], "link-close");
    assert_eq!(w["payload"]["reason"], "revoked");
}

#[test]
fn peer_permanent_close_abandons_pending_and_clears_reliable_state() {
    let mut h = H::new();
    h.online();
    h.link_open(true, "link-request-0007");
    h.e.send_push(PHONE, "maker:event", json!({ "sessionId": "s" }), None).unwrap();
    assert_eq!(h.e.peer_reliable_queue_depth(PHONE), 1);
    h.recv(json!({ "v": 1, "kind": "link-close", "src": PHONE, "payload": { "reason": "user" } }));
    assert_eq!(h.e.peer_reliable_queue_depth(PHONE), 0);
    assert!(h.app.iter().any(|e| e.kind == "link-close"));
}

#[test]
fn transport_timeout_close_from_peer_keeps_pending() {
    let mut h = H::new();
    h.online();
    h.link_open(true, "link-request-0008");
    let stream = h.last_accept()["payload"]["transportStreamId"].as_str().unwrap().to_owned();
    h.confirm("link-request-0008", 0, &stream);
    h.e.send_invoke_result(PHONE, "keep", json!({ "ok": true, "result": 1 })).unwrap();
    h.pump();
    h.recv(json!({ "v": 1, "kind": "link-close", "src": PHONE, "payload": { "reason": "transport-timeout" } }));
    assert_eq!(h.e.peer_reliable_queue_depth(PHONE), 1, "瞬时重置保留未确认消息");
    assert!(!h.e.is_link_ready(PHONE));
}

#[test]
fn invalid_frames_are_dropped_without_state_change() {
    let mut h = H::new();
    h.online();
    for bad in ["not json", "[]", r#"{"v":2,"kind":"push"}"#, r#"{"v":1,"kind":"zzz"}"#, r#"{"v":1,"kind":"push","payload":{}}"#, r#"{"v":1,"kind":"relay-error","payload":{"code":1}}"#] {
        h.e.on_message(h.epoch, bad);
    }
    h.pump();
    assert!(h.app.is_empty());
    assert_eq!(h.e.status(), Status::Online);
}

#[test]
fn relay_error_device_offline_marks_peer_down_and_keeps_results_for_replay() {
    let mut h = H::new();
    h.online();
    h.link_open(true, "link-request-0010");
    let stream = h.last_accept()["payload"]["transportStreamId"].as_str().unwrap().to_owned();
    h.confirm("link-request-0010", 0, &stream);
    h.e.send_invoke_result(PHONE, "res", json!({ "ok": true, "result": 1 })).unwrap();
    h.pump();
    let frame_id = h.writes.iter().rev().find(|w| w["kind"] == "invoke-result").unwrap()["id"].as_str().unwrap().to_owned();
    h.recv(json!({ "v": 1, "kind": "relay-error", "id": frame_id, "payload": { "code": "DEVICE_OFFLINE", "message": "gone", "dst": PHONE } }));
    assert!(!h.e.is_link_ready(PHONE));
    assert_eq!(h.e.peer_reliable_queue_depth(PHONE), 1, "已完成的结果保留，等下次 link-open 重放");
}

#[test]
fn send_budget_rotates_between_peers() {
    use super::peer::SendBudget;
    let mut b = SendBudget::default();
    let peers = vec!["a".to_owned(), "b".to_owned()];
    assert!(b.take("a", 4, &peers, 0.0));
    assert!(!b.take("a", 1, &peers, 0.0), "每个 peer 在一个窗口里最多拿到自己的份额");
    assert!(b.take("b", 4, &peers, 0.0));
    // 下个 250ms 窗口恢复
    assert!(b.take("a", 1, &peers, 300.0));
}
