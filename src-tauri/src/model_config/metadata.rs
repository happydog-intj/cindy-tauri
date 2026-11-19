//! 模型元数据分层：移植 model-providers 的 modelMetadataLayers.ts / modelGeneration.ts / effortResolution 的相关部分。
//! 元数据全是 JSON 值，字段白名单 + 校验与上游 `validModelMetadata` 一致；注册表在 `registry.rs`。
use crate::local_db::mapper::js_trim;
use regex::Regex;
use serde_json::{json, Map, Value};
use std::sync::OnceLock;

pub type Meta = Map<String, Value>;

pub const EFFORT_ORDER: [&str; 7] = ["minimal", "low", "medium", "high", "xhigh", "max", "ultra"];
const FIELDS: [&str; 16] = [
    "nativeApi", "mode", "modalities", "officialDocs", "name", "description", "group", "contextWindow", "contextWindowMax", "maxOutputTokens",
    "efforts", "defaultEffort", "supportsFastMode", "supportsImageInput", "supportsToolCalls", "reasoningRequired",
];
pub const NATIVE_APIS: [&str; 11] = [
    "anthropic-messages", "openai-responses", "openai-completions", "google-generative-ai", "openai-images", "openai-videos", "xai-videos",
    "openai-audio-speech", "openai-audio-transcriptions", "openai-realtime", "openai-embeddings",
];

pub fn utf16_len(s: &str) -> usize {
    s.encode_utf16().count()
}

fn ident_re() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new(r"^[a-z][a-z0-9_]{0,63}$").unwrap())
}

/// Number.isSafeInteger(v) && v > 0
pub fn is_pos_safe_int(v: &Value) -> bool {
    match v.as_f64() {
        Some(n) => v.is_number() && n.fract() == 0.0 && n > 0.0 && n <= 9_007_199_254_740_991.0,
        None => false,
    }
}

fn is_effort(v: &Value) -> bool {
    v.as_str().is_some_and(|s| EFFORT_ORDER.contains(&s))
}

fn valid_modalities(v: &Value) -> bool {
    let Some(m) = v.as_object() else { return false };
    if !m.keys().all(|k| k == "input" || k == "output") {
        return false;
    }
    ["input", "output"].iter().all(|k| match m.get(*k).and_then(Value::as_array) {
        Some(list) => {
            let names: Vec<&str> = list.iter().filter_map(Value::as_str).collect();
            list.len() <= 16
                && names.len() == list.len()
                && names.iter().all(|s| ident_re().is_match(s))
                && names.iter().collect::<std::collections::HashSet<_>>().len() == names.len()
        }
        None => false,
    })
}

fn valid_field(key: &str, v: &Value) -> bool {
    match key {
        "nativeApi" => v.is_null() || v.as_str().is_some_and(|s| NATIVE_APIS.contains(&s)),
        "mode" => v.as_str().is_some_and(|s| ident_re().is_match(s)),
        "modalities" => valid_modalities(v),
        "officialDocs" => match v.as_str() {
            Some(s) if utf16_len(s) <= 2048 => url::Url::parse(s).is_ok_and(|u| u.scheme() == "https" && u.username().is_empty() && u.password().is_none()),
            _ => false,
        },
        "name" | "description" | "group" => {
            let max = match key {
                "name" => 256,
                "group" => 128,
                _ => 2000,
            };
            v.as_str().is_some_and(|s| !js_trim(s).is_empty() && utf16_len(s) <= max)
        }
        "contextWindow" | "contextWindowMax" | "maxOutputTokens" => is_pos_safe_int(v),
        "efforts" => match v.as_array() {
            Some(a) => {
                a.iter().all(is_effort) && a.iter().map(|x| x.as_str().unwrap()).collect::<std::collections::HashSet<_>>().len() == a.len()
            }
            None => false,
        },
        "defaultEffort" => v.is_null() || is_effort(v),
        _ => v.is_boolean(),
    }
}

/// pickModelMetadata：只留白名单里值合法的字段（顺序按白名单）。
pub fn pick(value: Option<&Value>) -> Meta {
    let mut out = Meta::new();
    let Some(o) = value.and_then(Value::as_object) else { return out };
    for k in FIELDS {
        if let Some(v) = o.get(k) {
            if valid_field(k, v) {
                out.insert(k.to_string(), v.clone());
            }
        }
    }
    out
}

pub fn pick_meta(m: &Meta) -> Meta {
    let mut out = Meta::new();
    for k in FIELDS {
        if let Some(v) = m.get(k) {
            if valid_field(k, v) {
                out.insert(k.to_string(), v.clone());
            }
        }
    }
    out
}

/// mergeModelMetadata：后者覆盖前者；undefined 继承，false / null / [] 保留；数组整体替换。
pub fn merge(layers: &[Option<&Meta>]) -> Meta {
    let mut result = Meta::new();
    for layer in layers.iter().flatten() {
        let fields = pick_meta(layer);
        let has_modalities = fields.contains_key("modalities");
        let denies_image = fields.get("supportsImageInput") == Some(&json!(false));
        for (k, v) in fields {
            result.insert(k, v);
        }
        if denies_image && !has_modalities {
            if let Some(input) = result.get_mut("modalities").and_then(|m| m.get_mut("input")).and_then(Value::as_array_mut) {
                if input.iter().any(|x| x == "image") {
                    input.retain(|x| x != "image");
                }
            }
        }
    }
    result
}

// ── 代际 ─────────────────────────────────────────────────────────────────────

pub struct Generation {
    pub family: String,
    pub version: Vec<f64>,
}

fn size_unit_re() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    // 上游用前瞻 (?=[-_/.:]|$)；这里把它并入匹配，只用于 is_match，等价。
    R.get_or_init(|| Regex::new(r"^[a-z][a-z/-]*?\d+(?:\.\d+)?(?:x\d+(?:\.\d+)?)?[kmbt](?:[-_/.:]|$)").unwrap())
}

fn all_digits(s: &[u8]) -> bool {
    !s.is_empty() && s.iter().all(u8::is_ascii_digit)
}

/// `(?:[.-]\d{1,3}(?=[.-]|$))*` 加上 `([^0-9].*|$)`，带回溯，优先级与 JS 一致。返回版本段结束位置。
fn version_tail(s: &[u8], j: usize) -> Option<usize> {
    let n = s.len();
    if j < n && (s[j] == b'.' || s[j] == b'-') {
        for k in (1..=3).rev() {
            let e = j + 1 + k;
            if e <= n && all_digits(&s[j + 1..e]) && (e == n || s[e] == b'.' || s[e] == b'-') {
                if let Some(r) = version_tail(s, e) {
                    return Some(r);
                }
            }
        }
    }
    (j == n || !s[j].is_ascii_digit()).then_some(j)
}

pub fn generation(id: &str) -> Option<Generation> {
    let lower = id.to_lowercase();
    let normalized = ["openai/", "anthropic/", "google/", "x-ai/", "xai/", "deepseek/", "qwen/"]
        .iter()
        .find_map(|p| lower.strip_prefix(p))
        .unwrap_or(&lower);
    if size_unit_re().is_match(normalized) {
        return None;
    }
    let s = normalized.as_bytes();
    // 家族名 `[a-z][a-z/-]*?` 是惰性的，但数字不在字符类里，所以只能停在第一个数字上。
    if s.is_empty() || !s[0].is_ascii_lowercase() {
        return None;
    }
    let mut l = 1;
    while l < s.len() && (s[l].is_ascii_lowercase() || s[l] == b'/' || s[l] == b'-') {
        l += 1;
    }
    for k in (1..=3).rev() {
        let e = l + k;
        if e <= s.len() && all_digits(&s[l..e]) {
            if let Some(ve) = version_tail(s, e) {
                let version = normalized[l..ve].split(['.', '-']).map(|x| x.parse::<f64>().unwrap_or(f64::NAN)).collect();
                // `[^0-9].*`：首字符之后到行终止符为止；`.` 不匹配 \n \r \u2028 \u2029。
                let rest = &normalized[ve..];
                let mut tail = String::new();
                let mut chars = rest.chars();
                if let Some(c) = chars.next() {
                    tail.push(c);
                    for c in chars {
                        if matches!(c, '\n' | '\r' | '\u{2028}' | '\u{2029}') {
                            break;
                        }
                        tail.push(c);
                    }
                }
                return Some(Generation { family: format!("{}|{}", &normalized[..l], tail), version });
            }
        }
    }
    None
}

fn compare(a: &[f64], b: &[f64]) -> f64 {
    for i in 0..a.len().max(b.len()) {
        let d = a.get(i).copied().unwrap_or(0.0) - b.get(i).copied().unwrap_or(0.0);
        if d != 0.0 {
            return d;
        }
    }
    0.0
}

/// 同家族里版本更低的候选，旧的在前（近的代际后覆盖远的）。
pub fn previous_generations<T: Clone>(id: &str, candidates: &[T], get_id: impl Fn(&T) -> String) -> Vec<T> {
    let Some(target) = generation(id) else { return vec![] };
    let mut hits: Vec<(Vec<f64>, T)> = candidates
        .iter()
        .filter_map(|c| {
            let src = generation(&get_id(c))?;
            (src.family == target.family && compare(&src.version, &target.version) < 0.0).then(|| (src.version, c.clone()))
        })
        .collect();
    hits.sort_by(|a, b| compare(&a.0, &b.0).partial_cmp(&0.0).unwrap_or(std::cmp::Ordering::Equal));
    hits.into_iter().map(|(_, c)| c).collect()
}

/// 继承能力，不继承身份、文档、价格、路由或成员资格。
pub fn generation_capabilities(m: &Meta) -> Meta {
    let mut out = pick_meta(m);
    for k in ["name", "nativeApi", "description", "officialDocs", "group", "mode"] {
        out.shift_remove(k);
    }
    out
}

pub fn default_effort_for_capabilities(efforts: &[String]) -> Value {
    for pref in ["medium", "high", "low", "xhigh", "max", "minimal", "ultra"] {
        if let Some(e) = efforts.iter().find(|e| *e == pref) {
            return json!(e);
        }
    }
    efforts.first().map_or(Value::Null, |e| json!(e))
}

/// Number.isSafeInteger(v) && v >= 0
pub fn is_nonneg_safe_int(v: &Value) -> bool {
    v.as_f64().is_some_and(|n| n >= 0.0 && n.fract() == 0.0 && n <= 9007199254740991.0)
}
