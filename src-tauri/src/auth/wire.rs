// auth-client client.ts 里与传输无关的纯函数：request-id / 服务端错误码脱敏、Retry-After、错误体解析。
use regex::Regex;
use serde_json::Value;
use std::sync::OnceLock;

fn re(cell: &'static OnceLock<Regex>, pat: &str) -> &'static Regex {
    cell.get_or_init(|| Regex::new(pat).unwrap())
}

fn request_id_re() -> &'static Regex {
    static C: OnceLock<Regex> = OnceLock::new();
    re(&C, r"^[A-Za-z0-9][A-Za-z0-9._:-]{0,127}$")
}
fn phone_re() -> &'static Regex {
    static C: OnceLock<Regex> = OnceLock::new();
    re(&C, r"^(?:\+[0-9]{1,3}[\s.-]?)?(?:[0-9][\s.-]?){6,14}[0-9]$")
}
fn secret_prefix_re() -> &'static Regex {
    static C: OnceLock<Regex> = OnceLock::new();
    re(&C, r"(?i-u)^(?:sk-|pk-|rk-|ak-|gh[pousr]_|github_pat_|xox[baprs]-)")
}
fn credential_word_re() -> &'static Regex {
    static C: OnceLock<Regex> = OnceLock::new();
    re(&C, r"(?i-u)^(?:Bearer|Basic|Token)(?:\s|sk-|pk-|[A-Za-z0-9._:-]{8,})")
}

fn log_safe_request_id(v: &str) -> Option<&str> {
    if !request_id_re().is_match(v) || phone_re().is_match(v) {
        return None;
    }
    if secret_prefix_re().is_match(v) || credential_word_re().is_match(v) {
        return None;
    }
    Some(v)
}

fn log_safe_server_code(v: &str) -> Option<&str> {
    if !request_id_re().is_match(v) || phone_re().is_match(v) || secret_prefix_re().is_match(v) {
        return None;
    }
    Some(v)
}

/// authRequestId：依次看 x-request-id、x-litellm-call-id，取第一个安全的。
pub fn auth_request_id(get: impl Fn(&str) -> Option<String>) -> Option<String> {
    for name in ["x-request-id", "x-litellm-call-id"] {
        let Some(raw) = get(name) else { continue };
        let value = crate::local_db::mapper::js_trim(&raw);
        if value.is_empty() {
            continue;
        }
        if let Some(s) = log_safe_request_id(value) {
            return Some(s.to_string());
        }
    }
    None
}

/// Date.parse 的子集：IMF-fixdate（RFC 7231 首选格式，带 GMT / 数字时区）。其余格式返回 None。
pub fn parse_http_date(s: &str) -> Option<f64> {
    let t = crate::local_db::mapper::js_trim(s);
    // JS 的 Date.parse 不校验星期；chrono 会，所以先去掉「Mon, 」这类前缀。
    let t = match t.split_once(',') {
        Some((wd, rest)) if wd.trim().chars().all(|c| c.is_ascii_alphabetic()) => rest.trim_start(),
        _ => t,
    };
    let d = chrono::DateTime::parse_from_rfc2822(t).ok()?;
    Some(d.timestamp_millis() as f64)
}

/// retryAfterDeadline：秒数或 HTTP 日期，不凭空捏造冷却时间。
pub fn retry_after_deadline(retry_after: Option<&str>, date: Option<&str>, now: f64) -> Option<f64> {
    let value = crate::local_db::mapper::js_trim(retry_after?);
    if value.is_empty() {
        return None;
    }
    let deadline = if value.bytes().all(|b| b.is_ascii_digit()) {
        now + value.parse::<f64>().ok()? * 1000.0
    } else {
        // Date.parse 也认模棱两可的数字；HTTP 日期必须含月份名。
        if !Regex::new(r"[A-Za-z]{3}").unwrap().is_match(value) {
            return None;
        }
        let at = parse_http_date(value);
        let server = date.and_then(parse_http_date);
        match (at, server) {
            (Some(a), Some(s)) => now + (a - s).max(0.0),
            (Some(a), None) => a,
            (None, _) => return None,
        }
    };
    let safe_int = deadline.is_finite() && deadline.fract() == 0.0 && deadline.abs() <= 9_007_199_254_740_991.0;
    (safe_int && (0.0..=8.64e15).contains(&deadline)).then(|| deadline.max(now))
}

pub struct ErrorBody {
    pub code: Option<String>,
    pub message: Option<String>,
}

/// errorResponseSchema.safeParse：全部字段可缺省，但出现时类型必须对；失败返回 None。
pub fn parse_error_body(data: &Value) -> Option<ErrorBody> {
    let m = data.as_object()?;
    let opt_str = |m: &serde_json::Map<String, Value>, k: &str| -> Option<Option<String>> {
        match m.get(k) {
            None => Some(None),
            Some(Value::String(s)) => Some(Some(s.clone())),
            _ => None,
        }
    };
    let top_code = opt_str(m, "code")?;
    let top_msg = opt_str(m, "message")?;
    let (ecode, emsg) = match m.get("error") {
        None => (None, None),
        Some(Value::Object(e)) => (opt_str(e, "code")?, opt_str(e, "message")?),
        _ => return None,
    };
    Some(ErrorBody { code: ecode.or(top_code), message: emsg.or(top_msg) })
}

/// serverErrorCode：非空且通过脱敏才保留。
pub fn server_error_code(data: &Value) -> Option<String> {
    let code = parse_error_body(data)?.code?;
    (!code.is_empty() && log_safe_server_code(&code).is_some()).then_some(code)
}
