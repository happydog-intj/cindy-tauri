// CindyAuthClient（client.ts）的 Rust 版：REST 请求、错误归类、超时与取消。
// 不记录也不持久化凭据；onRequestError 只给路径、状态码与固定名称。
use super::hosted::Cancel;
use super::{validate as v, wire};
use serde_json::{json, Map, Value};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

#[derive(Debug, Clone)]
pub struct AuthApiError {
    pub code: String,
    pub status: u16,
    pub message: String,
    pub retry_at: Option<f64>,
    pub server_code: Option<String>,
    pub request_id: Option<String>,
}

impl AuthApiError {
    pub fn new(code: &str, status: u16, message: &str) -> Self {
        AuthApiError { code: code.into(), status, message: message.into(), retry_at: None, server_code: None, request_id: None }
    }
}

impl std::fmt::Display for AuthApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "[{}] {}", self.code, self.message)
    }
}

pub type OnRequestError = Arc<dyn Fn(&str, u16, &str) + Send + Sync>;

pub struct ClientOptions {
    pub base_url: String,
    pub region: String,
    pub device_id: String,
    pub client_type: String,
    pub locale: Option<String>,
    pub timeout_ms: u64,
    pub on_request_error: Option<OnRequestError>,
}

pub struct AuthClient {
    o: ClientOptions,
    base: String,
}

pub(crate) fn http() -> &'static reqwest::Client {
    static C: OnceLock<reqwest::Client> = OnceLock::new();
    C.get_or_init(|| {
        // reqwest 用 rustls-no-provider：先装 ring（updater 只在自己用时才装）。
        if rustls::crypto::CryptoProvider::get_default().is_none() {
            let _ = rustls::crypto::ring::default_provider().install_default();
        }
        reqwest::Client::builder().build().expect("http client")
    })
}

const POLL_TIMEOUT_MS: u64 = 30_000;

#[derive(Default)]
struct Req {
    token: Option<String>,
    timeout_ms: Option<u64>,
    cancel: Option<Cancel>,
}

fn now_ms() -> f64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as f64).unwrap_or(0.0)
}

impl AuthClient {
    pub fn new(o: ClientOptions) -> Self {
        let mut base = o.base_url.clone();
        while base.ends_with('/') {
            base.pop();
        }
        AuthClient { o, base }
    }

    fn locale(&self) -> Value {
        self.o.locale.as_deref().map(Value::from).unwrap_or(Value::Null)
    }

    /// 与 JSON.stringify 一致：值为 undefined 的键整体省略。
    fn with_locale(&self, mut m: Map<String, Value>) -> Value {
        if let Some(l) = &self.o.locale {
            m.insert("locale".into(), json!(l));
        }
        Value::Object(m)
    }

    pub async fn get_providers(&self) -> Result<Value, AuthApiError> {
        let r = self.request("/api/auth/providers", None, Req::default(), v::provider_config).await?;
        if r["region"] != json!(self.o.region) {
            return Err(AuthApiError::new(
                "REGION_MISMATCH",
                0,
                &format!("Configured {} build received {} auth deployment", self.o.region, r["region"].as_str().unwrap_or("")),
            ));
        }
        Ok(r)
    }

    pub async fn discover(&self, email: &str) -> Result<Vec<Value>, AuthApiError> {
        let raw = self
            .request("/api/auth/discovery", Some(json!({ "email": email })), Req::default(), |d| {
                let m = d.as_object().ok_or(())?;
                match m.get("methods") {
                    Some(Value::Array(a)) => Ok(json!({ "methods": a })),
                    _ => Err(()),
                }
            })
            .await?;
        let items = raw["methods"].as_array().cloned().unwrap_or_default();
        let methods = v::recognize_login_methods(&items).map_err(|_| {
            AuthApiError::new("INVALID_RESPONSE", 200, "Authentication server returned a malformed known login method")
        })?;
        if !items.is_empty() && methods.is_empty() {
            return Err(AuthApiError::new("INVALID_RESPONSE", 200, "Authentication server returned no recognized login methods"));
        }
        Ok(methods)
    }

    pub async fn discover_sso_org(&self, org: &str) -> Result<Value, AuthApiError> {
        let d = self.request("/api/auth/sso/discovery", Some(json!({ "org": org })), Req::default(), v::sso_org_discovery).await?;
        if d["region"] != json!(self.o.region) {
            return Err(AuthApiError::new(
                "REGION_MISMATCH",
                0,
                &format!("Queried {} auth deployment returned {}", self.o.region, d["region"].as_str().unwrap_or("")),
            ));
        }
        if d["connections"].as_array().map_or(true, |a| a.is_empty()) {
            return Err(AuthApiError::new("ORG_SSO_NOT_FOUND", 200, "Enterprise exists but has no SSO connection enabled"));
        }
        Ok(d)
    }

    pub async fn request_code(&self, kind: &str, identifier: &str, captcha: Option<&str>) -> Result<(), AuthApiError> {
        let mut m = Map::new();
        m.insert(if kind == "email" { "email" } else { "phone" }.into(), json!(identifier));
        if let Some(c) = captcha.filter(|c| !c.is_empty()) {
            // 键序：locale 在 captchaToken 之前（对象展开顺序）。
            let body = self.with_locale(m);
            let mut bm = body.as_object().unwrap().clone();
            bm.insert("captchaToken".into(), json!(c));
            return self.sent(&format!("/api/auth/{kind}/request-code"), Value::Object(bm)).await;
        }
        let body = self.with_locale(m);
        self.sent(&format!("/api/auth/{kind}/request-code"), body).await
    }

    async fn sent(&self, path: &str, body: Value) -> Result<(), AuthApiError> {
        self.request(path, Some(body), Req::default(), |d| v::status_literal(d, "sent")).await.map(|_| ())
    }

    pub async fn verify_code(&self, kind: &str, identifier: &str, code: &str) -> Result<Value, AuthApiError> {
        let mut m = Map::new();
        m.insert(if kind == "email" { "email" } else { "phone" }.into(), json!(identifier));
        m.insert("code".into(), json!(code));
        m.insert("deviceId".into(), json!(self.o.device_id));
        m.insert("clientType".into(), json!(self.o.client_type));
        let body = self.with_locale(m);
        self.request(&format!("/api/auth/{kind}/verify-code"), Some(body), Req::default(), v::login_outcome).await
    }

    pub async fn exchange_authorization_code(&self, code: &str, verifier: &str) -> Result<Value, AuthApiError> {
        let body = json!({"grantType": "authorization_code", "code": code, "codeVerifier": verifier, "deviceId": self.o.device_id});
        self.request("/api/auth/token", Some(body), Req::default(), v::login_outcome).await
    }

    pub async fn poll_desktop_authorization(&self, poll_secret: &str, cancel: &Cancel) -> Result<Value, AuthApiError> {
        let body = json!({"pollSecret": poll_secret, "deviceId": self.o.device_id});
        let req = Req { timeout_ms: Some(POLL_TIMEOUT_MS), cancel: Some(cancel.clone()), ..Default::default() };
        self.request("/api/auth/desktop/callback/poll", Some(body), req, v::desktop_poll).await
    }

    pub async fn select_account(&self, ticket: &str, account_id: &str) -> Result<Value, AuthApiError> {
        let body = json!({"loginTicket": ticket, "accountId": account_id, "deviceId": self.o.device_id});
        self.request("/api/auth/select-account", Some(body), Req::default(), v::login_outcome).await
    }

    pub async fn request_binding_code(&self, ticket: &str, bind_type: &str, contact: &str) -> Result<(), AuthApiError> {
        let mut m = Map::new();
        m.insert("bindTicket".into(), json!(ticket));
        m.insert(bind_type.into(), json!(contact));
        let body = self.with_locale(m);
        self.sent("/api/auth/binding/request-code", body).await
    }

    pub async fn verify_binding(&self, ticket: &str, bind_type: &str, contact: &str, code: &str) -> Result<Value, AuthApiError> {
        let mut m = Map::new();
        m.insert("bindTicket".into(), json!(ticket));
        m.insert(bind_type.into(), json!(contact));
        m.insert("code".into(), json!(code));
        m.insert("deviceId".into(), json!(self.o.device_id));
        self.request("/api/auth/binding/verify", Some(Value::Object(m)), Req::default(), v::login_outcome).await
    }

    pub async fn refresh(&self, refresh_token: &str) -> Result<Value, AuthApiError> {
        let body = json!({"refreshToken": refresh_token, "deviceId": self.o.device_id});
        self.request("/api/auth/refresh", Some(body), Req { timeout_ms: Some(0), ..Default::default() }, v::token_pair).await
    }

    pub async fn exchange_account_membership(&self, account_token: &str, membership_id: &str) -> Result<Value, AuthApiError> {
        let req = Req { token: Some(account_token.into()), ..Default::default() };
        self.request("/api/auth/account/exchange", Some(json!({"membershipId": membership_id})), req, v::token_pair).await
    }

    pub async fn request_sso_verification_code(&self, ticket: &str) -> Result<(), AuthApiError> {
        let body = json!({"verificationTicket": ticket, "deviceId": self.o.device_id});
        self.sent("/api/auth/sso/verification/request-code", body).await
    }

    pub async fn verify_sso_verification(&self, ticket: &str, code: &str) -> Result<Value, AuthApiError> {
        let body = json!({"verificationTicket": ticket, "code": code, "deviceId": self.o.device_id});
        self.request("/api/auth/sso/verification/verify", Some(body), Req::default(), v::login_outcome).await
    }

    pub async fn logout(&self, access_token: &str) -> Result<(), AuthApiError> {
        let req = Req { token: Some(access_token.into()), ..Default::default() };
        self.request("/api/auth/logout", Some(json!({})), req, |d| v::status_literal(d, "ok")).await.map(|_| ())
    }

    /// buildAuthorizeUrl：参数顺序与 URLSearchParams 序列化（application/x-www-form-urlencoded）保持一致。
    pub fn build_authorize_url(&self, kind: &str, id: &str, redirect_uri: &str, code_challenge: &str, state: &str) -> String {
        let enc = |s: &str| url::form_urlencoded::byte_serialize(s.as_bytes()).collect::<String>();
        let seg = if kind == "social" { "social" } else { "sso" };
        let path_id = percent_encoding::utf8_percent_encode(id, URI_COMPONENT).to_string();
        let mut q: Vec<(&str, String)> = vec![
            ("client_type", self.o.client_type.clone()),
            ("device_id", self.o.device_id.clone()),
            ("redirect_uri", redirect_uri.into()),
            ("code_challenge", code_challenge.into()),
            ("code_challenge_method", "S256".into()),
            ("client_state", state.into()),
        ];
        if let Some(l) = self.o.locale.as_ref().filter(|l| !l.is_empty()) {
            q.push(("ui_locale", l.clone()));
        }
        let qs = q.iter().map(|(k, v)| format!("{}={}", enc(k), enc(v))).collect::<Vec<_>>().join("&");
        format!("{}/api/auth/{}/{}/authorize?{}", self.base, seg, path_id, qs)
    }

    fn log(&self, path: &str, status: u16, name: &str) {
        if let Some(f) = &self.o.on_request_error {
            f(path, status, name);
        }
    }

    fn transport_failure(&self, aborted: bool, external: bool) -> AuthApiError {
        let code = if !aborted { "NETWORK_ERROR" } else if external { "USER_CANCELLED" } else { "REQUEST_TIMEOUT" };
        let msg = match code {
            "REQUEST_TIMEOUT" => "Authentication request timed out",
            "USER_CANCELLED" => "Authentication request was cancelled",
            _ => "Authentication network request failed",
        };
        AuthApiError::new(code, 0, msg)
    }

    async fn request(
        &self,
        path: &str,
        body: Option<Value>,
        opts: Req,
        schema: impl Fn(&Value) -> Result<Value, ()>,
    ) -> Result<Value, AuthApiError> {
        let timeout_ms = opts.timeout_ms.unwrap_or(self.o.timeout_ms);
        let cancel = opts.cancel.clone();
        // 超时与外部取消覆盖到响应体读完为止。
        let fut = self.request_inner(path, body, &opts, schema);
        tokio::pin!(fut);
        let timer = async {
            if timeout_ms > 0 { tokio::time::sleep(Duration::from_millis(timeout_ms)).await } else { std::future::pending::<()>().await }
        };
        let aborted = async {
            match &cancel { Some(c) => c.cancelled().await, None => std::future::pending::<()>().await }
        };
        tokio::select! {
            r = &mut fut => r,
            _ = timer => { self.log(path, 0, "AbortError"); Err(self.transport_failure(true, false)) }
            _ = aborted => { self.log(path, 0, "AbortError"); Err(self.transport_failure(true, true)) }
        }
    }

    async fn request_inner(
        &self,
        path: &str,
        body: Option<Value>,
        opts: &Req,
        schema: impl Fn(&Value) -> Result<Value, ()>,
    ) -> Result<Value, AuthApiError> {
        let url = format!("{}{}", self.base, path);
        let mut rb = if body.is_none() { http().get(&url) } else { http().post(&url) };
        rb = rb.header("Content-Type", "application/json");
        if let Some(t) = &opts.token {
            rb = rb.header("Authorization", format!("Bearer {t}"));
        }
        if let Some(b) = &body {
            rb = rb.body(serde_json::to_string(b).unwrap_or_default());
        }
        let resp = match rb.send().await {
            Ok(r) => r,
            Err(e) => {
                self.log(path, 0, if e.is_timeout() { "TimeoutError" } else { "TypeError" });
                return Err(self.transport_failure(false, false));
            }
        };
        let status = resp.status().as_u16();
        let ok = resp.status().is_success();
        let hdr = |n: &str| resp.headers().get(n).and_then(|v| v.to_str().ok()).map(String::from);
        let retry_at = if status == 429 {
            wire::retry_after_deadline(hdr("retry-after").as_deref(), hdr("date").as_deref(), now_ms())
        } else {
            None
        };
        let request_id = wire::auth_request_id(&hdr);
        let bytes = match resp.bytes().await {
            Ok(b) => b,
            Err(_) => {
                self.log(path, status, "TypeError");
                return Err(self.transport_failure(false, false));
            }
        };
        let data: Value = match serde_json::from_slice(&bytes) {
            Ok(d) => d,
            Err(_) => {
                self.log(path, status, "SyntaxError");
                let mut e = if status == 429 {
                    let mut e = AuthApiError::new("RATE_LIMITED", 429, "Too many requests");
                    e.retry_at = retry_at;
                    e
                } else {
                    AuthApiError::new("INVALID_RESPONSE", status, "Authentication server returned invalid JSON")
                };
                e.request_id = request_id;
                return Err(e);
            }
        };
        if !ok {
            let parsed = wire::parse_error_body(&data);
            let server_code = wire::server_error_code(&data);
            let code = if status == 429 { "RATE_LIMITED".to_string() } else { server_code.clone().unwrap_or_else(|| "AUTH_REQUEST_FAILED".into()) };
            let message = parsed.and_then(|p| p.message).unwrap_or_else(|| "Authentication request failed".into());
            return Err(AuthApiError {
                code,
                status,
                message,
                retry_at,
                server_code: if status == 429 { server_code } else { None },
                request_id,
            });
        }
        match schema(&data) {
            Ok(d) => Ok(d),
            Err(()) => {
                self.log(path, status, "ZodError");
                let mut e = AuthApiError::new("INVALID_RESPONSE", status, "Authentication server response did not match the client contract");
                e.request_id = request_id;
                Err(e)
            }
        }
    }
}

/// encodeURIComponent 不转义的字符集：A-Z a-z 0-9 - _ . ! ~ * ' ( )
const URI_COMPONENT: &percent_encoding::AsciiSet = &percent_encoding::NON_ALPHANUMERIC
    .remove(b'-').remove(b'_').remove(b'.').remove(b'!').remove(b'~').remove(b'*').remove(b'\'').remove(b'(').remove(b')');
