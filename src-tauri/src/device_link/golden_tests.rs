//! 与上游 device-link 的黄金数据逐项对照（`scripts/app-settings/dlink-entry.ts` 运行真实 allowlist/topics/transport 得到）。
use super::transport::*;
use super::{allowlist, topics};
use serde_json::{json, Value};

fn golden() -> Value {
    serde_json::from_str(include_str!("../device_link_golden.json")).unwrap()
}

#[test]
fn allowlist_hash_and_sizes_match_upstream() {
    let g = golden();
    assert_eq!(allowlist::allowlist_hash(), g["hash"].as_str().unwrap());
    assert_eq!(allowlist::invoke_count() as u64, g["invokeCount"].as_u64().unwrap());
    assert_eq!(allowlist::push_count() as u64, g["pushCount"].as_u64().unwrap());
}

#[test]
fn topic_routing_matches_upstream() {
    let g = golden();
    let cases = g["topics"].as_array().unwrap();
    assert_eq!(cases.len(), 798);
    for c in cases {
        let ch = c["channel"].as_str().unwrap();
        let got = topics::topic_for_push(ch, &c["payload"]);
        assert_eq!(got.as_deref(), c["topic"].as_str(), "channel={ch} payload={}", c["payload"]);
    }
}

fn payload_for(name: &str) -> Option<Value> {
    let rep = |n: usize, s: &str| s.repeat(n);
    Some(match name {
        "small" => json!({ "ok": true, "result": 1 }),
        "cjk" => json!({ "t": "中文字符串测试" }),
        "emoji" => json!({ "t": "😀😀😀" }),
        "edge" => json!({ "t": rep(MAX_CHUNK_BYTES - 20, "a") }),
        "two" => json!({ "t": rep(MAX_CHUNK_BYTES + 10, "a") }),
        "cjk-split" => json!({ "t": rep(60000, "中") }),
        "emoji-split" => json!({ "t": rep(40000, "😀") }),
        "undefined" => return None,
        "huge" => json!({ "t": rep(MAX_CHUNK_BYTES * 33, "a") }),
        "maxok" => json!({ "t": rep(4 * 1024 * 1024 - 20, "a") }),
        other => panic!("unknown case {other}"),
    })
}

#[test]
fn reliable_frame_encoding_matches_upstream() {
    let g = golden();
    let cases = g["encodeCases"].as_array().unwrap();
    assert_eq!(cases.len(), 50);
    for c in cases {
        let name = c["name"].as_str().unwrap();
        let (seq, base) = (c["seq"].as_u64().unwrap(), c["base"].as_u64().unwrap());
        let payload = payload_for(name);
        let got = encode_reliable_frames(payload.as_ref(), "stream-abcdef", seq, base);
        let tag = format!("{name} seq={seq} base={base}");
        if c["ok"].as_bool().unwrap() {
            let frames = got.unwrap_or_else(|e| panic!("{tag}: {e}"));
            let want = c["frames"].as_array().unwrap();
            assert_eq!(frames.len(), want.len(), "{tag}");
            for (f, w) in frames.iter().zip(want) {
                assert_eq!(f.payload()["__cindyDeviceLinkTransport"], w["meta"], "{tag}");
                assert_eq!(f.data.len() as u64, w["bytes"].as_u64().unwrap(), "{tag}");
                assert!(f.data.starts_with(w["head"].as_str().unwrap()), "{tag}");
                assert!(f.data.ends_with(w["tail"].as_str().unwrap()), "{tag}");
            }
            // 分片拼回去等于原 JSON
            let joined: String = frames.iter().map(|f| f.data.as_str()).collect();
            assert_eq!(joined, serde_json::to_string(payload.as_ref().unwrap()).unwrap(), "{tag}");
        } else {
            assert_eq!(got.unwrap_err(), c["error"].as_str().unwrap(), "{tag}");
        }
    }
}

#[test]
fn transport_payload_parsing_matches_upstream() {
    let g = golden();
    for c in g["parseCases"].as_array().unwrap() {
        let got = parse_transport_payload(&c["input"]).map(|f| json!({ "meta": f.meta.to_value(), "data": f.data }));
        assert_eq!(got.unwrap_or(Value::Null), c["out"], "input={}", c["input"]);
    }
}

#[test]
fn ack_encoding_and_parsing_match_upstream() {
    let g = golden();
    for c in g["ackMakeCases"].as_array().unwrap() {
        let got = make_transport_ack("dst", c["stream"].as_str().unwrap(), c["seq"].as_u64().unwrap(), c["link"].as_str(), c["compact"].as_bool().unwrap(), c["delayed"].as_bool().unwrap());
        assert_eq!(got, c["out"], "case={c}");
    }
    let parsed = g["ackParseCases"].as_array().unwrap();
    assert_eq!(parsed.len(), 29);
    for c in parsed {
        let got = parse_transport_ack(&c["input"]).map(|a| a.to_value()).unwrap_or(Value::Null);
        // 上游返回对象的键序是 {delayed?, streamId, ackSeq, linkRequestId?} 或紧凑形，比较时按值而非键序
        assert_eq!(got, c["out"], "input={}", c["input"]);
    }
}

fn est_from(v: &Value) -> RetransmitEstimate {
    RetransmitEstimate { srtt_ms: v["srttMs"].as_f64(), rttvar_ms: v["rttVarMs"].as_f64().unwrap_or(0.0), delivery_bytes_per_ms: v["deliveryBytesPerMs"].as_f64() }
}

fn close(a: f64, b: f64) -> bool {
    (a - b).abs() <= 1e-9 * a.abs().max(b.abs()).max(1.0)
}

#[test]
fn retransmit_pacing_matches_upstream() {
    let g = golden();
    for c in g["rttCases"].as_array().unwrap() {
        let mut est = RetransmitEstimate::default();
        for s in c["samples"].as_array().unwrap() {
            update_retransmit_estimate(&mut est, s.as_f64().unwrap());
        }
        let want = est_from(&c["est"]);
        assert_eq!(est.srtt_ms.is_some(), want.srtt_ms.is_some());
        assert!(close(est.srtt_ms.unwrap_or(0.0), want.srtt_ms.unwrap_or(0.0)), "{c}");
        assert!(close(est.rttvar_ms, want.rttvar_ms), "{c}");
        for s in c["steps"].as_array().unwrap() {
            let d = reliable_retry_delay_ms(&est, 2000.0, s["intervals"].as_f64().unwrap(), s["attempts"].as_u64().unwrap() as u32, s["bytes"].as_f64().unwrap());
            assert!(close(d, s["delay"].as_f64().unwrap()), "{s} got {d}");
            assert!(close(retransmit_base_ms(&est, 2000.0), s["base"].as_f64().unwrap()), "{s}");
        }
    }
    let mut est = RetransmitEstimate::default();
    let dc = g["deliveryCases"].as_array().unwrap();
    for c in &dc[..5] {
        update_delivery_estimate(&mut est, c["bytes"].as_f64().unwrap(), c["ms"].as_f64().unwrap());
        match c["rate"].as_f64() {
            Some(r) => assert!(close(est.delivery_bytes_per_ms.unwrap(), r), "{c}"),
            None => assert!(est.delivery_bytes_per_ms.is_none(), "{c}"),
        }
    }
    est.srtt_ms = Some(100.0);
    est.rttvar_ms = 10.0;
    assert!(close(reliable_retry_delay_ms(&est, 2000.0, 60.0, 1, 1_000_000.0), dc[5]["afterDelay"].as_f64().unwrap()));
}

#[test]
fn model_speed_projection_matches_upstream() {
    let g = golden();
    let cases = g["speedCases"].as_array().unwrap();
    assert_eq!(cases.len(), 220);
    for c in cases {
        let got = super::model_speed::encode_model_speed_payload(c["channel"].as_str().unwrap(), c["payload"].clone());
        assert_eq!(got, c["out"], "channel={} payload={}", c["channel"], c["payload"]);
        // 键序也必须一致（线格式 JSON 文本相同）
        assert_eq!(serde_json::to_string(&got).unwrap(), serde_json::to_string(&c["out"]).unwrap(), "key order: {}", c["payload"]);
    }
}
