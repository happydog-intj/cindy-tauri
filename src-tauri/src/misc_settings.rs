//! 其余设置页通道：Pi 扩展包 / Pi 内核、用量历史、Bot 模型链、辅助模型。
//! sidecar/src/settings/misc-settings.mjs 的 Rust 版（含上下文压缩阈值，写入要校验账号 owner）。
//! 形状与校验对齐 desktop；依赖的上游规则（modelRoute / botModelChain / auxiliaryModelSettings /
//! catalogModelPin / utilityModelProfiles 的档位名）在这里重写，之后需要手动对齐上游。

use crate::override_file::OverrideFile;
use regex::Regex;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

/// 随包 pi-rs 的版本号，与 pi-rs/Cargo.toml 保持一致（见下方单测）。
pub const PI_RS_VERSION: &str = "0.1.0";

fn fail<T>(code: &str, msg: &str) -> Result<T, String> {
    Err(format!("[{code}] {msg}"))
}
fn utf16_len(s: &str) -> usize {
    s.encode_utf16().count()
}
fn has_control(s: &str) -> bool {
    s.chars().any(|c| (c as u32) <= 31 || c as u32 == 127)
}
/// IPC 编码里的 undefined（`{"__cindy_t":"u"}`）；缺参数同样视为 undefined。
fn is_undefined(v: Option<&Value>) -> bool {
    match v {
        None => true,
        Some(o) => o.get("__cindy_t").and_then(Value::as_str) == Some("u"),
    }
}

// ── 上游规则：modelRoute ───────────────────────────────────────────────────────

const REMOTE_SOURCE_PREFIX: &str = "model-route:";
const BOT_MODEL_CHAIN_MAX: usize = 5;

/// packages/maker-shared/src/modelRoute.ts normalizeModelRoute。
fn normalize_model_route(value: &Value) -> Option<Value> {
    let mut rec = value.as_object()?.clone();
    if let Some(pid) = rec.get("providerId").and_then(Value::as_str).filter(|p| p.starts_with(REMOTE_SOURCE_PREFIX)) {
        let source: Value = serde_json::from_str(&pid[REMOTE_SOURCE_PREFIX.len()..]).ok()?;
        let arr = source.as_array().filter(|a| a.len() == 2)?;
        let device = arr[0].as_str()?;
        if !(arr[1].is_null() || arr[1].is_string()) {
            return None;
        }
        match rec.get("agentDeviceId") {
            Some(d) if !d.is_null() && d.as_str() != Some(device) => return None,
            _ => {}
        }
        rec.insert("agentDeviceId".into(), json!(device));
        rec.insert("providerId".into(), arr[1].clone());
    }
    let model = rec.get("model").and_then(Value::as_str).map(str::trim).filter(|m| !m.is_empty())?.to_string();
    if let Some(d) = rec.get("agentDeviceId").filter(|d| !d.is_null()) {
        let ok = d.as_str().map(|s| Regex::new(r"^[A-Za-z0-9_.:-]{1,128}$").unwrap().is_match(s)).unwrap_or(false);
        if !ok {
            return None;
        }
    }
    let harness = match rec.get("harness").and_then(Value::as_str) {
        Some(h @ ("codex" | "pi")) => h,
        _ => "claude",
    };
    let provider = rec.get("providerId").and_then(Value::as_str).map(str::trim).filter(|p| !p.is_empty());
    let fast = if rec.get("fastMode").and_then(Value::as_str) == Some("ultrafast") { json!("ultrafast") } else { json!(rec.get("fastMode") == Some(&json!(true))) };
    let mut out = json!({
        "harness": harness,
        "model": model,
        "providerId": provider,
        "effort": rec.get("effort").and_then(Value::as_str).unwrap_or(""),
        "fastMode": fast,
    });
    if let Some(d) = rec.get("agentDeviceId").and_then(Value::as_str) {
        out["agentDeviceId"] = json!(d.trim());
    }
    Some(out)
}

/// botModelChain.ts normalizeBotModelChain（不带 legacy）：去重、最多 5 条。
fn normalize_bot_chain(value: &Value) -> Vec<Value> {
    let mut seen = std::collections::HashSet::new();
    value
        .as_array()
        .map(|a| a.iter().filter_map(normalize_model_route).collect::<Vec<_>>())
        .unwrap_or_default()
        .into_iter()
        .filter(|r| {
            let key = format!(
                "{}\0{}\0{}{}",
                r["harness"].as_str().unwrap_or(""),
                r["providerId"].as_str().unwrap_or(""),
                r["model"].as_str().unwrap_or(""),
                r["agentDeviceId"].as_str().filter(|d| !d.is_empty()).map(|d| format!("\0{d}")).unwrap_or_default()
            );
            seen.insert(key)
        })
        .take(BOT_MODEL_CHAIN_MAX)
        .collect()
}

// ── 上游规则：辅助模型引用 ──────────────────────────────────────────────────────

const AUX_CHAIN_MAX: usize = 3;
const AUX_PIN_MAX_LEN: usize = 768;
/// utilityModelProfiles.ts UTILITY_MODEL_PROFILES 的档位名（只用来判断 key 是否合法）。
const UTILITY_PROFILE_KINDS: [&str; 9] = [
    "codex-gpt-5.4-mini", "codex-gpt-5.4-nano", "litellm-gpt-5.4-mini", "litellm-gpt-5.4-nano", "litellm-deepseek-v4-flash",
    "litellm-qwen3.6-plus", "litellm-qwen3.7-max", "litellm-glm-5.1", "litellm-kimi-k2.6",
];

fn is_utility_kind(v: &str) -> bool {
    UTILITY_PROFILE_KINDS.contains(&v)
}

/// JS decodeURIComponent：非法 % 序列或非法 UTF-8 返回 None。
fn decode_uri_component(s: &str) -> Option<String> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' {
            let hex = s.get(i + 1..i + 3)?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

/// catalogModelPin.ts decodeCatalogModelPin：`cat:<encoded provider>:<agent>:<model>`。
fn is_catalog_pin(raw: &str) -> bool {
    let Some(rest) = raw.strip_prefix("cat:") else { return false };
    let Some(first) = rest.find(':').filter(|i| *i > 0) else { return false };
    let Some(second) = rest[first + 1..].find(':').map(|i| i + first + 1).filter(|i| *i > first + 1) else { return false };
    let provider = decode_uri_component(&rest[..first]);
    let agent = &rest[first + 1..second];
    let model = &rest[second + 1..];
    provider.map(|p| !p.is_empty()).unwrap_or(false) && !model.is_empty() && ["codex", "claude-code", "pi"].contains(&agent)
}

fn normalize_aux_ref(v: &Value) -> Option<String> {
    let t = v.as_str()?.trim();
    if t.is_empty() || utf16_len(t) > AUX_PIN_MAX_LEN || has_control(t) || !(is_utility_kind(t) || is_catalog_pin(t)) {
        return None;
    }
    Some(t.to_string())
}
fn normalize_aux_list(v: &Value) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for e in v.as_array().map(Vec::as_slice).unwrap_or(&[]) {
        if let Some(r) = normalize_aux_ref(e) {
            if !out.contains(&r) {
                out.push(r);
                if out.len() >= AUX_CHAIN_MAX {
                    break;
                }
            }
        }
    }
    out
}
fn is_valid_aux_list_input(v: &Value) -> bool {
    let Some(a) = v.as_array().filter(|a| a.len() <= AUX_CHAIN_MAX) else { return false };
    let mut seen = std::collections::HashSet::new();
    a.iter().all(|e| e.as_str().is_some() && normalize_aux_ref(e).as_deref() == e.as_str() && seen.insert(e.as_str().unwrap().to_string()))
}

// ── 模块本体 ─────────────────────────────────────────────────────────────────

pub struct MiscSettings {
    bot_chain: OverrideFile,
    aux: OverrideFile,
    pi_binary: Option<PathBuf>,
    /// 上下文压缩阈值（Claude Code / Pi 各一份）：写入要校验账号 owner。
    compaction: [Compaction; 2],
    owner: Option<std::sync::Arc<crate::owner::Owner>>,
}

/// desktop maker-host/compaction-settings-store.ts：50–95，默认 90。
struct Compaction {
    prefix: &'static str,
    key: &'static str,
    label: &'static str,
    file: OverrideFile,
}

fn clamp_pct(v: Option<&Value>) -> i64 {
    match v.and_then(Value::as_f64) {
        Some(f) if f.is_finite() => (f + 0.5).floor().clamp(50.0, 95.0) as i64,
        _ => 90,
    }
}

/// bootstrap-electron.ts assertCompactionMutationOwner：owner 戳必须带齐且仍是当前账号会话。
fn assert_local_owner(owner: Option<&crate::owner::Owner>, raw: &Value) -> Result<(), String> {
    let generation_ok = raw.get("ownerGeneration").and_then(Value::as_f64).is_some_and(|g| g.fract() == 0.0 && g.abs() <= 9007199254740991.0);
    let id_ok = matches!(raw.get("dataOwnerId"), Some(Value::String(_)) | Some(Value::Null));
    if !(raw.is_object() && id_ok && generation_ok) {
        return fail("INVALID_PARAMS", "compaction owner stamp required");
    }
    match owner {
        Some(o) if o.is_current(&raw["dataOwnerId"], &raw["ownerGeneration"]) => Ok(()),
        _ => fail("PRECONDITION_FAILED", "Compaction setting belongs to a stale account session."),
    }
}

fn zero_money(estimate: bool) -> Value {
    let mut m = json!({ "amount": 0, "currency": "USD", "approximate": estimate, "kind": if estimate { "value-estimate" } else { "actual-cost" } });
    if estimate {
        m["estimateReasons"] = json!(["subscription-value"]);
    }
    m
}

impl MiscSettings {
    pub fn new(settings_dir: &Path, pi_binary: Option<PathBuf>) -> Self {
        let bot_chain = OverrideFile::new(settings_dir.join("bot-model-chain-settings.json"), json!({ "modelChain": [] }), |r| json!({ "modelChain": normalize_bot_chain(&r["modelChain"]) }));
        let aux = OverrideFile::new(settings_dir.join("auxiliary-model-settings.json"), json!({ "models": [] }), |r| json!({ "models": normalize_aux_list(&r["models"]) }));
        let compaction = [
            ("compaction", "compaction-settings.json", "claudeCodeAutoCompactPct", "compaction"),
            ("pi-compaction", "pi-compaction-settings.json", "piAutoCompactPct", "pi compaction"),
        ]
        .map(|(prefix, name, key, label)| Compaction {
            prefix,
            key,
            label,
            file: OverrideFile::new(settings_dir.join(name), json!({ key: 90 }), move |r| json!({ key: clamp_pct(r.get(key)) })),
        });
        Self { bot_chain, aux, pi_binary, compaction, owner: None }
    }

    /// 压缩阈值的写入要核对当前账号会话。
    pub fn with_owner(mut self, owner: std::sync::Arc<crate::owner::Owner>) -> Self {
        self.owner = Some(owner);
        self
    }

    fn compaction_invoke(&self, channel: &str, args: &[Value]) -> Option<Result<Value, String>> {
        let rest = channel.strip_prefix("maker:")?;
        let (c, op) = self.compaction.iter().find_map(|c| rest.strip_prefix(c.prefix).and_then(|r| r.strip_prefix(':')).map(|op| (c, op)))?;
        let wire = || {
            let s = c.file.read_state();
            json!({ "pct": s.value[c.key], "isCustomized": s.is_customized, "defaultPct": s.defaults[c.key] })
        };
        let a0 = args.first().unwrap_or(&Value::Null);
        Some(match op {
            "get-pct" => Ok(c.file.read()[c.key].clone()),
            "get-state" => Ok(wire()),
            "reset-pct" => assert_local_owner(self.owner.as_deref(), args.first().unwrap_or(&Value::Null)).and_then(|_| c.file.reset().map(|_| wire())),
            "set-pct" => {
                if !a0.as_f64().is_some_and(f64::is_finite) {
                    return Some(fail("INVALID_PARAMS", &format!("{} pct required (number)", c.label)));
                }
                assert_local_owner(self.owner.as_deref(), args.get(1).unwrap_or(&Value::Null))
                    .and_then(|_| c.file.write_patch(&json!({ c.key: clamp_pct(Some(a0)) }), false).map(|_| wire()))
            }
            _ => return None,
        })
    }

    pub fn invoke(&self, channel: &str, args: &[Value]) -> Option<Result<Value, String>> {
        if let Some(r) = self.compaction_invoke(channel, args) {
            return Some(r);
        }
        let a0 = args.first();
        Some(match channel {
            "maker:pi-packages:list" => Ok(json!({ "available": false, "packages": [] })),
            "maker:pi-packages:mutate" => Self::packages_mutate(a0.unwrap_or(&Value::Null)),
            "maker:agent:pi-kernel-state" => match a0 {
                Some(v) if !is_undefined(Some(v)) && !v.is_boolean() => fail("INVALID_PARAMS", "check must be boolean"),
                _ => Ok(self.kernel_state()),
            },
            "maker:agent:pi-kernel-install" => Self::kernel_install(a0.unwrap_or(&Value::Null)),
            "maker:usage:history" => Ok(Self::usage_history()),
            "local-db:bots:model-chain-settings-get" => Ok(self.bot_wire()),
            "local-db:bots:model-chain-settings-set" => {
                let chain = normalize_bot_chain(&a0.filter(|v| v.is_object()).map(|v| v["modelChain"].clone()).unwrap_or(Value::Null));
                if chain.is_empty() {
                    return Some(fail("INVALID_PARAMS", "Bot model chain must contain at least one route"));
                }
                self.bot_chain.write_patch(&json!({ "modelChain": chain }), true).map(|_| self.bot_wire())
            }
            "local-db:bots:model-chain-settings-reset" => match self.bot_chain.reset() {
                Ok(_) => Ok(self.bot_wire()),
                Err(_) => fail("INTERNAL", "Could not restore Bot model defaults"),
            },
            "maker:auxiliary-model-settings:get" => Ok(self.aux_wire()),
            "maker:auxiliary-model-settings:set" => self.aux_set(a0.unwrap_or(&Value::Null)),
            _ => return None,
        })
    }

    fn bot_wire(&self) -> Value {
        let s = self.bot_chain.read_state();
        json!({ "modelChain": s.value["modelChain"], "isCustomized": s.is_customized })
    }

    fn aux_wire(&self) -> Value {
        let mut w = self.aux.read_state().wire();
        w["options"] = json!([]);
        w
    }

    fn aux_set(&self, raw: &Value) -> Result<Value, String> {
        let Some(obj) = raw.as_object() else { return fail("INVALID_PARAMS", "auxiliary model settings patch required") };
        if obj.is_empty() || obj.keys().any(|k| k != "models") {
            return fail("INVALID_PARAMS", "auxiliary model settings patch has invalid keys");
        }
        let models = &obj["models"];
        if !is_valid_aux_list_input(models) {
            return fail("INVALID_PARAMS", "models must be a unique list of at most 3 auxiliary refs");
        }
        // 轻量档位 key 总是允许；catalog pin 只接受已持久化的（本机没有可路由的候选）。
        let persisted = self.aux.read()["models"].as_array().cloned().unwrap_or_default();
        if models.as_array().unwrap().iter().any(|m| !is_utility_kind(m.as_str().unwrap()) && !persisted.contains(m)) {
            return fail("INVALID_PARAMS", "models contains a catalog pin that is not currently routable");
        }
        self.aux.write_patch(&json!({ "models": models }), false)?;
        Ok(self.aux_wire())
    }

    /// 包管理要调 Pi 原生 `pi install/remove`，本机版未接：校验入参后拒绝。
    fn packages_mutate(raw: &Value) -> Result<Value, String> {
        let Some(o) = raw.as_object() else { return fail("INVALID_PARAMS", "payload must be an object") };
        if !["install", "remove", "update", "set-enabled"].contains(&o.get("action").and_then(Value::as_str).unwrap_or("")) {
            return fail("INVALID_PARAMS", "action must be one of install, remove, update, set-enabled");
        }
        match o.get("source").and_then(Value::as_str) {
            Some(s) if utf16_len(s) <= 2048 => {}
            _ => return fail("INVALID_PARAMS", "invalid Pi extension source"),
        }
        if o.contains_key("confirmed") {
            return fail("INVALID_PARAMS", "Renderer confirmation is not accepted");
        }
        if !is_undefined(o.get("enabled")) && !o["enabled"].is_boolean() {
            return fail("INVALID_PARAMS", "enabled must be a boolean");
        }
        if !is_undefined(o.get("mutationTarget")) {
            let ok = o["mutationTarget"].as_str().map(|t| Regex::new(r"^cindy-pi-package:[a-f0-9]{64}$").unwrap().is_match(t)).unwrap_or(false);
            if !ok {
                return fail("INVALID_PARAMS", "invalid Pi extension mutation target");
            }
        }
        fail("PRECONDITION_FAILED", "Pi extension management is not available in this build")
    }

    fn kernel_state(&self) -> Value {
        let installed = self.pi_binary.as_ref().map(|p| p.is_file()).unwrap_or(false);
        let empty = || json!({ "release": null, "checkedAt": null, "error": false });
        json!({
            "currentVersion": if installed { json!(PI_RS_VERSION) } else { Value::Null },
            "restartRequired": false, "official": empty(), "upstream": empty(), "operation": null,
        })
    }

    fn kernel_install(body: &Value) -> Result<Value, String> {
        let Some(o) = body.as_object() else { return fail("INVALID_PARAMS", "Invalid Pi install request") };
        let ok = matches!(o.get("source").and_then(Value::as_str), Some("official" | "upstream"))
            && o.get("version").and_then(Value::as_str).map(|v| Regex::new(r"^\d+\.\d+\.\d+$").unwrap().is_match(v)).unwrap_or(false)
            && o.keys().all(|k| k == "source" || k == "version");
        if !ok {
            return fail("INVALID_PARAMS", "Invalid Pi install target");
        }
        // 没有可安装的候选版本（Pi 随包钉死）。
        fail("PRECONDITION_FAILED", "version-changed")
    }

    /// 本机还没有日消费账本：返回 desktop 的空载荷形状，币种按 global 区域（USD）。
    fn usage_history() -> Value {
        let now = chrono::Local::now();
        json!({
            "generatedAt": now.timestamp_millis(),
            "todayKey": now.format("%Y-%m-%d").to_string(),
            "stale": false, "estimatesPending": false,
            "days": [], "modelDaily": [], "models": [],
            "streak": { "current": 0, "longest": 0 },
            "totals": {
                "today": zero_money(false), "last30Days": zero_money(false),
                "last30DaysWithEstimatedValue": zero_money(false), "last30DaysEstimatedValue": zero_money(true),
                "todayTokens": 0, "last30DaysTokens": 0,
            },
            "anomaly": { "isAnomalous": false, "trailing7DayAvg": null },
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rig(name: &str) -> MiscSettings {
        let d = std::env::temp_dir().join(format!("misc-settings-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        MiscSettings::new(&d, Some(std::env::current_exe().unwrap()))
    }
    fn call(s: &MiscSettings, ch: &str, args: Value) -> Result<Value, String> {
        s.invoke(ch, args.as_array().unwrap()).expect("owned")
    }

    #[test]
    fn bundled_pi_rs_version_matches_the_crate() {
        let toml = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/../pi-rs/Cargo.toml")).unwrap();
        assert!(toml.contains(&format!("version = \"{PI_RS_VERSION}\"")));
    }

    #[test]
    fn bot_model_chain_normalizes_dedupes_caps_and_rejects_empty() {
        let s = rig("bot");
        let g = call(&s, "local-db:bots:model-chain-settings-get", json!([])).unwrap();
        assert_eq!((g["modelChain"].clone(), g["isCustomized"].clone()), (json!([]), json!(false)));
        let routes = json!([
            { "model": " opus ", "providerId": " p ", "harness": "pi", "effort": "high", "fastMode": true },
            { "model": "opus", "providerId": "p", "harness": "pi" },
            { "model": "", "providerId": "p" },
            "junk",
            { "model": "m2", "harness": "weird", "fastMode": "ultrafast" },
        ]);
        let w = call(&s, "local-db:bots:model-chain-settings-set", json!([{ "modelChain": routes }])).unwrap();
        assert_eq!(w["isCustomized"], true);
        assert_eq!(w["modelChain"], json!([
            { "harness": "pi", "model": "opus", "providerId": "p", "effort": "high", "fastMode": true },
            { "harness": "claude", "model": "m2", "providerId": null, "effort": "", "fastMode": "ultrafast" },
        ]));
        // 最多 5 条；远端设备路由要解出 agentDeviceId。
        let many: Vec<Value> = (0..8).map(|i| json!({ "model": format!("m{i}") })).collect();
        assert_eq!(call(&s, "local-db:bots:model-chain-settings-set", json!([{ "modelChain": many }])).unwrap()["modelChain"].as_array().unwrap().len(), 5);
        let remote = json!([{ "model": "m", "providerId": "model-route:[\"dev-1\",\"p\"]" }]);
        let w = call(&s, "local-db:bots:model-chain-settings-set", json!([{ "modelChain": remote }])).unwrap();
        assert_eq!((w["modelChain"][0]["agentDeviceId"].clone(), w["modelChain"][0]["providerId"].clone()), (json!("dev-1"), json!("p")));
        let mismatch = json!([{ "model": "m", "agentDeviceId": "other", "providerId": "model-route:[\"dev-1\",null]" }]);
        assert!(call(&s, "local-db:bots:model-chain-settings-set", json!([{ "modelChain": mismatch }])).unwrap_err().contains("at least one route"));
        assert!(call(&s, "local-db:bots:model-chain-settings-set", json!(["x"])).unwrap_err().contains("INVALID_PARAMS"));
        assert_eq!(call(&s, "local-db:bots:model-chain-settings-reset", json!([])).unwrap()["isCustomized"], false);
    }

    #[test]
    fn auxiliary_models_accept_profile_keys_and_reject_unroutable_pins() {
        let s = rig("aux");
        let g = call(&s, "maker:auxiliary-model-settings:get", json!([])).unwrap();
        assert_eq!((g["models"].clone(), g["options"].clone(), g["isCustomized"].clone()), (json!([]), json!([]), json!(false)));
        let w = call(&s, "maker:auxiliary-model-settings:set", json!([{ "models": ["litellm-glm-5.1", "codex-gpt-5.4-mini"] }])).unwrap();
        assert_eq!(w["models"], json!(["litellm-glm-5.1", "codex-gpt-5.4-mini"]));
        assert_eq!(w["customizedKeys"], json!(["models"]));
        let bad = |v: Value| call(&s, "maker:auxiliary-model-settings:set", json!([v])).unwrap_err();
        assert!(bad(json!({ "models": ["litellm-glm-5.1", "litellm-glm-5.1"] })).contains("unique list"));
        assert!(bad(json!({ "models": ["a", "b", "c", "d"] })).contains("unique list"));
        assert!(bad(json!({ "models": ["toString"] })).contains("unique list"), "继承键不是合法档位");
        assert!(bad(json!({ "models": [" litellm-glm-5.1"] })).contains("unique list"), "必须是规范形式");
        assert!(bad(json!({ "other": 1 })).contains("invalid keys"));
        assert!(bad(json!({})).contains("invalid keys"));
        assert!(bad(json!("x")).contains("patch required"));
        // 格式合法的 catalog pin：本机没有可路由候选，未持久化的拒绝。
        assert!(bad(json!({ "models": ["cat:xd:codex:deepseek/deepseek-v4-flash"] })).contains("not currently routable"));
        assert!(bad(json!({ "models": ["cat:xd:nope:m"] })).contains("unique list"));
        assert!(is_catalog_pin("cat:a%3Ab:pi:m:with:colons") && !is_catalog_pin("cat:%ZZ:pi:m") && !is_catalog_pin("cat::pi:m") && !is_catalog_pin("cat:a:pi:"));
        // 已持久化的 pin（旧版本留下）可以继续保留。
        let d = std::env::temp_dir().join(format!("misc-aux-pin-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("auxiliary-model-settings.json"), r#"{"models":["cat:xd:pi:m1","garbage"]}"#).unwrap();
        let s2 = MiscSettings::new(&d, None);
        assert_eq!(call(&s2, "maker:auxiliary-model-settings:get", json!([])).unwrap()["models"], json!(["cat:xd:pi:m1"]));
        assert!(call(&s2, "maker:auxiliary-model-settings:set", json!([{ "models": ["cat:xd:pi:m1", "litellm-glm-5.1"] }])).is_ok());
    }

    #[test]
    fn pi_packages_and_kernel_are_validated_but_not_available() {
        let s = rig("pi");
        assert_eq!(call(&s, "maker:pi-packages:list", json!([])).unwrap(), json!({ "available": false, "packages": [] }));
        let m = |v: Value| call(&s, "maker:pi-packages:mutate", json!([v])).unwrap_err();
        assert!(m(json!("x")).contains("must be an object"));
        assert!(m(json!({ "action": "zap", "source": "a" })).contains("action must be one of"));
        assert!(m(json!({ "action": "install", "source": 5 })).contains("invalid Pi extension source"));
        assert!(m(json!({ "action": "install", "source": "a", "confirmed": true })).contains("not accepted"));
        assert!(m(json!({ "action": "install", "source": "a", "enabled": "y" })).contains("must be a boolean"));
        assert!(m(json!({ "action": "install", "source": "a", "mutationTarget": "x" })).contains("mutation target"));
        assert!(m(json!({ "action": "install", "source": "a" })).contains("PRECONDITION_FAILED"));
        let ok_target = format!("cindy-pi-package:{}", "a".repeat(64));
        assert!(m(json!({ "action": "remove", "source": "a", "mutationTarget": ok_target })).contains("PRECONDITION_FAILED"));

        let k = call(&s, "maker:agent:pi-kernel-state", json!([])).unwrap();
        assert_eq!((k["currentVersion"].clone(), k["restartRequired"].clone()), (json!(PI_RS_VERSION), json!(false)));
        assert!(call(&s, "maker:agent:pi-kernel-state", json!([{ "__cindy_t": "u" }])).is_ok());
        assert!(call(&s, "maker:agent:pi-kernel-state", json!(["yes"])).unwrap_err().contains("check must be boolean"));
        let missing = MiscSettings::new(&std::env::temp_dir(), Some(PathBuf::from("/nonexistent/pi")));
        assert_eq!(call(&missing, "maker:agent:pi-kernel-state", json!([false])).unwrap()["currentVersion"], Value::Null);
        assert!(call(&s, "maker:agent:pi-kernel-install", json!([{ "source": "official", "version": "1.2" }])).unwrap_err().contains("Invalid Pi install target"));
        assert!(call(&s, "maker:agent:pi-kernel-install", json!([{ "source": "official", "version": "1.2.3", "x": 1 }])).unwrap_err().contains("Invalid Pi install target"));
        assert!(call(&s, "maker:agent:pi-kernel-install", json!([{ "source": "upstream", "version": "1.2.3" }])).unwrap_err().contains("version-changed"));
        assert!(call(&s, "maker:agent:pi-kernel-install", json!(["x"])).unwrap_err().contains("Invalid Pi install request"));
    }

    #[test]
    fn usage_history_is_the_empty_desktop_payload_with_a_local_date_key() {
        let s = rig("usage");
        let u = call(&s, "maker:usage:history", json!([])).unwrap();
        assert_eq!(u["totals"]["today"], json!({ "amount": 0, "currency": "USD", "approximate": false, "kind": "actual-cost" }));
        assert_eq!(u["totals"]["last30DaysEstimatedValue"]["estimateReasons"], json!(["subscription-value"]));
        assert!(Regex::new(r"^\d{4}-\d{2}-\d{2}$").unwrap().is_match(u["todayKey"].as_str().unwrap()));
        assert!(s.invoke("maker:compaction:get-state", &[]).is_some(), "压缩阈值也归本模块");
    }
}

#[cfg(test)]
mod compaction_tests {
    use super::*;
    use crate::owner::Owner;
    use std::sync::Arc;

    fn rig(name: &str) -> (MiscSettings, Arc<Owner>) {
        let d = std::env::temp_dir().join(format!("compaction-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        let owner = Arc::new(Owner::new(&d));
        (MiscSettings::new(&d, None).with_owner(owner.clone()), owner)
    }
    fn call(s: &MiscSettings, ch: &str, args: Value) -> Result<Value, String> {
        s.invoke(ch, args.as_array().unwrap()).expect("owned")
    }

    #[test]
    fn pct_is_clamped_and_requires_a_current_owner_stamp() {
        let (s, o) = rig("pct");
        assert_eq!(call(&s, "maker:compaction:get-pct", json!([])).unwrap(), json!(90));
        let st = o.stamp();
        let set = call(&s, "maker:compaction:set-pct", json!([120, st])).unwrap();
        assert_eq!(set, json!({ "pct": 95, "isCustomized": true, "defaultPct": 90 }));
        assert_eq!(call(&s, "maker:compaction:set-pct", json!([10.4, st])).unwrap()["pct"], 50);
        // 另一份文件互不影响
        assert_eq!(call(&s, "maker:pi-compaction:get-state", json!([])).unwrap(), json!({ "pct": 90, "isCustomized": false, "defaultPct": 90 }));
        assert!(call(&s, "maker:compaction:set-pct", json!(["x", st])).unwrap_err().contains("INVALID_PARAMS"));
        assert!(call(&s, "maker:compaction:set-pct", json!([60])).unwrap_err().contains("owner stamp required"));
        let stale = json!({ "dataOwnerId": st["dataOwnerId"], "ownerGeneration": st["ownerGeneration"].as_i64().unwrap() + 1 });
        assert!(call(&s, "maker:compaction:set-pct", json!([60, stale])).unwrap_err().contains("PRECONDITION_FAILED"));
        assert!(call(&s, "maker:compaction:reset-pct", json!([stale])).unwrap_err().contains("PRECONDITION_FAILED"));
        assert_eq!(call(&s, "maker:compaction:reset-pct", json!([st])).unwrap()["isCustomized"], false);
    }
}
