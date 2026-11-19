//! 外观设置的纯函数：desktop shared/appearanceSettings.ts + pluginAppearance.ts + plugin-protocol/appearance.ts 的 Rust 版。
//!
//! 静态表（壁纸 id、应用图标、默认值）来自 `app_settings_data.json`，由 scripts/gen-app-settings-data.mjs 用上游代码生成；
//! 规范化逻辑用 `app_settings_golden.json`（同一份上游代码的实际输出）逐条核对。

use regex::Regex;
use serde_json::{json, Map, Value};
use std::sync::OnceLock;

const DATA_JSON: &str = include_str!("app_settings_data.json");

pub fn data() -> &'static Value {
    static D: OnceLock<Value> = OnceLock::new();
    D.get_or_init(|| serde_json::from_str(DATA_JSON).expect("app_settings_data.json"))
}

/// JS 数字 → JSON：整数值输出为整数（与 JSON.stringify 一致）。
pub fn jnum(x: f64) -> Value {
    if x.is_finite() && x.fract() == 0.0 && x.abs() < 9.007_199_254_740_992e15 {
        json!(x as i64)
    } else {
        json!(x)
    }
}

/// Math.round：.5 向 +∞ 取整（Rust 的 round 是远离 0）。
fn js_round(x: f64) -> f64 {
    let f = x.floor();
    if x - f >= 0.5 { f + 1.0 } else { f }
}

fn round_decimal(v: f64, places: i32) -> f64 {
    let factor = 10f64.powi(places);
    js_round(v * factor) / factor
}

fn clamp_integer(v: f64, min: f64, max: f64, fallback: f64) -> f64 {
    if !v.is_finite() {
        return fallback;
    }
    js_round(v).clamp(min, max)
}

pub fn utf16_len(s: &str) -> usize {
    s.encode_utf16().count()
}

/// JS 真值判断。
pub fn truthy(v: Option<&Value>) -> bool {
    match v {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_f64().map(|f| f != 0.0 && !f.is_nan()).unwrap_or(true),
        Some(Value::String(s)) => !s.is_empty(),
        Some(_) => true,
    }
}

fn finite_num(v: Option<&Value>) -> Option<f64> {
    v.and_then(Value::as_f64).filter(|f| f.is_finite())
}

pub fn clamp_ui_size(v: f64) -> f64 {
    clamp_integer(v, 12.0, 24.0, data()["appearanceDefaults"]["uiSize"].as_f64().unwrap_or(14.0))
}
pub fn clamp_code_size(v: f64) -> f64 {
    clamp_integer(v, 10.0, 24.0, data()["appearanceDefaults"]["codeSize"].as_f64().unwrap_or(14.0))
}
pub fn clamp_terminal_size(v: f64) -> f64 {
    clamp_integer(v, 10.0, 24.0, 13.0)
}
pub fn clamp_window_zoom(v: f64) -> f64 {
    if !v.is_finite() {
        return 1.0;
    }
    let stepped = js_round(v / 0.1) * 0.1;
    round_decimal(stepped.clamp(0.5, 3.0), 2)
}
pub fn clamp_wallpaper_overlay(v: f64) -> f64 {
    if !v.is_finite() {
        return 0.2;
    }
    let stepped = js_round(v / 0.05) * 0.05;
    round_decimal(stepped.clamp(0.0, 1.0), 2)
}
pub fn clamp_wallpaper_visibility(v: f64) -> f64 {
    if v.is_finite() { round_decimal(v.clamp(0.0, 1.0), 2) } else { 0.0 }
}
pub fn clamp_wallpaper_blur(v: f64) -> f64 {
    clamp_integer(v, 0.0, 20.0, 0.0)
}

pub fn wallpaper_ids() -> Vec<&'static str> {
    data()["wallpaperIds"].as_array().map(|a| a.iter().filter_map(Value::as_str).collect()).unwrap_or_default()
}

fn re(cell: &'static OnceLock<Regex>, pat: &str) -> &'static Regex {
    cell.get_or_init(|| Regex::new(pat).unwrap())
}

pub fn normalize_custom_wallpaper_url(v: Option<&Value>) -> String {
    static R: OnceLock<Regex> = OnceLock::new();
    match v.and_then(Value::as_str) {
        Some(s) if re(&R, r"^cindy-media://client-wallpaper/[0-9a-f]{64}\.(webp|mp4)$").is_match(s) => s.to_string(),
        _ => String::new(),
    }
}

// ── plugin-protocol/appearance.ts ──

const COLORS: [&str; 7] = ["text", "secondaryText", "surface", "border", "userBubble", "userText", "assistantBubble"];

fn is_record(v: &Value) -> bool {
    v.is_object()
}

fn keys_ok(m: &Map<String, Value>, allowed: &[&str]) -> bool {
    !m.is_empty() && m.keys().all(|k| allowed.contains(&k.as_str()))
}

fn in_range(v: &Value, lo: f64, hi: f64) -> bool {
    v.as_f64().map(|f| f.is_finite() && f >= lo && f <= hi).unwrap_or(false)
}

pub fn is_plugin_resource(v: Option<&Value>) -> bool {
    static A: OnceLock<Regex> = OnceLock::new();
    static B: OnceLock<Regex> = OnceLock::new();
    static BAD: OnceLock<Regex> = OnceLock::new();
    let Some(s) = v.and_then(Value::as_str) else { return false };
    if utf16_len(s) > 1024 {
        return false;
    }
    if re(&A, r"^asset:[a-zA-Z0-9_-]{1,64}$").is_match(s) || re(&B, r"^cindy-media://blobs/[0-9a-f]{64}\.[a-z0-9]+$").is_match(s) {
        return true;
    }
    let Some(path) = s.strip_prefix("library:") else { return false };
    !path.is_empty()
        && !re(&BAD, r"[\\\x00-\x1f\x7f:%?#]").is_match(path)
        && path.split('/').all(|p| !p.is_empty() && p != "." && p != "..")
}

/// parsePluginAppearancePatch 只对 `{[field]: value}` 单键调用；这里直接校验该字段的值。
fn font_valid(role: &str, font: &Value) -> bool {
    static FAM: OnceLock<Regex> = OnceLock::new();
    let Some(f) = font.as_object() else { return false };
    if !keys_ok(f, &["family", "size", "resource", "weight", "style"]) {
        return false;
    }
    if let Some(fam) = f.get("family") {
        match fam.as_str() {
            Some(s) if utf16_len(s) <= 256 && re(&FAM, r#"^[\p{L}\p{N} _.,'"-]*$"#).is_match(s) => {}
            _ => return false,
        }
    }
    if let Some(r) = f.get("resource") {
        if !is_plugin_resource(Some(r)) {
            return false;
        }
    }
    if let Some(sz) = f.get("size") {
        let (lo, hi) = match role {
            "uiFont" => (12.0, 24.0),
            "codeFont" => (10.0, 24.0),
            _ => (12.0, 28.0),
        };
        if !in_range(sz, lo, hi) {
            return false;
        }
    }
    if let Some(w) = f.get("weight") {
        if !in_range(w, 100.0, 900.0) || w.as_f64().unwrap().fract() != 0.0 {
            return false;
        }
    }
    if let Some(st) = f.get("style") {
        if !matches!(st.as_str(), Some("normal") | Some("italic")) {
            return false;
        }
    }
    !(f.contains_key("resource") && f.contains_key("family"))
}

fn theme_valid(theme: &Value) -> bool {
    static HEX: OnceLock<Regex> = OnceLock::new();
    let Some(t) = theme.as_object() else { return false };
    if !keys_ok(t, &["light", "dark"]) {
        return false;
    }
    t.values().all(|mode| match mode.as_object() {
        Some(m) => keys_ok(m, &COLORS) && m.values().all(|c| c.as_str().map(|s| re(&HEX, r"^#[0-9a-fA-F]{6}$").is_match(s)).unwrap_or(false)),
        None => false,
    })
}

fn section_valid(section: &str, v: &Value) -> bool {
    let allowed: &[&str] = if section == "reading" { &["bodyLineHeight", "codeLineHeight", "maxWidth"] } else { &["messageGap", "messagePadding", "messageRadius"] };
    let Some(m) = v.as_object() else { return false };
    if !keys_ok(m, allowed) {
        return false;
    }
    m.iter().all(|(k, n)| {
        let (lo, hi) = match k.as_str() {
            "bodyLineHeight" => (1.2, 2.4),
            "codeLineHeight" => (1.1, 2.4),
            "maxWidth" => (320.0, 1400.0),
            "messageGap" => (0.0, 48.0),
            _ => (0.0, 32.0),
        };
        in_range(n, lo, hi)
    })
}

// ── pluginAppearance.ts normalizePluginAppearanceSettings ──

fn hex64(v: Option<&Value>) -> Option<&str> {
    static R: OnceLock<Regex> = OnceLock::new();
    v.and_then(Value::as_str).filter(|s| re(&R, r"^[a-f0-9]{64}$").is_match(s))
}

fn normalize_font_resource(value: &Value) -> Option<Value> {
    static ASSET: OnceLock<Regex> = OnceLock::new();
    let v = value.as_object()?;
    let source = v.get("source");
    let hash = hex64(v.get("hash"));
    let family = v.get("family").and_then(Value::as_str);
    if let (Some(src), Some(hash)) = (source.and_then(Value::as_object), hash) {
        let inst = src.get("instanceId").and_then(Value::as_str).filter(|s| !s.is_empty() && utf16_len(s) <= 256);
        let key = hex64(src.get("key"));
        if let (Some(inst), Some(key)) = (inst, key) {
            if is_plugin_resource(v.get("reference")) && family == Some(format!("CindyPlugin_{hash}").as_str()) {
                return Some(json!({ "source": { "instanceId": inst, "key": key }, "reference": v["reference"], "hash": hash, "family": family }));
            }
        }
        let theme = src.get("themeId").and_then(Value::as_str).filter(|s| utf16_len(s) <= 512);
        let pkg = hex64(src.get("packageSha256"));
        let reference = v.get("reference").and_then(Value::as_str).filter(|s| re(&ASSET, r"^asset:[a-zA-Z0-9_-]{1,64}$").is_match(s));
        if let (Some(theme), Some(pkg), Some(reference)) = (theme, pkg, reference) {
            if family == Some(format!("CindyTheme_{hash}").as_str()) {
                return Some(json!({ "source": { "themeId": theme, "packageSha256": pkg }, "reference": reference, "hash": hash, "family": family }));
            }
        }
    }
    None
}

pub fn normalize_plugin_settings(raw: &Map<String, Value>) -> Map<String, Value> {
    let mut out = Map::new();
    for field in ["uiFontResource", "codeFontResource", "bodyFontResource"] {
        if let Some(r) = raw.get(field).and_then(normalize_font_resource) {
            out.insert(field.into(), r);
        }
    }
    for field in ["reading", "contentStyle", "themeColors", "bodyFont", "uiFontTraits", "codeFontTraits"] {
        let Some(v) = raw.get(field) else { continue };
        let ok = match field {
            "themeColors" => theme_valid(v),
            "reading" | "contentStyle" => section_valid(field, v),
            "bodyFont" => font_valid("bodyFont", v) && v.get("resource").is_none(),
            _ => {
                let role = if field == "uiFontTraits" { "uiFont" } else { "codeFont" };
                font_valid(role, v) && ["family", "size", "resource"].iter().all(|k| v.get(*k).is_none())
            }
        };
        if ok {
            out.insert(field.into(), v.clone());
        }
    }
    out
}

// ── appearanceSettings.ts normalizeAppearanceSettings ──

/// JS String.prototype.trim：空白集与 Rust 的 char::is_whitespace 只差 U+0085（JS 不算）和 U+FEFF（JS 算）。
pub fn js_trim(s: &str) -> &str {
    s.trim_matches(|c: char| (c.is_whitespace() && c != '\u{85}') || c == '\u{feff}')
}

fn normalize_family(v: Option<&Value>) -> String {
    match v.and_then(Value::as_str) {
        Some(s) => {
            let units: Vec<u16> = js_trim(s).encode_utf16().take(256).collect();
            String::from_utf16_lossy(&units)
        }
        None => String::new(),
    }
}

pub fn normalize_appearance(raw: &Value) -> Value {
    let defaults = &data()["appearanceDefaults"];
    let Some(value) = raw.as_object() else { return defaults.clone() };
    let mut out = normalize_plugin_settings(value);
    if let Some(keys) = value.get("customizedKeys").and_then(Value::as_array) {
        if keys.len() <= 128 && keys.iter().all(Value::is_string) {
            out.insert("customizedKeys".into(), Value::Array(keys.clone()));
        }
    }
    let n = |k: &str| finite_num(value.get(k));
    out.insert("density".into(), jnum(n("density").map(|d| d.clamp(0.75, 1.25)).unwrap_or(1.0)));
    out.insert("motion".into(), jnum(n("motion").map(|d| d.clamp(0.0, 2.0)).unwrap_or(1.0)));
    if truthy(value.get("terminalFontResource")) {
        if let Some(r) = value.get("terminalFontResource").and_then(normalize_font_resource) {
            out.insert("terminalFontResource".into(), r);
        }
    }
    let fallback = normalize_custom_wallpaper_url(value.get("customWallpaperFallbackUrl"));
    if !fallback.is_empty() {
        out.insert("customWallpaperFallbackUrl".into(), json!(fallback));
    }
    for k in ["uiFamily", "codeFamily", "terminalFamily"] {
        out.insert(k.into(), json!(normalize_family(value.get(k))));
    }
    let sized = |k: &str, f: fn(f64) -> f64| -> Value {
        match value.get(k).and_then(Value::as_f64) {
            Some(x) => jnum(f(x)),
            None => defaults[k].clone(),
        }
    };
    out.insert("uiSize".into(), sized("uiSize", clamp_ui_size));
    out.insert("codeSize".into(), sized("codeSize", clamp_code_size));
    out.insert("terminalSize".into(), sized("terminalSize", clamp_terminal_size));
    out.insert("windowZoom".into(), sized("windowZoom", clamp_window_zoom));
    let wid = value.get("wallpaperId").and_then(Value::as_str).filter(|s| wallpaper_ids().contains(s));
    out.insert("wallpaperId".into(), json!(wid.unwrap_or("none")));
    out.insert("wallpaperOverlay".into(), sized("wallpaperOverlay", clamp_wallpaper_overlay));
    out.insert("wallpaperMotion".into(), json!(if value.get("wallpaperMotion").and_then(Value::as_str) == Some("dynamic") { "dynamic" } else { "static" }));
    let icon = value.get("appIcon").and_then(Value::as_str).filter(|s| data()["appIcons"].as_array().map(|a| a.iter().any(|i| i == s)).unwrap_or(false));
    out.insert("appIcon".into(), json!(icon.unwrap_or("default")));
    if let Some(x) = n("wallpaperBlur") {
        out.insert("wallpaperBlur".into(), jnum(clamp_wallpaper_blur(x)));
    }
    if let Some(x) = n("wallpaperVisibility") {
        out.insert("wallpaperVisibility".into(), jnum(clamp_wallpaper_visibility(x)));
    }
    if let Some(x) = n("desktopVisibility") {
        out.insert("desktopVisibility".into(), jnum(clamp_wallpaper_visibility(x)));
    }
    out.insert("customWallpaperUrl".into(), json!(normalize_custom_wallpaper_url(value.get("customWallpaperUrl"))));
    Value::Object(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn golden_matches_upstream() {
        let g: Value = serde_json::from_str(include_str!("app_settings_golden.json")).unwrap();
        let cases = g["appearance"].as_array().unwrap();
        assert!(cases.len() > 20);
        for (i, c) in cases.iter().enumerate() {
            assert_eq!(normalize_appearance(&c["input"]), c["output"], "case {i}: {}", c["input"]);
        }
    }

    #[test]
    fn js_rounding() {
        assert_eq!(js_round(-2.5), -2.0);
        assert_eq!(js_round(2.5), 3.0);
        assert_eq!(clamp_window_zoom(1.234), 1.2);
        assert_eq!(clamp_wallpaper_overlay(0.337), 0.35);
    }
}
