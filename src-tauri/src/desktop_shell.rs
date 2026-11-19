//! 桌面外壳：系统通知、Dock / 任务栏角标、原生对话框、打开链接 / 文件、窗口关闭行为、托盘文案。
//! sidecar/src/desktop-shell.mjs 的 Rust 版。通道校验和状态在这里，真正的原生动作经 `Ports` 交给 Tauri。
//! 约定：每个通道返回 `Option<Result>`，None 表示不归这里。参数是 IPC 解码后的 JSON 数组。

use crate::override_file::{read_json, write_json_atomic};
use crate::shell_policy as policy;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

const CLIENT_NOTIFICATION_NAME: &str = "Cindy";
const CHATGPT_APP_URL: &str = "codex://";
const MAX_SESSION_IDS: usize = 10_000;

fn fail<T>(code: &str, msg: &str) -> Result<T, String> {
    Err(format!("[{code}] {msg}"))
}

/// 原生动作与旁路，生产里由 Tauri 实现，测试里用假的。
pub trait Ports: Send + Sync + 'static {
    /// 通知 / 角标 / 托盘 / 关闭动作：不等结果。
    fn notify(&self, method: &str, params: Value);
    /// 要结果的原生调用（对话框、打开链接）；`wait_user` 表示等用户操作，不超时。
    fn call(&self, method: &str, params: Value, wait_user: bool) -> Result<Value, String>;
    /// 向 renderer 推事件（同时转给设备互联的手机端）。
    fn emit(&self, channel: &str, arg: Value);
    /// 会话是否被用户静音通知。
    fn is_muted(&self, session_id: &str) -> bool;
    /// 有没有会话回合在跑。
    fn any_session_in_turn(&self) -> bool;
    /// 当前数据归属（owner）：(dataOwnerId, generation)。
    fn owner(&self) -> (Option<String>, i64);
}

#[derive(Clone)]
struct WindowBehavior {
    swallow_activation_click: bool,
    windows_close: Option<String>,
    linux_close: Option<String>,
}

impl WindowBehavior {
    fn normalize(raw: &Value) -> Self {
        let pick = |k: &str, allowed: [&str; 2]| raw.get(k).and_then(Value::as_str).filter(|v| allowed.contains(v)).map(str::to_owned);
        WindowBehavior {
            swallow_activation_click: raw.get("swallowActivationClick").and_then(Value::as_bool).unwrap_or(false),
            windows_close: pick("windowsCloseBehavior", ["quit", "tray"]),
            linux_close: pick("linuxCloseBehavior", ["quit", "minimize"]),
        }
    }
    fn to_json(&self) -> Value {
        json!({
            "swallowActivationClick": self.swallow_activation_click,
            "windowsCloseBehavior": self.windows_close,
            "linuxCloseBehavior": self.linux_close,
        })
    }
}

#[derive(Default)]
struct Attention {
    marked: std::collections::HashSet<String>,
    projected_ids: std::collections::HashSet<String>,
    projected_count: Option<i64>,
    last_badge: i64,
}

pub struct DesktopShell {
    ports: Arc<dyn Ports>,
    data_dir: PathBuf,
    windows: bool,
    macos: bool,
    /// None = 跟随系统语言；renderer 通过 app-menu:set-locale 同步后固定。
    locale: Mutex<Option<String>>,
    behavior: Mutex<Option<WindowBehavior>>,
    attention: Mutex<Attention>,
    desktop_notifications: Mutex<bool>,
    /// Windows 首次关闭时「等 renderer 弹自绘框，超时再弹原生框」的计时代号。
    close_prompt_gen: Mutex<u64>,
}

impl DesktopShell {
    pub fn new(ports: Arc<dyn Ports>, data_dir: &Path) -> Arc<Self> {
        Self::with_platform(ports, data_dir, cfg!(windows), cfg!(target_os = "macos"))
    }

    pub fn with_platform(ports: Arc<dyn Ports>, data_dir: &Path, windows: bool, macos: bool) -> Arc<Self> {
        let s = Arc::new(DesktopShell {
            ports,
            data_dir: data_dir.to_path_buf(),
            windows,
            macos,
            locale: Mutex::new(None),
            behavior: Mutex::new(None),
            attention: Mutex::new(Attention { last_badge: -1, ..Default::default() }),
            desktop_notifications: Mutex::new(true),
            close_prompt_gen: Mutex::new(0),
        });
        s.push_tray_labels();
        s
    }

    // ── 文案 / 语言 ──

    fn locale(&self) -> String {
        self.locale.lock().unwrap().clone().unwrap_or_else(|| policy::system_locale().to_string())
    }
    fn t(&self, key: &str) -> String {
        policy::translate(key, &self.locale())
    }
    fn push_tray_labels(&self) {
        self.ports.notify(
            "tray.setLabels",
            json!({
                "show": self.t("settings.windowBehavior.trayMenu.show"),
                "openLogs": self.t("credentialStore.dialog.openLogs"),
                "quit": self.t("settings.windowBehavior.trayMenu.quit"),
                "tooltip": CLIENT_NOTIFICATION_NAME,
            }),
        );
    }
    /// 别的模块（登录、导入）要按界面语言取文案时用。
    pub fn current_locale(&self) -> String {
        self.locale()
    }
    fn set_locale(&self, v: &Value) {
        let Some(l) = v.as_str().filter(|l| policy::is_supported_locale(l)) else { return };
        let changed = {
            let mut g = self.locale.lock().unwrap();
            let changed = g.as_deref() != Some(l);
            *g = Some(l.to_string());
            changed
        };
        if changed {
            self.push_tray_labels();
            // sidecar 里还没迁走的模块（登录、模型服务、桌面导入）要按界面语言取值。
            self.ports.notify("sidecar.locale", json!({ "locale": l }));
        }
    }

    // ── 窗口行为设置 ──

    fn behavior_file(&self) -> PathBuf {
        self.data_dir.join("window-behavior-settings.json")
    }
    fn behavior(&self) -> WindowBehavior {
        let mut g = self.behavior.lock().unwrap();
        if g.is_none() {
            let raw = read_json(&self.behavior_file(), None).ok().flatten().unwrap_or(Value::Null);
            *g = Some(WindowBehavior::normalize(&raw));
        }
        g.clone().unwrap()
    }
    fn write_behavior(&self, f: impl FnOnce(&mut WindowBehavior)) -> Result<(), String> {
        let mut next = self.behavior();
        f(&mut next);
        write_json_atomic(&self.behavior_file(), &next.to_json(), None)?;
        *self.behavior.lock().unwrap() = Some(next);
        Ok(())
    }

    // ── 角标 ──

    fn apply_badge(&self, a: &mut Attention) {
        let count = a.projected_count.unwrap_or(a.marked.len() as i64);
        if count == a.last_badge {
            return;
        }
        a.last_badge = count;
        self.ports.notify("badge.set", json!({ "count": count }));
    }
    fn mark_attention(&self, sid: &str) {
        let mut a = self.attention.lock().unwrap();
        if a.marked.insert(sid.to_string()) && (a.projected_count.is_none() || !a.projected_ids.contains(sid)) {
            self.apply_badge(&mut a);
        }
    }
    fn clear_attention(&self, sid: &str, intent: &str) {
        {
            let mut a = self.attention.lock().unwrap();
            if a.marked.remove(sid) && (a.projected_count.is_none() || !a.projected_ids.contains(sid)) {
                self.apply_badge(&mut a);
            }
        }
        self.ports.emit("notification:session-attention-cleared", json!({ "sessionId": sid, "intent": intent }));
    }
    #[cfg(test)]
    fn attention_count(&self) -> i64 {
        let a = self.attention.lock().unwrap();
        a.projected_count.unwrap_or(a.marked.len() as i64)
    }

    fn valid_sid(v: &Value) -> Option<&str> {
        v.as_str().filter(|s| !s.is_empty() && s.len() <= 256)
    }

    // ── 通道 ──

    pub fn owns(channel: &str) -> bool {
        CHANNELS.contains(&channel)
    }

    pub fn invoke(&self, channel: &str, args: &[Value]) -> Option<Result<Value, String>> {
        if !Self::owns(channel) {
            return None;
        }
        let a0 = args.first().unwrap_or(&Value::Null);
        let r = match channel {
            "app-menu:set-locale" => {
                self.set_locale(a0);
                Ok(json!({ "ok": true }))
            }
            "notification:set-app-attention-count" => self.set_attention_count(a0),
            "notification:mark-session-attention" => {
                if let Some(s) = Self::valid_sid(a0) {
                    self.mark_attention(s);
                }
                Ok(Value::Null)
            }
            "notification:clear-session-attention" => {
                if let Some(s) = Self::valid_sid(a0) {
                    let intent = if args.get(1).and_then(Value::as_str) == Some("explicit") { "explicit" } else { "passive" };
                    self.clear_attention(s, intent);
                }
                Ok(Value::Null)
            }
            "notification:set-desktop-enabled" => match a0.as_bool() {
                Some(b) => {
                    *self.desktop_notifications.lock().unwrap() = b;
                    Ok(json!({ "ok": true }))
                }
                None => fail("INVALID_PARAMS", "notification desktop enabled must be a boolean"),
            },
            "notification:show-session-event" => self.show_session_event(a0),
            "shell:open-external" => Ok(self.open_external(a0)),
            "shell:open-chatgpt-app" => Ok(bool_result(self.ports.call("open.url", json!({ "url": CHATGPT_APP_URL }), false))),
            "shell:open-file-in-browser" => Ok(self.open_file_in_browser(a0)),
            "shell:show-item-in-folder" => Ok(self.show_item_in_folder(a0)),
            "app:open-logs-dir" => Ok(self.open_logs_dir()),
            "show-open-directory-dialog" => self.pick_folder(None).map(|picked| match picked {
                Value::String(_) => json!({ "canceled": false, "path": picked }),
                _ => json!({ "canceled": true }),
            }),
            "dialog:show-open-directory" => self.show_open_directory(a0),
            "dialog:show-open-file" => self.show_open_file(a0),
            "dialog:show-open-resource" => self.show_open_resource(a0),
            "window-behavior:set-swallow-activation-click" => match a0.as_bool() {
                Some(b) => self.write_behavior(|w| w.swallow_activation_click = b).map(|_| json!({ "ok": true })),
                None => fail("INVALID_PARAMS", "enabled must be a boolean"),
            },
            "window-behavior:get-windows-close-behavior" => Ok(json!(self.behavior().windows_close)),
            "window-behavior:set-windows-close-behavior" => match a0.as_str() {
                Some(v @ ("quit" | "tray")) => {
                    self.cancel_close_prompt();
                    self.write_behavior(|w| w.windows_close = Some(v.to_string())).map(|_| json!(v))
                }
                _ => fail("INVALID_PARAMS", "Windows close behavior required (quit|tray)"),
            },
            "window-behavior:get-linux-close-behavior" => Ok(json!(self.behavior().linux_close)),
            "window-behavior:set-linux-close-behavior" => match a0.as_str() {
                Some(v @ ("quit" | "minimize")) => self.write_behavior(|w| w.linux_close = Some(v.to_string())).map(|_| json!(v)),
                _ => fail("INVALID_PARAMS", "Linux close behavior required (quit|minimize)"),
            },
            _ => return None,
        };
        Some(r)
    }

    /// renderer 的 send 通道（无返回值）。返回是否由这里处理。
    pub fn send(&self, channel: &str) -> bool {
        match channel {
            "window-behavior:windows-close-behavior-shown" => {
                self.cancel_close_prompt();
                true
            }
            "window-behavior:linux-close-behavior-shown" => true,
            _ => false,
        }
    }

    fn set_attention_count(&self, snapshot: &Value) -> Result<Value, String> {
        let Some(obj) = snapshot.as_object() else {
            return fail("INVALID_PARAMS", "App attention snapshot requires an owner stamp");
        };
        let owner_ok = matches!(obj.get("dataOwnerId"), Some(Value::String(_)) | Some(Value::Null)) && obj.get("ownerGeneration").is_some_and(Value::is_number);
        if !owner_ok {
            return fail("INVALID_PARAMS", "App attention snapshot requires an owner stamp");
        }
        let count = obj.get("count").and_then(Value::as_i64).filter(|c| *c >= 0 && (*c as usize) <= MAX_SESSION_IDS);
        let Some(count) = count else {
            return fail("INVALID_PARAMS", "App attention count exceeds the supported inventory range");
        };
        let ids = obj.get("sessionIds").and_then(Value::as_array).filter(|a| a.len() <= MAX_SESSION_IDS && a.iter().all(|v| Self::valid_sid(v).is_some()));
        let Some(ids) = ids else {
            return fail("INVALID_PARAMS", "App attention snapshot requires session IDs");
        };
        let (owner, generation) = self.ports.owner();
        let stamp_owner = obj.get("dataOwnerId").and_then(Value::as_str).map(str::to_owned);
        if stamp_owner != owner || obj.get("ownerGeneration").and_then(Value::as_i64) != Some(generation) {
            return Ok(Value::Null);
        }
        let mut a = self.attention.lock().unwrap();
        a.projected_count = Some(count);
        for id in ids {
            a.projected_ids.insert(id.as_str().unwrap().to_string());
        }
        if a.projected_ids.len() > MAX_SESSION_IDS * 2 {
            a.projected_ids.clear();
        }
        self.apply_badge(&mut a);
        Ok(Value::Null)
    }

    fn show_session_event(&self, p: &Value) -> Result<Value, String> {
        let bad = || fail("INVALID_PARAMS", "invalid session event notification payload");
        let Some(o) = p.as_object() else { return bad() };
        let Some(sid) = o.get("sessionId").and_then(Value::as_str).filter(|s| !s.is_empty() && s.len() <= 256) else { return bad() };
        let Some(title) = o.get("title").and_then(Value::as_str).filter(|t| t.len() <= 1024) else { return bad() };
        let Some(kind) = o.get("kind").and_then(Value::as_str).filter(|k| matches!(*k, "done" | "error" | "needs-reply")) else { return bad() };
        if o.get("channels").is_some_and(|c| !c.is_object()) || o.get("markAttention").is_some_and(|m| !m.is_boolean()) {
            return bad();
        }
        if let Some(h) = o.get("inputHandoffRequestId") {
            let ok = kind == "needs-reply" && h.as_str().is_some_and(|s| !s.is_empty() && s.len() <= 256);
            if !ok {
                return bad();
            }
        }
        let mark = o.get("markAttention").and_then(Value::as_bool).unwrap_or(true);
        if mark {
            self.mark_attention(sid);
        }
        if mark && self.ports.is_muted(sid) {
            return Ok(Value::Null);
        }
        // 窗口聚焦时不弹由 renderer 判断（document.hasFocus），与 desktop 一致。
        let desktop_channel = o.get("channels").and_then(|c| c.get("desktop")).and_then(Value::as_bool).unwrap_or(true);
        if !desktop_channel || !*self.desktop_notifications.lock().unwrap() {
            return Ok(Value::Null);
        }
        let trimmed = title.trim();
        let safe_title = if !trimmed.is_empty() {
            trimmed.to_string()
        } else {
            let short: String = sid.chars().take(8).collect();
            if short.is_empty() { self.t("settings.notifications.sessionEvent.untitled") } else { short }
        };
        let body_key = match kind {
            "done" => "settings.notifications.sessionEvent.done",
            "error" => "settings.notifications.sessionEvent.error",
            _ => "settings.notifications.sessionEvent.needsReply",
        };
        self.ports.notify(
            "notify",
            json!({ "title": format!("{CLIENT_NOTIFICATION_NAME} · {safe_title}"), "body": self.t(body_key), "sessionId": sid }),
        );
        Ok(Value::Null)
    }

    pub fn desktop_notifications_enabled(&self) -> bool {
        *self.desktop_notifications.lock().unwrap()
    }

    // ── 打开链接 / 文件 ──

    fn open_external(&self, url: &Value) -> Value {
        if !policy::is_allowed_external_url(url, self.macos) {
            return json!({ "success": false });
        }
        bool_result(self.ports.call("open.url", json!({ "url": url }), false))
    }

    fn open_file_in_browser(&self, v: &Value) -> Value {
        let err = |msg: &str, code: &str| json!({ "success": false, "error": msg, "errorCode": code });
        let Some(target) = v.as_str().and_then(|s| policy::resolve_browser_target(s, self.windows)) else {
            return err("路径必须是绝对路径或本地 file:// URL", "BROWSER_FILE_INVALID_TARGET");
        };
        if !policy::is_path_allowed(&target.file_path, self.windows) {
            return err("不允许访问该路径", "BROWSER_FILE_PATH_NOT_ALLOWED");
        }
        if !policy::is_browser_openable(&target.file_path) {
            return err("该文件类型不支持浏览器查看", "BROWSER_FILE_UNSUPPORTED_TYPE");
        }
        if !Path::new(&target.file_path).exists() {
            return err("文件不存在", "BROWSER_FILE_NOT_FOUND");
        }
        match self.ports.call("open.url", json!({ "url": target.file_url }), false) {
            Ok(_) => json!({ "success": true }),
            Err(first) => {
                if target.has_url_state {
                    return err("无法在浏览器中打开该文件", "BROWSER_FILE_OPEN_FAILED");
                }
                match self.ports.call("open.path", json!({ "path": target.file_path }), false) {
                    Ok(_) => json!({ "success": true }),
                    Err(second) => err(if second.is_empty() { &first } else { &second }, "BROWSER_FILE_OPEN_FAILED"),
                }
            }
        }
    }

    fn show_item_in_folder(&self, params: &Value) -> Value {
        // xdt-image:// 等缓存还没移植，只支持绝对路径。
        let Some(p) = params.get("filePath").and_then(Value::as_str).filter(|p| Path::new(p).is_absolute()) else {
            return json!({ "success": false, "error": "url 或 filePath 必须二选一" });
        };
        if !policy::is_path_allowed(p, self.windows) {
            return json!({ "success": false, "error": "不允许访问该路径" });
        }
        if !Path::new(p).exists() {
            return json!({ "success": false, "error": "文件不存在" });
        }
        match self.ports.call("reveal", json!({ "path": p }), false) {
            Ok(_) => json!({ "success": true }),
            Err(e) => json!({ "success": false, "error": e }),
        }
    }

    pub fn open_logs_dir(&self) -> Value {
        let dir = self.data_dir.join("logs");
        let _ = std::fs::create_dir_all(&dir);
        match self.ports.call("open.path", json!({ "path": dir }), false) {
            Ok(_) => json!({ "success": true }),
            Err(e) => {
                eprintln!("[desktop-shell] could not open logs directory: {e}");
                json!({ "success": false })
            }
        }
    }

    // ── 选择器 ──

    fn optional_absolute(v: Option<&Value>, name: &str) -> Result<Option<String>, String> {
        match v {
            None | Some(Value::Null) => Ok(None),
            Some(Value::String(s)) if s.is_empty() => Ok(None),
            Some(Value::String(s)) if s.len() <= 4096 && Path::new(s).is_absolute() => Ok(Some(s.clone())),
            _ => fail("INVALID_PARAMS", &format!("{name} must be an absolute path")),
        }
    }
    fn pick_folder(&self, default: Option<String>) -> Result<Value, String> {
        self.ports.call("dialog.pickFolder", json!({ "defaultPath": default }), true)
    }
    fn pick_file(&self, default: Option<String>, filters: Option<Value>) -> Result<Value, String> {
        self.ports.call("dialog.pickFile", json!({ "defaultPath": default, "filters": filters }), true)
    }

    fn show_open_directory(&self, params: &Value) -> Result<Value, String> {
        let body = if params.is_object() { params } else { &Value::Null };
        if let Some(g) = body.get("writableGrantScope") {
            if !g.as_str().is_some_and(|s| !s.is_empty() && s.len() <= 128) {
                return fail("INVALID_PARAMS", "invalid writable directory grant scope");
            }
        }
        // extraDirs 可写授权还没移植（Pi 能力里 extraDirs 为 false），这里只做选择。
        let picked = self.pick_folder(Self::optional_absolute(body.get("defaultPath"), "defaultPath")?)?;
        Ok(json!({ "success": true, "path": nullable(picked) }))
    }

    fn show_open_file(&self, params: &Value) -> Result<Value, String> {
        let body = if params.is_object() { params } else { &Value::Null };
        let mut filters = None;
        if let Some(f) = body.get("filters").filter(|f| !f.is_null()) {
            let Some(arr) = f.as_array().filter(|a| a.len() <= 20) else {
                return fail("INVALID_PARAMS", "filters must be an array");
            };
            let ext_ok = |e: &Value| e.as_str().is_some_and(|s| !s.is_empty() && s.len() <= 32 && s.chars().all(|c| c.is_ascii_alphanumeric() || "*_-".contains(c)));
            let mut out = Vec::new();
            for item in arr {
                let name = item.get("name").and_then(Value::as_str);
                let exts = item.get("extensions").and_then(Value::as_array).filter(|e| e.iter().all(ext_ok));
                match (name, exts) {
                    (Some(n), Some(e)) => out.push(json!({ "name": n.chars().take(128).collect::<String>(), "extensions": e })),
                    _ => return fail("INVALID_PARAMS", "invalid file filter"),
                }
            }
            filters = Some(Value::Array(out));
        }
        let picked = self.pick_file(Self::optional_absolute(body.get("defaultPath"), "defaultPath")?, filters)?;
        Ok(json!({ "success": true, "path": nullable(picked) }))
    }

    fn show_open_resource(&self, params: &Value) -> Result<Value, String> {
        if !params.is_object() && !params.is_null() {
            return fail("INVALID_PARAMS", "params must be an object");
        }
        // desktop 在 macOS 上可同时选文件或目录；Tauri 选择器一次只能选一种，这里统一选文件。
        let picked = self.pick_file(Self::optional_absolute(params.get("defaultPath"), "defaultPath")?, None)?;
        let Some(p) = picked.as_str() else {
            return Ok(json!({ "success": true, "path": null, "kind": null }));
        };
        let kind = if Path::new(p).is_dir() { "directory" } else { "file" };
        Ok(json!({ "success": true, "path": p, "kind": kind }))
    }

    // ── 关闭行为 ──

    fn cancel_close_prompt(&self) {
        *self.close_prompt_gen.lock().unwrap() += 1;
    }

    fn native_close_prompt(&self) -> &'static str {
        let tray = self.t("settings.windowBehavior.closeBehavior.tray");
        let quit = self.t("settings.windowBehavior.closeBehavior.quit");
        let idx = self
            .ports
            .call(
                "dialog.message",
                json!({
                    "kind": "info",
                    "title": self.t("settings.windowBehavior.closePrompt.title"),
                    "message": self.t("settings.windowBehavior.closePrompt.message"),
                    "detail": self.t("settings.windowBehavior.closePrompt.detail"),
                    "buttons": [tray, quit],
                }),
                true,
            )
            .ok()
            .and_then(|v| v.as_i64())
            .unwrap_or(0);
        if idx == 1 { "quit" } else { "tray" }
    }

    /// Windows 首次关闭：先让 renderer 弹自绘框；`delay` 内它没确认显示，就弹原生框。
    fn request_windows_close_prompt(self: &Arc<Self>, delay: std::time::Duration) {
        self.ports.emit("window-behavior:windows-close-behavior-requested", Value::Null);
        let gen = {
            let mut g = self.close_prompt_gen.lock().unwrap();
            *g += 1;
            *g
        };
        let me = self.clone();
        std::thread::spawn(move || {
            std::thread::sleep(delay);
            if *me.close_prompt_gen.lock().unwrap() != gen || me.behavior().windows_close.is_some() {
                return;
            }
            let choice = me.native_close_prompt();
            // 原生框打开期间 renderer 可能已经存了选择，保留先到的那个。
            if me.behavior().windows_close.is_some() {
                return;
            }
            let _ = me.write_behavior(|w| w.windows_close = Some(choice.to_string()));
            me.ports.notify("window.closeAction", json!({ "action": choice }));
        });
    }

    /// 主窗口收到关闭请求（自绘 X、Alt+F4、任务栏关闭）。返回 `quit` / `tray` / `none`。
    pub fn main_window_close_requested(self: &Arc<Self>) -> &'static str {
        self.main_window_close_requested_with(std::time::Duration::from_secs(2))
    }
    fn main_window_close_requested_with(self: &Arc<Self>, delay: std::time::Duration) -> &'static str {
        if !self.windows {
            return "quit";
        }
        match self.behavior().windows_close.as_deref() {
            Some("tray") => "tray",
            Some(_) => "quit",
            None => {
                self.request_windows_close_prompt(delay);
                "none"
            }
        }
    }

    /// 托盘「退出」：有任务在跑就先确认。
    pub fn confirm_quit_if_busy(&self) -> bool {
        if !self.ports.any_session_in_turn() {
            return true;
        }
        let idx = self
            .ports
            .call(
                "dialog.message",
                json!({
                    "kind": "warning",
                    "title": self.t("titleBar.closeConfirm.title"),
                    "message": self.t("titleBar.closeConfirm.title"),
                    "detail": self.t("titleBar.closeConfirm.description"),
                    // 第一个按钮是默认 / 取消，与 desktop defaultId: 0 一致。
                    "buttons": [self.t("titleBar.closeConfirm.cancel"), self.t("titleBar.closeConfirm.confirm")],
                }),
                true,
            )
            .ok()
            .and_then(|v| v.as_i64())
            .unwrap_or(0);
        idx == 1
    }

    pub fn tray_open_logs(&self) {
        let r = self.open_logs_dir();
        if r["success"] != json!(true) {
            let _ = self.ports.call(
                "dialog.message",
                json!({ "kind": "error", "title": self.t("logSupport.title"), "message": self.t("settings.about.openLogsDirError"), "buttons": [] }),
                true,
            );
        }
    }

    pub fn tray_unavailable(&self) {
        let _ = self.ports.call(
            "dialog.message",
            json!({
                "kind": "error",
                "title": self.t("settings.windowBehavior.trayError.title"),
                "message": self.t("settings.windowBehavior.trayError.message"),
                "buttons": [],
            }),
            true,
        );
    }

    /// 点击任务通知：让 renderer 跳到对应任务（desktop openMainWindowSession）。
    pub fn notification_clicked(&self, session_id: &str) {
        if !session_id.is_empty() && session_id.len() <= 256 {
            self.ports.emit("deep-link:navigate", json!({ "type": "session", "id": session_id }));
        }
    }
}

/// 归这里的 invoke 通道。send 通道（*-shown）走 `send`。
const CHANNELS: [&str; 20] = [
    "app-menu:set-locale",
    "notification:set-app-attention-count",
    "notification:mark-session-attention",
    "notification:clear-session-attention",
    "notification:set-desktop-enabled",
    "notification:show-session-event",
    "shell:open-external",
    "shell:open-chatgpt-app",
    "shell:open-file-in-browser",
    "shell:show-item-in-folder",
    "app:open-logs-dir",
    "show-open-directory-dialog",
    "dialog:show-open-directory",
    "dialog:show-open-file",
    "dialog:show-open-resource",
    "window-behavior:set-swallow-activation-click",
    "window-behavior:get-windows-close-behavior",
    "window-behavior:set-windows-close-behavior",
    "window-behavior:get-linux-close-behavior",
    "window-behavior:set-linux-close-behavior",
];

fn bool_result(r: Result<Value, String>) -> Value {
    json!({ "success": r.is_ok() })
}
fn nullable(v: Value) -> Value {
    if v.is_string() { v } else { Value::Null }
}

#[cfg(test)]
mod tests;
