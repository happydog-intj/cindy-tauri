// Cindy Tauri 入口。
// - 拉起 Node sidecar，前端通过 sidecar_call / ipc_invoke / ipc_send 与它通信；
// - 主窗口加载 cindy 的真实 renderer，并在页面脚本之前注入编译好的 preload（window.electronAPI）；
// - sendSync 走 cindy-ipc 自定义协议（同步 XHR）；
// - sidecar 推送的事件转发为 Tauri 事件 "cindy:ipc"；
// - CINDY_TAURI_SMOKE=1 时先跑 /__smoke/ 页面，再加载真实 renderer，最后写 CINDY_TAURI_SMOKE_OUT 并退出。
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod native;
mod secrets;
mod sidecar;

use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::{Arc, OnceLock};
use std::thread;
use std::time::Duration;
use tauri::http::{header, Response, StatusCode};
use tauri::{AppHandle, Emitter, Manager, State, WebviewUrl, WebviewWindowBuilder};

type SidecarHandle = Result<Arc<sidecar::Sidecar>, String>;

struct SidecarState(SidecarHandle);

// 实际使用的数据目录（冒烟收尾时按它清理钥匙串条目）。
static DATA_DIR: OnceLock<PathBuf> = OnceLock::new();

// 编译期嵌入的 preload（scripts/build-preload.mjs 生成）。
const PRELOAD_JS: &str = include_str!("../preload/cindy-preload.js");

// 冒烟包含 Pi 按需下载（30–45MB）、一次可选的真实模型调用和 renderer 首屏加载。
const SMOKE_TIMEOUT: Duration = Duration::from_secs(600);

fn smoke_out() -> Option<String> {
    if std::env::var("CINDY_TAURI_SMOKE").as_deref() != Ok("1") {
        return None;
    }
    std::env::var("CINDY_TAURI_SMOKE_OUT").ok()
}

fn write_smoke(ok: bool, detail: &str) {
    if let Some(out) = smoke_out() {
        let body = json!({
            "ok": ok,
            "detail": detail,
            "os": std::env::consts::OS,
            "arch": std::env::consts::ARCH,
        });
        let _ = std::fs::write(out, body.to_string());
    }
}

// Windows 上 Tauri 返回的路径可能带 \\?\ 前缀，Node 的 realpath 不接受，传给子进程前去掉。
pub(crate) fn strip_verbatim(p: PathBuf) -> PathBuf {
    let s = p.to_string_lossy().into_owned();
    if let Some(rest) = s.strip_prefix(r"\\?\UNC\") {
        PathBuf::from(format!(r"\\{rest}"))
    } else if let Some(rest) = s.strip_prefix(r"\\?\") {
        PathBuf::from(rest.to_string())
    } else {
        p
    }
}

// 开发模式用系统 node（或 CINDY_NODE 指定），发布模式用随包的 cindy-node。
fn runtime_paths(app: &AppHandle) -> (PathBuf, PathBuf) {
    if cfg!(debug_assertions) {
        let node = std::env::var("CINDY_NODE").map(PathBuf::from).unwrap_or_else(|_| PathBuf::from("node"));
        let script = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../sidecar/dist/sidecar.cjs");
        (node, script)
    } else {
        let exe_dir = std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(|d| d.to_path_buf()))
            .unwrap_or_default();
        let name = if cfg!(windows) { "cindy-node.exe" } else { "cindy-node" };
        let script = app.path().resource_dir().map(|d| d.join("sidecar/sidecar.cjs")).unwrap_or_default();
        (strip_verbatim(exe_dir.join(name)), strip_verbatim(script))
    }
}

// Electron 的 process.platform / arch 取值。
fn node_platform() -> &'static str {
    match std::env::consts::OS {
        "macos" => "darwin",
        "windows" => "win32",
        other => other,
    }
}

fn node_arch() -> &'static str {
    match std::env::consts::ARCH {
        "aarch64" => "arm64",
        "x86_64" => "x64",
        other => other,
    }
}

// 会话冒烟阶段：run（建会话、发消息、审批）或 verify（重启后检查会话列表）。只在冒烟模式下生效。
fn session_smoke_phase() -> Option<String> {
    smoke_out()?;
    match std::env::var("CINDY_TAURI_SESSION_SMOKE").as_deref() {
        Ok(p @ ("run" | "verify")) => Some(p.to_string()),
        _ => None,
    }
}

fn boot_script() -> String {
    let boot = json!({
        "platform": node_platform(),
        "arch": node_arch(),
        "argv": [],
        "smoke": smoke_out().is_some(),
        "sessionSmoke": session_smoke_phase().is_some(),
        "sessionSmokePhase": session_smoke_phase(),
    });
    format!("Object.defineProperty(window, '__CINDY_BOOT__', {{ value: Object.freeze({boot}), configurable: false }});")
}

fn sidecar(state: &State<'_, SidecarState>) -> Result<Arc<sidecar::Sidecar>, String> {
    state.0.clone()
}

#[tauri::command]
fn smoke_info() -> Option<Value> {
    smoke_out().map(|out| {
        json!({
            "out": out,
            // 可选：冒烟时跑一次真实任务。未设置则只验证到 agent 下载。
            "modelBaseUrl": std::env::var("CINDY_TAURI_SMOKE_MODEL_BASE_URL").ok(),
            "modelId": std::env::var("CINDY_TAURI_SMOKE_MODEL_ID").ok(),
            // 冒烟脚本每次生成的一次性假 key，用来走设置页保存 key 的同一条 IPC。
            "apiKey": std::env::var("CINDY_TAURI_SMOKE_API_KEY").ok(),
        })
    })
}

#[tauri::command]
async fn sidecar_call(state: State<'_, SidecarState>, method: String, params: Value) -> Result<Value, String> {
    // 主密钥只能由 Rust 自己送，native.* 是 Rust 专用回调，webview 都不能调用。
    if method.starts_with("secrets.") || method.starts_with("native.") {
        return Err(format!("method {method} is not callable from the webview"));
    }
    let sc = sidecar(&state)?;
    tauri::async_runtime::spawn_blocking(move || sc.call(&method, params)).await.map_err(|e| e.to_string())?
}

// payload 是 shared/ipc-codec.mjs 编码后的参数数组，原样交给 sidecar，返回同样编码的结果。
#[tauri::command]
async fn ipc_invoke(
    window: tauri::WebviewWindow,
    state: State<'_, SidecarState>,
    channel: String,
    payload: String,
) -> Result<Value, String> {
    if channel == "get-fullscreen-state" {
        let full = window.is_fullscreen().unwrap_or(false);
        // 与 shared/ipc-codec.mjs 的编码一致：普通 JSON 值原样序列化。
        return Ok(Value::String(serde_json::to_string(&full).unwrap()));
    }
    let sc = sidecar(&state)?;
    tauri::async_runtime::spawn_blocking(move || sc.call("ipc.invoke", json!({ "channel": channel, "payload": payload })))
        .await
        .map_err(|e| e.to_string())?
}

// 窗口控制类 channel 在 Rust 里原生处理（对齐 desktop bootstrap-electron.ts 的 ipcMain.on 实现）。
fn handle_window_send(window: &tauri::WebviewWindow, channel: &str) -> bool {
    let result = match channel {
        "window-minimize" => window.minimize(),
        "window-maximize" => {
            if window.is_maximized().unwrap_or(false) { window.unmaximize() } else { window.maximize() }
        }
        "window-exit-fullscreen" => window.set_fullscreen(false),
        // 自定义 X 按钮：主窗口 = 退出 app；Windows 走 close()，由关闭行为决定收起到托盘还是退出（与 desktop 一致）。
        "window-close" => {
            if window.label() == "main" && !cfg!(windows) {
                native::quit(window.app_handle());
                Ok(())
            } else {
                window.close()
            }
        }
        "window-close-self" => window.close(),
        _ => return false,
    };
    if let Err(e) = result {
        eprintln!("[window] {channel} failed: {e}");
    }
    true
}

#[tauri::command]
async fn ipc_send(
    window: tauri::WebviewWindow,
    state: State<'_, SidecarState>,
    channel: String,
    payload: String,
) -> Result<(), String> {
    if handle_window_send(&window, &channel) {
        return Ok(());
    }
    let sc = sidecar(&state)?;
    tauri::async_runtime::spawn_blocking(move || sc.call("ipc.send", json!({ "channel": channel, "payload": payload })))
        .await
        .map_err(|e| e.to_string())??;
    Ok(())
}

#[tauri::command]
fn smoke_finish(app: AppHandle, ok: bool, detail: String) {
    write_smoke(ok, &detail);
    // 冒烟专用的钥匙串主密钥：会话冒烟 run 阶段成功时留给 verify 用，其余情况都删掉。
    if session_smoke_phase().as_deref() != Some("run") || !ok {
        if let Some(dir) = DATA_DIR.get() {
            secrets::delete_smoke_entry(dir);
        }
    }
    app.exit(if ok { 0 } else { 1 });
}

// cindy-ipc://localhost/sync/<channel>，请求体是编码后的参数数组。
fn handle_sync(app: &AppHandle, path: &str, body: Vec<u8>) -> Response<Vec<u8>> {
    let respond = |status: StatusCode, text: String| {
        Response::builder()
            .status(status)
            .header(header::CONTENT_TYPE, "text/plain; charset=utf-8")
            .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
            .body(text.into_bytes())
            .unwrap()
    };
    let Some(raw) = path.strip_prefix("/sync/") else {
        return respond(StatusCode::NOT_FOUND, "unknown path".into());
    };
    let channel = percent_decode(raw);
    let payload = String::from_utf8_lossy(&body).into_owned();
    let state = app.state::<SidecarState>();
    let result = state.0.clone().and_then(|sc| sc.call("ipc.sendSync", json!({ "channel": channel, "payload": payload })));
    match result {
        Ok(Value::String(encoded)) => respond(StatusCode::OK, encoded),
        Ok(other) => respond(StatusCode::INTERNAL_SERVER_ERROR, format!("unexpected sidecar reply: {other}")),
        Err(e) => respond(StatusCode::INTERNAL_SERVER_ERROR, e),
    }
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Some(v) = std::str::from_utf8(&bytes[i + 1..i + 3]).ok().and_then(|h| u8::from_str_radix(h, 16).ok()) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn create_main_window(app: &AppHandle) -> tauri::Result<()> {
    let entry = if smoke_out().is_some() { "__smoke/index.html" } else { "index.html" };
    let builder = WebviewWindowBuilder::new(app, "main", WebviewUrl::App(entry.into()))
        .title("Cindy")
        .inner_size(1280.0, 800.0)
        .min_inner_size(800.0, 600.0)
        .initialization_script(boot_script())
        .initialization_script(PRELOAD_JS)
        // 主窗口里的 http(s) 链接交给系统浏览器（desktop will-navigate / setWindowOpenHandler）。
        .on_navigation({
            let app = app.clone();
            move |url| {
                if is_internal_url(url) {
                    return true;
                }
                native::open_external_from_webview(&app, url);
                false
            }
        })
        .on_new_window({
            let app = app.clone();
            move |url, _features| {
                native::open_external_from_webview(&app, &url);
                tauri::webview::NewWindowResponse::Deny
            }
        });
    // 与 Electron 版一致：macOS 隐藏标题栏保留红绿灯，Windows 无边框（renderer 自绘窗口按钮）。
    #[cfg(target_os = "macos")]
    let builder = builder
        .title_bar_style(tauri::TitleBarStyle::Overlay)
        .hidden_title(true)
        .traffic_light_position(tauri::LogicalPosition::new(12.0, 16.0));
    #[cfg(not(target_os = "macos"))]
    let builder = builder.decorations(false);
    let window = builder.build()?;
    // 全屏切换推送 fullscreen-change（desktop mainWindowFullscreenStartup.ts 的行为）。
    let watched = window.clone();
    let last = std::sync::atomic::AtomicBool::new(window.is_fullscreen().unwrap_or(false));
    window.on_window_event(move |event| {
        if let tauri::WindowEvent::CloseRequested { api, .. } = event {
            native::on_main_close_requested(&watched, api);
            return;
        }
        if let tauri::WindowEvent::Resized(_) = event {
            let now = watched.is_fullscreen().unwrap_or(false);
            if last.swap(now, std::sync::atomic::Ordering::Relaxed) != now {
                let payload = serde_json::to_string(&[now]).unwrap();
                let _ = watched.emit_to("main", "cindy:ipc", json!({ "channel": "fullscreen-change", "payload": payload }));
            }
        }
    });
    Ok(())
}

// renderer 自身的页面（tauri:// / http://tauri.localhost / 开发服务器）和内部 scheme。
fn is_internal_url(url: &tauri::Url) -> bool {
    match url.scheme() {
        "tauri" | "cindy-ipc" | "about" | "data" | "blob" => true,
        "http" | "https" => match url.host_str() {
            Some("tauri.localhost" | "cindy-ipc.localhost") => true,
            // 开发服务器只在 debug 构建放行。
            Some("localhost" | "127.0.0.1") => cfg!(debug_assertions),
            _ => false,
        },
        _ => false,
    }
}

fn main() {
    let app = tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        // 不注入 opener 的 JS 点击拦截（需要给 webview 开 opener 权限）；_blank 链接走 on_new_window。
        .plugin(tauri_plugin_opener::Builder::default().open_js_links_on_click(false).build())
        .plugin(tauri_plugin_notification::init())
        .register_asynchronous_uri_scheme_protocol("cindy-ipc", |ctx, request, responder| {
            let app = ctx.app_handle().clone();
            let path = request.uri().path().to_string();
            let body = request.body().clone();
            thread::spawn(move || responder.respond(handle_sync(&app, &path, body)));
        })
        .setup(|app| {
            let (node, script) = runtime_paths(app.handle());
            // CINDY_TAURI_DATA_DIR 覆盖数据目录（冒烟和开发用临时目录）。
            let data_dir = strip_verbatim(match std::env::var_os("CINDY_TAURI_DATA_DIR") {
                Some(dir) if !dir.is_empty() => PathBuf::from(dir),
                _ => app.path().app_data_dir().unwrap_or_else(|_| std::env::temp_dir().join("cindy-tauri")),
            });
            let _ = std::fs::create_dir_all(&data_dir);
            let env = [
                ("CINDY_TAURI_DATA_DIR", data_dir.to_string_lossy().into_owned()),
                ("CINDY_TAURI_APP_VERSION", app.package_info().version.to_string()),
            ];
            let emitter = app.handle().clone();
            let native_app = app.handle().clone();
            // 通知点击的回调要在第一条通知前注册，否则点击不会上报。
            native::register_notification_click(app.handle());
            let handle = sidecar::Sidecar::spawn(
                &node,
                &script,
                &env,
                move |push| {
                    if let Err(e) = emitter.emit("cindy:ipc", push) {
                        eprintln!("[sidecar] emit failed: {e}");
                    }
                },
                move |msg| native::dispatch(&native_app, msg),
            )
            .map(Arc::new)
            .map_err(|e| format!("spawn sidecar failed ({}): {e}", node.display()));
            if let Err(e) = &handle {
                eprintln!("{e}");
            }
            let _ = DATA_DIR.set(data_dir.clone());
            if let Ok(sc) = &handle {
                let _ = native::SIDECAR.set(sc.clone());
            }
            // 钥匙串可能弹系统授权框，放到后台线程；sidecar 收到前，带 key 的 provider 显示为未连接。
            if let Ok(sc) = &handle {
                let sc = sc.clone();
                let smoke = smoke_out().is_some();
                let dir = data_dir.clone();
                thread::spawn(move || {
                    let reply = match secrets::load_or_create(smoke, &dir) {
                        Ok(key) => sc.call("secrets.unlock", json!({ "key": key })),
                        Err(reason) => {
                            eprintln!("[secrets] keychain unavailable: {reason}");
                            sc.call("secrets.unavailable", json!({ "reason": reason }))
                        }
                    };
                    if let Err(e) = reply {
                        eprintln!("[secrets] handing master key to sidecar failed: {e}");
                    }
                });
            }
            app.manage(SidecarState(handle));
            create_main_window(app.handle())?;
            // desktop：Windows 启动就建托盘，渲染层出错时也能从托盘打开日志。
            if cfg!(windows) && smoke_out().is_none() {
                native::ensure_tray(app.handle());
            }

            if smoke_out().is_some() {
                thread::spawn(|| {
                    thread::sleep(SMOKE_TIMEOUT);
                    write_smoke(false, "smoke timeout");
                    std::process::exit(1);
                });
            }
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![smoke_info, sidecar_call, smoke_finish, ipc_invoke, ipc_send])
        .build(tauri::generate_context!())
        .expect("error while building cindy-tauri");
    app.run(|app, event| match event {
        // macOS 点 Dock 图标：恢复隐藏的主窗口。
        #[cfg(target_os = "macos")]
        tauri::RunEvent::Reopen { .. } => native::focus_main_window(app),
        // ⌘Q / 菜单退出：不再拦截主窗口关闭。
        tauri::RunEvent::ExitRequested { .. } => native::mark_quitting(),
        _ => {
            let _ = app;
        }
    });
}
