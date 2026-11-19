//! Cindy AI 云端模型的状态机装配：逐行移植 sidecar/src/model-access.mjs（它本身是 desktop
//! main/model-access/index.ts + billing + xd 投影的精简版）。纯逻辑在 `credsync` / `models_sync` /
//! `xd_catalog` / `billing` 里，这里只做 IO 装配：
//! - 服务端请求（`/api/model-access/*`），401 可续期时先续期一次再重试；
//! - 凭据元数据落 `<dataDir>/model-access-credentials.json`，网关 key 落密钥库的 `api_key`；
//! - 凭据就绪后拉 v5 模型清单，投影成 xd provider 的 Pi 清单；失败保留上次可用清单；
//! - 退出云端模式（登出 / 会话失效 / 进入本机模式）时删掉网关 key，下次登录重新下发；
//! - `model-access:*` 与 `billing:get-*` 通道，状态推送走 `model-access:status-change`。
use super::credsync::{CredSync, Deps, Status};
use super::models_sync::{parse_models_sync_payload, without_stale_payment_upsell};
use super::xd_catalog;
use crate::auth::manager::{Manager as AuthManager, AUTH_REGION};
use crate::owner::{Mode, Owner};
use crate::secret_store::SecretStore;
use serde_json::{json, Map, Value};
use super::credsync::BoxFut;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::watch;
use tokio::runtime::Handle;

pub const XD_PROVIDER_ID: &str = "xd";
/// 与 desktop shared/providerSecrets.ts STORAGE_KEYS.xd 一致。
pub const XD_KEY_NAME: &str = "api_key";
pub const STATUS_CHANNEL: &str = "model-access:status-change";
const CREDENTIALS_PATH: &str = "/api/model-access/credentials";
const BALANCE_PATH: &str = "/api/model-access/balance";
const CREDIT_USAGE_PATH: &str = "/api/model-access/credit-usage";
const MODELS_SYNC_PATH: &str = "/api/model-access/models?schemaVersion=5";
/// 上游 buildModelsSyncRequest 的声明头：只表示本端解析能力，老服务端会忽略。
const MODELS_SYNC_HEADERS: [(&str, &str); 1] = [("X-Cindy-Model-Capabilities", "audio_speech,display_pricing,evaluation,rerank")];
const API_FETCH_TIMEOUT_MS: u64 = 15_000;
const BILLING_REQUEST_TIMEOUT_MS: u64 = 20_000;
/// 上游 XD_MODELS_SYNC_TIMEOUT_MS：单个在途请求的截止时间，也是整体期限。
const MODELS_SYNC_TIMEOUT_MS: u64 = 20_000;
const FOREGROUND_REFRESH_INTERVAL_MS: f64 = 5.0 * 60_000.0;
const KEY_REJECTION_RESYNC_INTERVAL_MS: f64 = 5.0 * 60_000.0;

fn log(level: &str, msg: &str) {
    eprintln!("[model-access] {level}: {msg}");
}

fn ipc_error(code: &str, message: &str) -> String {
    format!("[{code}] {message}")
}

fn now_ms() -> f64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as f64).unwrap_or(0.0)
}

/// JS 数组的可选首参判定：`[undefined]` / `[]` 都算「没传 payload」，`{}` 算传了。
fn first_arg_is_absent(args: &[Value]) -> bool {
    args.first().map_or(true, Value::is_null)
}

// ── 服务端请求（desktop serverApiClient.ts 的精简版）─────────────────────────────

#[derive(Debug, Clone)]
pub struct ServerApiError {
    pub code: String,
    pub status_code: u16,
    pub message: String,
}

impl ServerApiError {
    fn new(code: &str, status_code: u16, message: &str) -> Self {
        Self { code: code.into(), status_code, message: message.into() }
    }
}

fn status_to_code(status: u16) -> String {
    match status {
        0 => "NETWORK_ERROR".into(),
        401 => "UNAUTHORIZED".into(),
        429 => "RATE_LIMITED".into(),
        s if s >= 500 => "INTERNAL_ERROR".into(),
        s => format!("HTTP_{s}"),
    }
}

fn read_error(data: &Value) -> (Option<String>, Option<String>) {
    let err = data.get("error").filter(|e| e.is_object());
    (
        err.and_then(|e| e["code"].as_str()).map(String::from),
        err.and_then(|e| e["message"].as_str()).map(String::from),
    )
}

fn is_refreshable_unauthorized(code: &str) -> bool {
    matches!(code, "TOKEN_EXPIRED" | "INVALID_TOKEN" | "UNAUTHORIZED")
}

fn redacted_path(api_path: &str) -> String {
    let head = api_path.split(['?', '#']).next().unwrap_or("");
    let segs: Vec<&str> = head.split('/').filter(|s| !s.is_empty()).take(3).collect();
    if segs.is_empty() {
        "/".into()
    } else {
        format!("/{}", segs.join("/"))
    }
}

struct RawResult {
    ok: bool,
    status: u16,
    data: Value,
}

struct ApiFetch {
    method: &'static str,
    body: Option<Value>,
    timeout_ms: u64,
    redact_error_details: bool,
    headers: &'static [(&'static str, &'static str)],
}

impl ApiFetch {
    fn get() -> Self {
        Self { method: "GET", body: None, timeout_ms: API_FETCH_TIMEOUT_MS, redact_error_details: false, headers: &[] }
    }
    fn redacted_get() -> Self {
        Self { redact_error_details: true, ..Self::get() }
    }
}

fn http() -> &'static reqwest::Client {
    static C: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
    C.get_or_init(|| {
        // reqwest 用 rustls-no-provider：先装 ring（已装则跳过）。
        if rustls::crypto::CryptoProvider::get_default().is_none() {
            let _ = rustls::crypto::ring::default_provider().install_default();
        }
        reqwest::Client::builder().build().expect("http client")
    })
}

pub type Emit = Arc<dyn Fn(&str, Value) + Send + Sync>;

/// 一次 models 同步的等待句柄：值变为 true 即结束。
type Flight = watch::Receiver<bool>;

async fn wait_flight(mut rx: Flight) {
    while !*rx.borrow() {
        if rx.changed().await.is_err() {
            return;
        }
    }
}

struct Creds {
    /// "server" | "manual"
    source: String,
    endpoint: Option<String>,
}

#[derive(Default)]
struct Inner {
    gateway_models: Vec<Value>,
    projected: Vec<Value>,
    account_tier: Option<String>,
    auth_generation: u64,
    last_auth_user_id: Option<String>,
    writing_xd_key: bool,
    /// 在途 flight：(所属世代, attempt 序号, 句柄)。
    flight: Option<(u64, u64, Flight)>,
    attempt: u64,
    last_succeeded_attempt: u64,
    last_started_at: f64,
    last_rejection: (Option<String>, f64),
}

pub struct Manager {
    me: std::sync::Weak<Manager>,
    sync: std::sync::OnceLock<Arc<CredSync>>,
    auth: Arc<AuthManager>,
    secrets: Arc<SecretStore>,
    owner: Arc<Owner>,
    emit: Emit,
    locale: Arc<dyn Fn() -> String + Send + Sync>,
    base: String,
    creds_path: PathBuf,
    creds: Mutex<Option<Creds>>,
    st: Mutex<Inner>,
    listeners: Mutex<Vec<Box<dyn Fn() + Send + Sync>>>,
    handle: Handle,
}

struct ModelDeps(std::sync::Weak<Manager>);

impl ModelDeps {
    fn m(&self) -> Option<Arc<Manager>> {
        self.0.upgrade()
    }
}

fn fetch_error(e: ServerApiError) -> Value {
    json!({ "code": e.code, "statusCode": e.status_code })
}

impl Deps for ModelDeps {
    fn fetch(&self) -> BoxFut<Result<Value, Value>> {
        let m = self.m();
        Box::pin(async move {
            let m = m.ok_or_else(|| json!({ "code": "UNKNOWN", "statusCode": 0 }))?;
            m.api_fetch(CREDENTIALS_PATH, ApiFetch::get()).await.map_err(fetch_error)
        })
    }
    fn rotate(&self) -> BoxFut<Result<Value, Value>> {
        let m = self.m();
        Box::pin(async move {
            let m = m.ok_or_else(|| json!({ "code": "UNKNOWN", "statusCode": 0 }))?;
            let opts = ApiFetch { method: "POST", ..ApiFetch::get() };
            m.api_fetch(&format!("{CREDENTIALS_PATH}/rotate"), opts).await.map_err(fetch_error)
        })
    }
    fn read_key(&self) -> Option<String> {
        self.m().and_then(|m| m.secrets.get(XD_KEY_NAME))
    }
    fn write_key(&self, key: String) -> BoxFut<bool> {
        let m = self.m();
        Box::pin(async move {
            let Some(m) = m else { return false };
            if m.owner.mode() != Mode::Cloud {
                return false;
            }
            m.st.lock().unwrap().writing_xd_key = true;
            let m2 = m.clone();
            let r = tauri::async_runtime::spawn_blocking(move || m2.secrets.set(XD_KEY_NAME, &key)).await;
            m.st.lock().unwrap().writing_xd_key = false;
            match r {
                Ok(Ok(())) => true,
                Ok(Err(e)) => {
                    log("warn", &format!("save gateway key failed: {e}"));
                    false
                }
                Err(e) => {
                    log("warn", &format!("save gateway key failed: {e}"));
                    false
                }
            }
        })
    }
    fn source(&self) -> Option<String> {
        self.m().and_then(|m| m.creds.lock().unwrap().as_ref().map(|c| c.source.clone()))
    }
    fn server_endpoint(&self) -> Option<String> {
        self.m().and_then(|m| m.server_endpoint())
    }
    fn set_server_credentials(&self, endpoint: &str) {
        if let Some(m) = self.m() {
            m.persist_creds(Some(Creds { source: "server".into(), endpoint: Some(endpoint.trim_end_matches('/').to_string()) }));
        }
    }
    fn set_manual_source(&self) {
        if let Some(m) = self.m() {
            m.persist_creds(Some(Creds { source: "manual".into(), endpoint: None }));
        }
    }
    fn clear_store(&self) {
        if let Some(m) = self.m() {
            m.persist_creds(None);
        }
    }
    fn on_status(&self, status: &Status) {
        if let Some(m) = self.m() {
            m.on_sync_status(status);
        }
    }
    fn log(&self, level: &str, msg: &str) {
        log(level, msg);
    }
}

impl Manager {
    /// `base`：model-access 服务地址（生产 `auth::env::model_access_base_url()`，测试传 mock 地址）。
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        data_dir: &Path,
        base: String,
        auth: Arc<AuthManager>,
        secrets: Arc<SecretStore>,
        owner: Arc<Owner>,
        emit: Emit,
        locale: Arc<dyn Fn() -> String + Send + Sync>,
        handle: Handle,
    ) -> Arc<Self> {
        let creds_path = data_dir.join("model-access-credentials.json");
        let me = Arc::new_cyclic(|w| Manager {
            me: w.clone(),
            sync: std::sync::OnceLock::new(),
            auth,
            secrets,
            owner,
            emit,
            locale,
            base: base.trim_end_matches('/').to_string(),
            creds: Mutex::new(load_creds(&creds_path)),
            creds_path,
            st: Mutex::new(Inner::default()),
            listeners: Mutex::new(vec![]),
            handle: handle.clone(),
        });
        let _ = me.sync.set(CredSync::new(Arc::new(ModelDeps(Arc::downgrade(&me))), handle));
        me
    }

    /// 接上登录态与密钥库变化。构造后调用一次。
    pub fn start(self: &Arc<Self>) {
        let w = Arc::downgrade(self);
        self.auth.on_change(move |info| {
            if let Some(m) = w.upgrade() {
                m.note_auth_state(info.authenticated, info.user_id.clone());
            }
        });
        // renderer 经通用 safe-storage 通道手填 / 删除网关 key 时，按 desktop 语义改来源标记。
        let w = Arc::downgrade(self);
        self.secrets.on_change(move |name| {
            let Some(m) = w.upgrade() else { return };
            match name {
                None => m.notify_change(),
                Some(n) => {
                    let writing = m.st.lock().unwrap().writing_xd_key;
                    if n != XD_KEY_NAME || writing {
                        if n == XD_KEY_NAME {
                            m.notify_change();
                        }
                        return;
                    }
                    if m.secrets.has(XD_KEY_NAME) {
                        m.sync().note_manual_key_saved();
                    } else {
                        m.sync().note_manual_key_removed();
                    }
                    m.notify_change();
                }
            }
        });
        let initial = self.auth.info();
        if initial.authenticated {
            self.note_auth_state(true, initial.user_id);
        } else if self.owner.mode() != Mode::Cloud && (self.secrets.has(XD_KEY_NAME) || self.creds.lock().unwrap().is_some()) {
            // 上次退出云端模式时没来得及删（或钥匙串当时锁着），启动时补删。
            self.spawn_clear_key();
        }
    }

    fn sync(&self) -> &Arc<CredSync> {
        self.sync.get().expect("credsync")
    }

    fn arc(&self) -> Arc<Manager> {
        self.me.upgrade().expect("manager alive")
    }

    fn server_endpoint(&self) -> Option<String> {
        self.creds.lock().unwrap().as_ref().filter(|c| c.source == "server").and_then(|c| c.endpoint.clone())
    }

    fn persist_creds(&self, next: Option<Creds>) {
        let doc = next.as_ref().map(|c| match &c.endpoint {
            Some(e) if c.source == "server" => json!({ "source": "server", "endpoint": e }),
            _ => json!({ "source": "manual" }),
        });
        *self.creds.lock().unwrap() = next;
        let r = match doc {
            None => match std::fs::remove_file(&self.creds_path) {
                Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e.to_string()),
                _ => Ok(()),
            },
            Some(d) => write_private(&self.creds_path, &d.to_string()),
        };
        // 只影响下次冷启动首轮同步前的 endpoint 记忆，不阻断主流程。
        if let Err(e) = r {
            log("warn", &format!("persist model-access credentials meta failed: {e}"));
        }
    }

    // ── 变更监听 ─────────────────────────────────────────────────────────

    /// xd provider 或其模型清单变化（登录 / 登出 / 目录刷新 / key 变化）。
    pub fn on_xd_change(&self, f: impl Fn() + Send + Sync + 'static) {
        self.listeners.lock().unwrap().push(Box::new(f));
    }

    fn notify_change(&self) {
        for f in self.listeners.lock().unwrap().iter() {
            f();
        }
    }
}

fn load_creds(path: &Path) -> Option<Creds> {
    let text = std::fs::read_to_string(path).ok()?;
    let raw: Value = match serde_json::from_str(&text) {
        Ok(v) => v,
        Err(e) => {
            log("warn", &format!("model-access-credentials.json unreadable ({e}); ignoring"));
            return None;
        }
    };
    match (raw["source"].as_str(), raw["endpoint"].as_str()) {
        (Some("server"), Some(e)) => Some(Creds { source: "server".into(), endpoint: Some(e.to_string()) }),
        (Some("manual"), _) => Some(Creds { source: "manual".into(), endpoint: None }),
        _ => None,
    }
}

fn write_private(file: &Path, text: &str) -> Result<(), String> {
    use std::io::Write;
    let tmp = file.with_extension(format!("json.tmp-{}", std::process::id()));
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let r = opts.open(&tmp).and_then(|mut f| f.write_all(text.as_bytes())).and_then(|_| std::fs::rename(&tmp, file));
    if let Err(e) = r {
        let _ = std::fs::remove_file(&tmp);
        return Err(e.to_string());
    }
    Ok(())
}

// ── 服务端请求 ───────────────────────────────────────────────────────────────

impl Manager {
    async fn raw_fetch(&self, api_path: &str, o: &ApiFetch) -> RawResult {
        let url = format!("{}{}", self.base, api_path);
        let mut rb = match o.method {
            "POST" => http().post(&url),
            _ => http().get(&url),
        };
        rb = rb.header("Content-Type", "application/json").header("Accept-Language", (self.locale)());
        for (k, v) in o.headers {
            rb = rb.header(*k, *v);
        }
        if let Some(t) = self.auth.access_token() {
            rb = rb.header("Authorization", format!("Bearer {t}"));
        }
        if let Some(b) = &o.body {
            rb = rb.body(b.to_string());
        }
        let work = async {
            let resp = rb.send().await?;
            let status = resp.status().as_u16();
            let ok = resp.status().is_success();
            let bytes = resp.bytes().await?;
            Ok::<_, reqwest::Error>((ok, status, serde_json::from_slice::<Value>(&bytes).unwrap_or(Value::Null)))
        };
        let res = if o.timeout_ms > 0 {
            match tokio::time::timeout(Duration::from_millis(o.timeout_ms), work).await {
                Ok(r) => r.map_err(|_| "TypeError"),
                Err(_) => Err("AbortError"),
            }
        } else {
            work.await.map_err(|_| "TypeError")
        };
        match res {
            Ok((ok, status, data)) => RawResult { ok, status, data },
            Err(name) => {
                log("warn", &format!("request failed path={} error={name}", redacted_path(api_path)));
                RawResult { ok: false, status: 0, data: Value::Null }
            }
        }
    }

    /// 成功返回 JSON；失败给 ServerApiError。可恢复的 401 自动续期一次再重试。
    async fn api_fetch(&self, api_path: &str, o: ApiFetch) -> Result<Value, ServerApiError> {
        let mut result = self.raw_fetch(api_path, &o).await;
        let mut retried = false;
        let first = read_error(&result.data).0.unwrap_or_else(|| status_to_code(result.status));
        if result.status == 401 && first != "ACCOUNT_UNAVAILABLE" && is_refreshable_unauthorized(&first) && self.auth.refresh().await {
            retried = true;
            result = self.raw_fetch(api_path, &o).await;
        }
        if !result.ok {
            let (code, message) = read_error(&result.data);
            let err_code = code.unwrap_or_else(|| status_to_code(result.status));
            let public_code = if o.redact_error_details { status_to_code(result.status) } else { err_code.clone() };
            if result.status == 401 && (err_code == "ACCOUNT_UNAVAILABLE" || (retried && is_refreshable_unauthorized(&err_code))) {
                let reason = if err_code == "ACCOUNT_UNAVAILABLE" { "account-unavailable" } else { "unknown" };
                let auth = self.auth.clone();
                self.handle.spawn(async move { auth.expire_session(reason, true, true).await });
            }
            log("warn", &format!("not ok path={} status={} code={public_code}", redacted_path(api_path), result.status));
            let fallback = format!("请求失败 ({})", result.status);
            let msg = if o.redact_error_details { fallback } else { message.unwrap_or(fallback) };
            return Err(ServerApiError::new(&public_code, result.status, &msg));
        }
        if result.data.is_null() {
            return Err(ServerApiError::new("EMPTY_RESPONSE", result.status, "响应体为空"));
        }
        Ok(result.data)
    }
}

// ── 模型目录 / 同步 ──────────────────────────────────────────────────────────

impl Manager {
    fn status_payload(&self) -> Value {
        let mut v = self.sync().status().to_json();
        v["accountTier"] = self.st.lock().unwrap().account_tier.clone().map_or(Value::Null, Value::from);
        v
    }

    fn broadcast_status(&self) {
        (self.emit)(STATUS_CHANNEL, self.status_payload());
    }

    fn set_account_tier(&self, next: Option<String>) {
        {
            let mut g = self.st.lock().unwrap();
            if g.account_tier == next {
                return;
            }
            g.account_tier = next;
        }
        self.broadcast_status();
    }

    fn apply_gateway_models(&self, models: Vec<Value>) {
        {
            let mut g = self.st.lock().unwrap();
            g.projected = xd_catalog::project_pi_models(&models);
            g.gateway_models = models;
        }
        self.notify_change();
    }

    fn on_sync_status(&self, status: &Status) {
        let down = matches!(status.state, "failed" | "disabled" | "unsupported" | "idle");
        if down {
            self.st.lock().unwrap().account_tier = None;
        }
        self.broadcast_status();
        if status.state == "ok" {
            let _ = self.schedule_models_sync();
            self.notify_change();
        } else if down {
            self.apply_gateway_models(vec![]);
        }
    }

    async fn run_models_sync(&self, gen: u64, attempt: u64) {
        let previous = self.st.lock().unwrap().gateway_models.clone();
        let failed = |why: String| {
            log("warn", &format!("xd gateway models {why} (keeping executable last valid list)"));
            if gen != self.st.lock().unwrap().auth_generation {
                return;
            }
            self.set_account_tier(None);
            self.apply_gateway_models(without_stale_payment_upsell(&previous));
        };
        let opts = ApiFetch { timeout_ms: MODELS_SYNC_TIMEOUT_MS, headers: &MODELS_SYNC_HEADERS, ..ApiFetch::get() };
        let data = match tokio::time::timeout(Duration::from_millis(MODELS_SYNC_TIMEOUT_MS), self.api_fetch(MODELS_SYNC_PATH, opts)).await {
            Ok(Ok(d)) => d,
            Ok(Err(e)) => return failed(format!("fetch failed: {}", e.code)),
            Err(_) => return failed(format!("fetch failed: Cindy AI model list refresh timed out after {MODELS_SYNC_TIMEOUT_MS}ms")),
        };
        if gen != self.st.lock().unwrap().auth_generation {
            return;
        }
        let parsed = match parse_models_sync_payload(&data) {
            Ok(p) => p,
            Err(e) => return failed(format!("response rejected: {e}")),
        };
        let (total, for_pi) = (parsed.models.len(), xd_catalog::project_pi_models(&parsed.models).len());
        self.apply_gateway_models(parsed.models);
        self.set_account_tier(Some(parsed.account_tier));
        self.st.lock().unwrap().last_succeeded_attempt = attempt;
        log("info", &format!("xd gateway models synced: {total} total, {for_pi} for pi"));
    }

    /// 同世代 single-flight；旧世代在途时等它结束再为当前账号补发。
    fn schedule_models_sync(&self) -> Option<Flight> {
        let me = self.arc();
        let mut g = self.st.lock().unwrap();
        g.last_auth_user_id.as_ref()?;
        let gen = g.auth_generation;
        if let Some((fgen, _, rx)) = &g.flight {
            if *fgen == gen {
                return Some(rx.clone());
            }
            let old = rx.clone();
            let (tx, rx) = watch::channel(false);
            drop(g);
            self.handle.spawn(async move {
                wait_flight(old).await;
                if let Some(next) = me.schedule_models_sync() {
                    wait_flight(next).await;
                }
                let _ = tx.send(true);
            });
            return Some(rx);
        }
        g.attempt += 1;
        let attempt = g.attempt;
        g.last_started_at = now_ms();
        let (tx, rx) = watch::channel(false);
        g.flight = Some((gen, attempt, rx.clone()));
        drop(g);
        self.handle.spawn(async move {
            me.run_models_sync(gen, attempt).await;
            {
                let mut g = me.st.lock().unwrap();
                if g.flight.as_ref().is_some_and(|f| f.1 == attempt) {
                    g.flight = None;
                }
            }
            let _ = tx.send(true);
        });
        Some(rx)
    }

    /// 设置页 / 模型选择器的显式刷新（maker:provider:models-refresh xd）：凭据没就绪先重试，再等一次新的 /models。
    pub async fn refresh_xd_gateway_models(&self) -> Result<(), String> {
        self.require_cloud()?;
        let status = if self.sync().status().state == "ok" { self.sync().status() } else { self.sync().retry().await };
        if status.state != "ok" {
            return Err(ipc_error("MODEL_ACCESS_FAILED", "Cindy AI credentials are not ready."));
        }
        let (gen, minimum) = {
            let g = self.st.lock().unwrap();
            (g.auth_generation, g.attempt + 1)
        };
        loop {
            let Some(flight) = self.schedule_models_sync() else {
                return Err(ipc_error("MODEL_ACCESS_FAILED", "Cindy AI model list refresh did not start."));
            };
            let attempt = self.st.lock().unwrap().attempt;
            wait_flight(flight).await;
            let g = self.st.lock().unwrap();
            if gen != g.auth_generation {
                return Err(ipc_error("MODEL_ACCESS_FAILED", "Cindy AI account changed during model list refresh."));
            }
            if attempt < minimum {
                continue;
            }
            if g.last_succeeded_attempt != attempt {
                return Err(ipc_error("MODEL_ACCESS_FAILED", "Cindy AI model list refresh failed."));
            }
            return Ok(());
        }
    }

    /// 代替 desktop 的窗口聚焦刷新：打开模型选择器 / 设置页时最多每 5 分钟拉一次。
    pub fn foreground_refresh_xd_models(&self) {
        {
            let g = self.st.lock().unwrap();
            if g.last_auth_user_id.is_none() || now_ms() - g.last_started_at < FOREGROUND_REFRESH_INTERVAL_MS {
                return;
            }
        }
        if self.sync().status().state != "ok" {
            return;
        }
        let _ = self.schedule_models_sync();
    }

    fn note_auth_state(&self, authenticated: bool, user_id: Option<String>) {
        let user = if authenticated { user_id } else { None };
        {
            let mut g = self.st.lock().unwrap();
            if user != g.last_auth_user_id {
                g.auth_generation += 1;
                g.last_auth_user_id = user.clone();
            }
        }
        self.sync().handle_auth_change(authenticated, user.as_deref(), authenticated.then_some(AUTH_REGION));
        if self.owner.mode() != Mode::Cloud && (self.secrets.has(XD_KEY_NAME) || self.creds.lock().unwrap().is_some()) {
            self.spawn_clear_key();
        }
        self.notify_change();
    }

    fn spawn_clear_key(&self) {
        let me = self.arc();
        self.handle.spawn(async move {
            let m = me.clone();
            let _ = tauri::async_runtime::spawn_blocking(move || {
                m.st.lock().unwrap().writing_xd_key = true;
                if let Err(e) = m.secrets.remove(XD_KEY_NAME) {
                    log("warn", &format!("remove gateway key failed: {e}"));
                }
                m.st.lock().unwrap().writing_xd_key = false;
                m.persist_creds(None);
            })
            .await;
            me.notify_change();
        });
    }
}

// ── xd provider 投影 / 会话运行配置 ─────────────────────────────────────────

fn is_chat_mode(mode: &Value) -> bool {
    match mode.as_str() {
        None => true,
        Some(m) => m.is_empty() || m == "chat" || m == "responses",
    }
}

fn normalize_model_id(id: &str) -> &str {
    id.strip_suffix("[1m]").unwrap_or(id)
}

/// CatalogModel → maker-core ModelDescriptor（desktop catalog-to-descriptors.ts toDescriptor）。
pub fn catalog_model_to_descriptor(m: &Value) -> Value {
    let mut d = Map::new();
    d.insert("id".into(), m["id"].clone());
    d.insert("displayName".into(), m["name"].clone());
    d.insert("contextWindow".into(), m["contextWindow"].clone());
    d.insert("efforts".into(), m["efforts"].clone());
    d.insert("defaultEffort".into(), m["defaultEffort"].clone());
    d.insert("effortsUnknown".into(), json!(m["effortsUnknown"] == true));
    for (from, to) in [
        ("description", "description"),
        ("effortDisplayNames", "effortDisplayNames"),
        ("supportsFastMode", "supportsFastMode"),
        ("supportsUltrafastMode", "supportsUltrafastMode"),
        ("group", "group"),
        ("sortOrder", "sortOrder"),
        ("mode", "mode"),
        ("defaultEnabled", "defaultEnabled"),
        ("newSessionDefault", "newSessionDefault"),
        ("cost", "cost"),
        ("maxOutput", "maxOutputTokens"),
    ] {
        if let Some(v) = m.get(from).filter(|v| !v.is_null()) {
            d.insert(to.into(), v.clone());
        }
    }
    let image = match m.get("supportsImageInput").filter(|v| !v.is_null()) {
        Some(v) => Some(v.clone()),
        None => m["modalities"]["input"].as_array().map(|a| json!(a.iter().any(|x| x == "image"))),
    };
    if let Some(v) = image {
        d.insert("supportsImageInput".into(), v);
    }
    Value::Object(d)
}

impl Manager {
    fn require_cloud(&self) -> Result<(), String> {
        if self.owner.mode() != Mode::Cloud {
            return Err(ipc_error("PERMISSION_DENIED", "Cindy AI requires a Cindy account."));
        }
        Ok(())
    }

    /// 网关 key 与配套 endpoint 都在、且处于云端模式。
    pub fn xd_connected(&self) -> bool {
        self.owner.mode() == Mode::Cloud && self.secrets.has(XD_KEY_NAME) && self.server_endpoint().is_some()
    }

    /// 目录里的 xd provider；不在云端模式返回 None（本机模式同样不列 Cindy AI）。
    pub fn xd_provider(&self) -> Option<Value> {
        if self.owner.mode() != Mode::Cloud {
            return None;
        }
        Some(xd_catalog::xd_provider(&self.st.lock().unwrap().projected))
    }

    /// 当前 xd 的 Pi 模型清单（已投影）。
    pub fn projected_models(&self) -> Vec<Value> {
        self.st.lock().unwrap().projected.clone()
    }

    fn find_pi_model(&self, model_id: &str) -> Option<Value> {
        let id = normalize_model_id(model_id);
        self.st.lock().unwrap().projected.iter().find(|m| m["id"] == id).cloned()
    }

    /// Pi 网关块用：xd 模型的描述符；不是 xd 成员返回 None。
    pub fn resolve_xd_descriptor(&self, model_id: &str) -> Option<Value> {
        self.find_pi_model(model_id).map(|m| catalog_model_to_descriptor(&m))
    }

    /// (xd, model) → 会话运行配置；未连接、不在清单或需付费返回 None。
    pub fn resolve_xd_run_config(&self, model_id: &str) -> Option<Value> {
        if !self.xd_connected() {
            return None;
        }
        let m = self.find_pi_model(model_id)?;
        if m["availability"] == "requires_payment" {
            return None;
        }
        let reasoning = m["efforts"].as_array().is_some_and(|a| !a.is_empty());
        Some(json!({ "providerId": XD_PROVIDER_ID, "providerName": "Cindy AI", "modelId": m["id"], "gateway": true, "reasoning": reasoning }))
    }

    /// 第一个可选的 xd 模型（没有自定义 provider 时的默认）。
    pub fn default_xd_run_config(&self) -> Option<Value> {
        if !self.xd_connected() {
            return None;
        }
        let list = self.projected_models();
        let ok = |x: &&Value| x["availability"] != "requires_payment" && is_chat_mode(&x["mode"]);
        let pick = list.iter().filter(ok).find(|x| x["defaultEnabled"] != false).or_else(|| list.iter().find(ok))?;
        self.resolve_xd_run_config(pick["id"].as_str()?)
    }

    /// Pi 网关块的协议与兼容性配置。`Err(())` = 不是 xd 成员；`Ok(None)` = 失败关闭。
    pub fn resolve_pi_gateway_model_spec(&self, model_id: &str) -> Result<Option<Value>, ()> {
        xd_catalog::resolve_pi_gateway_model_spec(&self.st.lock().unwrap().gateway_models, model_id)
    }

    /// 当前网关 {key, endpoint}；未登录返回 None。只给会话宿主写 key 文件用，不进日志、不回 renderer。
    pub fn gateway_upstream(&self) -> Option<(String, String)> {
        if !self.xd_connected() {
            return None;
        }
        let key = self.secrets.get(XD_KEY_NAME)?;
        let key = crate::auth::flow::js_trim(&key);
        let endpoint = self.server_endpoint()?;
        (!key.is_empty() && !endpoint.is_empty()).then(|| (key.to_string(), endpoint))
    }

    /// 网关拒绝了当前 key（上游 401，会话宿主上报）：同一个 key 5 分钟内最多静默重拉一次凭据。
    pub fn gateway_key_rejected(&self) -> bool {
        let Some((key, _)) = self.gateway_upstream() else { return false };
        {
            let mut g = self.st.lock().unwrap();
            let now = now_ms();
            if g.last_rejection.0.as_deref() == Some(key.as_str()) && now - g.last_rejection.1 < KEY_REJECTION_RESYNC_INTERVAL_MS {
                return false;
            }
            g.last_rejection = (Some(key), now);
        }
        log("warn", "gateway rejected the current key; re-syncing credentials");
        let sync = self.sync().clone();
        self.handle.spawn(async move {
            sync.resync_after_key_rejection().await;
        });
        true
    }
}

// ── IPC 通道 ─────────────────────────────────────────────────────────────────

const CHANNELS: [&str; 7] = [
    "model-access:get-status",
    "model-access:retry",
    "model-access:rotate",
    "model-access:byok-status",
    "model-access:byok-retry",
    "billing:get-balance",
    "billing:get-credit-usage",
];

fn map_server_error(e: &Value) -> String {
    let code = e["code"].as_str().unwrap_or("");
    let status = e["statusCode"].as_f64().unwrap_or(0.0);
    if status == 503.0 || code == "MODEL_ACCESS_DISABLED" {
        return ipc_error("MODEL_ACCESS_DISABLED", "模型访问服务未启用");
    }
    if status == 403.0 || code == "ORG_NOT_SUPPORTED" {
        return ipc_error("MODEL_ACCESS_UNSUPPORTED", "当前企业未开通模型访问");
    }
    ipc_error("MODEL_ACCESS_FAILED", &format!("[{code}]"))
}

impl Manager {
    pub fn owns(channel: &str) -> bool {
        CHANNELS.contains(&channel)
    }

    fn require_personal_account(&self) -> Result<(), String> {
        if self.owner.mode() != Mode::Cloud {
            return Err(ipc_error("PERMISSION_DENIED", "Billing requires a personal Cindy account."));
        }
        if self.auth.info().membership_kind.as_deref() != Some("personal") {
            return Err(ipc_error("PERMISSION_DENIED", "Billing is only available to personal accounts."));
        }
        Ok(())
    }

    async fn billing(&self, path: &str, args: &[Value], payload_message: &str, project: fn(&Value) -> Result<Value, ()>) -> Result<Value, String> {
        self.require_personal_account()?;
        if !first_arg_is_absent(args) {
            return Err(ipc_error("INVALID_PARAMS", payload_message));
        }
        let opts = ApiFetch { timeout_ms: BILLING_REQUEST_TIMEOUT_MS, ..ApiFetch::redacted_get() };
        let response = self.api_fetch(path, opts).await.map_err(|e| match e.status_code {
            401 | 403 => ipc_error("PERMISSION_DENIED", "balance request is not authorized"),
            404 => ipc_error("NOT_FOUND", "balance account is not provisioned"),
            501 => ipc_error("UNSUPPORTED_CAPABILITY", "balance is not supported for this account"),
            _ => ipc_error("MODEL_ACCESS_FAILED", "balance service request failed"),
        })?;
        project(&response).map_err(|_| ipc_error("INTERNAL", "billing service response was invalid"))
    }

    pub async fn invoke(&self, channel: &str, args: &[Value]) -> Option<Result<Value, String>> {
        Some(match channel {
            "model-access:get-status" => Ok(self.status_payload()),
            "model-access:retry" => async {
                self.require_cloud()?;
                self.sync().retry().await;
                Ok(self.status_payload())
            }
            .await,
            "model-access:rotate" => async {
                self.require_cloud()?;
                self.sync().rotate().await.map_err(|e| map_server_error(&e))?;
                Ok(self.status_payload())
            }
            .await,
            // 企业 BYOK 号池不在范围内：返回空闲、无 provider。
            "model-access:byok-status" | "model-access:byok-retry" => Ok(json!({ "state": "idle", "providers": [] })),
            "billing:get-balance" => {
                self.billing(BALANCE_PATH, args, "balance request does not accept a payload", crate::billing::project_balance).await
            }
            "billing:get-credit-usage" => {
                self.billing(CREDIT_USAGE_PATH, args, "credit usage request does not accept a payload", crate::billing::project_credit_usage).await
            }
            _ => return None,
        })
    }
}
