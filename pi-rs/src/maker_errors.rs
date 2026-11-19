// maker_errors.rs — 错误分类与脱敏（maker-core 的移植，供 maker.rs 翻译 auto_retry_* / 模型报错用）。
// 来源（cindy 仓库 packages/）：
//   maker-shared/src/errorRedaction.ts      redactSensitiveText / extractNonSecretErrorSignals
//   maker-shared/src/contextOverflowError.ts isContextOverflowErrorMessage
//   maker-core/src/agents/shared/{network-error,stream-interrupt-error,overload-error,unsupported-request-option-error}.ts
//   maker-core/src/agents/pi/translator.ts  isPiAbortedRequest / piAssistantErrorOf / parsePiAutoRetryProgress
// 约束：规则必须与 TS 逐条一致（scripts/maker-freeze.mjs 的手写场景会比对两侧产出）。
use regex::Regex;
use serde_json::{json, Value};
use std::sync::OnceLock;

pub const CONTEXT_OVERFLOW_REASON: &str = "context-overflow";
pub const UNSUPPORTED_REQUEST_OPTION_REASON: &str = "unsupported-request-option";
pub const UPSTREAM_STREAM_INTERRUPTED_REASON: &str = "upstream-stream-interrupted";
pub const UPSTREAM_OVERLOAD_REASON: &str = "upstream-overload";
pub const PI_GATEWAY_DROP_REASON: &str = "pi-gateway-drop";

macro_rules! re {
    ($pat:expr) => {{
        static R: OnceLock<Regex> = OnceLock::new();
        R.get_or_init(|| Regex::new($pat).expect("static regex"))
    }};
}

pub fn is_context_overflow(m: &str) -> bool {
    re!(r"(?i)prompt is too long|maximum (?:context|prompt) length|context.{0,20}(length|window).{0,40}(exceed|too)|(input|request|message).{0,20}exceeds?.{0,40}context.{0,20}(length|window)|context_length_exceeded").is_match(m)
}

pub fn is_stream_interrupted(m: &str) -> bool {
    re!(r"(?i)Response API in-stream error").is_match(m)
}

/// `Reconnecting... N/M`（也认 `…`）。
pub fn parse_reconnect(m: &str) -> Option<(i64, i64)> {
    let c = re!(r"(?i)\bReconnecting(?:\.{3}|…)\s*(\d+)\s*/\s*(\d+)\b").captures(m)?;
    let (a, b): (i64, i64) = (c[1].parse().ok()?, c[2].parse().ok()?);
    (a >= 1 && b >= a).then_some((a, b))
}

pub fn is_networkish(m: &str) -> bool {
    parse_reconnect(m).is_some()
        || re!(r"(?i)\b50[234]\b|Bad Gateway|\bService(?:\s+is)?(?:\s+temporarily)?\s+Unavailable\b|\bmodel(?:'s)?\s+availability\s+is\s+(?:currently\s+)?degraded\b|Gateway Time-?out|upstream unreachable|ECONNREFUSED|ECONNRESET|ETIMEDOUT|ENOTFOUND|ENETUNREACH|EHOSTUNREACH|EPIPE|EAI_AGAIN|fetch failed|network error|socket hang up|AggregateError|Request timed out|^API Error:\s*The operation timed out|^The operation timed out|stream ended before a terminal|Connection error|upstream stream error").is_match(m)
}

/// Pi 自己报的请求失败（区别于宿主主动中止）。
pub fn is_pi_aborted_request(e: &str) -> bool {
    re!(r"(?i)^Request was aborted[.!]?$").is_match(e.trim())
}

/// `overloaded`（529）/ `capacity`。codexErrorInfoTag 在 Pi 路径不存在。
pub fn parse_overload(m: &str, status: Option<i64>) -> Option<&'static str> {
    if status == Some(529) {
        return Some("overloaded");
    }
    if re!(r"(?i)\bat capacity\b").is_match(m) || re!(r"(?i)\bcurrently overloaded\b").is_match(m) {
        return Some("capacity");
    }
    if re!(r"\boverloaded_error\b|\b529\b").is_match(m) {
        return Some("overloaded");
    }
    None
}

pub fn format_overload_retry_message(m: &str, attempt: i64, max: i64) -> String {
    format!("{m} (auto-retry {attempt}/{max})")
}

/// `unsupported-request-option`：只认明确指向 prompt_cache_* 的 400。
pub fn is_unsupported_request_option(message: &str) -> bool {
    if message.is_empty() || message.len() > 64 * 1024 {
        return false;
    }
    let status = re!(r"^\s*(\d{3})\s*:").captures(message).map(|c| c[1].to_string());
    if status.as_deref().is_some_and(|s| s != "400") {
        return false;
    }
    let cache_field = |f: &str| f == "prompt_cache_retention" || f == "prompt_cache_key";
    let mut text = message.to_string();
    if let Some(start) = message.find('{') {
        match serde_json::from_str::<Value>(&message[start..]) {
            Ok(mut value) => {
                for _ in 0..3 {
                    let Some(err) = value.as_object() else { return false };
                    if let Some(p) = err.get("param") {
                        if !p.is_null() && p != "" && !p.as_str().is_some_and(cache_field) {
                            return false;
                        }
                    }
                    if let Some(t) = err.get("type") {
                        let t = t.as_str().map(String::from).unwrap_or_else(|| t.to_string());
                        if t != "invalid_request_error" && t != "unsupported_parameter" {
                            return false;
                        }
                    }
                    if err.get("code") == Some(&json!("UNKNOWN_FIELD")) {
                        if let Some(field) = err.get("data").and_then(|d| d.get("field")) {
                            if !field.as_str().is_some_and(cache_field) {
                                return false;
                            }
                            if status.as_deref() == Some("400") {
                                return true;
                            }
                        }
                    }
                    if let Some(m) = err.get("message").and_then(Value::as_str) {
                        text = m.to_string();
                        break;
                    }
                    let next = err.get("error").cloned().unwrap_or(Value::Null);
                    value = next;
                }
            }
            Err(_) => return false,
        }
    }
    re!(r"(?i)\bprompt_cache_(?:retention|key)\b").is_match(&text)
        && re!(r#"(?i)(?:unsupported\s+(?:request\s+)?parameters?(?:\(s\))?\s*:\s*[`"'\\\s]*(prompt_cache_(?:retention|key))\b|\b(prompt_cache_(?:retention|key))\b[`"'\\\s]*(?:is\s+(?:not\s+supported|unsupported)|isn't\s+supported)|未知请求字段\s*[:：][`"'\\\s]*(prompt_cache_(?:retention|key))\b)"#).is_match(&text)
}

/// 显式、非机密的路由信号：状态码（401/403/429/529）与用量上限。
pub fn extract_signals(input: &str) -> (Option<i64>, bool) {
    // 正则里的 (?=$|...) 先行断言 regex crate 不支持，改成消费一个边界字符或行尾。
    let status = re!(r#"(?i)(?:^|[\s,{;])["']?(?:status(?:[-_ ]?code)?|http(?:[-_ ]?status)?|code)["']?\s*(?:[:=]\s*)?["']?(401|403|429|529)["']?(?:$|[\s,;:}\]])"#)
        .captures(input)
        .or_else(|| re!(r"(?i)\brequest\s+rejected\s*\(\s*(401|403|429|529)\s*\)").captures(input))
        .and_then(|c| c[1].parse::<i64>().ok());
    let usage_limit = re!(r#"(?i)(?:^|[\s,{;])(?:rate\s+limit(?:ed|s|ing)?|usage\s+limit(?:ed|s|ing)?|too\s+many\s+requests|quota(?:["']?\s*[:=]\s*["']?|\s+)(?:exhausted|exceeded)|(?:["']?(?:code|type|error|message)["']?\s*[:=]\s*["']?)?(?:rate_limit_exceeded|insufficient_quota|exceeded[-_ ]?budget|budget[-_ ]?exceeded))\b"#).is_match(input);
    // 无结构化的 permission denied 不当 403 信号。
    let status = status.filter(|s| !(*s == 403 && re!(r"(?i)permission denied").is_match(input)));
    (status, usage_limit)
}

/// 去掉凭证与类凭证值，保留上下文。与 TS 同序：header 字段 → key=sk-.. → 通用字段 → Bearer → 裸 key → URL 参数 → aigw。
pub fn redact(input: &str) -> String {
    if input.is_empty() {
        return String::new();
    }
    let mut out = redact_header_fields(input, &["www-authenticate", "authorization", "proxy-authorization"], true);
    out = redact_header_fields(&out, &["set-cookie", "cookie"], false);
    out = re!(r#"(?i)((?:["']?)key(?:["']?)\s*(?:\([^)\r\n]{0,256}\)\s*)?(?:=|:)\s*)(?:sk|pk|rk)-[A-Za-z0-9][A-Za-z0-9._-]{6,}\b"#).replace_all(&out, "${1}[REDACTED_KEY]").into_owned();
    // TS 用 (?!\[REDACTED(?:_KEY)?\]) 先行否定断言；regex crate 不支持，改为手动逐处匹配：
    // 值（紧跟分隔符之后）以已脱敏占位符开头就原样保留，且匹配整体不吞掉占位符，保证幂等。
    let field = re!(r#"(?i)((?:["']?)(?:x-api-key|api[-_ ]?key|access[-_ ]?token|refresh[-_ ]?token|id[-_ ]?token|proxy[-_ ]?token|client[-_ ]?secret|received\s+api\s+key|key\s+hash|key|secret|password|passwd|token)(?:["']?)\s*(?:\([^)\r\n]{0,256}\)\s*)?(?:=|:)\s*)"#);
    let value = re!(r#"^(?:"[^"]*"|'[^']*'|<[^>]*>|Bearer\s+[A-Za-z0-9._~+/=-]+|[^\s,;}\]]+)"#);
    let mut rebuilt = String::new();
    let mut last = 0usize;
    for m in field.find_iter(&out) {
        if m.start() < last {
            continue;
        }
        let rest = &out[m.end()..];
        if rest.starts_with("[REDACTED]") || rest.starts_with("[REDACTED_KEY]") {
            continue;
        }
        if let Some(v) = value.find(rest) {
            rebuilt.push_str(&out[last..m.end()]);
            rebuilt.push_str("[REDACTED]");
            last = m.end() + v.end();
        }
    }
    rebuilt.push_str(&out[last..]);
    out = rebuilt;
    out = re!(r"(?i)\bBearer\s+[A-Za-z0-9._~+/=-]+").replace_all(&out, "Bearer [REDACTED]").into_owned();
    out = re!(r"\b(?:sk|pk|rk)-[A-Za-z0-9][A-Za-z0-9._-]{6,}\b").replace_all(&out, "[REDACTED_KEY]").into_owned();
    out = re!(r"(?i)([?&](?:api[-_]?key|access[-_]?token|refresh[-_]?token|token)=)[^&#\s]+").replace_all(&out, "${1}[REDACTED]").into_owned();
    out = re!(r#"(?i)\baigw:([^\s"'`,;)\]}]+)"#)
        .replace_all(&out, |c: &regex::Captures| if c[1].starts_with("[REDACTED") { c[0].to_string() } else { "aigw:[REDACTED]".to_string() })
        .into_owned();
    out
}

fn redact_header_fields(input: &str, names: &[&str], stop_at_semicolon: bool) -> String {
    let chars: Vec<char> = input.chars().collect();
    let mut out = String::new();
    let mut copy_from = 0usize;
    let mut cursor = 0usize;
    let mut touched = false;
    while cursor < chars.len() {
        match match_header_field(&chars, cursor, names) {
            None => cursor += 1,
            Some(value_start) => {
                let mut boundary = value_start;
                while boundary < chars.len() && (!stop_at_semicolon || chars[boundary] != ';') && chars[boundary] != '\r' && chars[boundary] != '\n' {
                    boundary += 1;
                }
                out.extend(chars[copy_from..value_start].iter());
                out.push_str("[REDACTED]");
                copy_from = boundary;
                cursor = boundary;
                touched = true;
            }
        }
    }
    if !touched {
        return input.to_string();
    }
    out.extend(chars[copy_from..].iter());
    out
}

fn match_header_field(chars: &[char], start: usize, names: &[&str]) -> Option<usize> {
    let quote = chars[start];
    let quoted = quote == '"' || quote == '\'';
    let name_start = if quoted { start + 1 } else { start };
    let max_len = names.iter().map(|n| n.len()).max().unwrap_or(0);
    let window: String = chars[name_start.min(chars.len())..(name_start + max_len).min(chars.len())].iter().collect::<String>().to_lowercase();
    let name = names.iter().find(|n| window.starts_with(**n))?;
    let mut cursor = name_start + name.len();
    if quoted {
        if chars.get(cursor) != Some(&quote) {
            return None;
        }
        cursor += 1;
    }
    while chars.get(cursor).is_some_and(|c| c.is_whitespace()) {
        cursor += 1;
    }
    if chars.get(cursor) == Some(&'(') {
        let mut closing = None;
        let mut offset = 1;
        while offset <= 256 && cursor + offset < chars.len() {
            if chars[cursor + offset] == ')' {
                closing = Some(cursor + offset);
                break;
            }
            offset += 1;
        }
        cursor = closing? + 1;
        while chars.get(cursor).is_some_and(|c| c.is_whitespace()) {
            cursor += 1;
        }
    }
    if chars.get(cursor) != Some(&'=') && chars.get(cursor) != Some(&':') {
        return None;
    }
    cursor += 1;
    while chars.get(cursor).is_some_and(|c| c.is_whitespace()) {
        cursor += 1;
    }
    Some(cursor)
}

/// Pi 模型报错 → 待收尾的错误（`pendingAssistantError`）。
pub fn assistant_error_of(raw: &str) -> Value {
    let (status, usage_limit) = extract_signals(raw);
    let redacted = redact(raw);
    let mut o = json!({ "message": redacted, "sdkError": redacted });
    if let Some(s) = status {
        o["errorStatus"] = json!(s);
    }
    if usage_limit {
        o["usageLimit"] = json!(true);
    }
    let reason = if is_context_overflow(&redacted) {
        Some(CONTEXT_OVERFLOW_REASON)
    } else if is_unsupported_request_option(&redacted) {
        Some(UNSUPPORTED_REQUEST_OPTION_REASON)
    } else if is_stream_interrupted(&redacted) || is_pi_aborted_request(&redacted) {
        Some(UPSTREAM_STREAM_INTERRUPTED_REASON)
    } else {
        None
    };
    if let Some(r) = reason {
        o["reason"] = json!(r);
    }
    o
}

/// message_end 的 assistant 消息是否是瞬时失败（会被 Pi 自己重试）。
pub fn is_transient_assistant_failure(message: &Value) -> bool {
    match message["stopReason"].as_str() {
        Some("error") => true,
        Some("aborted") => {
            let e = message["errorMessage"].as_str().unwrap_or("").trim();
            !e.is_empty() && (is_networkish(e) || is_stream_interrupted(e) || is_pi_aborted_request(e))
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_like_the_ts_rules() {
        assert!(is_context_overflow("prompt is too long: 250000 tokens > 200000 maximum"));
        assert!(is_context_overflow("context_length_exceeded"));
        assert!(!is_context_overflow("output limit reached"));
        assert!(is_networkish("fetch failed: ECONNRESET"));
        assert!(is_networkish("Reconnecting... 2/5"));
        assert!(is_networkish("502 Bad Gateway"));
        assert!(!is_networkish("401 invalid api key"));
        assert!(!is_networkish("service degraded"), "裸 degraded 不算");
        assert_eq!(parse_reconnect("Reconnecting... 2/5"), Some((2, 5)));
        assert_eq!(parse_reconnect("Reconnecting... 6/5"), None);
        assert_eq!(parse_overload("529 overloaded_error: Overloaded", None), Some("overloaded"));
        assert_eq!(parse_overload("Selected model is at capacity", None), Some("capacity"));
        assert_eq!(parse_overload("x", Some(529)), Some("overloaded"));
        assert_eq!(parse_overload("the word overloaded alone", None), None, "裸词不算过载");
        assert!(is_pi_aborted_request("Request was aborted."));
        assert!(!is_pi_aborted_request("Request was aborted by user because X"));
    }

    #[test]
    fn unsupported_request_option_needs_an_explicit_cache_field_rejection() {
        assert!(is_unsupported_request_option("400: Unsupported parameter: prompt_cache_retention"));
        assert!(is_unsupported_request_option("400: {\"error\":{\"type\":\"invalid_request_error\",\"message\":\"prompt_cache_key is not supported\"}}"));
        assert!(!is_unsupported_request_option("500: Unsupported parameter: prompt_cache_retention"), "非 400 不算");
        assert!(!is_unsupported_request_option("the prompt_cache_key header was mentioned"), "只提到字段名不算");
        assert!(!is_unsupported_request_option("400: {\"error\":{\"param\":\"temperature\",\"message\":\"prompt_cache_key is not supported\"}}"), "param 指向别的字段不算");
    }

    #[test]
    fn extracts_status_and_usage_limit_without_trusting_opaque_tokens() {
        assert_eq!(extract_signals("request rejected (429)").0, Some(429));
        assert_eq!(extract_signals("status: 401 Unauthorized").0, Some(401));
        assert_eq!(extract_signals("tok-401-x").0, None);
        assert_eq!(extract_signals("403 permission denied status: 403").0, None);
        assert!(extract_signals("rate limit exceeded").1);
        assert!(!extract_signals("hello").1);
    }

    #[test]
    fn redaction_masks_credentials_and_is_idempotent() {
        let r = redact("Authorization: Bearer abc.def\nkey: sk-abcdefghij1234 token=zzz https://x/y?api_key=s3cr3t aigw:user-1");
        assert!(!r.contains("abc.def") && !r.contains("sk-abcdefghij1234") && !r.contains("zzz") && !r.contains("s3cr3t") && !r.contains("user-1"), "{r}");
        assert_eq!(redact(&r), r, "二次脱敏结果不变");
        assert_eq!(redact("plain text"), "plain text");
    }

    #[test]
    fn assistant_error_carries_reason_and_signals() {
        let e = assistant_error_of("prompt is too long: 250000 tokens > 200000 maximum");
        assert_eq!(e["reason"], CONTEXT_OVERFLOW_REASON);
        let e = assistant_error_of("429 rate limit exceeded status: 429");
        assert_eq!(e["errorStatus"], 429);
        assert_eq!(e["usageLimit"], true);
        assert!(assistant_error_of("401 invalid api key").get("reason").is_none());
    }

    #[test]
    fn transient_failure_covers_errors_and_network_aborts_only() {
        assert!(is_transient_assistant_failure(&json!({ "stopReason": "error" })));
        assert!(is_transient_assistant_failure(&json!({ "stopReason": "aborted", "errorMessage": "fetch failed" })));
        assert!(!is_transient_assistant_failure(&json!({ "stopReason": "aborted" })), "用户 Stop 不算");
        assert!(!is_transient_assistant_failure(&json!({ "stopReason": "stop" })));
    }
}
