//! 推送 topic 路由（上游 topics.ts `topicForPush`）。
use serde_json::Value;

const FILE_BROWSER_EVENT_CHANNEL: &str = "maker:file-browser:event";

fn read_string_field<'a>(payload: &'a Value, key: &str) -> Option<&'a str> {
    payload.get(key).and_then(Value::as_str).filter(|s| !s.is_empty())
}

/// 被控端广播（channel + payload）属于哪个 topic；算不出返回 None（不转发）。
pub fn topic_for_push(channel: &str, payload: &Value) -> Option<String> {
    if super::allowlist::is_sessions_channel(channel) {
        return Some("sessions".into());
    }
    match channel {
        FILE_BROWSER_EVENT_CHANNEL => read_string_field(payload, "workdir").map(|w| format!("fs-watch:{w}")),
        "maker:bot-delegation:changed" => read_string_field(payload, "parentSessionId").map(|p| format!("session:{p}")),
        "maker:orca:worker-changed" => read_string_field(payload, "leadSessionId").map(|l| format!("session:{l}")),
        _ => read_string_field(payload, "sessionId").map(|s| format!("session:{s}")),
    }
}

/// 控制端订阅请求里的 topic 是否合法（被控端目前只接受 sessions / session:<id>）。
pub fn is_topic(value: &Value) -> bool {
    match value.as_str() {
        Some("sessions") => true,
        Some(s) => s.starts_with("session:") && s.len() > "session:".len(),
        None => false,
    }
}
