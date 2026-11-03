// Cindy Tauri P1 空壳入口。
// - 启动时拉起 Node sidecar；
// - 前端通过 sidecar_call 与 sidecar 通信；
// - 设置 CINDY_TAURI_SMOKE=1 时，前端完成调用后写入 CINDY_TAURI_SMOKE_OUT 并退出，供 CI 冒烟使用。
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod sidecar;

use serde_json::Value;
use std::path::PathBuf;
use std::sync::Arc;
use std::thread;
use std::time::Duration;
use tauri::{AppHandle, Manager, State};

type SidecarHandle = Result<Arc<sidecar::Sidecar>, String>;

struct SidecarState(SidecarHandle);

const SMOKE_TIMEOUT: Duration = Duration::from_secs(60);

fn smoke_out() -> Option<String> {
    if std::env::var("CINDY_TAURI_SMOKE").as_deref() != Ok("1") {
        return None;
    }
    std::env::var("CINDY_TAURI_SMOKE_OUT").ok()
}

fn write_smoke(ok: bool, detail: &str) {
    if let Some(out) = smoke_out() {
        let body = serde_json::json!({
            "ok": ok,
            "detail": detail,
            "os": std::env::consts::OS,
            "arch": std::env::consts::ARCH,
        });
        let _ = std::fs::write(out, body.to_string());
    }
}

// 开发模式用系统 node（或 CINDY_NODE 指定），发布模式用随包的 cindy-node。
fn runtime_paths(app: &AppHandle) -> (PathBuf, PathBuf) {
    if cfg!(debug_assertions) {
        let node = std::env::var("CINDY_NODE")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from("node"));
        let script = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../sidecar/dist/sidecar.cjs");
        (node, script)
    } else {
        let exe_dir = std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(|d| d.to_path_buf()))
            .unwrap_or_default();
        let name = if cfg!(windows) { "cindy-node.exe" } else { "cindy-node" };
        let script = app
            .path()
            .resource_dir()
            .map(|d| d.join("sidecar/sidecar.cjs"))
            .unwrap_or_default();
        (exe_dir.join(name), script)
    }
}

#[tauri::command]
fn smoke_info() -> Option<String> {
    smoke_out()
}

#[tauri::command]
async fn sidecar_call(state: State<'_, SidecarState>, method: String, params: Value) -> Result<Value, String> {
    let sc = state.0.clone()?;
    tauri::async_runtime::spawn_blocking(move || sc.call(&method, params))
        .await
        .map_err(|e| e.to_string())?
}

#[tauri::command]
fn smoke_finish(app: AppHandle, ok: bool, detail: String) {
    write_smoke(ok, &detail);
    app.exit(if ok { 0 } else { 1 });
}

fn main() {
    tauri::Builder::default()
        .setup(|app| {
            let (node, script) = runtime_paths(app.handle());
            let handle = sidecar::Sidecar::spawn(&node, &script)
                .map(Arc::new)
                .map_err(|e| format!("spawn sidecar failed ({}): {e}", node.display()));
            if let Err(e) = &handle {
                eprintln!("{e}");
            }
            app.manage(SidecarState(handle));

            if smoke_out().is_some() {
                thread::spawn(|| {
                    thread::sleep(SMOKE_TIMEOUT);
                    write_smoke(false, "smoke timeout");
                    std::process::exit(1);
                });
            }
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![smoke_info, sidecar_call, smoke_finish])
        .run(tauri::generate_context!())
        .expect("error while running cindy-tauri");
}
