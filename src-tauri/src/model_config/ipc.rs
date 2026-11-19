//! maker:provider:* / maker:model-visibility:* / ghosts:models:list 通道与 `/models` 探测（原 model-config.mjs 的 IPC 部分）。
use super::config::*;
use super::manager::Manager;
use crate::auth::hosted::Cancel;
use crate::local_db::mapper::js_trim;
use crate::model_access::manager::XD_PROVIDER_ID;
use serde_json::{json, Map, Value};
use std::time::{Duration, Instant};

const FETCH_TIMEOUT: Duration = Duration::from_secs(10);
const BUILTIN_REFRESHABLE: [&str; 4] = ["xd", "anthropic", "openai", "xai"];
const AUTO_REFRESH_TRIGGERS: [&str; 2] = ["providers-open", "model-selector-open"];
const CHANNELS: [&str; 15] = [
    "maker:provider:list", "maker:provider:presets", "maker:provider:custom:create", "maker:provider:custom:update", "maker:provider:custom:delete",
    "maker:provider:custom:disconnect", "maker:provider:test-connection", "maker:provider:models-fetch", "maker:provider:models-refresh",
    "maker:provider:models-auto-refresh", "maker:provider:order:set", "maker:provider:local-cli-scan", "maker:provider:detect", "maker:model-visibility:sync",
    "ghosts:models:list",
];
const SYNC_CHANNEL: &str = "maker:model-visibility:legacy-owner-claim-sync";

type R = Result<Value, String>;

pub fn classify_status(status: u16) -> &'static str {
    match status {
        401 => "AUTH_INVALID",
        403 => "AUTH_FORBIDDEN",
        404 => "ENDPOINT_NOT_FOUND",
        402 => "QUOTA_EXCEEDED",
        429 | 529 => "RATE_LIMITED",
        s if s >= 500 => "UPSTREAM_ERROR",
        _ => "UNKNOWN",
    }
}

pub struct Probe {
    pub ok: bool,
    pub cancelled: bool,
    pub code: &'static str,
    pub status: Option<u16>,
    pub latency_ms: u64,
    pub models: Option<Vec<Value>>,
}

/// GET <baseUrl>/models（或显式 modelsUrl），10 秒超时，不跟随重定向；有 key 时带 Authorization: Bearer。
pub async fn request_models(base_url: &str, models_url: Option<&str>, cancel: Option<&Cancel>, api_key: Option<&str>) -> Probe {
    let target = models_url.map(String::from).unwrap_or_else(|| format!("{base_url}/models"));
    let started = Instant::now();
    let done = |mut p: Probe| {
        p.latency_ms = started.elapsed().as_millis() as u64;
        p
    };
    let blank = |code: &'static str| Probe { ok: false, cancelled: false, code, status: None, latency_ms: 0, models: None };
    // reqwest 用 rustls-no-provider：先装 ring（已装则跳过）。
    if rustls::crypto::CryptoProvider::get_default().is_none() {
        let _ = rustls::crypto::ring::default_provider().install_default();
    }
    let Ok(client) = reqwest::Client::builder().redirect(reqwest::redirect::Policy::none()).build() else { return done(blank("UPSTREAM_UNREACHABLE")) };
    let mut req = client.get(&target).header("accept", "application/json");
    if let Some(k) = api_key {
        req = req.header("authorization", format!("Bearer {k}"));
    }
    let work = async {
        let res = req.send().await?;
        let status = res.status();
        let text = res.text().await?;
        Ok::<_, reqwest::Error>((status, text))
    };
    let never = async {
        match cancel {
            Some(c) => c.cancelled().await,
            None => std::future::pending::<()>().await,
        }
    };
    tokio::select! {
        _ = never => done(Probe { cancelled: true, ..blank("") }),
        r = tokio::time::timeout(FETCH_TIMEOUT, work) => match r {
            Err(_) => done(blank("TIMEOUT")),
            Ok(Err(_)) => done(blank("UPSTREAM_UNREACHABLE")),
            Ok(Ok((status, text))) => {
                if status.is_redirection() {
                    return done(blank("UPSTREAM_UNREACHABLE")); // redirect: 'error'
                }
                if !status.is_success() {
                    return done(Probe { status: Some(status.as_u16()), ..blank(classify_status(status.as_u16())) });
                }
                let json: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
                let models = super::discovery::parse_models_list_response(&json, Some(&target));
                done(Probe { ok: true, status: Some(status.as_u16()), models, ..blank("") })
            }
        },
    }
}

fn bad(msg: &str) -> String {
    ipc_error("INVALID_PARAMS", msg)
}
fn is_obj(v: &Value) -> bool {
    v.is_object()
}
fn s(v: &Value) -> Option<&str> {
    v.as_str()
}

impl Manager {
    pub fn owns(channel: &str) -> bool {
        CHANNELS.contains(&channel)
    }
    pub fn owns_sync(channel: &str) -> bool {
        channel == SYNC_CHANNEL
    }

    pub fn invoke_sync(&self, channel: &str) -> Option<Value> {
        (channel == SYNC_CHANNEL).then(|| {
            let stamp = self.owner.stamp();
            let writable = !stamp["dataOwnerId"].is_null();
            let mut o = stamp.as_object().cloned().unwrap_or_default();
            o.insert("canWriteOwnerScoped".into(), json!(writable));
            o.insert("claimed".into(), json!(false));
            o.insert("claimedByOtherOwner".into(), json!(false));
            o.insert("canInitialize".into(), json!(false));
            o.insert("profileOrigin".into(), json!("new"));
            Value::Object(o)
        })
    }

    pub async fn invoke(&self, channel: &str, args: &[Value]) -> Option<R> {
        let a = |i: usize| args.get(i).unwrap_or(&Value::Null).clone();
        Some(match channel {
            "maker:provider:list" => Ok(self.list_providers()),
            "maker:provider:presets" => Ok(json!({ "presets": [] })),
            "maker:provider:custom:create" => self.create_provider(&a(0), &a(1), &a(2)).await,
            "maker:provider:custom:update" => self.update_provider(&a(0), &a(1), &a(2)).await,
            "maker:provider:custom:delete" => self.delete_provider(&a(0), &a(1), &a(2)).await,
            "maker:provider:custom:disconnect" => self.disconnect_provider(&a(0), &a(1), &a(2)).await,
            "maker:provider:test-connection" => self.test_connection(&a(0)).await,
            "maker:provider:models-fetch" => self.fetch_models(&a(0)).await,
            "maker:provider:models-refresh" => match s(&a(0)).filter(|p| BUILTIN_REFRESHABLE.contains(p)) {
                None => Err(bad(&format!("providerId must be one of: {}", BUILTIN_REFRESHABLE.join(", ")))),
                Some(p) => {
                    if p == XD_PROVIDER_ID {
                        self.access.refresh_xd_gateway_models().await;
                    }
                    Ok(json!({ "ok": true, "providerId": p }))
                }
            },
            "maker:provider:models-auto-refresh" => match s(&a(0)).filter(|t| AUTO_REFRESH_TRIGGERS.contains(t)) {
                None => Err(bad(&format!("trigger must be one of: {}", AUTO_REFRESH_TRIGGERS.join(", ")))),
                Some(_) => {
                    self.access.foreground_refresh_xd_models();
                    Ok(json!({ "ok": true }))
                }
            },
            "maker:provider:order:set" => self.set_provider_order(&a(0)).await,
            "maker:provider:local-cli-scan" => Ok(json!({ "detections": [] })),
            "maker:provider:detect" => Ok(json!({ "candidates": [], "reason": "insufficient_evidence" })),
            "maker:model-visibility:sync" => self.sync_visibility(&a(0), &a(1), &a(2), &a(3)),
            "ghosts:models:list" => Ok(json!([])),
            _ => return None,
        })
    }

    fn list_providers(&self) -> Value {
        let mut o = self.owner.stamp().as_object().cloned().unwrap_or_default();
        o.insert("providers".into(), json!(self.provider_views()));
        o.insert("providerOrder".into(), json!(self.provider_order_ids()));
        o.insert("modelVisibilityOverrides".into(), self.visibility.lock().unwrap().clone());
        Value::Object(o)
    }

    fn assert_owner_scope(&self, scope: &Value) -> Result<(), String> {
        if scope.is_null() {
            return Ok(());
        }
        if !is_obj(scope) {
            return Err(bad("Invalid owner scope"));
        }
        if !self.owner.is_current(&scope["dataOwnerId"], &scope["ownerGeneration"]) {
            return Err(ipc_error("INTERNAL", "active account changed during provider mutation"));
        }
        Ok(())
    }
    fn assert_options(o: &Value) -> Result<(), String> {
        if o.is_null() || is_obj(o) { Ok(()) } else { Err(bad("invalid custom provider options")) }
    }
    fn assert_mutable(id: &str) -> Result<(), String> {
        if id == SMOKE_PROVIDER_ID { Err(ipc_error("PERMISSION_DENIED", "the Smoke provider is managed by environment variables")) } else { Ok(()) }
    }
    fn reserved(input: &Value) -> Result<(), String> {
        match s(&input["id"]).filter(|id| RESERVED_IDS.contains(id)) {
            Some(id) if is_obj(input) => Err(bad(&format!("id '{id}' is reserved"))),
            _ => Ok(()),
        }
    }

    /// key 变更：Some(k) 写入，None 删除。返回回滚用的变更前值。
    fn apply_key_mutation(&self, provider_id: &str, replacement: Option<&str>) -> Result<Option<String>, String> {
        let name = pi_key_name(provider_id);
        let previous = self.secrets.get(&name);
        let r = match replacement {
            Some(k) => self.secrets.set(&name, k),
            None => self.secrets.remove(&name).map(|_| ()),
        };
        if let Err(e) = r {
            return Err(if e.starts_with("[CREDENTIAL_STORAGE_UNAVAILABLE]") { e } else { ipc_error("CREDENTIAL_STORAGE_FAILED", "saving the API key to secure storage failed") });
        }
        Ok(previous)
    }
    fn rollback_key(&self, provider_id: &str, previous: Option<String>) {
        let name = pi_key_name(provider_id);
        let r = match previous {
            None => self.secrets.remove(&name).map(|_| ()),
            Some(p) => self.secrets.set(&name, &p),
        };
        if let Err(e) = r {
            eprintln!("[model-config] API key rollback failed for {provider_id}: {e}");
        }
    }

    async fn create_provider(&self, input: &Value, keys: &Value, options: &Value) -> R {
        Self::assert_options(options)?;
        let pi_key = parse_pi_key(Some(keys))?;
        Self::reserved(input)?;
        let config = sanitize_config(input)?;
        let id = config["id"].as_str().unwrap().to_string();
        let _g = self.queue.lock().await;
        if self.store.lock().unwrap().providers.iter().any(|p| p["id"] == id.as_str()) {
            return Err(ipc_error("ALREADY_EXISTS", &format!("custom provider '{id}' already exists")));
        }
        let mutation = plan_key_mutation(&config, &pi_key, None);
        let rollback = match &mutation {
            None => None,
            Some(m) => Some(self.apply_key_mutation(&id, m.as_deref())?),
        };
        self.store.lock().unwrap().providers.push(config);
        if let Err(e) = self.persist() {
            self.store.lock().unwrap().providers.pop();
            if let Some(p) = rollback {
                self.rollback_key(&id, p);
            }
            return Err(e);
        }
        self.changed();
        Ok(json!({ "ok": true }))
    }

    /// 从 desktop 版导入一个 provider（import_sessions 用）：同 id 已存在不覆盖，返回 "exists"。
    pub async fn import_provider(&self, input: &Value, api_key: Option<&str>) -> Result<&'static str, String> {
        Self::reserved(input)?;
        let config = sanitize_config(input)?;
        let key = api_key.map(|k| (k, js_trim(k))).filter(|(k, t)| !t.is_empty() && super::metadata::utf16_len(k) <= MAX_KEY_CHARS).map(|(_, t)| t.to_string());
        let id = config["id"].as_str().unwrap().to_string();
        let _g = self.queue.lock().await;
        if self.store.lock().unwrap().providers.iter().any(|p| p["id"] == id.as_str()) {
            return Ok("exists");
        }
        let rollback = match key.as_deref().filter(|_| uses_api_key(&config)) {
            Some(k) => Some(self.apply_key_mutation(&id, Some(k))?),
            None => None,
        };
        self.store.lock().unwrap().providers.push(config);
        if let Err(e) = self.persist() {
            self.store.lock().unwrap().providers.pop();
            if let Some(p) = rollback {
                self.rollback_key(&id, p);
            }
            return Err(e);
        }
        self.changed();
        Ok("imported")
    }

    async fn update_provider(&self, input: &Value, keys: &Value, options: &Value) -> R {
        Self::assert_options(options)?;
        let pi_key = parse_pi_key(Some(keys))?;
        if is_obj(input) {
            Self::assert_mutable(input["id"].as_str().unwrap_or(""))?;
        }
        let config = sanitize_config(input)?;
        let id = config["id"].as_str().unwrap().to_string();
        let _g = self.queue.lock().await;
        let (index, previous) = {
            let st = self.store.lock().unwrap();
            match st.providers.iter().position(|p| p["id"] == id.as_str()) {
                Some(i) => (i, st.providers[i].clone()),
                None => return Err(ipc_error("NOT_FOUND", &format!("custom provider '{id}' not found"))),
            }
        };
        let rollback = match plan_key_mutation(&config, &pi_key, Some(&previous)) {
            None => None,
            Some(m) => Some(self.apply_key_mutation(&id, m.as_deref())?),
        };
        self.store.lock().unwrap().providers[index] = config;
        if let Err(e) = self.persist() {
            self.store.lock().unwrap().providers[index] = previous;
            if let Some(p) = rollback {
                self.rollback_key(&id, p);
            }
            return Err(e);
        }
        self.changed();
        Ok(json!({ "ok": true }))
    }

    async fn delete_provider(&self, provider_id: &Value, scope: &Value, options: &Value) -> R {
        self.assert_owner_scope(scope)?;
        Self::assert_options(options)?;
        let id = s(provider_id).filter(|i| !i.is_empty()).ok_or_else(|| bad("providerId required"))?;
        Self::assert_mutable(id)?;
        let _g = self.queue.lock().await;
        let removed = {
            let mut st = self.store.lock().unwrap();
            let before = st.providers.len();
            st.providers.retain(|p| p["id"] != id);
            st.provider_order.retain(|x| x != id);
            st.providers.len() != before
        };
        if removed {
            self.persist()?;
        }
        // 配置删了，key 也一并删。
        if valid_provider_id(id) && self.secrets.has(&pi_key_name(id)) {
            self.secrets.remove(&pi_key_name(id))?;
        }
        if removed {
            self.changed();
        }
        Ok(json!({ "ok": true }))
    }

    async fn disconnect_provider(&self, provider_id: &Value, scope: &Value, options: &Value) -> R {
        self.assert_owner_scope(scope)?;
        Self::assert_options(options)?;
        let id = s(provider_id).filter(|i| !i.is_empty()).ok_or_else(|| bad("providerId required"))?;
        Self::assert_mutable(id)?;
        let _g = self.queue.lock().await;
        let config = self.store.lock().unwrap().providers.iter().find(|p| p["id"] == id).cloned();
        if !config.as_ref().is_some_and(uses_api_key) {
            return Err(bad("API connection required"));
        }
        self.apply_key_mutation(id, None)?;
        self.changed();
        Ok(json!({ "ok": true }))
    }

    fn saved_pi_route(&self, provider_id: &str) -> Option<(String, Option<String>)> {
        let configs = self.all_configs();
        let rt = &configs.iter().find(|c| c["id"] == provider_id)?["runtimes"]["pi"];
        rt.is_object().then(|| (rt["baseUrl"].as_str().unwrap_or("").to_string(), rt["modelsUrl"].as_str().map(String::from)))
    }

    async fn test_connection(&self, input: &Value) -> R {
        let invalid = || bad("invalid test-connection input");
        if !is_obj(input) {
            return Err(invalid());
        }
        let request_id: Option<String> = match input.get("requestId") {
            None | Some(Value::Null) => None,
            Some(v) => Some(s(v).filter(|r| !r.is_empty() && r.len() <= 100 && r.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')).ok_or_else(invalid)?.to_string()),
        };
        if input["kind"] == "cancel" {
            if let Some(c) = request_id.as_ref().and_then(|r| self.tests.lock().unwrap().get(r).cloned()) {
                c.abort();
            }
            return Ok(json!({ "ok": false, "latencyMs": 0, "decision": "cancelled" }));
        }
        let (base_url, models_url, model_id, api_key): (String, Option<String>, Option<String>, Option<String>);
        if input["kind"] == "saved" {
            let pid = s(&input["providerId"]).ok_or_else(invalid)?;
            let agent = s(&input["agent"]).filter(|a| AGENTS.contains(a)).ok_or_else(invalid)?;
            let configs = self.all_configs();
            let config = configs.iter().find(|c| c["id"] == pid);
            let rt = config.map(|c| &c["runtimes"]["pi"]).filter(|r| agent == "pi" && r.is_object()).ok_or_else(|| bad("invalid saved provider connection"))?;
            // 已存 key 只发往已存端点。
            base_url = rt["baseUrl"].as_str().unwrap_or("").to_string();
            models_url = rt["modelsUrl"].as_str().map(String::from);
            model_id = rt["models"].as_array().and_then(|m| m.first()).and_then(|m| m["id"].as_str()).map(String::from);
            api_key = if uses_api_key(config.unwrap()) {
                match self.read_provider_api_key(pid) {
                    Some(k) => Some(k),
                    None => return Ok(json!({ "ok": false, "code": "AUTH_INVALID", "latencyMs": 0, "decision": "blocked" })),
                }
            } else {
                None
            };
        } else if input["kind"] == "adhoc" {
            let spec = &input["spec"];
            if !is_obj(spec) || !s(&spec["agent"]).is_some_and(|a| AGENTS.contains(&a)) {
                return Err(invalid());
            }
            if spec["authMethod"] == "oauth" || spec["headers"].as_object().is_some_and(|h| !h.is_empty()) {
                return Err(ipc_error("NOT_IMPLEMENTED", NOT_IMPLEMENTED_SECRET));
            }
            if !spec["apiKey"].is_null() && !s(&spec["apiKey"]).is_some_and(|k| super::metadata::utf16_len(k) <= MAX_KEY_CHARS) {
                return Err(invalid());
            }
            // 表单里刚填的 key，只用于这一次探测，不落盘。
            api_key = if spec["authMethod"] == "none" { None } else { s(&spec["apiKey"]).map(js_trim).filter(|k| !k.is_empty()).map(String::from) };
            if !spec["wireProtocol"].is_null() && spec["wireProtocol"] != "openai-chat" {
                return Err(ipc_error("NOT_IMPLEMENTED", "only OpenAI Chat Completions compatible endpoints are supported"));
            }
            base_url = s(&spec["baseUrl"]).and_then(normalize_base_url).ok_or_else(invalid)?;
            models_url = None;
            model_id = Some(s(&spec["modelId"]).filter(|m| !m.is_empty()).ok_or_else(invalid)?.to_string());
        } else {
            return Err(invalid());
        }
        let cancel = Cancel::new();
        if let Some(r) = &request_id {
            let mut t = self.tests.lock().unwrap();
            if t.contains_key(r) {
                return Err(ipc_error("PRECONDITION_FAILED", "connection test already running"));
            }
            t.insert(r.clone(), cancel.clone());
        }
        let r = request_models(&base_url, models_url.as_deref(), Some(&cancel), api_key.as_deref()).await;
        if let Some(id) = &request_id {
            self.tests.lock().unwrap().remove(id);
        }
        if r.cancelled {
            return Ok(json!({ "ok": false, "latencyMs": 0, "decision": "cancelled" }));
        }
        if !r.ok {
            let mut o = json!({ "ok": false, "code": r.code, "latencyMs": r.latency_ms, "decision": "blocked" });
            if let Some(st) = r.status {
                o["status"] = json!(st);
            }
            return Ok(o);
        }
        // 端点能列出模型但不含所选模型时判为 MODEL_NOT_FOUND；列表不可解析时只认端点可达。
        if let (Some(m), Some(list)) = (&model_id, &r.models) {
            if !list.is_empty() && !list.iter().any(|x| x["id"] == m.as_str()) {
                return Ok(json!({ "ok": false, "code": "MODEL_NOT_FOUND", "status": r.status, "latencyMs": r.latency_ms, "decision": "blocked" }));
            }
        }
        let mut o = json!({ "ok": true, "status": r.status, "latencyMs": r.latency_ms, "decision": "success" });
        if input["strategy"] == "auto" {
            o["selected"] = json!({ "baseUrl": base_url, "wireProtocol": "openai-chat" });
        }
        Ok(o)
    }

    async fn fetch_models(&self, input: &Value) -> R {
        let invalid = || bad("invalid models-fetch input");
        if !is_obj(input) || !s(&input["agent"]).is_some_and(|a| AGENTS.contains(&a)) {
            return Err(invalid());
        }
        let auth = s(&input["authMethod"]).filter(|a| ["apiKey", "oauth", "none"].contains(a)).ok_or_else(invalid)?;
        if auth == "oauth" || input["headers"].as_object().is_some_and(|h| !h.is_empty()) {
            return Err(ipc_error("NOT_IMPLEMENTED", NOT_IMPLEMENTED_SECRET));
        }
        if !input["apiKey"].is_null() && !s(&input["apiKey"]).is_some_and(|k| super::metadata::utf16_len(k) <= MAX_KEY_CHARS) {
            return Err(invalid());
        }
        let mut api_key: Option<String> = if auth == "apiKey" { s(&input["apiKey"]).map(js_trim).filter(|k| !k.is_empty()).map(String::from) } else { None };
        let mut base_url = s(&input["baseUrl"]).and_then(normalize_base_url);
        let mut models_url: Option<String> = None;
        if let Some(m) = s(&input["modelsUrl"]).filter(|m| !js_trim(m).is_empty()) {
            models_url = Some(normalize_base_url(m).ok_or_else(invalid)?);
        }
        let mut base = base_url.take().ok_or_else(invalid)?;
        // 已保存 provider：目标钉回已存端点；表单没带 key 时用已存 key。
        if let Some(saved_id) = s(&input["savedProviderId"]) {
            if let Some((b, m)) = self.saved_pi_route(saved_id) {
                base = b;
                models_url = m;
                if api_key.is_none() && auth == "apiKey" {
                    api_key = self.read_provider_api_key(saved_id);
                }
            }
        }
        let r = request_models(&base, models_url.as_deref(), None, api_key.as_deref()).await;
        if !r.ok {
            let mut o = json!({ "ok": false, "code": r.code });
            if let Some(st) = r.status {
                o["status"] = json!(st);
            }
            return Ok(o);
        }
        let Some(models) = r.models else { return Ok(json!({ "ok": false, "code": "WIRE_INCOMPATIBLE", "status": r.status })) };
        if models.is_empty() && input["allowEmpty"] != true {
            return Ok(json!({ "ok": false, "code": "MODEL_NOT_FOUND", "status": r.status }));
        }
        Ok(json!({ "ok": true, "models": models }))
    }

    async fn set_provider_order(&self, input: &Value) -> R {
        let invalid = || bad("invalid provider order input");
        let o = input.as_object().ok_or_else(invalid)?;
        let owner_ok = o.get("dataOwnerId").is_some_and(|d| d.is_null() || d.is_string());
        let gen_ok = o.get("ownerGeneration").is_some_and(super::metadata::is_nonneg_safe_int);
        let ids: Vec<String> = match o.get("providerIds").and_then(Value::as_array) {
            Some(a) if o.len() == 3 && owner_ok && gen_ok && !a.is_empty() && a.len() <= MAX_ORDER_ITEMS => {
                let mut v = vec![];
                for x in a {
                    match x.as_str().filter(|x| !x.is_empty() && super::metadata::utf16_len(x) <= 256) {
                        Some(x) => v.push(x.to_string()),
                        None => return Err(bad("providerIds must be a bounded unique non-empty string[]")),
                    }
                }
                v
            }
            _ => return Err(bad("providerIds must be a bounded unique non-empty string[]")),
        };
        if ids.iter().collect::<std::collections::HashSet<_>>().len() != ids.len() {
            return Err(bad("providerIds must be a bounded unique non-empty string[]"));
        }
        self.assert_owner_scope(&json!({ "dataOwnerId": o["dataOwnerId"], "ownerGeneration": o["ownerGeneration"] }))?;
        let catalog: std::collections::HashSet<String> = self.provider_order_ids().into_iter().collect();
        if ids.iter().any(|i| !catalog.contains(i)) {
            return Err(bad("providerIds must belong to the current provider catalog"));
        }
        let custom: Vec<String> = ids.into_iter().filter(|i| i != XD_PROVIDER_ID).collect();
        let differs = self.store.lock().unwrap().provider_order != custom;
        if differs {
            self.store.lock().unwrap().provider_order = custom;
            self.persist()?;
            self.changed();
        }
        Ok(json!({ "ok": true }))
    }

    /// renderer 单向镜像的模型可见性快照；owner 不匹配的迟到快照直接丢弃。
    fn sync_visibility(&self, owner: &Value, generation: &Value, map: &Value, policy: &Value) -> R {
        if !self.owner.is_current(owner, generation) {
            return Ok(Value::Null);
        }
        let m = map.as_object().ok_or_else(|| bad("invalid visibility map"))?;
        if policy["pending"] == true {
            return Ok(Value::Null);
        }
        let next: Map<String, Value> = m.iter().filter(|(_, v)| v.is_boolean()).map(|(k, v)| (k.clone(), v.clone())).collect();
        let next = Value::Object(next);
        let differs = *self.visibility.lock().unwrap() != next;
        if differs {
            *self.visibility.lock().unwrap() = next;
            self.changed();
        }
        Ok(Value::Null)
    }
}
