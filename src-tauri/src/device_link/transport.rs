//! 可靠传输的线格式与重发节奏（上游 transport.ts）。
//! 只含无状态的编解码和估计函数；收发状态机在 `reliable.rs`。
use serde_json::{json, Map, Value};

pub const PROTOCOL_VERSION: u64 = 1;
pub const CAP_RELIABLE_TRANSPORT: &str = "reliable-transport-v1";
pub const CAP_TRANSPORT_TIMEOUT_CLOSE: &str = "transport-timeout-close-v1";
pub const CAP_RELIABLE_LINK_CONFIRM: &str = "reliable-link-confirm-v1";
pub const CAP_TRANSPORT_ACK_V2: &str = "transport-ack-v2";
pub const CAP_HISTORY_VIEW_V1: &str = "history-view-v1";
pub const CAP_BACKGROUND_LINK_V1: &str = "background-link-v1";
pub const TRANSPORT_ACK_CHANNEL: &str = "__cindy/device-link/transport-ack";
pub const ACK_STREAM_PREFIX_CHARS: usize = 12;
pub const DELAYED_ACK_MS: u64 = 200;
pub const DELAYED_ACK_MESSAGES: u64 = 2;
pub const DELAYED_ACK_BYTES: u64 = 32 * 1024;
pub const MAX_CHUNK_BYTES: usize = 128 * 1024;
pub const MAX_SEGMENTS: usize = 64;
pub const MAX_MESSAGE_BYTES: usize = 4 * 1024 * 1024;
pub const MAX_REASSEMBLY_BYTES: usize = 16 * 1024 * 1024;
pub const MAX_SEQUENCE_WINDOW: u64 = 64;
pub const MAX_PENDING_MESSAGES: usize = 64;
pub const MAX_PENDING_BYTES: usize = 16 * 1024 * 1024;
pub const RETRY_INTERVAL_MS: u64 = 2_000;
pub const MAX_RETRY_ATTEMPTS: u32 = 5;
pub const RETRY_PASS_BUDGET: usize = 8;
pub const PENDING_PUSH_MAX_AGE_MS: u64 = 5 * 60_000;
pub const ADAPTIVE_RETRY_MAX_MS: f64 = 10_000.0;

const MARKER: &str = "__cindyDeviceLinkTransport";
const SKIP_MARKER: &str = "__cindyDeviceLinkTransportSkip";

pub fn byte_length(text: &str) -> usize {
    text.len()
}

#[derive(Debug, Clone, PartialEq)]
pub struct Segment {
    pub index: usize,
    pub total: usize,
    pub total_bytes: usize,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Meta {
    pub stream_id: String,
    pub seq: u64,
    pub base_seq: u64,
    pub segment: Option<Segment>,
}

impl Meta {
    pub fn to_value(&self) -> Value {
        let mut m = Map::new();
        m.insert("version".into(), json!(1));
        m.insert("streamId".into(), json!(self.stream_id));
        m.insert("seq".into(), json!(self.seq));
        if self.base_seq > 1 {
            m.insert("baseSeq".into(), json!(self.base_seq));
        }
        if let Some(s) = &self.segment {
            m.insert("segment".into(), json!({ "index": s.index, "total": s.total, "totalBytes": s.total_bytes }));
        }
        Value::Object(m)
    }
}

/// 一个可靠传输帧的 payload（信封其余字段由调用方保留）。
#[derive(Debug, Clone, PartialEq)]
pub struct Frame {
    pub meta: Meta,
    pub data: String,
}

impl Frame {
    pub fn payload(&self) -> Value {
        json!({ MARKER: self.meta.to_value(), "data": self.data })
    }
}

/// 按 UTF-8 字节切分，不切断字符。
fn split_utf8(text: &str, max_bytes: usize) -> Result<Vec<&str>, String> {
    if max_bytes == 0 {
        return Err("maxBytes must be positive".into());
    }
    let mut chunks = Vec::new();
    let (mut start, mut chunk_bytes) = (0usize, 0usize);
    for (offset, ch) in text.char_indices() {
        let n = ch.len_utf8();
        if offset > start && chunk_bytes + n > max_bytes {
            chunks.push(&text[start..offset]);
            start = offset;
            chunk_bytes = 0;
        }
        if n > max_bytes {
            return Err("transport chunk size is smaller than one code point".into());
        }
        chunk_bytes += n;
    }
    if text.len() > start || text.is_empty() {
        chunks.push(&text[start..]);
    }
    Ok(chunks)
}

/// 把逻辑 payload 编成一个或多个帧。`payload` 为 None 相当于 JS 的 undefined。
pub fn encode_reliable_frames(payload: Option<&Value>, stream_id: &str, seq: u64, base_seq: u64) -> Result<Vec<Frame>, String> {
    if stream_id.is_empty() || seq == 0 || base_seq == 0 || base_seq > seq || seq > (1 << 53) - 1 {
        return Err("invalid transport stream metadata".into());
    }
    let Some(payload) = payload else { return Err("transport payload is not JSON serializable".into()) };
    let json = serde_json::to_string(payload).map_err(|e| e.to_string())?;
    let total_bytes = byte_length(&json);
    if total_bytes > MAX_MESSAGE_BYTES {
        return Err(format!("transport payload exceeds {MAX_MESSAGE_BYTES} bytes"));
    }
    let chunks = split_utf8(&json, MAX_CHUNK_BYTES)?;
    if chunks.len() > MAX_SEGMENTS {
        return Err(format!("transport payload exceeds {MAX_SEGMENTS} segments"));
    }
    let total = chunks.len();
    Ok(chunks
        .into_iter()
        .enumerate()
        .map(|(index, data)| Frame {
            meta: Meta {
                stream_id: stream_id.to_owned(),
                seq,
                base_seq,
                segment: (total > 1).then_some(Segment { index, total, total_bytes }),
            },
            data: data.to_owned(),
        })
        .collect())
}

fn safe_int(v: &Value) -> Option<i64> {
    let n = v.as_f64()?;
    (n.fract() == 0.0 && n.abs() <= 9_007_199_254_740_991.0).then_some(n as i64)
}

fn utf16_len(s: &str) -> usize {
    s.encode_utf16().count()
}

/// None = 不是 transport 帧（旧客户端的普通 payload）。
pub fn parse_transport_payload(payload: &Value) -> Option<Frame> {
    let raw = payload.as_object()?.get(MARKER)?.as_object()?;
    if raw.get("version").and_then(Value::as_f64) != Some(1.0) {
        return None;
    }
    let stream_id = raw.get("streamId")?.as_str()?;
    let seq = safe_int(raw.get("seq")?).filter(|n| *n > 0)? as u64;
    let base_seq = match raw.get("baseSeq") {
        None => 1,
        Some(v) => safe_int(v).filter(|n| *n > 0)? as u64,
    };
    let data = payload.get("data")?.as_str()?;
    if stream_id.is_empty() || utf16_len(stream_id) > 128 || base_seq > seq {
        return None;
    }
    let segment = match raw.get("segment") {
        None => None,
        Some(s) => {
            let s = s.as_object()?;
            let index = safe_int(s.get("index")?)?;
            let total = safe_int(s.get("total")?)?;
            let total_bytes = safe_int(s.get("totalBytes")?)?;
            if index < 0
                || total < 2
                || index >= total
                || total > MAX_SEGMENTS as i64
                || total_bytes < 0
                || total_bytes > MAX_MESSAGE_BYTES as i64
                || total_bytes <= MAX_CHUNK_BYTES as i64
                || total_bytes > total * MAX_CHUNK_BYTES as i64
            {
                return None;
            }
            Some(Segment { index: index as usize, total: total as usize, total_bytes: total_bytes as usize })
        }
    };
    Some(Frame { meta: Meta { stream_id: stream_id.to_owned(), seq, base_seq, segment }, data: data.to_owned() })
}

pub fn skip_payload() -> Value {
    json!({ SKIP_MARKER: true })
}

pub fn is_skip_payload(payload: &Value) -> bool {
    payload.as_object().and_then(|o| o.get(SKIP_MARKER)) == Some(&Value::Bool(true))
}

fn prefix(s: &str) -> String {
    s.chars().take(ACK_STREAM_PREFIX_CHARS).collect()
}

/// 累计 ACK 帧（push 信封）。
pub fn make_transport_ack(dst: &str, stream_id: &str, ack_seq: u64, link_request_id: Option<&str>, compact: bool, delayed: bool) -> Value {
    let link = link_request_id.filter(|l| !l.is_empty());
    let inner = if compact {
        let mut m = Map::new();
        m.insert("s".into(), json!(prefix(stream_id)));
        m.insert("a".into(), json!(ack_seq));
        if let Some(l) = link {
            m.insert("l".into(), json!(prefix(l)));
        }
        if delayed {
            m.insert("d".into(), json!(1));
        }
        Value::Object(m)
    } else {
        let mut m = Map::new();
        m.insert("streamId".into(), json!(stream_id));
        m.insert("ackSeq".into(), json!(ack_seq));
        if let Some(l) = link {
            m.insert("linkRequestId".into(), json!(l));
        }
        if delayed {
            m.insert("delayed".into(), json!(true));
        }
        Value::Object(m)
    };
    json!({ "v": PROTOCOL_VERSION, "kind": "push", "dst": dst, "payload": { "channel": TRANSPORT_ACK_CHANNEL, "payload": inner } })
}

#[derive(Debug, Clone, PartialEq)]
pub struct Ack {
    pub stream_id: String,
    pub ack_seq: u64,
    pub link_request_id: Option<String>,
    pub stream_prefix: bool,
    pub delayed: bool,
}

impl Ack {
    pub fn to_value(&self) -> Value {
        let mut m = Map::new();
        m.insert("streamId".into(), json!(self.stream_id));
        m.insert("ackSeq".into(), json!(self.ack_seq));
        if self.stream_prefix {
            m.insert("streamPrefix".into(), json!(true));
        }
        if let Some(l) = &self.link_request_id {
            m.insert("linkRequestId".into(), json!(l));
        }
        if self.delayed {
            m.insert("delayed".into(), json!(true));
        }
        Value::Object(m)
    }
}

pub fn parse_transport_ack(env: &Value) -> Option<Ack> {
    if env.get("kind")?.as_str()? != "push" {
        return None;
    }
    let p = env.get("payload")?.as_object()?;
    if p.get("channel")?.as_str()? != TRANSPORT_ACK_CHANNEL {
        return None;
    }
    let inner = p.get("payload").and_then(Value::as_object);
    if let Some(i) = inner {
        if !i.contains_key("streamId") {
            if let Some(s) = i.get("s").and_then(Value::as_str) {
                let seq = i.get("a").and_then(safe_int).filter(|n| *n >= 0)?;
                if utf16_len(s) < 8 || utf16_len(s) > 128 {
                    return None;
                }
                let link = match i.get("l") {
                    None => None,
                    Some(l) => {
                        let l = l.as_str()?;
                        if utf16_len(l) < 8 || utf16_len(l) > 128 {
                            return None;
                        }
                        Some(l.to_owned())
                    }
                };
                return Some(Ack {
                    stream_id: s.to_owned(),
                    ack_seq: seq as u64,
                    link_request_id: link,
                    stream_prefix: true,
                    delayed: i.get("d").and_then(Value::as_f64) == Some(1.0),
                });
            }
        }
    }
    let i = inner?;
    let stream_id = i.get("streamId")?.as_str()?;
    let ack_seq = i.get("ackSeq").and_then(safe_int).filter(|n| *n >= 0)?;
    let link = match i.get("linkRequestId") {
        None => None,
        Some(l) => {
            let l = l.as_str()?;
            if l.is_empty() || utf16_len(l) > 256 {
                return None;
            }
            Some(l.to_owned())
        }
    };
    Some(Ack {
        stream_id: stream_id.to_owned(),
        ack_seq: ack_seq as u64,
        link_request_id: link,
        stream_prefix: false,
        delayed: i.get("delayed") == Some(&Value::Bool(true)),
    })
}

/// RFC 6298 平滑往返估计 + 交付速度估计。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RetransmitEstimate {
    pub srtt_ms: Option<f64>,
    pub rttvar_ms: f64,
    pub delivery_bytes_per_ms: Option<f64>,
}

pub fn update_delivery_estimate(est: &mut RetransmitEstimate, bytes: f64, elapsed_ms: f64) {
    if bytes < 64.0 * 1024.0 || !elapsed_ms.is_finite() || elapsed_ms <= 0.0 {
        return;
    }
    let sample = bytes / elapsed_ms;
    est.delivery_bytes_per_ms = Some(match est.delivery_bytes_per_ms {
        None => sample,
        Some(prev) => sample.min(prev * 0.75 + sample * 0.25),
    });
}

pub fn update_retransmit_estimate(est: &mut RetransmitEstimate, sample_ms: f64) {
    let sample = ADAPTIVE_RETRY_MAX_MS.min(sample_ms.max(0.0));
    match est.srtt_ms {
        None => {
            est.srtt_ms = Some(sample);
            est.rttvar_ms = sample / 2.0;
        }
        Some(srtt) => {
            est.rttvar_ms = 0.75 * est.rttvar_ms + 0.25 * (srtt - sample).abs();
            est.srtt_ms = Some(0.875 * srtt + 0.125 * sample);
        }
    }
}

pub fn retransmit_base_ms(est: &RetransmitEstimate, configured_ms: f64) -> f64 {
    match est.srtt_ms {
        None => configured_ms,
        Some(srtt) => configured_ms.max(ADAPTIVE_RETRY_MAX_MS.min(srtt + 4.0 * est.rttvar_ms)),
    }
}

/// 单条可靠消息距上次写出后多久可以重发。
pub fn reliable_retry_delay_ms(est: &RetransmitEstimate, configured_ms: f64, size_intervals: f64, attempts: u32, bytes: f64) -> f64 {
    let backoff = 4f64.min(2f64.powi(attempts.saturating_sub(1) as i32));
    let measured = match est.delivery_bytes_per_ms {
        Some(rate) if bytes >= 64.0 * 1024.0 && rate != 0.0 => bytes / rate * 1.5,
        _ => 0.0,
    };
    let delivery = 120_000f64.min((configured_ms * size_intervals).max(measured) * if bytes > 512.0 * 1024.0 { backoff } else { 1.0 });
    delivery.max(retransmit_base_ms(est, configured_ms) * backoff)
}
