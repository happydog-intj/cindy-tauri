// sidecar → Rust 的反向调用（sidecar/src/native.mjs）和桌面外壳的原生部分：
// 系统通知、Dock 角标、原生对话框、打开链接 / 文件、Windows 托盘、主窗口关闭行为。
// 通道校验和文案在 desktop_shell.rs，这里只执行原生动作。
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::Duration;
use tauri::{AppHandle, Manager, Runtime, Window};
use tauri_plugin_dialog::{DialogExt, MessageDialogButtons, MessageDialogKind, MessageDialogResult};
use tauri_plugin_notification::NotificationExt;
use tauri_plugin_opener::OpenerExt;

// 真正退出时置位，主窗口 CloseRequested 不再拦截。
static QUITTING: AtomicBool = AtomicBool::new(false);

#[derive(Clone)]
struct TrayLabels {
    show: String,
    open_logs: String,
    quit: String,
    tooltip: String,
}

static TRAY_LABELS: Mutex<Option<TrayLabels>> = Mutex::new(None);
/// 桌面外壳（通知 / 角标 / 关闭行为），setup 时建好。
pub static AUTH: OnceLock<Arc<crate::auth::manager::Manager>> = OnceLock::new();
pub static DEVICE_LINK: OnceLock<Arc<crate::device_link::host::DeviceLink>> = OnceLock::new();

/// renderer 推送的设备互联旁路：转发给 Rust 设备互联宿主。
pub fn push_tap(channel: &str, payload: &Value) {
    if let Some(d) = DEVICE_LINK.get() {
        d.on_push(channel, payload.clone());
    }
}
pub static MODEL_ACCESS: OnceLock<Arc<crate::model_access::manager::Manager>> = OnceLock::new();
pub static MODELS: OnceLock<Arc<crate::model_config::manager::Manager>> = OnceLock::new();

/// renderer 推送 + 设备互联旁路（手机端也要这份推送）。
pub fn emit_all(app: &AppHandle, channel: &str, arg: Value) {
    use tauri::Emitter;
    let payload = serde_json::to_string(&json!({ "v": [arg.clone()] })).unwrap_or_default();
    let _ = app.emit_to(tauri::EventTarget::webview("main"), "cindy:ipc", json!({ "channel": channel, "payload": payload }));
    push_tap(channel, &arg);
}

pub static MODEL_HOST: OnceLock<Arc<crate::model_config::host::ModelHost>> = OnceLock::new();

/// 原 sidecar 的 `host.*` 模型相关回调，现在由 Rust 直接回答。None = 不是这几个方法。
pub fn model_host_call(method: &str, p: &Value) -> Option<Result<Value, String>> {
    match MODEL_HOST.get() {
        Some(h) => h.call(method, p),
        None if method.starts_with("host.") && ["resolve-launch", "pi-models", "gateway-rejected", "secrets-ready", "has-provider", "import-provider"].contains(&&method[5..]) => {
            Some(Err("[PRECONDITION_FAILED] models are not ready".into()))
        }
        None => None,
    }
}
pub static LOCAL_DB: OnceLock<Arc<crate::local_db::LocalDb>> = OnceLock::new();
pub static SHELL: OnceLock<Arc<crate::desktop_shell::DesktopShell>> = OnceLock::new();

/// 桌面版导入（原 sidecar desktop-import.mjs）。
pub static IMPORT: OnceLock<Arc<crate::import_run::DesktopImport>> = OnceLock::new();

pub fn desktop_import() -> Option<Arc<crate::import_run::DesktopImport>> {
    IMPORT.get().cloned()
}

/// 导入的生产端口：模型服务 / secret store / 本地库文件名向 sidecar 要，其余走本模块。
pub struct ImportAppPorts(pub AppHandle);

impl crate::import_run::ImportPorts for ImportAppPorts {
    fn sidecar(&self, method: &str, params: Value) -> Result<Value, String> {
        if let Some(r) = model_host_call(method, &params) {
            return r;
        }
        if method == "host.local-db-file" {
            return LOCAL_DB.get().map(|d| json!(d.file.to_string_lossy())).ok_or_else(|| "[PRECONDITION_FAILED] local database is not ready".to_string());
        }
        Err(format!("[NOT_IMPLEMENTED] {method}"))
    }
    fn safe_storage_key(&self, request: &Value) -> Result<Value, String> {
        crate::desktop_import::safe_storage_key(request)
    }
    fn dialog(&self, params: Value) -> Result<i64, String> {
        run(&self.0, "dialog.message", &params).map(|v| v.as_i64().unwrap_or(0))
    }
    fn emit(&self, channel: &str, arg: Value) {
        crate::desktop_shell::Ports::emit(&AppPorts(self.0.clone()), channel, arg);
    }
    fn locale(&self) -> String {
        shell().map(|s| s.current_locale()).unwrap_or_else(|| "en".into())
    }
}

pub fn shell() -> Option<Arc<crate::desktop_shell::DesktopShell>> {
    SHELL.get().cloned()
}

/// 外壳的生产端口：原生动作直接调本模块，owner / 静音标记向 sidecar 查询（auth 与落库迁移后改本地读）。
pub struct AppPorts(pub AppHandle);

impl crate::desktop_shell::Ports for AppPorts {
    fn notify(&self, method: &str, params: Value) {
        let app = self.0.clone();
        let method = method.to_string();
        thread::spawn(move || {
            if method == "sidecar.locale" {
                if let Some(i) = desktop_import() {
                    i.locale_changed();
                }
                return;
            }
            if let Err(e) = run(&app, &method, &params) {
                eprintln!("[native] {method} failed: {e}");
            }
        });
    }
    fn call(&self, method: &str, params: Value, _wait_user: bool) -> Result<Value, String> {
        run(&self.0, method, &params)
    }
    fn emit(&self, channel: &str, arg: Value) {
        use tauri::Emitter;
        let payload = serde_json::to_string(&json!({ "v": [arg] })).unwrap_or_default();
        let _ = self.0.emit_to(tauri::EventTarget::webview("main"), "cindy:ipc", json!({ "channel": channel, "payload": payload }));
        // 手机端（设备互联）也要这份推送。
        push_tap(channel, &arg);
    }
    fn is_muted(&self, session_id: &str) -> bool {
        LOCAL_DB.get().map(|d| d.session_muted(session_id)).unwrap_or(false)
    }
    fn any_session_in_turn(&self) -> bool {
        // 探测失败不能变成无确认退出。
        match self.0.try_state::<crate::MakerState>() {
            Some(m) => m.0.as_ref().map(|r| r.any_in_turn()).unwrap_or(false),
            None => true,
        }
    }
    fn owner(&self) -> (Option<String>, i64) {
        // 应用会话在 Rust（owner.rs），不再绕 sidecar。
        let v = self.0.try_state::<Arc<crate::owner::Owner>>().map(|o| o.stamp()).unwrap_or(Value::Null);
        (v.get("dataOwnerId").and_then(Value::as_str).map(str::to_owned), v.get("ownerGeneration").and_then(Value::as_i64).unwrap_or(0))
    }
}
const TRAY_ID: &str = "main";

// 主窗口里还有浏览器子 webview，所以按 Window 取（get_webview_window 在多 webview 时返回 None）。
pub fn main_window<R: Runtime>(app: &AppHandle<R>) -> Option<Window<R>> {
    app.get_window("main")
}

pub fn quit<R: Runtime>(app: &AppHandle<R>) {
    QUITTING.store(true, Ordering::SeqCst);
    app.exit(0);
}

pub fn mark_quitting() {
    QUITTING.store(true, Ordering::SeqCst);
}

pub fn is_quitting() -> bool {
    QUITTING.load(Ordering::SeqCst)
}

// desktop focusMainWindow：显示、取消最小化、聚焦。
pub fn focus_main_window<R: Runtime>(app: &AppHandle<R>) {
    if let Some(w) = main_window(app) {
        let _ = w.show();
        if w.is_minimized().unwrap_or(false) {
            let _ = w.unminimize();
        }
        let _ = w.set_focus();
    }
}

// 隐藏主窗口；全屏时先退出全屏（macOS 全屏窗口直接 hide 会留下黑色空间）。
pub fn hide_main_window<R: Runtime>(window: &Window<R>) {
    if window.is_fullscreen().unwrap_or(false) {
        let _ = window.set_fullscreen(false);
        let w = window.clone();
        thread::spawn(move || {
            // 退出全屏动画约 0.5–0.8 秒，等它结束再隐藏。
            for _ in 0..40 {
                thread::sleep(Duration::from_millis(50));
                if !w.is_fullscreen().unwrap_or(false) {
                    break;
                }
            }
            thread::sleep(Duration::from_millis(300));
            let _ = w.hide();
        });
    } else {
        let _ = window.hide();
    }
}

// ── 主窗口关闭（红灯 / Alt+F4 / 任务栏关闭 / Windows 自绘 X 里的 window.close()） ──

pub fn on_main_close_requested<R: Runtime>(window: &Window<R>, api: &tauri::CloseRequestApi) {
    if is_quitting() {
        return;
    }
    // macOS：保留窗口和 renderer，只隐藏，Dock 点击时恢复。
    if cfg!(target_os = "macos") {
        api.prevent_close();
        hide_main_window(window);
        return;
    }
    if cfg!(windows) {
        api.prevent_close();
        let app = window.app_handle().clone();
        thread::spawn(move || {
            let action = shell().map(|s| s.main_window_close_requested()).unwrap_or("quit");
            apply_close_action(&app, action);
        });
    }
    // Linux：先保持默认关闭（desktop 的 Linux 托盘未移植）。
}

fn apply_close_action<R: Runtime>(app: &AppHandle<R>, action: &str) {
    match action {
        "quit" => quit(app),
        "tray" => {
            let ok = ensure_tray(app);
            if ok {
                if let Some(w) = main_window(app) {
                    hide_main_window(&w);
                }
            } else {
                if let Some(s) = shell() {
                    s.tray_unavailable();
                }
            }
        }
        _ => {}
    }
}

// ── Windows 托盘（desktop ensureWindowsTray：左键聚焦，右键菜单 显示 / 打开日志 / 退出） ──

fn current_labels() -> TrayLabels {
    TRAY_LABELS.lock().ok().and_then(|g| g.clone()).unwrap_or(TrayLabels {
        show: "Show Cindy".into(),
        open_logs: "Open Logs".into(),
        quit: "Quit Cindy".into(),
        tooltip: "Cindy".into(),
    })
}

fn build_tray_menu<R: Runtime>(app: &AppHandle<R>, labels: &TrayLabels) -> tauri::Result<tauri::menu::Menu<R>> {
    use tauri::menu::{Menu, MenuItem, PredefinedMenuItem};
    let show = MenuItem::with_id(app, "tray-show", &labels.show, true, None::<&str>)?;
    let logs = MenuItem::with_id(app, "tray-open-logs", &labels.open_logs, true, None::<&str>)?;
    let sep = PredefinedMenuItem::separator(app)?;
    let quit = MenuItem::with_id(app, "tray-quit", &labels.quit, true, None::<&str>)?;
    Menu::with_items(app, &[&show, &logs, &sep, &quit])
}

pub fn ensure_tray<R: Runtime>(app: &AppHandle<R>) -> bool {
    if !cfg!(windows) {
        return false;
    }
    if app.tray_by_id(TRAY_ID).is_some() {
        return true;
    }
    let labels = current_labels();
    let built = (|| -> tauri::Result<()> {
        use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
        let menu = build_tray_menu(app, &labels)?;
        let mut builder = TrayIconBuilder::with_id(TRAY_ID)
            .tooltip(&labels.tooltip)
            .menu(&menu)
            .show_menu_on_left_click(false)
            .on_menu_event(|app, event| on_tray_menu(app, event.id().as_ref()))
            .on_tray_icon_event(|tray, event| {
                if let TrayIconEvent::Click { button: MouseButton::Left, button_state: MouseButtonState::Up, .. } = event {
                    focus_main_window(tray.app_handle());
                }
            });
        if let Some(icon) = app.default_window_icon() {
            builder = builder.icon(icon.clone());
        }
        builder.build(app)?;
        Ok(())
    })();
    if let Err(e) = &built {
        eprintln!("[tray] failed to create tray icon: {e}");
    }
    built.is_ok()
}

fn on_tray_menu<R: Runtime>(app: &AppHandle<R>, id: &str) {
    match id {
        "tray-show" => focus_main_window(app),
        "tray-open-logs" => {
            thread::spawn(|| {
                if let Some(s) = shell() {
                    s.tray_open_logs();
                }
            });
        }
        "tray-quit" => {
            let app = app.clone();
            thread::spawn(move || {
                // 有任务在跑时先弹确认框。
                let ok = shell().map(|s| s.confirm_quit_if_busy()).unwrap_or(true);
                if ok {
                    quit(&app);
                }
            });
        }
        _ => {}
    }
}

fn set_tray_labels<R: Runtime>(app: &AppHandle<R>, params: &Value) {
    let get = |k: &str| params.get(k).and_then(Value::as_str).map(str::to_owned);
    let (Some(show), Some(open_logs), Some(quit), Some(tooltip)) = (get("show"), get("openLogs"), get("quit"), get("tooltip")) else {
        return;
    };
    let labels = TrayLabels { show, open_logs, quit, tooltip };
    if let Ok(mut g) = TRAY_LABELS.lock() {
        *g = Some(labels.clone());
    }
    if let Some(tray) = app.tray_by_id(TRAY_ID) {
        if let Ok(menu) = build_tray_menu(app, &labels) {
            let _ = tray.set_menu(Some(menu));
        }
        let _ = tray.set_tooltip(Some(&labels.tooltip));
    }
}

// ── 系统通知：点击回到主窗口并跳到对应任务 ──

pub fn register_notification_click<R: Runtime>(app: &AppHandle<R>) {
    let handle = app.clone();
    let _ = app.notification().on_action(move |performed| {
        if performed.action_id() != "tap" {
            return;
        }
        let Some(session_id) = performed
            .notification()
            .and_then(|n| n.extra().get("sessionId"))
            .and_then(Value::as_str)
            .map(str::to_owned)
        else {
            return;
        };
        focus_main_window(&handle);
        thread::spawn(move || {
            if let Some(s) = shell() {
                s.notification_clicked(&session_id);
            }
        });
    });
}

fn show_notification<R: Runtime>(app: &AppHandle<R>, params: &Value) -> Result<Value, String> {
    let title = params.get("title").and_then(Value::as_str).unwrap_or("Cindy");
    let body = params.get("body").and_then(Value::as_str).unwrap_or("");
    let mut builder = app.notification().builder().title(title).body(body);
    if let Some(sid) = params.get("sessionId").and_then(Value::as_str) {
        builder = builder.extra("sessionId", sid);
    }
    builder.show().map_err(|e| e.to_string())?;
    Ok(Value::Null)
}

// ── Dock 角标（Windows 任务栏 overlay 还没做） ──

fn set_badge<R: Runtime>(app: &AppHandle<R>, params: &Value) {
    let count = params.get("count").and_then(Value::as_i64).unwrap_or(0);
    #[cfg(target_os = "macos")]
    if let Some(w) = main_window(app) {
        let _ = w.set_badge_count(if count > 0 { Some(count) } else { None });
    }
    #[cfg(not(target_os = "macos"))]
    let _ = (app, count);
}

// ── 对话框（blocking_* 只能在非主线程用，dispatch 已经在独立线程里） ──

fn dialog_message<R: Runtime>(app: &AppHandle<R>, params: &Value) -> Result<Value, String> {
    let s = |k: &str| params.get(k).and_then(Value::as_str).unwrap_or("").to_string();
    let buttons: Vec<String> = params
        .get("buttons")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(|b| b.as_str().map(str::to_owned)).collect())
        .unwrap_or_default();
    let kind = match s("kind").as_str() {
        "warning" => MessageDialogKind::Warning,
        "error" => MessageDialogKind::Error,
        _ => MessageDialogKind::Info,
    };
    let detail = s("detail");
    let text = if detail.is_empty() { s("message") } else { format!("{}\n\n{}", s("message"), detail) };
    let mut builder = app.dialog().message(text).title(s("title")).kind(kind);
    builder = match buttons.as_slice() {
        [] => builder.buttons(MessageDialogButtons::Ok),
        [a] => builder.buttons(MessageDialogButtons::OkCustom(a.clone())),
        [a, b, ..] => builder.buttons(MessageDialogButtons::OkCancelCustom(a.clone(), b.clone())),
    };
    if let Some(w) = main_window(app) {
        builder = builder.parent(&w);
    }
    // 返回按下的按钮下标；Esc / 关闭按 0 处理（调用方把 0 放成安全选项）。
    let index = match builder.blocking_show_with_result() {
        MessageDialogResult::Custom(label) => buttons.iter().position(|b| *b == label).unwrap_or(0),
        MessageDialogResult::Cancel | MessageDialogResult::No => 0,
        _ => 0,
    };
    Ok(json!(index))
}

fn file_dialog<R: Runtime>(app: &AppHandle<R>, params: &Value) -> tauri_plugin_dialog::FileDialogBuilder<R> {
    let mut builder = app.dialog().file();
    if let Some(dir) = params.get("defaultPath").and_then(Value::as_str) {
        builder = builder.set_directory(dir);
    }
    if let Some(w) = main_window(app) {
        builder = builder.set_parent(&w);
    }
    builder
}

fn path_value(p: Option<tauri_plugin_dialog::FilePath>) -> Value {
    match p.and_then(|p| p.into_path().ok()) {
        Some(path) => Value::String(crate::strip_verbatim(path).to_string_lossy().into_owned()),
        None => Value::Null,
    }
}

fn pick_folder<R: Runtime>(app: &AppHandle<R>, params: &Value) -> Result<Value, String> {
    Ok(path_value(file_dialog(app, params).set_can_create_directories(true).blocking_pick_folder()))
}

fn pick_file<R: Runtime>(app: &AppHandle<R>, params: &Value) -> Result<Value, String> {
    let mut builder = file_dialog(app, params);
    if let Some(filters) = params.get("filters").and_then(Value::as_array) {
        for f in filters {
            let name = f.get("name").and_then(Value::as_str).unwrap_or("");
            let exts: Vec<&str> = f
                .get("extensions")
                .and_then(Value::as_array)
                .map(|a| a.iter().filter_map(Value::as_str).collect())
                .unwrap_or_default();
            builder = builder.add_filter(name, &exts);
        }
    }
    Ok(path_value(builder.blocking_pick_file()))
}

// ── 打开链接 / 文件 / 在文件夹中显示（URL 白名单在 sidecar 已校验） ──

fn str_param(params: &Value, key: &str) -> Result<String, String> {
    params.get(key).and_then(Value::as_str).map(str::to_owned).ok_or_else(|| format!("{key} is required"))
}

fn open_url<R: Runtime>(app: &AppHandle<R>, params: &Value) -> Result<Value, String> {
    app.opener().open_url(str_param(params, "url")?, None::<&str>).map_err(|e| e.to_string())?;
    Ok(Value::Null)
}

fn open_path<R: Runtime>(app: &AppHandle<R>, params: &Value) -> Result<Value, String> {
    app.opener().open_path(str_param(params, "path")?, None::<&str>).map_err(|e| e.to_string())?;
    Ok(Value::Null)
}

fn reveal<R: Runtime>(app: &AppHandle<R>, params: &Value) -> Result<Value, String> {
    app.opener().reveal_item_in_dir(PathBuf::from(str_param(params, "path")?)).map_err(|e| e.to_string())?;
    Ok(Value::Null)
}

// 主窗口里 target=_blank / window.open 的 http(s) 链接交给系统浏览器，其余一律拒绝
// （desktop setWindowOpenHandler；内部弹窗还没移植）。
pub fn open_external_from_webview<R: Runtime>(app: &AppHandle<R>, url: &tauri::Url) {
    if matches!(url.scheme(), "http" | "https") {
        if let Err(e) = app.opener().open_url(url.as_str(), None::<&str>) {
            eprintln!("[shell] open external failed: {e}");
        }
    }
}

// ── 分发 ──

fn run<R: Runtime>(app: &AppHandle<R>, method: &str, params: &Value) -> Result<Value, String> {
    match method {
        "notify" => show_notification(app, params),
        "badge.set" => {
            set_badge(app, params);
            Ok(Value::Null)
        }
        "tray.setLabels" => {
            let app2 = app.clone();
            let params = params.clone();
            app.run_on_main_thread(move || set_tray_labels(&app2, &params)).map_err(|e| e.to_string())?;
            Ok(Value::Null)
        }
        "window.closeAction" => {
            let action = params.get("action").and_then(Value::as_str).unwrap_or("").to_string();
            let app2 = app.clone();
            app.run_on_main_thread(move || apply_close_action(&app2, &action)).map_err(|e| e.to_string())?;
            Ok(Value::Null)
        }
        "dialog.message" => dialog_message(app, params),
        "dialog.pickFolder" => pick_folder(app, params),
        "dialog.pickFile" => pick_file(app, params),
        "open.url" => open_url(app, params),
        // sidecar 里的 model-access / device-link 经这两个回调使用 Rust 的账号会话。
        "auth.refresh" | "auth.expire" => AUTH
            .get()
            .cloned()
            .ok_or_else(|| "[PRECONDITION_FAILED] auth is not ready".to_string())?
            .handle_native(method, params)
            .unwrap_or(Ok(Value::Null)),
        "open.path" => open_path(app, params),
        "reveal" => reveal(app, params),
        // 从桌面版导入：取 Electron safeStorage 密钥材料（macOS 可能弹钥匙串授权框）。
        "desktopImport.safeStorageKey" => crate::desktop_import::safe_storage_key(params),
        // goal 控制器经 sidecar 回调 Rust 会话宿主（会话在 Rust 时，goal 的发送 / 中止走这里）。
        "maker.enqueue" => crate::maker_wiring::native_enqueue(app, params),
        "maker.abort" => crate::maker_wiring::native_abort(app, params),
        "maker.invoke" => crate::maker_wiring::native_invoke(app, params),
        "maker.gateway-key" => crate::maker_wiring::native_gateway_key(app, params),
        // messages:view：分页在 Rust，sidecar 只做 projectHistoryView 投影。
        "local-db.view-page" => LOCAL_DB.get().ok_or("[PRECONDITION_FAILED] local database is not ready")?.view_page(&params["sid"], &params["opts"]),
        // update.* 原生路由已移除：更新状态机在 Rust（update_machine.rs），直接调 updater.rs。
        other => Err(format!("[NOT_IMPLEMENTED] native method {other}")),
    }
}

/// 账号登录的生产端口。
pub struct AuthPorts(pub AppHandle);

impl crate::auth::env::Ports for AuthPorts {
    fn emit(&self, channel: &str, arg: Value) {
        crate::desktop_shell::Ports::emit(&AppPorts(self.0.clone()), channel, arg);
    }
    fn open_url(&self, url: &str) -> Result<(), String> {
        open_url(&self.0, &json!({ "url": url })).map(|_| ())
    }
    fn locale(&self) -> String {
        shell().map(|s| s.current_locale()).unwrap_or_else(|| "en".into())
    }
}

/// 设备互联宿主的外部依赖（账号令牌、renderer 推送、本机 invoke 路由）。
pub struct DeviceLinkPorts {
    pub app: AppHandle,
    pub name: String,
}

impl crate::device_link::host::Ports for DeviceLinkPorts {
    fn token(&self) -> crate::device_link::runtime::BoxFut<Option<String>> {
        Box::pin(async move {
            let auth = AUTH.get().cloned()?;
            if !auth.info().authenticated {
                return None;
            }
            if let Some(t) = auth.access_token() {
                return Some(t);
            }
            auth.refresh().await;
            auth.access_token()
        })
    }
    fn authenticated(&self) -> (bool, Option<String>) {
        AUTH.get().map(|a| a.info()).map(|i| (i.authenticated, i.user_id)).unwrap_or((false, None))
    }
    fn ws_url(&self) -> String {
        let base = crate::auth::env::device_link_base_url();
        let base = if let Some(r) = base.strip_prefix("https") { format!("wss{r}") } else if let Some(r) = base.strip_prefix("http") { format!("ws{r}") } else { base };
        format!("{base}/api/device-link/ws")
    }
    fn http_base(&self) -> String {
        crate::auth::env::device_link_base_url()
    }
    fn emit(&self, channel: &str, arg: Value) {
        // 只推 renderer：设备互联自己的状态通知不需要再转给手机
        use tauri::Emitter;
        let payload = serde_json::to_string(&json!({ "v": [arg] })).unwrap_or_default();
        let _ = self.app.emit_to(tauri::EventTarget::webview("main"), "cindy:ipc", json!({ "channel": channel, "payload": payload }));
    }
    fn owner_stamp(&self) -> Value {
        use tauri::Manager;
        self.app.state::<std::sync::Arc<crate::owner::Owner>>().stamp()
    }
    fn invoke(&self, channel: String, args: Vec<Value>) -> crate::device_link::runtime::BoxFut<Result<Value, String>> {
        let app = self.app.clone();
        Box::pin(async move {
            let payload = serde_json::to_string(&json!({ "v": args })).unwrap_or_default();
            // 远程调用走与 renderer 同一套路由，返回值是 `{"v": <value>}` 编码串
            let r = crate::ipc_invoke_core(&app, None, channel, payload).await?;
            let s = r.as_str().ok_or_else(|| "[INTERNAL] unexpected invoke result".to_string())?;
            let decoded: Value = serde_json::from_str(s).map_err(|e| format!("[INTERNAL] {e}"))?;
            Ok(decoded.get("v").cloned().unwrap_or(Value::Null))
        })
    }
    fn log(&self, level: &str, msg: &str) {
        eprintln!("[device-link] {level} {msg}");
    }
    fn device_name(&self) -> String {
        self.name.clone()
    }
    fn app_version(&self) -> String {
        std::env::var("CINDY_TAURI_APP_VERSION").unwrap_or_else(|_| env!("CARGO_PKG_VERSION").to_owned())
    }
}
