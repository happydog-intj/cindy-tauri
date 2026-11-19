//! 语音输入设置 / 历史的规范化：desktop shared/voiceInputData.ts（设置、快捷键、历史部分）+
//! voiceConversation.ts 的三个 normalize 函数 + voice-input-core 的历史压缩。
//! 区域固定为 global（Tauri 构建只做 global 区，语言默认 auto）。逻辑用 `voice_input_golden.json` 核对。

use crate::appearance_core::{jnum, js_trim, truthy, utf16_len};
use crate::voice_dict::{self as vd, slice_utf16, Env};
use regex::Regex;
use serde_json::{json, Map, Value};
use std::sync::OnceLock;

pub const UNAVAILABLE: &str = "voice input is not available in the Tauri build yet";
pub const PROVIDER_KIND: &str = "litellm-volcengine-sauc-asr";
const LOCALES: [&str; 12] = ["zh-CN", "zh-TW", "en", "ja", "ko", "es", "fr", "de", "pt-PT", "pt-BR", "zh-Hant-x-lzh", "zh-Hans-x-lzh"];
const PROTOCOLS: [&str; 5] = ["openai", "elevenlabs", "minimax", "qwen-tts", "dashscope-tts"];
const MODIFIER_CODES: [&str; 7] = ["MetaLeft", "MetaRight", "AltLeft", "AltRight", "ControlLeft", "ControlRight", "Fn"];
pub const STRICTNESS: [&str; 3] = ["relaxed", "standard", "strict"];
const DEFAULT_INSTRUCTIONS: &str = "让文本清楚自然，不明显改写；保留我的语气，不改成公文或客服话术。\n保留技术词、模型名、产品名、变量、路径、命令和大小写。\n可补标点、断句和必要换行；删除无意义口头词、口吃和重复。\n遇到“不对”“不是”“我的意思是”等自我修正，以后面的说法为准。";
const HISTORY_HEADER: &str = "语音输入历史（旧到新，仅作术语、别名和用词风格参考）：";
const COMPACT_CHARS: usize = 12_000;
const COMPACT_TARGET: usize = 8_000;
const COMPACT_KEEP: usize = 40;
const MAX_ITEM_CHARS: usize = 360;

pub fn is_undef(v: &Value) -> bool {
    v.get("__cindy_t").and_then(Value::as_str) == Some("u")
}

/// 取字段；IPC 里的 undefined 标记视同缺省。
pub fn field<'a>(o: &'a Map<String, Value>, k: &str) -> Option<&'a Value> {
    o.get(k).filter(|v| !is_undef(v))
}

pub fn bool_of(v: Option<&Value>) -> Option<bool> {
    v.and_then(Value::as_bool)
}

// ── 默认值 ──

pub fn default_shortcut(platform: &str) -> Value {
    let mods = |alt: bool, ctrl: bool, shift: bool| json!({ "meta": false, "ctrl": ctrl, "alt": alt, "shift": shift, "fn": false });
    match platform {
        "linux" => Value::Null,
        "darwin" => json!({ "trigger": "keyboard", "code": "Space", "key": " ", "modifiers": mods(true, false, false) }),
        _ => json!({ "trigger": "keyboard", "code": "Space", "key": " ", "modifiers": mods(false, true, true) }),
    }
}

pub fn default_settings(platform: &str) -> Map<String, Value> {
    let Value::Object(m) = json!({
        "language": "auto", "microphoneDeviceId": null, "muteSystemAudio": true, "playInteractionSound": true,
        "fastActivationEnabled": false, "refinementEnabled": true, "refinementInstructions": DEFAULT_INSTRUCTIONS,
        "autoDictionaryEnabled": true, "dictionarySyncEnabled": true, "composerLongPressEnabled": true,
        "speakerFilterEnabled": false, "speakerFilterStrictness": "standard",
        "dictionaryEntries": [], "dictionaryCandidates": [], "suppressedAutomaticDictionaryTexts": [],
        "shortcut": default_shortcut(platform),
    }) else { unreachable!() };
    m
}

// ── 快捷键 ──

pub fn normalize_shortcut(raw: &Value) -> Value {
    let Some(c) = raw.as_object().filter(|_| !raw.is_null()) else { return Value::Null };
    let code = match field(c, "code").and_then(Value::as_str) {
        Some(s) if !js_trim(s).is_empty() => s,
        _ => return Value::Null,
    };
    let key = field(c, "key").and_then(Value::as_str).unwrap_or(code);
    if field(c, "trigger").and_then(Value::as_str) == Some("modifier") {
        if !MODIFIER_CODES.contains(&code) {
            return Value::Null;
        }
        return json!({ "trigger": "modifier", "code": code, "key": key, "modifiers": { "meta": false, "ctrl": false, "alt": false, "shift": false, "fn": false } });
    }
    let Some(m) = field(c, "modifiers").filter(|m| m.is_object() || m.is_array()) else { return Value::Null };
    let b = |k: &str| truthy(m.get(k));
    json!({ "trigger": "keyboard", "code": code, "key": key, "modifiers": { "meta": b("meta"), "ctrl": b("ctrl"), "alt": b("alt"), "shift": b("shift"), "fn": b("fn") } })
}

// ── 语音对话选择（voiceConversation.ts）──

fn voice_id(v: Option<&Value>) -> Option<String> {
    let s = v?.as_str()?;
    if utf16_len(s) > 128 {
        return None;
    }
    let bad = s.chars().any(|c| (c as u32) < 32 || c as u32 == 127 || c == '/' || c == '\\');
    let t = js_trim(s);
    (!t.is_empty() && !bad).then(|| t.to_string())
}

fn speech_selection(o: &Map<String, Value>) -> Option<Map<String, Value>> {
    let model = field(o, "modelId")?.as_str()?;
    if js_trim(model).is_empty() || utf16_len(model) > 256 {
        return None;
    }
    let (provider, agent, protocol) = (field(o, "providerId"), field(o, "agent"), field(o, "protocol"));
    if let Some(p) = provider {
        match p.as_str() {
            Some(s) if !js_trim(s).is_empty() && utf16_len(s) <= 256 => {}
            _ => return None,
        }
    }
    if let Some(p) = protocol {
        if !p.as_str().map(|s| PROTOCOLS.contains(&s)).unwrap_or(false) {
            return None;
        }
    }
    let default_provider = provider.is_none() || provider.and_then(Value::as_str) == Some("xd");
    if default_provider && protocol.is_some() && protocol.and_then(Value::as_str) != Some("openai") {
        return None;
    }
    if let Some(a) = agent {
        if !a.as_str().map(|s| ["claude-code", "codex", "pi"].contains(&s)).unwrap_or(false) {
            return None;
        }
    }
    if (provider.is_some() && !default_provider && agent.is_none()) || (default_provider && agent.is_some()) {
        return None;
    }
    let mut out = Map::new();
    out.insert("modelId".into(), json!(js_trim(model)));
    if let Some(p) = provider {
        out.insert("providerId".into(), p.clone());
    }
    if let Some(a) = agent {
        out.insert("agent".into(), a.clone());
    }
    if let Some(p) = protocol {
        out.insert("protocol".into(), p.clone());
    }
    Some(out)
}

fn conversation_selection(v: &Value) -> Option<Value> {
    let o = v.as_object()?;
    let mut route = speech_selection(o)?;
    let voice = voice_id(field(o, "voice"))?;
    if let Some(s) = field(o, "speed") {
        match s.as_f64() {
            Some(f) if f.is_finite() && (0.25..=4.0).contains(&f) => {
                route.insert("voice".into(), json!(voice));
                route.insert("speed".into(), s.clone());
                return Some(Value::Object(route));
            }
            _ => return None,
        }
    }
    route.insert("voice".into(), json!(voice));
    Some(Value::Object(route))
}

fn conversation_prefs(v: Option<&Value>) -> Map<String, Value> {
    let mut out = Map::new();
    let Some(o) = v.and_then(Value::as_object) else { return out };
    for (id, raw) in o {
        if js_trim(id).is_empty() || utf16_len(id) > 256 {
            continue;
        }
        if raw.is_null() {
            out.insert(id.clone(), Value::Null);
        } else if let Some(sel) = conversation_selection(raw) {
            out.insert(id.clone(), sel);
        }
    }
    out
}

fn sentence_playback(v: Option<&Value>) -> Map<String, Value> {
    let mut out = Map::new();
    if let Some(o) = v.and_then(Value::as_object) {
        for (id, b) in o {
            if b.is_boolean() && !js_trim(id).is_empty() && utf16_len(id) <= 256 {
                out.insert(id.clone(), b.clone());
            }
        }
    }
    out
}

// ── 设置 ──

fn is_language(v: Option<&Value>) -> bool {
    v.and_then(Value::as_str).map(|s| s == "auto" || LOCALES.contains(&s)).unwrap_or(false)
}

fn refinement_instructions(v: Option<&Value>, fallback: &Value) -> Value {
    let Some(s) = v.and_then(Value::as_str) else { return fallback.clone() };
    let n = slice_utf16(&s.replace("\r\n", "\n").replace('\r', "\n"), 1000);
    if js_trim(&n).is_empty() { fallback.clone() } else { json!(n) }
}

pub fn normalize_settings(env: &mut dyn Env, raw: Option<&Value>, platform: &str) -> Value {
    let defaults = default_settings(platform);
    let empty = Map::new();
    let c = match raw {
        Some(Value::Object(o)) => o,
        Some(Value::Array(_)) => &empty,
        _ => return Value::Object(defaults),
    };
    let mut out = Map::new();
    if let Some(sel) = field(c, "conversationSelection").and_then(conversation_selection) {
        out.insert("conversationSelection".into(), sel);
    }
    let prefs = conversation_prefs(field(c, "conversationSelections"));
    if !prefs.is_empty() {
        out.insert("conversationSelections".into(), Value::Object(prefs));
    }
    let playback = sentence_playback(field(c, "conversationSentencePlayback"));
    if !playback.is_empty() {
        out.insert("conversationSentencePlayback".into(), Value::Object(playback));
    }
    let lang = field(c, "language");
    out.insert("language".into(), if is_language(lang) { lang.unwrap().clone() } else { defaults["language"].clone() });
    let mic = field(c, "microphoneDeviceId").and_then(Value::as_str).filter(|s| !js_trim(s).is_empty());
    out.insert("microphoneDeviceId".into(), mic.map(|s| json!(s)).unwrap_or(defaults["microphoneDeviceId"].clone()));
    let b = |k: &str, d: &Value| bool_of(field(c, k)).map(Value::Bool).unwrap_or_else(|| d.clone());
    out.insert("muteSystemAudio".into(), b("muteSystemAudio", &defaults["muteSystemAudio"]));
    let legacy_sound = bool_of(field(c, "playStartSound")).map(Value::Bool);
    out.insert(
        "playInteractionSound".into(),
        bool_of(field(c, "playInteractionSound")).map(Value::Bool).or(legacy_sound).unwrap_or(defaults["playInteractionSound"].clone()),
    );
    out.insert("fastActivationEnabled".into(), b("fastActivationEnabled", &defaults["fastActivationEnabled"]));
    out.insert("refinementEnabled".into(), b("refinementEnabled", &defaults["refinementEnabled"]));
    out.insert("refinementInstructions".into(), refinement_instructions(field(c, "refinementInstructions"), &defaults["refinementInstructions"]));
    out.insert("autoDictionaryEnabled".into(), b("autoDictionaryEnabled", &defaults["autoDictionaryEnabled"]));
    // dictionarySync：只认 override；旧版直接存了有效值，仅当它与默认（true）不同才当作用户选择。
    match field(c, "dictionarySyncEnabledOverride") {
        Some(Value::Null) => {
            out.insert("dictionarySyncEnabled".into(), json!(true));
        }
        other => {
            let ov = bool_of(other).or_else(|| bool_of(field(c, "dictionarySyncEnabled")).filter(|v| !*v));
            match ov {
                Some(v) => {
                    out.insert("dictionarySyncEnabled".into(), json!(v));
                    out.insert("dictionarySyncEnabledOverride".into(), json!(v));
                }
                None => {
                    out.insert("dictionarySyncEnabled".into(), json!(true));
                }
            }
        }
    }
    match bool_of(field(c, "composerLongPressEnabledOverride")) {
        Some(v) => {
            out.insert("composerLongPressEnabled".into(), json!(v));
            out.insert("composerLongPressEnabledOverride".into(), json!(v));
        }
        None => {
            out.insert("composerLongPressEnabled".into(), json!(true));
        }
    }
    out.insert("speakerFilterEnabled".into(), b("speakerFilterEnabled", &defaults["speakerFilterEnabled"]));
    match field(c, "speakerFilterStrictnessOverride").and_then(Value::as_str).filter(|s| STRICTNESS.contains(s)) {
        Some(s) => {
            out.insert("speakerFilterStrictness".into(), json!(s));
            out.insert("speakerFilterStrictnessOverride".into(), json!(s));
        }
        None => {
            out.insert("speakerFilterStrictness".into(), json!("standard"));
        }
    }
    let entries_raw = field(c, "dictionaryEntries").filter(|v| !v.is_null()).or_else(|| field(c, "customDictionary"));
    let entries = vd::normalize_entries(env, entries_raw);
    out.insert("dictionaryEntries".into(), Value::Array(entries.iter().map(vd::Entry::to_value).collect()));
    let cands = vd::normalize_candidates(env, field(c, "dictionaryCandidates"));
    out.insert("dictionaryCandidates".into(), Value::Array(cands.iter().map(vd::Candidate::to_value).collect()));
    out.insert("suppressedAutomaticDictionaryTexts".into(), json!(vd::normalize_suppressed(field(c, "suppressedAutomaticDictionaryTexts"))));
    let shortcut = if c.contains_key("shortcut") { normalize_shortcut(c.get("shortcut").unwrap()) } else { defaults["shortcut"].clone() };
    out.insert("shortcut".into(), if platform == "linux" { Value::Null } else { shortcut });
    Value::Object(out)
}

// ── 历史 ──

/// JS `Number(x)`（对象 / 数组按 NaN 处理，属已知简化）。
pub fn js_number(v: Option<&Value>) -> f64 {
    static DEC: OnceLock<Regex> = OnceLock::new();
    match v {
        None | Some(Value::Object(_)) | Some(Value::Array(_)) => f64::NAN,
        Some(Value::Null) => 0.0,
        Some(Value::Bool(b)) => *b as u8 as f64,
        Some(Value::Number(n)) => n.as_f64().unwrap_or(f64::NAN),
        Some(Value::String(s)) => {
            let t = js_trim(s);
            if t.is_empty() {
                return 0.0;
            }
            let lower = t.to_ascii_lowercase();
            for (p, radix) in [("0x", 16), ("0o", 8), ("0b", 2)] {
                if let Some(rest) = lower.strip_prefix(p) {
                    return u64::from_str_radix(rest, radix).map(|n| n as f64).unwrap_or(f64::NAN);
                }
            }
            match t {
                "Infinity" | "+Infinity" => return f64::INFINITY,
                "-Infinity" => return f64::NEG_INFINITY,
                _ => {}
            }
            if DEC.get_or_init(|| Regex::new(r"^[+-]?(\d+\.?\d*|\.\d+)([eE][+-]?\d+)?$").unwrap()).is_match(t) {
                t.parse().unwrap_or(f64::NAN)
            } else {
                f64::NAN
            }
        }
    }
}

pub fn normalize_history(env: &mut dyn Env, raw: Option<&Value>) -> Vec<Value> {
    let Some(arr) = raw.and_then(Value::as_array) else { return vec![] };
    let mut out: Vec<(f64, Value)> = Vec::new();
    for e in arr {
        let Some(o) = e.as_object() else { continue };
        let text = field(o, "text").and_then(Value::as_str).map(|s| js_trim(s).to_string()).unwrap_or_default();
        let created = js_number(field(o, "createdAt"));
        if text.is_empty() || !created.is_finite() || created <= 0.0 {
            continue;
        }
        let id = match field(o, "id").and_then(Value::as_str).filter(|s| !js_trim(s).is_empty()) {
            Some(s) => s.to_string(),
            None => {
                let now = env.now();
                env.id("voice", now, "")
            }
        };
        out.push((created, json!({ "id": id, "text": text, "createdAt": jnum(created) })));
    }
    out.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
    out.into_iter().map(|(_, v)| v).collect()
}

pub fn create_history_entry(env: &mut dyn Env, text: &str) -> Option<Value> {
    let t = js_trim(text);
    if t.is_empty() {
        return None;
    }
    let ts = env.now();
    Some(json!({ "id": env.id("voice", ts, ""), "text": t, "createdAt": jnum(ts) }))
}

fn history_text(text: &str) -> String {
    let joined = text.split(|c: char| (c.is_whitespace() && c != '\u{85}') || c == '\u{feff}').filter(|p| !p.is_empty()).collect::<Vec<_>>().join(" ");
    js_trim(&slice_utf16(&joined, MAX_ITEM_CHARS)).to_string()
}

fn estimate_chars(entries: &[Value]) -> usize {
    let texts: Vec<usize> = entries.iter().map(|e| history_text(e["text"].as_str().unwrap_or(""))).filter(|t| !t.is_empty()).map(|t| utf16_len(&t)).collect();
    if texts.is_empty() { 0 } else { texts.iter().fold(utf16_len(HISTORY_HEADER), |a, l| a + 3 + l) }
}

pub fn needs_compaction(entries: &[Value]) -> bool {
    estimate_chars(entries) > COMPACT_CHARS
}

pub fn compact_history(entries: &[Value]) -> Vec<Value> {
    if !needs_compaction(entries) {
        return entries.to_vec();
    }
    let mut recent: Vec<Value> = entries.iter().take(COMPACT_KEEP).cloned().collect();
    while recent.len() > 1 && estimate_chars(&recent) > COMPACT_TARGET {
        recent.pop();
    }
    recent
}

pub fn normalize_snapshot(env: &mut dyn Env, raw: Option<&Value>, platform: &str) -> (Value, Vec<Value>) {
    match raw {
        Some(Value::Object(o)) => (normalize_settings(env, field(o, "settings"), platform), normalize_history(env, field(o, "history"))),
        Some(Value::Array(_)) => (normalize_settings(env, None, platform), vec![]),
        _ => (Value::Object(default_settings(platform)), vec![]),
    }
}
