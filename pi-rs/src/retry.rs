// 请求重试与超时：判定、退避和设置与原版 1.4.0 一致（ai/utils/retry.ts、settings-manager）。
// 设置每次使用时重读 settings.json（Cindy 会在会话中途改写）。
use serde_json::Value;
use std::sync::OnceLock;
use std::time::Duration;

pub struct Policy {
    pub enabled: bool,
    pub max_retries: u64,
    pub base_delay_ms: u64,
    pub max_delay_ms: u64,
}

fn settings_json() -> Value {
    std::fs::read_to_string(crate::util::agent_dir().join("settings.json")).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or(Value::Null)
}

pub fn enabled_from_settings() -> bool {
    settings_json()["retry"]["enabled"].as_bool().unwrap_or(true)
}

/// enabled 取运行时开关（set_auto_retry），其余取 settings.json，缺省 3 次 / 2000ms / 上限 60000ms。
pub fn policy(enabled: bool) -> Policy {
    let r = &settings_json()["retry"];
    Policy {
        enabled,
        max_retries: r["maxRetries"].as_u64().unwrap_or(3),
        base_delay_ms: r["baseDelayMs"].as_u64().unwrap_or(2000),
        max_delay_ms: r["maxAgentDelayMs"].as_u64().unwrap_or(60000),
    }
}

/// 第 attempt 次重试前的等待：base * 2^(attempt-1)，不超过上限。
pub fn delay_ms(p: &Policy, attempt: u64) -> u64 {
    p.base_delay_ms.saturating_mul(1u64.checked_shl((attempt.max(1) - 1) as u32).unwrap_or(u64::MAX)).min(p.max_delay_ms)
}

const NON_RETRYABLE: &[&str] = &[
    "GoUsageLimitError", "FreeUsageLimitError", "Monthly usage limit reached", "available balance", "insufficient_quota", "out of budget",
    "quota exceeded", "billing", "subscription_sharing_usage_limit_exceeded", "subscription_sharing_workplace_individual_usage_limit_exceeded",
];

const RETRYABLE: &[&str] = &[
    "overloaded", "server_busy", "servers are currently busy", "currently experiencing high demand", "model is at capacity", "rate.?limit",
    "too many requests", "service.?(?:is.?)?(?:temporarily.?)?unavailable", "server.?error", "internal.?error", "provider.?returned.?(?:an.?)?error",
    "exceeded request buffer limit while retrying upstream", "unable to forward upstream", "upstream.?request.?failed", "upstream_stream_read_error",
    "stream was interrupted", "http/?2 stream failed", "network.?error", "connection.?error", "connection.?refused", "connection.?lost",
    "other side closed", "fetch failed", r"request to \S+ failed, reason:", "getaddrinfo", "ENOTFOUND", "EAI_AGAIN", "upstream.?connect",
    "reset before headers", "socket hang up", "socket connection was closed", "timed? out", "timeout", "terminated", "websocket.?closed",
    "websocket.?error", "ended without", "stream ended before message_stop", "stream ended before a terminal response event",
    "http2 request did not get a response", "stream disconnected before completion", "pending stream has been canceled", "retry delay",
    "you can retry your request", "try your request again", "please retry your request", "ResourceExhausted",
    "subscription_sharing_usage_unavailable", "subscription_sharing_user_unavailable",
];

fn re(pats: &[&str]) -> regex_lite::Regex {
    regex_lite::Regex::new(&format!("(?i){}", pats.join("|"))).expect("retry patterns")
}

/// 独立的 HTTP 状态码（前后不是字母数字），对应原版的 (?<![a-z0-9])(?:429|500|...)(?![a-z0-9])。
fn has_status_code(s: &str) -> bool {
    let b = s.as_bytes();
    for code in ["429", "500", "502", "503", "504", "520", "524"] {
        let mut from = 0;
        while let Some(p) = s[from..].find(code) {
            let i = from + p;
            let before = i == 0 || !b[i - 1].is_ascii_alphanumeric();
            let after = i + 3 >= b.len() || !b[i + 3].is_ascii_alphanumeric();
            if before && after { return true; }
            from = i + 1;
        }
    }
    false
}

/// isRetryableAssistantError（不含上下文溢出的排除，调用方另行判断）。
pub fn is_retryable(err: &str) -> bool {
    static BAD: OnceLock<regex_lite::Regex> = OnceLock::new();
    static OK: OnceLock<regex_lite::Regex> = OnceLock::new();
    if err.is_empty() || BAD.get_or_init(|| re(NON_RETRYABLE)).is_match(err) { return false; }
    OK.get_or_init(|| re(RETRYABLE)).is_match(err) || has_status_code(err)
}

/// settings.json 里的 defaultThinkingLevel（有就覆盖当前档位）。
pub fn default_thinking_level() -> Option<String> {
    settings_json()["defaultThinkingLevel"].as_str().map(String::from)
}

/// settings.json 里的 thinkingBudgets（按档位覆盖默认预算）。
pub fn thinking_budgets() -> Value {
    settings_json()["thinkingBudgets"].clone()
}

/// httpIdleTimeoutMs：等响应头、以及流里两次数据之间的最长空闲时间。默认 5 分钟，0 表示不限。
pub fn idle_timeout() -> Option<Duration> {
    match settings_json()["httpIdleTimeoutMs"].as_u64() {
        Some(0) => None,
        Some(ms) => Some(Duration::from_millis(ms)),
        None => Some(Duration::from_millis(300_000)),
    }
}

/// 给一次等待加空闲超时；超时返回能被识别为可重试的错误文本。
pub async fn timed<T>(d: Option<Duration>, what: &str, f: impl std::future::Future<Output = T>) -> Result<T, String> {
    match d {
        None => Ok(f.await),
        Some(d) => tokio::time::timeout(d, f).await.map_err(|_| { let _ = what; "Request timed out.".to_string() }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn retryable_matrix() {
        assert!(is_retryable(r#"503 {"type":"error","error":{"type":"overloaded_error"}}"#));
        assert!(is_retryable("429 Too Many Requests"));
        assert!(is_retryable("Connection error: error sending request"));
        assert!(is_retryable("Request timed out."));
        assert!(is_retryable("Stream was interrupted: connection reset"));
        assert!(!is_retryable("400 invalid_request_error: messages.0.content: Input should be a valid list"));
        assert!(!is_retryable("401 invalid x-api-key"));
        assert!(!is_retryable("429 insufficient_quota: billing"));
        assert!(!is_retryable("request id 15030 failed")); // 15030 里的 503 不是独立状态码
    }
    #[test]
    fn backoff() {
        let p = Policy { enabled: true, max_retries: 6, base_delay_ms: 2000, max_delay_ms: 60000 };
        assert_eq!([1, 2, 3, 6, 7].map(|a| delay_ms(&p, a)), [2000, 4000, 8000, 60000, 60000]);
    }
}
