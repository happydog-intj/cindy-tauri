//! xd（Cindy AI 网关）模型目录投影：把校验后的网关模型清单投影成 Pi 可用的 xd provider 模型表。
//! 对应 sidecar model-access.mjs 的 projectPiModels / resolvePiGatewayModelSpec，以及它们用到的
//! model-providers 解析（resolveModelNativeApi、pi-gateway-model-catalog、effort 规则）和
//! desktop 的 effectiveGatewayModelCost。内置注册表与 Pi 目录行是编译期嵌入的紧凑表
//! （scripts/app-settings/xdcatalog-entry.ts 生成 xd_catalog.json；黄金样例 xd_golden.json）。

use serde_json::{json, Map, Value};
use std::collections::HashSet;
use std::sync::OnceLock;

const PI_APIS: [&str; 4] = ["anthropic-messages", "openai-responses", "openai-completions", "google-generative-ai"];
const EFFORT_ORDER: [&str; 7] = ["minimal", "low", "medium", "high", "xhigh", "max", "ultra"];
const GATEWAY_UNSUPPORTED_ANTHROPIC_COMPAT: [&str; 3] = ["supportsStrictTools", "supportsMidConvoToolChanges", "supportsMidConvoEffort"];
const DEEPSEEK_REASONING_REPLAY_MODELS: [&str; 5] = [
    "deepseek/deepseek-v4.1-flash", "deepseek/deepseek-flash", "deepseek/deepseek-v4-pro", "deepseek/deepseek-v4-flash-vision-exp", "deepseek/deepseek-v4-flash",
];

/// resolveModelNativeApi 等的三态结果：未知 / 明确不可用（null） / 具体协议。
#[derive(Clone, Debug, PartialEq)]
pub enum Api {
    Unknown,
    Null,
    Known(String),
}

impl Api {
    pub fn to_json_pub(&self) -> Value {
        self.to_json()
    }

    fn to_json(&self) -> Value {
        match self {
            Api::Unknown => json!({ "u": 1 }),
            Api::Null => Value::Null,
            Api::Known(s) => json!(s),
        }
    }
}

struct Catalog {
    raw: Value,
}

fn catalog() -> &'static Catalog {
    static C: OnceLock<Catalog> = OnceLock::new();
    C.get_or_init(|| Catalog { raw: serde_json::from_str(include_str!("xd_catalog.json")).expect("xd_catalog.json") })
}

fn normalize_model_id(id: &str) -> &str {
    id.strip_suffix("[1m]").unwrap_or(id)
}

fn str_of<'a>(v: &'a Value, k: &str) -> Option<&'a str> {
    v.get(k).and_then(Value::as_str)
}

/// 注册表里路由含 (xd, id) 的条目。
fn xd_entries(id: &str) -> Vec<&'static Value> {
    catalog().raw["entries"].as_array().unwrap().iter()
        .filter(|e| e["routes"].as_array().unwrap().iter().any(|r| r["providerId"] == "xd" && r["modelId"] == id))
        .collect()
}

fn entry_native_api(e: &Value) -> Api {
    match &e["nativeApi"] {
        Value::Null => Api::Null,
        Value::String(s) => Api::Known(s.clone()),
        _ => Api::Unknown,
    }
}

fn is_chat_mode(mode: Option<&str>) -> bool {
    matches!(mode, None | Some("chat") | Some("responses"))
}

/// resolveModelNativeApi(registry, 'xd', modelId)
pub fn resolve_model_native_api(model_id: &str) -> Api {
    let c = catalog();
    let id = normalize_model_id(model_id);
    let entries = xd_entries(id);
    if entries.iter().any(|e| e["status"] == "retired" || entry_native_api(e) == Api::Null) {
        return Api::Null;
    }
    if c.raw["schemaVersion"].as_f64().unwrap_or(0.0) < 3.0 {
        return Api::Unknown;
    }
    let mut explicit: Vec<String> = vec![];
    for e in &entries {
        if let Api::Known(s) = entry_native_api(e) {
            if !explicit.contains(&s) {
                explicit.push(s);
            }
        }
    }
    if !explicit.is_empty() {
        return if explicit.len() == 1 { Api::Known(explicit.remove(0)) } else { Api::Null };
    }
    // 有效 mode：条目默认 mode 里第一个有值的，否则基础模型的 mode。
    let from_entries = entries.iter().find_map(|e| str_of(e, "mode"));
    let effective = from_entries.or_else(|| find_base_mode(id));
    let rules = c.raw["rules"].as_array().unwrap();
    let mut best: Option<(&str, &str)> = None;
    for r in rules {
        let prefix = str_of(r, "modelIdPrefix").unwrap();
        let api = str_of(r, "nativeApi").unwrap();
        if !id.starts_with(prefix) || id[prefix.len()..].contains('/') {
            continue;
        }
        let mode_ok = is_chat_mode(effective) || effective.is_none() || !PI_APIS.contains(&api);
        if !mode_ok {
            continue;
        }
        // 前缀更长的优先；等长保持声明顺序（JS 稳定排序）。
        if best.map_or(true, |(p, _)| prefix.len() > p.len()) {
            best = Some((prefix, api));
        }
    }
    match best {
        Some((_, api)) => Api::Known(api.to_string()),
        None => Api::Unknown,
    }
}

fn find_base_mode(identity: &str) -> Option<&'static str> {
    let matches: Vec<&Value> = catalog().raw["bases"].as_array().unwrap().iter()
        .filter(|b| b["id"] == identity || b["aliases"].as_array().unwrap().iter().any(|a| a == identity))
        .collect();
    if matches.len() == 1 { str_of(matches[0], "mode") } else { None }
}

fn is_retired(id: &str) -> bool {
    xd_entries(normalize_model_id(id)).iter().any(|e| e["status"] == "retired")
}

fn piapi_from_wire(w: Option<&str>) -> Option<&'static str> {
    match w? {
        "google-generative-ai" => Some("google-generative-ai"),
        "anthropic-messages" => Some("anthropic-messages"),
        "openai-responses" => Some("openai-responses"),
        "openai-chat" => Some("openai-completions"),
        _ => None,
    }
}

/// routedCatalogApis：非 xd 路由在 provider / preset 的 Pi 模型声明里给出的协议。
fn routed_catalog_apis(provider_id: &str, model_id: &str) -> Vec<&'static str> {
    if provider_id == "xd" {
        return vec![];
    }
    let c = &catalog().raw;
    let mut out = vec![];
    for table in ["providers", "presets"] {
        let Some(p) = c[table].get(provider_id) else { continue };
        let default_wire = str_of(p, "wire");
        if let Some(m) = p["models"].as_array().unwrap().iter().find(|m| m["id"] == model_id) {
            let api = str_of(m, "piApi").and_then(|a| PI_APIS.iter().find(|x| **x == a).copied().or(Some("")))
                .filter(|a| !a.is_empty())
                .or_else(|| piapi_from_wire(str_of(m, "wire").or(default_wire)));
            if let Some(a) = api {
                out.push(a);
            }
        }
    }
    out
}

/// resolveCatalogPiGatewayModelApi：None = 未声明，Some(None) = 冲突/退役（必须失败关闭）。
fn resolve_catalog_pi_gateway_model_api(id: &str) -> Option<Option<&'static str>> {
    let normalized = normalize_model_id(id);
    let entries = xd_entries(normalized);
    if entries.iter().any(|e| e["status"] == "retired") {
        return Some(None);
    }
    let mut apis: Vec<&str> = vec![];
    for e in &entries {
        for r in e["routes"].as_array().unwrap() {
            for a in routed_catalog_apis(str_of(r, "providerId").unwrap(), str_of(r, "modelId").unwrap()) {
                if !apis.contains(&a) {
                    apis.push(a);
                }
            }
        }
    }
    match apis.len() {
        0 => None,
        1 => Some(Some(apis[0])),
        _ => Some(None),
    }
}

fn catalog_identity_overrides(id: &str) -> Option<(String, String)> {
    let p = |a: &str, b: &str| Some((a.to_string(), b.to_string()));
    const CLAUDE: [&str; 9] = [
        "claude-fable-5", "claude-haiku-4-5", "claude-haiku-4-5-20251001", "claude-opus-4-6", "claude-opus-4-7", "claude-opus-4-8", "claude-opus-5",
        "claude-sonnet-4-6", "claude-sonnet-5",
    ];
    const GPT: [&str; 8] = ["gpt-5.4", "gpt-5.4-mini", "gpt-5.4-nano", "gpt-5.5", "gpt-5.6-luna", "gpt-5.6-sol", "gpt-5.6-terra", "gpt-6-astra"];
    if CLAUDE.contains(&id) {
        return p("anthropic", id);
    }
    if id == "anthropic/claude-opus-5" {
        return p("anthropic", "claude-opus-5");
    }
    for g in GPT {
        if id == g || id == format!("openai-codex/{g}") || id == format!("codex/{g}") {
            return p("openai", g);
        }
    }
    match id {
        "openai-codex/gpt-5.5:auto" | "codex/gpt-5.5:auto" => return p("openai", "gpt-5.5"),
        "deepseek/deepseek-flash" | "deepseek/deepseek-v4.1-flash" | "deepseek/deepseek-v4-flash" | "deepseek/deepseek-v4-flash-vision-exp" => {
            return p("deepseek", "deepseek-flash")
        }
        "deepseek/deepseek-v4-pro" => return p("deepseek", "deepseek-v4-pro"),
        "moonshot/kimi-k3" | "moonshotai/kimi-k3" => return p("moonshotai", "kimi-k3"),
        "moonshotai/kimi-k2.6" => return p("moonshotai", "kimi-k2.6"),
        _ => {}
    }
    if let Some(m) = id.strip_prefix("qwen/") {
        if ["qwen3.7-max", "qwen3.8-27b", "qwen3.8-flash", "qwen3.8-max"].contains(&m) {
            return p("qwen-token-plan-cn", m);
        }
    }
    if let Some(m) = id.strip_prefix("z-ai/") {
        if ["glm-5.1", "glm-5.2", "glm-5.3", "glm-5.3-flash", "glm-5.3-highspeed"].contains(&m) {
            return p("zai", m);
        }
    }
    for prefix in ["x-ai/", "x-ai-grok/"] {
        if let Some(m) = id.strip_prefix(prefix) {
            if ["grok-4.5", "grok-4.6"].contains(&m) {
                return p("xai", m);
            }
        }
    }
    None
}

fn pi_row(provider: &str, id: &str) -> Option<&'static Value> {
    catalog().raw["piRows"].as_array().unwrap().iter().find(|r| r["provider"] == provider && r["id"] == id)
}

fn bundled_identity(model_id: &str) -> Option<(String, String)> {
    let normalized = normalize_model_id(model_id);
    if resolve_model_native_api(normalized) == Api::Known("google-generative-ai".into()) {
        return Some(("google".into(), normalized.strip_prefix("google/").unwrap_or(normalized).to_string()));
    }
    if let Some(e) = catalog_identity_overrides(normalized) {
        return Some(e);
    }
    catalog().raw["piRows"].as_array().unwrap().iter()
        .find(|r| format!("{}/{}", r["provider"].as_str().unwrap(), r["id"].as_str().unwrap()) == normalized)
        .map(|r| (r["provider"].as_str().unwrap().to_string(), r["id"].as_str().unwrap().to_string()))
}

/// resolveBundledPiGatewayModelProfile：{api, compat?, samplingParams?, thinkingLevelMap?}
pub fn bundled_profile(model_id: &str) -> Option<Value> {
    let matched = bundled_identity(model_id).and_then(|(p, id)| pi_row(&p, &id));
    let canonical = resolve_model_native_api(model_id);
    let api = match &canonical {
        Api::Unknown => matched.and_then(|m| str_of(m, "api")).map(String::from),
        Api::Null => None,
        Api::Known(s) => Some(s.clone()),
    }?;
    if !PI_APIS.contains(&api.as_str()) {
        return None;
    }
    let compatible = matched.filter(|m| str_of(m, "api") == Some(api.as_str()));
    let mut out = Map::new();
    out.insert("api".into(), json!(api));
    if let Some(m) = compatible {
        for k in ["compat", "samplingParams", "thinkingLevelMap"] {
            if let Some(v) = m.get(k) {
                out.insert(k.into(), v.clone());
            }
        }
    }
    Some(Value::Object(out))
}

fn known_pi_api(s: &str) -> Option<&'static str> {
    PI_APIS.iter().find(|a| **a == s).copied()
}

fn hint_api(gm: &Value) -> Option<&'static str> {
    known_pi_api(gm["perAgent"]["pi"]["wireProtocol"].as_str()?)
}

/// 网关模型对应的 Pi 协议；None = 不进清单（失败关闭）。
fn resolve_pi_api(gm: &Value) -> Option<&'static str> {
    let id = gm["id"].as_str()?;
    if is_retired(id) {
        return None;
    }
    let native = match resolve_model_native_api(id) {
        Api::Known(s) => known_pi_api(&s).map(Some).unwrap_or(None).map(Api2::Known).unwrap_or(Api2::Rejected),
        Api::Null => Api2::Null,
        Api::Unknown => Api2::Unknown,
    };
    if let Api2::Known(a) = native {
        return Some(a);
    }
    if let Some(declared) = resolve_catalog_pi_gateway_model_api(id) {
        return declared.and_then(known_pi_api);
    }
    if catalog().raw["schemaVersion"].as_f64().unwrap_or(0.0) >= 3.0 || matches!(native, Api2::Null) {
        return hint_api(gm);
    }
    if let Some(local) = bundled_profile(id) {
        return local["api"].as_str().and_then(known_pi_api);
    }
    hint_api(gm)
}

enum Api2 {
    Known(&'static str),
    Null,
    Unknown,
    /// 注册表给了协议但不是 Pi 支持的（上游 nativeApiForRoute 返回 undefined）。
    Rejected,
}

fn nonneg_finite(v: Option<&Value>) -> Option<f64> {
    v.and_then(Value::as_f64).filter(|n| n.is_finite() && *n >= 0.0)
}

/// effectiveGatewayModelCost：每百万 token 的成本；缺 input/output 返回 None。
pub fn effective_gateway_model_cost(gm: &Value) -> Option<Value> {
    let input = nonneg_finite(gm.get("inputCostPerToken"))?;
    let output = nonneg_finite(gm.get("outputCostPerToken"))?;
    let discount = gm.get("costDiscount").and_then(Value::as_f64).filter(|d| *d > 0.0 && *d <= 1.0).unwrap_or(0.0);
    let m = 1.0 - discount;
    let mut o = Map::new();
    o.insert("input".into(), crate::auth::flow::js_number(input * 1_000_000.0 * m));
    o.insert("output".into(), crate::auth::flow::js_number(output * 1_000_000.0 * m));
    if let Some(c) = nonneg_finite(gm.get("cacheReadInputTokenCost")) {
        o.insert("cacheRead".into(), crate::auth::flow::js_number(c * 1_000_000.0 * m));
    }
    if let Some(c) = nonneg_finite(gm.get("cacheCreationInputTokenCost")) {
        o.insert("cacheWrite".into(), crate::auth::flow::js_number(c * 1_000_000.0 * m));
    }
    Some(Value::Object(o))
}

fn effort_rank(e: &str) -> usize {
    EFFORT_ORDER.iter().position(|x| *x == e).unwrap_or(EFFORT_ORDER.len())
}

pub fn default_effort_for_capabilities(efforts: &[String]) -> Option<String> {
    for preferred in ["medium", "high", "low", "xhigh", "max", "minimal", "ultra"] {
        if let Some(e) = efforts.iter().find(|e| *e == preferred) {
            return Some(e.clone());
        }
    }
    efforts.first().cloned()
}

/// clampEffortToSupported：不在支持列表里就取「rank ≤ 请求档」的最高档，没有则取最低档。
pub fn clamp_effort_to_supported(effort: &str, efforts: &[String]) -> String {
    if effort.is_empty() || efforts.is_empty() || efforts.iter().any(|e| e == effort) {
        return effort.to_string();
    }
    let want = effort_rank(effort);
    let mut best: Option<&String> = None;
    let mut best_rank: i64 = -1;
    for e in efforts {
        let r = effort_rank(e) as i64;
        if r <= want as i64 && r > best_rank {
            best = Some(e);
            best_rank = r;
        }
    }
    if let Some(b) = best {
        return b.clone();
    }
    let mut lowest = &efforts[0];
    for e in efforts {
        if effort_rank(e) < effort_rank(lowest) {
            lowest = e;
        }
    }
    lowest.clone()
}

fn string_list(v: Option<&Value>) -> Vec<String> {
    v.and_then(Value::as_array).map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect()).unwrap_or_default()
}

/// 保持 JS `a ?? b` 的语义：只有 undefined/null 才回落。
fn first<'a>(a: Option<&'a Value>, b: Option<&'a Value>) -> Option<&'a Value> {
    match a {
        Some(v) if !v.is_null() => Some(v),
        _ => b.filter(|v| !v.is_null()),
    }
}

pub fn project_pi_models(models: &[Value]) -> Vec<Value> {
    let mut out: Vec<Value> = vec![];
    for gm in models {
        if !gm["agents"].as_array().is_some_and(|a| a.iter().any(|x| x == "pi")) {
            continue;
        }
        let Some(pi_api) = resolve_pi_api(gm) else { continue };
        let ov = &gm["perAgent"]["pi"];
        let efforts_v = first(ov.get("efforts"), gm.get("efforts"));
        let efforts = string_list(efforts_v);
        let intent = ov.get("defaultEffort").filter(|v| !v.is_undefined_like()).or_else(|| gm.get("defaultEffort"));
        let default_effort: Value = if efforts.is_empty() {
            Value::Null
        } else {
            let want = match intent {
                Some(Value::String(s)) => Some(s.clone()),
                Some(Value::Null) => None,
                _ => default_effort_for_capabilities(&efforts),
            };
            match want {
                Some(w) => json!(clamp_effort_to_supported(&w, &efforts)),
                None => Value::Null,
            }
        };
        let native_api = match resolve_model_native_api(gm["id"].as_str().unwrap_or("")) {
            Api::Known(s) => known_pi_api(&s).map(|s| json!(s)),
            Api::Null => Some(Value::Null),
            Api::Unknown => None,
        };
        let cost = effective_gateway_model_cost(gm);
        let default_enabled = first(ov.get("defaultEnabled"), gm.get("defaultEnabled"));
        let fast = first(ov.get("supportsFastMode"), gm.get("supportsFastMode"));
        let mut m = Map::new();
        if let (Some(dp), Some(cur)) = (gm.get("displayPricing"), gm.get("currency")) {
            if !dp.is_null() && !cur.is_null() {
                m.insert("displayPricing".into(), dp.clone());
                m.insert("currency".into(), cur.clone());
                if let Some(d) = gm.get("costDiscount") {
                    m.insert("costDiscount".into(), d.clone());
                }
            }
        }
        m.insert("id".into(), gm["id"].clone());
        if let Some(n) = native_api {
            m.insert("nativeApi".into(), n);
        }
        for (k, key) in [("availability", "availability")] {
            if let Some(v) = gm.get(key).filter(|v| v.as_str().is_some_and(|s| !s.is_empty())) {
                m.insert(k.into(), v.clone());
            }
        }
        m.insert("name".into(), gm["name"].clone());
        if let Some(v) = gm.get("group") {
            m.insert("group".into(), v.clone());
        }
        let cw = first(ov.get("contextWindow"), gm.get("contextWindow")).cloned().unwrap_or(Value::Null);
        if !cw.is_null() {
            m.insert("contextWindow".into(), cw);
        }
        if let Some(v) = gm.get("contextWindow") {
            m.insert("contextWindowMax".into(), v.clone());
        }
        if let Some(v) = gm.get("maxOutputTokens") {
            m.insert("maxOutput".into(), v.clone());
        }
        m.insert("contextWindowVerified".into(), json!(true));
        m.insert("efforts".into(), json!(efforts));
        m.insert("defaultEffort".into(), default_effort);
        if let Some(v) = fast {
            m.insert("supportsFastMode".into(), v.clone());
        }
        for k in ["mode", "description", "sortOrder"] {
            if let Some(v) = gm.get(k) {
                m.insert(k.into(), v.clone());
            }
        }
        if let Some(v) = default_enabled {
            m.insert("defaultEnabled".into(), v.clone());
        }
        for k in ["newSessionDefault", "icon"] {
            if let Some(v) = gm.get(k) {
                m.insert(k.into(), v.clone());
            }
        }
        if let Some(c) = cost {
            m.insert("cost".into(), c);
        }
        if let Some(v) = gm.get("modalities") {
            m.insert("modalities".into(), v.clone());
        }
        m.insert("piApi".into(), json!(pi_api));
        out.push(Value::Object(m));
    }
    // selectDefaultModels：需付费或非对话模型默认收起。
    let visible: HashSet<&str> = models.iter()
        .filter(|m| m["availability"] != "requires_payment" && is_chat_mode(m["mode"].as_str()))
        .filter_map(|m| m["id"].as_str()).collect();
    let mut indexed: Vec<(usize, Value)> = out.into_iter().enumerate().map(|(i, mut m)| {
        let chat = is_chat_mode(m["mode"].as_str());
        let id = m["id"].as_str().unwrap_or("").to_string();
        if chat && m.get("defaultEnabled") != Some(&json!(false)) && !visible.contains(id.as_str()) {
            m["defaultEnabled"] = json!(false);
        }
        (i, m)
    }).collect();
    indexed.sort_by(|a, b| {
        let key = |m: &Value| m["sortOrder"].as_f64().unwrap_or(f64::MAX);
        key(&a.1).partial_cmp(&key(&b.1)).unwrap_or(std::cmp::Ordering::Equal).then(a.0.cmp(&b.0))
    });
    indexed.into_iter().map(|(_, m)| m).collect()
}

trait UndefinedLike {
    fn is_undefined_like(&self) -> bool;
}

impl UndefinedLike for Value {
    /// JSON 没有 undefined：缺字段才算；显式 null 是「有意置空」，要保留。
    fn is_undefined_like(&self) -> bool {
        false
    }
}

/// resolvePiGatewayModelSpec：Pi 网关块用的协议与兼容性配置。
/// `Err(())` = 上游返回 undefined（不是 xd 成员）；`Ok(None)` = 返回 null（失败关闭）。
pub fn resolve_pi_gateway_model_spec(gateway_models: &[Value], model_id: &str) -> Result<Option<Value>, ()> {
    let id = normalize_model_id(model_id);
    let gm = gateway_models.iter().find(|m| m["id"] == id).filter(|m| m["agents"].as_array().is_some_and(|a| a.iter().any(|x| x == "pi"))).ok_or(())?;
    let Some(api) = resolve_pi_api(gm) else { return Ok(None) };
    let bundled = bundled_profile(id);
    let profile = bundled.filter(|b| b["api"] == api);
    let mut compat: Option<Map<String, Value>> = profile.as_ref().and_then(|p| p["compat"].as_object().cloned());
    if api == "openai-completions" && DEEPSEEK_REASONING_REPLAY_MODELS.contains(&id)
        && compat.as_ref().map_or(true, |c| !c.contains_key("requiresReasoningContentOnAssistantMessages"))
    {
        compat.get_or_insert_with(Map::new).insert("requiresReasoningContentOnAssistantMessages".into(), json!(true));
    }
    if api == "anthropic-messages" {
        if let Some(c) = compat.as_mut() {
            if GATEWAY_UNSUPPORTED_ANTHROPIC_COMPAT.iter().any(|k| c.contains_key(*k)) {
                c.retain(|k, _| !GATEWAY_UNSUPPORTED_ANTHROPIC_COMPAT.contains(&k.as_str()));
            }
        }
    }
    if api == "openai-completions" || api == "anthropic-messages" {
        compat.get_or_insert_with(Map::new).insert("sendSessionAffinityHeaders".into(), json!(true));
    }
    let mut out = Map::new();
    out.insert("api".into(), json!(api));
    if let Some(c) = compat {
        out.insert("compat".into(), Value::Object(c));
    }
    if let Some(p) = &profile {
        for k in ["samplingParams", "thinkingLevelMap"] {
            if let Some(v) = p.get(k).filter(|v| !v.is_null()) {
                out.insert(k.into(), v.clone());
            }
        }
    }
    Ok(Some(Value::Object(out)))
}

/// xd provider 的目录形状（Provider）；只投 Pi。
pub fn xd_provider(projected: &[Value]) -> Value {
    let mut p = catalog().raw["xdProvider"].clone();
    p["models"] = json!({ "pi": projected });
    p
}

impl Catalog {
    #[allow(dead_code)]
    fn len(&self) -> usize {
        self.raw["entries"].as_array().map_or(0, Vec::len)
    }
}
