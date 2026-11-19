//! 语音输入数据（设置 / 历史 / 词典）：原 sidecar settings/voice-input-data.mjs。
//! 对齐 desktop main/voice-input/VoiceInputDataStore.ts 的通道与返回形状。
//! 与 desktop 的差别：没有跨设备词典同步；语音识别本身未迁移，readiness / 权限报告不可用。
//! 纯函数在 voice_dict.rs / voice_settings.rs（有上游黄金样例），这里只管存储、通道与推送。

use crate::appearance_core::{js_trim, utf16_len};
use crate::maker_host::Emitter;
use crate::override_file::{read_json, write_json_atomic};
use crate::voice_dict::{self as vd, slice_utf16, Entry, Env, SystemEnv};
use crate::voice_settings as vs;
use serde_json::{json, Map, Value};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

pub const INVOKE_CHANNELS: &[&str] = &[
    "voice-input:settings:update",
    "voice-input:dictionary:delete-entries",
    "voice-input:dictionary:add-entry",
    "voice-input:dictionary:import-entries",
    "voice-input:dictionary:rename-entry",
    "voice-input:dictionary:edit-entry",
    "voice-input:dictionary-learning:record-actions",
    "voice-input:get-system-permissions",
    "voice-input:get-readiness",
    "voice-input:cancel",
];
pub const SYNC_CHANNELS: &[&str] = &[
    "voice-input:data:get",
    "voice-input:data:migrate-legacy",
    "voice-input:history:get",
    "voice-input:history:get-for-refinement",
    "voice-input:history:record",
    "voice-input:history:update",
    "voice-input:history:delete",
    "voice-input:get-system-permissions-cached",
    "voice-input:get-readiness-cached",
];

#[derive(Clone)]
struct State {
    legacy_migrated: bool,
    settings: Value,
    history: Vec<Value>,
}

pub struct VoiceInput {
    file: PathBuf,
    platform: String,
    emitter: Arc<dyn Emitter>,
    state: Mutex<Option<State>>,
}

fn strings(v: Option<&Value>, max: usize, cap_each: bool) -> Vec<String> {
    let Some(a) = v.and_then(Value::as_array) else { return vec![] };
    a.iter().filter_map(Value::as_str).map(|s| if cap_each { slice_utf16(s, vd::MAX_ENTRY_CHARS) } else { s.to_string() }).take(max).collect()
}

impl VoiceInput {
    pub fn new(settings_dir: &Path, emitter: Arc<dyn Emitter>) -> Self {
        Self::with_platform(settings_dir, emitter, crate::app_settings::host_platform())
    }

    pub fn with_platform(settings_dir: &Path, emitter: Arc<dyn Emitter>, platform: &str) -> Self {
        Self { file: settings_dir.join("voice-input-data.v1.json"), platform: platform.into(), emitter, state: Mutex::new(None) }
    }

    pub fn owns_invoke(c: &str) -> bool {
        INVOKE_CHANNELS.contains(&c)
    }
    pub fn owns_sync(c: &str) -> bool {
        SYNC_CHANNELS.contains(&c)
    }

    fn load(&self, env: &mut dyn Env) -> State {
        let mut g = self.state.lock().unwrap();
        if g.is_none() {
            // desktop 同样按默认值继续（VoiceInputDataStore.ts:524）；坏文件留在原处，直到下一次写入。
            let raw = match read_json(&self.file, None) {
                Ok(v) => v,
                Err(e) => {
                    eprintln!("[voice-input] data read failed, using defaults: {e}");
                    None
                }
            };
            let (settings, history) = vs::normalize_snapshot(env, raw.as_ref(), &self.platform);
            let legacy = raw.as_ref().and_then(|r| r.get("legacyRendererStorageMigrated")) == Some(&json!(true));
            *g = Some(State { legacy_migrated: legacy, settings, history });
        }
        g.clone().unwrap()
    }

    /// projectVoiceInputDataForPersist：有效值字段不落盘，只存 *Override。
    fn save(&self, next: State) -> Result<(), String> {
        let mut persisted = next.settings.clone();
        if let Some(o) = persisted.as_object_mut() {
            for k in ["composerLongPressEnabled", "dictionarySyncEnabled", "speakerFilterStrictness"] {
                o.shift_remove(k);
            }
        }
        let body = json!({ "version": 1, "legacyRendererStorageMigrated": next.legacy_migrated, "settings": persisted, "history": next.history });
        write_json_atomic(&self.file, &body, None)?;
        self.emitter.emit("voice-input:data-changed", json!({ "settings": next.settings, "history": next.history }));
        *self.state.lock().unwrap() = Some(next);
        Ok(())
    }

    fn replace_settings(&self, env: &mut dyn Env, settings: &Value) -> Result<Value, String> {
        let cur = self.load(env);
        let normalized = vs::normalize_settings(env, Some(settings), &self.platform);
        self.save(State { settings: normalized, ..cur })?;
        Ok(self.load(env).settings)
    }

    fn snapshot(&self, env: &mut dyn Env) -> Value {
        let s = self.load(env);
        json!({ "settings": s.settings, "history": s.history })
    }

    fn with_dict(cur: &Value, entries: &[Entry]) -> Value {
        let mut v = cur.clone();
        v["dictionaryEntries"] = Value::Array(entries.iter().map(Entry::to_value).collect());
        v
    }

    fn entries_of(cur: &Value) -> Vec<Entry> {
        cur["dictionaryEntries"].as_array().map(|a| a.iter().map(Entry::from_normalized).collect()).unwrap_or_default()
    }

    // ── 通道 ──

    pub fn invoke(&self, channel: &str, args: &[Value]) -> Option<Result<Value, String>> {
        self.invoke_with(&mut SystemEnv, channel, args)
    }

    pub fn invoke_with(&self, env: &mut dyn Env, channel: &str, args: &[Value]) -> Option<Result<Value, String>> {
        let a0 = args.first();
        Some(match channel {
            "voice-input:settings:update" => self.settings_update(env, a0),
            "voice-input:dictionary:delete-entries" => {
                let cur = self.load(env).settings;
                let ids: Vec<String> = a0
                    .and_then(Value::as_array)
                    .map(|a| a.iter().filter_map(Value::as_str).filter(|s| !s.is_empty()).map(String::from).take(vd::MAX_ENTRIES).collect())
                    .unwrap_or_default();
                match self.delete_settings(env, &cur, &ids) {
                    Some(next) => self.replace_settings(env, &next),
                    None => Ok(cur),
                }
            }
            "voice-input:dictionary:add-entry" => {
                let cur = self.load(env).settings;
                let text = a0.and_then(Value::as_str).unwrap_or("");
                let Some(entry) = vd::create_manual_entry(env, text) else { return Some(Ok(cur)) };
                let mut entries = Self::entries_of(&cur);
                let key = entry.text.to_lowercase();
                if entries.iter().any(|e| e.text.to_lowercase() == key) || entries.len() >= vd::MAX_ENTRIES {
                    return Some(Ok(cur));
                }
                entries.push(entry);
                self.replace_settings(env, &Self::with_dict(&cur, &entries))
            }
            "voice-input:dictionary:import-entries" => {
                let cur = self.load(env).settings;
                let terms = strings(a0, vd::MAX_ENTRIES, true);
                let merged = vd::merge_csv_terms(env, Some(&cur["dictionaryEntries"]), &terms);
                if merged.imported == 0 { Ok(cur) } else { self.replace_settings(env, &Self::with_dict(&cur, &merged.entries)) }
            }
            "voice-input:dictionary:rename-entry" => {
                let id = a0.and_then(|p| p.get("entryId")).and_then(Value::as_str).unwrap_or("");
                if id.is_empty() {
                    return Some(Ok(self.load(env).settings));
                }
                let text = a0.and_then(|p| p.get("text")).and_then(Value::as_str).unwrap_or("");
                self.edit_entry(env, id, text, None)
            }
            "voice-input:dictionary:edit-entry" => {
                let id = a0.and_then(|p| p.get("entryId")).and_then(Value::as_str).unwrap_or("");
                if id.is_empty() {
                    return Some(Ok(self.load(env).settings));
                }
                let text = a0.and_then(|p| p.get("text")).and_then(Value::as_str).map(|s| slice_utf16(s, vd::MAX_ENTRY_CHARS)).unwrap_or_default();
                let aliases = strings(a0.and_then(|p| p.get("aliases")), vd::MAX_ALIASES, true);
                self.edit_entry(env, id, &text, Some(aliases))
            }
            "voice-input:dictionary-learning:record-actions" => self.record_actions(env, a0),
            "voice-input:get-system-permissions" => Ok(self.permissions()),
            "voice-input:get-readiness" => Ok(readiness()),
            "voice-input:cancel" => Ok(json!({ "ok": true })),
            _ => return None,
        })
    }

    fn settings_update(&self, env: &mut dyn Env, patch: Option<&Value>) -> Result<Value, String> {
        let cur = self.load(env).settings;
        let mut p: Map<String, Value> = match patch {
            Some(Value::Object(o)) => o.clone(),
            _ => Map::new(),
        };
        // desktop :1021 stripDictionaryFields：词典只走语义化入口；开关只记 override。
        for k in ["dictionaryEntries", "dictionaryCandidates", "suppressedAutomaticDictionaryTexts"] {
            p.shift_remove(k);
        }
        if let Some(v) = p.get("dictionarySyncEnabled").cloned().filter(|v| v.is_boolean() || v.is_null()) {
            p.insert("dictionarySyncEnabledOverride".into(), v);
            p.shift_remove("dictionarySyncEnabled");
        }
        // desktop :1048 / :1065 translateOverride
        let translate = |p: &mut Map<String, Value>, name: &str, valid: &dyn Fn(&Value) -> bool| {
            if let Some(v) = p.shift_remove(name) {
                let key = format!("{name}Override");
                if valid(&v) {
                    p.insert(key, v);
                } else if v.is_null() {
                    p.insert(key, Value::Null);
                }
            }
        };
        translate(&mut p, "composerLongPressEnabled", &|v| v.is_boolean());
        translate(&mut p, "speakerFilterStrictness", &|v| v.as_str().map(|s| vs::STRICTNESS.contains(&s)).unwrap_or(false));
        let mut merged = cur.as_object().cloned().unwrap_or_default();
        for (k, v) in p {
            merged.insert(k, v);
        }
        self.replace_settings(env, &Value::Object(merged))
    }

    /// deleteVoiceInputDictionaryEntriesFromSettings：None = 无变化。内部按「平台未知」规范化，与上游一致。
    fn delete_settings(&self, env: &mut dyn Env, cur: &Value, ids: &[String]) -> Option<Value> {
        let entries = Self::entries_of(cur);
        let cands: Vec<vd::Candidate> = cur["dictionaryCandidates"].as_array().map(|a| a.iter().map(vd::Candidate::from_normalized).collect()).unwrap_or_default();
        let suppressed: Vec<String> = cur["suppressedAutomaticDictionaryTexts"].as_array().map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect()).unwrap_or_default();
        let (e, c, s) = vd::delete_entries(&entries, &cands, &suppressed, ids)?;
        let mut next = Self::with_dict(cur, &e);
        next["dictionaryCandidates"] = Value::Array(c.iter().map(vd::Candidate::to_value).collect());
        next["suppressedAutomaticDictionaryTexts"] = json!(s);
        Some(vs::normalize_settings(env, Some(&next), ""))
    }

    fn edit_entry(&self, env: &mut dyn Env, id: &str, text: &str, aliases: Option<Vec<String>>) -> Result<Value, String> {
        let cur = self.load(env).settings;
        let normalized = vd::normalize_str(text);
        let entries = Self::entries_of(&cur);
        let Some(pos) = entries.iter().position(|e| e.id == id).filter(|_| !normalized.is_empty()) else { return Ok(cur) };
        let now = env.now();
        let mut e = entries[pos].clone();
        e.text = normalized;
        e.updated_at = now;
        if let Some(list) = aliases {
            e.aliases = list
                .iter()
                .map(|a| vd::normalize_str(a))
                .filter(|a| !a.is_empty())
                .map(|a| vd::Alias { text: a, count: 1.0, last_seen_at: now })
                .collect();
        }
        let mut all = entries;
        all[pos] = e;
        self.replace_settings(env, &Self::with_dict(&cur, &all))
    }

    /// applyVoiceInputDictionaryLearningActions（设置层）：None = 无变化。
    pub fn apply_learning(env: &mut dyn Env, cur: &Value, actions: &[Value]) -> Option<Value> {
        if actions.is_empty() || cur["refinementEnabled"] != json!(true) || cur["autoDictionaryEnabled"] != json!(true) {
            return None;
        }
        let entries = Self::entries_of(cur);
        let cands: Vec<vd::Candidate> = cur["dictionaryCandidates"].as_array().map(|a| a.iter().map(vd::Candidate::from_normalized).collect()).unwrap_or_default();
        let suppressed: Vec<String> = cur["suppressedAutomaticDictionaryTexts"].as_array().map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect()).unwrap_or_default();
        let d = vd::apply_learning(env, entries, cands, &suppressed, actions)?;
        let mut next = Self::with_dict(cur, &d.entries);
        next["dictionaryCandidates"] = Value::Array(d.candidates.iter().map(vd::Candidate::to_value).collect());
        Some(next)
    }

    fn record_actions(&self, env: &mut dyn Env, actions: Option<&Value>) -> Result<Value, String> {
        let cur = self.load(env).settings;
        let list: Vec<Value> = actions
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter(|x| {
                        x.is_object()
                            && x.get("action").map(Value::is_string).unwrap_or(false)
                            && x.get("term").and_then(Value::as_str).map(|t| !js_trim(t).is_empty() && utf16_len(js_trim(t)) <= vd::MAX_ENTRY_CHARS).unwrap_or(false)
                    })
                    .cloned()
                    .collect()
            })
            .unwrap_or_default();
        let Some(next) = Self::apply_learning(env, &cur, &list) else {
            return Ok(json!({ "settings": cur, "newAutomaticEntries": [] }));
        };
        let fresh = vd::new_automatic_entries(env, &Self::entries_of(&cur), &Self::entries_of(&next));
        let settings = self.replace_settings(env, &next)?;
        Ok(json!({ "settings": settings, "newAutomaticEntries": fresh }))
    }

    // ── sync 通道 ──

    pub fn sync(&self, channel: &str, args: &[Value]) -> Option<Result<Value, String>> {
        self.sync_with(&mut SystemEnv, channel, args)
    }

    pub fn sync_with(&self, env: &mut dyn Env, channel: &str, args: &[Value]) -> Option<Result<Value, String>> {
        let a0 = args.first();
        Some(match channel {
            "voice-input:data:get" => Ok(self.snapshot(env)),
            "voice-input:data:migrate-legacy" => self.migrate_legacy(env, a0),
            "voice-input:history:get" => {
                let h = self.load(env).history;
                Ok(match a0.and_then(Value::as_f64) {
                    Some(n) => Value::Array(h.into_iter().take(if n > 0.0 { n.floor() as usize } else { 0 }).collect()),
                    None => Value::Array(h),
                })
            }
            "voice-input:history:get-for-refinement" => {
                let cur = self.load(env);
                let compacted = vs::compact_history(&cur.history);
                if vs::needs_compaction(&cur.history) {
                    if let Err(e) = self.save(State { history: compacted.clone(), ..cur }) {
                        return Some(Err(e));
                    }
                }
                Ok(Value::Array(compacted))
            }
            "voice-input:history:record" => {
                let t = a0.and_then(Value::as_str).map(|s| js_trim(s).to_string()).unwrap_or_default();
                if t.is_empty() {
                    return Some(Ok(Value::Null));
                }
                let cur = self.load(env);
                if let Some(dup) = cur.history.iter().find(|e| e["text"] == json!(t)) {
                    return Some(Ok(dup["id"].clone()));
                }
                let Some(entry) = vs::create_history_entry(env, &t) else { return Some(Ok(Value::Null)) };
                let id = entry["id"].clone();
                let mut all = vec![entry];
                all.extend(cur.history.iter().cloned());
                self.save(State { history: vs::compact_history(&all), ..cur }).map(|_| id)
            }
            "voice-input:history:update" => {
                let id = a0.and_then(|p| p.get("id")).and_then(Value::as_str).unwrap_or("");
                let t = a0.and_then(|p| p.get("text")).and_then(Value::as_str).map(|s| js_trim(s).to_string()).unwrap_or_default();
                let cur = self.load(env);
                let entry = if !id.is_empty() && !t.is_empty() { cur.history.iter().find(|e| e["id"] == json!(id)).cloned() } else { None };
                if let Some(mut e) = entry {
                    e["text"] = json!(t);
                    let mut all = vec![e];
                    all.extend(cur.history.iter().filter(|x| x["id"] != json!(id)).cloned());
                    let history = vs::normalize_history(env, Some(&Value::Array(all)));
                    if let Err(err) = self.save(State { history, ..cur }) {
                        return Some(Err(err));
                    }
                }
                Ok(json!(true))
            }
            "voice-input:history:delete" => {
                let cur = self.load(env);
                let history: Vec<Value> = cur.history.iter().filter(|e| Some(&e["id"]) != a0).cloned().collect();
                if history.len() != cur.history.len() {
                    if let Err(e) = self.save(State { history, ..cur }) {
                        return Some(Err(e));
                    }
                }
                Ok(json!(true))
            }
            "voice-input:get-system-permissions-cached" => Ok(self.permissions()),
            "voice-input:get-readiness-cached" => Ok(readiness()),
            _ => return None,
        })
    }

    fn migrate_legacy(&self, env: &mut dyn Env, payload: Option<&Value>) -> Result<Value, String> {
        let cur = self.load(env);
        if cur.legacy_migrated {
            return Ok(self.snapshot(env));
        }
        let (mut settings, mut history) = (cur.settings.clone(), cur.history.clone());
        // 旧 localStorage 损坏时保留当前值。
        if let Some(raw) = payload.and_then(|p| p.get("settingsRaw")).and_then(Value::as_str) {
            if let Ok(parsed) = serde_json::from_str::<Value>(raw) {
                let mut merged = settings.as_object().cloned().unwrap_or_default();
                if let Some(o) = parsed.as_object() {
                    for (k, v) in o {
                        merged.insert(k.clone(), v.clone());
                    }
                }
                settings = vs::normalize_settings(env, Some(&Value::Object(merged)), &self.platform);
            }
        }
        if let Some(raw) = payload.and_then(|p| p.get("historyRaw")).and_then(Value::as_str) {
            if let Ok(parsed) = serde_json::from_str::<Value>(raw) {
                history = vs::normalize_history(env, Some(&parsed));
            }
        }
        self.save(State { legacy_migrated: true, settings, history })?;
        Ok(self.snapshot(env))
    }

    fn permissions(&self) -> Value {
        let not_required = self.platform != "darwin";
        let perm = |name: &str| {
            if not_required && name != "microphone" {
                json!({ "ok": true, "status": "not-required" })
            } else {
                json!({ "ok": false, "status": "unknown", "error": vs::UNAVAILABLE })
            }
        };
        json!({ "microphone": perm("microphone"), "inputMonitoring": perm("inputMonitoring"), "accessibility": perm("accessibility") })
    }
}

fn readiness() -> Value {
    json!({ "ok": false, "serviceMode": "byok", "provider": vs::PROVIDER_KIND, "providerModel": "", "auth": "api-key", "settingsTab": "providers", "error": vs::UNAVAILABLE })
}

#[cfg(test)]
mod tests;
