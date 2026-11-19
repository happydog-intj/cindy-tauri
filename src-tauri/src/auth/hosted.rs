// 托管回调登录的纯逻辑（authHostedCallback.ts）：轮询凭据、节奏、结果映射、轮询循环。
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::future::Future;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Notify;

pub const FAST_MS: u64 = 1_000;
pub const SLOW_MS: u64 = 2_000;
pub const BACKOFF_AFTER_MS: u64 = 30_000;
pub const MAX_CONSECUTIVE_FAILURES: u32 = 8;
const FALLBACK_CODE: &str = "AUTH_REQUEST_FAILED";

/// 等价 AbortController：可克隆，abort 后唤醒所有等待者。
#[derive(Clone, Default)]
pub struct Cancel {
    flag: Arc<AtomicBool>,
    notify: Arc<Notify>,
}

impl Cancel {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn abort(&self) {
        self.flag.store(true, Ordering::SeqCst);
        self.notify.notify_waiters();
    }
    /// 用于判断两个 Cancel 是否同一个（指针相等）。
    pub fn flag_arc(&self) -> Arc<AtomicBool> {
        self.flag.clone()
    }
    pub fn aborted(&self) -> bool {
        self.flag.load(Ordering::SeqCst)
    }
    /// 等到 abort；已 abort 则立即返回。
    pub async fn cancelled(&self) {
        loop {
            let waiter = self.notify.notified();
            tokio::pin!(waiter);
            // 先登记再检查，避免 abort 落在两步之间。
            waiter.as_mut().enable();
            if self.aborted() {
                return;
            }
            waiter.await;
        }
    }
    /// 睡 ms，abort 时提前结束（等价 sleepUnlessAborted）。
    pub async fn sleep(&self, ms: u64) {
        if self.aborted() {
            return;
        }
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_millis(ms)) => {}
            _ = self.cancelled() => {}
        }
    }
}

pub struct PollCredentials {
    pub client_state: String,
    pub poll_secret: String,
}

pub fn derive_client_state(poll_secret: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(poll_secret.as_bytes()))
}

pub fn create_poll_credentials() -> PollCredentials {
    let mut b = [0u8; 32];
    getrandom::fill(&mut b).expect("系统随机数不可用");
    let poll_secret = URL_SAFE_NO_PAD.encode(b);
    PollCredentials { client_state: derive_client_state(&poll_secret), poll_secret }
}

pub fn poll_delay_ms(elapsed_ms: u64) -> u64 {
    if elapsed_ms >= BACKOFF_AFTER_MS { SLOW_MS } else { FAST_MS }
}

/// 回调结果：Ok(授权码) 或 Err(错误码)。
pub type Settled = Result<String, String>;

/// poll 响应 → 结论；None = 继续轮询。
pub fn map_poll(poll: &Value) -> Option<Settled> {
    match poll.get("status").and_then(Value::as_str) {
        Some("pending") => None,
        Some("ok") => Some(Ok(poll["code"].as_str().unwrap_or("").to_string())),
        Some("error") => Some(Err(poll["error"].as_str().unwrap_or("").to_string())),
        _ => Some(Err("INVALID_AUTH_CODE".to_string())),
    }
}

pub fn poll_error_code(code: Option<&str>) -> String {
    match code {
        Some(c) if !c.is_empty() => c.to_string(),
        _ => FALLBACK_CODE.to_string(),
    }
}

/// 轮询失败的错误码（来自 AuthApiError.code）。
pub struct PollFailure(pub Option<String>);

pub struct PollingDeps<P, N> {
    pub poll: P,
    pub now: N,
    pub cancel: Cancel,
    pub timeout_ms: u64,
    pub max_failures: u32,
}

/// 轮询直到拿到结论、被取消或超时。sleep 走 `cancel.sleep`，测试可用 `tokio::time::pause`。
pub async fn run_polling<P, Fut, N>(mut deps: PollingDeps<P, N>) -> Settled
where
    P: FnMut() -> Fut,
    Fut: Future<Output = Result<Value, PollFailure>>,
    N: Fn() -> u64,
{
    let started = (deps.now)();
    let mut failures = 0u32;
    loop {
        if deps.cancel.aborted() {
            return Err("USER_CANCELLED".into());
        }
        if (deps.now)().saturating_sub(started) >= deps.timeout_ms {
            return Err("REQUEST_TIMEOUT".into());
        }
        match (deps.poll)().await {
            Ok(outcome) => {
                failures = 0;
                if let Some(s) = map_poll(&outcome) {
                    return s;
                }
            }
            Err(PollFailure(code)) => {
                if deps.cancel.aborted() {
                    return Err("USER_CANCELLED".into());
                }
                failures += 1;
                if failures >= deps.max_failures {
                    return Err(poll_error_code(code.as_deref()));
                }
            }
        }
        let elapsed = (deps.now)().saturating_sub(started);
        deps.cancel.sleep(poll_delay_ms(elapsed)).await;
    }
}
