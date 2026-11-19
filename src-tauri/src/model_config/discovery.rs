//! `/models` 响应解析：移植 modelDiscovery.ts 的 parseModelsListResponse（OpenAI / Anthropic / Gemini / OpenRouter / Vercel /
//! OpenHubs / ElevenLabs 等形状）。输出与上游一致：`{id, name, discoveredMetadata, ...}[]`，无法识别返回 None。
use super::metadata::{is_pos_safe_int, pick, utf16_len, Meta};
use crate::local_db::mapper::js_trim;
use serde_json::{json, Map, Value};

fn host_path(value: &str) -> Option<(String, String, bool)> {
    let u = url::Url::parse(value).ok()?;
    let origin = u.origin().ascii_serialization();
    Some((origin, u.path().trim_end_matches('/').to_string(), u.query().is_some() || u.fragment().is_some() || !u.username().is_empty() || u.password().is_some()))
}

fn is_openrouter(v: &str) -> bool {
    host_path(v).is_some_and(|(o, p, _)| o == "https://openrouter.ai" && p == "/api/v1/models")
}
fn is_openhubs(v: &str) -> bool {
    host_path(v).is_some_and(|(o, p, extra)| o == "https://openhubs.zhidemai.com" && p == "/v1/models" && !extra)
}
fn is_vercel(v: &str) -> bool {
    host_path(v).is_some_and(|(o, p, _)| o == "https://ai-gateway.vercel.sh" && p == "/v1/models")
}

/// 十进制 token 数：字符串必须全是数字；`truncate` 时小数取整。
fn context_tokens(v: Option<&Value>, truncate: bool) -> Option<Value> {
    let v = v?;
    let n: f64 = match v {
        Value::String(s) => {
            let t = js_trim(s);
            if t.is_empty() || !t.bytes().all(|b| b.is_ascii_digit()) {
                return None;
            }
            t.parse().ok()?
        }
        Value::Number(n) => n.as_f64()?,
        _ => return None,
    };
    if !n.is_finite() {
        return None;
    }
    let t = if truncate { n.floor() } else { n };
    let out = crate::auth::flow::js_number(t);
    is_pos_safe_int(&out).then_some(out)
}

fn reasoning_efforts(v: Option<&Value>, key: &str) -> Option<Value> {
    let a = v?.as_array()?;
    let mut seen: Vec<Value> = vec![];
    for level in a {
        let x = match level {
            Value::String(_) => Some(level.clone()),
            Value::Object(o) => o.get(key).cloned(),
            _ => None,
        };
        if let Some(x) = x {
            if x != "none" && !seen.contains(&x) {
                seen.push(x);
            }
        }
    }
    pick(Some(&json!({ "efforts": seen }))).get("efforts").cloned()
}

fn text(v: Option<&Value>, max: usize) -> bool {
    v.and_then(Value::as_str).is_some_and(|s| !js_trim(s).is_empty() && utf16_len(s) <= max && !s.chars().any(|c| c <= '\u{1f}' || c == '\u{7f}'))
}

fn magpie(rec: &Map<String, Value>) -> Option<Value> {
    if !text(rec.get("magpie_label"), 4096) || !text(rec.get("owned_by"), 256) {
        return None;
    }
    let mut s = Map::new();
    s.insert("kind".into(), json!("magpie"));
    s.insert("providerId".into(), rec["owned_by"].clone());
    s.insert("label".into(), rec["magpie_label"].clone());
    if text(rec.get("display_name"), 1024) {
        s.insert("discoveredName".into(), rec["display_name"].clone());
    }
    Some(Value::Object(s))
}

fn s(v: Option<&Value>) -> Option<&str> {
    v.and_then(Value::as_str)
}

fn nonempty_str(v: Option<&Value>) -> bool {
    s(v).is_some_and(|x| !x.is_empty())
}

/// `a ?? b ?? c`：只在 undefined / null 时落到下一个。
fn coalesce<'a>(vals: &[Option<&'a Value>]) -> Option<&'a Value> {
    vals.iter().flatten().find(|v| !v.is_null()).copied()
}

pub fn parse_models_list_response(json_in: &Value, source_url: Option<&str>) -> Option<Vec<Value>> {
    if let Some(u) = source_url.filter(|u| is_openhubs(u)) {
        let _ = u;
        let o = json_in.as_object()?;
        let data = o.get("data")?.as_array()?;
        if o.get("error").is_some_and(|e| !e.is_null()) || o.get("error_code").is_some_and(|c| *c != json!(0)) {
            return None;
        }
        if data.iter().any(|m| !m.get("id").and_then(Value::as_str).is_some_and(|i| !js_trim(i).is_empty())) {
            return None;
        }
    }
    let list: &Vec<Value> = match json_in {
        Value::Array(a) => a,
        Value::Object(o) => match (o.get("data"), o.get("models")) {
            (Some(Value::Array(a)), _) => a,
            (_, Some(Value::Array(a))) => a,
            _ => return None,
        },
        _ => return None,
    };
    let empty = Map::new();
    let magpie_catalog = list.iter().any(|i| i.as_object().is_some_and(|r| magpie(r).is_some()));
    let vercel = source_url.is_some_and(is_vercel);
    let mut out: Vec<Value> = vec![];
    let mut seen: Vec<String> = vec![];
    for item in list {
        let obj = item.as_object();
        let record = obj.unwrap_or(&empty);
        let gen_methods = record.get("supportedGenerationMethods");
        let google_name = s(record.get("name")).filter(|n| n.starts_with("models/"));
        let google = obj.is_some() && google_name.is_some() && gen_methods.and_then(Value::as_array).is_some_and(|a| a.iter().any(|x| x == "generateContent"));
        let id: Option<String> = match item {
            Value::String(x) => Some(x.clone()),
            Value::Object(_) => ["id", "slug", "key", "model_name", "model_id"]
                .iter()
                .find_map(|k| s(record.get(*k)).map(String::from))
                .or_else(|| google.then(|| google_name.unwrap()["models/".len()..].to_string())),
            _ => None,
        };
        let Some(id) = id.filter(|i| !i.is_empty()) else { continue };
        if seen.contains(&id) {
            continue;
        }
        seen.push(id.clone());
        let rec = obj;
        let r = |k: &str| rec.and_then(|o| o.get(k));
        let google_display = google.then(|| record.get("displayName")).flatten();
        let name_v: Value = if let Some(g) = google_display.filter(|g| g.is_string()) {
            g.clone()
        } else if nonempty_str(r("display_name")) {
            r("display_name").unwrap().clone()
        } else if nonempty_str(r("name")) {
            r("name").unwrap().clone()
        } else {
            json!(id)
        };
        let limit = r("limit").and_then(Value::as_object);
        let nested = {
            let mut m = Map::new();
            if let Some(v) = context_tokens(limit.and_then(|l| l.get("context")), false) {
                m.insert("contextWindow".into(), v);
            }
            if let Some(v) = limit.and_then(|l| l.get("output")) {
                m.insert("maxOutputTokens".into(), v.clone());
            }
            pick(Some(&Value::Object(m)))
        };
        let raw_window = if rec.is_some() {
            [r("context_length"), r("context_window"), r("max_context_length"), r("max_input_tokens"), google.then(|| record.get("inputTokenLimit")).flatten()]
                .iter()
                .find_map(|v| context_tokens(*v, true))
        } else {
            None
        };
        let info = record.get("model_info").and_then(Value::as_object).unwrap_or(&empty);
        let capabilities = record.get("capabilities").and_then(Value::as_object).unwrap_or(&empty);
        let architecture = record.get("architecture").and_then(Value::as_object);
        let reasoning = record.get("reasoning").and_then(Value::as_object);
        let rg = |k: &str| reasoning.and_then(|o| o.get(k));
        let raw_default = coalesce(&[rg("defaultEffort"), rg("default_effort"), record.get("default_effort"), record.get("default_reasoning_level"), record.get("reasoningEffort")]);
        let arch_in = architecture.and_then(|a| a.get("input_modalities"));
        let arch_out = architecture.and_then(|a| a.get("output_modalities"));
        let modalities: Option<Value> = record.get("modalities").filter(|m| !m.is_null()).cloned().or_else(|| match (arch_in, arch_out) {
            (Some(i), Some(o)) if i.is_array() && o.is_array() => Some(json!({ "input": i, "output": o })),
            _ => None,
        });
        let input_modalities = modalities.as_ref().and_then(|m| m.get("input")).filter(|v| !v.is_null()).cloned().or_else(|| record.get("input_modalities").cloned());
        let context_window_max = context_tokens(coalesce(&[record.get("max_context_window"), record.get("contextWindowMax")]), false);
        let service_tiers = record.get("service_tiers");
        let additional = record.get("additional_speed_tiers");
        let mut declared_speed: Vec<Value> = vec![];
        if let Some(a) = service_tiers.and_then(Value::as_array) {
            declared_speed.extend(a.iter().map(|t| t.get("id").cloned().unwrap_or(Value::Null)));
        }
        if let Some(a) = additional.and_then(Value::as_array) {
            declared_speed.extend(a.iter().cloned());
        }
        let rtype = record.get("type");
        if vercel && rtype.is_some_and(|t| !t.is_null() && *t != json!("language")) {
            continue;
        }
        let eleven_mode = (s(record.get("model_id")) == Some(&id) && record.get("can_do_text_to_speech").is_some_and(Value::is_boolean))
            .then(|| if record["can_do_text_to_speech"] == json!(true) { "audio_speech" } else { "other" });
        let or_speech = source_url.is_some_and(is_openrouter) && arch_out.and_then(Value::as_array).is_some_and(|a| a.iter().any(|x| x == "speech"));
        let mode_v: Option<Value> = coalesce(&[record.get("mode"), info.get("mode")]).cloned().or_else(|| eleven_mode.map(|m| json!(m))).or_else(|| or_speech.then(|| json!("audio_speech"))).or_else(|| {
            if vercel && rtype == Some(&json!("language")) {
                Some(json!("chat"))
            } else if vercel {
                rtype.cloned()
            } else {
                None
            }
        });
        let has_name = [r("display_name"), r("name"), google_display].iter().flatten().any(|v| v.as_str().is_some_and(|x| !js_trim(x).is_empty()));
        let efforts_v: Option<Value> = coalesce(&[rg("supportedEfforts")]).cloned()
            .or_else(|| rg("supported_efforts").and_then(Value::as_array).map(|a| Value::Array(a.iter().filter(|v| **v != "none").cloned().collect())))
            .or_else(|| record.get("supported_efforts").cloned())
            .or_else(|| reasoning_efforts(record.get("supported_reasoning_levels"), "effort"))
            .or_else(|| if record.get("supportsReasoningEffort") == Some(&json!(false)) { Some(json!([])) } else { reasoning_efforts(record.get("reasoningEfforts"), "value") })
            .or_else(|| {
                if !vercel {
                    return None;
                }
                record.get("reasoning_options").and_then(Value::as_array).and_then(|a| a.iter().find(|o| o.get("type") == Some(&json!("effort")))).and_then(|o| o.get("values")).cloned()
            })
            .or_else(|| {
                let no_params = record.get("supported_parameters").and_then(Value::as_array).is_some_and(|p| {
                    !p.iter().any(|v| v.as_str().is_some_and(|x| ["reasoning", "reasoning_effort", "reasoning_budget", "thinking"].contains(&x)))
                });
                (record.get("reasoning") == Some(&json!(false)) || info.get("supports_reasoning") == Some(&json!(false)) || no_params).then(|| json!([]))
            });
        let tools: Option<Value> = match record.get("supported_parameters").and_then(Value::as_array) {
            Some(p) => Some(json!(p.iter().any(|x| x == "tools"))),
            None => coalesce(&[info.get("supports_function_calling"), capabilities.get("trained_for_tool_use")]).cloned(),
        };
        let fast: Option<Value> = coalesce(&[record.get("supports_fast_mode"), record.get("supportsServiceTier")]).cloned().or_else(|| {
            let tiers_ok = service_tiers.and_then(Value::as_array).is_some_and(|a| a.iter().all(|t| t.get("id").is_some_and(Value::is_string)));
            if tiers_ok {
                Some(json!(declared_speed.iter().any(|t| *t == "priority" || *t == "fast")))
            } else {
                additional.and_then(Value::as_array).map(|a| json!(a.iter().any(|t| *t == "fast")))
            }
        });
        let image: Option<Value> = coalesce(&[record.get("supports_image_input"), info.get("supports_vision"), capabilities.get("vision")]).cloned().or_else(|| {
            input_modalities.as_ref().and_then(Value::as_array).filter(|a| a.iter().all(Value::is_string)).map(|a| json!(a.iter().any(|x| x == "image")))
        });
        let max_output = coalesce(&[
            record.get("max_output_tokens"),
            info.get("max_output_tokens"),
            google.then(|| record.get("outputTokenLimit")).flatten(),
            record.get("maxOutputTokens"),
            if vercel { record.get("max_tokens") } else { None },
            record.get("top_provider").and_then(Value::as_object).and_then(|t| t.get("max_completion_tokens")),
        ]);
        let window: Option<Value> = raw_window.or_else(|| context_tokens(info.get("max_input_tokens"), false)).or_else(|| context_tokens(record.get("contextWindow"), false));
        let mut layer = Map::new();
        let mut put = |k: &str, v: Option<Value>| {
            if let Some(v) = v {
                layer.insert(k.to_string(), v);
            }
        };
        put("nativeApi", if record.get("nativeApi").is_some() { record.get("nativeApi").cloned() } else { record.get("native_api").cloned() });
        if has_name {
            put("name", Some(name_v.clone()));
        }
        put("mode", mode_v);
        put("modalities", modalities.clone());
        put("officialDocs", record.get("officialDocs").cloned());
        put("description", record.get("description").cloned());
        put("group", record.get("group").cloned());
        put("contextWindow", window);
        put("contextWindowMax", context_window_max);
        put("maxOutputTokens", max_output.cloned());
        put("efforts", efforts_v);
        put("supportsToolCalls", tools);
        put("reasoningRequired", coalesce(&[rg("required"), rg("mandatory")]).cloned());
        put("defaultEffort", raw_default.map(|d| if *d == json!("none") { Value::Null } else { d.clone() }));
        put("supportsFastMode", fast);
        put("supportsImageInput", image);
        let mut meta: Meta = nested;
        for (k, v) in pick(Some(&Value::Object(record.clone()))) {
            meta.insert(k, v);
        }
        for (k, v) in pick(Some(&Value::Object(layer))) {
            meta.insert(k, v);
        }
        if let (Some(mx), Some(w)) = (meta.get("contextWindowMax").and_then(Value::as_f64), meta.get("contextWindow").and_then(Value::as_f64)) {
            if mx < w {
                meta.shift_remove("contextWindowMax");
            }
        }
        // 价格：只认 OpenRouter / Vercel 语言模型，单位是每 token 美元，换算成每百万。
        let prices = if source_url.is_some() && !or_speech && (source_url.is_some_and(is_openrouter) || (vercel && rtype == Some(&json!("language")))) {
            record.get("pricing").and_then(Value::as_object)
        } else {
            None
        };
        let pairs: [(&str, &str); 4] = if vercel {
            [("input", "input"), ("output", "output"), ("input_cache_read", "cacheRead"), ("input_cache_write", "cacheWrite")]
        } else {
            [("prompt", "input"), ("completion", "output"), ("input_cache_read", "cacheRead"), ("input_cache_write", "cacheWrite")]
        };
        let mut cost = Map::new();
        for (src, dst) in pairs {
            let Some(raw) = prices.and_then(|p| p.get(src)) else { continue };
            let amount = match raw {
                Value::String(t) if !js_trim(t).is_empty() => js_to_number(t),
                Value::Number(n) => n.as_f64(),
                _ => None,
            };
            if let Some(a) = amount.map(|a| a * 1_000_000.0).filter(|a| a.is_finite() && *a >= 0.0) {
                cost.insert(dst.into(), crate::auth::flow::js_number(a));
            }
        }
        let mut entry = Map::new();
        entry.insert("id".into(), json!(id));
        if let Some(src) = magpie(record) {
            entry.insert("discoveredSource".into(), src);
        }
        if magpie_catalog {
            entry.insert("opaqueModelId".into(), json!(true));
        }
        if service_tiers.is_some_and(Value::is_array) || additional.is_some_and(Value::is_array) {
            entry.insert("supportsUltrafastMode".into(), json!(declared_speed.iter().any(|t| *t == "ultrafast")));
        }
        let window_out = meta.get("contextWindow").cloned();
        entry.insert("discoveredMetadata".into(), Value::Object(meta));
        if !cost.is_empty() {
            entry.insert("discoveredCost".into(), Value::Object(cost));
        }
        entry.insert("name".into(), name_v);
        if let Some(w) = window_out {
            entry.insert("contextWindow".into(), w);
        }
        out.push(Value::Object(entry));
    }
    Some(out)
}

/// JS `Number(string)`：去空白后按十进制 / 十六进制 / 科学计数解析，空串为 0，其余 NaN。
fn js_to_number(t: &str) -> Option<f64> {
    let t = js_trim(t);
    if t.is_empty() {
        return Some(0.0);
    }
    if let Some(h) = t.strip_prefix("0x").or_else(|| t.strip_prefix("0X")) {
        return u64::from_str_radix(h, 16).ok().map(|n| n as f64);
    }
    match t {
        "Infinity" | "+Infinity" => return Some(f64::INFINITY),
        "-Infinity" => return Some(f64::NEG_INFINITY),
        _ => {}
    }
    if t.bytes().any(|b| !(b.is_ascii_digit() || matches!(b, b'.' | b'e' | b'E' | b'+' | b'-'))) {
        return None;
    }
    t.parse::<f64>().ok()
}
