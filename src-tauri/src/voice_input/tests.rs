use super::*;
use crate::voice_dict::{self as vd, Env};
use crate::voice_settings as vs;
use serde_json::{json, Value};

/// 与 scripts/app-settings/voice-entry.ts 同一套固定时钟 / id。
struct Fixed(u32);
impl Env for Fixed {
    fn now(&mut self) -> f64 {
        1_700_000_000_000.0
    }
    fn id(&mut self, _p: &str, _t: f64, _s: &str) -> String {
        self.0 += 1;
        format!("id-{}", self.0)
    }
}

fn golden() -> Value {
    serde_json::from_str(include_str!("../voice_input_golden.json")).unwrap()
}

/// JS 里 undefined 在序列化时消失；黄金输入里的 "__undef__" 对应它。
fn input(v: &Value) -> Option<Value> {
    if v == &json!("__undef__") { None } else { Some(v.clone()) }
}

#[test]
fn settings_normalization_matches_upstream() {
    let g = golden();
    let cases = g["settings"].as_array().unwrap();
    assert!(cases.len() > 100);
    for (i, c) in cases.iter().enumerate() {
        let got = vs::normalize_settings(&mut Fixed(0), Some(&c["input"]), c["platform"].as_str().unwrap());
        assert_eq!(got, c["output"], "settings case {i} {}: {}", c["platform"], c["input"]);
    }
}

#[test]
fn snapshots_texts_history_match_upstream() {
    let g = golden();
    for c in g["snapshots"].as_array().unwrap() {
        let (s, h) = vs::normalize_snapshot(&mut Fixed(0), Some(&c["input"]), c["platform"].as_str().unwrap());
        assert_eq!(json!({ "settings": s, "history": h }), c["output"], "snapshot {}", c["input"]);
    }
    for c in g["texts"].as_array().unwrap() {
        let got = vd::normalize_text(input(&c["input"]).as_ref());
        assert_eq!(json!(got), c["output"], "text {}", c["input"]);
    }
    for c in g["history"].as_array().unwrap() {
        assert_eq!(json!(vs::normalize_history(&mut Fixed(0), Some(&c["input"]))), c["output"], "history {}", c["input"]);
    }
    for c in g["create"].as_array().unwrap() {
        assert_eq!(vs::create_history_entry(&mut Fixed(0), c["input"].as_str().unwrap()).unwrap_or(Value::Null), c["output"]);
    }
    for c in g["compact"].as_array().unwrap() {
        let input: Vec<Value> = c["input"].as_array().unwrap().clone();
        assert_eq!(json!(vs::compact_history(&input)), c["output"]);
    }
    for c in g["manual"].as_array().unwrap() {
        let got = vd::create_manual_entry(&mut Fixed(0), c["input"].as_str().unwrap()).map(|e| e.to_value()).unwrap_or(Value::Null);
        assert_eq!(got, c["output"], "manual {}", c["input"]);
    }
    assert_eq!(g["providerKind"], vs::PROVIDER_KIND);
}

#[test]
fn csv_merge_matches_upstream() {
    for c in golden()["csv"].as_array().unwrap() {
        let terms: Vec<String> = c["terms"].as_array().unwrap().iter().map(|t| t.as_str().unwrap().to_string()).collect();
        let r = vd::merge_csv_terms(&mut Fixed(0), Some(&c["existing"]), &terms);
        assert_eq!(Value::Array(r.entries.iter().map(vd::Entry::to_value).collect()), c["entries"], "csv {}", c["existing"]);
        assert_eq!(json!(r.imported), c["importedCount"]);
    }
}

#[test]
fn learning_and_delete_match_upstream() {
    let g = golden();
    let mut n = 0;
    for c in g["learning"].as_array().unwrap() {
        let cur = &c["current"];
        let actions: Vec<Value> = c["actions"].as_array().unwrap().clone();
        let next = VoiceInput::apply_learning(&mut Fixed(0), cur, &actions);
        assert_eq!(next.is_some(), c["changed"], "changed {}", c["actions"]);
        if let Some(next) = next {
            // 设置层最终经 normalizeVoiceInputSettings（replaceSettings 里）；这里直接比较词典两项。
            assert_eq!(next["dictionaryEntries"], c["next"]["dictionaryEntries"], "entries {}", c["actions"]);
            assert_eq!(next["dictionaryCandidates"], c["next"]["dictionaryCandidates"], "candidates {}", c["actions"]);
            let fresh = vd::new_automatic_entries(&mut Fixed(0), &VoiceInput::entries_of(cur), &VoiceInput::entries_of(&next));
            assert_eq!(Value::Array(fresh), c["newAutomatic"], "fresh {}", c["actions"]);
            n += 1;
        }
    }
    let expected = g["learning"].as_array().unwrap().iter().filter(|c| c["changed"] == json!(true)).count();
    assert!(expected >= 9 && n == expected, "changed cases: {n} / {expected}");
    let rig = VoiceInput::with_platform(&std::env::temp_dir(), std::sync::Arc::new(Rec), "darwin");
    for c in g["deletes"].as_array().unwrap() {
        let ids: Vec<String> = c["ids"].as_array().unwrap().iter().map(|i| i.as_str().unwrap().to_string()).collect();
        let next = rig.delete_settings(&mut Fixed(0), &c["current"], &ids);
        assert_eq!(next.is_some(), c["changed"], "delete changed {}", c["ids"]);
        if let Some(next) = next {
            assert_eq!(next, c["next"], "delete {}", c["ids"]);
        }
    }
}

struct Rec;
impl Emitter for Rec {
    fn emit(&self, _c: &str, _a: Value) {}
}

struct Log(Mutex<Vec<String>>);
impl Emitter for Log {
    fn emit(&self, c: &str, _a: Value) {
        self.0.lock().unwrap().push(c.to_string());
    }
}

fn rig(name: &str) -> (VoiceInput, std::path::PathBuf, Arc<Log>) {
    let d = std::env::temp_dir().join(format!("voice-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    let log = Arc::new(Log(Mutex::new(vec![])));
    (VoiceInput::with_platform(&d, log.clone(), "darwin"), d, log)
}

fn inv(v: &VoiceInput, ch: &str, args: &[Value]) -> Value {
    v.invoke(ch, args).unwrap().unwrap()
}
fn syn(v: &VoiceInput, ch: &str, args: &[Value]) -> Value {
    v.sync(ch, args).unwrap().unwrap()
}

#[test]
fn dictionary_history_roundtrip_and_restart() {
    let (v, d, log) = rig("rt");
    let added = inv(&v, "voice-input:dictionary:add-entry", &[json!("Cindy Tauri")]);
    assert!(added["dictionaryEntries"].as_array().unwrap().iter().any(|e| e["text"] == "Cindy Tauri"));
    assert!(log.0.lock().unwrap().iter().any(|c| c == "voice-input:data-changed"));
    let dupe = inv(&v, "voice-input:dictionary:add-entry", &[json!("cindy tauri")]);
    assert_eq!(dupe["dictionaryEntries"].as_array().unwrap().len(), 1);
    let id = added["dictionaryEntries"][0]["id"].clone();
    let renamed = inv(&v, "voice-input:dictionary:rename-entry", &[json!({ "entryId": id, "text": "Cindy" })]);
    assert_eq!(renamed["dictionaryEntries"][0]["text"], "Cindy");
    let imported = inv(&v, "voice-input:dictionary:import-entries", &[json!(["alpha", "beta", "alpha"])]);
    assert_eq!(imported["dictionaryEntries"].as_array().unwrap().len(), 3);

    let h1 = syn(&v, "voice-input:history:record", &[json!("  你好 Cindy  ")]);
    assert!(h1.is_string());
    assert_eq!(syn(&v, "voice-input:history:record", &[json!("你好 Cindy")]), h1);
    assert_eq!(syn(&v, "voice-input:history:record", &[json!("   ")]), Value::Null);
    let upd = inv(&v, "voice-input:settings:update", &[json!({ "composerLongPressEnabled": false, "dictionaryEntries": [] })]);
    assert_eq!(upd["composerLongPressEnabled"], false);
    assert_eq!(upd["composerLongPressEnabledOverride"], false);
    assert_eq!(upd["dictionaryEntries"].as_array().unwrap().len(), 3, "词典只走语义化入口");

    // 有效值字段不落盘，只存 override
    let on_disk: Value = serde_json::from_str(&std::fs::read_to_string(d.join("voice-input-data.v1.json")).unwrap()).unwrap();
    assert_eq!(on_disk["version"], 1);
    assert!(on_disk["settings"].get("composerLongPressEnabled").is_none());
    assert_eq!(on_disk["settings"]["composerLongPressEnabledOverride"], false);
    assert!(on_disk["settings"].get("dictionarySyncEnabled").is_none());

    // 重启后仍在
    let v2 = VoiceInput::with_platform(&d, Arc::new(Rec), "darwin");
    let data = syn(&v2, "voice-input:data:get", &[]);
    assert_eq!(data["settings"]["dictionaryEntries"].as_array().unwrap().len(), 3);
    assert_eq!(data["history"][0]["text"], "你好 Cindy");
    assert_eq!(data["settings"]["composerLongPressEnabled"], false);
    let del = inv(&v2, "voice-input:dictionary:delete-entries", &[json!([id])]);
    assert!(!del["dictionaryEntries"].as_array().unwrap().iter().any(|e| e["id"] == id));
    syn(&v2, "voice-input:history:delete", &[h1]);
    assert_eq!(syn(&v2, "voice-input:history:get", &[]), json!([]));
    // 恢复默认：null 删除 override
    let reset = inv(&v2, "voice-input:settings:update", &[json!({ "composerLongPressEnabled": null })]);
    assert_eq!(reset["composerLongPressEnabled"], true);
    assert!(reset.get("composerLongPressEnabledOverride").is_none());
}

#[test]
fn edit_entry_learning_and_legacy_migration() {
    let (v, _d, _l) = rig("misc");
    let a = inv(&v, "voice-input:dictionary:add-entry", &[json!("Foo")]);
    let id = a["dictionaryEntries"][0]["id"].clone();
    let e = inv(&v, "voice-input:dictionary:edit-entry", &[json!({ "entryId": id, "text": "Foo2", "aliases": ["f", " ", "ff"] })]);
    assert_eq!(e["dictionaryEntries"][0]["text"], "Foo2");
    assert_eq!(e["dictionaryEntries"][0]["aliases"].as_array().unwrap().len(), 2);
    assert_eq!(inv(&v, "voice-input:dictionary:rename-entry", &[json!({ "entryId": "", "text": "x" })])["dictionaryEntries"].as_array().unwrap().len(), 1);

    let act = |a: &str, t: &str| json!({ "action": a, "term": t, "aliases": [], "type": "term", "confidence": "high" });
    let r = inv(&v, "voice-input:dictionary-learning:record-actions", &[json!([act("add_entry", "Learned"), json!(5), json!({ "action": 1, "term": "x" })])]);
    assert_eq!(r["newAutomaticEntries"].as_array().unwrap().len(), 1);
    assert_eq!(r["newAutomaticEntries"][0]["text"], "Learned");
    let none = inv(&v, "voice-input:dictionary-learning:record-actions", &[json!([])]);
    assert_eq!(none["newAutomaticEntries"], json!([]));
    // 删掉自动词条后进入抑制名单，再学不会回来
    let lid = r["settings"]["dictionaryEntries"].as_array().unwrap().iter().find(|e| e["text"] == "Learned").unwrap()["id"].clone();
    let after = inv(&v, "voice-input:dictionary:delete-entries", &[json!([lid])]);
    assert_eq!(after["suppressedAutomaticDictionaryTexts"], json!(["Learned"]));
    let again = inv(&v, "voice-input:dictionary-learning:record-actions", &[json!([act("add_entry", "learned")])]);
    assert_eq!(again["newAutomaticEntries"], json!([]));

    let migrated = syn(&v, "voice-input:data:migrate-legacy", &[json!({ "settingsRaw": "{\"language\":\"ja\"}", "historyRaw": "[{\"text\":\"old\",\"createdAt\":5,\"id\":\"o1\"}]" })]);
    assert_eq!(migrated["settings"]["language"], "ja");
    assert_eq!(migrated["history"][0]["id"], "o1");
    let twice = syn(&v, "voice-input:data:migrate-legacy", &[json!({ "settingsRaw": "{\"language\":\"de\"}" })]);
    assert_eq!(twice["settings"]["language"], "ja", "只迁移一次");
}

#[test]
fn history_get_limit_and_stubs() {
    let (v, _d, _l) = rig("hist");
    for t in ["a", "b", "c"] {
        syn(&v, "voice-input:history:record", &[json!(t)]);
    }
    assert_eq!(syn(&v, "voice-input:history:get", &[json!(2)]).as_array().unwrap().len(), 2);
    assert_eq!(syn(&v, "voice-input:history:get", &[json!(-1)]), json!([]));
    assert_eq!(syn(&v, "voice-input:history:get", &[]).as_array().unwrap().len(), 3);
    let first = syn(&v, "voice-input:history:get", &[])[0]["id"].clone();
    assert_eq!(syn(&v, "voice-input:history:update", &[json!({ "id": first, "text": " edited " })]), json!(true));
    assert_eq!(syn(&v, "voice-input:history:get", &[])[0]["text"], "edited");
    assert_eq!(syn(&v, "voice-input:history:get-for-refinement", &[]).as_array().unwrap().len(), 3);
    let p = syn(&v, "voice-input:get-system-permissions-cached", &[]);
    assert_eq!(p["microphone"]["status"], "unknown");
    let lin = VoiceInput::with_platform(&std::env::temp_dir(), Arc::new(Rec), "linux");
    assert_eq!(syn(&lin, "voice-input:get-system-permissions-cached", &[])["accessibility"]["status"], "not-required");
    assert_eq!(syn(&v, "voice-input:get-readiness-cached", &[])["ok"], false);
    assert_eq!(inv(&v, "voice-input:cancel", &[]), json!({ "ok": true }));
    for ch in INVOKE_CHANNELS {
        assert!(VoiceInput::owns_invoke(ch));
    }
    for ch in SYNC_CHANNELS {
        assert!(VoiceInput::owns_sync(ch));
    }
}
