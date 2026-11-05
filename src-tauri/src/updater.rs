// P4 自动更新的原生部分（sidecar update.mjs 经反向调用 update.* 驱动）：
// - update.check：按 tauri.conf.json 的 endpoints 读 latest.json，只认比当前版本高的发布；
// - update.download：下载并用内置公钥校验签名（含签名里绑定的版本号），落到 <dataDir>/updates/；
// - update.install：再次校验后安装。Windows 交给 NSIS（passive，装完由安装程序拉起新版本），
//   macOS 原地替换 .app 后重启；两边都先让 sidecar 收尾退出（关库、杀终端）；
// - update.idleSeconds：系统级无操作时长，给「空闲时自动重启」用。
// 冒烟（CINDY_TAURI_SMOKE=1）可用 CINDY_TAURI_SMOKE_UPDATE_URL 指向本机 http 的 latest.json，签名校验不变。
use crate::native;
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};
use tauri::{AppHandle, Runtime};
use tauri_plugin_updater::{Update, UpdaterExt};

// 最近一次 check 发现的更新；download / install 只接受同一个版本。
static PENDING: Mutex<Option<Update>> = Mutex::new(None);
// 已下载并校验过的安装包和它对应的 Update。和 PENDING 分开放：ready 之后再查到更新的版本
// 但下载失败时，sidecar 回到旧的 ready，安装仍要用旧版本的这一份。
static STAGED: Mutex<Option<(Update, PathBuf)>> = Mutex::new(None);
static UPDATES_DIR: OnceLock<PathBuf> = OnceLock::new();
// Windows：安装前已经让 sidecar 退出（之后安装失败只能重启回旧版本）。
static SIDECAR_STOPPED: AtomicBool = AtomicBool::new(false);
const SIDECAR_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);
const PROGRESS_INTERVAL: Duration = Duration::from_millis(200);

fn is_smoke() -> bool {
    std::env::var("CINDY_TAURI_SMOKE").as_deref() == Ok("1") && std::env::var_os("CINDY_TAURI_SMOKE_OUT").is_some()
}

// 冒烟用的更新地址：只接受本机 http。
pub fn smoke_endpoint() -> Option<String> {
    if !is_smoke() {
        return None;
    }
    let raw = std::env::var("CINDY_TAURI_SMOKE_UPDATE_URL").ok()?;
    let url = tauri::Url::parse(&raw).ok()?;
    (url.scheme() == "http" && url.host_str() == Some("127.0.0.1")).then_some(raw)
}

// 传给 sidecar 的 CINDY_TAURI_UPDATES：正式包自动轮询，冒烟只在配了本机地址时手动检查，开发包关闭。
pub fn mode() -> &'static str {
    if is_smoke() {
        if smoke_endpoint().is_some() { "manual" } else { "off" }
    } else if cfg!(debug_assertions) {
        "off"
    } else {
        "auto"
    }
}

// 冒烟改写插件配置：只换更新地址并允许本机 http，公钥和签名版本校验保持不变。
pub fn apply_smoke_config(config: &mut tauri::Config) {
    let Some(url) = smoke_endpoint() else { return };
    if let Some(Value::Object(cfg)) = config.plugins.0.get_mut("updater") {
        cfg.insert("endpoints".into(), json!([url]));
        cfg.insert("dangerousInsecureTransportProtocol".into(), Value::Bool(true));
    }
}

// 启动时清掉上次没装的安装包（重新检查会再下载）。
pub fn init(data_dir: &std::path::Path) {
    let dir = data_dir.join("updates");
    let _ = std::fs::remove_dir_all(&dir);
    let _ = UPDATES_DIR.set(dir);
}

fn err(e: impl std::fmt::Display) -> String {
    e.to_string()
}

fn version_param(params: &Value) -> Result<String, String> {
    params.get("version").and_then(Value::as_str).map(str::to_owned).ok_or_else(|| "version required".to_string())
}

fn check<R: Runtime>(app: &AppHandle<R>) -> Result<Value, String> {
    let handle = app.clone();
    let updater = app
        .updater_builder()
        // 不设整体 timeout（会限制下载总时长），只限制连接和读取停顿。
        .configure_client(|c| c.connect_timeout(Duration::from_secs(15)).read_timeout(Duration::from_secs(60)))
        // Windows：启动安装程序前让 sidecar 退出，否则 cindy-node.exe 被占用无法覆盖。
        .on_before_exit(move || {
            if let Some(sc) = native::sidecar_handle() {
                sc.shutdown(SIDECAR_SHUTDOWN_TIMEOUT);
            }
            SIDECAR_STOPPED.store(true, Ordering::SeqCst);
            handle.cleanup_before_exit();
        })
        // 冒烟：Windows 安装程序以管理员身份拉起新版本时会丢掉冒烟环境变量，改由脚本等安装完成后自己启动。
        .restart_after_install(!is_smoke())
        .build()
        .map_err(err)?;
    let found = tauri::async_runtime::block_on(updater.check()).map_err(err)?;
    let mut pending = PENDING.lock().map_err(err)?;
    Ok(match found {
        Some(update) => {
            let reply = json!({
                "available": true,
                "version": update.version,
                "currentVersion": update.current_version,
                "body": update.body,
                "date": update.date.map(|d| d.to_string()),
            });
            *pending = Some(update);
            reply
        }
        None => {
            *pending = None;
            json!({ "available": false })
        }
    })
}

fn pending_for(version: &str) -> Result<Update, String> {
    let pending = PENDING.lock().map_err(err)?;
    match pending.as_ref() {
        Some(u) if u.version == version => Ok(u.clone()),
        _ => Err(format!("[PRECONDITION_FAILED] no checked update for version {version}")),
    }
}

fn download(params: &Value) -> Result<Value, String> {
    let version = version_param(params)?;
    let update = pending_for(&version)?;
    let dir = UPDATES_DIR.get().ok_or("updates dir not set")?.clone();
    std::fs::create_dir_all(&dir).map_err(err)?;
    let sc = native::sidecar_handle();
    let mut received: u64 = 0;
    let mut last = Instant::now() - PROGRESS_INTERVAL;
    // download 内部已按公钥和签名里的版本号校验，校验失败直接报错。
    let bytes = tauri::async_runtime::block_on(update.download(
        |chunk, total| {
            received += chunk as u64;
            if last.elapsed() >= PROGRESS_INTERVAL {
                last = Instant::now();
                if let Some(sc) = &sc {
                    sc.notify("native.updateProgress", json!({ "received": received, "total": total }));
                }
            }
        },
        || {},
    ))
    .map_err(err)?;
    if let Some(sc) = &sc {
        let n = bytes.len() as u64;
        sc.notify("native.updateProgress", json!({ "received": n, "total": n }));
    }
    let file = dir.join(format!("update-{version}.pkg"));
    let tmp = dir.join(format!("update-{version}.pkg.part"));
    std::fs::write(&tmp, &bytes).map_err(err)?;
    std::fs::rename(&tmp, &file).map_err(err)?;
    let mut staged = STAGED.lock().map_err(err)?;
    if let Some((_, old)) = staged.replace((update, file.clone())) {
        if old != file {
            let _ = std::fs::remove_file(old);
        }
    }
    Ok(json!({ "version": version, "size": bytes.len() }))
}

fn install<R: Runtime>(app: &AppHandle<R>, params: &Value) -> Result<Value, String> {
    let version = version_param(params)?;
    let (update, file) = match STAGED.lock().map_err(err)?.as_ref() {
        Some((u, f)) if u.version == version => (u.clone(), f.clone()),
        _ => return Err(format!("[PRECONDITION_FAILED] update {version} is not downloaded")),
    };
    let bytes = std::fs::read(&file).map_err(err)?;
    // install 会重新校验签名；Windows 成功时在里面启动安装程序并退出进程，不会返回。
    if let Err(e) = update.install(&bytes) {
        eprintln!("[updater] install {version} failed: {e}");
        if SIDECAR_STOPPED.load(Ordering::SeqCst) {
            // sidecar 已退出，界面无法继续用，重启回当前版本。
            app.restart();
        }
        return Err(err(e));
    }
    // macOS：新 .app 已就位。先让 sidecar 关库退出，再重启进新版本。
    if let Some(sc) = native::sidecar_handle() {
        sc.shutdown(SIDECAR_SHUTDOWN_TIMEOUT);
    }
    relaunch(app)
}

// 冒烟直接拉起新的可执行文件并继承环境变量（经 LaunchServices 重启会丢掉冒烟用的环境变量）。
fn relaunch<R: Runtime>(app: &AppHandle<R>) -> ! {
    native::mark_quitting();
    if is_smoke() {
        if let Ok(exe) = std::env::current_exe() {
            let mut cmd = std::process::Command::new(exe);
            cmd.stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null());
            #[cfg(unix)]
            {
                use std::os::unix::process::CommandExt;
                cmd.process_group(0);
            }
            if let Err(e) = cmd.spawn() {
                eprintln!("[updater] relaunch failed: {e}");
            }
        }
        app.exit(0);
        std::process::exit(0);
    }
    app.restart()
}

#[cfg(target_os = "macos")]
fn idle_seconds() -> Result<Value, String> {
    #[link(name = "CoreGraphics", kind = "framework")]
    extern "C" {
        fn CGEventSourceSecondsSinceLastEventType(state: i32, event_type: u32) -> f64;
    }
    // kCGEventSourceStateCombinedSessionState = 0，kCGAnyInputEventType = ~0
    let secs = unsafe { CGEventSourceSecondsSinceLastEventType(0, u32::MAX) };
    Ok(json!(secs.max(0.0)))
}

#[cfg(windows)]
fn idle_seconds() -> Result<Value, String> {
    use windows::Win32::System::SystemInformation::GetTickCount;
    use windows::Win32::UI::Input::KeyboardAndMouse::{GetLastInputInfo, LASTINPUTINFO};
    let mut info = LASTINPUTINFO { cbSize: std::mem::size_of::<LASTINPUTINFO>() as u32, dwTime: 0 };
    if !unsafe { GetLastInputInfo(&mut info) }.as_bool() {
        return Err("GetLastInputInfo failed".into());
    }
    let ms = unsafe { GetTickCount() }.wrapping_sub(info.dwTime);
    Ok(json!(ms as f64 / 1000.0))
}

#[cfg(not(any(target_os = "macos", windows)))]
fn idle_seconds() -> Result<Value, String> {
    Err("[UNSUPPORTED_CAPABILITY] idle time".into())
}

pub fn run<R: Runtime>(app: &AppHandle<R>, method: &str, params: &Value) -> Result<Value, String> {
    if mode() == "off" {
        return Err("[UNSUPPORTED_CAPABILITY] updates are disabled in this build".into());
    }
    match method {
        "update.check" => check(app),
        "update.download" => download(params),
        "update.install" => install(app, params),
        "update.idleSeconds" => idle_seconds(),
        other => Err(format!("[NOT_IMPLEMENTED] native method {other}")),
    }
}
