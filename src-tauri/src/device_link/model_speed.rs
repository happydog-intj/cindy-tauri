//! 推送给控制端前的模型速度投影（上游 sessionListTransport.ts `encodeModelSpeedPayload`）：
//! `ultrafast` 档对老控制端降级成布尔，并把精确档位放进 `*Tier(s)` 附加字段。
use serde_json::{Map, Value};

const CONTAINERS: &[&str] = &[
    "sessions", "session", "items", "value", "patch", "state", "runtime", "runtimeBaseline", "runtimeEffective", "pendingMutation",
    "profile", "effectiveProfile", "baseline", "modelChain", "modelChainOverride", "modelOverride", "favorites", "capabilities",
    "claudeCode", "codex", "pi",
];

/// `^(?:local-db:(?:sessions|bots):|maker:(?:list-active$|get-new-maker-defaults$|new-maker-draft:changed$|model-favorites:|schedule:|session-model-pref:|session-sync$))`
fn speed_channel(channel: &str) -> bool {
    if channel.starts_with("local-db:sessions:") || channel.starts_with("local-db:bots:") {
        return true;
    }
    let Some(rest) = channel.strip_prefix("maker:") else { return false };
    matches!(rest, "list-active" | "get-new-maker-defaults" | "new-maker-draft:changed" | "session-sync")
        || rest.starts_with("model-favorites:")
        || rest.starts_with("schedule:")
        || rest.starts_with("session-model-pref:")
}

pub fn encode_model_speed_payload(channel: &str, value: Value) -> Value {
    if !channel.is_empty() && speed_channel(channel) { map(value, 0) } else { value }
}

fn map(value: Value, depth: u32) -> Value {
    if depth > 16 {
        return value;
    }
    match value {
        Value::Array(items) => Value::Array(items.into_iter().map(|i| map(i, depth + 1)).collect()),
        Value::Object(mut o) => {
            for key in ["fastMode", "fast"] {
                if o.get(key).and_then(Value::as_str) == Some("ultrafast") {
                    set(&mut o, key, Value::Bool(true));
                    set(&mut o, &format!("{key}Tier"), Value::String("ultrafast".into()));
                }
            }
            for key in ["fastModeByModel", "fastByModel"] {
                let Some(values) = o.get(key).and_then(Value::as_object).cloned() else { continue };
                let tier_key = format!("{key}Tiers");
                let mut mapped = values.clone();
                let mut tiers = o.get(&tier_key).and_then(Value::as_object).cloned().unwrap_or_default();
                let mut changed = false;
                for (model, speed) in &values {
                    if speed.as_str() == Some("ultrafast") {
                        mapped.insert(model.clone(), Value::Bool(true));
                        tiers.insert(model.clone(), Value::String("ultrafast".into()));
                        changed = true;
                    }
                }
                if changed {
                    set(&mut o, key, Value::Object(mapped));
                    if !tiers.is_empty() {
                        set(&mut o, &tier_key, Value::Object(tiers));
                    } else {
                        o.shift_remove(&tier_key);
                    }
                }
            }
            if let Some(memory) = o.get("providerModelMemory").and_then(Value::as_object).cloned() {
                let slots: Map<String, Value> = memory.iter().map(|(k, v)| (k.clone(), map(v.clone(), depth + 1))).collect();
                if slots != memory {
                    set(&mut o, "providerModelMemory", Value::Object(slots));
                }
            }
            for key in CONTAINERS {
                if let Some(child) = o.get(*key).cloned() {
                    let mapped = map(child.clone(), depth + 1);
                    if mapped != child {
                        set(&mut o, key, mapped);
                    }
                }
            }
            Value::Object(o)
        }
        other => other,
    }
}

/// 覆盖已有键保持原位置，新键追加到末尾（与 JS 对象的键序一致）。
fn set(o: &mut Map<String, Value>, key: &str, v: Value) {
    o.insert(key.to_owned(), v);
}
