//! providers.json 的配置模型与输入校验：移植 model-config.mjs 的 sanitizeConfig / parsePiKey / normalizeBaseUrl 等。
use crate::local_db::mapper::js_trim;
use serde_json::{json, Map, Value};
use super::metadata::{is_pos_safe_int, utf16_len};
use regex::Regex;
use std::sync::OnceLock;

pub const SMOKE_PROVIDER_ID: &str = "smoke";
pub const MAX_MODELS: usize = 512;
pub const MAX_ORDER_ITEMS: usize = 256;
pub const MAX_KEY_CHARS: usize = 16384;
pub const AGENTS: [&str; 3] = ["claude-code", "codex", "pi"];
pub const RESERVED_IDS: [&str; 6] = ["anthropic", "openai", "xd", "xai", "cindy", SMOKE_PROVIDER_ID];
pub const NOT_IMPLEMENTED_SECRET: &str = "providers with OAuth or custom request headers are not supported in the Tauri build yet";

pub fn ipc_error(code: &str, message: &str) -> String {
    format!("[{code}] {message}")
}

fn provider_id_re() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new(r"^[a-z0-9_-]{1,64}$").unwrap())
}
fn model_id_re() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new(r"^[A-Za-z0-9._:/-]{1,128}$").unwrap())
}
pub fn valid_provider_id(id: &str) -> bool {
    provider_id_re().is_match(id)
}
pub fn valid_model_id(id: &str) -> bool {
    model_id_re().is_match(id) && utf16_len(id) <= 128
}

/// 与 index.mjs validateModel 同一规则：HTTPS 或回环 HTTP，不能内嵌凭据。返回去掉尾部斜杠的 href。
pub fn normalize_base_url(raw: &str) -> Option<String> {
    if raw.is_empty() || utf16_len(raw) > 4096 {
        return None;
    }
    let url = url::Url::parse(js_trim(raw)).ok()?;
    let host = url.host_str().unwrap_or("");
    let loopback = host == "127.0.0.1" || host == "localhost" || host == "[::1]" || host == "::1";
    if !url.username().is_empty() || url.password().is_some() || url.query().is_some() || url.fragment().is_some() {
        return None;
    }
    if !(url.scheme() == "https" || (url.scheme() == "http" && loopback)) {
        return None;
    }
    Some(url.as_str().trim_end_matches('/').to_string())
}

/// 与 desktop customProviderSecretStorageKey(id, 'pi') 一致。
pub fn pi_key_name(provider_id: &str) -> String {
    format!("provider_key_{provider_id}_pi")
}

pub fn uses_api_key(config: &Value) -> bool {
    config["auth"]["method"] == "apiKey"
}

/// desktop parseRuntimeKeys：{ [agent]: string }；返回去空白后的 pi key（可能为空串）。
pub fn parse_pi_key(keys: Option<&Value>) -> Result<String, String> {
    let bad = || ipc_error("INVALID_PARAMS", "invalid provider runtime keys");
    let Some(keys) = keys.filter(|k| !k.is_null()) else { return Ok(String::new()) };
    let o = keys.as_object().ok_or_else(bad)?;
    for (agent, v) in o {
        if !AGENTS.contains(&agent.as_str()) || !v.as_str().is_some_and(|s| utf16_len(s) <= MAX_KEY_CHARS) {
            return Err(bad());
        }
    }
    Ok(js_trim(o.get("pi").and_then(Value::as_str).unwrap_or("")).to_string())
}

fn truthy_obj(v: &Value) -> bool {
    v.as_object().is_some_and(|o| !o.is_empty())
}

/// 把 renderer 传来的 CustomProviderConfig 收敛成只含 pi runtime、无凭据的规整形状；非法给 IPC 错误。
pub fn sanitize_config(input: &Value) -> Result<Value, String> {
    let bad = |m: &str| ipc_error("INVALID_PARAMS", m);
    let o = input.as_object().ok_or_else(|| bad("config must be an object"))?;
    let id = o.get("id").and_then(Value::as_str).filter(|i| valid_provider_id(i)).ok_or_else(|| bad("id must match /^[a-z0-9_-]{1,64}$/"))?;
    let name = o.get("name").and_then(Value::as_str).filter(|n| !js_trim(n).is_empty() && utf16_len(n) <= 120).ok_or_else(|| bad("name required"))?;
    let auth = o.get("auth").filter(|a| !a.is_null());
    if auth.is_some_and(|a| !a.is_object()) {
        return Err(bad("invalid auth"));
    }
    let method = auth.and_then(|a| a.get("method")).filter(|m| !m.is_null()).map(|m| m.as_str().unwrap_or("\u{0}").to_string()).unwrap_or_else(|| "apiKey".into());
    if method == "oauth" {
        return Err(ipc_error("NOT_IMPLEMENTED", NOT_IMPLEMENTED_SECRET));
    }
    if method != "none" && method != "apiKey" {
        return Err(bad("invalid auth method"));
    }
    let runtimes = o.get("runtimes").and_then(Value::as_object).ok_or_else(|| bad("runtimes required"))?;
    if runtimes.keys().any(|a| !AGENTS.contains(&a.as_str())) {
        return Err(bad("invalid runtime"));
    }
    if runtimes.len() != 1 || !runtimes.contains_key("pi") {
        return Err(ipc_error("NOT_IMPLEMENTED", "only the Pi runtime is supported in the Tauri build"));
    }
    let rt = &runtimes["pi"];
    let rto = rt.as_object().ok_or_else(|| bad("pi runtime required"))?;
    if rto.get("headers").is_some_and(truthy_obj) {
        return Err(ipc_error("NOT_IMPLEMENTED", NOT_IMPLEMENTED_SECRET));
    }
    if rto.get("wireProtocol").is_some_and(|w| !w.is_null() && *w != "openai-chat") {
        return Err(ipc_error("NOT_IMPLEMENTED", "only OpenAI Chat Completions compatible endpoints are supported"));
    }
    let base_url = rto.get("baseUrl").and_then(Value::as_str).and_then(normalize_base_url).ok_or_else(|| bad("baseUrl must be HTTPS or loopback HTTP without credentials"))?;
    let mut models_url: Option<String> = None;
    if let Some(m) = rto.get("modelsUrl").and_then(Value::as_str).filter(|m| !js_trim(m).is_empty()) {
        models_url = Some(normalize_base_url(m).ok_or_else(|| bad("modelsUrl must be HTTPS or loopback HTTP without credentials"))?);
    }
    let list = rto.get("models").and_then(Value::as_array).filter(|l| l.len() <= MAX_MODELS).ok_or_else(|| bad("models must be an array"))?;
    let mut seen: Vec<String> = vec![];
    let mut models: Vec<Value> = vec![];
    for m in list {
        let mo = m.as_object();
        let mid = mo.and_then(|m| m.get("id")).and_then(Value::as_str).map(js_trim).filter(|i| valid_model_id(i)).ok_or_else(|| bad("invalid model id"))?;
        if seen.iter().any(|s| s == mid) {
            continue;
        }
        seen.push(mid.to_string());
        let mo = mo.unwrap();
        let mname: String = match mo.get("name").and_then(Value::as_str).map(js_trim).filter(|n| !n.is_empty()) {
            Some(n) => n.encode_utf16().take(200).collect::<Vec<u16>>().pipe(|u| String::from_utf16_lossy(&u)),
            None => mid.to_string(),
        };
        let mut model = Map::new();
        model.insert("id".into(), json!(mid));
        model.insert("name".into(), json!(mname));
        model.insert("reasoning".into(), json!(false));
        if let Some(w) = mo.get("contextWindow").filter(|w| is_pos_safe_int(w)) {
            model.insert("contextWindow".into(), w.clone());
        }
        if let Some(b) = mo.get("defaultEnabled").filter(|b| b.is_boolean()) {
            model.insert("defaultEnabled".into(), b.clone());
        }
        if let Some(b) = mo.get("supportsImageInput").filter(|b| b.is_boolean()) {
            model.insert("supportsImageInput".into(), b.clone());
        }
        models.push(Value::Object(model));
    }
    let mut pi = Map::new();
    pi.insert("baseUrl".into(), json!(base_url));
    pi.insert("wireProtocol".into(), json!("openai-chat"));
    pi.insert("models".into(), json!(models));
    if let Some(u) = models_url {
        pi.insert("modelsUrl".into(), json!(u));
    }
    Ok(json!({ "id": id, "name": js_trim(name), "auth": { "method": method }, "runtimes": { "pi": Value::Object(pi) } }))
}

trait Pipe: Sized {
    fn pipe<R>(self, f: impl FnOnce(Self) -> R) -> R {
        f(self)
    }
}
impl<T> Pipe for T {}

/// 端点（baseUrl / modelsUrl）是否变了：key 绑定端点，变了就不能带去新主机。
pub fn endpoint_changed(prev: Option<&Value>, next: &Value) -> bool {
    let (Some(a), b) = (prev.map(|p| &p["runtimes"]["pi"]), &next["runtimes"]["pi"]) else { return false };
    if !a.is_object() || !b.is_object() {
        return false;
    }
    a["baseUrl"] != b["baseUrl"] || a["modelsUrl"].as_str().unwrap_or("") != b["modelsUrl"].as_str().unwrap_or("")
}

/// 对一次保存应当对 key 做什么：Some(Some(k)) 写入，Some(None) 删除，None 不动。
pub fn plan_key_mutation(config: &Value, pi_key: &str, previous: Option<&Value>) -> Option<Option<String>> {
    if !uses_api_key(config) {
        return previous.filter(|p| uses_api_key(p)).map(|_| None);
    }
    if !pi_key.is_empty() {
        return Some(Some(pi_key.to_string()));
    }
    if previous.is_some() && endpoint_changed(previous, config) {
        return Some(None);
    }
    None
}
