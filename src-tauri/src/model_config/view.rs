//! provider 视图：移植 user-provider.ts 的 `buildUserProvider`（受限形态）与 registry.ts 的 `buildRegistry`。
//! 受限形态 = 配置经 `sanitizeConfig` 收敛过：只有 pi runtime、wireProtocol 为 openai-chat、无请求头 / requestPath /
//! OAuth / 预设绑定 / 注册表选项；模型只带 id、name、reasoning:false 和可选的 contextWindow / defaultEnabled / supportsImageInput。
//! 受限之外的分支（预设、OAuth、selectedInterface、发现元数据）上游有，这里不存在，也不会被调用。
use super::metadata::{self, default_effort_for_capabilities, generation_capabilities, merge, pick, pick_meta, previous_generations, Meta};
use super::registry::{registry, Generated};
use crate::model_access::xd_catalog::Api;
use serde_json::{json, Map, Value};
use std::sync::OnceLock;

pub const DEFAULT_CUSTOM_CONTEXT_WINDOW: u64 = 500_000;
const PI_MODEL_APIS: [&str; 8] = [
    "anthropic-messages", "openai-responses", "openai-completions", "google-generative-ai", "bedrock-converse-stream", "azure-openai-responses",
    "google-vertex", "mistral-conversations",
];

fn st<'a>(v: &'a Value, k: &str) -> Option<&'a str> {
    v.get(k).and_then(Value::as_str)
}

/// isLoopbackProviderUrl：只认真正的 loopback 主机。
pub fn is_loopback_provider_url(value: &str) -> bool {
    if value.is_empty() {
        return false;
    }
    let Ok(u) = url::Url::parse(value) else { return false };
    if !matches!(u.scheme(), "http" | "https") || !u.username().is_empty() || u.password().is_some() {
        return false;
    }
    let host = u.host_str().unwrap_or("").to_lowercase();
    if host == "localhost" || host == "[::1]" || host == "::1" {
        return true;
    }
    let octets: Vec<&str> = host.split('.').collect();
    octets.len() == 4 && octets[0] == "127" && octets.iter().all(|o| !o.is_empty() && o.len() <= 3 && o.bytes().all(|b| b.is_ascii_digit()) && o.parse::<u32>().unwrap_or(999) <= 255)
}

pub fn is_mimo_token_plan_url(value: &str) -> bool {
    let Ok(u) = url::Url::parse(value) else { return false };
    u.scheme() == "https" && u.port().is_none() && matches!(u.host_str(), Some("token-plan-cn.xiaomimimo.com" | "token-plan-ams.xiaomimimo.com" | "token-plan-sgp.xiaomimimo.com"))
}

pub fn provider_media_field(mode: Option<&str>) -> Option<&'static str> {
    match mode? {
        "image_generation" => Some("imageModels"),
        "video_generation" => Some("videoModels"),
        "audio_speech" | "audio_transcription" | "audio_generation" | "realtime" => Some("audioModels"),
        "embedding" => Some("embeddingModels"),
        _ => None,
    }
}

fn compat_protocol(api: &str) -> Option<&'static str> {
    match api {
        "openai-responses" | "azure-openai-responses" => Some("openai-responses"),
        "openai-chat" | "openai-completions" => Some("openai-chat"),
        "anthropic" | "anthropic-messages" => Some("anthropic-messages"),
        "google" | "google-generative-ai" | "google-vertex" => Some("google-generative-ai"),
        _ => None,
    }
}

/// providerBaseUrlForApi：只有 Google 官方公开端点有 /openai 路径关系，其余原样。
fn base_url_for_api(base: &str, api: &str) -> String {
    let Ok(mut u) = url::Url::parse(base) else { return base.to_string() };
    if u.origin().ascii_serialization() != "https://generativelanguage.googleapis.com" {
        return base.to_string();
    }
    let path = u.path().trim_end_matches('/').to_string();
    let ok = ["/v1", "/v1/openai", "/v1beta", "/v1beta/openai"].contains(&path.as_str());
    if !ok {
        return base.to_string();
    }
    let stripped = path.strip_suffix("/openai").unwrap_or(&path).to_string();
    match api {
        "google-generative-ai" => u.set_path(&stripped),
        "openai-completions" => u.set_path(&format!("{stripped}/openai")),
        _ => return base.to_string(),
    }
    let s = u.to_string();
    s.strip_suffix('/').unwrap_or(&s).to_string()
}

fn api_json(a: &Api) -> Option<Value> {
    match a {
        Api::Known(s) => Some(json!(s)),
        Api::Null => Some(Value::Null),
        Api::Unknown => None,
    }
}

/// 公开基础模型的（id, 协议, 默认元数据）候选；只算一次。
fn public_candidates() -> &'static Vec<(String, Api, Meta)> {
    static C: OnceLock<Vec<(String, Api, Meta)>> = OnceLock::new();
    C.get_or_init(|| {
        let r = registry();
        let mut out = vec![];
        for b in &r.bases {
            let protocol = r.resolve_catalog_model_native_api(&b.id, None);
            for id in std::iter::once(&b.id).chain(b.aliases.iter()) {
                out.push((id.clone(), protocol.clone(), b.defaults.clone()));
            }
        }
        out
    })
}

/// runtimeUserModelMetadata：用户显式配置的字段（名称、窗口、图片输入、推理档位）。
pub fn runtime_user_model_metadata(m: &Value) -> Meta {
    let mut o = pick(Some(m));
    o.shift_remove("name");
    if m.get("discoveredMetadata").is_none() || m["nameExplicit"].as_bool().unwrap_or(false) {
        if let Some(n) = m.get("name") {
            o.insert("name".into(), n.clone());
        }
    }
    for k in ["contextWindow", "supportsImageInput"] {
        if let Some(v) = m.get(k) {
            o.insert(k.into(), v.clone());
        }
    }
    if m["reasoning"] == json!(false) {
        o.insert("efforts".into(), json!([]));
    } else if let Some(v) = m.get("reasoningEfforts") {
        o.insert("efforts".into(), v.clone());
    }
    if let Some(v) = m.get("reasoningDefaultEffort") {
        o.insert("defaultEffort".into(), v.clone());
    }
    pick_meta(&o)
}

/// resolveModelMetadata（无注册表）：家族继承 < 预设 / 目录默认 < 用户配置，再收敛窗口与默认档位。
fn resolve_model_metadata(user: &Meta, provider_defaults: Option<&Meta>, generation_defaults: &Meta) -> (Meta, bool) {
    let defaults = merge(&[provider_defaults]);
    let default_effort_layer: Option<Meta> = defaults.get("defaultEffort").map(|v| Meta::from_iter([("defaultEffort".to_string(), v.clone())]));
    let mut result = merge(&[Some(generation_defaults), Some(&defaults), default_effort_layer.as_ref(), Some(user)]);
    let has_own_window = defaults.contains_key("contextWindow") || user.contains_key("contextWindow");
    let mut inherited = false;
    if !result.contains_key("contextWindow") && result.contains_key("contextWindowMax") {
        result.insert("contextWindow".into(), result["contextWindowMax"].clone());
        inherited = true;
    }
    if let (Some(w), Some(mx)) = (result.get("contextWindow").and_then(Value::as_f64), result.get("contextWindowMax").and_then(Value::as_f64)) {
        if mx < w {
            result.shift_remove("contextWindowMax");
        }
    }
    if result.contains_key("contextWindow") && !has_own_window {
        inherited = true;
    }
    let efforts: Option<Vec<String>> = result.get("efforts").and_then(Value::as_array).map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect());
    match efforts {
        Some(e) if e.is_empty() => {
            result.insert("defaultEffort".into(), Value::Null);
        }
        Some(e) => {
            if let Some(d) = result.get("defaultEffort").and_then(Value::as_str).map(String::from) {
                if !e.contains(&d) {
                    let idx = |s: &str| metadata::EFFORT_ORDER.iter().position(|x| *x == s).map_or(-1, |i| i as i64);
                    let mut supported = e.clone();
                    supported.sort_by_key(|s| idx(s));
                    let want = idx(&d);
                    let pick = supported.iter().filter(|v| idx(v) <= want).last().or_else(|| supported.first());
                    result.insert("defaultEffort".into(), pick.map_or(Value::Null, |p| json!(p)));
                }
            }
        }
        None => {}
    }
    (result, inherited)
}

/// applyModelMetadata：把解析出的元数据盖到目录模型上。
fn apply_model_metadata(mut model: Map<String, Value>, meta: &Meta, inherited: bool) -> Map<String, Value> {
    let mut fields = meta.clone();
    let max_output = fields.shift_remove("maxOutputTokens");
    for (k, v) in fields {
        model.insert(k, v);
    }
    if meta.contains_key("contextWindow") {
        model.insert("contextWindowVerified".into(), json!(!inherited));
    }
    if let Some(v) = max_output {
        model.insert("maxOutput".into(), v);
    }
    if let (Some(mx), Some(w)) = (model.get("contextWindowMax").and_then(Value::as_f64), model.get("contextWindow").and_then(Value::as_f64)) {
        if mx < w {
            model.shift_remove("contextWindowMax");
        }
    }
    let efforts = model.get("efforts").and_then(Value::as_array).cloned().unwrap_or_default();
    let bad_default = model.get("defaultEffort").is_some_and(|d| !d.is_null() && !efforts.contains(d));
    if efforts.is_empty() || bad_default {
        model.insert("defaultEffort".into(), Value::Null);
    }
    model
}

/// toCatalogModel（pi、无注册表、无发现元数据）。
fn to_catalog_model(m: &Value, provider_id: &str, provider_defaults: Option<&Meta>, generation_defaults: &Meta) -> Map<String, Value> {
    let reasoning = m["reasoning"] == json!(true);
    let efforts: Vec<String> = if reasoning {
        m["reasoningEfforts"].as_array().map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect()).unwrap_or_default()
    } else {
        vec![]
    };
    let default_effort = match st(m, "reasoningDefaultEffort") {
        Some(d) if reasoning && efforts.iter().any(|e| e == d) => json!(d),
        _ => default_effort_for_capabilities(&efforts),
    };
    let mut model = Map::new();
    model.insert("userModelConfig".into(), m.clone());
    model.insert("id".into(), m["id"].clone());
    if let Some(n) = m.get("nameExplicit") {
        model.insert("nameExplicit".into(), n.clone());
    }
    model.insert("name".into(), m["name"].clone());
    let has_window = m.get("contextWindow").is_some();
    model.insert("contextWindow".into(), m.get("contextWindow").cloned().unwrap_or(json!(DEFAULT_CUSTOM_CONTEXT_WINDOW)));
    if has_window {
        model.insert("contextWindowVerified".into(), json!(true));
        model.insert("contextWindowExplicit".into(), json!(true));
    }
    model.insert("efforts".into(), json!(efforts));
    model.insert("defaultEffort".into(), default_effort);
    model.insert("group".into(), json!(format!("custom:{provider_id}")));
    model.insert("defaultEnabled".into(), m.get("defaultEnabled").filter(|v| !v.is_null()).cloned().unwrap_or(json!(true)));
    if m["supportsImageInput"] == json!(true) {
        model.insert("supportsImageInput".into(), json!(true));
    }
    if m["thinkingToggle"] == json!(true) {
        model.insert("thinkingToggle".into(), json!(true));
    }
    let user = runtime_user_model_metadata(m);
    let (resolved, inherited) = if provider_defaults.is_some() || !generation_defaults.is_empty() {
        resolve_model_metadata(&user, provider_defaults, generation_defaults)
    } else {
        (pick_meta(&user), false)
    };
    apply_model_metadata(model, &resolved, inherited)
}

/// 命中的目录记录：精确路由身份，或沿用旧代参数生成的记录。
enum Imported {
    Exact(usize),
    Generated(Generated),
}

impl Imported {
    fn row(&self) -> usize {
        match self {
            Imported::Exact(i) => *i,
            Imported::Generated(g) => g.row,
        }
    }
    fn inherited_from(&self) -> Option<&str> {
        match self {
            Imported::Generated(g) => Some(&g.inherited_from),
            _ => None,
        }
    }
    fn metadata(&self) -> Meta {
        let r = registry();
        match self {
            Imported::Exact(i) => r.row_metadata(*i, None, None),
            Imported::Generated(g) => r.row_metadata(g.row, Some(&g.id), None),
        }
    }
}

fn model_str<'a>(m: &'a Value, k: &str) -> Option<&'a str> {
    m.get(k).and_then(Value::as_str)
}

/// 单个存储模型 → 目录模型（`buildUserProvider` 里 `rt.models.map(...)` 的受限形态）。
fn project_model(config_id: &str, base_url: &str, stored: &Value) -> Value {
    let reg = registry();
    let wire = "openai-chat";
    let id = model_str(stored, "id").unwrap_or("");
    // imported：精确路由身份，其次沿用旧代参数。
    let imported: Option<Imported> = reg
        .provider_model_record(id, base_url, Some(wire), true)
        .map(Imported::Exact)
        .or_else(|| reg.provider_model_generation_record(id, base_url, Some(wire)).map(Imported::Generated));
    let has_base = reg.find_base_model(id).is_some();
    let keep_existing_api = imported.as_ref().and_then(Imported::inherited_from).is_some() && has_base;
    let imported_api: Option<String> = imported.as_ref().filter(|_| !keep_existing_api).and_then(|i| {
        let api = &reg.rows[i.row()].api;
        PI_MODEL_APIS.contains(&api.as_str()).then(|| api.clone())
    });

    // 代际默认：公开基础模型（同协议）+ 同连接里更老的存储模型 + 命中记录的继承来源。
    let protocol = Api::Known("openai-completions".into());
    let mut candidates: Vec<(String, Meta)> = public_candidates().iter().filter(|(_, p, _)| *p == protocol).map(|(i, _, d)| (i.clone(), d.clone())).collect();
    if let Some(from) = imported.as_ref().and_then(Imported::inherited_from) {
        candidates.push((from.to_string(), imported.as_ref().unwrap().metadata()));
    }
    // rt.models 里更老的代际（这里就是 config 自己的模型清单，调用方传进来的 stored 所在清单）。
    let siblings = SIBLINGS.with(|s| s.borrow().clone());
    for cand in previous_generations(id, &siblings, |m| model_str(m, "id").unwrap_or("").to_string()) {
        let cid = model_str(&cand, "id").unwrap_or("");
        let row = reg.provider_model_record(cid, base_url, Some("openai-completions"), false).map(|i| reg.row_metadata(i, None, None));
        let base_defaults = reg.find_base_model(cid).map(|b| &b.defaults);
        let user = runtime_user_model_metadata(&cand);
        candidates.push((cid.to_string(), merge(&[base_defaults, row.as_ref(), Some(&user)])));
    }
    let generation_defaults = {
        let layers: Vec<Meta> = previous_generations(id, &candidates, |c| c.0.clone()).into_iter().map(|c| generation_capabilities(&c.1)).collect();
        merge(&layers.iter().map(Some).collect::<Vec<_>>())
    };

    // 精确的厂商记录搬到中转站上仍是这个模型自己的声明；只有严格更老的 id 才是代际回退。
    let exact_relay_defaults: Option<Meta> = match &imported {
        Some(i) if i.inherited_from() == Some(id) && !has_base && !reg.entries.iter().any(|e| e.id == id || e.routes.iter().any(|(_, m)| m == id)) => Some(merge(&[Some(&i.metadata())])),
        _ => None,
    };
    let defaults: Option<Meta> = imported.as_ref().map(|i| {
        let first = if i.inherited_from().is_none() { Some(i.metadata()) } else { exact_relay_defaults.clone() };
        merge(&[first.as_ref()])
    });

    let projected = to_catalog_model(stored, config_id, defaults.as_ref(), &generation_defaults);
    let proj_mode = projected.get("mode").and_then(Value::as_str);
    // 打包注册表里的厂商声明：路由声明优先，其次按目录模型身份。
    let declaration = {
        let base = reg.find_base_model(id);
        match reg.resolve_model_native_api(config_id, id, proj_mode) {
            Api::Unknown => reg.resolve_catalog_model_native_api(base.map_or(id, |b| b.id.as_str()), proj_mode),
            known => known,
        }
    };
    let native_api = if provider_media_field(proj_mode).is_some() && stored.get("nativeApi").is_some() { stored.get("nativeApi").cloned() } else { api_json(&declaration) };

    let mut out = Map::new();
    if let Some(i) = &imported {
        if let Imported::Exact(row) = i {
            if let Some(c) = reg.rows[*row].raw.get("cost").filter(|c| c.is_object()) {
                out.insert("cost".into(), c.clone());
            }
        }
    }
    for (k, v) in projected {
        out.insert(k, v);
    }
    if let Some(a) = native_api {
        out.insert("nativeApi".into(), a);
    }
    out.insert("userModelConfig".into(), stored.clone());
    if let Some(api) = &imported_api {
        out.insert("api".into(), json!(api));
        out.insert("piApi".into(), json!(api));
        let wp = compat_protocol(api);
        if let Some(wp) = wp.filter(|w| *w != wire) {
            out.insert("route".into(), json!({ "baseUrl": base_url_for_api(base_url, api), "wireProtocol": wp }));
        }
    }
    Value::Object(out)
}

thread_local! {
    static SIBLINGS: std::cell::RefCell<Vec<Value>> = const { std::cell::RefCell::new(Vec::new()) };
}

/// buildUserProvider（受限形态）：只有 pi runtime 的自定义 provider。
pub fn build_user_provider(config: &Value) -> Value {
    let id = st(config, "id").unwrap_or("");
    let runtime_id = if id == "xai" { "custom:xai" } else { id };
    let no_auth = config["auth"]["method"] == "none";
    let strategy = if no_auth { "none" } else { "api-key-header" };
    let rt = &config["runtimes"]["pi"];
    let mut agents: Vec<&str> = vec![];
    let mut routing = Map::new();
    let mut models = Map::new();
    if rt.is_object() {
        agents.push("pi");
        let base_url = st(rt, "baseUrl").unwrap_or("");
        let models_url = st(rt, "modelsUrl");
        let mut r = Map::new();
        r.insert("upstream".into(), json!(base_url));
        r.insert("authStrategy".into(), json!(strategy));
        if strategy == "none" && (!is_loopback_provider_url(base_url) || models_url.is_some_and(|u| !is_loopback_provider_url(u))) {
            r.insert("disabled".into(), json!(true));
        }
        r.insert("wireProtocol".into(), json!("openai-chat"));
        if let Some(u) = models_url.filter(|u| !u.is_empty()) {
            r.insert("modelsUrl".into(), json!(u));
        }
        routing.insert("pi".into(), Value::Object(r));
        let stored: Vec<Value> = rt["models"].as_array().cloned().unwrap_or_default();
        SIBLINGS.with(|s| *s.borrow_mut() = stored.clone());
        let mut list: Vec<Value> = stored.iter().map(|m| project_model(id, base_url, m)).collect();
        // 手填模型缺省视为默认可见：显式配置里没写 defaultEnabled 的，按“引擎能直连”判定（pi 的出站协议总是已知）。
        for (m, s) in list.iter_mut().zip(&stored) {
            if provider_media_field(model_str(m, "mode")).is_none() && s.get("defaultEnabled").map_or(true, Value::is_null) {
                m["defaultEnabled"] = json!(true);
            }
        }
        SIBLINGS.with(|s| s.borrow_mut().clear());
        models.insert("pi".into(), Value::Array(list));
    }
    let mimo = !routing.is_empty()
        && routing.values().all(|r| r["upstream"].as_str().is_some_and(is_mimo_token_plan_url))
        && models.values().all(|l| l.as_array().is_some_and(|l| l.iter().all(|m| m.get("route").map_or(true, |r| r["baseUrl"].as_str().is_some_and(is_mimo_token_plan_url)))));
    json!({
        "id": runtime_id,
        "name": config["name"],
        "source": "user",
        "agents": agents,
        "auth": { "method": if no_auth { "none" } else { "apiKey" } },
        "access": if !no_auth && mimo { json!({ "kind": "subscription", "product": "MiMo Token Plan" }) } else { json!({ "kind": "api" }) },
        "routing": routing,
        "models": models,
    })
}

/// buildRegistry（无停用 override、无发现失败）：给每个 provider 补 connected。
pub fn build_registry(providers: &[Value], connected: &std::collections::HashMap<String, bool>) -> Vec<Value> {
    providers
        .iter()
        .map(|p| {
            let mut v = p.clone();
            v["connected"] = json!(connected.get(st(p, "id").unwrap_or("")).copied().unwrap_or(false));
            v
        })
        .collect()
}
