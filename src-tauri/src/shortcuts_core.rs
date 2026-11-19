//! 应用级快捷键的纯函数：desktop shared/appShortcuts.ts + keyboardReserved.ts 的 Rust 版。
//! 默认组合表来自 `app_settings_data.json`（上游代码导出）；逻辑用 `app_settings_golden.json` 核对。

use crate::appearance_core::data;
use regex::Regex;
use serde_json::{json, Map, Value};
use std::sync::OnceLock;

#[derive(Clone, Debug, PartialEq)]
pub struct Combo {
    pub code: String,
    pub key: Option<String>,
    pub meta: bool,
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
}

impl Combo {
    pub fn from_value(v: &Value) -> Option<Combo> {
        let o = v.as_object()?;
        Some(Combo {
            code: o.get("code")?.as_str()?.to_string(),
            key: o.get("key").and_then(Value::as_str).map(String::from),
            meta: o.get("meta").and_then(Value::as_bool).unwrap_or(false),
            ctrl: o.get("ctrl").and_then(Value::as_bool).unwrap_or(false),
            alt: o.get("alt").and_then(Value::as_bool).unwrap_or(false),
            shift: o.get("shift").and_then(Value::as_bool).unwrap_or(false),
        })
    }
    pub fn to_value(&self) -> Value {
        let mut m = Map::new();
        m.insert("code".into(), json!(self.code));
        if let Some(k) = &self.key {
            m.insert("key".into(), json!(k));
        }
        m.insert("meta".into(), json!(self.meta));
        m.insert("ctrl".into(), json!(self.ctrl));
        m.insert("alt".into(), json!(self.alt));
        m.insert("shift".into(), json!(self.shift));
        Value::Object(m)
    }
    fn same(&self, o: &Combo) -> bool {
        self.code == o.code && self.meta == o.meta && self.ctrl == o.ctrl && self.alt == o.alt && self.shift == o.shift
    }
}

pub struct Def {
    pub id: String,
    pub scope: String,
    pub rebindable: bool,
    pub yields: bool,
    pub menu_backed: bool,
    platforms: Option<Vec<String>>,
    defaults: Value,
}

impl Def {
    pub fn available_on(&self, platform: &str) -> bool {
        self.platforms.as_ref().map(|p| p.iter().any(|x| x == platform)).unwrap_or(true)
    }
    pub fn default_combos(&self, platform: &str) -> Vec<Combo> {
        let key = if self.defaults.get(platform).is_some() { platform } else { "linux" };
        self.defaults[key].as_array().map(|a| a.iter().filter_map(Combo::from_value).collect()).unwrap_or_default()
    }
}

pub fn defs() -> &'static [Def] {
    static D: OnceLock<Vec<Def>> = OnceLock::new();
    D.get_or_init(|| {
        data()["shortcuts"]
            .as_array()
            .unwrap()
            .iter()
            .map(|d| Def {
                id: d["id"].as_str().unwrap().into(),
                scope: d["scope"].as_str().unwrap().into(),
                rebindable: d["rebindable"].as_bool().unwrap_or(false),
                yields: d["yieldsToUserBindings"].as_bool().unwrap_or(false),
                menu_backed: d["menuBacked"].as_bool().unwrap_or(false),
                platforms: d["platforms"].as_array().map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect()),
                defaults: d["defaults"].clone(),
            })
            .collect()
    })
}

pub fn def_of(id: &str) -> Option<&'static Def> {
    defs().iter().find(|d| d.id == id)
}

pub fn is_id(v: &str) -> bool {
    def_of(v).is_some()
}

const MODIFIER_CODES: [&str; 11] =
    ["AltLeft", "AltRight", "ControlLeft", "ControlRight", "MetaLeft", "MetaRight", "ShiftLeft", "ShiftRight", "Fn", "FnLock", "CapsLock"];

/// normalizeAppShortcutCombo：结构非法返回 None。
pub fn normalize_combo(raw: &Value) -> Option<Combo> {
    let o = raw.as_object()?;
    let code = o.get("code")?.as_str()?;
    if code.trim().is_empty() || MODIFIER_CODES.contains(&code) {
        return None;
    }
    let b = |k: &str| crate::appearance_core::truthy(o.get(k));
    Some(Combo {
        code: code.to_string(),
        key: o.get("key").and_then(Value::as_str).map(String::from),
        meta: b("meta"),
        ctrl: b("ctrl"),
        alt: b("alt"),
        shift: b("shift"),
    })
}

fn collides_with_fixed(c: &Combo, platform: &str) -> bool {
    defs().iter().any(|d| !d.rebindable && d.available_on(platform) && d.default_combos(platform).iter().any(|x| x.same(c)))
}

/// 覆盖项：None = 删除绑定（null）。保持写入顺序。
pub type Overrides = Vec<(String, Option<Combo>)>;

pub fn overrides_to_value(o: &Overrides) -> Value {
    let mut m = Map::new();
    for (k, v) in o {
        m.insert(k.clone(), v.as_ref().map(Combo::to_value).unwrap_or(Value::Null));
    }
    Value::Object(m)
}

pub fn normalize_overrides(raw: Option<&Value>, platform: &str) -> Overrides {
    let mut out: Overrides = Vec::new();
    let Some(o) = raw.and_then(Value::as_object) else { return out };
    for (key, value) in o {
        let Some(def) = def_of(key) else { continue };
        if !def.rebindable || !def.available_on(platform) {
            continue;
        }
        if value.is_null() {
            out.push((key.clone(), None));
            continue;
        }
        let Some(c) = normalize_combo(value) else { continue };
        if collides_with_fixed(&c, platform) {
            continue;
        }
        out.push((key.clone(), Some(c)));
    }
    out
}

fn get<'a>(o: &'a Overrides, id: &str) -> Option<&'a Option<Combo>> {
    o.iter().find(|(k, _)| k == id).map(|(_, v)| v)
}

pub fn scopes_overlap(a: &str, b: &str) -> bool {
    if a == "app" || b == "app" || a == "global" || b == "global" || a == b {
        return true;
    }
    let mut p = [a, b];
    p.sort();
    matches!(p.join(":").as_str(), "browser:workdir-doc" | "composer:workdir-doc")
}

/// getEffectiveAppShortcuts（yieldToCombos 恒为空：Tauri 侧无调用方传入）。
pub fn effective(o: &Overrides, platform: &str) -> Vec<(String, Vec<Combo>)> {
    let mut entries: Vec<(&str, &Combo)> = Vec::new();
    for d in defs() {
        if !d.available_on(platform) {
            continue;
        }
        if let Some(Some(c)) = get(o, &d.id) {
            entries.push((&d.scope, c));
        }
    }
    let mut out = Vec::new();
    for d in defs() {
        if !d.available_on(platform) {
            continue;
        }
        match get(o, &d.id) {
            Some(None) => out.push((d.id.clone(), vec![])),
            Some(Some(c)) => out.push((d.id.clone(), vec![c.clone()])),
            None => {
                let mut combos = d.default_combos(platform);
                if d.yields {
                    combos.retain(|c| !entries.iter().any(|(s, oc)| scopes_overlap(&d.scope, s) && oc.same(c)));
                }
                out.push((d.id.clone(), combos));
            }
        }
    }
    out
}

pub fn find_conflict(id: &str, c: &Combo, o: &Overrides, platform: &str) -> Option<String> {
    let me = def_of(id)?;
    let eff = effective(o, platform);
    for d in defs() {
        if d.id == id || !scopes_overlap(&me.scope, &d.scope) {
            continue;
        }
        if d.yields && get(o, &d.id).is_none() {
            continue;
        }
        if eff.iter().find(|(k, _)| *k == d.id).map(|(_, v)| v.iter().any(|x| x.same(c))).unwrap_or(false) {
            return Some(d.id.clone());
        }
    }
    None
}

fn fkey(code: &str) -> bool {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new(r"^F([1-9]|1[0-9]|2[0-4])$").unwrap()).is_match(code)
}

pub fn is_bindable(c: &Combo) -> bool {
    static R: OnceLock<Regex> = OnceLock::new();
    if c.meta || c.ctrl || c.alt {
        return true;
    }
    if c.shift {
        return R
            .get_or_init(|| Regex::new(r"^(Tab|ArrowUp|ArrowDown|ArrowLeft|ArrowRight|Home|End|PageUp|PageDown|F([1-9]|1[0-9]|2[0-4]))$").unwrap())
            .is_match(&c.code);
    }
    fkey(&c.code)
}

fn accel_key(code: &str) -> Option<String> {
    static L: OnceLock<Regex> = OnceLock::new();
    static D: OnceLock<Regex> = OnceLock::new();
    if let Some(m) = L.get_or_init(|| Regex::new(r"^Key([A-Z])$").unwrap()).captures(code) {
        return Some(m[1].to_string());
    }
    if let Some(m) = D.get_or_init(|| Regex::new(r"^Digit([0-9])$").unwrap()).captures(code) {
        return Some(m[1].to_string());
    }
    if fkey(code) {
        return Some(code.to_string());
    }
    Some(
        match code {
            "Comma" => ",",
            "Period" => ".",
            "Semicolon" => ";",
            "Quote" => "'",
            "Slash" => "/",
            "Backslash" => "\\",
            "BracketLeft" => "[",
            "BracketRight" => "]",
            "Backquote" => "`",
            "Minus" => "-",
            "Equal" => "=",
            "Space" => "Space",
            "Tab" => "Tab",
            "Enter" => "Enter",
            "Escape" => "Esc",
            "Backspace" => "Backspace",
            "Delete" => "Delete",
            "Home" => "Home",
            "End" => "End",
            "PageUp" => "PageUp",
            "PageDown" => "PageDown",
            "ArrowUp" => "Up",
            "ArrowDown" => "Down",
            "ArrowLeft" => "Left",
            "ArrowRight" => "Right",
            _ => return None,
        }
        .to_string(),
    )
}

pub fn to_accelerator(c: &Combo, platform: &str) -> Option<String> {
    let key = accel_key(&c.code)?;
    let mut p: Vec<&str> = Vec::new();
    if c.ctrl {
        p.push("Ctrl");
    }
    if c.alt {
        p.push("Alt");
    }
    if c.shift {
        p.push("Shift");
    }
    if c.meta {
        p.push(if platform == "darwin" { "Command" } else { "Super" });
    }
    p.push(&key);
    Some(p.join("+"))
}

pub fn global_usable(c: &Combo, platform: &str) -> bool {
    if to_accelerator(c, platform).is_none() {
        return false;
    }
    let (editing, others) = if platform == "darwin" { (c.meta, c.ctrl || c.alt || c.shift) } else { (c.ctrl, c.meta || c.alt || c.shift) };
    !(editing && !others && ["KeyA", "KeyC", "KeyV", "KeyX", "KeyY", "KeyZ"].contains(&c.code.as_str()))
}

// ── keyboardReserved.ts ──

fn mac_reserved(c: &Combo) -> bool {
    let code = c.code.as_str();
    let only_cmd = c.meta && !c.ctrl && !c.alt && !c.shift;
    let cmd_shift = c.meta && c.shift && !c.ctrl && !c.alt;
    let cmd_alt = c.meta && c.alt && !c.ctrl && !c.shift;
    let cmd_ctrl = c.meta && c.ctrl && !c.alt && !c.shift;
    if only_cmd {
        return ["KeyA", "KeyC", "KeyV", "KeyX", "KeyZ", "Comma", "KeyQ", "KeyH", "KeyM", "KeyW", "Digit0", "Equal", "Minus"].contains(&code);
    }
    if cmd_shift {
        return code == "KeyZ" || code == "Equal";
    }
    if cmd_alt {
        return code == "KeyH";
    }
    if cmd_ctrl {
        return code == "KeyF";
    }
    false
}

fn windows_reserved(c: &Combo) -> bool {
    static DIGIT: OnceLock<Regex> = OnceLock::new();
    let code = c.code.as_str();
    let ctrl_only = c.ctrl && !c.alt && !c.shift && !c.meta;
    let alt_only = c.alt && !c.ctrl && !c.shift && !c.meta;
    let ctrl_alt = c.ctrl && c.alt && !c.shift && !c.meta;
    if ctrl_only && code == "Space" {
        return true;
    }
    if alt_only && ["Tab", "F4", "Escape"].contains(&code) {
        return true;
    }
    if ctrl_alt && code == "Delete" {
        return true;
    }
    if !c.meta {
        return false;
    }
    let only_meta = !c.ctrl && !c.alt && !c.shift;
    let meta_shift = c.shift && !c.ctrl && !c.alt;
    let meta_ctrl = c.ctrl && !c.shift && !c.alt;
    let meta_alt = c.alt && !c.ctrl && !c.shift;
    let meta_ctrl_shift = c.ctrl && c.shift && !c.alt;
    if only_meta {
        if DIGIT.get_or_init(|| Regex::new(r"^Digit[0-9]$").unwrap()).is_match(code) {
            return true;
        }
        return [
            "KeyA", "KeyC", "KeyD", "KeyE", "KeyF", "KeyG", "KeyH", "KeyI", "KeyJ", "KeyK", "KeyL", "KeyM", "KeyN", "KeyO", "KeyP", "KeyQ", "KeyR", "KeyS",
            "KeyT", "KeyV", "KeyW", "KeyX", "KeyZ", "ArrowUp", "ArrowDown", "ArrowLeft", "ArrowRight", "Comma", "Period", "Semicolon", "Slash", "Tab", "Space",
            "Home", "Escape", "Minus", "Equal", "PrintScreen", "Pause",
        ]
        .contains(&code);
    }
    if meta_shift {
        return ["KeyA", "KeyS", "ArrowUp", "ArrowDown", "ArrowLeft", "ArrowRight", "Enter"].contains(&code);
    }
    if meta_ctrl {
        return ["KeyC", "KeyD", "KeyF", "KeyQ", "KeyV", "Enter", "Space", "ArrowLeft", "ArrowRight", "F4"].contains(&code);
    }
    if meta_alt {
        return ["KeyB", "KeyD", "KeyH", "KeyK", "ArrowUp", "ArrowDown"].contains(&code);
    }
    if meta_ctrl_shift {
        return code == "KeyB";
    }
    false
}

/// family：mac / windows / other。
pub fn system_reserved(c: &Combo, platform: &str) -> bool {
    match platform {
        "darwin" => mac_reserved(c),
        "win32" => windows_reserved(c),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ov(v: &Value) -> Overrides {
        v.as_object().unwrap().iter().map(|(k, x)| (k.clone(), if x.is_null() { None } else { Combo::from_value(x) })).collect()
    }

    #[test]
    fn golden_matches_upstream() {
        let g: Value = serde_json::from_str(include_str!("app_settings_golden.json")).unwrap();
        for (i, c) in g["shortcutOverrides"].as_array().unwrap().iter().enumerate() {
            let got = overrides_to_value(&normalize_overrides(Some(&c["input"]), c["platform"].as_str().unwrap()));
            assert_eq!(got, c["output"], "override case {i}");
        }
        let combos = g["comboCases"].as_array().unwrap();
        assert!(combos.len() > 700);
        for c in combos {
            let combo = Combo::from_value(&c["combo"]).unwrap();
            for (fam, plat) in [("mac", "darwin"), ("windows", "win32"), ("other", "linux")] {
                assert_eq!(system_reserved(&combo, plat), c["reserved"][fam], "reserved {fam} {}", c["combo"]);
            }
            for p in ["darwin", "win32", "linux"] {
                assert_eq!(to_accelerator(&combo, p).map(Value::from).unwrap_or(Value::Null), c["accel"][p], "accel {p} {}", c["combo"]);
            }
        }
        let conflicts = g["conflictCases"].as_array().unwrap();
        assert!(conflicts.len() > 1000);
        for c in conflicts {
            let combo = Combo::from_value(&c["combo"]).unwrap();
            let got = find_conflict(c["id"].as_str().unwrap(), &combo, &ov(&c["overrides"]), c["platform"].as_str().unwrap());
            assert_eq!(got.map(Value::from).unwrap_or(Value::Null), c["conflict"], "conflict {c}");
        }
        for c in g["effective"].as_array().unwrap() {
            let p = c["platform"].as_str().unwrap();
            let got: Map<String, Value> = effective(&ov(&c["overrides"]), p)
                .into_iter()
                .map(|(k, v)| (k, Value::Array(v.iter().map(Combo::to_value).collect())))
                .collect();
            assert_eq!(Value::Object(got), c["result"], "effective {p}");
        }
    }
}
