// Cindy Tauri 入口。
// - 前端通过 ipc_invoke / ipc_send 与 Rust 宿主通信；
// - 主窗口加载 cindy 的真实 renderer，并在页面脚本之前注入编译好的 preload（window.electronAPI）；
// - sendSync 走 cindy-ipc 自定义协议（同步 XHR）；
// - sidecar 推送的事件转发为 Tauri 事件 "cindy:ipc"；
// - CINDY_TAURI_SMOKE=1 时先跑 /__smoke/ 页面，再加载真实 renderer，最后写 CINDY_TAURI_SMOKE_OUT 并退出。
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod agent_catalog;
mod app_settings;
mod boot_stubs;
mod browser_panel;
mod layout_core;
mod owner;
mod secret_store;
mod voice_input;
#[allow(dead_code)]
mod voice_dict;
#[allow(dead_code)]
mod voice_settings;
#[allow(dead_code)]
mod appearance_core;
#[allow(dead_code)]
mod shortcuts_core;
mod desktop_shell;
#[allow(dead_code)]
mod import_crypto;
#[allow(dead_code)]
mod import_db;
mod import_run;
#[allow(dead_code)]
mod goal_core;
#[allow(dead_code)]
mod goal_host;
mod import_sessions;
#[allow(dead_code)]
mod shell_policy;
mod browser;
mod desktop_import;
mod bang_cmd;
mod dir_grants;
mod input_queue;
mod interactions;
mod maker_host;
mod maker_wiring;
#[cfg(test)]
mod rust_session_e2e;
#[cfg(test)]
mod import_e2e;
mod maker_registry;
mod pi_launch;
mod pi_proc;
mod pi_session_core;
// 事件翻译器与 pi-rs 共用同一份源码（pi-rs/src/maker.rs），maker-freeze 保证它与 TS 参考实现逐帧一致。
#[allow(dead_code)]
#[path = "../../pi-rs/src/maker.rs"]
mod maker_translate;
// maker.rs 用 `crate::maker_errors` 引用错误分类（pi-rs 里它是同级模块）；这里按同名挂在 crate 根下。
#[allow(dead_code)]
#[path = "../../pi-rs/src/maker_errors.rs"]
mod maker_errors;
#[allow(dead_code)]
#[path = "../../pi-rs/src/maker_text.rs"]
mod maker_text;
#[macro_use]
mod applog;
mod auth;
mod billing;
mod device_link;
mod history_view;
mod model_access;
mod model_config;
#[cfg(test)]
mod auth_e2e;
mod local_db;
mod fallback;
mod maker_settings;
mod misc_settings;
mod native;
mod override_file;
mod secrets;
mod terminal;
mod update_machine;
mod updater;

use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::{Arc, OnceLock};
use std::thread;
use std::time::Duration;
use tauri::http::{header, Response, StatusCode};
use tauri::{AppHandle, Emitter, EventTarget, Manager, State, Webview, WebviewUrl, WebviewWindowBuilder, Window};

// 终端 PTY（原 node-pty，已迁到 Rust）。
/// Rust 会话宿主；只有找不到 pi-rs 时为空（会话通道会返回明确错误，没有 sidecar 回退）。
struct SettingsState(Arc<maker_settings::MakerSettings>, Arc<misc_settings::MiscSettings>, Arc<app_settings::AppSettings>, Arc<voice_input::VoiceInput>, Arc<browser_panel::BrowserPanel>, Arc<boot_stubs::BootStubs>);
struct MakerState(Option<Arc<maker_registry::MakerRegistry>>);
struct TerminalState(Arc<terminal::TerminalManager>);

// 自动更新状态机（原 sidecar update.mjs，已迁到 Rust）。
struct UpdateState(Arc<update_machine::UpdateMachine>);

// 通用 IPC 推送：payload 用 ipc-codec 的 encode 形状（{"v":[arg]}）。
struct IpcPush(AppHandle);
impl IpcPush {
    fn send(&self, channel: &str, arg: &Value) {
        let payload = serde_json::to_string(&json!({ "v": [arg] })).unwrap();
        if let Err(e) = self.0.emit_to(EventTarget::webview("main"), "cindy:ipc", json!({ "channel": channel, "payload": payload })) {
            eprintln!("[ipc-push] emit {channel} failed: {e}");
        }
    }
}
impl update_machine::Push for IpcPush {
    fn push(&self, channel: &str, arg: Value) {
        self.send(channel, &arg);
    }
}

// 自动更新的忙闲 / 空闲自动重启开关：忙闲看会话注册表（没有 pi-rs 时没有会话，视为空闲）。
struct UpdateProbe(AppHandle, Arc<app_settings::AppSettings>);
impl update_machine::HostProbe for UpdateProbe {
    fn busy(&self) -> Result<bool, ()> {
        match self.0.try_state::<MakerState>().and_then(|s| s.0.clone()) {
            Some(reg) => reg.invoke("maker:any-session-in-turn", &[]).ok().and_then(|v| v.as_bool()).ok_or(()),
            None => Ok(false),
        }
    }
    fn auto_relaunch_enabled(&self) -> Result<bool, ()> {
        Ok(self.1.auto_relaunch_on_idle())
    }
}

// 浏览器面板打开系统浏览器（扩展商店）。
struct AppUrlOpener(AppHandle);
impl browser_panel::UrlOpener for AppUrlOpener {
    fn open(&self, url: &str) -> Result<(), String> {
        use tauri_plugin_opener::OpenerExt;
        self.0.opener().open_url(url, None::<&str>).map_err(|e| e.to_string())
    }
}

// 终端推送：走 "cindy:ipc" 事件，payload 用 ipc-codec 的 encode 形状（{"v":[args]}）。
struct AppTerminalPush(AppHandle);
impl terminal::TerminalPush for AppTerminalPush {
    fn data(&self, id: &str, chunk: &str) {
        self.send("terminal:data", json!({ "id": id, "chunk": chunk }));
    }
    fn exit(&self, id: &str, exit: Value) {
        self.send("terminal:exit", json!({ "id": id, "exit": exit }));
    }
}
impl AppTerminalPush {
    fn send(&self, channel: &str, obj: Value) {
        let payload = serde_json::to_string(&json!({ "v": [obj] })).unwrap();
        if let Err(e) = self.0.emit_to(EventTarget::webview("main"), "cindy:ipc", json!({ "channel": channel, "payload": payload })) {
            eprintln!("[terminal] emit {channel} failed: {e}");
        }
    }
}

// 实际使用的数据目录（冒烟收尾时按它清理钥匙串条目）。
static DATA_DIR: OnceLock<PathBuf> = OnceLock::new();

// 编译期嵌入的 preload（scripts/build-frontend.mjs 生成）。
const PRELOAD_JS: &str = include_str!("../preload/cindy-preload.js");

// 冒烟包含 Pi 按需下载（30–45MB）、一次可选的真实模型调用和 renderer 首屏加载。
const SMOKE_TIMEOUT: Duration = Duration::from_secs(600);

fn no_keychain_smoke() -> bool {
    smoke_out().is_some() && std::env::var("CINDY_TAURI_SMOKE_NO_KEYCHAIN").as_deref() == Ok("1")
}

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
        "loginSmokeEmail": smoke_out().and(std::env::var("CINDY_TAURI_SMOKE_LOGIN_EMAIL").ok()),
        "settingsSmoke": smoke_out().is_some() && std::env::var("CINDY_TAURI_SMOKE_SETTINGS").as_deref() == Ok("1"),
        // 浏览器面板冒烟用的本地测试页（scripts/smoke.mjs 起的 127.0.0.1 服务）。
        "browserSmokeUrl": smoke_out().and(std::env::var("CINDY_TAURI_SMOKE_BROWSER_URL").ok()),
        // 自动更新冒烟：期望更新到的版本（scripts/update-smoke.mjs 起本机 latest.json）。
        "updateSmokeExpect": updater::smoke_endpoint().and(std::env::var("CINDY_TAURI_SMOKE_UPDATE_EXPECT").ok()),
    });
    format!("Object.defineProperty(window, '__CINDY_BOOT__', {{ value: Object.freeze({boot}), configurable: false }});")
}

fn dirs_home() -> std::path::PathBuf {
    std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" }).map(std::path::PathBuf::from).unwrap_or_default()
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

/// 冒烟页 / 开发页的运行时自检：进程信息 + 随包 pi-rs 是否就绪。
/// 冒烟页读取：渲染层错误与未实现通道计数。
#[tauri::command]
fn ipc_stats() -> Value {
    fallback::stats()
}

#[tauri::command]
fn runtime_info(app: AppHandle) -> Value {
    let bin = maker_wiring::locate_pi_rs(app.path().resource_dir().ok().as_deref());
    json!({
        "info": { "platform": node_platform(), "arch": node_arch(), "pid": std::process::id() },
        "agent": {
            "kind": "pi", "version": misc_settings::PI_RS_VERSION,
            "installed": bin.is_some(),
            "binary": bin.map(|p| p.to_string_lossy().into_owned()),
        },
    })
}

// payload 是 src/shared/ipc-codec.mjs 编码后的参数数组；返回同样编码的结果。
// 主窗口有浏览器子 webview 后就不再是 WebviewWindow，命令参数用 Webview，并且只接受主 webview。
#[tauri::command]
async fn ipc_invoke(webview: Webview, channel: String, payload: String) -> Result<Value, String> {
    browser::require_main(&webview)?;
    let app = webview.app_handle().clone();
    let name = channel.clone();
    let r = ipc_invoke_core(&app, Some(&webview), channel, payload).await;
    // 兜底通道（fallback.rs）已经自己记过；这里补上 local-db:/maker: 等原生通道，建任务失败时日志里才有错误码。
    if let Err(e) = &r {
        if !e.starts_with("[NOT_IMPLEMENTED]") && (name.starts_with("local-db:") || name.starts_with("maker:")) {
            eprintln!("[ipc] invoke {name} failed: {}", e.lines().next().unwrap_or("").chars().take(300).collect::<String>());
        }
    }
    r
}

/// invoke 路由内核。`webview` 为 None = 设备互联的远程调用（没有窗口，fullscreen 之类按未全屏处理）。
pub(crate) async fn ipc_invoke_core(app: &tauri::AppHandle, webview: Option<&Webview>, channel: String, payload: String) -> Result<Value, String> {
    let terminal_state = app.state::<TerminalState>();
    let update_state = app.state::<UpdateState>();
    let maker_state = app.state::<MakerState>();
    let settings_state = app.state::<SettingsState>();
    let secrets = app.state::<Arc<secret_store::SecretStore>>();
    // 本地库（会话 / 消息 / 页签 / 最近目录 / 别名，含 maker:auto-title）。messages:view 见下。
    if local_db::LocalDb::owns(&channel) {
        let Some(db) = native::LOCAL_DB.get().cloned() else { return Err("[PRECONDITION_FAILED] local database is not ready".into()) };
        let registry = maker_state.0.clone();
        return tauri::async_runtime::spawn_blocking(move || {
            let decoded = serde_json::from_str::<Value>(&payload).unwrap_or(Value::Null);
            let empty = Vec::new();
            let args = decoded.get("v").and_then(Value::as_array).unwrap_or(&empty);
            // 切换工作目录：回合进行中拒绝（旧进程还在往旧目录写）；成功后关掉旧 pi-rs，下次发送按新目录重建并续上历史。
            let switching = (channel == "local-db:sessions:update" && args.get(1).map(|p| p.get("workingDir").is_some()).unwrap_or(false))
                .then(|| args.first().and_then(Value::as_str).map(String::from))
                .flatten();
            if let (Some(sid), Some(reg)) = (&switching, &registry) {
                if reg.invoke("maker:session-in-turn", &[json!(sid)]).ok().and_then(|v| v.as_bool()).unwrap_or(false) {
                    return Err("[INVALID_PARAMS] 模型正在回答，等它结束后再切换工作目录".to_string());
                }
            }
            let out = db.invoke(&channel, args)?;
            if let (Some(sid), Some(reg)) = (&switching, &registry) {
                let _ = reg.invoke("maker:close-session", &[json!(sid)]);
            }
            Ok(out)
        })
        .await
        .map_err(|e| e.to_string())?
        .map(|v| Value::String(serde_json::to_string(&json!({ "v": v })).unwrap()));
    }
    // messages:view：分页读库与 projectHistoryView 投影都在 Rust。
    if channel == "local-db:messages:view" {
        let Some(db) = native::LOCAL_DB.get().cloned() else { return Err("[PRECONDITION_FAILED] local database is not ready".into()) };
        let running = maker_state.0.clone();
        return tauri::async_runtime::spawn_blocking(move || {
            let decoded = serde_json::from_str::<Value>(&payload).unwrap_or(Value::Null);
            let args = decoded.get("v").and_then(Value::as_array).cloned().unwrap_or_default();
            let sid = args.first().cloned().unwrap_or(Value::Null);
            let opts = args.get(1).filter(|v| v.is_object()).cloned().unwrap_or(json!({}));
            let mut page = db.view_page(&sid, &opts)?;
            let in_turn = sid.as_str().map(|s| running.as_ref().map(|r| r.invoke("maker:session-in-turn", &[json!(s)]).ok().and_then(|v| v.as_bool()).unwrap_or(false)).unwrap_or(false)).unwrap_or(false);
            page["streaming"] = json!(page["before"].is_null() && in_turn);
            let rows = page["page"].as_array().cloned().unwrap_or_default();
            let items = history_view::project::project_history_view(&rows, page["streaming"].as_bool() == Some(true), false);
            Ok::<Value, String>(json!({ "version": 1, "items": items, "hasMore": page["hasMore"], "nextCursor": page["nextCursor"] }))
        })
        .await
        .map_err(|e| e.to_string())?
        .map(|v| Value::String(serde_json::to_string(&json!({ "v": v })).unwrap()));
    }
    // maker:* 中归 Rust 的通道。pi-rs 缺失时直接报错：sidecar 里已没有会话实现，转过去只会得到更含糊的失败。
    if maker_registry::MakerRegistry::owns(&channel) {
        let Some(reg) = maker_state.0.clone() else {
            return Err("[PRECONDITION_FAILED] pi-rs not found: session host unavailable".into());
        };
        return tauri::async_runtime::spawn_blocking(move || {
            let decoded = serde_json::from_str::<Value>(&payload).unwrap_or(Value::Null);
            let empty = Vec::new();
            let args = decoded.get("v").and_then(Value::as_array).unwrap_or(&empty);
            reg.invoke(&channel, args)
        })
        .await
        .map_err(|e| e.to_string())?
        .map(|result| Value::String(serde_json::to_string(&json!({ "v": result })).unwrap()));
    }
    if channel == "get-fullscreen-state" {
        let full = webview.map(|w| w.window().is_fullscreen().unwrap_or(false)).unwrap_or(false);
        // 与 src/shared/ipc-codec.mjs 的编码一致：普通 JSON 值原样序列化。
        return Ok(Value::String(serde_json::to_string(&full).unwrap()));
    }
    // 桌面版导入（import_run.rs）：扫描 / 导入涉及磁盘和钥匙串授权框，放进 blocking 线程。
    if import_run::DesktopImport::owns(&channel) {
        let Some(imp) = native::desktop_import() else {
            return Err("[PRECONDITION_FAILED] desktop import not ready".into());
        };
        return tauri::async_runtime::spawn_blocking(move || {
            let decoded = serde_json::from_str::<Value>(&payload).unwrap_or(Value::Null);
            let empty = Vec::new();
            let args = decoded.get("v").and_then(Value::as_array).unwrap_or(&empty);
            imp.invoke(&channel, args)
        })
        .await
        .map_err(|e| e.to_string())?
        .map(|result| Value::String(serde_json::to_string(&json!({ "v": result })).unwrap()));
    }
    // 桌面外壳通道（desktop_shell.rs）。原生对话框会阻塞到用户操作，放进 blocking 线程。
    if desktop_shell::DesktopShell::owns(&channel) {
        let Some(shell) = native::shell() else {
            return Err("[PRECONDITION_FAILED] desktop shell not ready".into());
        };
        return tauri::async_runtime::spawn_blocking(move || {
            let decoded = serde_json::from_str::<Value>(&payload).unwrap_or(Value::Null);
            let empty = Vec::new();
            let args = decoded.get("v").and_then(Value::as_array).unwrap_or(&empty);
            shell.invoke(&channel, args).unwrap_or(Ok(Value::Null))
        })
        .await
        .map_err(|e| e.to_string())?
        .map(|result| Value::String(serde_json::to_string(&json!({ "v": result })).unwrap()));
    }
    // maker-core 的静态查询（agent_catalog.rs，原 sidecar 里的 PiAgent）。
    if matches!(channel.as_str(), "maker:list-available-agents" | "maker:get-capabilities" | "maker:agent:status" | "maker:list-agent-commands" | "maker:list-agent-skills") {
        let pi = maker_wiring::locate_pi_rs(app.path().resource_dir().ok().as_deref());
        return tauri::async_runtime::spawn_blocking(move || {
            let decoded = serde_json::from_str::<Value>(&payload).unwrap_or(Value::Null);
            let empty = Vec::new();
            let args = decoded.get("v").and_then(Value::as_array).unwrap_or(&empty);
            let a0 = args.first().cloned().unwrap_or(Value::Null);
            let result = match channel.as_str() {
                "maker:list-available-agents" => Ok(agent_catalog::list_available_agents()),
                "maker:get-capabilities" => {
                    // 只有 pi 需要模型清单；其余引擎直接返回固定值。
                    let models = if a0.as_str() == Some("pi") {
                        native::model_host_call("host.pi-models", &Value::Null).and_then(Result::ok).and_then(|v| v.as_array().cloned()).unwrap_or_default()
                    } else {
                        Vec::new()
                    };
                    agent_catalog::capabilities(&a0, models)
                }
                "maker:agent:status" => Ok(agent_catalog::agent_status(&a0, pi.as_deref())),
                "maker:list-agent-commands" => Ok(agent_catalog::list_agent_commands(&a0)),
                _ => Ok(agent_catalog::list_agent_skills(&a0, args.get(1).unwrap_or(&Value::Null), &dirs_home())),
            };
            result.map(|v| Value::String(serde_json::to_string(&json!({ "v": v })).unwrap()))
        })
        .await
        .map_err(|e| e.to_string())?;
    }
    // Cindy AI 云端模型 / 余额 / 自定义模型服务（原 sidecar model-access.mjs、model-config.mjs）。
    if model_access::manager::Manager::owns(&channel) || model_config::manager::Manager::owns(&channel) {
        let (Some(access), Some(models)) = (native::MODEL_ACCESS.get().cloned(), native::MODELS.get().cloned()) else { return Err("[PRECONDITION_FAILED] models are not ready".into()) };
        let decoded = serde_json::from_str::<Value>(&payload).unwrap_or(Value::Null);
        let empty = Vec::new();
        let args = decoded.get("v").and_then(Value::as_array).unwrap_or(&empty);
        let r = if model_access::manager::Manager::owns(&channel) { access.invoke(&channel, args).await } else { models.invoke(&channel, args).await };
        return r.unwrap_or(Ok(Value::Null)).map(|v| Value::String(serde_json::to_string(&json!({ "v": v })).unwrap()));
    }
    // 设备互联设置 / 状态（device_link/host.rs，原 sidecar device-link.mjs）。
    if device_link::host::DeviceLink::owns(&channel) {
        let Some(dl) = native::DEVICE_LINK.get().cloned() else { return Err("[PRECONDITION_FAILED] device link is not ready".into()) };
        let decoded = serde_json::from_str::<Value>(&payload).unwrap_or(Value::Null);
        let empty = Vec::new();
        let args = decoded.get("v").and_then(Value::as_array).unwrap_or(&empty);
        return match dl.invoke(&channel, args).await {
            Some(r) => r.map(|v| Value::String(serde_json::to_string(&json!({ "v": v })).unwrap())),
            None => Err(format!("[NOT_IMPLEMENTED] {channel} is not available in the Tauri build yet")),
        };
    }
    // 账号登录（原 sidecar auth.mjs）：会话与登录流程都在 Rust。
    if auth::manager::Manager::owns(&channel) {
        let Some(mgr) = native::AUTH.get().cloned() else { return Err("[PRECONDITION_FAILED] auth is not ready".into()) };
        let decoded = serde_json::from_str::<Value>(&payload).unwrap_or(Value::Null);
        let empty = Vec::new();
        let args = decoded.get("v").and_then(Value::as_array).unwrap_or(&empty);
        return match mgr.invoke(&channel, args).await {
            Some(r) => r.map(|v| Value::String(serde_json::to_string(&json!({ "v": v })).unwrap())),
            None => Err(format!("[NOT_IMPLEMENTED] {channel} is not available in the Tauri build yet")),
        };
    }
    // 本机密钥库的 renderer 通道（safe-storage-*）。可能等主密钥（钥匙串授权框），放 blocking 线程。
    if secret_store::SecretStore::owns(&channel) {
        let store = secrets.inner().clone();
        return tauri::async_runtime::spawn_blocking(move || {
            let decoded = serde_json::from_str::<Value>(&payload).unwrap_or(Value::Null);
            let empty = Vec::new();
            let args = decoded.get("v").and_then(Value::as_array).unwrap_or(&empty);
            store.invoke(&channel, args).unwrap_or_else(|| Err("[INTERNAL] secret channel not handled".into()))
        })
        .await
        .map_err(|e| e.to_string())?
        .map(|v| Value::String(serde_json::to_string(&json!({ "v": v })).unwrap()));
    }
    // 启动占位 / 布局 / 项目排序（boot_stubs.rs，原 sidecar boot-stubs.mjs）。
    if boot_stubs::BootStubs::owns(&channel) {
        let stubs = settings_state.5.clone();
        return tauri::async_runtime::spawn_blocking(move || {
            let decoded = serde_json::from_str::<Value>(&payload).unwrap_or(Value::Null);
            let empty = Vec::new();
            let args = decoded.get("v").and_then(Value::as_array).unwrap_or(&empty);
            stubs.invoke(&channel, args).unwrap_or_else(|| Err("[INTERNAL] boot channel not handled".into()))
        })
        .await
        .map_err(|e| e.to_string())?
        .map(|v| Value::String(serde_json::to_string(&json!({ "v": v })).unwrap()));
    }
    // 浏览器面板（browser_panel.rs，原 sidecar browser-panel.mjs）。tauri-browser:popup 可能等 report，放 blocking 线程。
    if browser_panel::BrowserPanel::owns(&channel) {
        let panel = settings_state.4.clone();
        return tauri::async_runtime::spawn_blocking(move || {
            let decoded = serde_json::from_str::<Value>(&payload).unwrap_or(Value::Null);
            let empty = Vec::new();
            let args = decoded.get("v").and_then(Value::as_array).unwrap_or(&empty);
            panel.invoke(&channel, args).unwrap_or_else(|| Err("[INTERNAL] browser channel not handled".into()))
        })
        .await
        .map_err(|e| e.to_string())?
        .map(|v| Value::String(serde_json::to_string(&json!({ "v": v })).unwrap()));
    }
    // 语音输入数据（voice_input.rs，原 sidecar settings/voice-input-data.mjs）。
    if voice_input::VoiceInput::owns_invoke(&channel) {
        let voice = settings_state.3.clone();
        return tauri::async_runtime::spawn_blocking(move || {
            let decoded = serde_json::from_str::<Value>(&payload).unwrap_or(Value::Null);
            let empty = Vec::new();
            let args = decoded.get("v").and_then(Value::as_array).unwrap_or(&empty);
            voice.invoke(&channel, args).unwrap_or_else(|| Err("[INTERNAL] voice channel not handled".into()))
        })
        .await
        .map_err(|e| e.to_string())?
        .map(|v| Value::String(serde_json::to_string(&json!({ "v": v })).unwrap()));
    }
    // 应用级设置（app_settings.rs，原 sidecar settings/app-settings.mjs）。
    if app_settings::AppSettings::owns_invoke(&channel) {
        let app_cfg = settings_state.2.clone();
        let hit = tauri::async_runtime::spawn_blocking(move || {
            let decoded = serde_json::from_str::<Value>(&payload).unwrap_or(Value::Null);
            let empty = Vec::new();
            let args = decoded.get("v").and_then(Value::as_array).unwrap_or(&empty);
            app_cfg.invoke(&channel, args)
        })
        .await
        .map_err(|e| e.to_string())?;
        if let Some(result) = hit {
            return result.map(|v| Value::String(serde_json::to_string(&json!({ "v": v })).unwrap()));
        }
        return Err("[INTERNAL] app settings channel not handled".into());
    }
    // maker 侧覆盖项设置（maker_settings.rs，原 sidecar settings/maker-settings.mjs）。
    if channel.starts_with("maker:") || channel.starts_with("local-db:bots:model-chain-settings-") {
        let (settings, misc) = (settings_state.0.clone(), settings_state.1.clone());
        let (ch, pl) = (channel.clone(), payload.clone());
        let hit = tauri::async_runtime::spawn_blocking(move || {
            let decoded = serde_json::from_str::<Value>(&pl).unwrap_or(Value::Null);
            let empty = Vec::new();
            let args = decoded.get("v").and_then(Value::as_array).unwrap_or(&empty);
            settings.invoke(&ch, args).or_else(|| misc.invoke(&ch, args))
        })
        .await
        .map_err(|e| e.to_string())?;
        if let Some(result) = hit {
            return result.map(|v| Value::String(serde_json::to_string(&json!({ "v": v })).unwrap()));
        }
    }
    // terminal:* 已迁到 Rust（原 node-pty sidecar 实现）；通道名 / 参数形状 / 错误码与旧版一致。
    if channel.starts_with("terminal:") {
        let mgr = terminal_state.0.clone();
        return tauri::async_runtime::spawn_blocking(move || {
            let decoded = serde_json::from_str::<Value>(&payload).unwrap_or(Value::Null);
            let empty = Vec::new();
            let args = decoded.get("v").and_then(Value::as_array).unwrap_or(&empty);
            mgr.invoke(&channel, args)
        })
        .await
        .map_err(|e| e.to_string())?
        .map(|result| {
            // 返回 encode(result) 字符串（单个值，不是数组）。
            Value::String(serde_json::to_string(&json!({ "v": result })).unwrap())
        });
    }
    // update-* 通道已迁到 Rust（原 sidecar update.mjs）。check_now 是网络阻塞调用，放 blocking。
    if let Some(result) = handle_update_invoke(&update_state, &channel, &payload).await {
        return result;
    }
    // 没有专属实现者的通道：降级通道 / 未实现错误（fallback.rs）。
    let decoded = serde_json::from_str::<Value>(&payload).unwrap_or(Value::Null);
    let empty = Vec::new();
    let args = decoded.get("v").and_then(Value::as_array).unwrap_or(&empty);
    fallback::invoke(&channel, args).map(|v| Value::String(serde_json::to_string(&json!({ "v": v })).unwrap()))
}

// 窗口控制类 channel 在 Rust 里原生处理（对齐 desktop bootstrap-electron.ts 的 ipcMain.on 实现）。
fn handle_window_send(window: &Window, channel: &str) -> bool {
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
    webview: Webview,
    update_state: State<'_, UpdateState>,
    channel: String,
    payload: String,
) -> Result<(), String> {
    browser::require_main(&webview)?;
    // 快捷键录制期间 desktop 会暂停菜单 accelerator；Tauri 版没有原生菜单快捷键，直接吞掉。
    if app_settings::AppSettings::owns_send(&channel) {
        return Ok(());
    }
    if handle_window_send(&webview.window(), &channel) {
        return Ok(());
    }
    if let Some(shell) = native::shell() {
        if shell.send(&channel) {
            return Ok(());
        }
    }
    // update-* send 通道已迁到 Rust。
    match channel.as_str() {
        // 用户在 UpdateBanner 点「立即重启」；不 await（JS 亦 void executeRelaunch）。
        "update-relaunch" => {
            update_state.0.relaunch_send();
            return Ok(());
        }
        // Tauri 版不需要主题过渡，直接丢弃。
        "update-set-relaunch-theme" => return Ok(()),
        _ => {}
    }
    let decoded = serde_json::from_str::<Value>(&payload).unwrap_or(Value::Null);
    let empty = Vec::new();
    fallback::send(&channel, decoded.get("v").and_then(Value::as_array).unwrap_or(&empty));
    Ok(())
}

// update-* invoke 通道分发（update_machine.rs）。None = 不是 update 通道。
async fn handle_update_invoke(
    state: &State<'_, UpdateState>,
    channel: &str,
    payload: &str,
) -> Option<Result<Value, String>> {
    let m = state.0.clone();
    let result: Value = match channel {
        "update-get-status" => json!(m.get_status()),
        "update-check-now" => tauri::async_runtime::spawn_blocking(move || m.check_now()).await.unwrap_or_else(|e| json!({ "result": format!("join error: {e}") })),
        "update-relaunch-auto" => json!(m.relaunch_auto()),
        "update-relaunch:blocking-activity" => json!(m.blocking_activity()),
        "update-relaunch:blocking-summary" => m.blocking_summary(),
        "update-move-to-applications" => json!({ "moved": false }),
        "release-notes:fetch-index" => Value::Null,
        "release-notes:fetch" => {
            let decoded = serde_json::from_str::<Value>(payload).unwrap_or(Value::Null);
            let version = decoded.get("v").and_then(Value::as_array).and_then(|a| a.first()).and_then(Value::as_str).unwrap_or("");
            m.release_notes(version)
        }
        _ => return None,
    };
    // 返回 encode(result) 字符串（单个值）。
    Some(Ok(Value::String(serde_json::to_string(&json!({ "v": result })).unwrap())))
}

#[tauri::command]
fn smoke_finish(app: AppHandle, ok: bool, detail: String) {
    write_smoke(ok, &detail);
    // 冒烟专用的钥匙串主密钥：会话冒烟 run 阶段成功时留给 verify 用，其余情况都删掉。
    if (session_smoke_phase().as_deref() != Some("run") || !ok) && !no_keychain_smoke() {
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
    if app_settings::AppSettings::owns_sync(&channel) {
        let decoded = serde_json::from_str::<Value>(&payload).unwrap_or(Value::Null);
        let empty = Vec::new();
        let args = decoded.get("v").and_then(Value::as_array).unwrap_or(&empty);
        let cfg = app.state::<SettingsState>().2.clone();
        return match cfg.sync(&channel, args) {
            Some(v) => respond(StatusCode::OK, serde_json::to_string(&json!({ "v": v })).unwrap()),
            None => respond(StatusCode::INTERNAL_SERVER_ERROR, "app settings sync channel not handled".into()),
        };
    }
    if model_config::manager::Manager::owns_sync(&channel) {
        let v = native::MODELS.get().and_then(|m| m.invoke_sync(&channel));
        return match v {
            Some(v) => respond(StatusCode::OK, serde_json::to_string(&json!({ "v": v })).unwrap()),
            None => respond(StatusCode::INTERNAL_SERVER_ERROR, "model sync channel not handled".into()),
        };
    }
    if auth::manager::Manager::owns_sync(&channel) {
        let v = native::AUTH.get().and_then(|m| m.invoke_sync(&channel));
        return match v {
            Some(v) => respond(StatusCode::OK, serde_json::to_string(&json!({ "v": v })).unwrap()),
            None => respond(StatusCode::INTERNAL_SERVER_ERROR, "auth sync channel not handled".into()),
        };
    }
    if boot_stubs::BootStubs::owns_sync(&channel) {
        let stubs = app.state::<SettingsState>().5.clone();
        return match stubs.sync(&channel) {
            Some(v) => respond(StatusCode::OK, serde_json::to_string(&json!({ "v": v })).unwrap()),
            None => respond(StatusCode::INTERNAL_SERVER_ERROR, "boot sync channel not handled".into()),
        };
    }
    if voice_input::VoiceInput::owns_sync(&channel) {
        let decoded = serde_json::from_str::<Value>(&payload).unwrap_or(Value::Null);
        let empty = Vec::new();
        let args = decoded.get("v").and_then(Value::as_array).unwrap_or(&empty);
        let voice = app.state::<SettingsState>().3.clone();
        return match voice.sync(&channel, args) {
            Some(Ok(v)) => respond(StatusCode::OK, serde_json::to_string(&json!({ "v": v })).unwrap()),
            Some(Err(e)) => respond(StatusCode::INTERNAL_SERVER_ERROR, e),
            None => respond(StatusCode::INTERNAL_SERVER_ERROR, "voice sync channel not handled".into()),
        };
    }
    // 没有专属实现者的同步通道：首屏默认值，未知通道返回 undefined（fallback.rs）。
    let v = fallback::sync(&channel).unwrap_or_else(fallback::undefined);
    respond(StatusCode::OK, serde_json::to_string(&json!({ "v": v })).unwrap())
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
    // 冒烟时窗口常被别的窗口挡住，WebKit 会挂起被遮挡页面的定时器，回合等不到结束（macOS 14+ 生效）。
    let builder = if smoke_out().is_some() {
        builder.background_throttling(tauri::utils::config::BackgroundThrottlingPolicy::Disabled)
    } else {
        builder
    };
    // 与 Electron 版一致：macOS 隐藏标题栏保留红绿灯，Windows 无边框（renderer 自绘窗口按钮）。
    #[cfg(target_os = "macos")]
    let builder = builder
        .title_bar_style(tauri::TitleBarStyle::Overlay)
        .hidden_title(true)
        .traffic_light_position(tauri::LogicalPosition::new(12.0, 16.0));
    #[cfg(not(target_os = "macos"))]
    let builder = builder.decorations(false);
    // 之后会挂浏览器子 webview，窗口相关操作都按 Window 做。
    let window = builder.build()?.as_ref().window();
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

// 自动更新计时：冒烟可用环境变量缩短（对齐 sidecar update.mjs 的 timing()，只认 CINDY_TAURI_SMOKE=1）。
fn update_timing() -> update_machine::Timing {
    let mut t = update_machine::DEFAULT_TIMING;
    if std::env::var("CINDY_TAURI_SMOKE").as_deref() == Ok("1") {
        let env = |name: &str, slot: &mut u64| {
            if let Ok(v) = std::env::var(name) {
                if let Ok(parsed) = v.parse::<u64>() {
                    *slot = parsed;
                }
            }
        };
        env("CINDY_TAURI_UPDATE_FIRST_CHECK_MS", &mut t.first_check_ms);
        env("CINDY_TAURI_UPDATE_POLL_MS", &mut t.poll_ms);
        env("CINDY_TAURI_UPDATE_AUTO_RELAUNCH_MS", &mut t.auto_relaunch_poll_ms);
        if let Ok(v) = std::env::var("CINDY_TAURI_UPDATE_IDLE_SECONDS") {
            if let Ok(parsed) = v.parse::<f64>() {
                t.idle_threshold_seconds = parsed;
            }
        }
    }
    t
}

#[allow(dead_code)]
struct InstanceLock(std::fs::File);

fn main() {
    let app = tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        // 不注入 opener 的 JS 点击拦截（需要给 webview 开 opener 权限）；_blank 链接走 on_new_window。
        .plugin(tauri_plugin_opener::Builder::default().open_js_links_on_click(false).build())
        .plugin(tauri_plugin_notification::init())
        // 自动更新：配置在 tauri.conf.json plugins.updater（公钥、GitHub Releases 的 latest.json）。
        .plugin(tauri_plugin_updater::Builder::new().build())
        .register_asynchronous_uri_scheme_protocol("cindy-ipc", |ctx, request, responder| {
            // 自定义协议算本地来源，只给主 webview 用；浏览器子 webview 的请求直接拒绝。
            if ctx.webview_label() != "main" {
                let denied = Response::builder().status(StatusCode::FORBIDDEN).body(Vec::new()).unwrap();
                responder.respond(denied);
                return;
            }
            let app = ctx.app_handle().clone();
            let path = request.uri().path().to_string();
            let body = request.body().clone();
            thread::spawn(move || responder.respond(handle_sync(&app, &path, body)));
        })
        .setup(move |app| {
            // CINDY_TAURI_DATA_DIR 覆盖数据目录（冒烟和开发用临时目录）。
            let data_dir = strip_verbatim(match std::env::var_os("CINDY_TAURI_DATA_DIR") {
                Some(dir) if !dir.is_empty() => PathBuf::from(dir),
                _ => app.path().app_data_dir().unwrap_or_else(|_| std::env::temp_dir().join("cindy-tauri")),
            });
            let _ = std::fs::create_dir_all(&data_dir);
            applog::init(&data_dir.join("logs"));
            // 同一数据目录只允许一个实例：两个实例会互相删对方会话的运行目录、各自拉起 pi 进程。
            // 锁随进程退出自动释放；文件句柄放进 managed state 保持到退出。
            match std::fs::OpenOptions::new().create(true).write(true).open(data_dir.join("app.lock")) {
                Ok(f) => match f.try_lock() {
                    Ok(()) => {
                        app.manage(InstanceLock(f));
                    }
                    Err(std::fs::TryLockError::WouldBlock) => {
                        eprintln!("[app] another instance already uses {}; exiting", data_dir.display());
                        std::process::exit(0);
                    }
                    Err(e) => eprintln!("[app] instance lock unavailable ({e}); continuing without it"),
                },
                Err(e) => eprintln!("[app] cannot open instance lock file ({e}); continuing without it"),
            }
            auth::env::init_server_base(&data_dir);
            fallback::init(&app.package_info().version.to_string());
            updater::init(&data_dir);
            let _ = browser::PARTITION_ROOT.set(data_dir.join("webview-partitions"));
            // 密钥库与应用会话（账号模式 / owner 戳）。
            {
                let secret_store = Arc::new(secret_store::SecretStore::new(&data_dir));
                app.manage(secret_store);
                let owner = Arc::new(owner::Owner::new(&data_dir));
                app.manage(owner);
            }
            // 本地库（会话 / 消息 / 页签）。
            match local_db::LocalDb::open(&data_dir, "local-v1", Arc::new(local_db::SystemEnv), Arc::new(maker_wiring::AppPush(app.handle().clone()))) {
                Ok(db) => {
                    let db = Arc::new(db);
                    let _ = native::LOCAL_DB.set(db.clone());
                    app.manage(db);
                }
                Err(e) => eprintln!("[local-db] open failed: {e}"),
            }
            // 通知点击的回调要在第一条通知前注册，否则点击不会上报。
            native::register_notification_click(app.handle());
            let _ = DATA_DIR.set(data_dir.clone());
            // 账号登录：会话与登录流程都在 Rust。
            {
                let tokio_handle = match tauri::async_runtime::handle() {
                    tauri::async_runtime::RuntimeHandle::Tokio(h) => h.clone(),
                };
                let env = Arc::new(auth::env::AppEnv {
                    owner: app.state::<Arc<owner::Owner>>().inner().clone(),
                    secrets: app.state::<Arc<secret_store::SecretStore>>().inner().clone(),
                    ports: Arc::new(native::AuthPorts(app.handle().clone())),
                    device_id: boot_stubs::device_id(&data_dir),
                    auth_base: None,
                });
                let mgr = auth::manager::Manager::new(env, auth::backoff::Backoff::system(), tokio_handle);
                let _ = native::AUTH.set(mgr.clone());
                // 设备互联被控端（原 sidecar device-link.mjs）：登录后连中继，登录态直接取自 Rust 账号会话。
                {
                    let ports = Arc::new(native::DeviceLinkPorts { app: app.handle().clone(), name: device_link::host::read_computer_name() });
                    let dl = device_link::host::DeviceLink::new(data_dir.join("settings"), ports);
                    let dl2 = dl.clone();
                    mgr.on_change(move |_| dl2.sync_with_auth());
                    let _ = native::DEVICE_LINK.set(dl.clone());
                    dl.sync_with_auth();
                }
                let emit_app = app.handle().clone();
                let emit: model_access::manager::Emit = Arc::new(move |ch, v| native::emit_all(&emit_app, ch, v));
                let secrets = app.state::<Arc<secret_store::SecretStore>>().inner().clone();
                let owner = app.state::<Arc<owner::Owner>>().inner().clone();
                let tokio_handle = match tauri::async_runtime::handle() {
                    tauri::async_runtime::RuntimeHandle::Tokio(h) => h.clone(),
                };
                let access = model_access::manager::Manager::new(
                    &data_dir,
                    auth::env::model_access_base_url(),
                    mgr,
                    secrets.clone(),
                    owner.clone(),
                    emit.clone(),
                    Arc::new(|| native::shell().map(|s| s.current_locale()).unwrap_or_else(|| "en".into())),
                    tokio_handle,
                );
                access.start();
                let smoke = model_config::manager::smoke_provider(
                    std::env::var("CINDY_TAURI_SMOKE_MODEL_BASE_URL").ok().as_deref(),
                    std::env::var("CINDY_TAURI_SMOKE_MODEL_ID").ok().as_deref(),
                );
                let models = model_config::manager::Manager::new(&data_dir, secrets, owner, access.clone(), emit, smoke);
                models.start();
                let _ = native::MODEL_HOST.set(model_config::host::ModelHost::new(access.clone(), models.clone()));
                let _ = native::MODEL_ACCESS.set(access);
                let _ = native::MODELS.set(models);
            }
            // 钥匙串可能弹系统授权框，放到后台线程；解锁前带 key 的 provider 显示为未连接。
            {
                let store = app.state::<Arc<secret_store::SecretStore>>().inner().clone();
                let smoke = smoke_out().is_some();
                let dir = data_dir.clone();
                thread::spawn(move || {
                    // 更新冒烟：新旧两个版本是不同的 ad-hoc 签名，读同一钥匙串条目会弹系统授权框，冒烟里跳过钥匙串。
                    let reply = match if no_keychain_smoke() {
                        Err("smoke without keychain".to_string())
                    } else {
                        secrets::load_or_create(smoke, &dir)
                    } {
                        Ok(key) => store.unlock(&key).map(|_| ()),
                        Err(reason) => {
                            eprintln!("[secrets] keychain unavailable: {reason}");
                            store.mark_unavailable(&reason);
                            Ok(())
                        }
                    };
                    if let Err(e) = reply {
                        eprintln!("[secrets] unlocking secret store failed: {e}");
                    }
                });
            }
            // 应用级设置（外观 / 快捷键 / 更新通道 / 外设占位；原 sidecar app-settings.mjs）。
            let app_settings = Arc::new(app_settings::AppSettings::new(&data_dir.join("settings"), Arc::new(maker_wiring::TauriEmitter(app.handle().clone()))));
            // 自动更新状态机：机制在 updater.rs，忙闲看会话注册表。
            {
                let mode = updater::mode();
                let version = app.package_info().version.to_string();
                let backend = Arc::new(updater::NativeUpdateBackend(app.handle().clone()));
                let probe = Arc::new(UpdateProbe(app.handle().clone(), app_settings.clone()));
                let push = Arc::new(IpcPush(app.handle().clone()));
                let machine = update_machine::UpdateMachine::new(mode, &version, updater::smoke(), update_timing(), push, backend, probe);
                machine.start_timers();
                app.manage(UpdateState(machine));
            }
            // Rust 会话宿主：pi-rs 找不到时应用照常启动（设置、登录等不受影响），只是会话通道报错。
            let maker_reg = match maker_wiring::locate_pi_rs(app.path().resource_dir().ok().as_deref()) {
                Some(bin) => native::LOCAL_DB.get().cloned().map(|db| maker_wiring::build(app.handle().clone(), data_dir.clone(), bin, db)),
                None => {
                    eprintln!("[maker-host] 找不到 pi-rs：会话不可用");
                    None
                }
            };
            app.manage(MakerState(maker_reg));
            let pi_for_settings = maker_wiring::locate_pi_rs(app.path().resource_dir().ok().as_deref());
            app.manage(SettingsState(
                Arc::new(maker_settings::MakerSettings::new(&data_dir.join("settings"))),
                Arc::new(misc_settings::MiscSettings::new(&data_dir.join("settings"), pi_for_settings).with_owner(app.state::<Arc<owner::Owner>>().inner().clone())),
                app_settings.clone(),
                Arc::new(voice_input::VoiceInput::new(&data_dir.join("settings"), Arc::new(maker_wiring::TauriEmitter(app.handle().clone())))),
                Arc::new(browser_panel::BrowserPanel::new(
                    &data_dir.join("settings"),
                    Arc::new(maker_wiring::TauriEmitter(app.handle().clone())),
                    Arc::new(AppUrlOpener(app.handle().clone())),
                )),
                Arc::new(boot_stubs::BootStubs::new(
                    &data_dir,
                    maker_wiring::locate_pi_rs(app.path().resource_dir().ok().as_deref()),
                    app.state::<Arc<owner::Owner>>().inner().clone(),
                    Arc::new(maker_wiring::TauriEmitter(app.handle().clone())),
                )),
            ));
            // 桌面外壳：通知 / 角标 / 对话框 / 窗口关闭行为（原 sidecar desktop-shell.mjs）。
            let shell = desktop_shell::DesktopShell::new(Arc::new(native::AppPorts(app.handle().clone())), &data_dir);
            let _ = native::SHELL.set(shell);
            // 桌面版导入：首次启动问一次（冒烟模式不问）。
            let importer = import_run::DesktopImport::new(data_dir.clone(), Arc::new(native::ImportAppPorts(app.handle().clone())));
            let _ = native::IMPORT.set(importer.clone());
            if smoke_out().is_none() {
                importer.spawn_prompt();
            }
            // 终端 PTY：settings 目录放 terminal-prefs.json。
            app.manage(TerminalState(Arc::new(terminal::TerminalManager::new(
                data_dir.join("settings"),
                Arc::new(AppTerminalPush(app.handle().clone())),
            ))));
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
        .invoke_handler(tauri::generate_handler![
            smoke_info,
            runtime_info,
            ipc_stats,
            smoke_finish,
            ipc_invoke,
            ipc_send,
            browser::browser_create,
            browser::browser_op,
            browser::browser_reset
        ])
        .build({
            let mut ctx = tauri::generate_context!();
            updater::apply_smoke_config(ctx.config_mut());
            ctx
        })
        .expect("error while building cindy-tauri");
    app.run(|app, event| match event {
        // macOS 点 Dock 图标：恢复隐藏的主窗口。
        #[cfg(target_os = "macos")]
        tauri::RunEvent::Reopen { .. } => native::focus_main_window(app),
        // ⌘Q / 菜单退出：不再拦截主窗口关闭。
        tauri::RunEvent::ExitRequested { .. } => {
            native::mark_quitting();
            // 终端会话随进程退出，主动 kill 防止 shell 成为孤儿进程。
            if let Some(t) = app.try_state::<TerminalState>() {
                t.0.dispose_all();
            }
            // pi-rs 子进程：先关 stdin 让其自行收尾，再清运行目录（key 文件）。
            if let Some(reg) = app.try_state::<MakerState>().and_then(|m| m.0.clone()) {
                reg.shutdown_all();
            }
        }
        _ => {
            let _ = app;
        }
    });
}
