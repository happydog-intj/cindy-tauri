//! 引擎的公共类型：信封、输出事件、定时器、时序配置。
use serde_json::{json, Map, Value};

/// relay 信封。`payload` 为 None 对应 JS 的 undefined。
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Env {
    pub kind: String,
    pub id: Option<String>,
    pub src: Option<String>,
    pub dst: Option<String>,
    pub payload: Option<Value>,
    /// 其余顶层字段（owner 等），原样保留。
    pub extra: Map<String, Value>,
}

impl Env {
    pub fn new(kind: &str) -> Self {
        Env { kind: kind.to_owned(), ..Default::default() }
    }
    pub fn to(mut self, dst: &str) -> Self {
        self.dst = Some(dst.to_owned());
        self
    }
    pub fn id(mut self, id: &str) -> Self {
        self.id = Some(id.to_owned());
        self
    }
    pub fn with(mut self, payload: Value) -> Self {
        self.payload = Some(payload);
        self
    }
    pub fn from_value(v: &Value) -> Option<Env> {
        let o = v.as_object()?;
        if o.get("v").and_then(Value::as_f64) != Some(1.0) {
            return None;
        }
        let kind = o.get("kind")?.as_str()?.to_owned();
        let s = |k: &str| o.get(k).and_then(Value::as_str).map(str::to_owned);
        let mut extra = Map::new();
        for (k, val) in o {
            if !matches!(k.as_str(), "v" | "kind" | "id" | "src" | "dst" | "payload") {
                extra.insert(k.clone(), val.clone());
            }
        }
        Some(Env { kind, id: s("id"), src: s("src"), dst: s("dst"), payload: o.get("payload").cloned(), extra })
    }
    /// 线格式（键序与上游一致：v, kind, id, src, dst, payload, 其余）。
    pub fn to_value(&self) -> Value {
        let mut m = Map::new();
        m.insert("v".into(), json!(1));
        m.insert("kind".into(), json!(self.kind));
        if let Some(i) = &self.id {
            m.insert("id".into(), json!(i));
        }
        if let Some(s) = &self.src {
            m.insert("src".into(), json!(s));
        }
        if let Some(d) = &self.dst {
            m.insert("dst".into(), json!(d));
        }
        if let Some(p) = &self.payload {
            m.insert("payload".into(), p.clone());
        }
        for (k, v) in &self.extra {
            m.insert(k.clone(), v.clone());
        }
        Value::Object(m)
    }
    pub fn to_text(&self) -> String {
        serde_json::to_string(&self.to_value()).unwrap_or_default()
    }
    pub fn wire_bytes(&self) -> usize {
        self.to_text().len()
    }
    pub fn channel(&self) -> Option<&str> {
        self.payload.as_ref()?.get("channel")?.as_str()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Stopped,
    Connecting,
    Online,
}

impl Status {
    pub fn as_str(self) -> &'static str {
        match self {
            Status::Stopped => "stopped",
            Status::Connecting => "connecting",
            Status::Online => "online",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Issue {
    pub kind: &'static str,
    pub close_code: Option<u16>,
    pub detail: Option<String>,
    pub at: u64,
}

/// 引擎对外的副作用。运行时按顺序执行。
#[derive(Debug, Clone, PartialEq)]
pub enum Out {
    /// 向当前 socket 写一个文本帧。
    Write(String),
    /// 需要 access token 来建连；运行时取到后调 `on_token(epoch, token)`。
    RequestToken { epoch: u64 },
    /// 打开 websocket（带 Authorization: Bearer）。
    OpenSocket { epoch: u64, token: String },
    CloseSocket { code: u16, reason: String },
    /// 交给应用层的入站帧（invoke / link-open / link-close / push / 未配对响应）。
    App(Env),
    PresenceChanged(Value),
    Status(Status),
    Issue(Option<Issue>),
    Log(&'static str, String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum TimerKind {
    Reconnect,
    Handshake,
    Ping,
    PresenceRetry,
    ReconnectStable,
    CongestionStable,
    /// 每个 peer 一个重发定时器。
    Retry,
    /// 每个 peer 的链路确认超时 / transport-timeout 通知重试。
    LinkConfirm,
    TimeoutCloseNotify,
    /// 每个接收 stream 的延后 ACK。
    DelayedAck,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TimerKey {
    pub kind: TimerKind,
    pub peer: String,
    pub stream: String,
}

#[derive(Debug, Clone)]
pub struct Timing {
    pub reconnect_base_ms: u64,
    pub reconnect_max_ms: u64,
    pub reconnect_stable_reset_ms: u64,
    pub ping_interval_ms: u64,
    pub pong_miss_limit: u32,
    pub get_token_timeout_ms: u64,
    pub handshake_timeout_ms: u64,
    pub transport_retry_interval_ms: u64,
    pub transport_max_retry_attempts: u32,
    pub transport_retry_pass_budget: usize,
    pub presence_retry_interval_ms: u64,
    pub stalled_link_pending_max_age_ms: u64,
    pub congestion_backoff_base_ms: u64,
    pub congestion_backoff_max_ms: u64,
    pub congestion_stable_reset_ms: u64,
}

impl Default for Timing {
    fn default() -> Self {
        Timing {
            reconnect_base_ms: 1_000,
            reconnect_max_ms: 30_000,
            reconnect_stable_reset_ms: 10_000,
            ping_interval_ms: 20_000,
            pong_miss_limit: 3,
            get_token_timeout_ms: 15_000,
            handshake_timeout_ms: 15_000,
            transport_retry_interval_ms: super::super::transport::RETRY_INTERVAL_MS,
            transport_max_retry_attempts: super::super::transport::MAX_RETRY_ATTEMPTS,
            transport_retry_pass_budget: super::super::transport::RETRY_PASS_BUDGET,
            presence_retry_interval_ms: 500,
            stalled_link_pending_max_age_ms: 60_000,
            congestion_backoff_base_ms: 5_000,
            congestion_backoff_max_ms: 30_000,
            congestion_stable_reset_ms: 15 * 60_000,
        }
    }
}

/// 发送失败的错误码（对应上游 DeviceLinkError.code）。
#[derive(Debug, Clone, PartialEq)]
pub struct LinkError {
    pub code: &'static str,
    pub message: String,
}

impl LinkError {
    pub fn new(code: &'static str, message: impl Into<String>) -> Self {
        LinkError { code, message: message.into() }
    }
}

/// 重连退避（对应上游 computeReconnectDelayMs）。`random` ∈ [0,1]。
pub fn reconnect_delay_ms(attempt: u32, congestion_streak: u32, t: &Timing, random: f64) -> u64 {
    let random = if random.is_finite() { random.clamp(0.0, 1.0) } else { 0.0 };
    let base = (t.reconnect_base_ms as f64 * 2f64.powi(attempt as i32)).min(t.reconnect_max_ms as f64);
    let floor = if congestion_streak > 0 {
        (t.congestion_backoff_base_ms as f64 * 2f64.powi(congestion_streak as i32 - 1)).min(t.congestion_backoff_max_ms as f64)
    } else {
        0.0
    };
    (base.max(floor) * (0.7 + random * 0.3)).round() as u64
}

/// 关闭码 / 原因 → 连接问题类别（对应上游 classifyConnectionIssue）。
pub fn classify_connection_issue(code: Option<u16>, reason: &str, socket_error: Option<&str>) -> Option<&'static str> {
    match code {
        Some(4410) => return Some("device-removed"),
        Some(4409) => return Some("replaced"),
        Some(4429) => return Some("too-many-connections"),
        Some(4400) if reason.to_lowercase().contains("version") => return Some("version-mismatch"),
        _ => {}
    }
    let detail = format!("{} {}", reason, socket_error.unwrap_or("")).to_lowercase();
    let has_401 = detail.match_indices("401").any(|(i, _)| {
        let b = detail.as_bytes();
        let before = i == 0 || !(b[i - 1].is_ascii_alphanumeric() || b[i - 1] == b'_');
        let after = i + 3 >= b.len() || !(b[i + 3].is_ascii_alphanumeric() || b[i + 3] == b'_');
        before && after
    });
    if has_401 || detail.contains("unauthorized") {
        return Some("auth-failed");
    }
    None
}
