// 浏览器面板（P3）：renderer 里的 <webview> 由 shim/webview.mjs 模拟，真正的网页跑在主窗口的
// Tauri 子 webview 里（label = "wv-<id>"，id 由 shim 分配）。这里只做原生动作：
//   browser_create / browser_op / browser_reset 三个命令，只接受主 webview（label "main"）调用；
//   子 webview 的页面事件（开始 / 完成加载、标题、弹窗）以 Tauri 事件 "cindy:webview" 发给主 webview。
// 子 webview 只能打开 http(s) 外部页面和本地 file://，不能导航到 app 自身或任何 *.localhost 子域（Windows 上的自定义协议），
// 这样子页面永远不是 Tauri 眼里的本地来源，ACL 拒绝它调用任何命令（capabilities 也只给 main webview）。
use serde::Deserialize;
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::mpsc;
use std::sync::OnceLock;
use std::time::Duration;
use tauri::webview::{NewWindowResponse, PageLoadEvent, WebviewBuilder};
use tauri::{AppHandle, Emitter, EventTarget, LogicalPosition, LogicalSize, Manager, Rect, Runtime, Url, Webview, WebviewUrl};

// Windows 上持久分区的数据目录根（<dataDir>/webview-partitions）。macOS 用 WKWebsiteDataStore 标识符。
pub static PARTITION_ROOT: OnceLock<PathBuf> = OnceLock::new();

const LABEL_PREFIX: &str = "wv-";
// desktop 浏览器池上限 5 个，再加验证码 / 视频预览 / 插件页，留足余量。
const MAX_CHILDREN: usize = 16;
const EVAL_TIMEOUT: Duration = Duration::from_secs(15);
const NAV_TIMEOUT: Duration = Duration::from_secs(5);
// 不可见时放到窗口外，避免创建到 hide 之间闪一帧。
const PARKED: (f64, f64) = (-20000.0, -20000.0);

fn label(id: u32) -> String {
    format!("{LABEL_PREFIX}{id}")
}

fn err(e: impl std::fmt::Display) -> String {
    e.to_string()
}

pub fn require_main<R: Runtime>(webview: &Webview<R>) -> Result<(), String> {
    if webview.label() == "main" {
        Ok(())
    } else {
        Err(format!("command is not available in webview {}", webview.label()))
    }
}

// 子 webview 允许的地址：http(s) 外部站点、本地 HTML 预览的 file:，加上页面内部常用的 about: / data: / blob:（iframe）。
// 拒绝 *.localhost 子域：Windows 上 tauri.localhost / cindy-ipc.localhost 会被 Tauri 当成本地来源。
pub fn allowed_child_url(url: &Url) -> bool {
    match url.scheme() {
        "http" | "https" => match url.host_str() {
            Some(h) => !h.to_ascii_lowercase().ends_with(".localhost"),
            None => false,
        },
        "file" | "about" | "data" | "blob" => true,
        _ => false,
    }
}

fn emit<R: Runtime>(app: &AppHandle<R>, id: u32, kind: &str, mut extra: Value) {
    if let Value::Object(m) = &mut extra {
        m.insert("id".into(), json!(id));
        m.insert("type".into(), json!(kind));
    }
    if let Err(e) = app.emit_to(EventTarget::webview("main"), "cindy:webview", extra) {
        eprintln!("[browser] emit failed: {e}");
    }
}

// 分区名 → 稳定的 16 字节标识（两轮 FNV-1a，跨版本不变，升级后 cookie 还在）。
fn partition_id(name: &str) -> [u8; 16] {
    let fnv = |mut h: u64| {
        for b in name.bytes() {
            h ^= b as u64;
            h = h.wrapping_mul(0x100000001b3);
        }
        h
    };
    let a = fnv(0xcbf29ce484222325).to_be_bytes();
    let b = fnv(0x84222325cbf29ce4).to_be_bytes();
    let mut out = [0u8; 16];
    out[..8].copy_from_slice(&a);
    out[8..].copy_from_slice(&b);
    out
}

// Electron partition 语义：persist:<name> 持久，其余（如 login-captcha、cindy-ghost-*）只在内存里。
// 空分区在 Electron 里是默认 session；这里也给独立存储，不和主 renderer 共用。
fn with_partition<R: Runtime>(builder: WebviewBuilder<R>, partition: &str) -> WebviewBuilder<R> {
    let name = match partition.strip_prefix("persist:") {
        Some(n) => n,
        None if partition.is_empty() => "",
        None => return builder.incognito(true),
    };
    let id = partition_id(name);
    #[cfg(target_os = "macos")]
    let builder = builder.data_store_identifier(id);
    #[cfg(windows)]
    let builder = match PARTITION_ROOT.get() {
        Some(root) => builder.data_directory(root.join(id.iter().map(|b| format!("{b:02x}")).collect::<String>())),
        None => builder,
    };
    #[cfg(not(any(target_os = "macos", windows)))]
    let _ = id;
    builder
}

fn child_builder<R: Runtime>(app: &AppHandle<R>, id: u32, url: Url, partition: &str) -> WebviewBuilder<R> {
    let popup_app = app.clone();
    let builder = WebviewBuilder::new(label(id), WebviewUrl::External(url))
        .focused(false)
        .on_navigation(allowed_child_url)
        // window.open / target=_blank（需要用户手势）：交给 renderer 变成新标签页（desktop rsb:browser-popup），原生窗口一律拒绝。
        .on_new_window(move |url, _features| {
            emit(&popup_app, id, "new-window", json!({ "url": url.as_str() }));
            NewWindowResponse::Deny
        })
        .on_page_load(move |wv, payload| {
            let kind = match payload.event() {
                PageLoadEvent::Started => "load-started",
                PageLoadEvent::Finished => "load-finished",
            };
            emit(wv.app_handle(), id, kind, json!({ "url": payload.url().as_str() }));
        })
        .on_document_title_changed(move |wv, title| emit(wv.app_handle(), id, "title", json!({ "title": title })))
        // 下载保存到系统“下载”文件夹（wry 默认目标）；不处理就会被取消。
        .on_download(|_wv, _event| true);
    with_partition(builder, partition)
}

#[derive(Deserialize)]
pub struct Bounds {
    x: f64,
    y: f64,
    width: f64,
    height: f64,
}

impl Bounds {
    fn rect(&self) -> Rect {
        Rect {
            position: LogicalPosition::new(self.x, self.y).into(),
            size: LogicalSize::new(self.width.max(1.0), self.height.max(1.0)).into(),
        }
    }
}

#[tauri::command]
pub async fn browser_create(
    webview: Webview,
    id: u32,
    url: String,
    partition: String,
    bounds: Bounds,
    visible: bool,
) -> Result<(), String> {
    require_main(&webview)?;
    let app = webview.app_handle().clone();
    if id == 0 || app.get_webview(&label(id)).is_some() {
        return Err(format!("[ALREADY_EXISTS] webview {id} already exists"));
    }
    let window = webview.window();
    let children = window.webviews().iter().filter(|w| w.label().starts_with(LABEL_PREFIX)).count();
    if children >= MAX_CHILDREN {
        return Err(format!("[LIMIT] at most {MAX_CHILDREN} browser webviews"));
    }
    let target = Url::parse(&url).ok().filter(allowed_child_url);
    // file: 要走平台专用的加载方式（WKWebView 需要授权读取目录），先建空白页再加载。
    let initial = match &target {
        Some(u) if u.scheme() != "file" => u.clone(),
        _ => Url::parse("about:blank").unwrap(),
    };
    let builder = child_builder(&app, id, initial, &partition);
    let (pos, size) = if visible {
        (LogicalPosition::new(bounds.x, bounds.y), LogicalSize::new(bounds.width.max(1.0), bounds.height.max(1.0)))
    } else {
        (LogicalPosition::new(PARKED.0, PARKED.1), LogicalSize::new(1280.0, 720.0))
    };
    let child = window.add_child(builder, pos, size).map_err(err)?;
    if !visible {
        let _ = child.hide();
    }
    if let Some(u) = target.filter(|u| u.scheme() == "file") {
        load(&child, u)?;
    }
    Ok(())
}

#[tauri::command]
pub async fn browser_op(webview: Webview, id: u32, op: String, arg: Option<Value>) -> Result<Value, String> {
    require_main(&webview)?;
    let child = webview
        .app_handle()
        .get_webview(&label(id))
        .ok_or_else(|| format!("[NOT_FOUND] webview {id} not found"))?;
    let arg = arg.unwrap_or(Value::Null);
    match op.as_str() {
        "navigate" => {
            let url = arg
                .get("url")
                .and_then(Value::as_str)
                .and_then(|u| Url::parse(u).ok())
                .filter(allowed_child_url)
                .ok_or("[INVALID_PARAMS] url is not allowed in the browser panel")?;
            load(&child, url)?;
        }
        "reload" => child.reload().map_err(err)?,
        "setBounds" => {
            let b: Bounds = serde_json::from_value(arg).map_err(|e| format!("[INVALID_PARAMS] {e}"))?;
            child.set_bounds(b.rect()).map_err(err)?;
        }
        "show" => child.show().map_err(err)?,
        "hide" => child.hide().map_err(err)?,
        "focus" => child.set_focus().map_err(err)?,
        "close" => child.close().map_err(err)?,
        "eval" => {
            let code = arg.get("code").and_then(Value::as_str).ok_or("[INVALID_PARAMS] code is required")?;
            return eval(&child, code.to_string()).await;
        }
        "back" | "forward" | "stop" | "state" => return nav(&child, op).await,
        other => return Err(format!("[INVALID_PARAMS] unknown browser op {other}")),
    }
    Ok(Value::Null)
}

// renderer 重新加载后，上一代 renderer 建的子 webview 已经没有宿主元素，全部关掉。
#[tauri::command]
pub fn browser_reset(webview: Webview) -> Result<(), String> {
    require_main(&webview)?;
    for w in webview.window().webviews() {
        if w.label().starts_with(LABEL_PREFIX) {
            let _ = w.close();
        }
    }
    Ok(())
}

fn load(child: &Webview, url: Url) -> Result<(), String> {
    if url.scheme() == "file" {
        return load_file(child, url);
    }
    child.navigate(url).map_err(err)
}

// macOS：loadRequest 不能读本地文件，用 loadFileURL 并只授权文件所在目录（同目录的 css / js / 图片）。
#[cfg(target_os = "macos")]
fn load_file(child: &Webview, url: Url) -> Result<(), String> {
    let text = url.to_string();
    child
        .with_webview(move |pv| {
            use objc2_foundation::{NSString, NSURL};
            use objc2_web_kit::WKWebView;
            // SAFETY: inner() 是 wry 持有的 WKWebView，with_webview 的回调在主线程执行。
            unsafe {
                let wv = &*(pv.inner() as *const WKWebView);
                let Some(file) = NSURL::URLWithString(&NSString::from_str(&text)) else {
                    return;
                };
                if let Some(dir) = file.URLByDeletingLastPathComponent() {
                    let _ = wv.loadFileURL_allowingReadAccessToURL(&file, &dir);
                }
            }
        })
        .map_err(err)
}

#[cfg(not(target_os = "macos"))]
fn load_file(child: &Webview, url: Url) -> Result<(), String> {
    child.navigate(url).map_err(err)
}

// 返回页面脚本结果的 JSON 文本（shim 包装过脚本，结果总是字符串）。
async fn eval(child: &Webview, code: String) -> Result<Value, String> {
    let (tx, rx) = mpsc::channel();
    child
        .eval_with_callback(code, move |res| {
            let _ = tx.send(res);
        })
        .map_err(err)?;
    let res = tauri::async_runtime::spawn_blocking(move || rx.recv_timeout(EVAL_TIMEOUT))
        .await
        .map_err(err)?
        .map_err(|_| "[TIMEOUT] executeJavaScript timed out".to_string())?;
    Ok(Value::String(res))
}

async fn nav(child: &Webview, op: String) -> Result<Value, String> {
    let (tx, rx) = mpsc::channel();
    child
        .with_webview(move |pv| {
            let _ = tx.send(platform_nav(pv, &op));
        })
        .map_err(err)?;
    let mut state = tauri::async_runtime::spawn_blocking(move || rx.recv_timeout(NAV_TIMEOUT))
        .await
        .map_err(err)?
        .map_err(|_| "[TIMEOUT] webview did not respond".to_string())?;
    let url = child.url().map(|u| u.to_string()).unwrap_or_default();
    let scale = child.window().scale_factor().unwrap_or(1.0);
    let bounds = child.bounds().ok().map(|r| {
        let p = r.position.to_logical::<f64>(scale);
        let s = r.size.to_logical::<f64>(scale);
        json!({ "x": p.x, "y": p.y, "width": s.width, "height": s.height })
    });
    if let Value::Object(m) = &mut state {
        m.insert("url".into(), json!(url));
        m.insert("bounds".into(), bounds.unwrap_or(Value::Null));
    }
    Ok(state)
}

// 前进 / 后退 / 停止 / 可否前进后退：Tauri 没有这些 API，直接调原生 webview（回调在主线程）。
#[cfg(target_os = "macos")]
fn platform_nav(pv: tauri::webview::PlatformWebview, op: &str) -> Value {
    use objc2_web_kit::WKWebView;
    // SAFETY: inner() 是 wry 持有的 WKWebView，with_webview 的回调在主线程执行。
    unsafe {
        let wv = &*(pv.inner() as *const WKWebView);
        match op {
            "back" => {
                let _ = wv.goBack();
            }
            "forward" => {
                let _ = wv.goForward();
            }
            "stop" => wv.stopLoading(),
            _ => {}
        }
        json!({ "canGoBack": wv.canGoBack(), "canGoForward": wv.canGoForward(), "isLoading": wv.isLoading() })
    }
}

#[cfg(windows)]
fn platform_nav(pv: tauri::webview::PlatformWebview, op: &str) -> Value {
    // SAFETY: controller() 是 wry 持有的 WebView2 控制器，回调在主线程（COM STA）执行。
    unsafe {
        let Ok(core) = pv.controller().CoreWebView2() else {
            return json!({});
        };
        match op {
            "back" => {
                let _ = core.GoBack();
            }
            "forward" => {
                let _ = core.GoForward();
            }
            "stop" => {
                let _ = core.Stop();
            }
            _ => {}
        }
        let mut back = windows_core::BOOL::default();
        let mut forward = windows_core::BOOL::default();
        let _ = core.CanGoBack(&mut back);
        let _ = core.CanGoForward(&mut forward);
        // WebView2 没有同步的 isLoading，由 shim 按页面加载事件自己记。
        json!({ "canGoBack": back.as_bool(), "canGoForward": forward.as_bool(), "isLoading": Value::Null })
    }
}

#[cfg(not(any(target_os = "macos", windows)))]
fn platform_nav(_pv: tauri::webview::PlatformWebview, _op: &str) -> Value {
    json!({})
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn child_urls() {
        let ok = |s: &str| allowed_child_url(&Url::parse(s).unwrap());
        assert!(ok("https://example.com/"));
        assert!(ok("http://127.0.0.1:8080/a"));
        assert!(ok("http://localhost:3000/"));
        assert!(ok("about:blank"));
        assert!(!ok("http://tauri.localhost/"));
        assert!(!ok("https://cindy-ipc.LOCALHOST/sync/x"));
        assert!(!ok("tauri://localhost/"));
        assert!(!ok("cindy-ipc://localhost/sync/x"));
        assert!(ok("file:///Users/a/index.html"));
        assert!(!ok("cindy-ghost://plugin/index.html"));
    }

    #[test]
    fn partition_ids_are_stable() {
        assert_eq!(partition_id("xdmaker-browser-app"), partition_id("xdmaker-browser-app"));
        assert_ne!(partition_id("a"), partition_id("b"));
    }
}
