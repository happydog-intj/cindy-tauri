//! 应用级设置：外观 / 快捷键 / 本地主题 / 更新通道 / 外设占位（原 sidecar settings/app-settings.mjs）。
//!
//! 形状与校验对齐 desktop，纯函数部分在 appearance_core.rs / shortcuts_core.rs（有上游黄金样例）。
//! `device-link:set-keep-awake` 留在 sidecar：device-link-settings.json 由 device-link.mjs 独占读写。

use crate::appearance_core::{self as ap, js_trim, jnum, utf16_len};
use crate::maker_host::Emitter;
use crate::override_file::{write_json_atomic, OverrideFile};
use crate::shortcuts_core::{self as sc, Combo, Overrides};
use serde_json::{json, Map, Value};
use std::path::Path;
use std::sync::{Arc, Mutex};

fn fail<T>(code: &str, msg: impl AsRef<str>) -> Result<T, String> {
    Err(format!("[{code}] {}", msg.as_ref()))
}

fn require_bool(v: Option<&Value>, name: &str) -> Result<bool, String> {
    match v.and_then(Value::as_bool) {
        Some(b) => Ok(b),
        None => fail("INVALID_PARAMS", format!("{name} must be a boolean")),
    }
}

/// JS `String(x)`（只覆盖 id 会出现的类型）。
fn js_string(v: Option<&Value>) -> String {
    match v {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Null) => "null".into(),
        Some(Value::Number(n)) => n.to_string(),
        Some(Value::Bool(b)) => b.to_string(),
        Some(Value::Array(_)) => "".into(),
        Some(_) => "[object Object]".into(),
        None => "undefined".into(),
    }
}

pub const INVOKE_CHANNELS: &[&str] = &[
    "appearance-settings:get",
    "appearance-settings:set-patch",
    "appearance-settings:reset",
    "appearance-settings:import-wallpaper",
    "appearance-settings:remove-wallpaper",
    "local-themes:list",
    "app-shortcuts:set-override",
    "app-shortcuts:clear-override",
    "app-shortcuts:reset-all",
    "app-locale:set-classical-style",
    "menubar:get-state",
    "menubar:set-enabled",
    "login-item:get",
    "login-item:set",
    "update-channel-settings-get",
    "update-channel-settings-set",
    "update-channel-settings-reset",
    "update-channel-probe-beta",
    "update-auto-settings-get",
    "update-auto-settings-set",
    "update-auto-settings-reset",
    "worklouder-codex:get-state",
    "codex-micro-guard:get-state",
    "xbox-gamepad:get-state",
];
pub const SYNC_CHANNELS: &[&str] = &["appearance-settings:get-sync", "local-themes:list-sync", "app-shortcuts:get"];
pub const SEND_CHANNELS: &[&str] = &["app-shortcuts:set-recording"];

pub struct AppSettings {
    appearance: OverrideFile,
    custom_wallpaper: OverrideFile,
    channel: OverrideFile,
    auto_update: OverrideFile,
    shortcuts_file: std::path::PathBuf,
    shortcuts: Mutex<Option<Overrides>>,
    emitter: Arc<dyn Emitter>,
    platform: String,
    beta_supported: bool,
}

pub fn host_platform() -> &'static str {
    match std::env::consts::OS {
        "macos" => "darwin",
        "windows" => "win32",
        o if o == "linux" => "linux",
        _ => "linux",
    }
}

pub fn host_arch() -> &'static str {
    match std::env::consts::ARCH {
        "x86_64" => "x64",
        "aarch64" => "arm64",
        a => a,
    }
}

impl AppSettings {
    pub fn new(settings_dir: &Path, emitter: Arc<dyn Emitter>) -> Self {
        Self::with_platform(settings_dir, emitter, host_platform(), host_arch())
    }

    pub fn with_platform(settings_dir: &Path, emitter: Arc<dyn Emitter>, platform: &str, arch: &str) -> Self {
        let f = |n: &str| settings_dir.join(n);
        // desktop main/appearance-settings-store.ts：16KB 上限、坏文件保留。
        let appearance = OverrideFile::new(f("appearance-settings.json"), ap::data()["appearanceDefaults"].clone(), ap::normalize_appearance)
            .max_bytes(16 * 1024)
            .preserving_unreadable();
        let custom_wallpaper = OverrideFile::new(f("custom-wallpaper.json"), json!({ "url": "" }), |r| json!({ "url": ap::normalize_custom_wallpaper_url(r.get("url")) }))
            .max_bytes(4096)
            .preserving_unreadable();
        let channel = OverrideFile::new(f("update-channel-settings.json"), json!({ "enableBeta": false, "orgDefaultEnableBeta": false }), |r| {
            json!({ "enableBeta": r.get("enableBeta") == Some(&json!(true)), "orgDefaultEnableBeta": r.get("orgDefaultEnableBeta") == Some(&json!(true)) })
        });
        let auto_update = OverrideFile::new(f("auto-update-settings.json"), json!({ "autoRelaunchOnIdle": false }), |r| {
            json!({ "autoRelaunchOnIdle": r.get("autoRelaunchOnIdle").and_then(Value::as_bool).unwrap_or(false) })
        });
        Self {
            appearance,
            custom_wallpaper,
            channel,
            auto_update,
            shortcuts_file: f("app-shortcuts.v1.json"),
            shortcuts: Mutex::new(None),
            emitter,
            platform: platform.to_string(),
            beta_supported: platform != "linux" || arch == "x64" || arch == "arm64",
        }
    }

    pub fn owns_invoke(channel: &str) -> bool {
        INVOKE_CHANNELS.contains(&channel)
    }
    pub fn owns_sync(channel: &str) -> bool {
        SYNC_CHANNELS.contains(&channel)
    }
    pub fn owns_send(channel: &str) -> bool {
        SEND_CHANNELS.contains(&channel)
    }

    /// update_machine 查「空闲时自动重启」。
    pub fn auto_relaunch_on_idle(&self) -> bool {
        self.auto_update.read()["autoRelaunchOnIdle"].as_bool().unwrap_or(false)
    }

    // ── 外观 ──

    fn read_appearance(&self) -> Value {
        let mut v = self.appearance.read();
        let url = self.custom_wallpaper.read()["url"].as_str().unwrap_or("").to_string();
        if let Some(o) = v.as_object_mut() {
            if o.get("wallpaperId") == Some(&json!("custom")) && url.is_empty() {
                o.insert("wallpaperId".into(), json!("none"));
            }
            o.insert("customWallpaperUrl".into(), json!(url));
        }
        v
    }

    fn broadcast_appearance(&self, s: &Value) {
        self.emitter.emit("appearance-settings:changed", s.clone());
    }

    fn appearance_and_broadcast(&self) -> Value {
        let s = self.read_appearance();
        self.broadcast_appearance(&s);
        s
    }

    // ── 快捷键 ──

    fn load_shortcuts(&self) -> Overrides {
        let mut g = self.shortcuts.lock().unwrap();
        if g.is_none() {
            let parsed = crate::override_file::read_json(&self.shortcuts_file, None);
            *g = Some(match parsed {
                Ok(Some(v)) => sc::normalize_overrides(v.get("overrides"), &self.platform),
                Ok(None) => Vec::new(),
                Err(e) => {
                    eprintln!("[settings] app shortcut overrides read failed, using defaults: {e}");
                    Vec::new()
                }
            });
        }
        g.clone().unwrap()
    }

    fn replace_shortcuts(&self, next: Overrides) -> Result<(), String> {
        let body = json!({ "version": 1, "overrides": sc::overrides_to_value(&next) });
        if let Err(e) = write_json_atomic(&self.shortcuts_file, &body, None) {
            return fail("INTERNAL", format!("failed to save app shortcut overrides: {e}"));
        }
        let wire = sc::overrides_to_value(&next);
        *self.shortcuts.lock().unwrap() = Some(next);
        self.emitter.emit("app-shortcuts:changed", json!({ "overrides": wire, "globalFailures": [] }));
        Ok(())
    }

    /// AppShortcutStore.setOverride：返回拒绝原因或 None。
    fn set_shortcut_override(&self, id: Option<&Value>, combo: Option<&Value>) -> Result<Option<&'static str>, String> {
        let Some(id_s) = id.and_then(Value::as_str).filter(|s| sc::is_id(s)) else { return Ok(Some("unknown-id")) };
        let def = sc::def_of(id_s).unwrap();
        if !def.rebindable {
            return Ok(Some("not-rebindable"));
        }
        if !def.available_on(&self.platform) {
            return Ok(Some("platform-unavailable"));
        }
        let mut current = self.load_shortcuts();
        let upsert = |cur: &mut Overrides, v: Option<Combo>| match cur.iter_mut().find(|(k, _)| k == id_s) {
            Some(slot) => slot.1 = v,
            None => cur.push((id_s.to_string(), v)),
        };
        if matches!(combo, Some(Value::Null)) {
            upsert(&mut current, None);
            self.replace_shortcuts(current)?;
            return Ok(None);
        }
        let Some(n) = combo.and_then(sc::normalize_combo) else { return Ok(Some("invalid-combo")) };
        if !sc::is_bindable(&n) {
            return Ok(Some("not-bindable"));
        }
        if sc::system_reserved(&n, &self.platform) {
            return Ok(Some("system-reserved"));
        }
        if def.menu_backed && self.platform == "darwin" && sc::to_accelerator(&n, "darwin").is_none() {
            return Ok(Some("menu-inexpressible"));
        }
        if def.scope == "global" && !sc::global_usable(&n, &self.platform) {
            return Ok(Some("global-unusable"));
        }
        if sc::find_conflict(id_s, &n, &current, &self.platform).is_some() {
            return Ok(Some("conflict"));
        }
        upsert(&mut current, Some(n));
        self.replace_shortcuts(current)?;
        Ok(None)
    }

    fn rejection_message(reason: &str, id: Option<&Value>) -> String {
        let id = js_string(id);
        match reason {
            "unknown-id" => format!("unknown app shortcut id: {id}"),
            "not-rebindable" => format!("app shortcut is not rebindable: {id}"),
            "platform-unavailable" => format!("app shortcut not available on this platform: {id}"),
            "invalid-combo" => "invalid shortcut combo".into(),
            "not-bindable" => "shortcut combo not bindable (bare keys limited to F1-F24 / Shift+non-printable)".into(),
            "system-reserved" => "shortcut combo is reserved by the operating system".into(),
            "menu-inexpressible" => "shortcut combo cannot be expressed as a menu accelerator on macOS".into(),
            "global-unusable" => "shortcut combo cannot be used as a system-wide shortcut".into(),
            _ => "shortcut combo conflicts with another shortcut in an overlapping scope".into(),
        }
    }

    // ── 更新通道 ──

    fn channel_wire(&self) -> Value {
        let s = self.channel.read_state();
        let custom = s.customized_keys.iter().any(|k| k == "enableBeta");
        let beta = self.beta_supported && s.value[if custom { "enableBeta" } else { "orgDefaultEnableBeta" }].as_bool().unwrap_or(false);
        json!({ "enableBeta": beta, "isCustomized": custom })
    }

    fn auto_wire(&self) -> Value {
        let s = self.auto_update.read_state();
        json!({
            "autoRelaunchOnIdle": s.value["autoRelaunchOnIdle"],
            "isCustomized": s.is_customized,
            "defaultAutoRelaunchOnIdle": s.defaults["autoRelaunchOnIdle"],
        })
    }

    // ── 入口 ──

    pub fn sync(&self, channel: &str, _args: &[Value]) -> Option<Value> {
        Some(match channel {
            "appearance-settings:get-sync" => self.read_appearance(),
            "local-themes:list-sync" => json!({ "success": true, "themes": [], "diagnostics": [] }),
            "app-shortcuts:get" => json!({ "overrides": sc::overrides_to_value(&self.load_shortcuts()), "platform": self.platform, "globalFailures": [] }),
            _ => return None,
        })
    }

    pub fn invoke(&self, channel: &str, args: &[Value]) -> Option<Result<Value, String>> {
        let a0 = args.first();
        Some(match channel {
            "appearance-settings:get" => {
                let s = self.appearance.read_state();
                let value = self.read_appearance();
                self.broadcast_appearance(&value);
                Ok(json!({ "value": value, "isCustomized": s.is_customized, "defaults": s.defaults, "customizedKeys": s.customized_keys }))
            }
            "appearance-settings:set-patch" => parse_appearance_patch(a0).and_then(|p| {
                self.appearance.write_patch(&p, false)?;
                Ok(self.appearance_and_broadcast())
            }),
            "appearance-settings:reset" => self.appearance.reset().map(|_| self.appearance_and_broadcast()),
            // 自定义壁纸依赖 cindy-media:// 协议与图片转码，Tauri 版暂无；null 即 desktop「用户取消」形状。
            "appearance-settings:import-wallpaper" => Ok(Value::Null),
            "appearance-settings:remove-wallpaper" => self.custom_wallpaper.reset().map(|_| self.appearance_and_broadcast()),
            "local-themes:list" => Ok(json!({ "success": true, "themes": [], "diagnostics": [] })),
            "app-shortcuts:set-override" => match self.set_shortcut_override(a0, args.get(1)) {
                Err(e) => Err(e),
                Ok(Some(reason)) => fail("INVALID_PARAMS", Self::rejection_message(reason, a0)),
                Ok(None) => Ok(json!({ "overrides": sc::overrides_to_value(&self.load_shortcuts()) })),
            },
            "app-shortcuts:clear-override" => {
                let mut cur = self.load_shortcuts();
                let hit = a0.and_then(Value::as_str).filter(|s| sc::is_id(s)).filter(|s| cur.iter().any(|(k, _)| k == s));
                if let Some(id) = hit {
                    cur.retain(|(k, _)| k != id);
                    if let Err(e) = self.replace_shortcuts(cur) {
                        return Some(Err(e));
                    }
                }
                Ok(json!({ "overrides": sc::overrides_to_value(&self.load_shortcuts()) }))
            }
            "app-shortcuts:reset-all" => {
                if !self.load_shortcuts().is_empty() {
                    if let Err(e) = self.replace_shortcuts(Vec::new()) {
                        return Some(Err(e));
                    }
                }
                Ok(json!({ "overrides": {} }))
            }
            // desktop bootstrap-electron.ts:3190：非目录内的风格静默忽略；Tauri 版暂无回复脚注消费方。
            "app-locale:set-classical-style" => Ok(json!({ "ok": true })),
            "menubar:get-state" => Ok(json!({ "available": false, "enabled": false })),
            "menubar:set-enabled" => require_bool(a0, "enabled").map(|_| json!({ "available": false, "enabled": false })),
            "login-item:get" => Ok(json!({
                "available": false,
                "unavailableReason": if self.platform != "win32" && self.platform != "darwin" { "platform" } else { "development" },
                "enabled": false,
                "requiresApproval": false,
            })),
            "login-item:set" => require_bool(a0, "enabled").and_then(|_| fail("UNSUPPORTED_CAPABILITY", "Login startup requires an installed Windows or macOS app")),
            "update-channel-settings-get" => Ok(self.channel_wire()),
            "update-channel-settings-set" => self.channel_set(a0),
            "update-channel-settings-reset" => match self.channel.reset() {
                Ok(_) => {
                    let w = self.channel_wire();
                    self.emitter.emit("update-channel-settings", w.clone());
                    Ok(w)
                }
                Err(_) => fail("INTERNAL", "failed to reset update channel settings"),
            },
            "update-channel-probe-beta" => Ok(json!({ "available": false })),
            "update-auto-settings-get" => Ok(self.auto_wire()),
            "update-auto-settings-set" => match a0 {
                Some(p) if p.is_object() || p.is_array() => match p.get("autoRelaunchOnIdle").and_then(Value::as_bool) {
                    Some(b) => self.auto_update.write_patch(&json!({ "autoRelaunchOnIdle": b }), false).map(|_| self.auto_wire()),
                    None => fail("INVALID_PARAMS", "autoRelaunchOnIdle required (boolean)"),
                },
                _ => fail("INVALID_PARAMS", "auto update settings payload required"),
            },
            "update-auto-settings-reset" => self.auto_update.reset().map(|_| self.auto_wire()),
            // Tauri 版没有 HID / 手柄宿主：返回 desktop「SDK 不可用」快照（上游代码导出）。
            "worklouder-codex:get-state" => Ok(ap::data()["workLouder"].clone()),
            "codex-micro-guard:get-state" => Ok(json!({ "supported": false, "enabled": false, "status": "unsupported" })),
            "xbox-gamepad:get-state" => Ok(ap::data()["xbox"].clone()),
            _ => return None,
        })
    }

    fn channel_set(&self, payload: Option<&Value>) -> Result<Value, String> {
        if !self.beta_supported {
            return fail("INVALID_PARAMS", "This build does not support the beta update channel");
        }
        let Some(p) = payload.filter(|p| p.is_object() || p.is_array()) else {
            return fail("INVALID_PARAMS", "update channel settings payload required");
        };
        let Some(b) = p.get("enableBeta").and_then(Value::as_bool) else {
            return fail("INVALID_PARAMS", "enableBeta required (boolean)");
        };
        if self.channel.write_patch(&json!({ "enableBeta": b }), true).is_err() {
            return fail("INTERNAL", "failed to write update channel settings");
        }
        let w = self.channel_wire();
        self.emitter.emit("update-channel-settings", w.clone());
        Ok(w)
    }
}

/// desktop main/appearance-settings-ipc.ts parsePatch（customWallpaperUrl 只读，不接受写入）。
fn parse_appearance_patch(raw: Option<&Value>) -> Result<Value, String> {
    const ALLOWED: [&str; 12] = [
        "uiFamily", "codeFamily", "uiSize", "codeSize", "terminalFamily", "terminalSize", "windowZoom", "wallpaperId", "wallpaperOverlay",
        "wallpaperVisibility", "wallpaperBlur", "wallpaperMotion",
    ];
    let Some(o) = raw.and_then(Value::as_object) else { return fail("INVALID_PARAMS", "appearance patch must be an object") };
    for k in o.keys() {
        if !ALLOWED.contains(&k.as_str()) {
            return fail("INVALID_PARAMS", format!("unknown appearance field: {k}"));
        }
    }
    let family = |v: &Value, name: &str| -> Result<Value, String> {
        match v.as_str() {
            Some(s) if utf16_len(s) <= 256 => Ok(json!(js_trim(s))),
            _ => fail("INVALID_PARAMS", format!("{name} must be a string up to 256 characters")),
        }
    };
    let num = |v: &Value, name: &str, clamp: fn(f64) -> f64| -> Result<Value, String> {
        match v.as_f64().filter(|f| f.is_finite()) {
            Some(f) => Ok(jnum(clamp(f))),
            None => fail("INVALID_PARAMS", format!("{name} must be a finite number")),
        }
    };
    let mut patch = Map::new();
    for k in ["uiFamily", "codeFamily", "terminalFamily"] {
        if let Some(v) = o.get(k) {
            patch.insert(k.into(), family(v, k)?);
        }
    }
    for (k, f) in [
        ("uiSize", ap::clamp_ui_size as fn(f64) -> f64),
        ("codeSize", ap::clamp_code_size),
        ("terminalSize", ap::clamp_terminal_size),
        ("windowZoom", ap::clamp_window_zoom),
    ] {
        if let Some(v) = o.get(k) {
            patch.insert(k.into(), num(v, k, f)?);
        }
    }
    if let Some(v) = o.get("wallpaperId") {
        match v.as_str().filter(|s| ap::wallpaper_ids().contains(s)) {
            Some(s) => patch.insert("wallpaperId".into(), json!(s)),
            None => return fail("INVALID_PARAMS", "wallpaperId is not supported"),
        };
    }
    if let Some(v) = o.get("wallpaperOverlay") {
        patch.insert("wallpaperOverlay".into(), num(v, "wallpaperOverlay", ap::clamp_wallpaper_overlay)?);
    }
    if let Some(v) = o.get("wallpaperMotion") {
        match v.as_str().filter(|s| *s == "static" || *s == "dynamic") {
            Some(s) => patch.insert("wallpaperMotion".into(), json!(s)),
            None => return fail("INVALID_PARAMS", "wallpaperMotion is not supported"),
        };
    }
    for (k, f) in [("wallpaperVisibility", ap::clamp_wallpaper_visibility as fn(f64) -> f64), ("wallpaperBlur", ap::clamp_wallpaper_blur)] {
        if let Some(v) = o.get(k) {
            patch.insert(k.into(), if v.is_null() { Value::Null } else { num(v, k, f)? });
        }
    }
    Ok(Value::Object(patch))
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Rec(Mutex<Vec<(String, Value)>>);
    impl Emitter for Rec {
        fn emit(&self, channel: &str, arg: Value) {
            self.0.lock().unwrap().push((channel.into(), arg));
        }
    }

    fn rig(name: &str, platform: &str, arch: &str) -> (AppSettings, Arc<Rec>, std::path::PathBuf) {
        let d = std::env::temp_dir().join(format!("app-settings-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        let rec = Arc::new(Rec(Mutex::new(Vec::new())));
        (AppSettings::with_platform(&d, rec.clone(), platform, arch), rec, d)
    }

    fn call(s: &AppSettings, ch: &str, args: &[Value]) -> Result<Value, String> {
        s.invoke(ch, args).expect("owned channel")
    }

    #[test]
    fn appearance_patch_roundtrip_and_restart() {
        let (s, rec, d) = rig("ap", "darwin", "arm64");
        assert_eq!(call(&s, "appearance-settings:get", &[]).unwrap()["value"]["uiSize"], 14);
        let r = call(&s, "appearance-settings:set-patch", &[json!({ "uiSize": 15.4, "uiFamily": "  Foo " })]).unwrap();
        assert_eq!(r["uiSize"], 15);
        assert_eq!(r["uiFamily"], "Foo");
        assert!(rec.0.lock().unwrap().iter().any(|(c, _)| c == "appearance-settings:changed"));
        let e = call(&s, "appearance-settings:set-patch", &[json!({ "uiFontSize": 15 })]).unwrap_err();
        assert!(e.contains("INVALID_PARAMS") && e.contains("unknown appearance field"), "{e}");
        assert!(call(&s, "appearance-settings:set-patch", &[json!({ "uiSize": "x" })]).is_err());
        // 重新加载后仍在；回默认值的 key 不留在文件里
        let s2 = AppSettings::with_platform(&d, rec.clone(), "darwin", "arm64");
        assert_eq!(s2.sync("appearance-settings:get-sync", &[]).unwrap()["uiSize"], 15);
        call(&s2, "appearance-settings:set-patch", &[json!({ "uiSize": 14 })]).unwrap();
        let on_disk = std::fs::read_to_string(d.join("appearance-settings.json")).unwrap();
        assert!(!on_disk.contains("uiSize"), "{on_disk}");
        assert!(call(&s2, "appearance-settings:reset", &[]).is_ok());
        assert!(!d.join("appearance-settings.json").exists());
    }

    #[test]
    fn custom_wallpaper_without_url_falls_back_to_none() {
        let (s, _r, _d) = rig("wp", "darwin", "arm64");
        let r = call(&s, "appearance-settings:set-patch", &[json!({ "wallpaperId": "custom" })]).unwrap();
        assert_eq!(r["wallpaperId"], "none");
        assert_eq!(call(&s, "appearance-settings:import-wallpaper", &[]).unwrap(), Value::Null);
        assert!(call(&s, "appearance-settings:set-patch", &[json!({ "wallpaperId": "nope" })]).is_err());
        let r = call(&s, "appearance-settings:set-patch", &[json!({ "wallpaperVisibility": 0.456 })]).unwrap();
        assert_eq!(r["wallpaperVisibility"], 0.46);
        let r = call(&s, "appearance-settings:set-patch", &[json!({ "wallpaperVisibility": null })]).unwrap();
        assert!(r.get("wallpaperVisibility").is_none());
    }

    #[test]
    fn shortcut_override_rules() {
        let (s, rec, d) = rig("sc", "darwin", "arm64");
        let combo = json!({ "code": "KeyB", "meta": true, "ctrl": false, "alt": false, "shift": false });
        let ok = call(&s, "app-shortcuts:set-override", &[json!("open-terminal"), json!({ "code": "KeyJ", "meta": true })]).unwrap();
        assert_eq!(ok["overrides"]["open-terminal"]["code"], "KeyJ");
        // 与已绑定的 toggle-sidebar 默认撞键
        let sidebar = call_def(&s, "toggle-sidebar");
        let e = call(&s, "app-shortcuts:set-override", &[json!("new-maker"), sidebar]).unwrap_err();
        assert!(e.contains("conflicts"), "{e}");
        let e = call(&s, "app-shortcuts:set-override", &[json!("nope"), combo.clone()]).unwrap_err();
        assert!(e.contains("unknown app shortcut id: nope"), "{e}");
        let e = call(&s, "app-shortcuts:set-override", &[json!("close-tab-or-window"), combo.clone()]).unwrap_err();
        assert!(e.contains("not rebindable"), "{e}");
        let e = call(&s, "app-shortcuts:set-override", &[json!("find-in-page"), json!({ "code": "KeyQ", "meta": true })]).unwrap_err();
        assert!(e.contains("reserved by the operating system"), "{e}");
        let e = call(&s, "app-shortcuts:set-override", &[json!("find-in-page"), json!({ "code": "KeyA" })]).unwrap_err();
        assert!(e.contains("not bindable"), "{e}");
        let e = call(&s, "app-shortcuts:set-override", &[json!("find-in-page"), json!({ "code": "ShiftLeft", "meta": true })]).unwrap_err();
        assert!(e.contains("invalid shortcut combo"), "{e}");
        assert_eq!(call(&s, "app-shortcuts:set-override", &[json!("find-in-page"), Value::Null]).unwrap()["overrides"]["find-in-page"], Value::Null);
        assert!(rec.0.lock().unwrap().iter().any(|(c, _)| c == "app-shortcuts:changed"));
        // 重启后还在，顺序保持
        let s2 = AppSettings::with_platform(&d, rec.clone(), "darwin", "arm64");
        let got = s2.sync("app-shortcuts:get", &[]).unwrap();
        assert_eq!(got["platform"], "darwin");
        assert_eq!(got["overrides"].as_object().unwrap().keys().collect::<Vec<_>>(), ["open-terminal", "find-in-page"]);
        call(&s2, "app-shortcuts:clear-override", &[json!("open-terminal")]).unwrap();
        assert!(s2.sync("app-shortcuts:get", &[]).unwrap()["overrides"].get("open-terminal").is_none());
        assert_eq!(call(&s2, "app-shortcuts:reset-all", &[]).unwrap(), json!({ "overrides": {} }));
        assert_eq!(s2.sync("app-shortcuts:get", &[]).unwrap()["overrides"], json!({}));
    }

    fn call_def(_s: &AppSettings, id: &str) -> Value {
        sc::def_of(id).unwrap().default_combos("darwin")[0].to_value()
    }

    #[test]
    fn update_channel_and_auto_update() {
        let (s, _r, d) = rig("up", "darwin", "arm64");
        assert_eq!(call(&s, "update-channel-settings-get", &[]).unwrap(), json!({ "enableBeta": false, "isCustomized": false }));
        let w = call(&s, "update-channel-settings-set", &[json!({ "enableBeta": false })]).unwrap();
        assert_eq!(w["isCustomized"], true, "preserveDefaults 让显式的 false 也算自定义");
        assert!(call(&s, "update-channel-settings-set", &[json!({})]).unwrap_err().contains("enableBeta required"));
        assert!(call(&s, "update-channel-settings-set", &[Value::Null]).unwrap_err().contains("payload required"));
        assert_eq!(call(&s, "update-channel-settings-reset", &[]).unwrap()["isCustomized"], false);
        let (lin, _r2, _d2) = rig("up2", "linux", "ia32");
        assert!(call(&lin, "update-channel-settings-set", &[json!({ "enableBeta": true })]).unwrap_err().contains("does not support"));

        assert!(!s.auto_relaunch_on_idle());
        let w = call(&s, "update-auto-settings-set", &[json!({ "autoRelaunchOnIdle": true })]).unwrap();
        assert_eq!(w, json!({ "autoRelaunchOnIdle": true, "isCustomized": true, "defaultAutoRelaunchOnIdle": false }));
        assert!(s.auto_relaunch_on_idle());
        assert!(AppSettings::with_platform(&d, Arc::new(Rec(Mutex::new(vec![]))), "darwin", "arm64").auto_relaunch_on_idle());
        assert!(call(&s, "update-auto-settings-set", &[json!({ "autoRelaunchOnIdle": 1 })]).is_err());
        assert_eq!(call(&s, "update-auto-settings-reset", &[]).unwrap()["isCustomized"], false);
    }

    #[test]
    fn static_stubs() {
        let (s, _r, _d) = rig("st", "win32", "x64");
        assert_eq!(call(&s, "login-item:get", &[]).unwrap()["unavailableReason"], "development");
        assert!(call(&s, "login-item:set", &[json!(true)]).unwrap_err().contains("UNSUPPORTED_CAPABILITY"));
        assert!(call(&s, "login-item:set", &[json!("x")]).unwrap_err().contains("must be a boolean"));
        assert_eq!(call(&s, "menubar:set-enabled", &[json!(true)]).unwrap(), json!({ "available": false, "enabled": false }));
        let wl = call(&s, "worklouder-codex:get-state", &[]).unwrap();
        assert_eq!(wl.as_object().unwrap().len(), 2);
        assert_eq!(wl["codex-micro"]["connectionStatus"], "disabled");
        assert_eq!(call(&s, "xbox-gamepad:get-state", &[]).unwrap().as_object().unwrap().len(), 4);
        assert_eq!(call(&s, "local-themes:list", &[]).unwrap()["themes"], json!([]));
        assert_eq!(call(&s, "app-locale:set-classical-style", &[json!("prose")]).unwrap(), json!({ "ok": true }));
        for ch in INVOKE_CHANNELS {
            assert!(AppSettings::owns_invoke(ch));
        }
    }
}
