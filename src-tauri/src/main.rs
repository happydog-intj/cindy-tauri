// Cindy Tauri 入口。
// - 拉起 Node sidecar，前端通过 sidecar_call / ipc_invoke / ipc_send 与它通信；
// - 主窗口加载 cindy 的真实 renderer，并在页面脚本之前注入编译好的 preload（window.electronAPI）；
// - sendSync 走 cindy-ipc 自定义协议（同步 XHR）；
// - sidecar 推送的事件转发为 Tauri 事件 "cindy:ipc"；
// - CINDY_TAURI_SMOKE=1 时先跑 /__smoke/ 页面，再加载真实 renderer，最后写 CINDY_TAURI_SMOKE_OUT 并退出。
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod sidecar;

use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::Arc;
use std::thread;
use std::time::Duration;
use tauri::http::{header, Response, StatusCode};
use tauri::{AppHandle, Emitter, Manager, State, WebviewUrl, WebviewWindowBuilder};

type SidecarHandle = Result<Arc<sidecar::Sidecar>, String>;

struct SidecarState(SidecarHandle);

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
fn strip_verbatim(p: PathBuf) -> PathBuf {
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

fn boot_script() -> String {
    let boot = json!({
        "platform": node_platform(),
        "arch": node_arch(),
        "argv": [],
        "smoke": smoke_out().is_some(),
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
        })
    })
}

#[tauri::command]
async fn sidecar_call(state: State<'_, SidecarState>, method: String, params: Value) -> Result<Value, String> {
    let sc = sidecar(&state)?;
    tauri::async_runtime::spawn_blocking(move || sc.call(&method, params)).await.map_err(|e| e.to_string())?
}

// payload 是 shared/ipc-codec.mjs 编码后的参数数组，原样交给 sidecar，返回同样编码的结果。
#[tauri::command]
async fn ipc_invoke(state: State<'_, SidecarState>, channel: String, payload: String) -> Result<Value, String> {
    let sc = sidecar(&state)?;
    tauri::async_runtime::spawn_blocking(move || sc.call("ipc.invoke", json!({ "channel": channel, "payload": payload })))
        .await
        .map_err(|e| e.to_string())?
}

#[tauri::command]
async fn ipc_send(state: State<'_, SidecarState>, channel: String, payload: String) -> Result<(), String> {
    let sc = sidecar(&state)?;
    tauri::async_runtime::spawn_blocking(move || sc.call("ipc.send", json!({ "channel": channel, "payload": payload })))
        .await
        .map_err(|e| e.to_string())??;
    Ok(())
}

#[tauri::command]
fn smoke_finish(app: AppHandle, ok: bool, detail: String) {
    write_smoke(ok, &detail);
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
        .initialization_script(PRELOAD_JS);
    // 与 Electron 版一致：macOS 隐藏标题栏保留红绿灯，Windows 无边框（renderer 自绘窗口按钮）。
    #[cfg(target_os = "macos")]
    let builder = builder
        .title_bar_style(tauri::TitleBarStyle::Overlay)
        .hidden_title(true)
        .traffic_light_position(tauri::LogicalPosition::new(12.0, 16.0));
    #[cfg(not(target_os = "macos"))]
    let builder = builder.decorations(false);
    builder.build()?;
    Ok(())
}

fn main() {
    tauri::Builder::default()
        .register_asynchronous_uri_scheme_protocol("cindy-ipc", |ctx, request, responder| {
            let app = ctx.app_handle().clone();
            let path = request.uri().path().to_string();
            let body = request.body().clone();
            thread::spawn(move || responder.respond(handle_sync(&app, &path, body)));
        })
        .setup(|app| {
            let (node, script) = runtime_paths(app.handle());
            let data_dir = strip_verbatim(
                app.path().app_data_dir().unwrap_or_else(|_| std::env::temp_dir().join("cindy-tauri")),
            );
            let _ = std::fs::create_dir_all(&data_dir);
            let env = [
                ("CINDY_TAURI_DATA_DIR", data_dir.to_string_lossy().into_owned()),
                ("CINDY_TAURI_APP_VERSION", app.package_info().version.to_string()),
            ];
            let emitter = app.handle().clone();
            let handle = sidecar::Sidecar::spawn(&node, &script, &env, move |push| {
                if let Err(e) = emitter.emit("cindy:ipc", push) {
                    eprintln!("[sidecar] emit failed: {e}");
                }
            })
            .map(Arc::new)
            .map_err(|e| format!("spawn sidecar failed ({}): {e}", node.display()));
            if let Err(e) = &handle {
                eprintln!("{e}");
            }
            app.manage(SidecarState(handle));
            create_main_window(app.handle())?;

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
        .run(tauri::generate_context!())
        .expect("error while running cindy-tauri");
}
