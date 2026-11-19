//! `/api/model-access/models`（schemaVersion 5）响应校验：移植 desktop `parseModelsSyncPayload`，
//! 即 model-providers 的 `parseListModelsResponse` v5 分支（`modelEntryError`、`sanitizeModelEntryAgents`）
//! 与 `parseModelDisplayPricing`。校验失败给出与上游相同的错误文案（只进日志）。
//! 黄金样例：scripts/app-settings/models-sync-entry.ts → models_sync_golden.json。

use crate::auth::flow::js_trim;
use serde_json::{Map, Value};

type Obj = Map<String, Value>;
type Res<T> = Result<T, String>;

const AGENTS: [&str; 3] = ["claude-code", "codex", "pi"];
const WIRE_PROTOCOLS: [&str; 4] = ["anthropic-messages", "openai-responses", "openai-completions", "google-generative-ai"];
const EFFORTS: [&str; 7] = ["minimal", "low", "medium", "high", "xhigh", "max", "ultra"];
const STANDALONE_MODES: [&str; 9] = [
    "image_generation", "video_generation", "audio_generation", "audio_speech", "audio_transcription", "realtime", "embedding", "evaluation", "rerank",
];
const PRICING_FIELDS: [&str; 35] = [
    "costDiscount", "inputCostPerToken", "outputCostPerToken", "inputCostPerTokenPriority", "outputCostPerTokenPriority",
    "cacheReadInputTokenCost", "cacheReadInputTokenCostPriority", "cacheCreationInputTokenCost",
    "inputCostPerTokenAbove200kTokens", "outputCostPerTokenAbove200kTokens", "cacheReadInputTokenCostAbove200kTokens",
    "inputCostPerTokenAbove200kTokensPriority", "outputCostPerTokenAbove200kTokensPriority", "cacheReadInputTokenCostAbove200kTokensPriority",
    "inputCostPerTokenAbove272kTokens", "outputCostPerTokenAbove272kTokens", "cacheReadInputTokenCostAbove272kTokens",
    "inputCostPerTokenAbove272kTokensPriority", "outputCostPerTokenAbove272kTokensPriority", "cacheReadInputTokenCostAbove272kTokensPriority",
    "inputCostPerCharacter", "outputCostPerCharacter", "inputCostPerSecond", "outputCostPerSecond", "inputCostPerAudioToken",
    "outputCostPerAudioToken", "inputCostPerAudioPerSecond", "outputCostPerAudioPerSecond", "inputCostPerImage", "outputCostPerImage",
    "inputCostPerImageToken", "outputCostPerImageToken", "cacheReadInputImageTokenCost", "inputCostPerVideoPerSecond", "outputCostPerVideoPerSecond",
];
const ENTRY_FIELDS: [&str; 17] = [
    "id", "mode", "currency", "agents", "name", "group", "description", "icon", "contextWindow", "maxOutputTokens", "modalities", "efforts",
    "defaultEffort", "sortOrder", "supportsFastMode", "defaultEnabled", "perAgent",
];
const OVERRIDE_FIELDS: [&str; 6] = ["contextWindow", "efforts", "defaultEffort", "supportsFastMode", "defaultEnabled", "wireProtocol"];
const TIER_FIELDS: [&str; 5] = ["range", "inputCostPerToken", "outputCostPerToken", "cacheReadInputTokenCost", "cacheCreationInputTokenCost"];

pub struct ModelsSync {
    /// 已校验、清洗后的模型条目（含 availability 与可选的 displayPricing）。
    pub models: Vec<Value>,
    /// free | paid | not_applicable
    pub account_tier: String,
}

fn utf16_len(s: &str) -> usize {
    s.encode_utf16().count()
}

fn is_agent(v: &Value) -> bool {
    v.as_str().is_some_and(|s| AGENTS.contains(&s))
}

fn is_effort(v: &Value) -> bool {
    v.as_str().is_some_and(|s| EFFORTS.contains(&s))
}

fn is_wire_protocol(v: &Value) -> bool {
    v.as_str().is_some_and(|s| WIRE_PROTOCOLS.contains(&s))
}

fn finite(v: &Value) -> Option<f64> {
    v.as_f64().filter(|n| n.is_finite())
}

fn unknown_field(o: &Obj, allowed: &[&str], path: &str) -> Option<String> {
    o.keys().find(|k| !allowed.contains(&k.as_str())).map(|k| format!("{path}.{k} is not allowed by this schema version"))
}

fn optional_string(v: Option<&Value>, path: &str, max: usize) -> Option<String> {
    match v {
        None => None,
        Some(Value::String(s)) => (utf16_len(s) > max).then(|| format!("{path} must contain at most {max} characters")),
        Some(_) => Some(format!("{path} must be a string when present")),
    }
}

fn optional_positive_integer(v: Option<&Value>, path: &str) -> Option<String> {
    let v = v?;
    let ok = v.as_f64().is_some_and(|n| n.is_finite() && n.fract() == 0.0 && n > 0.0);
    (!ok).then(|| format!("{path} must be a positive integer when present"))
}

fn optional_finite(v: Option<&Value>, path: &str, non_negative: bool) -> Option<String> {
    let v = v?;
    let Some(n) = finite(v) else { return Some(format!("{path} must be a finite number when present")) };
    (non_negative && n < 0.0).then(|| format!("{path} must be non-negative when present"))
}

fn effort_list(v: Option<&Value>, path: &str) -> Option<String> {
    let v = v?;
    match v.as_array() {
        Some(a) if a.iter().all(is_effort) => None,
        _ => Some(format!("{path} must contain only supported effort values")),
    }
}

fn modalities(v: Option<&Value>, path: &str) -> Option<String> {
    let v = v?;
    let Some(o) = v.as_object() else { return Some(format!("{path} must be an object when present")) };
    if let Some(e) = unknown_field(o, &["input", "output"], path) {
        return Some(e);
    }
    for d in ["input", "output"] {
        let ok = o.get(d).and_then(Value::as_array).is_some_and(|a| a.iter().all(Value::is_string));
        if !ok {
            return Some(format!("{path}.{d} must be an array of strings"));
        }
    }
    None
}

fn valid_efforts(v: Option<&Value>) -> Option<Vec<&str>> {
    let a = v?.as_array()?;
    a.iter().map(|e| e.as_str().filter(|s| EFFORTS.contains(s))).collect()
}

fn override_error(v: &Value, path: &str, base_efforts: Option<&Vec<&str>>, base_default: Option<&str>) -> Option<String> {
    let Some(o) = v.as_object() else { return Some(format!("{path} must be an object")) };
    if let Some(e) = unknown_field(o, &OVERRIDE_FIELDS, path) {
        return Some(e);
    }
    if let Some(e) = optional_positive_integer(o.get("contextWindow"), &format!("{path}.contextWindow")) {
        return Some(e);
    }
    if let Some(e) = effort_list(o.get("efforts"), &format!("{path}.efforts")) {
        return Some(e);
    }
    let default_effort = o.get("defaultEffort");
    if matches!(default_effort, Some(v) if !v.is_null() && !is_effort(v)) {
        return Some(format!("{path}.defaultEffort must be a supported effort value when present"));
    }
    let own = valid_efforts(o.get("efforts"));
    let effective: Option<&Vec<&str>> = own.as_ref().or(base_efforts);
    if let (Some(d), Some(eff)) = (default_effort.and_then(Value::as_str).filter(|_| default_effort.is_some_and(is_effort)), effective) {
        if !eff.contains(&d) {
            return Some(format!("{path}.defaultEffort must be included in {path}.efforts or the base efforts"));
        }
    }
    if default_effort.is_none() {
        if let (Some(b), Some(eff)) = (base_default, effective) {
            if !eff.contains(&b) {
                return Some(format!("{path}.efforts must include the inherited base defaultEffort"));
            }
        }
    }
    for k in ["supportsFastMode", "defaultEnabled"] {
        if matches!(o.get(k), Some(v) if !v.is_boolean()) {
            return Some(format!("{path}.{k} must be a boolean when present"));
        }
    }
    None
}

fn tiered_pricing(v: Option<&Value>, path: &str) -> Option<String> {
    let v = v?;
    let Some(a) = v.as_array() else { return Some(format!("{path} must be an array when present")) };
    for (i, tier) in a.iter().enumerate() {
        let tp = format!("{path}[{i}]");
        let Some(t) = tier.as_object() else { return Some(format!("{tp} must be an object")) };
        if let Some(e) = unknown_field(t, &TIER_FIELDS, &tp) {
            return Some(e);
        }
        let range_ok = t.get("range").and_then(Value::as_array).is_some_and(|r| {
            r.len() == 2 && {
                let (a, b) = (finite(&r[0]), finite(&r[1]));
                matches!((a, b), (Some(a), Some(b)) if a >= 0.0 && b >= a)
            }
        });
        if !range_ok {
            return Some(format!("{tp}.range must be an ascending pair of non-negative finite numbers"));
        }
        for f in ["inputCostPerToken", "outputCostPerToken", "cacheReadInputTokenCost", "cacheCreationInputTokenCost"] {
            if let Some(e) = optional_finite(t.get(f), &format!("{tp}.{f}"), true) {
                return Some(e);
            }
        }
    }
    None
}

fn new_session_default(v: Option<&Value>, path: &str, supported: &[&str]) -> Option<String> {
    let v = v?;
    let a = v.as_array().filter(|a| !a.is_empty());
    let Some(a) = a else { return Some(format!("{path} must be a non-empty array when present")) };
    let mut seen: Vec<&str> = vec![];
    for agent in a {
        if !is_agent(agent) {
            return Some(format!("{path} must contain only supported agents"));
        }
        let s = agent.as_str().unwrap();
        if seen.contains(&s) {
            return Some(format!("{path} must not contain duplicates"));
        }
        seen.push(s);
        if !supported.contains(&s) {
            return Some(format!("{path} agents must be supported by the model entry"));
        }
    }
    None
}

fn model_entry_error(value: &Value, path: &str) -> Option<String> {
    let Some(o) = value.as_object() else { return Some(format!("{path} must be an object")) };
    let mut allowed: Vec<&str> = ENTRY_FIELDS.to_vec();
    allowed.extend(PRICING_FIELDS);
    allowed.extend(["tieredPricing", "newSessionDefault"]);
    if let Some(e) = unknown_field(o, &allowed, path) {
        return Some(e);
    }
    if !o.get("id").and_then(Value::as_str).is_some_and(|s| !s.is_empty() && utf16_len(s) <= 256) {
        return Some(format!("{path}.id must be a non-empty string of at most 256 characters"));
    }
    if let Some(e) = optional_string(o.get("mode"), &format!("{path}.mode"), 128) {
        return Some(e);
    }
    if matches!(o.get("currency"), Some(c) if !matches!(c.as_str(), Some("CNY" | "USD"))) {
        return Some(format!("{path}.currency must be CNY or USD when present"));
    }
    let Some(agents) = o.get("agents").and_then(Value::as_array) else {
        return Some(format!("{path}.agents must be an array in schema version 3 or 4"));
    };
    if !agents.iter().all(is_agent) {
        return Some(format!("{path}.agents must be an array of supported agents when present"));
    }
    let supported: Vec<&str> = agents.iter().filter_map(Value::as_str).collect();
    if let Some(e) = new_session_default(o.get("newSessionDefault"), &format!("{path}.newSessionDefault"), &supported) {
        return Some(e);
    }
    let standalone = o.get("mode").and_then(Value::as_str).is_some_and(|m| STANDALONE_MODES.contains(&m));
    if standalone && !supported.is_empty() {
        return Some(format!("{path}.agents must be empty for a v4 Gateway standalone capability mode"));
    }
    if supported.is_empty() && !standalone {
        return Some(format!("{path}.agents may be empty only for a v4 Gateway standalone capability mode"));
    }
    for (k, max) in [("name", 256), ("group", 128), ("description", 2_000)] {
        if let Some(e) = optional_string(o.get(k), &format!("{path}.{k}"), max) {
            return Some(e);
        }
    }
    if !o.get("name").and_then(Value::as_str).is_some_and(|s| !js_trim(s).is_empty()) {
        return Some(format!("{path}.name must be a non-empty string in schema version 3 or 4"));
    }
    if matches!(o.get("icon"), Some(v) if !v.as_str().is_some_and(|s| !js_trim(s).is_empty())) {
        return Some(format!("{path}.icon must be a non-empty string when present"));
    }
    for k in ["contextWindow", "maxOutputTokens"] {
        if let Some(e) = optional_positive_integer(o.get(k), &format!("{path}.{k}")) {
            return Some(e);
        }
    }
    if !standalone && o.get("contextWindow").is_none() {
        return Some(format!("{path}.contextWindow is required for schema version 3 and v4 chat models"));
    }
    if let Some(e) = modalities(o.get("modalities"), &format!("{path}.modalities")) {
        return Some(e);
    }
    if let Some(e) = effort_list(o.get("efforts"), &format!("{path}.efforts")) {
        return Some(e);
    }
    let default_effort = o.get("defaultEffort");
    if matches!(default_effort, Some(v) if !v.is_null() && !is_effort(v)) {
        return Some(format!("{path}.defaultEffort must be a supported effort value when present"));
    }
    let efforts = valid_efforts(o.get("efforts"));
    let default_str = default_effort.filter(|v| is_effort(v)).and_then(Value::as_str);
    if let (Some(d), Some(eff)) = (default_str, &efforts) {
        if !eff.contains(&d) {
            return Some(format!("{path}.defaultEffort must be included in {path}.efforts"));
        }
    }
    if let Some(e) = optional_finite(o.get("sortOrder"), &format!("{path}.sortOrder"), false) {
        return Some(e);
    }
    for k in ["supportsFastMode", "defaultEnabled"] {
        if matches!(o.get(k), Some(v) if !v.is_boolean()) {
            return Some(format!("{path}.{k} must be a boolean when present"));
        }
    }
    for f in PRICING_FIELDS {
        if let Some(e) = optional_finite(o.get(f), &format!("{path}.{f}"), f != "costDiscount") {
            return Some(e);
        }
    }
    if let Some(e) = tiered_pricing(o.get("tieredPricing"), &format!("{path}.tieredPricing")) {
        return Some(e);
    }
    if let Some(pa) = o.get("perAgent") {
        let Some(pa) = pa.as_object() else { return Some(format!("{path}.perAgent must be an object when present")) };
        for (agent, ov) in pa {
            if !AGENTS.contains(&agent.as_str()) {
                return Some(format!("{path}.perAgent.{agent} is not a supported agent"));
            }
            if !supported.contains(&agent.as_str()) {
                return Some(format!("{path}.perAgent.{agent} must be included in {path}.agents"));
            }
            if let Some(e) = override_error(ov, &format!("{path}.perAgent.{agent}"), efforts.as_ref(), default_str) {
                return Some(e);
            }
            let wire = ov.as_object().and_then(|x| x.get("wireProtocol"));
            if agent == "pi" {
                if matches!(wire, Some(w) if !w.as_str().is_some_and(|s| !js_trim(s).is_empty())) {
                    return Some(format!("{path}.perAgent.pi.wireProtocol must be a non-empty string when present"));
                }
            } else if let Some(w) = wire {
                if !is_wire_protocol(w) {
                    return Some(format!("{path}.perAgent.{agent}.wireProtocol must be a supported wire protocol"));
                }
                let expected = if agent == "claude-code" { "anthropic-messages" } else { "openai-responses" };
                if w.as_str() != Some(expected) {
                    return Some(format!("{path}.perAgent.{agent}.wireProtocol must be {expected}"));
                }
            }
        }
    }
    for agent in &supported {
        if *agent == "pi" {
            continue;
        }
        let ok = o.get("perAgent").and_then(Value::as_object).and_then(|p| p.get(*agent)).and_then(Value::as_object)
            .and_then(|x| x.get("wireProtocol")).is_some_and(is_wire_protocol);
        if !ok {
            return Some(format!("{path}.perAgent.{agent}.wireProtocol is required when {path}.agents includes {agent}"));
        }
    }
    None
}

fn filter_unknown_agents(v: &Value) -> Value {
    match v.as_array() {
        Some(a) => Value::Array(a.iter().filter(|x| !x.is_string() || is_agent(x)).cloned().collect()),
        None => v.clone(),
    }
}

/// 剔除当前客户端不认识的 agent 字符串（未来新增的 agent 不应让整份目录作废）。
fn sanitize_agents(mut o: Obj) -> Obj {
    if let Some(a) = o.get("agents").cloned() {
        o.insert("agents".into(), filter_unknown_agents(&a));
    }
    if let Some(original) = o.get("newSessionDefault").cloned() {
        let filtered = filter_unknown_agents(&original);
        let was_non_empty = original.as_array().is_some_and(|a| !a.is_empty());
        if was_non_empty && filtered.as_array().is_some_and(|a| a.is_empty()) {
            o.shift_remove("newSessionDefault");
        } else {
            o.insert("newSessionDefault".into(), filtered);
        }
    }
    if let Some(pa) = o.get("perAgent").and_then(Value::as_object) {
        let known: Obj = pa.iter().filter(|(k, _)| AGENTS.contains(&k.as_str())).map(|(k, v)| (k.clone(), v.clone())).collect();
        if known.is_empty() {
            o.shift_remove("perAgent");
        } else {
            o.insert("perAgent".into(), Value::Object(known));
        }
    }
    o
}

// ── 展示价格（modelDisplayPricing.ts）──────────────────────────────────────

fn text(v: Option<&Value>, max: usize) -> Option<&str> {
    let s = v?.as_str()?;
    (!js_trim(s).is_empty() && utf16_len(s) <= max).then_some(s)
}

struct Quote {
    price: Value,
    unit: String,
    quantity: Value,
    tier: Option<String>,
    price_type: Option<String>,
}

fn parse_quote(row: &Obj) -> Option<Quote> {
    let unit = text(row.get("unit"), 128)?;
    let tier = match row.get("tier") {
        None => None,
        Some(_) => Some(text(row.get("tier"), 256)?),
    };
    let price = row.get("price").filter(|v| finite(v).is_some_and(|n| n >= 0.0))?;
    let quantity = row.get("quantity").filter(|v| finite(v).is_some_and(|n| n > 0.0))?;
    let price_type = match row.get("priceType") {
        None => None,
        Some(Value::String(s)) if matches!(s.as_str(), "exact" | "estimated" | "starting_at") => Some(s.clone()),
        Some(_) => return None,
    };
    Some(Quote { price: price.clone(), unit: unit.to_string(), quantity: quantity.clone(), tier: tier.map(String::from), price_type })
}

fn quote_fields(q: &Quote, o: &mut Obj, with_price_type: bool) {
    o.insert("price".into(), q.price.clone());
    o.insert("unit".into(), q.unit.clone().into());
    o.insert("quantity".into(), q.quantity.clone());
    if let Some(t) = &q.tier {
        o.insert("tier".into(), t.clone().into());
    }
    if with_price_type {
        if let Some(p) = &q.price_type {
            o.insert("priceType".into(), p.clone().into());
        }
    }
}

fn parse_summary(v: Option<&Value>) -> Option<Value> {
    let raw = v?.as_object()?;
    let mut summary = Obj::new();
    for role in ["input", "output", "cacheRead", "cacheWrite"] {
        let Some(quotes) = raw.get(role) else { continue };
        let quotes = quotes.as_array().filter(|a| !a.is_empty() && a.len() <= 4)?;
        let mut result: Vec<(Quote, Value)> = vec![];
        for quote in quotes {
            let q = quote.as_object()?;
            let item = parse_quote(q)?;
            if item.price_type.as_deref() == Some("starting_at") {
                return None;
            }
            let max_price = q.get("maxPrice");
            if let Some(m) = max_price {
                let m = finite(m)?;
                if m <= finite(&item.price)? {
                    return None;
                }
            }
            if result.iter().any(|(p, _)| p.unit == item.unit && p.quantity == item.quantity && p.tier == item.tier) {
                return None;
            }
            let mut out = Obj::new();
            quote_fields(&item, &mut out, false);
            if let Some(c) = q.get("component").and_then(Value::as_str).filter(|c| valid_component(c)) {
                out.insert("component".into(), c.into());
            }
            if let Some(p) = &item.price_type {
                out.insert("priceType".into(), p.clone().into());
            }
            if let Some(m) = max_price {
                out.insert("maxPrice".into(), m.clone());
            }
            result.push((item, Value::Object(out)));
        }
        summary.insert(role.into(), Value::Array(result.into_iter().map(|(_, v)| v).collect()));
    }
    (!summary.is_empty()).then_some(Value::Object(summary))
}

/// `^[a-z][a-z0-9_]{0,31}$`
fn valid_component(s: &str) -> bool {
    let b = s.as_bytes();
    !b.is_empty() && b.len() <= 32 && b[0].is_ascii_lowercase() && b[1..].iter().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'_')
}

/// 整张表原子：任何一行无效就整体丢弃（不影响模型本身）。
pub fn parse_display_pricing(v: Option<&Value>) -> Option<Value> {
    let raw = v?.as_object()?;
    let items = raw.get("items")?.as_array().filter(|a| !a.is_empty() && a.len() <= 128)?;
    if let Some(n) = raw.get("notes") {
        let a = n.as_array().filter(|a| a.len() <= 32)?;
        if !a.iter().all(|x| text(Some(x), 2048).is_some()) {
            return None;
        }
    }
    let mut out_items = vec![];
    for item in items {
        let row = item.as_object()?;
        let label = text(row.get("label"), 256)?;
        let q = parse_quote(row)?;
        let mut o = Obj::new();
        o.insert("label".into(), label.into());
        quote_fields(&q, &mut o, true);
        out_items.push(Value::Object(o));
    }
    let mut out = Obj::new();
    out.insert("items".into(), Value::Array(out_items));
    if let Some(s) = parse_summary(raw.get("summary")) {
        out.insert("summary".into(), s);
    }
    if let Some(n) = raw.get("notes") {
        out.insert("notes".into(), n.clone());
    }
    Some(Value::Object(out))
}

/// 校验 v5 目录响应。非 v5 的版本一律拒绝（桌面端只接受当前版本）。
pub fn parse_models_sync_payload(value: &Value) -> Res<ModelsSync> {
    let Some(o) = value.as_object() else { return Err("response must be an object".into()) };
    let version = o.get("schemaVersion").and_then(Value::as_f64);
    if !matches!(version, Some(v) if [1.0, 2.0, 3.0, 4.0, 5.0].contains(&v)) {
        return Err("response.schemaVersion must be 1, 2, 3, 4, or 5".into());
    }
    if version != Some(5.0) {
        return Err("response.schemaVersion must be 5 for Desktop model sync".into());
    }
    if let Some(e) = unknown_field(o, &["schemaVersion", "accountTier", "models"], "response") {
        return Err(e);
    }
    let tier = match o.get("accountTier").and_then(Value::as_str) {
        Some(t @ ("free" | "paid" | "not_applicable")) => t.to_string(),
        _ => return Err("response.accountTier must be free, paid, or not_applicable".into()),
    };
    let raw_models = o.get("models").and_then(Value::as_array).ok_or("response.models must be an array")?;
    let mut ids = std::collections::HashSet::new();
    let mut models = Vec::with_capacity(raw_models.len());
    for (index, raw) in raw_models.iter().enumerate() {
        let Some(raw) = raw.as_object() else { return Err(format!("response.models[{index}] must be an object")) };
        let availability = match raw.get("availability").and_then(Value::as_str) {
            Some(a @ ("available" | "requires_payment")) => a.to_string(),
            _ => return Err(format!("response.models[{index}].availability must be available or requires_payment")),
        };
        let mut legacy = raw.clone();
        legacy.shift_remove("availability");
        let display_pricing = legacy.shift_remove("displayPricing");
        let display = parse_display_pricing(display_pricing.as_ref());
        let sanitized = sanitize_agents(legacy);
        if let Some(id) = sanitized.get("id").and_then(Value::as_str) {
            if !ids.insert(id.to_string()) {
                return Err(format!("response.models[{index}].id must be unique"));
            }
        }
        let sanitized = Value::Object(sanitized);
        if let Some(e) = model_entry_error(&sanitized, &format!("response.models[{index}]")) {
            return Err(e);
        }
        let Value::Object(mut m) = sanitized else { unreachable!() };
        m.insert("availability".into(), availability.into());
        if let Some(d) = display {
            m.insert("displayPricing".into(), d);
        }
        models.push(Value::Object(m));
    }
    Ok(ModelsSync { models, account_tier: tier })
}

/// 失败后的 LKG 只保留可执行模型：过期的付费差集不能继续当作升级营销事实。
pub fn without_stale_payment_upsell(models: &[Value]) -> Vec<Value> {
    models.iter().filter(|m| m["availability"] != "requires_payment").cloned().collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn matches_upstream_cases() {
        let g: Value = serde_json::from_str(include_str!("../models_sync_golden.json")).unwrap();
        let cases = g["cases"].as_array().unwrap();
        assert!(cases.len() >= 200, "{}", cases.len());
        let mut bad = vec![];
        for c in cases {
            let want = &c["result"];
            let got = parse_models_sync_payload(&c["input"]);
            let same = match (&got, want["ok"].as_bool().unwrap()) {
                (Ok(g), true) => json!({ "models": g.models, "accountTier": g.account_tier }) == json!({ "models": want["models"], "accountTier": want["accountTier"] }),
                (Err(e), false) => {
                    // 旧版本（1-4）上游先跑完整的旧版校验，文案不同；这里只要求同样被拒绝。
                    let legacy = matches!(c["input"]["schemaVersion"].as_f64(), Some(v) if v < 5.0);
                    legacy || e == want["error"].as_str().unwrap()
                }
                _ => false,
            };
            if !same {
                bad.push(format!("{}\n 期望 {}\n 实际 {:?}", c["input"], want, got.as_ref().map(|g| (&g.models, &g.account_tier)).map_err(|e| e)));
            }
        }
        assert!(bad.is_empty(), "{} 个用例不一致:\n{}", bad.len(), bad.iter().take(5).cloned().collect::<Vec<_>>().join("\n"));
    }

    #[test]
    fn lkg_drops_payment_required() {
        let m = vec![json!({"id":"a","availability":"available"}), json!({"id":"b","availability":"requires_payment"})];
        assert_eq!(without_stale_payment_upsell(&m), vec![json!({"id":"a","availability":"available"})]);
    }
}
