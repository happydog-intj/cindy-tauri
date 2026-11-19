//! 自定义模型服务（原 sidecar/src/model-config.mjs）：providers.json 持久化、连接状态、模型解析、Pi 启动参数。
//! IPC 通道与 `/models` 探测在 `ipc.rs`。云端模型（xd）来自 `model_access::manager::Manager`。
use super::classify::is_model_selectable_for_new_route;
use super::config::*;
use super::view::{build_registry, build_user_provider};
use crate::model_access::manager::{catalog_model_to_descriptor, Emit, Manager as Access, XD_PROVIDER_ID};
use crate::owner::Owner;
use crate::secret_store::SecretStore;
use serde_json::{json, Map, Value};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// pi-rs 要求 provider 有 key 才可选；无鉴权的占位值不会发给任何上游。
pub const KEYLESS_PLACEHOLDER: &str = "cindy-tauri-keyless";
pub const PROVIDER_CHANGED: &str = "maker:provider:changed";
const RESERVED_PI_PROVIDER_IDS: [&str; 5] = ["cindy", "xd", "openai", "anthropic", "xai"];

#[derive(Default, Clone)]
pub struct Store {
    pub providers: Vec<Value>,
    pub provider_order: Vec<String>,
}

pub struct Manager {
    pub(super) path: PathBuf,
    pub(super) store: Mutex<Store>,
    pub(super) visibility: Mutex<Value>,
    pub(super) secrets: Arc<SecretStore>,
    pub(super) owner: Arc<Owner>,
    pub(super) access: Arc<Access>,
    pub(super) emit: Emit,
    smoke: Option<Value>,
    /// 同一 provider 的配置 / key 变更串行执行，避免并发保存把 key 和配置拆开。
    pub(super) queue: tokio::sync::Mutex<()>,
    pub(super) tests: Mutex<HashMap<String, crate::auth::hosted::Cancel>>,
}

fn log(msg: &str) {
    eprintln!("[model-config] {msg}");
}

/// smoke provider：由调用方传入环境变量值（测试不改进程环境）。
pub fn smoke_provider(raw_url: Option<&str>, model_id: Option<&str>) -> Option<Value> {
    let (raw, id) = (raw_url.filter(|s| !s.is_empty())?, model_id.filter(|s| !s.is_empty())?);
    let base = normalize_base_url(raw);
    match base {
        Some(b) if valid_model_id(id) => Some(json!({
            "id": SMOKE_PROVIDER_ID, "name": "Smoke", "auth": { "method": "none" },
            "runtimes": { "pi": { "baseUrl": b, "wireProtocol": "openai-chat", "models": [{ "id": id, "name": id, "reasoning": false }] } },
        })),
        _ => {
            log("smoke model env ignored: invalid base URL or model id");
            None
        }
    }
}

fn load_store(path: &Path) -> Store {
    let Ok(text) = std::fs::read_to_string(path) else { return Store::default() };
    match serde_json::from_str::<Value>(&text) {
        Ok(raw) => Store {
            providers: raw["providers"].as_array().map(|a| a.iter().filter_map(|p| sanitize_config(p).ok()).collect()).unwrap_or_default(),
            provider_order: raw["providerOrder"].as_array().map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect()).unwrap_or_default(),
        },
        Err(e) => {
            // 坏文件改名保留，不覆盖用户数据。
            let ms = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or(0);
            let backup = path.with_extension(format!("json.corrupt-{ms}"));
            log(&format!("providers.json unreadable ({e}); moved to {}", backup.file_name().and_then(|n| n.to_str()).unwrap_or("")));
            let _ = std::fs::rename(path, backup);
            Store::default()
        }
    }
}

impl Manager {
    pub fn new(data_dir: &Path, secrets: Arc<SecretStore>, owner: Arc<Owner>, access: Arc<Access>, emit: Emit, smoke: Option<Value>) -> Arc<Self> {
        let path = data_dir.join("providers.json");
        Arc::new(Self {
            store: Mutex::new(load_store(&path)),
            path,
            visibility: Mutex::new(json!({})),
            secrets,
            owner,
            access,
            emit,
            smoke,
            queue: tokio::sync::Mutex::new(()),
            tests: Mutex::new(HashMap::new()),
        })
    }

    /// 钥匙串解锁、key 写入或删除、云端模型变化后，连接状态可能变化，通知 renderer 重新拉列表。
    pub fn start(self: &Arc<Self>) {
        let w = Arc::downgrade(self);
        self.secrets.on_change(move |name| {
            let Some(m) = w.upgrade() else { return };
            if name.map_or(true, |n| n.strip_prefix("provider_key_").and_then(|r| r.strip_suffix("_pi")).is_some_and(|id| !id.is_empty() && id.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-'))) {
                m.changed();
            }
        });
        let w = Arc::downgrade(self);
        self.access.on_xd_change(move || {
            if let Some(m) = w.upgrade() {
                m.changed();
            }
        });
    }

    pub(super) fn changed(&self) {
        (self.emit)(PROVIDER_CHANGED, json!({}));
    }

    pub(super) fn persist(&self) -> Result<(), String> {
        use std::io::Write;
        let s = self.store.lock().unwrap().clone();
        let doc = json!({ "version": 1, "providers": s.providers, "providerOrder": s.provider_order });
        let text = serde_json::to_string_pretty(&doc).map_err(|e| e.to_string())?;
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        }
        let tmp = self.path.with_extension(format!("json.tmp-{}", std::process::id()));
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let r = opts.open(&tmp).and_then(|mut f| f.write_all(text.as_bytes())).and_then(|_| std::fs::rename(&tmp, &self.path));
        if let Err(e) = r {
            let _ = std::fs::remove_file(&tmp);
            return Err(e.to_string());
        }
        Ok(())
    }

    // ── 查询 ─────────────────────────────────────────────────────────────

    /// 全部 provider（Smoke 在前），按 providerOrder 排序；未出现在 order 里的保持原相对顺序排在后面。
    pub fn all_configs(&self) -> Vec<Value> {
        let s = self.store.lock().unwrap();
        let mut list: Vec<Value> = vec![];
        list.extend(self.smoke.clone());
        list.extend(s.providers.iter().cloned());
        let rank: HashMap<&str, usize> = s.provider_order.iter().enumerate().map(|(i, id)| (id.as_str(), i)).collect();
        let mut keyed: Vec<(usize, usize, Value)> =
            list.into_iter().enumerate().map(|(i, c)| (rank.get(c["id"].as_str().unwrap_or("")).copied().unwrap_or(MAX_ORDER_ITEMS + i), i, c)).collect();
        keyed.sort_by_key(|k| (k.0, k.1));
        keyed.into_iter().map(|k| k.2).collect()
    }

    pub fn is_connected(&self, config: &Value) -> bool {
        if config["auth"]["method"] == "none" {
            return true;
        }
        uses_api_key(config) && self.secrets.has(&pi_key_name(config["id"].as_str().unwrap_or("")))
    }

    /// 已连接 apiKey provider 的明文 key；无鉴权或未存 key 返回 None。
    pub fn read_provider_api_key(&self, provider_id: &str) -> Option<String> {
        let configs = self.all_configs();
        let config = configs.iter().find(|c| c["id"] == provider_id).filter(|c| uses_api_key(c))?;
        let key = self.secrets.get(&pi_key_name(config["id"].as_str()?))?;
        let t = crate::local_db::mapper::js_trim(&key);
        (!t.is_empty()).then(|| t.to_string())
    }

    /// 当前目录的 ProviderView[]（与 desktop maker:provider:list 的 providers 同形状）。
    pub fn provider_views(&self) -> Vec<Value> {
        let configs = self.all_configs();
        let xd = self.access.xd_provider();
        let mut providers: Vec<Value> = xd.iter().cloned().collect();
        providers.extend(configs.iter().map(build_user_provider));
        let mut connected: HashMap<String, bool> = configs.iter().map(|c| (c["id"].as_str().unwrap_or("").to_string(), self.is_connected(c))).collect();
        if xd.is_some() {
            connected.insert(XD_PROVIDER_ID.into(), self.access.xd_connected());
        }
        build_registry(&providers, &connected)
    }

    /// pi 的 availableModels（ModelDescriptor[]）：只取已连接 provider、可用于新路由的模型，按 id 首见去重。
    pub fn pi_model_descriptors(&self) -> Vec<Value> {
        let mut seen: HashSet<String> = HashSet::new();
        let mut out = vec![];
        for p in self.provider_views() {
            if p["connected"] != true || p["routing"]["pi"]["disabled"] == true {
                continue;
            }
            let user = p["source"] == "user";
            for m in p["models"]["pi"].as_array().into_iter().flatten() {
                let id = m["id"].as_str().unwrap_or("").to_string();
                if seen.contains(&id) || !is_model_selectable_for_new_route(m, user) {
                    continue;
                }
                seen.insert(id);
                out.push(catalog_model_to_descriptor(m));
            }
        }
        out
    }

    pub fn provider_order_ids(&self) -> Vec<String> {
        let mut ids = vec![];
        if self.access.xd_provider().is_some() {
            ids.push(XD_PROVIDER_ID.to_string());
        }
        ids.extend(self.all_configs().iter().filter_map(|c| c["id"].as_str().map(String::from)));
        ids
    }

    fn to_run_config(config: &Value, model: &Value) -> Value {
        let mut out = Map::new();
        out.insert("providerId".into(), config["id"].clone());
        out.insert("providerName".into(), config["name"].clone());
        out.insert("modelId".into(), model["id"].clone());
        out.insert("baseUrl".into(), config["runtimes"]["pi"]["baseUrl"].clone());
        out.insert("api".into(), json!("openai-completions"));
        out.insert("authMethod".into(), json!(if uses_api_key(config) { "apiKey" } else { "none" }));
        out.insert("reasoning".into(), json!(false));
        if super::metadata::is_pos_safe_int(&model["contextWindow"]) {
            out.insert("contextLimit".into(), model["contextWindow"].clone());
        }
        Value::Object(out)
    }

    /// renderer 传来的 (providerId, modelId) → Pi 运行配置；找不到或不可用返回 None。
    pub fn resolve_model(&self, provider_id: &str, model_id: &str) -> Option<Value> {
        if provider_id == XD_PROVIDER_ID {
            return self.access.resolve_xd_run_config(model_id);
        }
        let configs = self.all_configs();
        let config = configs.iter().find(|c| c["id"] == provider_id)?;
        if !self.is_connected(config) || !config["runtimes"]["pi"].is_object() {
            return None;
        }
        let model = config["runtimes"]["pi"]["models"].as_array()?.iter().find(|m| m["id"] == model_id)?;
        Some(Self::to_run_config(config, model))
    }

    /// 默认 Pi 运行配置：Smoke 优先，其次按 provider 顺序第一个已连接 provider 的第一个模型，最后是 Cindy AI。
    pub fn model_config(&self) -> Option<Value> {
        let configs = self.all_configs();
        let ordered: Vec<&Value> = configs.iter().filter(|c| c["id"] == SMOKE_PROVIDER_ID).chain(configs.iter().filter(|c| c["id"] != SMOKE_PROVIDER_ID)).collect();
        for config in ordered {
            if self.is_connected(config) {
                if let Some(model) = config["runtimes"]["pi"]["models"].as_array().and_then(|m| m.first()) {
                    return Some(Self::to_run_config(config, model));
                }
            }
        }
        self.access.default_xd_run_config()
    }

    pub fn has_custom_provider(&self, id: &str) -> bool {
        self.store.lock().unwrap().providers.iter().any(|p| p["id"] == id)
    }

    /// 钥匙串主密钥到位（或确认不可用）后再解析模型，避免启动瞬间带 key 的 provider 被当成未连接。
    pub fn when_secrets_ready(&self) -> bool {
        self.secrets.when_ready(std::time::Duration::from_secs(15))
    }
}

// ── Pi 启动参数（pi-launch.mjs）─────────────────────────────────────────────

/// Pi 运行时 provider id 只允许 slug；保留字加前缀避开网关路由。
pub fn pi_runtime_provider_id(provider_id: &str) -> String {
    let lower = provider_id.to_lowercase();
    let mut slug = String::new();
    let mut run = false;
    for c in lower.chars() {
        if c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-' {
            slug.push(c);
            run = false;
        } else if !run {
            slug.push('-');
            run = true;
        }
    }
    let slug = slug.trim_matches('-').to_string();
    let slug = if slug.is_empty() { "custom".to_string() } else { slug };
    if RESERVED_PI_PROVIDER_IDS.contains(&slug.as_str()) { format!("cindy-tauri-{slug}") } else { slug }
}

fn truthy_num(v: &Value) -> bool {
    v.as_f64().is_some_and(|n| n != 0.0 && !n.is_nan())
}

impl Manager {
    /// 把 (providerId, model) 落到运行配置上；缺 providerId 时用当前默认配置。
    pub fn resolve_session_model(&self, provider_id: Option<&str>, model_id: Option<&str>) -> Result<Value, String> {
        let provider_id = provider_id.filter(|s| !s.is_empty());
        let model_id = model_id.filter(|s| !s.is_empty());
        // renderer 选 Cindy AI 模型时 providerId 为空（网关）：先按 xd 找这个模型，找不到才退回默认模型。
        let gateway_hit = match (provider_id, model_id) {
            (None, Some(m)) => self.resolve_model(XD_PROVIDER_ID, m),
            _ => None,
        };
        let configured = || match (provider_id, model_id) {
            (Some(p), Some(m)) => self.resolve_model(p, m),
            _ => None,
        };
        let cfg = gateway_hit.or_else(configured).or_else(|| if provider_id.is_some() { None } else { self.model_config() });
        let Some(cfg) = cfg else {
            return Err(format!("[MODEL_NOT_CONFIGURED] model {}/{} is not configured", provider_id.unwrap_or("(default)"), model_id.unwrap_or("")));
        };
        if let (Some(m), Some(p)) = (model_id, provider_id) {
            if cfg["modelId"] != m {
                return Err(format!("[MODEL_NOT_CONFIGURED] model {m} not found in provider {p}"));
            }
        }
        Ok(cfg)
    }

    /// Rust 会话宿主取启动参数（原 host.resolve-launch）：models.json 的 provider 块 + 要写进 0600 key 文件的明文 key。
    /// fail-closed：取不到 key 就报 [AUTH_INVALID] / [MODEL_NOT_CONFIGURED]，不带空 key 发请求。
    pub fn resolve_launch(&self, provider_id: Option<&str>, model_id: Option<&str>) -> Result<Value, String> {
        self.when_secrets_ready();
        let cfg = self.resolve_session_model(provider_id, model_id)?;
        if cfg["gateway"] == true {
            return self.gateway_launch(&cfg, cfg["modelId"].as_str().unwrap_or(""));
        }
        build_custom_launch(&cfg, |id| self.read_provider_api_key(id))
    }

    fn gateway_launch(&self, cfg: &Value, model: &str) -> Result<Value, String> {
        // Err(()) = 不是 xd 成员（用默认协议）；Ok(None) = 网关不认这个模型。
        let spec: Option<Value> = match self.access.resolve_pi_gateway_model_spec(model) {
            Err(()) => None,
            Ok(None) => Some(Value::Null),
            Ok(Some(s)) => Some(s),
        };
        let descriptor = self.access.resolve_xd_descriptor(model);
        build_gateway_launch(cfg, spec.as_ref(), descriptor.as_ref(), self.access.gateway_upstream())
    }
}

/// 网关启动参数（纯函数）。`spec`：None = 默认协议；Some(Null) = 网关不认这个模型；Some(obj) = 网关给出的协议。
pub fn build_gateway_launch(cfg: &Value, spec: Option<&Value>, descriptor: Option<&Value>, upstream: Option<(String, String)>) -> Result<Value, String> {
    let model = cfg["modelId"].as_str().unwrap_or("");
    let Some((key, endpoint)) = upstream.filter(|(k, e)| !k.is_empty() && !e.is_empty()) else { return Err("[AUTH_INVALID] Cindy AI is not signed in".into()) };
    if matches!(spec, Some(Value::Null)) || spec.is_some_and(|s| s.get("api") == Some(&Value::Null)) {
        return Err(format!("[MODEL_NOT_CONFIGURED] model {model} is not available on the Cindy gateway"));
    }
    {
        let api = spec.and_then(|s| s["api"].as_str()).unwrap_or("anthropic-messages").to_string();
        let trimmed = endpoint.trim_end_matches('/');
        // 与 maker-core piGatewayModelBaseUrl 同规则：anthropic 用根，openai 系用 /v1。
        let base_url = if api == "anthropic-messages" || trimmed.ends_with("/v1") { trimmed.to_string() } else { format!("{trimmed}/v1") };
        let d = descriptor.cloned().unwrap_or_else(|| json!({}));
        let mut m = Map::new();
        m.insert("id".into(), json!(model));
        m.insert("name".into(), d.get("displayName").filter(|v| !v.is_null()).cloned().unwrap_or(json!(model)));
        m.insert("api".into(), json!(api));
        m.insert("reasoning".into(), json!(match d["efforts"].as_array() {
            Some(e) => !e.is_empty(),
            None => cfg["reasoning"] == true,
        }));
        m.insert("input".into(), if d["supportsImageInput"] == false { json!(["text"]) } else { json!(["text", "image"]) });
        if truthy_num(&d["contextWindow"]) {
            m.insert("contextWindow".into(), d["contextWindow"].clone());
        }
        if truthy_num(&d["maxOutputTokens"]) {
            m.insert("maxTokens".into(), d["maxOutputTokens"].clone());
        }
        if let Some(c) = spec.and_then(|s| s.get("compat")).filter(|c| !c.is_null()) {
            m.insert("compat".into(), c.clone());
        }
        if let Some(t) = spec.and_then(|s| s.get("thinkingLevelMap")).filter(|c| !c.is_null()) {
            m.insert("thinkingLevelMap".into(), t.clone());
        }
        if let Some(c) = d.get("cost").filter(|c| !c.is_null()) {
            let f = |k: &str| c.get(k).filter(|v| !v.is_null()).cloned().unwrap_or(json!(0));
            m.insert("cost".into(), json!({ "input": f("input"), "output": f("output"), "cacheRead": f("cacheRead"), "cacheWrite": f("cacheWrite") }));
        }
        Ok(json!({
            "providerId": "cindy",
            "modelId": model,
            "keyKind": "gateway",
            "key": key,
            "provider": {
                "name": "Cindy AI",
                "baseUrl": base_url,
                "api": api,
                // 直连网关：key 走文件，同时写 authorization 与 x-api-key。
                "apiKeyHeaders": ["authorization", "x-api-key"],
                "models": [Value::Object(m)],
            },
        }))
    }
}

/// 自定义 provider 的启动参数（纯函数）：带 key 的从 `read_key` 取，缺失 fail-closed。
pub fn build_custom_launch(cfg: &Value, read_key: impl Fn(&str) -> Option<String>) -> Result<Value, String> {
    let model = cfg["modelId"].as_str().unwrap_or("");
    let pid = cfg["providerId"].as_str().unwrap_or("");
    let uses_key = cfg["authMethod"] == "apiKey";
    let key = if uses_key { read_key(pid) } else { None };
    if uses_key && key.is_none() {
        return Err(format!("[AUTH_INVALID] API key for provider {pid} is missing"));
    }
    let mut m = Map::new();
    m.insert("id".into(), json!(model));
    m.insert("name".into(), json!(model));
    m.insert("reasoning".into(), json!(cfg["reasoning"] == true));
    m.insert("input".into(), json!(["text"]));
    if truthy_num(&cfg["contextLimit"]) {
        m.insert("contextWindow".into(), cfg["contextLimit"].clone());
    }
    let name = cfg["providerName"].as_str().filter(|s| !s.is_empty()).unwrap_or(pid);
    let mut provider = Map::new();
    provider.insert("name".into(), json!(name));
    provider.insert("baseUrl".into(), cfg["baseUrl"].clone());
    provider.insert("api".into(), json!(cfg["api"].as_str().filter(|s| !s.is_empty()).unwrap_or("openai-completions")));
    if key.is_none() {
        provider.insert("apiKey".into(), json!(KEYLESS_PLACEHOLDER));
    }
    provider.insert("models".into(), json!([Value::Object(m)]));
    Ok(json!({
        "providerId": pi_runtime_provider_id(pid),
        "modelId": model,
        "keyKind": if key.is_some() { "provider" } else { "none" },
        "key": key,
        "provider": Value::Object(provider),
    }))

}
