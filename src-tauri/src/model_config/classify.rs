//! 新路由准入：移植 classification.ts 的 categorize / classifyModel / isChatEligible / isAgentSelectableModel /
//! isModelSelectableForNewRoute。纯字符串规则，id 一律先转小写（JS toLowerCase）。
use regex::Regex;
use serde_json::Value;
use std::sync::OnceLock;

const KNOWN: [&str; 18] = [
    "anthropic", "gpt-budget", "gpt", "grok", "google", "china", "ungrouped", "image", "video", "audio", "tts", "stt", "realtime", "embedding", "decision",
    "rerank", "compression", "other",
];
const CHAT_VENDOR: [&str; 7] = ["anthropic", "gpt", "gpt-budget", "grok", "google", "china", "ungrouped"];

fn strip_namespace(id: &str) -> &str {
    id.rfind('/').map_or(id, |i| &id[i + 1..])
}

fn re(cell: &'static OnceLock<Regex>, pat: &str) -> &'static Regex {
    cell.get_or_init(|| Regex::new(pat).unwrap())
}

fn is_codex_gateway_wire_model(id: &str) -> bool {
    let id = crate::local_db::mapper::js_trim(id).to_lowercase();
    ["openai-codex/", "codex/"].iter().any(|p| id.starts_with(p) && id.len() > p.len())
}

pub fn categorize(raw: &str) -> &'static str {
    static EMBED: OnceLock<Regex> = OnceLock::new();
    static IMAGE: OnceLock<Regex> = OnceLock::new();
    static VIDEO: OnceLock<Regex> = OnceLock::new();
    static SORA: OnceLock<Regex> = OnceLock::new();
    static MODER: OnceLock<Regex> = OnceLock::new();
    static RERANK: OnceLock<Regex> = OnceLock::new();
    static LEGACY: OnceLock<Regex> = OnceLock::new();
    static STT: OnceLock<Regex> = OnceLock::new();
    static TTS: OnceLock<Regex> = OnceLock::new();
    static RT: OnceLock<Regex> = OnceLock::new();
    static OMNI: OnceLock<Regex> = OnceLock::new();
    let lower = raw.to_lowercase();
    let id = lower.as_str();
    let tail = strip_namespace(id);
    if id.starts_with("claude-") || tail.starts_with("claude-") {
        return "anthropic";
    }
    if re(&EMBED, "embedding").is_match(id) || id.starts_with("voyage/") || id.starts_with("embed-") || tail.starts_with("embed-") {
        return "embedding";
    }
    if re(&IMAGE, "image").is_match(id) || id.starts_with("dall-e") || tail.starts_with("dall-e") {
        return "image";
    }
    if re(&VIDEO, "seedance|happyhorse|-t2v|-i2v|-r2v").is_match(id) || re(&SORA, "^sora|^veo-").is_match(id) || re(&SORA, "^sora|^veo-").is_match(tail) {
        return "video";
    }
    if id == "ai-gateway-doc" {
        return "compression";
    }
    if re(&MODER, "moderation").is_match(id) {
        return "other";
    }
    if re(&RERANK, "rerank").is_match(id) {
        return "rerank";
    }
    let legacy = re(&LEGACY, r"^(babbage-002|davinci-002|gpt-3\.5-turbo-instruct|text-davinci-\d{3}|text-curie-001|text-babbage-001|text-ada-001|code-davinci-002)$");
    if legacy.is_match(id) || legacy.is_match(tail) {
        return "other";
    }
    if id.starts_with("elevenlabs/scribe") || re(&STT, "transcribe|whisper|asr").is_match(id) {
        return "stt";
    }
    if id.starts_with("elevenlabs/eleven") || re(&TTS, "speech|tts").is_match(id) {
        return "tts";
    }
    let rt = re(&RT, "^gpt-realtime|^gpt-4o-realtime");
    if rt.is_match(id) || rt.is_match(tail) || re(&OMNI, "gemini-omni").is_match(id) {
        return "realtime";
    }
    if is_codex_gateway_wire_model(id) {
        return "gpt-budget";
    }
    if id.starts_with("gpt-") || id.starts_with("chatgpt/") || tail.starts_with("gpt-") {
        return "gpt";
    }
    if id.starts_with("xai/") || id.starts_with("x-ai/") || id.starts_with("grok") || tail.starts_with("grok") {
        return "grok";
    }
    if id.starts_with("gemini-") || tail.starts_with("gemini-") {
        return "google";
    }
    "ungrouped"
}

fn group_of(id: &str, group: Option<&str>) -> &'static str {
    if let Some(g) = group.filter(|g| !g.is_empty()) {
        if let Some(k) = KNOWN.iter().find(|k| **k == g) {
            return k;
        }
    }
    categorize(id)
}

fn is_chat_mode(mode: &str) -> bool {
    mode == "chat" || mode == "responses"
}

pub fn is_chat_eligible(id: &str, group: Option<&str>, mode: Option<&str>) -> bool {
    if let Some(m) = mode {
        return is_chat_mode(m);
    }
    if let Some(g) = group.filter(|g| !g.is_empty()) {
        if KNOWN.contains(&g) && !CHAT_VENDOR.contains(&g) {
            return false;
        }
    }
    CHAT_VENDOR.contains(&categorize(id))
}

pub fn is_agent_selectable_model(id: &str, group: Option<&str>, mode: Option<&str>, user_provider: bool) -> bool {
    if mode.is_some() {
        return is_chat_eligible(id, group, mode);
    }
    if user_provider && group.is_some_and(|g| !g.is_empty() && !KNOWN.contains(&g)) {
        return true;
    }
    let _ = group_of;
    is_chat_eligible(id, group, mode)
}

/// isModelSelectableForNewRoute(model, { userProvider })
pub fn is_model_selectable_for_new_route(model: &Value, user_provider: bool) -> bool {
    model.get("disabled") != Some(&Value::Bool(true))
        && model.get("status").and_then(Value::as_str) != Some("retired")
        && model.get("availability").and_then(Value::as_str) != Some("requires_payment")
        && is_agent_selectable_model(
            model["id"].as_str().unwrap_or(""),
            model.get("group").and_then(Value::as_str),
            model.get("mode").and_then(Value::as_str),
            user_provider,
        )
}
