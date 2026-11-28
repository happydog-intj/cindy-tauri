//! 邮箱验证码登录。接口形状与客户端已验证的云端 mock 一致（/api/auth/*）。
//! 没有邮件通道：验证码由服务端生成并打印到服务端日志，能看到日志的人才能登录（自用）。
use crate::db::{now_ms, random_hex, Db, Identity};
use axum::{
    extract::State,
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD as B64, Engine};
use serde_json::{json, Value};
use std::{collections::HashMap, sync::{Arc, Mutex}};

const ACCESS_TTL_MS: i64 = 3_600_000;
const CODE_TTL_MS: i64 = 600_000;
const REQUEST_GAP_MS: i64 = 30_000;
const MAX_ATTEMPTS: u32 = 5;

struct Pending {
    code: String,
    expires: i64,
    attempts: u32,
    requested_at: i64,
}

pub struct AuthState {
    pub db: Arc<Db>,
    /// 为空表示不限制邮箱；非空时只有列表内的邮箱能登录。
    pub allowed: Vec<String>,
    pending: Mutex<HashMap<String, Pending>>,
    /// 测试用：固定验证码，同时不打印。
    pub fixed_code: Option<String>,
}

impl AuthState {
    pub fn new(db: Arc<Db>, allowed: Vec<String>, fixed_code: Option<String>) -> Self {
        Self { db, allowed, pending: Mutex::new(HashMap::new()), fixed_code }
    }
}

pub fn error(status: StatusCode, code: &str) -> Response {
    (status, Json(json!({ "error": { "code": code, "message": code } }))).into_response()
}

pub fn bearer(h: &HeaderMap) -> Option<&str> {
    h.get(header::AUTHORIZATION)?.to_str().ok()?.strip_prefix("Bearer ")
}

pub fn authed(db: &Db, h: &HeaderMap) -> Option<Identity> {
    db.identity_for_access(bearer(h)?)
}

fn membership(user_id: &str, email: &str) -> Value {
    json!({
        "id": format!("mem-{user_id}"), "passportId": user_id, "kind": "personal", "role": "owner",
        "displayName": email.split('@').next().unwrap_or(email), "avatarUrl": null, "email": email,
        "orgId": null, "orgName": null, "orgLogoUrl": null,
    })
}

/// 签发一对令牌。访问令牌是 JWT 形状（客户端读 exp 安排刷新），服务端以哈希查库校验，不依赖签名。
fn issue(db: &Db, user_id: &str, email: &str, device_id: &str) -> Result<Value, Response> {
    let exp_ms = now_ms() + ACCESS_TTL_MS;
    let head = B64.encode(r#"{"alg":"none"}"#);
    let body = B64.encode(json!({ "sub": user_id, "exp": exp_ms / 1000 }).to_string());
    let access = format!("{head}.{body}.{}", random_hex(16));
    let refresh = format!("rt-{}", random_hex(24));
    db.issue(user_id, device_id, &access, exp_ms, &refresh).map_err(|_| error(StatusCode::INTERNAL_SERVER_ERROR, "INTERNAL"))?;
    Ok(json!({ "status": "ok", "accessToken": access, "refreshToken": refresh, "membership": membership(user_id, email) }))
}

fn valid_email(e: &str) -> bool {
    e.len() <= 254 && e.matches('@').count() == 1 && !e.starts_with('@') && !e.ends_with('@') && !e.contains(char::is_whitespace)
}

fn str_field<'a>(b: &'a Value, k: &str) -> Option<&'a str> {
    b.get(k)?.as_str().filter(|s| !s.is_empty())
}

async fn providers() -> Json<Value> {
    Json(json!({ "region": "global", "attribution": "email", "email": true, "phone": false, "social": [] }))
}

async fn discovery() -> Json<Value> {
    Json(json!({ "methods": [{ "type": "email_code" }] }))
}

async fn sso_discovery() -> Response {
    error(StatusCode::NOT_FOUND, "ORG_SSO_NOT_FOUND")
}

async fn request_code(State(s): State<Arc<AuthState>>, Json(b): Json<Value>) -> Response {
    let Some(email) = str_field(&b, "email").map(|e| e.trim().to_lowercase()).filter(|e| valid_email(e)) else {
        return error(StatusCode::BAD_REQUEST, "INVALID_PARAMS");
    };
    if !s.allowed.is_empty() && !s.allowed.contains(&email) {
        // 不暴露名单：照常返回 sent，但不生成验证码。
        return Json(json!({ "status": "sent" })).into_response();
    }
    let now = now_ms();
    let mut p = s.pending.lock().unwrap();
    p.retain(|_, v| v.expires > now);
    if p.get(&email).is_some_and(|v| now - v.requested_at < REQUEST_GAP_MS) {
        return error(StatusCode::TOO_MANY_REQUESTS, "RATE_LIMITED");
    }
    let code = s.fixed_code.clone().unwrap_or_else(|| {
        let mut n = [0u8; 4];
        getrandom::fill(&mut n).expect("系统随机数不可用");
        format!("{:06}", u32::from_le_bytes(n) % 1_000_000)
    });
    if s.fixed_code.is_none() {
        eprintln!("[cindy-server] 登录验证码 {email}: {code}（10 分钟内有效）");
    }
    p.insert(email, Pending { code, expires: now + CODE_TTL_MS, attempts: 0, requested_at: now });
    Json(json!({ "status": "sent" })).into_response()
}

async fn verify_code(State(s): State<Arc<AuthState>>, Json(b): Json<Value>) -> Response {
    let (Some(email), Some(code), Some(device)) = (str_field(&b, "email"), str_field(&b, "code"), str_field(&b, "deviceId")) else {
        return error(StatusCode::BAD_REQUEST, "INVALID_PARAMS");
    };
    if b.get("clientType").and_then(Value::as_str) != Some("desktop") || device.len() > 128 {
        return error(StatusCode::BAD_REQUEST, "INVALID_PARAMS");
    }
    let email = email.trim().to_lowercase();
    {
        let mut p = s.pending.lock().unwrap();
        let Some(entry) = p.get_mut(&email).filter(|e| e.expires > now_ms()) else {
            return error(StatusCode::BAD_REQUEST, "INVALID_CODE");
        };
        entry.attempts += 1;
        if entry.code != code {
            if entry.attempts >= MAX_ATTEMPTS {
                p.remove(&email);
            }
            return error(StatusCode::BAD_REQUEST, "INVALID_CODE");
        }
        p.remove(&email);
    }
    let Ok(user) = s.db.user_for_email(&email) else { return error(StatusCode::INTERNAL_SERVER_ERROR, "INTERNAL") };
    issue(&s.db, &user, &email, device).map_or_else(|e| e, |v| Json(v).into_response())
}

async fn refresh(State(s): State<Arc<AuthState>>, Json(b): Json<Value>) -> Response {
    let Some(token) = str_field(&b, "refreshToken") else { return error(StatusCode::BAD_REQUEST, "INVALID_PARAMS") };
    let Some(id) = s.db.take_refresh(token) else { return error(StatusCode::UNAUTHORIZED, "INVALID_REFRESH_TOKEN") };
    issue(&s.db, &id.user_id, &id.email, &id.device_id).map_or_else(|e| e, |v| Json(v).into_response())
}

async fn logout(State(s): State<Arc<AuthState>>, headers: HeaderMap) -> Response {
    let Some(id) = authed(&s.db, &headers) else { return error(StatusCode::UNAUTHORIZED, "INVALID_TOKEN") };
    s.db.revoke(&id.user_id, &id.device_id);
    Json(json!({ "status": "ok" })).into_response()
}

pub fn routes(s: Arc<AuthState>) -> Router {
    Router::new()
        .route("/api/auth/providers", get(providers))
        .route("/api/auth/discovery", post(discovery))
        .route("/api/auth/sso/discovery", post(sso_discovery))
        .route("/api/auth/email/request-code", post(request_code))
        .route("/api/auth/email/verify-code", post(verify_code))
        .route("/api/auth/refresh", post(refresh))
        .route("/api/auth/logout", post(logout))
        .with_state(s)
}
