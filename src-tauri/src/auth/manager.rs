// 账号会话与登录流程编排（sidecar/src/auth.mjs 的 Rust 版）。
// 状态在一个 std Mutex 里，锁内不 await；异步步骤在 epoch 上做“被取代”检查，与 JS 的单线程语义一致。
use super::backoff::Backoff;
use super::client::{AuthApiError, AuthClient, ClientOptions};
use super::flow::{self, js_trim};
use super::hosted::{self, Cancel, PollFailure, PollingDeps};
use crate::owner::Mode;
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::Duration;

pub const AUTH_SESSION_KEY: &str = "cindy_auth_session_v1";
pub const AUTH_REGION: &str = "global";
const DEFAULT_MODEL: &str = "claude-sonnet-4-6";
const DEFAULT_EFFORT: &str = "medium";
const LOGIN_PROVIDERS_GATE_MS: u64 = 30_000;
const COLD_START_GATE_MS: u64 = 20_000;
const BROWSER_AUTH_TIMEOUT_MS: u64 = 5 * 60_000;
const BROWSER_LAUNCH_TIMEOUT_MS: u64 = 15_000;
const LOGOUT_REQUEST_TIMEOUT_MS: u64 = 5_000;

fn definitive_refresh_failure(code: &str) -> Option<&'static str> {
    match code {
        "INVALID_REFRESH_TOKEN" => Some("replaced-elsewhere"),
        "REFRESH_TOKEN_EXPIRED" => Some("expired"),
        "MEMBERSHIP_DISABLED" => Some("account-unavailable"),
        _ => None,
    }
}

fn flow_cannot_retry(code: &str) -> bool {
    matches!(
        code,
        "INVALID_LOGIN_TICKET" | "INVALID_BIND_TICKET" | "INVALID_SSO_VERIFICATION_TICKET" | "INVALID_AUTH_CODE"
            | "INVALID_TOKEN" | "TOKEN_EXPIRED" | "USER_CANCELLED"
    )
}

pub type BoxFut<T> = Pin<Box<dyn Future<Output = T> + Send>>;

/// 外部依赖：应用会话、密钥库、推送、打开浏览器、语言、端点。测试里换成假实现。
pub trait Env: Send + Sync + 'static {
    fn mode(&self) -> Mode;
    fn cached_user(&self) -> Option<Value>;
    fn stamp(&self) -> Value;
    fn commit(&self, mode: Mode, user: Option<&Value>) -> Result<(), String>;
    fn secrets_ready(&self) -> BoxFut<bool>;
    fn secret_get(&self, name: &str) -> Option<String>;
    fn secret_has(&self, name: &str) -> bool;
    fn secret_set(&self, name: &str, value: &str) -> Result<(), String>;
    fn secret_remove(&self, name: &str) -> Result<(), String>;
    fn emit(&self, channel: &str, arg: Value);
    fn open_url(&self, url: &str) -> BoxFut<Result<(), String>>;
    fn locale(&self) -> String;
    fn auth_base_url(&self) -> String;
    fn desktop_callback_url(&self) -> String;
    fn device_id(&self) -> String;
}

/// 给 model-access / device-link 等使用者的快照（不含 token）。
#[derive(Clone, Debug, PartialEq)]
pub struct AuthInfo {
    pub authenticated: bool,
    pub user_id: Option<String>,
    pub membership_kind: Option<String>,
    pub epoch: u64,
}

#[derive(Default)]
struct State {
    access_token: Option<String>,
    latest_refresh_token: Option<String>,
    current_user: Option<Value>,
    auth_epoch: u64,
    refresh_gen: u64, // 续期定时器代次：取代旧定时器
    refreshing: bool,
    credential_store_unavailable: bool,
    login_epoch: u64,
    login_state: Option<Value>,
    providers: Option<Value>,
    discovered: Vec<Value>,
    pending_account_token: Option<String>,
    pending_login_ticket: Option<String>,
    pending_bind_ticket: Option<String>,
    pending_sso_ticket: Option<String>,
    action_running: Option<u64>, // 正在跑的登录动作所属 login_epoch
    commit_sealed: bool,
    browser_cancel: Option<Cancel>,
}

pub struct Manager {
    env: Arc<dyn Env>,
    st: Mutex<State>,
    backoff: Mutex<Backoff>,
    listeners: Mutex<Vec<Box<dyn Fn(&AuthInfo) + Send + Sync>>>,
    /// 给还留在 sidecar 的使用者（model-access / device-link）镜像状态：(payload, 是否触发 onAuthChange)。
    mirrors: Mutex<Vec<Box<dyn Fn(&Value, bool) + Send + Sync>>>,
    action_done: tokio::sync::Notify,
    refresh_done: tokio::sync::Notify,
    handle: tokio::runtime::Handle,
}

fn now_ms() -> f64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as f64).unwrap_or(0.0)
}

fn api_code(e: &AuthApiError) -> (String, u16) {
    (e.code.clone(), e.status)
}

fn superseded() -> AuthApiError {
    AuthApiError::new("AUTH_FLOW_SUPERSEDED", 409, "Login was superseded by a newer auth action")
}

fn log(level: &str, msg: &str) {
    eprintln!("[auth] {level} {msg}");
}

/// 解 access token 的 payload（不验签）。
fn jwt_claims(token: &str) -> Option<Value> {
    let p = token.split('.').nth(1)?;
    let bytes = URL_SAFE_NO_PAD.decode(p.trim_end_matches('=')).ok()?;
    serde_json::from_slice(&bytes).ok()
}

pub fn org_slug_from_token(token: Option<&str>) -> Option<String> {
    let claims = jwt_claims(token?)?;
    claims.get("orgSlug").and_then(Value::as_str).filter(|s| !s.is_empty()).map(String::from)
}

fn map_membership(m: &Value) -> Value {
    let name = m["displayName"].as_str().filter(|s| !s.is_empty()).or(m["email"].as_str().filter(|s| !s.is_empty())).unwrap_or("Cindy");
    json!({
        "id": m["id"], "name": name, "avatar": m.get("avatarUrl").filter(|v| !v.is_null()).cloned().unwrap_or(Value::Null),
        "email": m["email"], "defaultModel": DEFAULT_MODEL, "defaultEffort": DEFAULT_EFFORT,
        "membershipKind": m["kind"], "membershipRole": m["role"], "orgId": m["orgId"], "orgName": m["orgName"],
        "orgLogoUrl": m.get("orgLogoUrl").filter(|v| !v.is_null()).cloned().unwrap_or(Value::Null),
        "passportId": m.get("passportId").and_then(Value::as_str).unwrap_or(""),
    })
}

fn merge_membership(m: &Value, existing: Option<&Value>) -> Value {
    let mut mapped = map_membership(m);
    if let Some(e) = existing {
        if e["id"] == mapped["id"] {
            if mapped["avatar"].is_null() {
                mapped["avatar"] = e["avatar"].clone();
            }
            if mapped["passportId"].as_str() == Some("") {
                mapped["passportId"] = e["passportId"].clone();
            }
        }
    }
    mapped
}

fn user_from_cache(c: &Value) -> Value {
    json!({
        "id": c["id"], "name": c["name"].as_str().filter(|s| !s.is_empty()).or(c["email"].as_str().filter(|s| !s.is_empty())).unwrap_or("Cindy"),
        "avatar": null, "email": c.get("email").cloned().unwrap_or(Value::Null),
        "defaultModel": DEFAULT_MODEL, "defaultEffort": DEFAULT_EFFORT,
        "membershipKind": c["membershipKind"], "membershipRole": "member",
        "orgId": null, "orgName": null, "orgLogoUrl": null, "passportId": "",
    })
}

fn failure(error: &AuthApiError) -> (Value, Value) {
    let st = flow::login_preparing_error_state(&error.code, error.retry_at, Some(error.status as i64));
    (json!(error.code), st)
}

impl Manager {
    pub fn new(env: Arc<dyn Env>, backoff: Backoff, handle: tokio::runtime::Handle) -> Arc<Self> {
        Arc::new(Manager {
            env,
            st: Mutex::new(State::default()),
            backoff: Mutex::new(backoff),
            listeners: Mutex::new(Vec::new()),
            mirrors: Mutex::new(Vec::new()),
            action_done: tokio::sync::Notify::new(),
            refresh_done: tokio::sync::Notify::new(),
            handle,
        })
    }

    fn client(&self) -> AuthClient {
        AuthClient::new(ClientOptions {
            base_url: self.env.auth_base_url(),
            region: AUTH_REGION.into(),
            device_id: self.env.device_id(),
            client_type: "desktop".into(),
            locale: Some(self.env.locale()),
            timeout_ms: 15_000,
            on_request_error: Some(Arc::new(|p, s, n| log("warn", &format!("auth request failed {p} status={s} {n}")))),
        })
    }

    // ── 快照与监听 ────────────────────────────────────────────────────────

    pub fn snapshot(&self) -> Value {
        let mode = self.env.mode();
        let s = self.st.lock().unwrap();
        let user = if mode == Mode::Cloud { s.current_user.clone() } else { None };
        let stamp = self.env.stamp();
        let user_json = match user {
            Some(mut u) => {
                u["orgSlug"] = org_slug_from_token(s.access_token.as_deref()).map(Value::from).unwrap_or(Value::Null);
                u
            }
            None => Value::Null,
        };
        json!({
            "user": user_json, "mode": mode.as_str(),
            "dataOwnerId": stamp["dataOwnerId"], "ownerGeneration": stamp["ownerGeneration"],
            "canEnterApp": mode != Mode::SignedOut,
            "isAuthenticated": mode == Mode::Cloud && s.access_token.is_some() && s.current_user.is_some(),
            "isCanary": false, "deviceId": self.env.device_id(),
            "hasAccountDeletionReceipt": false, "accountDeletionRestored": false,
            "credentialStoreUnavailable": s.credential_store_unavailable,
        })
    }

    fn notify_renderer(&self) {
        self.env.emit("auth:state-change", self.snapshot());
        self.push_mirror(false);
    }

    /// 镜像载荷：登录信息 + 当前 access token（sidecar 的云端模型请求要带它）。
    pub fn mirror_payload(&self) -> Value {
        let info = self.info();
        json!({
            "authenticated": info.authenticated, "userId": info.user_id, "membershipKind": info.membership_kind,
            "epoch": info.epoch, "accessToken": self.access_token(),
        })
    }

    fn push_mirror(&self, notify: bool) {
        let payload = self.mirror_payload();
        for f in self.mirrors.lock().unwrap().iter() {
            f(&payload, notify);
        }
    }

    pub fn on_mirror(&self, f: impl Fn(&Value, bool) + Send + Sync + 'static) {
        self.mirrors.lock().unwrap().push(Box::new(f));
    }

    pub fn info(&self) -> AuthInfo {
        let s = self.st.lock().unwrap();
        let authed = self.env.mode() == Mode::Cloud && s.access_token.is_some() && s.current_user.is_some();
        AuthInfo {
            authenticated: authed,
            user_id: authed.then(|| s.current_user.as_ref().and_then(|u| u["id"].as_str().map(String::from))).flatten(),
            membership_kind: authed.then(|| s.current_user.as_ref().and_then(|u| u["membershipKind"].as_str().map(String::from))).flatten(),
            epoch: s.auth_epoch,
        }
    }

    pub fn on_change(&self, f: impl Fn(&AuthInfo) + Send + Sync + 'static) {
        self.listeners.lock().unwrap().push(Box::new(f));
    }

    fn notify_listeners(&self) {
        self.push_mirror(true);
        let info = self.info();
        for f in self.listeners.lock().unwrap().iter() {
            f(&info);
        }
    }

    pub fn access_token(&self) -> Option<String> {
        if self.env.mode() == Mode::Cloud { self.st.lock().unwrap().access_token.clone() } else { None }
    }

    // ── 续期 ──────────────────────────────────────────────────────────────

    fn clear_refresh_timer(&self) {
        self.st.lock().unwrap().refresh_gen += 1;
    }

    fn spawn_timer(self: &Arc<Self>, delay_ms: f64) {
        let gen = {
            let mut s = self.st.lock().unwrap();
            s.refresh_gen += 1;
            s.refresh_gen
        };
        let me = self.clone();
        self.handle.spawn(async move {
            tokio::time::sleep(Duration::from_millis(delay_ms.clamp(0.0, 2_147_483_647.0) as u64)).await;
            if me.st.lock().unwrap().refresh_gen == gen {
                let _ = me.refresh().await;
            }
        });
    }

    fn schedule_refresh(self: &Arc<Self>, token: &str) {
        self.backoff.lock().unwrap().clear();
        self.clear_refresh_timer();
        let Some(exp) = jwt_claims(token).and_then(|c| c.get("exp").and_then(Value::as_f64)) else { return };
        let delay = (exp - 300.0) * 1000.0 - now_ms();
        if !delay.is_finite() {
            return;
        }
        if delay <= 0.0 {
            let me = self.clone();
            self.handle.spawn(async move {
                let _ = me.refresh().await;
            });
        } else {
            self.spawn_timer(delay);
        }
    }

    fn schedule_retry(self: &Arc<Self>, epoch: u64, retry_at: Option<f64>, network: bool) {
        self.clear_refresh_timer();
        let delay = self.backoff.lock().unwrap().defer(epoch as i64, retry_at, network);
        self.spawn_timer(delay.max(1000.0));
    }

    fn clear_session_memory(&self) {
        self.clear_refresh_timer();
        self.backoff.lock().unwrap().clear();
        let mut s = self.st.lock().unwrap();
        s.access_token = None;
        s.latest_refresh_token = None;
        s.current_user = None;
    }

    /// 会话失效：回到未登录。确定性失效才删 refresh token。
    pub async fn expire_session(self: &Arc<Self>, reason: &str, remove_token: bool, notify_expired: bool) {
        {
            self.st.lock().unwrap().auth_epoch += 1;
        }
        self.clear_session_memory();
        if remove_token {
            if let Err(e) = self.env.secret_remove(AUTH_SESSION_KEY) {
                log("warn", &format!("remove auth session failed: {e}"));
            }
        }
        if let Err(e) = self.env.commit(Mode::SignedOut, None) {
            log("warn", &format!("commit signed-out failed: {e}"));
        }
        self.reset_login_flow_state();
        self.notify_renderer();
        if notify_expired {
            self.env.emit("auth:session-expired", json!({ "message": "", "reason": if reason.is_empty() { "unknown" } else { reason } }));
        }
        self.notify_listeners();
    }

    async fn read_stored_refresh_token(&self) -> Result<String, &'static str> {
        if let Some(t) = self.st.lock().unwrap().latest_refresh_token.clone() {
            return Ok(t);
        }
        if !self.env.secrets_ready().await {
            return Err("unavailable");
        }
        if let Some((realm, tok)) = flow::parse_session_record(self.env.secret_get(AUTH_SESSION_KEY).as_deref()) {
            if realm == AUTH_REGION {
                return Ok(tok);
            }
        }
        // 条目在但读不出：按瞬时故障处理，不踢人。
        if self.env.secret_has(AUTH_SESSION_KEY) { Err("unavailable") } else { Err("missing") }
    }

    async fn run_refresh(self: &Arc<Self>, cold_start: bool) -> bool {
        let epoch = self.st.lock().unwrap().auth_epoch;
        let stored = self.read_stored_refresh_token().await;
        if epoch != self.st.lock().unwrap().auth_epoch {
            return false;
        }
        let token = match stored {
            Ok(t) => t,
            Err("unavailable") => {
                log("warn", "refresh token unreadable (secret store unavailable); retrying later");
                let first = {
                    let mut s = self.st.lock().unwrap();
                    !std::mem::replace(&mut s.credential_store_unavailable, true)
                };
                if first {
                    self.notify_renderer();
                }
                self.schedule_retry(epoch, None, false);
                return false;
            }
            Err(_) => {
                self.expire_session("credential-lost", false, !cold_start).await;
                return false;
            }
        };
        match self.client().refresh(&token).await {
            Ok(pair) => {
                if epoch != self.st.lock().unwrap().auth_epoch {
                    return false;
                }
                let rt = pair["refreshToken"].as_str().unwrap_or("").to_string();
                self.st.lock().unwrap().latest_refresh_token = Some(rt.clone());
                let persisted = self.env.secret_set(AUTH_SESSION_KEY, &flow::serialize_session_record(AUTH_REGION, &rt));
                self.st.lock().unwrap().credential_store_unavailable = persisted.is_err();
                if let Err(e) = persisted {
                    log("warn", &format!("persist rotated refresh token failed: {e}"));
                }
                if epoch != self.st.lock().unwrap().auth_epoch {
                    return false;
                }
                let (changed, user) = {
                    let mut s = self.st.lock().unwrap();
                    let previous = s.access_token.as_ref().and_then(|_| s.current_user.as_ref().map(|u| u["id"].clone()));
                    s.access_token = pair["accessToken"].as_str().map(String::from);
                    let u = merge_membership(&pair["membership"], s.current_user.as_ref());
                    let changed = previous.as_ref() != Some(&u["id"]);
                    s.current_user = Some(u.clone());
                    (changed, u)
                };
                let _ = self.env.commit(Mode::Cloud, Some(&user));
                self.schedule_refresh(pair["accessToken"].as_str().unwrap_or(""));
                self.notify_renderer();
                if changed {
                    self.notify_listeners();
                }
                true
            }
            Err(err) => {
                if epoch != self.st.lock().unwrap().auth_epoch {
                    return false;
                }
                let (code, status) = api_code(&err);
                log("warn", &format!("refresh failed status={status} code={code}"));
                if let Some(reason) = definitive_refresh_failure(&code) {
                    self.expire_session(reason, true, !cold_start).await;
                    return false;
                }
                if code == "DEVICE_MISMATCH" {
                    self.expire_session("device-mismatch", false, !cold_start).await;
                    return false;
                }
                self.schedule_retry(epoch, err.retry_at, status == 0);
                false
            }
        }
    }

    /// 续期 access token。本机 / 未登录模式返回 false；并发调用共用一次请求。
    pub async fn refresh(self: &Arc<Self>) -> bool {
        if self.env.mode() != Mode::Cloud {
            return false;
        }
        loop {
            let waiter = self.refresh_done.notified();
            tokio::pin!(waiter);
            waiter.as_mut().enable();
            {
                let mut s = self.st.lock().unwrap();
                if !s.refreshing {
                    let epoch = s.auth_epoch as i64;
                    let wait = self.backoff.lock().unwrap().runtime_remaining(epoch);
                    if wait > 0.0 {
                        return false;
                    }
                    s.refreshing = true;
                    s.refresh_gen += 1;
                    self.backoff.lock().unwrap().start_runtime_attempt(epoch);
                    break;
                }
            }
            // 已有续期在飞：等它结束，共用结果（JS 里返回同一个 promise）。
            waiter.await;
            return self.st.lock().unwrap().access_token.is_some();
        }
        let ok = self.run_refresh(false).await;
        self.st.lock().unwrap().refreshing = false;
        self.refresh_done.notify_waiters();
        ok
    }

    // ── 启动 ──────────────────────────────────────────────────────────────

    pub async fn initialize(self: &Arc<Self>) -> Value {
        if self.env.mode() != Mode::Cloud {
            return self.snapshot();
        }
        {
            let mut s = self.st.lock().unwrap();
            if s.access_token.is_some() && s.current_user.is_some() {
                drop(s);
                return self.snapshot();
            }
            if s.current_user.is_none() {
                s.current_user = self.env.cached_user().map(|c| user_from_cache(&c));
            }
        }
        let start = {
            let mut s = self.st.lock().unwrap();
            !std::mem::replace(&mut s.refreshing, true)
        };
        if start {
            let me = self.clone();
            self.handle.spawn(async move {
                let _ = me.run_refresh(true).await;
                me.st.lock().unwrap().refreshing = false;
                me.refresh_done.notify_waiters();
            });
        }
        let waiter = self.refresh_done.notified();
        tokio::pin!(waiter);
        waiter.as_mut().enable();
        if self.st.lock().unwrap().refreshing {
            tokio::select! {
                _ = waiter => {}
                _ = tokio::time::sleep(Duration::from_millis(COLD_START_GATE_MS)) => {
                    log("warn", &format!("cold start refresh still pending after {COLD_START_GATE_MS}ms"));
                }
            }
        }
        self.snapshot()
    }
}

// ── 登录流程 ──────────────────────────────────────────────────────────────

type Settled = Value; // {success, state, code?, retryAt?}

impl Manager {
    fn assert_flow_current(&self, epoch: u64) -> Result<(), AuthApiError> {
        if self.st.lock().unwrap().login_epoch != epoch { Err(superseded()) } else { Ok(()) }
    }

    fn reset_login_flow_state(&self) {
        let mut s = self.st.lock().unwrap();
        s.login_state = None;
        s.discovered.clear();
        s.pending_account_token = None;
        s.pending_login_ticket = None;
        s.pending_bind_ticket = None;
        s.pending_sso_ticket = None;
    }

    fn cancel_browser(&self) -> bool {
        match self.st.lock().unwrap().browser_cancel.take() {
            Some(c) => {
                c.abort();
                true
            }
            None => false,
        }
    }

    fn set_flow(&self, action: Value) -> Value {
        let st = flow::reduce_auth_flow(&action);
        self.st.lock().unwrap().login_state = Some(st.clone());
        st
    }

    fn login_state_value(&self) -> Value {
        self.st.lock().unwrap().login_state.clone().unwrap_or(Value::Null)
    }

    async fn load_login_providers(self: &Arc<Self>, epoch: u64) -> Result<Value, AuthApiError> {
        self.reset_login_flow_state();
        let me = self.clone();
        let fetch = tokio::spawn(async move { me.client().get_providers().await });
        let providers = match tokio::time::timeout(Duration::from_millis(LOGIN_PROVIDERS_GATE_MS), fetch).await {
            Ok(Ok(r)) => r?,
            Ok(Err(_)) => return Err(AuthApiError::new("AUTH_SERVICE_UNAVAILABLE", 0, "Authentication server did not respond in time")),
            Err(_) => return Err(AuthApiError::new("AUTH_SERVICE_UNAVAILABLE", 0, "Authentication server did not respond in time")),
        };
        self.assert_flow_current(epoch)?;
        self.st.lock().unwrap().providers = Some(providers.clone());
        Ok(self.set_flow(json!({ "type": "providers-loaded", "providers": providers })))
    }

    fn providers_failure(&self, e: &AuthApiError) -> Value {
        let code = if e.code.is_empty() { "AUTH_SERVICE_UNAVAILABLE" } else { &e.code };
        let state = flow::login_preparing_error_state(code, e.retry_at, None);
        let mut m = Map::new();
        m.insert("success".into(), json!(false));
        m.insert("code".into(), json!(code));
        if let Some(r) = state.get("retryAt") {
            m.insert("retryAt".into(), r.clone());
        }
        m.insert("state".into(), state);
        Value::Object(m)
    }

    pub async fn get_login_state(self: &Arc<Self>) -> Value {
        let epoch = self.st.lock().unwrap().login_epoch;
        if let Some(s) = self.st.lock().unwrap().login_state.clone() {
            return json!({ "success": true, "state": s });
        }
        match self.load_login_providers(epoch).await {
            Ok(state) => json!({ "success": true, "state": state }),
            Err(error) => {
                if epoch != self.st.lock().unwrap().login_epoch {
                    return json!({ "success": false, "code": "AUTH_FLOW_SUPERSEDED", "state": self.login_state_value() });
                }
                // 非 AuthApiError（超时 join 失败）在这里已被归一成 AUTH_SERVICE_UNAVAILABLE。
                let f = self.providers_failure(&error);
                log("warn", &format!("load login providers failed code={}", f["code"].as_str().unwrap_or("")));
                self.st.lock().unwrap().login_state = Some(f["state"].clone());
                f
            }
        }
    }

    async fn assert_credential_store_ready(&self) -> Result<(), AuthApiError> {
        if self.env.secrets_ready().await {
            Ok(())
        } else {
            Err(AuthApiError::new("CREDENTIAL_STORE_UNAVAILABLE", 503, "Saved account credentials are temporarily unavailable"))
        }
    }

    async fn complete_login(self: &Arc<Self>, outcome: &Value, epoch: u64) -> Result<Value, AuthApiError> {
        self.assert_flow_current(epoch)?;
        self.st.lock().unwrap().commit_sealed = true;
        let r = self.complete_login_inner(outcome, epoch).await;
        self.st.lock().unwrap().commit_sealed = false;
        r
    }

    async fn complete_login_inner(self: &Arc<Self>, outcome: &Value, epoch: u64) -> Result<Value, AuthApiError> {
        let login_epoch = {
            let mut s = self.st.lock().unwrap();
            s.auth_epoch += 1;
            s.auth_epoch
        };
        let rt = outcome["refreshToken"].as_str().unwrap_or("");
        if self.env.secret_set(AUTH_SESSION_KEY, &flow::serialize_session_record(AUTH_REGION, rt)).is_err() {
            return Err(AuthApiError::new("CREDENTIAL_STORE_UNAVAILABLE", 503, "Saved account credentials are temporarily unavailable"));
        }
        {
            let s = self.st.lock().unwrap();
            if login_epoch != s.auth_epoch || epoch != s.login_epoch {
                return Err(superseded());
            }
        }
        self.clear_session_memory();
        let user = map_membership(&outcome["membership"]);
        {
            let mut s = self.st.lock().unwrap();
            s.access_token = outcome["accessToken"].as_str().map(String::from);
            s.latest_refresh_token = Some(rt.to_string());
            s.current_user = Some(user.clone());
            s.credential_store_unavailable = false;
        }
        self.cancel_browser();
        let _ = self.env.commit(Mode::Cloud, Some(&user));
        self.schedule_refresh(outcome["accessToken"].as_str().unwrap_or(""));
        self.reset_login_flow_state();
        let state = self.set_flow(json!({ "type": "outcome", "outcome": outcome }));
        self.notify_renderer();
        self.notify_listeners();
        Ok(state)
    }

    async fn accept_login_outcome(self: &Arc<Self>, outcome: &Value, epoch: u64) -> Result<Value, AuthApiError> {
        self.assert_flow_current(epoch)?;
        let status = outcome["status"].as_str().unwrap_or("");
        {
            let mut s = self.st.lock().unwrap();
            s.pending_account_token = if status == "select_account" { outcome["accountToken"].as_str().map(String::from) } else { None };
        }
        if status == "ok" {
            return self.complete_login(outcome, epoch).await;
        }
        {
            let mut s = self.st.lock().unwrap();
            s.pending_login_ticket = (status == "select_account").then(|| outcome["loginTicket"].as_str().map(String::from)).flatten();
            s.pending_bind_ticket = (status == "binding_required").then(|| outcome["bindTicket"].as_str().map(String::from)).flatten();
            s.pending_sso_ticket = (status == "sso_verification_required").then(|| outcome["verificationTicket"].as_str().map(String::from)).flatten();
        }
        Ok(self.set_flow(json!({ "type": "outcome", "outcome": outcome })))
    }

    async fn launch_browser(&self, url: &str, cancel: &Cancel) -> Result<(), String> {
        if cancel.aborted() {
            return Err("USER_CANCELLED".into());
        }
        let open = self.env.open_url(url);
        tokio::select! {
            r = open => r.map_err(|_| "BROWSER_OPEN_FAILED".to_string()),
            _ = tokio::time::sleep(Duration::from_millis(BROWSER_LAUNCH_TIMEOUT_MS)) => Err("BROWSER_OPEN_TIMEOUT".into()),
            _ = cancel.cancelled() => Err("USER_CANCELLED".into()),
        }
    }

    async fn browser_authorization(self: &Arc<Self>, client: &Arc<AuthClient>, kind: &str, id: &str, challenge: &str, cancel: &Cancel) -> hosted::Settled {
        let creds = hosted::create_poll_credentials();
        let url = client.build_authorize_url(kind, id, &self.env.desktop_callback_url(), challenge, &creds.client_state);
        let started = std::time::Instant::now();
        self.launch_browser(&url, cancel).await?;
        let secret = creds.poll_secret.clone();
        let (c2, cancel2) = (client.clone(), cancel.clone());
        let remaining = BROWSER_AUTH_TIMEOUT_MS.saturating_sub(started.elapsed().as_millis() as u64);
        hosted::run_polling(PollingDeps {
            poll: move || {
                let (c, cancel, secret) = (c2.clone(), cancel2.clone(), secret.clone());
                async move {
                    c.poll_desktop_authorization(&secret, &cancel).await.map_err(|e| {
                        if !cancel.aborted() {
                            log("warn", &format!("hosted auth callback poll failed code={}", e.code));
                        }
                        PollFailure(Some(e.code))
                    })
                }
            },
            now: move || started.elapsed().as_millis() as u64,
            cancel: cancel.clone(),
            timeout_ms: remaining,
            max_failures: hosted::MAX_CONSECUTIVE_FAILURES,
        })
        .await
    }

    async fn start_browser(self: &Arc<Self>, client: &Arc<AuthClient>, action: &Value, epoch: u64) -> Result<Value, AuthApiError> {
        self.assert_credential_store_ready().await?;
        let kind = action["kind"].as_str().unwrap_or("");
        let id = action["providerOrConnectionId"].as_str().unwrap_or("");
        if kind == "social" {
            let ok = self.st.lock().unwrap().providers.as_ref().and_then(|p| p["social"].as_array()).map_or(false, |a| a.iter().any(|x| x.as_str() == Some(id)));
            if !ok {
                return Err(AuthApiError::new("SOCIAL_PROVIDER_DISABLED", 400, "Provider is disabled"));
            }
        } else {
            let ok = self.st.lock().unwrap().discovered.iter().any(|m| m["type"] == "sso" && m["connectionId"].as_str() == Some(id));
            if !ok {
                return Err(AuthApiError::new("CONNECTION_NOT_FOUND", 404, "SSO connection is unavailable"));
            }
        }
        let mut b = [0u8; 32];
        getrandom::fill(&mut b).map_err(|_| AuthApiError::new("AUTH_REQUEST_FAILED", 0, "random unavailable"))?;
        let verifier = URL_SAFE_NO_PAD.encode(b);
        let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
        self.set_flow(json!({ "type": "browser-started", "label": action["label"] }));
        self.cancel_browser();
        let cancel = Cancel::new();
        self.st.lock().unwrap().browser_cancel = Some(cancel.clone());
        let result = async {
            let callback = self.browser_authorization(client, kind, id, &challenge, &cancel).await;
            self.assert_flow_current(epoch)?;
            let code = callback.map_err(|e| AuthApiError::new(&e, 0, "Browser authorization did not complete"))?;
            if cancel.aborted() {
                return Err(AuthApiError::new("USER_CANCELLED", 0, "Browser authorization was cancelled"));
            }
            let outcome = client.exchange_authorization_code(&code, &verifier).await?;
            if cancel.aborted() {
                return Err(AuthApiError::new("USER_CANCELLED", 0, "Browser authorization was cancelled"));
            }
            self.accept_login_outcome(&outcome, epoch).await
        }
        .await;
        {
            // 只清自己的那份（后来者已经换了就不动）。
            let mut s = self.st.lock().unwrap();
            if s.browser_cancel.as_ref().map_or(false, |c| Arc::ptr_eq(&c.flag_arc(), &cancel.flag_arc())) {
                s.browser_cancel = None;
            }
        }
        result
    }

    async fn discover_org_same_region(&self, client: &AuthClient, org: &str) -> Result<Value, AuthApiError> {
        let d = client.discover_sso_org(org).await?;
        if d["region"].as_str() != Some(AUTH_REGION) {
            return Err(AuthApiError::new("ORG_SSO_NOT_FOUND", 404, "Organization is in another region"));
        }
        Ok(d)
    }
}

impl Manager {
    fn ok_state(state: Value) -> Settled {
        json!({ "success": true, "state": state })
    }

    async fn run_login_action(self: &Arc<Self>, action: Value) -> Settled {
        let epoch = self.st.lock().unwrap().login_epoch;
        let client = Arc::new(self.client());
        let before = {
            let s = self.st.lock().unwrap();
            match &s.login_state {
                Some(st) if st["step"] != "error" => Some(st.clone()),
                _ => None,
            }
        };
        let t = action["type"].as_str().unwrap_or("").to_string();
        let r = self.login_action_inner(&client, &action, &t, epoch).await;
        match r {
            Ok(v) => v,
            Err(error) => {
                if self.st.lock().unwrap().login_epoch != epoch {
                    return json!({ "success": false, "code": "AUTH_FLOW_SUPERSEDED", "state": self.login_state_value() });
                }
                let (code, status) = api_code(&error);
                log("warn", &format!("login action failed action={t} status={status} code={code}"));
                let cannot_retry = flow_cannot_retry(&code);
                if cannot_retry {
                    let mut s = self.st.lock().unwrap();
                    s.pending_account_token = None;
                    s.pending_login_ticket = None;
                    s.pending_bind_ticket = None;
                    s.pending_sso_ticket = None;
                }
                let error_state = flow::login_preparing_error_state(&code, error.retry_at, Some(status as i64));
                let next = if cannot_retry || code == "CREDENTIAL_STORE_UNAVAILABLE" { error_state.clone() } else { before.unwrap_or_else(|| error_state.clone()) };
                self.st.lock().unwrap().login_state = Some(next.clone());
                let mut m = Map::new();
                m.insert("success".into(), json!(false));
                m.insert("code".into(), json!(code));
                m.insert("state".into(), next);
                if let Some(r) = error_state.get("retryAt") {
                    m.insert("retryAt".into(), r.clone());
                }
                Value::Object(m)
            }
        }
    }

    async fn login_action_inner(self: &Arc<Self>, client: &Arc<AuthClient>, action: &Value, t: &str, epoch: u64) -> Result<Settled, AuthApiError> {
        let s = |k: &str| action[k].as_str().unwrap_or("").to_string();
        match t {
            "cancel-browser" => return Err(AuthApiError::new("INVALID_AUTH_ACTION", 400, "Unexpected browser cancellation")),
            "reset" => {
                // 云端模式下冷启动续期还没成功：先重试一次，避免用新登录替换仍然有效的会话。
                if self.env.mode() == Mode::Cloud && self.st.lock().unwrap().access_token.is_none() {
                    let e = self.st.lock().unwrap().auth_epoch as i64;
                    self.backoff.lock().unwrap().release(e);
                    self.refresh().await;
                    self.assert_flow_current(epoch)?;
                    let (has, st) = {
                        let g = self.st.lock().unwrap();
                        (g.access_token.is_some() && g.current_user.is_some(), g.login_state.clone())
                    };
                    if has {
                        return Ok(Self::ok_state(st.unwrap_or_else(|| json!({ "step": "entry" }))));
                    }
                }
                return Ok(Self::ok_state(self.load_login_providers(epoch).await?));
            }
            "confirm-sso-realm" => return Err(AuthApiError::new("INVALID_AUTH_ACTION", 400, "No enterprise region switch is waiting for confirmation")),
            "cancel-sso-realm" => return Err(AuthApiError::new("INVALID_AUTH_ACTION", 400, "No enterprise region switch is waiting for cancellation")),
            _ => {}
        }
        if self.st.lock().unwrap().providers.is_none() {
            self.load_login_providers(epoch).await?;
        }
        match t {
            "discover" => {
                self.st.lock().unwrap().discovered.clear();
                let email = js_trim(&s("email")).to_lowercase();
                if email.encode_utf16().count() > 254 || !flow::is_valid_email(&email) {
                    return Err(AuthApiError::new("INVALID_PARAMS", 400, "Invalid email address"));
                }
                // 只把域名发给组织发现；跨区域的组织按“未找到”处理（只有 global 一个区域）。
                let domain = &email[email.find('@').map_or(0, |i| i + 1)..];
                match self.discover_org_same_region(client, domain).await {
                    Ok(_) => {}
                    Err(e) if e.code == "ORG_SSO_NOT_FOUND" => {}
                    Err(e) => return Err(e),
                }
                // 同区域的企业与“未找到”都走个人登录方式（上游 discoverEmailLogin 在非跨区域时的行为）。
                let methods = client.discover(&email).await?;
                self.assert_flow_current(epoch)?;
                self.st.lock().unwrap().discovered = methods.clone();
                Ok(Self::ok_state(self.set_flow(json!({ "type": "discovery-loaded", "email": email, "methods": methods }))))
            }
            "discover-sso-org" => {
                let d = self.discover_org_same_region(client, &js_trim(&s("org")).to_lowercase()).await?;
                self.assert_flow_current(epoch)?;
                let methods: Vec<Value> = d["connections"].as_array().cloned().unwrap_or_default().iter().map(|c| json!({
                    "type": "sso", "connectionId": c["connectionId"], "protocol": c["protocol"],
                    "orgName": d["orgName"], "connectionName": c["connectionName"], "ssoRequired": false,
                })).collect();
                self.st.lock().unwrap().discovered = methods.clone();
                Ok(Self::ok_state(self.set_flow(json!({ "type": "discovery-loaded", "email": "", "methods": methods }))))
            }
            "request-code" => {
                let kind = s("kind");
                let phone_ok = self.st.lock().unwrap().providers.as_ref().map_or(false, |p| p["phone"] == true);
                if kind == "phone" && !phone_ok {
                    return Err(AuthApiError::new("PHONE_LOGIN_DISABLED", 400, "Phone login is disabled"));
                }
                self.assert_credential_store_ready().await?;
                client.request_code(&kind, &s("identifier"), action["captchaToken"].as_str()).await?;
                self.assert_flow_current(epoch)?;
                self.st.lock().unwrap().discovered.clear();
                Ok(Self::ok_state(self.set_flow(json!({ "type": "code-requested", "kind": kind, "identifier": s("identifier") }))))
            }
            "verify-code" => {
                self.assert_credential_store_ready().await?;
                let outcome = client.verify_code(&s("kind"), &s("identifier"), &s("code")).await?;
                Ok(Self::ok_state(self.accept_login_outcome(&outcome, epoch).await?))
            }
            "start-browser" => Ok(Self::ok_state(self.start_browser(client, action, epoch).await?)),
            "select-account" => {
                let pending = self.st.lock().unwrap().pending_account_token.clone();
                if let Some(tok) = pending {
                    let pair = client.exchange_account_membership(&tok, &s("accountId")).await?;
                    self.assert_flow_current(epoch)?;
                    self.st.lock().unwrap().pending_account_token = None;
                    let mut outcome = pair.clone();
                    outcome["status"] = json!("ok");
                    return Ok(Self::ok_state(self.complete_login(&outcome, epoch).await?));
                }
                let ticket = self.st.lock().unwrap().pending_login_ticket.clone();
                let ticket = ticket.ok_or_else(|| AuthApiError::new("INVALID_LOGIN_TICKET", 401, "Missing login ticket"))?;
                let outcome = client.select_account(&ticket, &s("accountId")).await?;
                Ok(Self::ok_state(self.accept_login_outcome(&outcome, epoch).await?))
            }
            "request-sso-verification-code" | "verify-sso-verification" => {
                let (ticket, st) = {
                    let g = self.st.lock().unwrap();
                    (g.pending_sso_ticket.clone(), g.login_state.clone())
                };
                let (Some(ticket), Some(st)) = (ticket, st.filter(|x| x["step"] == "sso-verification")) else {
                    return Err(AuthApiError::new("INVALID_SSO_VERIFICATION_TICKET", 401, "Missing SSO verification ticket"));
                };
                if t == "verify-sso-verification" {
                    let outcome = client.verify_sso_verification(&ticket, &s("code")).await?;
                    return Ok(Self::ok_state(self.accept_login_outcome(&outcome, epoch).await?));
                }
                client.request_sso_verification_code(&ticket).await?;
                self.assert_flow_current(epoch)?;
                Ok(Self::ok_state(self.set_flow(json!({
                    "type": "sso-verification-code-requested", "channel": st["channel"], "targetMasked": st["targetMasked"],
                }))))
            }
            _ => {
                // request-binding-code / verify-binding
                let (ticket, st) = {
                    let g = self.st.lock().unwrap();
                    (g.pending_bind_ticket.clone(), g.login_state.clone())
                };
                let (Some(ticket), Some(st)) = (ticket, st.filter(|x| x["step"] == "binding")) else {
                    return Err(AuthApiError::new("INVALID_BIND_TICKET", 401, "Missing binding ticket"));
                };
                let bind_type = st["bindType"].as_str().unwrap_or("").to_string();
                if t == "request-binding-code" {
                    client.request_binding_code(&ticket, &bind_type, &s("contact")).await?;
                    self.assert_flow_current(epoch)?;
                    return Ok(Self::ok_state(self.set_flow(json!({ "type": "binding-code-requested", "bindType": bind_type, "contact": s("contact") }))));
                }
                let outcome = client.verify_binding(&ticket, &bind_type, &s("contact"), &s("code")).await?;
                Ok(Self::ok_state(self.accept_login_outcome(&outcome, epoch).await?))
            }
        }
    }

    pub async fn dispatch_login_action(self: &Arc<Self>, raw: &Value) -> Settled {
        let Some(action) = flow::parse_login_action(raw) else {
            return json!({ "success": false, "code": "INVALID_AUTH_ACTION", "state": self.login_state_value() });
        };
        let t = action["type"].as_str().unwrap_or("");
        if t == "back" {
            if self.st.lock().unwrap().commit_sealed {
                return json!({ "success": false, "code": "LOGIN_BUSY", "state": self.login_state_value() });
            }
            let providers = self.st.lock().unwrap().providers.clone();
            self.st.lock().unwrap().login_epoch += 1;
            self.cancel_browser();
            self.reset_login_flow_state();
            self.st.lock().unwrap().providers = providers.clone();
            return match providers {
                Some(p) => Self::ok_state(self.set_flow(json!({ "type": "providers-loaded", "providers": p }))),
                None => Self::ok_state(json!({ "step": "entry" })),
            };
        }
        if t == "cancel-browser" {
            let pending = {
                let g = self.st.lock().unwrap();
                g.action_running == Some(g.login_epoch)
            };
            let cancelled = self.cancel_browser();
            if !cancelled && !pending {
                return json!({ "success": false, "code": "NO_BROWSER_AUTH_IN_PROGRESS", "state": self.login_state_value() });
            }
            if pending {
                // 等在跑的动作收尾，用它的最终状态。
                loop {
                    let w = self.action_done.notified();
                    tokio::pin!(w);
                    w.as_mut().enable();
                    if self.st.lock().unwrap().action_running.is_none() {
                        break;
                    }
                    w.await;
                }
            }
            let st = self.st.lock().unwrap().login_state.clone();
            let epoch = self.st.lock().unwrap().login_epoch;
            return Self::ok_state(match st {
                Some(s) => s,
                None => self.load_login_providers(epoch).await.unwrap_or(Value::Null),
            });
        }
        {
            let mut g = self.st.lock().unwrap();
            if g.action_running == Some(g.login_epoch) {
                return json!({ "success": false, "code": "LOGIN_BUSY", "state": g.login_state.clone().unwrap_or(Value::Null) });
            }
            g.action_running = Some(g.login_epoch);
        }
        let r = self.run_login_action(action).await;
        self.st.lock().unwrap().action_running = None;
        self.action_done.notify_waiters();
        r
    }

    // ── 退出 / 本机模式 ───────────────────────────────────────────────────

    pub async fn logout(self: &Arc<Self>) -> Result<(), String> {
        if self.env.mode() != Mode::Cloud || self.st.lock().unwrap().current_user.is_none() {
            return Err("[UNAUTHENTICATED] No current account to log out".into());
        }
        let token = self.st.lock().unwrap().access_token.clone();
        {
            let mut s = self.st.lock().unwrap();
            s.auth_epoch += 1;
            s.login_epoch += 1;
        }
        self.cancel_browser();
        self.clear_session_memory();
        // 服务端注销尽力而为、限时；本地凭据无论如何都清掉。
        if let Some(t) = token {
            let c = self.client();
            match tokio::time::timeout(Duration::from_millis(LOGOUT_REQUEST_TIMEOUT_MS), c.logout(&t)).await {
                Ok(Ok(())) => {}
                Ok(Err(e)) => log("warn", &format!("server logout failed: {}", e.code)),
                Err(_) => log("warn", "server logout failed: logout request timed out"),
            }
        }
        if let Err(e) = self.env.secret_remove(AUTH_SESSION_KEY) {
            log("warn", &format!("remove auth session failed: {e}"));
        }
        let _ = self.env.commit(Mode::SignedOut, None);
        self.reset_login_flow_state();
        self.st.lock().unwrap().providers = None;
        self.notify_renderer();
        self.notify_listeners();
        Ok(())
    }

    pub fn enter_local(self: &Arc<Self>) -> Value {
        if self.env.mode() == Mode::Local {
            return self.snapshot();
        }
        let was_cloud = self.env.mode() == Mode::Cloud;
        {
            let mut s = self.st.lock().unwrap();
            s.auth_epoch += 1;
            s.login_epoch += 1;
        }
        self.cancel_browser();
        self.clear_session_memory();
        let _ = self.env.commit(Mode::Local, None);
        self.reset_login_flow_state();
        self.notify_renderer();
        if was_cloud {
            self.notify_listeners();
        }
        self.snapshot()
    }

    pub fn exit_local(&self) -> Result<Value, String> {
        if self.env.mode() != Mode::Local {
            return Err("[PRECONDITION_FAILED] Local mode is not active.".into());
        }
        self.env.commit(Mode::SignedOut, None)?;
        self.notify_renderer();
        Ok(self.snapshot())
    }

    async fn begin_add_account(self: &Arc<Self>) -> Result<Value, String> {
        if self.env.mode() == Mode::Cloud {
            return Err("[NOT_SUPPORTED] Multiple accounts are not supported in the Tauri build yet".into());
        }
        self.st.lock().unwrap().login_epoch += 1;
        self.cancel_browser();
        self.reset_login_flow_state();
        Ok(self.get_login_state().await)
    }

    fn cancel_add_account(&self) {
        if self.st.lock().unwrap().commit_sealed {
            log("info", "add-account close ignored after accepted login commit began");
            return;
        }
        self.st.lock().unwrap().login_epoch += 1;
        self.cancel_browser();
        self.reset_login_flow_state();
    }

    /// sidecar 的反向调用（model-access / device-link 用 Rust 的账号会话）。阻塞调用，调用方在独立线程里。
    pub fn handle_native(self: &Arc<Self>, method: &str, params: &Value) -> Option<Result<Value, String>> {
        match method {
            "auth.refresh" => Some(Ok(json!(tauri::async_runtime::block_on(self.refresh())))),
            "auth.expire" => {
                let reason = params.get("reason").and_then(Value::as_str).unwrap_or("unknown");
                tauri::async_runtime::block_on(self.expire_session(reason, true, true));
                Some(Ok(Value::Null))
            }
            _ => None,
        }
    }

    pub fn dispose(&self) {
        self.clear_refresh_timer();
        self.cancel_browser();
    }

    // ── IPC ───────────────────────────────────────────────────────────────

    pub fn owns(channel: &str) -> bool {
        channel.starts_with("auth:") && channel != "auth:has-persisted-session-hint-sync"
    }

    pub fn owns_sync(channel: &str) -> bool {
        channel == "auth:has-persisted-session-hint-sync"
    }

    pub fn invoke_sync(&self, channel: &str) -> Option<Value> {
        (channel == "auth:has-persisted-session-hint-sync").then(|| json!(self.env.mode() == Mode::Cloud))
    }

    pub async fn invoke(self: &Arc<Self>, channel: &str, args: &[Value]) -> Option<Result<Value, String>> {
        let a0 = args.first().cloned().unwrap_or(Value::Null);
        let unsupported_deletion = json!({ "success": false, "code": "ACCOUNT_DELETION_UNAVAILABLE" });
        Some(match channel {
            "auth:initialize" => Ok(self.initialize().await),
            "auth:get-login-state" => Ok(self.get_login_state().await),
            "auth:dispatch-login-action" => Ok(self.dispatch_login_action(&a0).await),
            "auth:get-captcha-challenge-url" => Ok(json!(format!("{}/captcha/turnstile", self.env.auth_base_url()))),
            "auth:logout" => self.logout().await.map(|_| Value::Null),
            "auth:refresh" => Ok(json!(self.refresh().await)),
            "auth:enter-local" => Ok(self.enter_local()),
            "auth:exit-local" => self.exit_local(),
            "auth:accounts:list" | "auth:accounts:sync" => Ok(json!({ "accounts": [], "mutationAllowed": false })),
            "auth:accounts:switch" => Err("[NOT_SUPPORTED] Multiple accounts are not supported in the Tauri build yet".into()),
            "auth:accounts:begin-add" => self.begin_add_account().await,
            "auth:accounts:cancel-add" => {
                self.cancel_add_account();
                Ok(Value::Null)
            }
            "auth:account-deletion:get-availability" | "auth:account-deletion:request-challenge" | "auth:account-deletion:confirm" => Ok(unsupported_deletion),
            "auth:account-deletion:get-status" => Ok(json!({ "success": true, "value": null })),
            "auth:account-deletion:clear-receipt" => Ok(Value::Null),
            "auth:account-deletion:consume-restored-notice" => Ok(json!(false)),
            _ => return None,
        })
    }
}
