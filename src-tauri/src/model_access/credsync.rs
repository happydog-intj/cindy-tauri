//! 网关凭据同步状态机：逐行移植 desktop main/model-access/credentialsSync.ts。
//! 所有 IO（HTTP / 密钥库 / 元数据存储 / 推送 / 睡眠）经 `Deps` 注入；世代号（epoch）和 single-flight 语义与上游一致。
//! 黄金样例：scripts/app-settings/credsync-entry.ts → credsync_golden.json（45 个场景）。

use serde_json::{json, Value};
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use tokio::runtime::Handle;
use tokio::sync::watch;

pub type BoxFut<T> = Pin<Box<dyn Future<Output = T> + Send>>;

const RETRY_DELAYS_MS: [u64; 2] = [2_000, 8_000];

#[derive(Clone, Debug, PartialEq)]
pub struct Status {
    /// idle | syncing | ok | failed | disabled | unsupported
    pub state: &'static str,
    pub error_code: Option<String>,
    pub source: Option<String>,
    pub endpoint: Option<String>,
}

impl Status {
    pub fn to_json(&self) -> Value {
        let mut v = json!({ "state": self.state, "source": self.source, "endpoint": self.endpoint, "accountTier": null });
        if let Some(c) = &self.error_code {
            v["errorCode"] = json!(c);
        }
        v
    }
}

pub trait Deps: Send + Sync + 'static {
    /// GET /api/model-access/credentials。失败给 `{code, statusCode}` 形状（其余形状按 UNKNOWN 处理）。
    fn fetch(&self) -> BoxFut<Result<Value, Value>>;
    fn rotate(&self) -> BoxFut<Result<Value, Value>>;
    fn read_key(&self) -> Option<String>;
    /// false = 落盘失败。
    fn write_key(&self, key: String) -> BoxFut<bool>;
    fn source(&self) -> Option<String>;
    fn server_endpoint(&self) -> Option<String>;
    fn set_server_credentials(&self, endpoint: &str);
    fn set_manual_source(&self);
    fn clear_store(&self);
    fn on_status(&self, status: &Status);
    fn sleep(&self, ms: u64) -> BoxFut<()> {
        Box::pin(tokio::time::sleep(std::time::Duration::from_millis(ms)))
    }
    fn log(&self, _level: &str, _msg: &str) {}
}

struct Payload {
    endpoint: String,
    api_key: String,
}

/// 下发凭据落盘前的校验：key 非空白、endpoint 是 http(s) URL。
fn parse_payload(v: &Value) -> Option<Payload> {
    let o = v.as_object()?;
    let key = o.get("apiKey")?.as_str()?;
    let endpoint = o.get("endpoint")?.as_str()?;
    if crate::auth::flow::js_trim(key).is_empty() || crate::auth::flow::js_trim(endpoint).is_empty() {
        return None;
    }
    let url = url::Url::parse(endpoint).ok()?;
    matches!(url.scheme(), "http" | "https").then(|| Payload { endpoint: endpoint.to_string(), api_key: key.to_string() })
}

fn as_fetch_error(v: &Value) -> (String, i64) {
    match (v.get("code").and_then(Value::as_str), v.get("statusCode").and_then(Value::as_f64)) {
        (Some(c), Some(s)) => (c.to_string(), s as i64),
        _ => ("UNKNOWN".to_string(), 0),
    }
}

enum Attempt {
    Done(Status),
    Retryable,
    Stale,
}

struct Inner {
    status: Status,
    /// (运行编号, 结果接收端)
    inflight: Option<(u64, watch::Receiver<Option<Status>>)>,
    inflight_mode: Arc<AtomicBool>,
    inflight_epoch: i64,
    epoch: i64,
    run_seq: u64,
    current_user: Option<String>,
    current_realm: Option<String>,
}

pub struct CredSync {
    deps: Arc<dyn Deps>,
    inner: Mutex<Inner>,
    handle: Handle,
}

impl CredSync {
    pub fn new(deps: Arc<dyn Deps>, handle: Handle) -> Arc<Self> {
        let status = Status { state: "idle", error_code: None, source: deps.source(), endpoint: deps.server_endpoint() };
        Arc::new(Self {
            deps,
            handle,
            inner: Mutex::new(Inner {
                status,
                inflight: None,
                inflight_mode: Arc::new(AtomicBool::new(false)),
                inflight_epoch: -1,
                epoch: 0,
                run_seq: 0,
                current_user: None,
                current_realm: None,
            }),
        })
    }

    pub fn status(&self) -> Status {
        self.inner.lock().unwrap().status.clone()
    }

    fn epoch(&self) -> i64 {
        self.inner.lock().unwrap().epoch
    }

    fn snapshot(&self, state: &'static str, error_code: Option<&str>) -> Status {
        Status { state, error_code: error_code.map(String::from), source: self.deps.source(), endpoint: self.deps.server_endpoint() }
    }

    fn set_status(&self, state: &'static str, error_code: Option<&str>) -> Status {
        let s = self.snapshot(state, error_code);
        self.inner.lock().unwrap().status = s.clone();
        self.deps.on_status(&s);
        s
    }

    /// 世代闸下的状态写入：发起世代已过期则不动状态。
    fn set_status_if_fresh(&self, my: i64, state: &'static str, error_code: Option<&str>) -> Status {
        if my != self.epoch() {
            return self.status();
        }
        self.set_status(state, error_code)
    }

    async fn attempt_once(&self, my: i64, silent: &AtomicBool) -> Attempt {
        let payload = match self.deps.fetch().await {
            Ok(p) => p,
            Err(err) => {
                if my != self.epoch() {
                    return Attempt::Stale;
                }
                let (code, status_code) = as_fetch_error(&err);
                if status_code == 503 || code == "MODEL_ACCESS_DISABLED" {
                    self.deps.log("info", "model-access disabled on server, falling back to manual key flow");
                    return Attempt::Done(self.set_status_if_fresh(my, "disabled", None));
                }
                if status_code == 403 || code == "ORG_NOT_SUPPORTED" {
                    self.deps.log("info", "org not supported by model-access, xd provider unavailable");
                    return Attempt::Done(self.set_status_if_fresh(my, "unsupported", None));
                }
                self.deps.log("warn", &format!("model-access credentials fetch failed: {code} ({status_code})"));
                return Attempt::Retryable;
            }
        };
        if my != self.epoch() {
            return Attempt::Stale;
        }
        let Some(p) = parse_payload(&payload) else {
            self.deps.log("warn", "model-access credentials response invalid (empty key / bad endpoint), kept local");
            return Attempt::Retryable;
        };
        Attempt::Done(self.apply_payload(p, my, silent.load(Ordering::SeqCst)).await)
    }

    /// 下发结果落盘（sync 与 rotate 共用，世代闸）。值未变不写 key。
    async fn apply_payload(&self, p: Payload, my: i64, silent: bool) -> Status {
        if my != self.epoch() {
            return self.status();
        }
        if self.deps.read_key().as_deref() != Some(p.api_key.as_str()) {
            let ok = self.deps.write_key(p.api_key.clone()).await;
            if my != self.epoch() {
                return self.status();
            }
            if !ok {
                if silent {
                    self.deps.log("warn", "model-access key write failed after key rejection, keeping current status");
                    return self.status();
                }
                self.deps.log("warn", "model-access key write failed (safeStorage unavailable?)");
                return self.set_status("failed", Some("SAFE_STORAGE_UNAVAILABLE"));
            }
        }
        self.deps.set_server_credentials(&p.endpoint);
        self.set_status("ok", None)
    }

    async fn run_sync(&self, my: i64, mode: Arc<AtomicBool>) -> Status {
        if my != self.epoch() {
            return self.status();
        }
        if !mode.load(Ordering::SeqCst) {
            self.set_status("syncing", None);
        }
        let mut attempt = 0usize;
        loop {
            if my != self.epoch() {
                return self.status();
            }
            match self.attempt_once(my, &mode).await {
                Attempt::Stale => return self.status(),
                _ if my != self.epoch() => return self.status(),
                Attempt::Done(s) => return s,
                Attempt::Retryable => {}
            }
            if attempt >= RETRY_DELAYS_MS.len() {
                if mode.load(Ordering::SeqCst) {
                    self.deps.log("warn", "model-access re-sync after key rejection failed, keeping current status");
                    return self.status();
                }
                return self.set_status_if_fresh(my, "failed", Some("SYNC_FAILED"));
            }
            self.deps.sleep(RETRY_DELAYS_MS[attempt]).await;
            if my != self.epoch() {
                return self.status();
            }
            attempt += 1;
        }
    }

    /// 同世代 single-flight；返回的 future 等该轮结束。
    fn start_sync(self: &Arc<Self>, silent: bool) -> impl Future<Output = Status> + Send + 'static {
        let rx = {
            let mut g = self.inner.lock().unwrap();
            let reuse = match (&g.inflight, g.inflight_epoch == g.epoch) {
                (Some((_, rx)), true) => Some(rx.clone()),
                _ => None,
            };
            if let Some(rx) = reuse {
                if !silent {
                    g.inflight_mode.store(false, Ordering::SeqCst);
                }
                rx
            } else {
                let my = g.epoch;
                g.inflight_epoch = my;
                let mode = Arc::new(AtomicBool::new(silent));
                g.inflight_mode = mode.clone();
                g.run_seq += 1;
                let run_id = g.run_seq;
                let (tx, rx) = watch::channel(None);
                g.inflight = Some((run_id, rx.clone()));
                drop(g);
                let me = self.clone();
                self.handle.spawn(async move {
                    let result = me.run_sync(my, mode).await;
                    {
                        let mut g = me.inner.lock().unwrap();
                        if matches!(&g.inflight, Some((id, _)) if *id == run_id) {
                            g.inflight = None;
                        }
                    }
                    let _ = tx.send(Some(result));
                });
                rx
            }
        };
        let me = self.clone();
        async move { wait(rx, || me.status()).await }
    }

    /// 登录触发的同步。unsupported / disabled 是本轮登录期内的终态，不再打服务端。
    pub fn sync(self: &Arc<Self>) -> impl Future<Output = Status> + Send + 'static {
        let st = self.status();
        let terminal = matches!(st.state, "unsupported" | "disabled");
        let f = (!terminal).then(|| self.start_sync(false));
        async move {
            match f {
                Some(f) => f.await,
                None => st,
            }
        }
    }

    /// UI 手动重试：允许从任何状态重新发起。
    pub fn retry(self: &Arc<Self>) -> impl Future<Output = Status> + Send + 'static {
        self.start_sync(false)
    }

    /// 网关拒绝 key 后的静默重新同步：只在拿到新凭据或确定终态时改状态。
    pub fn resync_after_key_rejection(self: &Arc<Self>) -> impl Future<Output = Status> + Send + 'static {
        let st = self.status();
        let terminal = matches!(st.state, "unsupported" | "disabled");
        let f = (!terminal).then(|| self.start_sync(true));
        async move {
            match f {
                Some(f) => f.await,
                None => st,
            }
        }
    }

    /// 轮换：等在途同步结束后执行。失败给 `{code, statusCode}`。
    pub async fn rotate(self: &Arc<Self>) -> Result<Status, Value> {
        let pending = self.inner.lock().unwrap().inflight.as_ref().map(|(_, rx)| rx.clone());
        if let Some(rx) = pending {
            let me = self.clone();
            wait(rx, move || me.status()).await;
        }
        let my = self.epoch();
        let payload = self.deps.rotate().await?;
        if my != self.epoch() {
            return Err(json!({ "code": "STALE_ACCOUNT", "statusCode": 0 }));
        }
        let Some(p) = parse_payload(&payload) else {
            return Err(json!({ "code": "INVALID_RESPONSE", "statusCode": 0 }));
        };
        Ok(self.apply_payload(p, my, false).await)
    }

    /// 登录 / 登出 / 切账号入口。userId + realm 构成身份边界。
    pub fn handle_auth_change(self: &Arc<Self>, is_authenticated: bool, user_id: Option<&str>, realm: Option<&str>) {
        if is_authenticated {
            {
                let mut g = self.inner.lock().unwrap();
                // 首次获知身份不算切换；已建立身份的 userId 或 realm 任一变化都作废旧请求。
                let changed = matches!((&g.current_user, user_id), (Some(a), Some(b)) if a != b)
                    || matches!((&g.current_realm, realm), (Some(a), Some(b)) if a != b);
                if changed {
                    g.epoch += 1;
                }
                if let Some(u) = user_id {
                    g.current_user = Some(u.to_string());
                }
                if let Some(r) = realm {
                    g.current_realm = Some(r.to_string());
                }
            }
            drop(self.start_sync(false));
        } else {
            {
                let mut g = self.inner.lock().unwrap();
                g.epoch += 1;
                g.current_user = None;
                g.current_realm = None;
            }
            self.set_status("idle", None);
        }
    }

    pub fn note_manual_key_saved(&self) {
        self.deps.set_manual_source();
        let s = self.status();
        self.set_status(s.state, s.error_code.as_deref());
    }

    pub fn note_manual_key_removed(&self) {
        self.deps.clear_store();
        let s = self.status();
        self.set_status(s.state, s.error_code.as_deref());
    }
}

async fn wait(mut rx: watch::Receiver<Option<Status>>, fallback: impl Fn() -> Status) -> Status {
    loop {
        if let Some(v) = rx.borrow().clone() {
            return v;
        }
        if rx.changed().await.is_err() {
            return fallback();
        }
    }
}
