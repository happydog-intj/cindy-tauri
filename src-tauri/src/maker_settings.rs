//! Maker 侧「覆盖项」设置：sidecar/src/settings/maker-settings.mjs 的 Rust 版。
//! 协作 / 资源 / git 安全 / LSP / 静默加密重试 / 运行时兜底 / 聊天向量 / subagent 模型 / 视觉桥 / 记忆开关。
//! 形状、默认值、校验与原实现一致（原实现逐项对齐 desktop）。这些设置只落盘，运行时（只有 Pi）暂不读取。

use crate::override_file::{OverrideFile, State};
use serde_json::{json, Map, Value};
use std::path::Path;

fn fail<T>(code: &str, msg: impl AsRef<str>) -> Result<T, String> {
    Err(format!("[{code}] {}", msg.as_ref()))
}
fn bool_or(v: &Value, d: bool) -> bool {
    v.as_bool().unwrap_or(d)
}
fn clamp(v: f64, min: f64, max: f64) -> f64 {
    if v.is_finite() { v.max(min).min(max) } else { min }
}
fn num(v: &Value) -> Option<f64> {
    v.as_f64()
}
fn is_int(v: &Value) -> bool {
    v.is_i64() || v.is_u64() || v.as_f64().map(|f| f.fract() == 0.0).unwrap_or(false)
}
fn arg<'a>(args: &'a [Value], i: usize) -> &'a Value {
    args.get(i).unwrap_or(&Value::Null)
}
fn wire(s: &State) -> Value {
    s.wire()
}

const MAX_MODEL_ID: usize = 256;
fn has_control(s: &str) -> bool {
    s.chars().any(|c| (c as u32) <= 31 || c as u32 == 127)
}
/// 磁盘读取的宽松归一化：非法值回退为“不指定”。
fn norm_model_id(v: &Value) -> Value {
    let Some(s) = v.as_str() else { return Value::Null };
    let t = s.trim();
    if t.is_empty() || t.chars().count() > MAX_MODEL_ID || has_control(t) { Value::Null } else { json!(t) }
}
/// IPC 边界的严格校验；空串与 null 都表示“不指定”。
fn valid_model_id_input(v: &Value) -> bool {
    if v.is_null() {
        return true;
    }
    let Some(s) = v.as_str() else { return false };
    let t = s.trim();
    t.is_empty() || (t.chars().count() <= MAX_MODEL_ID && !has_control(t))
}

/// 开关类（enabled / isCustomized / defaultEnabled）。
struct Toggle {
    name: &'static str,
    store: OverrideFile,
    message: &'static str,
    effective: Option<&'static str>,
}

impl Toggle {
    fn new(dir: &Path, name: &'static str, file: &str, default: bool, message: &'static str, effective: Option<&'static str>) -> Self {
        let store = OverrideFile::new(dir.join(file), json!({ "enabled": default }), move |r| json!({ "enabled": bool_or(&r["enabled"], default) }));
        Self { name, store, message, effective }
    }
    fn wire(&self) -> Value {
        let s = self.store.read_state();
        let mut w = json!({ "enabled": s.value["enabled"], "isCustomized": s.is_customized, "defaultEnabled": s.defaults["enabled"] });
        if let Some(e) = self.effective {
            w["effective"] = json!(e);
        }
        w
    }
}

pub struct MakerSettings {
    collab: OverrideFile,
    resource: OverrideFile,
    git: OverrideFile,
    lsp: OverrideFile,
    toggles: Vec<Toggle>,
    subagent: OverrideFile,
    vision: OverrideFile,
    memory: OverrideFile,
}

const GIT_MODES: [&str; 3] = ["off", "existing-git", "all-projects"];
const PRIORITIES: [&str; 3] = ["normal", "low", "lowest"];

fn backend_ref(r: &Value) -> Value {
    if !r.is_object() {
        return Value::Null;
    }
    let p = r["providerId"].as_str().map(str::trim).unwrap_or("");
    let m = r["modelId"].as_str().map(str::trim).unwrap_or("");
    if !p.is_empty() && !m.is_empty() { json!({ "providerId": p, "modelId": m }) } else { Value::Null }
}

impl MakerSettings {
    pub fn new(settings_dir: &Path) -> Self {
        let f = |n: &str| settings_dir.join(n);
        let collab = OverrideFile::new(f("collaboration-settings.json"), json!({ "workerSoftLimit": 5, "workerHardLimit": 8, "workerIdleReleaseMinutes": 0 }), |r| {
            let soft = clamp(num(&r["workerSoftLimit"]).unwrap_or(5.0), 1.0, 20.0);
            json!({
                "workerSoftLimit": soft as i64,
                "workerHardLimit": clamp(num(&r["workerHardLimit"]).unwrap_or(8.0), soft, 20.0) as i64,
                "workerIdleReleaseMinutes": num(&r["workerIdleReleaseMinutes"]).map(|v| clamp(v, 0.0, 120.0) as i64).unwrap_or(0),
            })
        });
        let resource = OverrideFile::new(f("agent-resource-settings.json"), json!({ "maxConcurrentCommands": 0, "processPriority": "normal", "capToolchainThreads": false }), |r| {
            json!({
                "maxConcurrentCommands": num(&r["maxConcurrentCommands"]).filter(|v| v.is_finite()).map(|v| v.floor().max(0.0).min(64.0) as i64).unwrap_or(0),
                "processPriority": match r["processPriority"].as_str() { Some(p @ ("low" | "lowest")) => p, _ => "normal" },
                "capToolchainThreads": r["capToolchainThreads"] == true,
            })
        });
        let git = OverrideFile::new(f("git-safety-settings.json"), json!({ "mode": "existing-git" }), |r| {
            let legacy = r["autoSnapshotEnabled"].as_bool().map(|b| if b { "all-projects" } else { "off" });
            let mode = match legacy {
                Some(l) if r["mode"].is_null() || r["mode"] == "existing-git" => l,
                _ => r["mode"].as_str().filter(|m| GIT_MODES.contains(m)).unwrap_or("existing-git"),
            };
            json!({ "mode": mode })
        })
        .with_merge(|patch, next, _d, mut o| {
            // 显式选过的 mode 即使等于默认值也记为自定义。
            if patch.get("mode").is_some() {
                o.insert("mode".into(), next["mode"].clone());
                o.shift_remove("autoSnapshotEnabled");
            }
            o
        });
        let lsp = OverrideFile::new(f("lsp-mode-settings.json"), json!({ "enabled": false }), |r| json!({ "enabled": bool_or(&r["enabled"], false) }));
        let toggles = vec![
            Toggle::new(settings_dir, "silent-encrypted-retry", "silent-encrypted-retry-settings.json", true, "silent encrypted retry enabled required (boolean)", Some("immediate")),
            Toggle::new(settings_dir, "session-runtime-fallback", "session-runtime-fallback-settings.json", false, "session runtime fallback enabled required (boolean)", Some("immediate")),
        ];
        let subagent = OverrideFile::new(
            f("subagent-model-settings.json"),
            json!({ "claudeCode": null, "claudeCodeProviderId": null, "codexSmartSubagentRouting": false }),
            |r| {
                let cc = norm_model_id(&r["claudeCode"]);
                let provider = if cc.is_null() { Value::Null } else { norm_model_id(&r["claudeCodeProviderId"]) };
                json!({ "claudeCode": cc, "claudeCodeProviderId": provider, "codexSmartSubagentRouting": r["codexSmartSubagentRouting"] == true })
            },
        )
        .with_merge(|patch, next, defaults, o| {
            let keys = ["claudeCode", "claudeCodeProviderId", "codexSmartSubagentRouting"];
            let mut out: Map<String, Value> = o.into_iter().filter(|(k, _)| keys.contains(&k.as_str())).collect();
            for k in patch.as_object().map(|p| p.keys().cloned().collect::<Vec<_>>()).unwrap_or_default() {
                if next[&k] == defaults[&k] { out.shift_remove(&k); } else { out.insert(k.clone(), next[&k].clone()); }
            }
            out
        });
        let vision = OverrideFile::new(f("vision-bridge-settings.json"), json!({ "enabled": false, "targetModels": [], "primary": null, "fallback": null }), |r| {
            let targets: Vec<Value> = r["targetModels"].as_array().map(|a| a.iter().map(|m| m.as_str().map(str::trim).unwrap_or("")).filter(|m| !m.is_empty()).map(|m| json!(m)).collect()).unwrap_or_default();
            json!({ "enabled": bool_or(&r["enabled"], false), "targetModels": targets, "primary": backend_ref(&r["primary"]), "fallback": backend_ref(&r["fallback"]) })
        });
        let memory = OverrideFile::new(f("memory-settings.json"), json!({ "maker": true, "claudeCode": true, "codex": true, "pi": true }), |r| {
            json!({ "maker": bool_or(&r["maker"], true), "claudeCode": bool_or(&r["claudeCode"], true), "codex": bool_or(&r["codex"], true), "pi": bool_or(&r["pi"], true) })
        });
        Self { collab, resource, git, lsp, toggles, subagent, vision, memory }
    }

    /// 返回 None 表示不是本模块的通道。
    pub fn invoke(&self, channel: &str, args: &[Value]) -> Option<Result<Value, String>> {
        let a = |i| arg(args, i);
        let r = match channel {
            "maker:collaboration-settings:get" => Ok(wire(&self.collab.read_state())),
            "maker:collaboration-settings:set" => self.collab_set(a(0)),
            "maker:collaboration-settings:reset" => self.collab.reset().map(|_| wire(&self.collab.read_state())),
            "maker:agent-resource-settings:get" => Ok(wire(&self.resource.read_state())),
            "maker:agent-resource-settings:set" => self.resource_set(a(0)),
            "maker:agent-resource-settings:reset" => self.resource.reset().map(|_| wire(&self.resource.read_state())),
            "maker:git-safety:get" => Ok(self.git_wire()),
            "maker:git-safety:set" => self.git_set(a(0)),
            "maker:git-safety:reset" => self.git.reset().map(|_| self.git_wire()),
            "maker:lsp-mode:get" => Ok(self.lsp.read()),
            "maker:lsp-mode:set" => match a(0).as_bool() {
                None => fail("INVALID_PARAMS", "lsp mode enabled required (boolean)"),
                Some(b) => self.lsp.write_patch(&json!({ "enabled": b }), true).map(|_| json!({ "effective": "next-session" })),
            },
            "maker:chat-embedding:get" | "maker:chat-embedding:reset" => Ok(Self::chat_embedding_wire()),
            "maker:chat-embedding:set" => match a(0).as_bool() {
                None => fail("INVALID_PARAMS", "chat embedding enabled required (boolean)"),
                // 本机模式没有云端组织账号，向量始终不可用。
                Some(true) => fail("UNSUPPORTED_CAPABILITY", "Chat embedding is not available for this account or region."),
                Some(false) => Ok(Self::chat_embedding_wire()),
            },
            "maker:subagent-model-settings:get" => Ok(wire(&self.subagent.read_state())),
            "maker:subagent-model-settings:set" => self.subagent_set(a(0)),
            "maker:subagent-model-settings:reset" => self.subagent.reset().map(|_| with_deferred(wire(&self.subagent.read_state()))),
            "maker:vision-bridge-settings:get" => Ok(wire(&self.vision.read_state())),
            "maker:vision-bridge-settings:set" => self.vision_set(a(0)),
            "maker:vision-bridge-settings:reset" => self.vision.reset().map(|_| wire(&self.vision.read_state())),
            "maker:memory:get-settings" => Ok(self.memory.read()),
            "maker:memory:get-settings-state" => Ok(wire(&self.memory.read_state())),
            "maker:memory:preserve-legacy-maker-disabled" => self.memory_preserve_legacy(a(0)),
            "maker:memory:get" => require_agent_kind(a(0)).map(|_| {
                let customized = self.memory.read_state().customized_keys.iter().any(|k| k == "pi");
                json!({ "enabled": false, "source": if customized { "host-runtime" } else { "agent-default" } })
            }),
            "maker:memory:set" => self.memory_set(a(0), a(1)),
            "maker:memory:reset" => require_agent_kind(a(0)).map(|_| json!({ "removedEntries": 0 })),
            "maker:memory:reset-settings" => self.memory.reset().map(|_| with_deferred(wire(&self.memory.read_state()))),
            "maker:maker-memory:set-enabled" => match a(0).as_bool() {
                None => fail("INVALID_PARAMS", "enabled required (boolean)"),
                Some(_) => fail("MAKER_MEMORY_NOT_READY", "maker memory not initialized"),
            },
            "maker:maker-memory:reset" => fail("MAKER_MEMORY_NOT_READY", "maker memory not initialized"),
            other => return self.toggle_invoke(other, a(0)),
        };
        Some(r)
    }

    fn toggle_invoke(&self, channel: &str, v: &Value) -> Option<Result<Value, String>> {
        let rest = channel.strip_prefix("maker:")?;
        let (name, op) = rest.rsplit_once(':')?;
        let t = self.toggles.iter().find(|t| t.name == name)?;
        Some(match op {
            "get" => Ok(t.wire()),
            "set" => match v.as_bool() {
                None => fail("INVALID_PARAMS", t.message),
                Some(b) => t.store.write_patch(&json!({ "enabled": b }), false).map(|_| t.wire()),
            },
            "reset" => t.store.reset().map(|_| t.wire()),
            _ => return None,
        })
    }

    fn chat_embedding_wire() -> Value {
        json!({ "enabled": false, "isCustomized": false, "defaultEnabled": false })
    }

    fn collab_set(&self, body: &Value) -> Result<Value, String> {
        let Some(key) = body["key"].as_str() else { return fail("INVALID_PARAMS", "key required") };
        if !["workerSoftLimit", "workerHardLimit", "workerIdleReleaseMinutes"].contains(&key) {
            return fail("INVALID_PARAMS", format!("unknown key: {key}"));
        }
        let value = &body["value"];
        if !value.is_number() || !is_int(value) {
            return fail("INVALID_PARAMS", format!("{key} must be an integer"));
        }
        let v = value.as_i64().unwrap_or(0);
        let cur = self.collab.read();
        if key == "workerIdleReleaseMinutes" {
            if v < 0 { return fail("INVALID_PARAMS", format!("{key} must be >= 0")); }
            if v > 120 { return fail("INVALID_PARAMS", format!("{key} must be <= 120")); }
        } else {
            if v < 1 { return fail("INVALID_PARAMS", format!("{key} must be >= 1")); }
            if v > 20 { return fail("INVALID_PARAMS", format!("{key} must be <= 20")); }
            if key == "workerSoftLimit" && v > cur["workerHardLimit"].as_i64().unwrap_or(8) {
                return fail("INVALID_PARAMS", "workerSoftLimit must be <= workerHardLimit");
            }
            if key == "workerHardLimit" && v < cur["workerSoftLimit"].as_i64().unwrap_or(5) {
                return fail("INVALID_PARAMS", "workerHardLimit must be >= workerSoftLimit");
            }
        }
        self.collab.write_patch(&json!({ key: v }), false).map(|s| wire(&s))
    }

    fn resource_set(&self, body: &Value) -> Result<Value, String> {
        let Some(key) = body["key"].as_str() else { return fail("INVALID_PARAMS", "key required") };
        let value = &body["value"];
        match key {
            "maxConcurrentCommands" => {
                if !value.is_number() || !is_int(value) { return fail("INVALID_PARAMS", format!("{key} must be an integer")); }
                let v = value.as_i64().unwrap_or(0);
                if v < 0 { return fail("INVALID_PARAMS", format!("{key} must be >= 0")); }
                if v > 64 { return fail("INVALID_PARAMS", format!("{key} must be <= 64")); }
            }
            "processPriority" => {
                if !value.as_str().map(|s| PRIORITIES.contains(&s)).unwrap_or(false) {
                    return fail("INVALID_PARAMS", format!("{key} must be one of {}", PRIORITIES.join("/")));
                }
            }
            "capToolchainThreads" => {
                if !value.is_boolean() { return fail("INVALID_PARAMS", format!("{key} must be a boolean")); }
            }
            _ => return fail("INVALID_PARAMS", format!("unknown key: {key}")),
        }
        self.resource.write_patch(&json!({ key: value }), false).map(|s| wire(&s))
    }

    fn git_wire(&self) -> Value {
        let s = self.git.read_state();
        let mode = s.value["mode"].as_str().unwrap_or("existing-git");
        let default_mode = s.defaults["mode"].as_str().unwrap_or("existing-git");
        json!({
            "mode": mode,
            "autoSnapshotEnabled": mode != "off",
            "autoInitProjectGit": mode == "all-projects",
            "isCustomized": s.is_customized,
            "defaultMode": default_mode,
            "defaultAutoSnapshotEnabled": default_mode != "off",
        })
    }

    fn git_set(&self, v: &Value) -> Result<Value, String> {
        let mode = match v {
            Value::Bool(b) => Some(if *b { "all-projects" } else { "off" }),
            Value::String(s) => GIT_MODES.iter().find(|m| **m == s).copied(),
            _ => None,
        };
        let Some(mode) = mode else { return fail("INVALID_PARAMS", "git safety mode required") };
        self.git.write_patch(&json!({ "mode": mode }), false)?;
        Ok(self.git_wire())
    }

    fn subagent_set(&self, raw: &Value) -> Result<Value, String> {
        let Some(obj) = raw.as_object() else { return fail("INVALID_PARAMS", "subagent model settings patch required (object)") };
        let mut patch = Map::new();
        for key in ["claudeCode", "claudeCodeProviderId"] {
            let Some(v) = obj.get(key) else { continue };
            if !valid_model_id_input(v) {
                return fail("INVALID_PARAMS", format!("subagent model {key} must be a valid string or null"));
            }
            patch.insert(key.into(), norm_model_id(v));
        }
        if let Some(v) = obj.get("codexSmartSubagentRouting") {
            if !v.is_boolean() {
                return fail("INVALID_PARAMS", "subagent codexSmartSubagentRouting must be boolean");
            }
            patch.insert("codexSmartSubagentRouting".into(), v.clone());
        }
        // 来源依附于模型：有效模型为 null 时，来源强制清为 null（对齐 reconcileSubagentModelSettingsPatch）。
        let current = self.subagent.read();
        let effective_model = patch.get("claudeCode").cloned().unwrap_or_else(|| current["claudeCode"].clone());
        if effective_model.is_null() {
            let effective_provider = patch.get("claudeCodeProviderId").cloned().unwrap_or_else(|| current["claudeCodeProviderId"].clone());
            if !effective_provider.is_null() || patch.contains_key("claudeCodeProviderId") {
                patch.insert("claudeCodeProviderId".into(), Value::Null);
            }
        }
        self.subagent.write_patch(&Value::Object(patch), false).map(|s| with_deferred(wire(&s)))
    }

    fn vision_set(&self, raw: &Value) -> Result<Value, String> {
        let Some(obj) = raw.as_object() else { return fail("INVALID_PARAMS", "vision bridge settings patch required (object)") };
        let mut patch = Map::new();
        if let Some(v) = obj.get("enabled") {
            if !v.is_boolean() { return fail("INVALID_PARAMS", "vision bridge enabled must be boolean"); }
            patch.insert("enabled".into(), v.clone());
        }
        if let Some(v) = obj.get("targetModels") {
            let Some(arr) = v.as_array() else { return fail("INVALID_PARAMS", "vision bridge targetModels must be a string array") };
            let trimmed: Vec<String> = arr.iter().map(|m| m.as_str().map(|s| s.trim().to_string()).unwrap_or_default()).collect();
            if trimmed.iter().any(|m| m.is_empty()) {
                return fail("INVALID_PARAMS", "vision bridge targetModels must be a non-blank string array");
            }
            patch.insert("targetModels".into(), json!(trimmed));
        }
        for key in ["primary", "fallback"] {
            let Some(v) = obj.get(key) else { continue };
            if v.is_null() {
                patch.insert(key.into(), Value::Null);
                continue;
            }
            let (Some(p), Some(m)) = (v["providerId"].as_str(), v["modelId"].as_str()) else {
                return fail("INVALID_PARAMS", format!("vision bridge {key} must be {{ providerId, modelId }} or null"));
            };
            if !v.is_object() {
                return fail("INVALID_PARAMS", format!("vision bridge {key} must be {{ providerId, modelId }} or null"));
            }
            let (p, m) = (p.trim(), m.trim());
            if p.is_empty() || m.is_empty() {
                return fail("INVALID_PARAMS", format!("vision bridge {key} providerId/modelId must not be blank"));
            }
            patch.insert(key.into(), json!({ "providerId": p, "modelId": m }));
        }
        self.vision.write_patch(&Value::Object(patch), false).map(|s| wire(&s))
    }

    fn memory_preserve_legacy(&self, legacy: &Value) -> Result<Value, String> {
        let state = self.memory.read_state();
        if state.customized_keys.iter().any(|k| k == "maker") {
            return Ok(state.value);
        }
        match legacy.as_bool() {
            Some(true) => self.memory.write_patch(&json!({ "maker": true }), true).map(|s| s.value),
            Some(false) => self.memory.write_patch(&json!({ "maker": false }), false).map(|s| s.value),
            None => Ok(state.value),
        }
    }

    fn memory_set(&self, kind: &Value, enabled: &Value) -> Result<Value, String> {
        require_agent_kind(kind)?;
        let Some(b) = enabled.as_bool() else { return fail("INVALID_PARAMS", "enabled required (boolean)") };
        let s = self.memory.write_patch(&json!({ "pi": b }), false)?;
        Ok(json!({ "effective": "next-session", "isCustomized": s.is_customized, "customizedKeys": s.customized_keys, "defaults": s.defaults }))
    }
}

/// Tauri 版没有 Codex 进程，codexRestartDeferred 恒为 false。
fn with_deferred(mut v: Value) -> Value {
    v["codexRestartDeferred"] = json!(false);
    v
}

fn require_agent_kind(kind: &Value) -> Result<(), String> {
    match kind.as_str() {
        Some("claude-code" | "codex") => Err(format!("Agent '{}' is not registered (available: pi)", kind.as_str().unwrap())),
        Some("pi") => Ok(()),
        _ => fail("INVALID_PARAMS", format!("agentKind required (claude-code | codex | pi), got {}", match kind { Value::Null => "undefined".to_string(), Value::String(s) => s.clone(), o => o.to_string() })),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings(name: &str) -> (MakerSettings, std::path::PathBuf) {
        let d = std::env::temp_dir().join(format!("maker-settings-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        (MakerSettings::new(&d), d)
    }
    fn call(s: &MakerSettings, ch: &str, args: Value) -> Result<Value, String> {
        s.invoke(ch, args.as_array().unwrap()).expect("channel owned")
    }

    #[test]
    fn collaboration_validates_ranges_and_persists_only_differences() {
        let (s, d) = settings("collab");
        let w = call(&s, "maker:collaboration-settings:get", json!([])).unwrap();
        assert_eq!((w["workerSoftLimit"].clone(), w["isCustomized"].clone()), (json!(5), json!(false)));
        let w = call(&s, "maker:collaboration-settings:set", json!([{ "key": "workerSoftLimit", "value": 3 }])).unwrap();
        assert_eq!(w["workerSoftLimit"], 3);
        assert_eq!(w["customizedKeys"], json!(["workerSoftLimit"]));
        assert_eq!(serde_json::from_str::<Value>(&std::fs::read_to_string(d.join("collaboration-settings.json")).unwrap()).unwrap(), json!({ "workerSoftLimit": 3 }));
        // 软上限不能超过硬上限；越界、非整数、未知键都拒绝。
        assert!(call(&s, "maker:collaboration-settings:set", json!([{ "key": "workerSoftLimit", "value": 15 }])).unwrap_err().contains("INVALID_PARAMS"));
        assert!(call(&s, "maker:collaboration-settings:set", json!([{ "key": "workerHardLimit", "value": 2 }])).unwrap_err().contains("must be >= workerSoftLimit"));
        assert!(call(&s, "maker:collaboration-settings:set", json!([{ "key": "workerSoftLimit", "value": 1.5 }])).unwrap_err().contains("integer"));
        assert!(call(&s, "maker:collaboration-settings:set", json!([{ "key": "x", "value": 1 }])).unwrap_err().contains("unknown key"));
        assert!(call(&s, "maker:collaboration-settings:set", json!(["nope"])).unwrap_err().contains("key required"));
        // 回到默认值：覆盖项删除，文件随之删除。
        call(&s, "maker:collaboration-settings:set", json!([{ "key": "workerSoftLimit", "value": 5 }])).unwrap();
        assert!(!d.join("collaboration-settings.json").exists());
        // 重启后读回。
        call(&s, "maker:collaboration-settings:set", json!([{ "key": "workerIdleReleaseMinutes", "value": 30 }])).unwrap();
        let again = MakerSettings::new(&d);
        assert_eq!(call(&again, "maker:collaboration-settings:get", json!([])).unwrap()["workerIdleReleaseMinutes"], 30);
        assert_eq!(call(&again, "maker:collaboration-settings:reset", json!([])).unwrap()["isCustomized"], false);
    }

    #[test]
    fn resource_and_git_safety_follow_the_desktop_wire_shapes() {
        let (s, _) = settings("resource");
        assert!(call(&s, "maker:agent-resource-settings:set", json!([{ "key": "processPriority", "value": "high" }])).unwrap_err().contains("normal/low/lowest"));
        assert!(call(&s, "maker:agent-resource-settings:set", json!([{ "key": "maxConcurrentCommands", "value": 65 }])).unwrap_err().contains("<= 64"));
        let w = call(&s, "maker:agent-resource-settings:set", json!([{ "key": "processPriority", "value": "low" }])).unwrap();
        assert_eq!((w["processPriority"].clone(), w["maxConcurrentCommands"].clone()), (json!("low"), json!(0)));

        let g = call(&s, "maker:git-safety:get", json!([])).unwrap();
        assert_eq!((g["mode"].clone(), g["autoSnapshotEnabled"].clone(), g["autoInitProjectGit"].clone()), (json!("existing-git"), json!(true), json!(false)));
        // 布尔旧形状映射到 mode；显式选默认值也算自定义。
        assert_eq!(call(&s, "maker:git-safety:set", json!([false])).unwrap()["mode"], "off");
        let g = call(&s, "maker:git-safety:set", json!(["existing-git"])).unwrap();
        assert_eq!(g["isCustomized"], true);
        assert!(call(&s, "maker:git-safety:set", json!(["weird"])).unwrap_err().contains("git safety mode required"));
        assert_eq!(call(&s, "maker:git-safety:reset", json!([])).unwrap()["isCustomized"], false);
    }

    #[test]
    fn toggles_lsp_and_chat_embedding() {
        let (s, _) = settings("toggle");
        let w = call(&s, "maker:silent-encrypted-retry:get", json!([])).unwrap();
        assert_eq!((w["enabled"].clone(), w["defaultEnabled"].clone()), (json!(true), json!(true)));
        let w = call(&s, "maker:silent-encrypted-retry:set", json!([false])).unwrap();
        assert_eq!((w["enabled"].clone(), w["isCustomized"].clone(), w["effective"].clone()), (json!(false), json!(true), json!("immediate")));
        assert!(call(&s, "maker:silent-encrypted-retry:set", json!(["yes"])).unwrap_err().contains("INVALID_PARAMS"));
        assert_eq!(call(&s, "maker:session-runtime-fallback:get", json!([])).unwrap()["enabled"], false);
        assert_eq!(call(&s, "maker:silent-encrypted-retry:reset", json!([])).unwrap()["enabled"], true);

        assert_eq!(call(&s, "maker:lsp-mode:set", json!([true])).unwrap(), json!({ "effective": "next-session" }));
        assert_eq!(call(&s, "maker:lsp-mode:get", json!([])).unwrap(), json!({ "enabled": true }));
        assert!(call(&s, "maker:lsp-mode:set", json!(["x"])).is_err());

        assert_eq!(call(&s, "maker:chat-embedding:get", json!([])).unwrap()["enabled"], false);
        assert!(call(&s, "maker:chat-embedding:set", json!([true])).unwrap_err().contains("UNSUPPORTED_CAPABILITY"));
        assert_eq!(call(&s, "maker:chat-embedding:set", json!([false])).unwrap()["enabled"], false);
    }

    #[test]
    fn subagent_settings_clear_an_orphan_provider_and_reject_bad_input() {
        let (s, _) = settings("subagent");
        assert!(call(&s, "maker:subagent-model-settings:set", json!(["x"])).unwrap_err().contains("patch required"));
        assert!(call(&s, "maker:subagent-model-settings:set", json!([{ "claudeCode": 5 }])).unwrap_err().contains("valid string or null"));
        assert!(call(&s, "maker:subagent-model-settings:set", json!([{ "claudeCode": "a\u{1}b" }])).is_err());
        assert!(call(&s, "maker:subagent-model-settings:set", json!([{ "codexSmartSubagentRouting": "yes" }])).is_err());
        // 没有模型时，只带来源的补丁不会留下孤儿来源。
        let w = call(&s, "maker:subagent-model-settings:set", json!([{ "claudeCodeProviderId": "p1" }])).unwrap();
        assert_eq!((w["claudeCodeProviderId"].clone(), w["isCustomized"].clone(), w["codexRestartDeferred"].clone()), (Value::Null, json!(false), json!(false)));
        let w = call(&s, "maker:subagent-model-settings:set", json!([{ "claudeCode": " opus ", "claudeCodeProviderId": "p1" }])).unwrap();
        assert_eq!((w["claudeCode"].clone(), w["claudeCodeProviderId"].clone()), (json!("opus"), json!("p1")));
        // 清掉模型时来源一并清掉。
        let w = call(&s, "maker:subagent-model-settings:set", json!([{ "claudeCode": "" }])).unwrap();
        assert_eq!((w["claudeCode"].clone(), w["claudeCodeProviderId"].clone(), w["isCustomized"].clone()), (Value::Null, Value::Null, json!(false)));
    }

    #[test]
    fn vision_bridge_validates_backends_and_normalizes_on_read() {
        let (s, _) = settings("vision");
        assert!(call(&s, "maker:vision-bridge-settings:set", json!([{ "enabled": "yes" }])).unwrap_err().contains("must be boolean"));
        assert!(call(&s, "maker:vision-bridge-settings:set", json!([{ "targetModels": ["a", " "] }])).unwrap_err().contains("non-blank"));
        assert!(call(&s, "maker:vision-bridge-settings:set", json!([{ "primary": { "providerId": "p" } }])).unwrap_err().contains("providerId, modelId"));
        assert!(call(&s, "maker:vision-bridge-settings:set", json!([{ "primary": { "providerId": " ", "modelId": "m" } }])).unwrap_err().contains("blank"));
        let w = call(&s, "maker:vision-bridge-settings:set", json!([{ "enabled": true, "targetModels": [" m1 "], "primary": { "providerId": " p ", "modelId": "m" } }])).unwrap();
        assert_eq!(w["targetModels"], json!(["m1"]));
        assert_eq!(w["primary"], json!({ "providerId": "p", "modelId": "m" }));
        assert_eq!(w["fallback"], Value::Null);
        assert_eq!(call(&s, "maker:vision-bridge-settings:reset", json!([])).unwrap()["enabled"], false);
    }

    #[test]
    fn memory_settings_only_support_pi_and_report_the_maker_memory_as_not_ready() {
        let (s, _) = settings("memory");
        assert_eq!(call(&s, "maker:memory:get-settings", json!([])).unwrap()["pi"], true);
        assert_eq!(call(&s, "maker:memory:get", json!(["pi"])).unwrap(), json!({ "enabled": false, "source": "agent-default" }));
        assert!(call(&s, "maker:memory:get", json!(["codex"])).unwrap_err().contains("not registered"));
        assert!(call(&s, "maker:memory:get", json!(["zzz"])).unwrap_err().contains("INVALID_PARAMS"));
        let r = call(&s, "maker:memory:set", json!(["pi", false])).unwrap();
        assert_eq!((r["effective"].clone(), r["isCustomized"].clone(), r["customizedKeys"].clone()), (json!("next-session"), json!(true), json!(["pi"])));
        assert_eq!(call(&s, "maker:memory:get", json!(["pi"])).unwrap()["source"], "host-runtime");
        assert!(call(&s, "maker:memory:set", json!(["pi", "x"])).is_err());
        // 旧版 maker 记忆被关闭：保留；已显式设置过则不覆盖。
        assert_eq!(call(&s, "maker:memory:preserve-legacy-maker-disabled", json!([false])).unwrap()["maker"], false);
        assert_eq!(call(&s, "maker:memory:preserve-legacy-maker-disabled", json!([true])).unwrap()["maker"], false);
        assert!(call(&s, "maker:maker-memory:set-enabled", json!([true])).unwrap_err().contains("MAKER_MEMORY_NOT_READY"));
        assert!(call(&s, "maker:maker-memory:reset", json!([])).unwrap_err().contains("MAKER_MEMORY_NOT_READY"));
        let w = call(&s, "maker:memory:reset-settings", json!([])).unwrap();
        assert_eq!((w["isCustomized"].clone(), w["codexRestartDeferred"].clone()), (json!(false), json!(false)));
    }

    #[test]
    fn unknown_channels_are_not_owned() {
        let (s, _) = settings("unknown");
        assert!(s.invoke("maker:create-session", &[]).is_none());
        assert!(s.invoke("maker:memory-something:get", &[]).is_none());
    }
}
