// auth::Env 的生产实现与云端地址（原 sidecar cloud-endpoints.mjs）。
use super::manager::{BoxFut, Env};
use crate::owner::{Mode, Owner};
use crate::secret_store::SecretStore;
use serde_json::Value;
use std::sync::Arc;
use std::time::Duration;

const AUTH_API_BASE: &str = "https://auth.cindy.app";
const MODEL_ACCESS_BASE: &str = "https://model-access.cindy.app";
const DEVICE_LINK_BASE: &str = "https://device-link.cindy.app";
const WEBSITE_URL: &str = "https://cindy.app";

/// 测试用的地址覆盖只接受 127.0.0.1 的 http：避免环境变量把登录凭据导向外部主机。
pub fn loopback_override(raw: Option<&str>) -> Option<String> {
    let raw = raw.filter(|r| !r.is_empty())?;
    let u = url::Url::parse(raw).ok()?;
    let ok = u.scheme() == "http" && u.host_str() == Some("127.0.0.1") && u.username().is_empty() && u.password().is_none() && u.query().is_none() && u.fragment().is_none();
    ok.then(|| u.to_string().trim_end_matches('/').to_string())
}

fn env_override(name: &str) -> Option<String> {
    let v = std::env::var(name).ok();
    let r = loopback_override(v.as_deref());
    if v.as_deref().map_or(false, |s| !s.is_empty()) && r.is_none() {
        eprintln!("[cloud-endpoints] ignoring {name}: only http://127.0.0.1 is allowed");
    }
    r
}

/// 自建服务器地址（`cindy-server`）：同一个服务同时提供登录和设备互联。
/// 只接受 https，或回环地址的 http；带用户名密码、查询、片段的一律拒绝，避免登录凭据被导向意外的位置。
pub fn parse_server_url(raw: &str) -> Option<String> {
    let u = url::Url::parse(raw.trim()).ok()?;
    let loopback = matches!(u.host_str(), Some("127.0.0.1" | "localhost" | "[::1]"));
    let scheme_ok = u.scheme() == "https" || (u.scheme() == "http" && loopback);
    let clean = u.username().is_empty() && u.password().is_none() && u.query().is_none() && u.fragment().is_none() && u.host_str().is_some();
    (scheme_ok && clean).then(|| u.to_string().trim_end_matches('/').to_string())
}

static SERVER_BASE: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();

/// 启动时调用一次：环境变量 `CINDY_SERVER_URL` 优先，其次数据目录下 `server.json` 的 `{"url": "..."}`。
/// 配置了但不合法时只记日志并退回官方地址，不静默改用别的主机。
pub fn init_server_base(data_dir: &std::path::Path) {
    let from_env = std::env::var("CINDY_SERVER_URL").ok().filter(|v| !v.is_empty());
    let from_file = || {
        let text = std::fs::read_to_string(data_dir.join("server.json")).ok()?;
        serde_json::from_str::<Value>(&text).ok()?.get("url")?.as_str().map(String::from)
    };
    let raw = from_env.or_else(from_file);
    let parsed = raw.as_deref().and_then(|r| {
        let p = parse_server_url(r);
        if p.is_none() {
            eprintln!("[cloud-endpoints] ignoring server url: only https, or http on localhost, is allowed");
        }
        p
    });
    if let Some(u) = &parsed {
        eprintln!("[cloud-endpoints] using custom server {u}");
    }
    let _ = SERVER_BASE.set(parsed);
}

fn custom_server() -> Option<String> {
    SERVER_BASE.get().cloned().flatten()
}

pub fn auth_base_url() -> String {
    env_override("CINDY_TAURI_AUTH_BASE_URL").or_else(custom_server).unwrap_or_else(|| AUTH_API_BASE.into())
}
pub fn model_access_base_url() -> String {
    env_override("CINDY_TAURI_MODEL_ACCESS_BASE_URL").or_else(custom_server).unwrap_or_else(|| MODEL_ACCESS_BASE.into())
}
pub fn device_link_base_url() -> String {
    env_override("CINDY_TAURI_DEVICE_LINK_BASE_URL").or_else(custom_server).unwrap_or_else(|| DEVICE_LINK_BASE.into())
}
pub fn desktop_callback_url() -> String {
    format!("{}/api/auth/desktop/callback", auth_base_url())
}
#[allow(dead_code)]
pub fn website_url() -> &'static str {
    WEBSITE_URL
}

/// 生产端口：应用会话 / 密钥库在 Rust 里直接读写；推送给 webview 与 sidecar（手机端）；打开浏览器走 opener。
pub trait Ports: Send + Sync + 'static {
    fn emit(&self, channel: &str, arg: Value);
    fn open_url(&self, url: &str) -> Result<(), String>;
    fn locale(&self) -> String;
}

pub struct AppEnv {
    pub owner: Arc<Owner>,
    pub secrets: Arc<SecretStore>,
    pub ports: Arc<dyn Ports>,
    pub device_id: String,
    /// 测试专用的认证服务地址；生产为 None，走 `CINDY_TAURI_AUTH_BASE_URL`（仅回环）或线上地址。
    pub auth_base: Option<String>,
}

impl Env for AppEnv {
    fn mode(&self) -> Mode {
        self.owner.mode()
    }
    fn cached_user(&self) -> Option<Value> {
        self.owner.cached_user()
    }
    fn stamp(&self) -> Value {
        self.owner.stamp()
    }
    fn commit(&self, mode: Mode, user: Option<&Value>) -> Result<(), String> {
        self.owner.commit(mode, user).map(|_| ())
    }
    fn secrets_ready(&self) -> BoxFut<bool> {
        let s = self.secrets.clone();
        // 等主密钥可能卡在钥匙串授权框：放 blocking 线程。
        Box::pin(async move { tauri::async_runtime::spawn_blocking(move || s.when_ready(Duration::from_secs(15))).await.unwrap_or(false) })
    }
    fn secret_get(&self, name: &str) -> Option<String> {
        self.secrets.get(name)
    }
    fn secret_has(&self, name: &str) -> bool {
        self.secrets.has(name)
    }
    fn secret_set(&self, name: &str, value: &str) -> Result<(), String> {
        // 登录凭据键对 renderer 不可见，变更监听方（model-access）靠 auth 通知，不需要 secret 通知。
        self.secrets.set_quiet(name, value)
    }
    fn secret_remove(&self, name: &str) -> Result<(), String> {
        self.secrets.remove_quiet(name).map(|_| ())
    }
    fn emit(&self, channel: &str, arg: Value) {
        self.ports.emit(channel, arg);
    }
    fn open_url(&self, url: &str) -> BoxFut<Result<(), String>> {
        let (p, u) = (self.ports.clone(), url.to_string());
        Box::pin(async move { tauri::async_runtime::spawn_blocking(move || p.open_url(&u)).await.map_err(|e| e.to_string())? })
    }
    fn locale(&self) -> String {
        self.ports.locale()
    }
    fn auth_base_url(&self) -> String {
        self.auth_base.clone().unwrap_or_else(auth_base_url)
    }
    fn desktop_callback_url(&self) -> String {
        match &self.auth_base {
            Some(b) => format!("{b}/api/auth/desktop/callback"),
            None => desktop_callback_url(),
        }
    }
    fn device_id(&self) -> String {
        self.device_id.clone()
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn server_url_accepts_https_or_loopback_http_only() {
        assert_eq!(super::parse_server_url("https://my.example.com/").as_deref(), Some("https://my.example.com"));
        assert_eq!(super::parse_server_url(" https://my.example.com:8443 ").as_deref(), Some("https://my.example.com:8443"));
        assert_eq!(super::parse_server_url("http://127.0.0.1:8787").as_deref(), Some("http://127.0.0.1:8787"));
        assert_eq!(super::parse_server_url("http://localhost:8787/").as_deref(), Some("http://localhost:8787"));
        for bad in ["http://my.example.com", "ftp://x.com", "https://u:p@x.com", "https://x.com/?a=1", "https://x.com/#f", "", "not a url", "https://"] {
            assert_eq!(super::parse_server_url(bad), None, "{bad}");
        }
    }

    use super::loopback_override;

    #[test]
    fn only_loopback_http_is_accepted() {
        assert_eq!(loopback_override(Some("http://127.0.0.1:8080")), Some("http://127.0.0.1:8080".into()));
        assert_eq!(loopback_override(Some("http://127.0.0.1:8080///")), Some("http://127.0.0.1:8080".into()));
        for bad in ["https://127.0.0.1:1", "http://localhost:1", "http://evil.com", "http://u:p@127.0.0.1:1", "http://127.0.0.1:1/?a=1", "http://127.0.0.1:1/#x", "", "nonsense"] {
            assert_eq!(loopback_override(Some(bad)), None, "{bad}");
        }
        assert_eq!(loopback_override(None), None);
    }
}
