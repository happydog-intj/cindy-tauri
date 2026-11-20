//! 浏览器面板的通道（原 sidecar browser-panel.mjs）：rsb-browser-bridge:*、browser-backend:*、rsb-native-popup:*，
//! 加上 src/shim/webview.mjs 专用的 tauri-browser:popup。
//!
//! desktop 的 TabRegistry 记 tabId → webContentsId，供 Agent 浏览器自动化后端和弹窗归属使用。Tauri 版没有
//! Agent 自动化后端（browser-backend 报不可用），registry 只用来给弹窗补 openerTabId / openerSessionId。
//! webContentsId 就是 shim 分配的子 webview id；Tauri 版只有主窗口一个 renderer，不做 desktop 的宿主校验。

use crate::maker_host::Emitter;
use crate::override_file::OverrideFile;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

const EXTENSION_STORE_URL: &str = "https://chromewebstore.google.com/detail/cindy/dmbhhhdijpeaibfeocdcjhfefkgpkeef";
const KINDS: [&str; 3] = ["external", "rsb-webview", "user-browser"];
/// 弹窗先于 report 到达时等待归属的上限（与 desktop DEFERRED_POPUP_ROUTE_TIMEOUT_MS 一致）。
const POPUP_OPENER_WAIT: Duration = Duration::from_secs(10);

pub const INVOKE_CHANNELS: &[&str] = &[
    "rsb-browser-bridge:report",
    "rsb-browser-bridge:release",
    "rsb-browser-bridge:snapshot",
    "rsb-browser-bridge:capture-screenshot",
    "rsb-browser-bridge:capture-screenshot-data",
    "rsb-browser-bridge:tab-op-result",
    "rsb-browser-bridge:set-active-session",
    "rsb-browser-bridge:set-foreground",
    "rsb-browser-bridge:force-kill",
    "tauri-browser:popup",
    "rsb-native-popup:claim",
    "rsb-native-popup:set-bounds",
    "rsb-native-popup:command",
    "rsb-native-popup:close",
    "browser-backend:get-state",
    "browser-backend:set-kind",
    "browser-backend:reset",
    "browser-backend:set-use-real-profile",
    "browser-backend:get-health",
    "browser-backend:recover",
    "browser-backend:probe-source-read",
    "browser-backend:open-extension-store",
];

/// 打开系统浏览器（原 `callNative('open.url')`）。
pub trait UrlOpener: Send + Sync + 'static {
    fn open(&self, url: &str) -> Result<(), String>;
}

fn fail<T>(code: &str, msg: impl AsRef<str>) -> Result<T, String> {
    Err(format!("[{code}] {}", msg.as_ref()))
}

fn require_object<'a>(v: Option<&'a Value>, name: &str) -> Result<&'a serde_json::Map<String, Value>, String> {
    match v.and_then(Value::as_object) {
        Some(o) => Ok(o),
        None => fail("INVALID_PARAMS", format!("{name} must be an object")),
    }
}

fn require_string(v: Option<&Value>, name: &str) -> Result<String, String> {
    match v.and_then(Value::as_str).filter(|s| !s.is_empty()) {
        Some(s) => Ok(s.to_string()),
        None => fail("INVALID_PARAMS", format!("{name} must be a non-empty string")),
    }
}

fn is_integer(v: &Value) -> Option<i64> {
    let n = v.as_f64()?;
    (n.is_finite() && n.fract() == 0.0).then_some(n as i64)
}

fn require_id(v: Option<&Value>, name: &str) -> Result<i64, String> {
    match v.and_then(is_integer).filter(|n| *n >= 0) {
        Some(n) => Ok(n),
        None => fail("INVALID_PARAMS", format!("{name} must be a non-negative integer")),
    }
}

fn is_routable_popup_url(url: &str) -> bool {
    url::Url::parse(url).map(|u| u.scheme() == "http" || u.scheme() == "https").unwrap_or(false)
}

#[derive(Clone, Debug)]
struct Record {
    session_id: String,
    tab_id: String,
    web_contents_id: i64,
}

#[derive(Default)]
struct Registry {
    /// tabId → 记录；插入顺序无意义（snapshot 的 dropped 按 tabId 排序，JS 里是插入顺序，e2e 只含一项）。
    records: HashMap<String, Record>,
    order: Vec<String>,
    pinned: Vec<String>,
    active_session_id: Option<String>,
    foreground_tab_id: Option<String>,
}

pub struct BrowserPanel {
    reg: Mutex<Registry>,
    /// report 到达时唤醒等待归属的弹窗。
    reported: Condvar,
    settings: OverrideFile,
    emitter: Arc<dyn Emitter>,
    opener: Arc<dyn UrlOpener>,
    popup_wait: Duration,
}

impl BrowserPanel {
    pub fn new(settings_dir: &Path, emitter: Arc<dyn Emitter>, opener: Arc<dyn UrlOpener>) -> Self {
        Self::with_wait(settings_dir, emitter, opener, POPUP_OPENER_WAIT)
    }

    pub fn with_wait(settings_dir: &Path, emitter: Arc<dyn Emitter>, opener: Arc<dyn UrlOpener>, popup_wait: Duration) -> Self {
        let settings = OverrideFile::new(settings_dir.join("browser-backend-settings.json"), json!({ "kind": "external", "useRealProfile": false }), |raw| {
            let kind = raw.get("kind").and_then(Value::as_str).filter(|k| KINDS.contains(k)).unwrap_or("external");
            json!({ "kind": kind, "useRealProfile": raw.get("useRealProfile") == Some(&json!(true)) })
        });
        Self { reg: Mutex::new(Registry::default()), reported: Condvar::new(), settings, emitter, opener, popup_wait }
    }

    pub fn owns(channel: &str) -> bool {
        INVOKE_CHANNELS.contains(&channel)
    }

    fn active(&self) -> String {
        self.settings.read()["kind"].as_str().unwrap_or("external").to_string()
    }

    fn health(&self) -> Value {
        json!({ "active": self.active(), "status": "error", "canRecover": false, "reason": "host-unavailable", "errorCode": "NOT_PORTED" })
    }

    pub fn invoke(&self, channel: &str, args: &[Value]) -> Option<Result<Value, String>> {
        let a0 = args.first();
        Some(match channel {
            "rsb-browser-bridge:report" => self.report(a0),
            "rsb-browser-bridge:release" => self.release(a0),
            "rsb-browser-bridge:snapshot" => self.snapshot(a0),
            // 截图要读子 webview 的像素，WKWebView / WebView2 的截图 API 尚未接入（DESIGN.md §10 的降级项）。
            "rsb-browser-bridge:capture-screenshot" | "rsb-browser-bridge:capture-screenshot-data" => require_object(a0, "capture-screenshot payload")
                .and_then(|o| require_string(o.get("tabId"), "tabId"))
                .and_then(|_| fail("UNSUPPORTED_CAPABILITY", "browser screenshots are not available in this build yet")),
            "rsb-browser-bridge:tab-op-result" => Ok(json!({ "ok": true })),
            "rsb-browser-bridge:set-active-session" => require_object(a0, "set-active-session payload").map(|o| {
                self.reg.lock().unwrap().active_session_id = o.get("sessionId").and_then(Value::as_str).filter(|s| !s.is_empty()).map(String::from);
                json!({ "ok": true })
            }),
            "rsb-browser-bridge:set-foreground" => require_object(a0, "set-foreground payload").and_then(|o| match o.get("tabId") {
                Some(Value::Null) => {
                    self.reg.lock().unwrap().foreground_tab_id = None;
                    Ok(json!({ "ok": true }))
                }
                Some(Value::String(s)) if !s.is_empty() => {
                    self.reg.lock().unwrap().foreground_tab_id = Some(s.clone());
                    Ok(json!({ "ok": true }))
                }
                _ => fail("INVALID_PARAMS", "tabId must be a non-empty string or null"),
            }),
            // 系统 webview 不给强杀单个页面进程的接口；资源看门狗也没有移植，所以这条路径不会被触发。
            "rsb-browser-bridge:force-kill" => require_object(a0, "force-kill payload")
                .and_then(|o| require_string(o.get("tabId"), "tabId"))
                .and_then(|_| fail("UNSUPPORTED_CAPABILITY", "terminating a browser page is not available in this build")),
            "tauri-browser:popup" => self.popup(a0),
            // desktop 把 Chromium 预建的弹窗 WebContents 收进原生视图；Tauri 版弹窗一律变成普通新标签页，没有原生弹窗面。
            "rsb-native-popup:claim" => Ok(json!({ "alive": false })),
            "rsb-native-popup:set-bounds" | "rsb-native-popup:command" | "rsb-native-popup:close" => Ok(json!({ "ok": true })),
            "browser-backend:get-state" => {
                let s = self.settings.read_state();
                Ok(json!({
                    "active": s.value["kind"], "systemDefault": s.defaults["kind"],
                    "isOverride": s.customized_keys.iter().any(|k| k == "kind"), "useRealProfile": s.value["useRealProfile"],
                }))
            }
            "browser-backend:set-kind" => require_object(a0, "set-kind payload").and_then(|o| match o.get("kind").and_then(Value::as_str).filter(|k| KINDS.contains(k)) {
                Some(k) => self.settings.write_patch(&json!({ "kind": k }), true).map(|_| json!({ "ok": true, "active": self.active() })),
                None => fail("INVALID_PARAMS", format!("kind must be one of {}", KINDS.join(", "))),
            }),
            "browser-backend:reset" => self.settings.reset().map(|_| json!({ "ok": true, "active": self.active() })),
            "browser-backend:set-use-real-profile" => require_object(a0, "set-use-real-profile payload").and_then(|o| match o.get("enabled").and_then(Value::as_bool) {
                None => fail("INVALID_PARAMS", "enabled must be a boolean"),
                // 复制系统 Chrome 登录态依赖外置 Chrome 后端，未移植。
                Some(true) => fail("NO_CHROMIUM", "copying browser logins is not available in this build"),
                Some(false) => self.settings.write_patch(&json!({ "useRealProfile": false }), false).map(|_| json!({ "ok": true, "enabled": false })),
            }),
            "browser-backend:get-health" => Ok(self.health()),
            "browser-backend:recover" => Ok(json!({ "ok": false, "health": self.health() })),
            "browser-backend:probe-source-read" => Ok(json!({ "readable": false })),
            "browser-backend:open-extension-store" => Ok(match self.opener.open(EXTENSION_STORE_URL) {
                Ok(_) => json!({ "ok": true }),
                Err(_) => json!({ "ok": false, "error": "OPEN_FAILED" }),
            }),
            _ => return None,
        })
    }

    fn report(&self, payload: Option<&Value>) -> Result<Value, String> {
        let o = require_object(payload, "report payload")?;
        let record = Record {
            session_id: require_string(o.get("sessionId"), "sessionId")?,
            tab_id: require_string(o.get("tabId"), "tabId")?,
            web_contents_id: require_id(o.get("webContentsId"), "webContentsId")?,
        };
        let mut g = self.reg.lock().unwrap();
        if g.records.insert(record.tab_id.clone(), record.clone()).is_none() {
            g.order.push(record.tab_id.clone());
        }
        drop(g);
        self.reported.notify_all();
        Ok(json!({ "ok": true }))
    }

    fn release(&self, payload: Option<&Value>) -> Result<Value, String> {
        let o = require_object(payload, "release payload")?;
        let tab_id = require_string(o.get("tabId"), "tabId")?;
        let wc = match o.get("webContentsId") {
            None | Some(Value::Null) => None,
            Some(v) if crate::voice_settings::is_undef(v) => None,
            Some(v) => match is_integer(v) {
                Some(n) => Some(n),
                None => return fail("INVALID_PARAMS", "webContentsId must be an integer when provided"),
            },
        };
        let mut g = self.reg.lock().unwrap();
        // 带旧 id 的 release 只在记录仍指向它时生效（desktop registry.release 的 stale 防护）。
        if let Some(cur) = g.records.get(&tab_id) {
            if wc.is_none() || Some(cur.web_contents_id) == wc {
                g.records.remove(&tab_id);
                g.order.retain(|t| *t != tab_id);
                g.pinned.retain(|t| *t != tab_id);
            }
        }
        Ok(json!({ "ok": true }))
    }

    fn snapshot(&self, payload: Option<&Value>) -> Result<Value, String> {
        let o = require_object(payload, "snapshot payload")?;
        let Some(ids) = o.get("liveTabIds").and_then(Value::as_array) else { return fail("INVALID_PARAMS", "liveTabIds must be an array") };
        let mut live = Vec::new();
        for (i, id) in ids.iter().enumerate() {
            live.push(require_string(Some(id), &format!("liveTabIds[{i}]"))?);
        }
        let mut g = self.reg.lock().unwrap();
        // renderer 重新加载后 shim 会关掉上一代全部子 webview，不在 live 里的记录都已失效。
        let dropped: Vec<String> = g.order.iter().filter(|t| !live.contains(t)).cloned().collect();
        for t in &dropped {
            g.records.remove(t);
        }
        g.order.retain(|t| !dropped.contains(t));
        Ok(json!({ "ok": true, "dropped": dropped, "kept": g.records.len(), "pinnedTabIds": g.pinned }))
    }

    /// src/shim/webview.mjs：子 webview 里 window.open / target=_blank 被原生拒绝后转到这里，补归属后推给 renderer 开新标签页。
    fn popup(&self, payload: Option<&Value>) -> Result<Value, String> {
        let o = require_object(payload, "popup payload")?;
        let wc = require_id(o.get("webContentsId"), "webContentsId")?;
        let url = require_string(o.get("url"), "url")?;
        let disposition = o.get("disposition").and_then(Value::as_str).filter(|s| !s.is_empty()).unwrap_or("foreground-tab");
        if !is_routable_popup_url(&url) {
            return Ok(json!({ "ok": false }));
        }
        let find = |g: &Registry| g.records.values().find(|r| r.web_contents_id == wc).cloned();
        let mut g = self.reg.lock().unwrap();
        let mut opener = find(&g);
        if opener.is_none() {
            // 页面 head 里的同步脚本可能在 renderer report 之前就 window.open，等一会儿归属。
            let deadline = Instant::now() + self.popup_wait;
            while opener.is_none() {
                let left = deadline.saturating_duration_since(Instant::now());
                if left.is_zero() {
                    break;
                }
                g = self.reported.wait_timeout(g, left).unwrap().0;
                opener = find(&g);
            }
        }
        drop(g);
        let mut ev = json!({ "url": url, "disposition": disposition });
        if let Some(r) = opener {
            ev["openerTabId"] = json!(r.tab_id);
            ev["openerSessionId"] = json!(r.session_id);
        }
        self.emitter.emit("rsb:browser-popup", ev);
        Ok(json!({ "ok": true }))
    }

    #[cfg(test)]
    fn state(&self) -> (Option<String>, Option<String>) {
        let g = self.reg.lock().unwrap();
        (g.active_session_id.clone(), g.foreground_tab_id.clone())
    }
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
    struct Opener(Mutex<Vec<String>>, bool);
    impl UrlOpener for Opener {
        fn open(&self, url: &str) -> Result<(), String> {
            self.0.lock().unwrap().push(url.into());
            if self.1 { Ok(()) } else { Err("no".into()) }
        }
    }

    fn rig(name: &str, wait_ms: u64, open_ok: bool) -> (Arc<BrowserPanel>, Arc<Rec>, Arc<Opener>, std::path::PathBuf) {
        let d = std::env::temp_dir().join(format!("browser-panel-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        let rec = Arc::new(Rec(Mutex::new(vec![])));
        let op = Arc::new(Opener(Mutex::new(vec![]), open_ok));
        (Arc::new(BrowserPanel::with_wait(&d, rec.clone(), op.clone(), Duration::from_millis(wait_ms))), rec, op, d)
    }
    fn call(p: &BrowserPanel, ch: &str, a: Value) -> Result<Value, String> {
        p.invoke(ch, &[a]).unwrap()
    }
    fn popups(r: &Rec) -> Vec<Value> {
        r.0.lock().unwrap().iter().filter(|(c, _)| c == "rsb:browser-popup").map(|(_, v)| v.clone()).collect()
    }

    #[test]
    fn popup_attribution_and_registry() {
        let (p, rec, _op, _d) = rig("pop", 3000, true);
        let e = call(&p, "rsb-browser-bridge:report", json!({ "sessionId": "s1", "tabId": "tab-a", "webContentsId": -1 })).unwrap_err();
        assert!(e.contains("INVALID_PARAMS"), "{e}");
        assert_eq!(call(&p, "rsb-browser-bridge:report", json!({ "sessionId": "s1", "tabId": "tab-a", "webContentsId": 41 })).unwrap()["ok"], true);
        call(&p, "tauri-browser:popup", json!({ "webContentsId": 41, "url": "https://popup.test/a", "disposition": "foreground-tab" })).unwrap();
        let ev = popups(&rec).pop().unwrap();
        assert_eq!(ev, json!({ "url": "https://popup.test/a", "disposition": "foreground-tab", "openerTabId": "tab-a", "openerSessionId": "s1" }));
        let non_http = call(&p, "tauri-browser:popup", json!({ "webContentsId": 41, "url": "file:///etc/passwd" })).unwrap();
        assert_eq!(non_http["ok"], false);
        assert_eq!(popups(&rec).len(), 1);

        // 弹窗先到、report 后到：等到 report 再路由。
        let p2 = p.clone();
        let late = std::thread::spawn(move || call(&p2, "tauri-browser:popup", json!({ "webContentsId": 42, "url": "https://popup.test/late" })));
        std::thread::sleep(Duration::from_millis(200));
        assert_eq!(popups(&rec).len(), 1, "report 之前不推");
        call(&p, "rsb-browser-bridge:report", json!({ "sessionId": "s1", "tabId": "tab-b", "webContentsId": 42 })).unwrap();
        late.join().unwrap().unwrap();
        let ev = popups(&rec).pop().unwrap();
        assert_eq!(ev["openerTabId"], "tab-b");
        assert_eq!(ev["disposition"], "foreground-tab");

        // 过期的 release 不删新记录；没有 webContentsId 的 release 才无条件删
        call(&p, "rsb-browser-bridge:release", json!({ "tabId": "tab-b", "webContentsId": 7 })).unwrap();
        call(&p, "tauri-browser:popup", json!({ "webContentsId": 42, "url": "https://popup.test/still" })).unwrap();
        assert_eq!(popups(&rec).pop().unwrap()["openerTabId"], "tab-b");
        let snap = call(&p, "rsb-browser-bridge:snapshot", json!({ "liveTabIds": ["tab-a"] })).unwrap();
        assert_eq!(snap, json!({ "ok": true, "dropped": ["tab-b"], "kept": 1, "pinnedTabIds": [] }));
        assert!(call(&p, "rsb-browser-bridge:snapshot", json!({ "liveTabIds": [""] })).unwrap_err().contains("INVALID_PARAMS"));
        assert!(call(&p, "rsb-browser-bridge:snapshot", json!({})).unwrap_err().contains("liveTabIds must be an array"));
        call(&p, "rsb-browser-bridge:release", json!({ "tabId": "tab-a" })).unwrap();
        let snap = call(&p, "rsb-browser-bridge:snapshot", json!({ "liveTabIds": [] })).unwrap();
        assert_eq!(snap["kept"], 0);
        assert!(call(&p, "rsb-browser-bridge:release", json!({ "tabId": "x", "webContentsId": 1.5 })).unwrap_err().contains("integer"));
    }

    #[test]
    fn popup_without_report_times_out_without_opener() {
        let (p, rec, _op, _d) = rig("timeout", 150, true);
        let t = Instant::now();
        call(&p, "tauri-browser:popup", json!({ "webContentsId": 5, "url": "https://x.test/" })).unwrap();
        assert!(t.elapsed() >= Duration::from_millis(140));
        assert_eq!(popups(&rec).pop().unwrap(), json!({ "url": "https://x.test/", "disposition": "foreground-tab" }));
    }

    #[test]
    fn bridge_stubs_and_validation() {
        let (p, _rec, _op, _d) = rig("stubs", 10, true);
        for (ch, arg) in [("set-active-session", json!({ "sessionId": "s" })), ("set-foreground", json!({ "tabId": "tab-a" })), ("set-foreground", json!({ "tabId": null })), ("tab-op-result", json!({}))] {
            assert_eq!(call(&p, &format!("rsb-browser-bridge:{ch}"), arg).unwrap()["ok"], true, "{ch}");
        }
        assert_eq!(p.state(), (Some("s".into()), None));
        assert!(call(&p, "rsb-browser-bridge:set-foreground", json!({ "tabId": 3 })).unwrap_err().contains("INVALID_PARAMS"));
        assert!(call(&p, "rsb-browser-bridge:set-foreground", json!({ "tabId": "" })).unwrap_err().contains("INVALID_PARAMS"));
        assert!(call(&p, "rsb-browser-bridge:capture-screenshot", json!({ "tabId": "t" })).unwrap_err().contains("UNSUPPORTED_CAPABILITY"));
        assert!(call(&p, "rsb-browser-bridge:capture-screenshot-data", json!({})).unwrap_err().contains("INVALID_PARAMS"));
        assert!(call(&p, "rsb-browser-bridge:force-kill", json!({ "tabId": "t" })).unwrap_err().contains("UNSUPPORTED_CAPABILITY"));
        assert!(p.invoke("rsb-browser-bridge:report", &[]).unwrap().unwrap_err().contains("report payload must be an object"));
        assert_eq!(call(&p, "rsb-native-popup:claim", json!({})).unwrap(), json!({ "alive": false }));
        assert_eq!(call(&p, "rsb-native-popup:close", json!({})).unwrap(), json!({ "ok": true }));
        for ch in INVOKE_CHANNELS {
            assert!(BrowserPanel::owns(ch));
        }
    }

    #[test]
    fn backend_settings_persist_and_degrade() {
        let (p, rec, op, d) = rig("backend", 10, true);
        let st = call(&p, "browser-backend:get-state", json!(null)).unwrap();
        assert_eq!(st, json!({ "active": "external", "systemDefault": "external", "isOverride": false, "useRealProfile": false }));
        assert_eq!(call(&p, "browser-backend:set-kind", json!({ "kind": "rsb-webview" })).unwrap()["active"], "rsb-webview");
        assert!(call(&p, "browser-backend:set-kind", json!({ "kind": "chrome" })).unwrap_err().contains("INVALID_PARAMS"));
        let h = call(&p, "browser-backend:get-health", json!(null)).unwrap();
        assert_eq!(h, json!({ "active": "rsb-webview", "status": "error", "canRecover": false, "reason": "host-unavailable", "errorCode": "NOT_PORTED" }));
        assert_eq!(call(&p, "browser-backend:recover", json!(null)).unwrap()["ok"], false);
        assert_eq!(call(&p, "browser-backend:probe-source-read", json!(null)).unwrap()["readable"], false);
        assert!(call(&p, "browser-backend:set-use-real-profile", json!({ "enabled": true })).unwrap_err().contains("NO_CHROMIUM"));
        assert!(call(&p, "browser-backend:set-use-real-profile", json!({ "enabled": 1 })).unwrap_err().contains("INVALID_PARAMS"));
        assert_eq!(call(&p, "browser-backend:set-use-real-profile", json!({ "enabled": false })).unwrap(), json!({ "ok": true, "enabled": false }));
        assert_eq!(call(&p, "browser-backend:open-extension-store", json!(null)).unwrap(), json!({ "ok": true }));
        assert!(op.0.lock().unwrap()[0].starts_with("https://chromewebstore.google.com/"));
        let _ = rec;
        // 重启后选择还在，reset 回默认
        let p2 = BrowserPanel::with_wait(&d, Arc::new(Rec(Mutex::new(vec![]))), op.clone(), Duration::from_millis(10));
        let st = p2.invoke("browser-backend:get-state", &[]).unwrap().unwrap();
        assert_eq!(st["active"], "rsb-webview");
        assert_eq!(st["isOverride"], true);
        assert_eq!(p2.invoke("browser-backend:reset", &[]).unwrap().unwrap()["active"], "external");
        let (bad, _r, _o, _d) = rig("openfail", 10, false);
        assert_eq!(call(&bad, "browser-backend:open-extension-store", json!(null)).unwrap(), json!({ "ok": false, "error": "OPEN_FAILED" }));
    }
}
