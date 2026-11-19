//! 本机密钥存储（原 sidecar secret-store.mjs），对齐 Electron safeStorage 的模型。
//! - 32 字节主密钥放系统钥匙串（secrets.rs），启动后由 main 交给 `unlock`；
//! - 每个条目 AES-256-GCM 加密（条目名作 AAD，防止密文挪到别的名字下），落 <dataDir>/secrets.v1.json，
//!   文件格式与 JS 版逐字节兼容（iv / tag / data 均 base64）；
//! - 主密钥换了（钥匙串条目被删后重建）时旧文件无法解密：改名保留为 .unrecoverable-*，从空开始；
//! - 明文只在内存里按需解密，不写日志。

use aes_gcm::aead::{generic_array::GenericArray, AeadInPlace, KeyInit};
use aes_gcm::Aes256Gcm;
use base64::{engine::general_purpose::STANDARD as B64, Engine};
use serde_json::{json, Map, Value};
use std::path::{Path, PathBuf};
use std::sync::{Condvar, Mutex};
use std::time::Duration;

const CHECK_NAME: &str = "__check__";
const CHECK_PLAIN: &str = "cindy-tauri-secret-store-v1";
const MAX_VALUE_CHARS: usize = 64 * 1024;
const READY_TIMEOUT: Duration = Duration::from_secs(15);
/// 登录凭据只在宿主内部使用（auth），renderer 的通用通道读写不到。
const AUTH_ONLY_KEYS: [&str; 4] = ["cindy_auth_session_v1", "cindy_auth_accounts_v1", "cindy_auth_refresh_token", "refresh_token"];
/// desktop providerSecrets.ts 的 MAIN_ONLY_PROVIDER_SECRET_STORAGE_KEYS（小写）。
const MAIN_ONLY_KEYS: [&str; 6] = [
    "voice_input_asr_api_key",
    "provider_key_gemini",
    "provider_key_openai_images",
    "remote_mcp_bridge_token",
    "pi_proxy_derivation_key",
    "mcp_app_revision_key",
];
const MAIN_ONLY_PREFIXES: [&str; 3] = ["provider_headers_", "bot_environment_", "ghost_alias_migration_"];

pub const INVOKE_CHANNELS: &[&str] = &["safe-storage-store", "safe-storage-read", "safe-storage-remove"];

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum State {
    /// 等主密钥。
    Locked,
    Ready,
    /// 钥匙串不可用或主密钥非法。
    Unavailable,
}

impl State {
    pub fn as_str(self) -> &'static str {
        match self {
            State::Locked => "locked",
            State::Ready => "ready",
            State::Unavailable => "unavailable",
        }
    }
}

struct Inner {
    state: State,
    reason: Option<String>,
    key: Option<[u8; 32]>,
    entries: Map<String, Value>,
}

type Listener = Box<dyn Fn(Option<&str>) + Send + Sync>;

pub struct SecretStore {
    file: PathBuf,
    inner: Mutex<Inner>,
    settled: Condvar,
    listeners: Mutex<Vec<Listener>>,
}

fn err<T>(code: &str, msg: &str) -> Result<T, String> {
    Err(format!("[{code}] {msg}"))
}

fn valid_name(name: &str) -> bool {
    (1..=200).contains(&name.len()) && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

pub fn is_custom_provider_runtime_key_name(name: &str) -> bool {
    // ^provider_key_[a-zA-Z0-9_-]+_(?:claude-code|codex|pi)$
    let Some(rest) = name.strip_prefix("provider_key_") else { return false };
    ["_claude-code", "_codex", "_pi"].iter().any(|suf| {
        rest.strip_suffix(suf).is_some_and(|id| !id.is_empty() && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-'))
    })
}

fn renderer_key(key: &str) -> bool {
    if !valid_name(key) || key == CHECK_NAME {
        return false;
    }
    let lower = key.to_lowercase();
    !AUTH_ONLY_KEYS.contains(&lower.as_str())
        && !lower.starts_with("cindy_auth_")
        && !MAIN_ONLY_KEYS.contains(&lower.as_str())
        && !MAIN_ONLY_PREFIXES.iter().any(|p| lower.starts_with(p))
}

fn cipher(key: &[u8; 32]) -> Aes256Gcm {
    Aes256Gcm::new(GenericArray::from_slice(key))
}

fn encrypt(key: &[u8; 32], name: &str, plain: &str) -> Result<Value, String> {
    let mut iv = [0u8; 12];
    getrandom::fill(&mut iv).map_err(|e| format!("[INTERNAL] random source failed: {e}"))?;
    let mut buf = plain.as_bytes().to_vec();
    let tag = cipher(key)
        .encrypt_in_place_detached(GenericArray::from_slice(&iv), name.as_bytes(), &mut buf)
        .map_err(|_| "[INTERNAL] encrypt failed".to_string())?;
    Ok(json!({ "iv": B64.encode(iv), "tag": B64.encode(tag), "data": B64.encode(&buf) }))
}

fn decrypt(key: &[u8; 32], name: &str, blob: &Value) -> Option<String> {
    let field = |k: &str| blob.get(k).and_then(Value::as_str).and_then(|s| B64.decode(s).ok());
    let (iv, tag, mut data) = (field("iv")?, field("tag")?, field("data")?);
    if iv.len() != 12 || tag.len() != 16 {
        return None;
    }
    cipher(key)
        .decrypt_in_place_detached(GenericArray::from_slice(&iv), name.as_bytes(), &mut data, GenericArray::from_slice(&tag))
        .ok()?;
    String::from_utf8(data).ok()
}

fn is_blob(v: &Value) -> bool {
    ["iv", "tag", "data"].iter().all(|k| v.get(k).is_some_and(Value::is_string))
}

fn load(file: &Path, key: &[u8; 32]) -> Map<String, Value> {
    let Ok(text) = std::fs::read_to_string(file) else { return Map::new() };
    let parsed: Option<Value> = serde_json::from_str(&text).ok();
    let ok = parsed.as_ref().and_then(|v| v.get("check")).filter(|c| is_blob(c)).and_then(|c| decrypt(key, CHECK_NAME, c)).as_deref() == Some(CHECK_PLAIN);
    let Some(raw) = parsed.filter(|_| ok) else {
        let ms = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or(0);
        let aside = PathBuf::from(format!("{}.unrecoverable-{ms}", file.display()));
        eprintln!("[secret-store] existing secrets cannot be decrypted; moved to {}", aside.file_name().and_then(|n| n.to_str()).unwrap_or("?"));
        let _ = std::fs::rename(file, &aside);
        return Map::new();
    };
    let mut entries = Map::new();
    if let Some(m) = raw.get("entries").and_then(Value::as_object) {
        for (name, blob) in m {
            if valid_name(name) && is_blob(blob) {
                entries.insert(name.clone(), blob.clone());
            }
        }
    }
    entries
}

fn write_private(file: &Path, text: &str) -> Result<(), String> {
    use std::io::Write;
    if let Some(dir) = file.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    let tmp = PathBuf::from(format!("{}.tmp-{}", file.display(), std::process::id()));
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let res = opts.open(&tmp).and_then(|mut f| f.write_all(text.as_bytes())).and_then(|_| std::fs::rename(&tmp, file));
    if let Err(e) = res {
        let _ = std::fs::remove_file(&tmp);
        return Err(e.to_string());
    }
    Ok(())
}

impl SecretStore {
    pub fn new(data_dir: &Path) -> Self {
        Self {
            file: data_dir.join("secrets.v1.json"),
            inner: Mutex::new(Inner { state: State::Locked, reason: None, key: None, entries: Map::new() }),
            settled: Condvar::new(),
            listeners: Mutex::new(Vec::new()),
        }
    }

    pub fn owns(channel: &str) -> bool {
        INVOKE_CHANNELS.contains(&channel)
    }

    /// 变更监听：`None` 表示整体状态变化（解锁 / 不可用），`Some(name)` 表示单个条目。回调在持锁之外执行。
    pub fn on_change(&self, f: impl Fn(Option<&str>) + Send + Sync + 'static) {
        self.listeners.lock().unwrap().push(Box::new(f));
    }

    fn notify(&self, name: Option<&str>) {
        for f in self.listeners.lock().unwrap().iter() {
            f(name);
        }
    }

    /// 送入主密钥（64 位小写十六进制）。只接受一次。
    pub fn unlock(&self, hex: &str) -> Result<usize, String> {
        let mut g = self.inner.lock().unwrap();
        if g.state != State::Locked {
            return err("PRECONDITION_FAILED", "secret store already initialized");
        }
        let valid = hex.len() == 64 && hex.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
        if !valid {
            g.state = State::Unavailable;
            g.reason = Some("invalid master key".into());
            drop(g);
            self.settled.notify_all();
            self.notify(None);
            return err("INVALID_PARAMS", "master key must be 64 lowercase hex chars");
        }
        let mut key = [0u8; 32];
        for (i, b) in key.iter_mut().enumerate() {
            *b = u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).unwrap();
        }
        g.entries = load(&self.file, &key);
        g.key = Some(key);
        g.state = State::Ready;
        let n = g.entries.len();
        drop(g);
        self.settled.notify_all();
        self.notify(None);
        Ok(n)
    }

    /// 钥匙串不可用（例如用户拒绝访问）。带 key 的 provider 显示为未连接，保存 key 时报错。
    pub fn mark_unavailable(&self, reason: &str) -> bool {
        let mut g = self.inner.lock().unwrap();
        if g.state != State::Locked {
            return false;
        }
        g.state = State::Unavailable;
        let r: String = reason.chars().take(300).collect();
        eprintln!("[secret-store] unavailable: {r}");
        g.reason = Some(r);
        drop(g);
        self.settled.notify_all();
        self.notify(None);
        true
    }

    pub fn state(&self) -> (State, Option<String>) {
        let g = self.inner.lock().unwrap();
        (g.state, g.reason.clone())
    }

    /// 等主密钥到位；超时或不可用返回 false。
    pub fn when_ready(&self, timeout: Duration) -> bool {
        let g = self.inner.lock().unwrap();
        let (g, _) = self.settled.wait_timeout_while(g, timeout, |i| i.state == State::Locked).unwrap();
        g.state == State::Ready
    }

    pub fn has(&self, name: &str) -> bool {
        let g = self.inner.lock().unwrap();
        g.state == State::Ready && g.entries.contains_key(name)
    }

    /// 读明文；未解锁、不存在或解密失败返回 None。
    pub fn get(&self, name: &str) -> Option<String> {
        let g = self.inner.lock().unwrap();
        if g.state != State::Ready {
            return None;
        }
        decrypt(g.key.as_ref()?, name, g.entries.get(name)?)
    }

    fn require_ready(&self) -> Result<(), String> {
        if self.when_ready(READY_TIMEOUT) {
            Ok(())
        } else {
            err("CREDENTIAL_STORAGE_UNAVAILABLE", "system keychain is not available, so API keys cannot be saved or read")
        }
    }

    fn persist(&self, g: &Inner) -> Result<(), String> {
        let key = g.key.as_ref().ok_or("[INTERNAL] store locked")?;
        let doc = json!({ "version": 1, "check": encrypt(key, CHECK_NAME, CHECK_PLAIN)?, "entries": Value::Object(g.entries.clone()) });
        write_private(&self.file, &serde_json::to_string_pretty(&doc).map_err(|e| e.to_string())?)
    }

    pub fn set(&self, name: &str, value: &str) -> Result<(), String> {
        self.set_inner(name, value, true)
    }

    /// 不触发变更监听：sidecar 经 localdb 协议写入时用，它自己在本地通知，避免回推一遍。
    pub fn set_quiet(&self, name: &str, value: &str) -> Result<(), String> {
        self.set_inner(name, value, false)
    }

    fn set_inner(&self, name: &str, value: &str, notify: bool) -> Result<(), String> {
        if !valid_name(name) || name == CHECK_NAME {
            return err("INVALID_PARAMS", "invalid secret name");
        }
        if value.is_empty() || value.chars().count() > MAX_VALUE_CHARS {
            return err("INVALID_PARAMS", "invalid secret value");
        }
        self.require_ready()?;
        {
            let mut g = self.inner.lock().unwrap();
            let key = g.key.ok_or("[INTERNAL] store locked")?;
            let blob = encrypt(&key, name, value)?;
            let prev = g.entries.insert(name.to_string(), blob);
            if let Err(e) = self.persist(&g) {
                // 落盘失败：内存回滚，保持与磁盘一致。
                match prev {
                    Some(p) => g.entries.insert(name.to_string(), p),
                    None => g.entries.remove(name),
                };
                return Err(e);
            }
        }
        if notify {
            self.notify(Some(name));
        }
        Ok(())
    }

    pub fn remove(&self, name: &str) -> Result<bool, String> {
        self.remove_inner(name, true)
    }

    pub fn remove_quiet(&self, name: &str) -> Result<bool, String> {
        self.remove_inner(name, false)
    }

    fn remove_inner(&self, name: &str, notify: bool) -> Result<bool, String> {
        if !valid_name(name) || name == CHECK_NAME {
            return err("INVALID_PARAMS", "invalid secret name");
        }
        self.require_ready()?;
        {
            let mut g = self.inner.lock().unwrap();
            let Some(prev) = g.entries.remove(name) else { return Ok(false) };
            if let Err(e) = self.persist(&g) {
                g.entries.insert(name.to_string(), prev);
                return Err(e);
            }
        }
        if notify {
            self.notify(Some(name));
        }
        Ok(true)
    }

    /// renderer 的通用 safeStorage 通道（desktop bootstrap-electron.ts safe-storage-*）：
    /// 键名规则和 main-only 黑名单与 desktop 相同；provider key 可回读，用于编辑表单回填。
    pub fn invoke(&self, channel: &str, args: &[Value]) -> Option<Result<Value, String>> {
        let key = args.first().and_then(Value::as_str).filter(|k| renderer_key(k));
        Some(match channel {
            "safe-storage-store" => {
                let (Some(key), Some(value)) = (key, args.get(1).and_then(Value::as_str)) else { return Some(Ok(json!(false))) };
                if value.chars().count() > MAX_VALUE_CHARS || !self.when_ready(READY_TIMEOUT) {
                    return Some(Ok(json!(false)));
                }
                let r = if value.is_empty() { self.remove(key).map(|_| ()) } else { self.set(key, value) };
                r.map(|_| json!(true))
            }
            "safe-storage-read" => {
                let Some(key) = key else { return Some(Ok(Value::Null)) };
                if !self.when_ready(READY_TIMEOUT) {
                    if is_custom_provider_runtime_key_name(key) {
                        return Some(err("INTERNAL", "custom provider credential is unavailable"));
                    }
                    return Some(Ok(Value::Null));
                }
                Ok(self.get(key).map(Value::String).unwrap_or(Value::Null))
            }
            "safe-storage-remove" => {
                let Some(key) = key else { return Some(Ok(json!({ "success": false, "error": "invalid key" }))) };
                if !self.when_ready(READY_TIMEOUT) {
                    return Some(Ok(json!({ "success": false, "error": "secure storage unavailable" })));
                }
                self.remove(key).map(|_| json!({ "success": true }))
            }
            _ => return None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn dir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("secret-store-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }
    const KEY: &str = "0707070707070707070707070707070707070707070707070707070707070707";

    /// 旧 JS 版（sidecar secret-store.mjs）写出的文件，Rust 必须能原样读出。
    #[test]
    fn reads_file_written_by_the_js_implementation() {
        let fx: Value = serde_json::from_str(include_str!("secret_store_js_fixture.json")).unwrap();
        assert_eq!(fx["key"], KEY);
        let d = dir("js");
        std::fs::write(d.join("secrets.v1.json"), fx["file"].to_string()).unwrap();
        let s = SecretStore::new(&d);
        assert_eq!(s.unlock(KEY).unwrap(), 2);
        for (k, v) in fx["plain"].as_object().unwrap() {
            assert_eq!(s.get(k).as_deref(), v.as_str(), "{k}");
        }
        // 条目名是 AAD：把密文挪到别的名字下读不出来
        let moved = fx["file"]["entries"]["api_key"].clone();
        let mut f = fx["file"].clone();
        f["entries"]["other_name"] = moved;
        let d2 = dir("js-moved");
        std::fs::write(d2.join("secrets.v1.json"), f.to_string()).unwrap();
        let s2 = SecretStore::new(&d2);
        s2.unlock(KEY).unwrap();
        assert_eq!(s2.get("other_name"), None);
        assert!(s2.has("other_name"));
    }

    #[test]
    fn lifecycle_persist_and_wrong_key() {
        let d = dir("life");
        let s = SecretStore::new(&d);
        assert_eq!(s.state().0, State::Locked);
        assert!(!s.when_ready(Duration::from_millis(20)));
        assert_eq!(s.get("api_key"), None);
        s.unlock(KEY).unwrap();
        assert!(s.unlock(KEY).unwrap_err().contains("PRECONDITION_FAILED"));
        assert!(!s.mark_unavailable("late"));
        s.set("api_key", "sk-中文").unwrap();
        s.set("mivo_api_key", "m").unwrap();
        assert!(s.set("bad name", "x").unwrap_err().contains("INVALID_PARAMS"));
        assert!(s.set("__check__", "x").unwrap_err().contains("INVALID_PARAMS"));
        assert!(s.set("api_key", "").unwrap_err().contains("INVALID_PARAMS"));
        assert_eq!(s.remove("nope").unwrap(), false);
        let text = std::fs::read_to_string(d.join("secrets.v1.json")).unwrap();
        assert!(!text.contains("sk-") && !text.contains("中文"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(d.join("secrets.v1.json")).unwrap().permissions().mode() & 0o777, 0o600);
        }
        // 重启后读得到；删除后落盘
        let s2 = SecretStore::new(&d);
        s2.unlock(KEY).unwrap();
        assert_eq!(s2.get("api_key").as_deref(), Some("sk-中文"));
        assert!(s2.remove("api_key").unwrap());
        let s3 = SecretStore::new(&d);
        s3.unlock(KEY).unwrap();
        assert!(!s3.has("api_key") && s3.has("mivo_api_key"));
        // 主密钥换了：旧文件改名保留，从空开始
        let s4 = SecretStore::new(&d);
        s4.unlock(&"11".repeat(32)).unwrap();
        assert!(!s4.has("mivo_api_key"));
        let aside = std::fs::read_dir(&d).unwrap().filter_map(Result::ok).any(|e| e.file_name().to_string_lossy().contains(".unrecoverable-"));
        assert!(aside);
    }

    #[test]
    fn invalid_key_and_unavailable_states() {
        let s = SecretStore::new(&dir("bad"));
        assert!(s.unlock("ABC").unwrap_err().contains("INVALID_PARAMS"));
        assert_eq!(s.state(), (State::Unavailable, Some("invalid master key".into())));
        assert!(s.set("api_key", "x").unwrap_err().contains("CREDENTIAL_STORAGE_UNAVAILABLE"));
        let u = SecretStore::new(&dir("unavail"));
        assert!(u.mark_unavailable("denied"));
        assert!(!u.when_ready(Duration::from_millis(10)));
        assert!(u.unlock(KEY).unwrap_err().contains("PRECONDITION_FAILED"));
    }

    #[test]
    fn change_listeners_and_waiters() {
        let d = dir("listen");
        let s = Arc::new(SecretStore::new(&d));
        let seen = Arc::new(Mutex::new(Vec::<Option<String>>::new()));
        let sink = seen.clone();
        s.on_change(move |n| sink.lock().unwrap().push(n.map(String::from)));
        let waiter = { let s = s.clone(); std::thread::spawn(move || s.when_ready(Duration::from_secs(5))) };
        std::thread::sleep(Duration::from_millis(50));
        s.unlock(KEY).unwrap();
        assert!(waiter.join().unwrap());
        s.set("api_key", "a").unwrap();
        s.remove("api_key").unwrap();
        assert_eq!(*seen.lock().unwrap(), vec![None, Some("api_key".into()), Some("api_key".into())]);
    }

    #[test]
    fn renderer_channels_enforce_key_rules() {
        let s = SecretStore::new(&dir("renderer"));
        s.unlock(KEY).unwrap();
        let call = |ch: &str, a: Vec<Value>| s.invoke(ch, &a).unwrap().unwrap();
        assert_eq!(call("safe-storage-store", vec![json!("api_key"), json!("v")]), json!(true));
        assert_eq!(call("safe-storage-read", vec![json!("api_key")]), json!("v"));
        for blocked in ["cindy_auth_session_v1", "Cindy_Auth_x", "refresh_token", "provider_key_gemini", "PROVIDER_HEADERS_x_codex", "bot_environment_1", "ghost_alias_migration_1", "pi_proxy_derivation_key", "bad key", "__check__"] {
            assert_eq!(call("safe-storage-store", vec![json!(blocked), json!("v")]), json!(false), "{blocked}");
            assert_eq!(call("safe-storage-read", vec![json!(blocked)]), Value::Null, "{blocked}");
            assert_eq!(call("safe-storage-remove", vec![json!(blocked)]), json!({ "success": false, "error": "invalid key" }), "{blocked}");
        }
        // 内部写入的受限键，renderer 读不到
        s.set("cindy_auth_session_v1", "secret").unwrap();
        assert_eq!(call("safe-storage-read", vec![json!("cindy_auth_session_v1")]), Value::Null);
        // 空串 = 删除；超长拒绝；非字符串值拒绝
        assert_eq!(call("safe-storage-store", vec![json!("api_key"), json!("")]), json!(true));
        assert!(!s.has("api_key"));
        assert_eq!(call("safe-storage-store", vec![json!("api_key"), json!("x".repeat(MAX_VALUE_CHARS + 1))]), json!(false));
        assert_eq!(call("safe-storage-store", vec![json!("api_key"), json!(5)]), json!(false));
        assert_eq!(call("safe-storage-remove", vec![json!("api_key")]), json!({ "success": true }));
        assert!(s.invoke("other", &[]).is_none());
    }

    #[test]
    fn locked_store_degrades_per_channel() {
        let s = SecretStore::new(&dir("locked"));
        s.mark_unavailable("no keychain");
        let call = |ch: &str, a: Vec<Value>| s.invoke(ch, &a).unwrap();
        assert_eq!(call("safe-storage-store", vec![json!("api_key"), json!("v")]).unwrap(), json!(false));
        assert_eq!(call("safe-storage-read", vec![json!("api_key")]).unwrap(), Value::Null);
        assert!(call("safe-storage-read", vec![json!("provider_key_acme_codex")]).unwrap_err().contains("custom provider credential"));
        assert_eq!(call("safe-storage-remove", vec![json!("api_key")]).unwrap(), json!({ "success": false, "error": "secure storage unavailable" }));
        assert!(is_custom_provider_runtime_key_name("provider_key_a-b_c_pi"));
        assert!(!is_custom_provider_runtime_key_name("provider_key__codex"));
        assert!(!is_custom_provider_runtime_key_name("provider_key_x_other"));
    }
}
