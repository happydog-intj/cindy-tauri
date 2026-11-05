// sidecar → Rust 的反向调用（sidecar/src/native.mjs）和桌面外壳的原生部分：
// 系统通知、Dock 角标、原生对话框、打开链接 / 文件、Windows 托盘、主窗口关闭行为。
// IPC 校验和文案都在 sidecar（desktop-shell.mjs），这里只执行原生动作。
use crate::sidecar::Sidecar;
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

pub static SIDECAR: OnceLock<Arc<Sidecar>> = OnceLock::new();
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
const TRAY_ID: &str = "main";

fn sidecar() -> Option<Arc<Sidecar>> {
    // 反向调用可能早于 SIDECAR 设置（sidecar 启动即发），稍等一下。
    for _ in 0..50 {
        if let Some(sc) = SIDECAR.get() {
            return Some(sc.clone());
        }
        thread::sleep(Duration::from_millis(20));
    }
    None
}

// 自动更新安装前要让 sidecar 退出。
pub fn sidecar_handle() -> Option<Arc<Sidecar>> {
    SIDECAR.get().cloned()
}

// 调用 sidecar 里只给 Rust 用的 native.* 方法（index.mjs rustOnly）。
fn call_sidecar(method: &str, params: Value) -> Result<Value, String> {
    sidecar().ok_or_else(|| "sidecar unavailable".to_string())?.call(method, params)
}

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
            let action = call_sidecar("native.mainWindowCloseRequested", json!({}))
                .ok()
                .and_then(|v| v.get("action").and_then(Value::as_str).map(str::to_owned))
                .unwrap_or_else(|| "quit".to_string());
            apply_close_action(&app, &action);
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
                let _ = call_sidecar("native.trayUnavailable", json!({}));
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
                let _ = call_sidecar("native.trayOpenLogs", json!({}));
            });
        }
        "tray-quit" => {
            let app = app.clone();
            thread::spawn(move || {
                // 有任务在跑时 sidecar 会先弹确认框；sidecar 不可用时直接退出。
                let ok = match call_sidecar("native.trayQuit", json!({})) {
                    Ok(v) => v.get("quit").and_then(Value::as_bool).unwrap_or(false),
                    Err(_) => true,
                };
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
            let _ = call_sidecar("native.notificationClicked", json!({ "sessionId": session_id }));
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
        "open.path" => open_path(app, params),
        "reveal" => reveal(app, params),
        // 从桌面版导入：取 Electron safeStorage 密钥材料（macOS 可能弹钥匙串授权框）。
        "desktopImport.safeStorageKey" => crate::desktop_import::safe_storage_key(params),
        // 自动更新：检查 / 下载 / 安装 / 系统空闲时长（updater.rs）。
        m if m.starts_with("update.") => crate::updater::run(app, m, params),
        other => Err(format!("[NOT_IMPLEMENTED] native method {other}")),
    }
}

// sidecar 读线程调用：每个请求放独立线程，阻塞式对话框不卡读线程和主线程。
pub fn dispatch<R: Runtime>(app: &AppHandle<R>, msg: Value) {
    let app = app.clone();
    thread::spawn(move || {
        let method = msg.get("method").and_then(Value::as_str).unwrap_or("").to_string();
        let params = msg.get("params").cloned().unwrap_or(Value::Null);
        let result = run(&app, &method, &params);
        match msg.get("id").and_then(Value::as_u64) {
            Some(id) => {
                if let Some(sc) = sidecar() {
                    sc.reply_native(id, result);
                }
            }
            None => {
                if let Err(e) = result {
                    eprintln!("[native] {method} failed: {e}");
                }
            }
        }
    });
}
