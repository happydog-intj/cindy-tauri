use super::*;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::Duration;

#[derive(Default)]
struct FakePorts {
    notified: Mutex<Vec<(String, Value)>>,
    calls: Mutex<Vec<(String, Value, bool)>>,
    emitted: Mutex<Vec<(String, Value)>>,
    /// method → 预设回复；缺省返回 Null。
    replies: Mutex<std::collections::HashMap<String, Result<Value, String>>>,
    /// dialog.message 依次返回的按钮下标。
    dialog_answers: Mutex<std::collections::VecDeque<i64>>,
    muted: Mutex<Vec<String>>,
    busy: Mutex<bool>,
    generation: AtomicI64,
}

impl Ports for FakePorts {
    fn notify(&self, method: &str, params: Value) {
        self.notified.lock().unwrap().push((method.to_string(), params));
    }
    fn call(&self, method: &str, params: Value, wait_user: bool) -> Result<Value, String> {
        self.calls.lock().unwrap().push((method.to_string(), params, wait_user));
        if method == "dialog.message" {
            if let Some(i) = self.dialog_answers.lock().unwrap().pop_front() {
                return Ok(json!(i));
            }
        }
        self.replies.lock().unwrap().get(method).cloned().unwrap_or(Ok(Value::Null))
    }
    fn emit(&self, channel: &str, arg: Value) {
        self.emitted.lock().unwrap().push((channel.to_string(), arg));
    }
    fn is_muted(&self, sid: &str) -> bool {
        self.muted.lock().unwrap().iter().any(|m| m == sid)
    }
    fn any_session_in_turn(&self) -> bool {
        *self.busy.lock().unwrap()
    }
    fn owner(&self) -> (Option<String>, i64) {
        (Some("local-v1".into()), self.generation.load(Ordering::SeqCst).max(1))
    }
}

impl FakePorts {
    fn last_notified(&self, m: &str) -> Option<Value> {
        self.notified.lock().unwrap().iter().rev().find(|(k, _)| k == m).map(|(_, v)| v.clone())
    }
    fn count_notified(&self, m: &str) -> usize {
        self.notified.lock().unwrap().iter().filter(|(k, _)| k == m).count()
    }
    fn count_calls(&self, m: &str) -> usize {
        self.calls.lock().unwrap().iter().filter(|(k, _, _)| k == m).count()
    }
    fn last_call(&self, m: &str) -> Option<Value> {
        self.calls.lock().unwrap().iter().rev().find(|(k, _, _)| k == m).map(|(_, v, _)| v.clone())
    }
    fn emitted_has(&self, ch: &str) -> bool {
        self.emitted.lock().unwrap().iter().any(|(k, _)| k == ch)
    }
}

fn temp(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("cindy-shell-{name}-{}-{}", std::process::id(), std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()));
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn rig(name: &str) -> (Arc<DesktopShell>, Arc<FakePorts>, PathBuf) {
    let dir = temp(name);
    let ports = Arc::new(FakePorts::default());
    let shell = DesktopShell::with_platform(ports.clone(), &dir, false, false);
    (shell, ports, dir)
}

fn call(s: &DesktopShell, ch: &str, args: Value) -> Result<Value, String> {
    s.invoke(ch, args.as_array().unwrap()).expect("channel owned")
}

#[test]
fn tray_labels_are_pushed_at_start_and_follow_the_locale() {
    let (s, p, _d) = rig("tray");
    assert!(p.last_notified("tray.setLabels").unwrap()["quit"].as_str().unwrap().len() > 0);
    let before = p.count_notified("tray.setLabels");
    call(&s, "app-menu:set-locale", json!(["zh-CN"])).unwrap();
    assert_eq!(p.last_notified("tray.setLabels").unwrap()["quit"], "退出 Cindy");
    call(&s, "app-menu:set-locale", json!(["xx-bogus"])).unwrap();
    assert_eq!(p.last_notified("tray.setLabels").unwrap()["quit"], "退出 Cindy", "不支持的语言保持当前");
    call(&s, "app-menu:set-locale", json!(["zh-CN"])).unwrap();
    assert_eq!(p.count_notified("tray.setLabels"), before + 1, "语言没变就不重发");
}

#[test]
fn session_event_toasts_badge_and_mute() {
    let (s, p, _d) = rig("toast");
    call(&s, "app-menu:set-locale", json!(["zh-CN"])).unwrap();
    call(&s, "notification:show-session-event", json!([{ "sessionId": "abcdef1234567890", "title": "  ", "kind": "done" }])).unwrap();
    let toast = p.last_notified("notify").unwrap();
    assert_eq!(toast["title"], "Cindy · abcdef12", "没标题时用会话 id 前 8 位");
    assert_ne!(toast["body"], "settings.notifications.sessionEvent.done", "正文已本地化");
    assert_eq!(toast["sessionId"], "abcdef1234567890");
    assert_eq!(p.last_notified("badge.set").unwrap()["count"], 1);

    let err = call(&s, "notification:show-session-event", json!([{ "sessionId": "x", "title": "", "kind": "boom" }])).unwrap_err();
    assert!(err.contains("INVALID_PARAMS"));
    // needs-reply 之外不许带 inputHandoffRequestId
    let err = call(&s, "notification:show-session-event", json!([{ "sessionId": "x", "title": "", "kind": "done", "inputHandoffRequestId": "r" }])).unwrap_err();
    assert!(err.contains("INVALID_PARAMS"));

    call(&s, "notification:set-desktop-enabled", json!([false])).unwrap();
    let n0 = p.count_notified("notify");
    call(&s, "notification:show-session-event", json!([{ "sessionId": "s2", "title": "T", "kind": "error" }])).unwrap();
    assert_eq!(p.count_notified("notify"), n0, "桌面通知关了就不弹");
    assert_eq!(s.attention_count(), 2, "但仍计入角标");
    call(&s, "notification:set-desktop-enabled", json!([true])).unwrap();
    assert!(call(&s, "notification:set-desktop-enabled", json!(["yes"])).unwrap_err().contains("INVALID_PARAMS"));

    p.muted.lock().unwrap().push("quiet".into());
    let n1 = p.count_notified("notify");
    call(&s, "notification:show-session-event", json!([{ "sessionId": "quiet", "title": "T", "kind": "done" }])).unwrap();
    assert_eq!(p.count_notified("notify"), n1, "静音的会话不弹");
    call(&s, "notification:show-session-event", json!([{ "sessionId": "loud", "title": "T", "kind": "done", "channels": { "desktop": false } }])).unwrap();
    assert_eq!(p.count_notified("notify"), n1, "channels.desktop=false 不弹");
    call(&s, "notification:show-session-event", json!([{ "sessionId": "loud2", "title": "T", "kind": "done", "markAttention": false }])).unwrap();
    assert_eq!(p.count_notified("notify"), n1 + 1);
    assert_eq!(s.attention_count(), 4, "markAttention=false 不计角标");
}

#[test]
fn attention_projection_takes_over_the_badge_and_needs_a_current_owner() {
    let (s, p, _d) = rig("attn");
    call(&s, "notification:mark-session-attention", json!(["a"])).unwrap();
    assert_eq!(p.last_notified("badge.set").unwrap()["count"], 1);
    call(&s, "notification:set-app-attention-count", json!([{ "dataOwnerId": "local-v1", "ownerGeneration": 1, "count": 7, "sessionIds": ["a"] }])).unwrap();
    assert_eq!(p.last_notified("badge.set").unwrap()["count"], 7, "投影之后以 renderer 为准");
    // 投影生效后角标数取投影值，标记新会话也不改它（数字没变就不重发）。
    let n = p.count_notified("badge.set");
    call(&s, "notification:mark-session-attention", json!(["a"])).unwrap();
    call(&s, "notification:mark-session-attention", json!(["z"])).unwrap();
    assert_eq!(p.count_notified("badge.set"), n);
    assert_eq!(s.attention_count(), 7);

    for bad in [
        json!({ "count": 1, "sessionIds": [] }),
        json!({ "dataOwnerId": "local-v1", "ownerGeneration": 1, "count": -1, "sessionIds": [] }),
        json!({ "dataOwnerId": "local-v1", "ownerGeneration": 1, "count": 1, "sessionIds": [""] }),
        json!({ "dataOwnerId": "local-v1", "ownerGeneration": 1, "count": 1 }),
    ] {
        assert!(call(&s, "notification:set-app-attention-count", json!([bad])).unwrap_err().contains("INVALID_PARAMS"));
    }
    // owner 过期：静默忽略
    let n2 = p.count_notified("badge.set");
    call(&s, "notification:set-app-attention-count", json!([{ "dataOwnerId": "local-v1", "ownerGeneration": 99, "count": 3, "sessionIds": [] }])).unwrap();
    call(&s, "notification:set-app-attention-count", json!([{ "dataOwnerId": null, "ownerGeneration": 1, "count": 3, "sessionIds": [] }])).unwrap();
    assert_eq!(p.count_notified("badge.set"), n2);

    call(&s, "notification:clear-session-attention", json!(["a", "explicit"])).unwrap();
    assert!(p.emitted_has("notification:session-attention-cleared"));
    let ev = p.emitted.lock().unwrap().iter().rev().find(|(k, _)| k == "notification:session-attention-cleared").unwrap().1.clone();
    assert_eq!(ev["intent"], "explicit");
    call(&s, "notification:clear-session-attention", json!(["a", "whatever"])).unwrap();
    let ev = p.emitted.lock().unwrap().iter().rev().find(|(k, _)| k == "notification:session-attention-cleared").unwrap().1.clone();
    assert_eq!(ev["intent"], "passive");
}

#[test]
fn notification_click_navigates_the_renderer() {
    let (s, p, _d) = rig("click");
    s.notification_clicked("s2");
    let ev = p.emitted.lock().unwrap().last().unwrap().clone();
    assert_eq!((ev.0.as_str(), ev.1["type"].clone(), ev.1["id"].clone()), ("deep-link:navigate", json!("session"), json!("s2")));
    s.notification_clicked("");
    assert_eq!(p.emitted.lock().unwrap().len(), 1);
}

#[test]
fn opening_links_and_files_is_checked_before_it_reaches_the_os() {
    let (s, p, dir) = rig("open");
    assert_eq!(call(&s, "shell:open-external", json!(["https://example.com/x"])).unwrap()["success"], true);
    let n = p.count_calls("open.url");
    for bad in ["javascript:alert(1)", "file:///etc/passwd", "mailto:a@b.com"] {
        assert_eq!(call(&s, "shell:open-external", json!([bad])).unwrap()["success"], false, "{bad}");
    }
    assert_eq!(p.count_calls("open.url"), n, "被拒的链接不会交给系统");
    assert_eq!(call(&s, "shell:open-external", json!(["mailto:xd-billing@xd.com?subject=hi"])).unwrap()["success"], true);
    assert_eq!(call(&s, "shell:open-chatgpt-app", json!([])).unwrap()["success"], true);
    assert_eq!(p.last_call("open.url").unwrap()["url"], "codex://");
    // 系统打开失败 → success:false，不抛错
    p.replies.lock().unwrap().insert("open.url".into(), Err("boom".into()));
    assert_eq!(call(&s, "shell:open-external", json!(["https://example.com"])).unwrap()["success"], false);
    p.replies.lock().unwrap().clear();

    let html = dir.join("page.html");
    std::fs::write(&html, "<p>hi</p>").unwrap();
    assert_eq!(call(&s, "shell:open-file-in-browser", json!([html.to_str().unwrap()])).unwrap()["success"], true);
    let txt = dir.join("a.txt");
    std::fs::write(&txt, "x").unwrap();
    assert_eq!(call(&s, "shell:open-file-in-browser", json!([txt.to_str().unwrap()])).unwrap()["errorCode"], "BROWSER_FILE_UNSUPPORTED_TYPE");
    assert_eq!(call(&s, "shell:open-file-in-browser", json!(["/etc/x.html"])).unwrap()["errorCode"], "BROWSER_FILE_PATH_NOT_ALLOWED");
    assert_eq!(call(&s, "shell:open-file-in-browser", json!([dir.join("none.html").to_str().unwrap()])).unwrap()["errorCode"], "BROWSER_FILE_NOT_FOUND");
    assert_eq!(call(&s, "shell:open-file-in-browser", json!(["relative.html"])).unwrap()["errorCode"], "BROWSER_FILE_INVALID_TARGET");
    // URL 打不开 → 退回按路径打开；带 #/? 状态的不退回
    p.replies.lock().unwrap().insert("open.url".into(), Err("no handler".into()));
    assert_eq!(call(&s, "shell:open-file-in-browser", json!([html.to_str().unwrap()])).unwrap()["success"], true);
    assert_eq!(p.last_call("open.path").unwrap()["path"], html.to_str().unwrap());
    let with_hash = format!("file://{}#top", html.to_str().unwrap());
    assert_eq!(call(&s, "shell:open-file-in-browser", json!([with_hash])).unwrap()["errorCode"], "BROWSER_FILE_OPEN_FAILED");

    p.replies.lock().unwrap().clear();
    assert_eq!(call(&s, "shell:show-item-in-folder", json!([{ "filePath": html.to_str().unwrap() }])).unwrap()["success"], true);
    assert_eq!(call(&s, "shell:show-item-in-folder", json!([{ "filePath": dir.join("nope").to_str().unwrap() }])).unwrap()["success"], false);
    assert_eq!(call(&s, "shell:show-item-in-folder", json!([{ "filePath": "/etc/passwd" }])).unwrap()["error"], "不允许访问该路径");
    assert_eq!(call(&s, "shell:show-item-in-folder", json!([{ "url": "xdt-image://x" }])).unwrap()["success"], false);

    p.replies.lock().unwrap().clear();
    assert_eq!(call(&s, "shell:open-path", json!([html.to_str().unwrap()])).unwrap()["success"], true);
    assert_eq!(p.last_call("open.path").unwrap()["path"], html.to_str().unwrap());
    assert_eq!(call(&s, "shell:open-path", json!([dir.to_str().unwrap()])).unwrap()["success"], true);
    assert_eq!(call(&s, "shell:open-path", json!(["rel.txt"])).unwrap()["error"], "Path must be absolute");
    assert_eq!(call(&s, "shell:open-path", json!([""])).unwrap()["success"], false);
    assert_eq!(call(&s, "shell:open-path", json!(["/etc/passwd"])).unwrap()["error"], "不允许访问该路径");
    assert_eq!(call(&s, "shell:open-path", json!([dir.join("nope").to_str().unwrap()])).unwrap()["error"], "文件不存在");
    assert_eq!(call(&s, "app:open-logs-dir", json!([])).unwrap()["success"], true);
    assert_eq!(p.last_call("open.path").unwrap()["path"], dir.join("logs").to_str().unwrap());
    assert!(dir.join("logs").is_dir());
}

#[test]
fn pickers_validate_parameters_and_wait_for_the_user() {
    let (s, p, dir) = rig("pick");
    p.replies.lock().unwrap().insert("dialog.pickFolder".into(), Ok(json!(dir.to_str().unwrap())));
    let html = dir.join("page.html");
    std::fs::write(&html, "x").unwrap();
    p.replies.lock().unwrap().insert("dialog.pickFile".into(), Ok(json!(html.to_str().unwrap())));

    assert_eq!(call(&s, "dialog:show-open-directory", json!([{}])).unwrap()["path"], dir.to_str().unwrap());
    assert_eq!(call(&s, "show-open-directory-dialog", json!([])).unwrap(), json!({ "canceled": false, "path": dir.to_str().unwrap() }));
    assert_eq!(call(&s, "dialog:show-open-resource", json!([{}])).unwrap()["kind"], "file");
    assert!(p.calls.lock().unwrap().iter().filter(|(m, _, _)| m.starts_with("dialog.pick")).all(|(_, _, wait)| *wait), "选择器不设超时");

    assert!(call(&s, "dialog:show-open-file", json!([{ "defaultPath": "rel" }])).unwrap_err().contains("INVALID_PARAMS"));
    assert!(call(&s, "dialog:show-open-directory", json!([{ "writableGrantScope": "" }])).unwrap_err().contains("INVALID_PARAMS"));
    assert!(call(&s, "dialog:show-open-file", json!([{ "filters": "x" }])).unwrap_err().contains("INVALID_PARAMS"));
    assert!(call(&s, "dialog:show-open-file", json!([{ "filters": [{ "name": "a", "extensions": ["a b"] }] }])).unwrap_err().contains("INVALID_PARAMS"));
    let picked = call(&s, "dialog:show-open-file", json!([{ "filters": [{ "name": "HTML", "extensions": ["html"] }] }])).unwrap();
    assert_eq!(picked["path"], html.to_str().unwrap());
    assert_eq!(p.last_call("dialog.pickFile").unwrap()["filters"][0]["extensions"][0], "html");

    // 取消 → path null / canceled
    p.replies.lock().unwrap().insert("dialog.pickFolder".into(), Ok(Value::Null));
    assert_eq!(call(&s, "show-open-directory-dialog", json!([])).unwrap(), json!({ "canceled": true }));
    assert_eq!(call(&s, "dialog:show-open-directory", json!([{}])).unwrap()["path"], Value::Null);
    p.replies.lock().unwrap().insert("dialog.pickFile".into(), Ok(Value::Null));
    assert_eq!(call(&s, "dialog:show-open-resource", json!([{}])).unwrap(), json!({ "success": true, "path": null, "kind": null }));
}

#[test]
fn close_behavior_persists_validates_and_decides_the_close_action() {
    let (s, _p, dir) = rig("close");
    assert_eq!(call(&s, "window-behavior:get-windows-close-behavior", json!([])).unwrap(), Value::Null);
    assert!(call(&s, "window-behavior:set-windows-close-behavior", json!(["nope"])).unwrap_err().contains("INVALID_PARAMS"));
    call(&s, "window-behavior:set-windows-close-behavior", json!(["tray"])).unwrap();
    let stored: Value = serde_json::from_str(&std::fs::read_to_string(dir.join("window-behavior-settings.json")).unwrap()).unwrap();
    assert_eq!(stored["windowsCloseBehavior"], "tray");
    assert_eq!(s.main_window_close_requested(), "quit", "非 Windows 直接退出");
    // 重启后读回
    let s2 = DesktopShell::with_platform(Arc::new(FakePorts::default()), &dir, true, false);
    assert_eq!(s2.main_window_close_requested(), "tray");
    assert_eq!(call(&s2, "window-behavior:get-windows-close-behavior", json!([])).unwrap(), "tray");

    assert!(call(&s, "window-behavior:set-linux-close-behavior", json!(["tray"])).unwrap_err().contains("INVALID_PARAMS"));
    call(&s, "window-behavior:set-linux-close-behavior", json!(["minimize"])).unwrap();
    assert_eq!(call(&s, "window-behavior:get-linux-close-behavior", json!([])).unwrap(), "minimize");
    assert!(call(&s, "window-behavior:set-swallow-activation-click", json!(["x"])).unwrap_err().contains("INVALID_PARAMS"));
    call(&s, "window-behavior:set-swallow-activation-click", json!([true])).unwrap();
    let stored: Value = serde_json::from_str(&std::fs::read_to_string(dir.join("window-behavior-settings.json")).unwrap()).unwrap();
    assert_eq!(stored, json!({ "swallowActivationClick": true, "windowsCloseBehavior": "tray", "linuxCloseBehavior": "minimize" }), "各项互不覆盖");

    // 文件里的非法值读回成默认
    std::fs::write(dir.join("window-behavior-settings.json"), r#"{"windowsCloseBehavior":"explode","linuxCloseBehavior":5}"#).unwrap();
    let s3 = DesktopShell::with_platform(Arc::new(FakePorts::default()), &dir, true, false);
    assert_eq!(call(&s3, "window-behavior:get-windows-close-behavior", json!([])).unwrap(), Value::Null);
    assert_eq!(call(&s3, "window-behavior:get-linux-close-behavior", json!([])).unwrap(), Value::Null);
}

fn wait_until(what: &str, f: impl Fn() -> bool) {
    for _ in 0..200 {
        if f() {
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!("timeout: {what}");
}

#[test]
fn first_windows_close_asks_the_renderer_then_falls_back_to_a_native_prompt() {
    let dir = temp("prompt");
    let p = Arc::new(FakePorts::default());
    p.dialog_answers.lock().unwrap().push_back(1); // 选「退出」
    let s = DesktopShell::with_platform(p.clone(), &dir, true, false);
    assert_eq!(s.main_window_close_requested_with(Duration::from_millis(40)), "none");
    assert!(p.emitted_has("window-behavior:windows-close-behavior-requested"));
    wait_until("native prompt fallback", || p.last_notified("window.closeAction").is_some());
    assert_eq!(p.last_notified("window.closeAction").unwrap()["action"], "quit");
    assert_eq!(call(&s, "window-behavior:get-windows-close-behavior", json!([])).unwrap(), "quit", "选择被保存");

    // renderer 在超时前确认显示：不弹原生框
    let dir2 = temp("prompt2");
    let p2 = Arc::new(FakePorts::default());
    let s2 = DesktopShell::with_platform(p2.clone(), &dir2, true, false);
    assert_eq!(s2.main_window_close_requested_with(Duration::from_millis(60)), "none");
    assert!(s2.send("window-behavior:windows-close-behavior-shown"));
    std::thread::sleep(Duration::from_millis(150));
    assert_eq!(p2.count_calls("dialog.message"), 0);
    assert!(p2.last_notified("window.closeAction").is_none());

    // renderer 在超时前存了选择：也不弹
    let dir3 = temp("prompt3");
    let p3 = Arc::new(FakePorts::default());
    let s3 = DesktopShell::with_platform(p3.clone(), &dir3, true, false);
    assert_eq!(s3.main_window_close_requested_with(Duration::from_millis(60)), "none");
    call(&s3, "window-behavior:set-windows-close-behavior", json!(["tray"])).unwrap();
    std::thread::sleep(Duration::from_millis(150));
    assert_eq!(p3.count_calls("dialog.message"), 0);
}

#[test]
fn tray_quit_confirms_only_when_a_turn_is_running() {
    let (s, p, _d) = rig("quit");
    assert!(s.confirm_quit_if_busy());
    assert_eq!(p.count_calls("dialog.message"), 0, "空闲时不弹确认");
    *p.busy.lock().unwrap() = true;
    p.dialog_answers.lock().unwrap().push_back(0);
    assert!(!s.confirm_quit_if_busy(), "第一个按钮是取消");
    p.dialog_answers.lock().unwrap().push_back(1);
    assert!(s.confirm_quit_if_busy());
    let dialog = p.last_call("dialog.message").unwrap();
    assert_eq!(dialog["kind"], "warning");
    assert_eq!(dialog["buttons"].as_array().unwrap().len(), 2);
    assert!(p.calls.lock().unwrap().iter().filter(|(m, _, _)| m == "dialog.message").all(|(_, _, wait)| *wait));

    p.replies.lock().unwrap().insert("open.path".into(), Err("x".into()));
    s.tray_open_logs();
    assert_eq!(p.last_call("dialog.message").unwrap()["kind"], "error", "打不开日志目录要告诉用户");
    s.tray_unavailable();
    assert_eq!(p.last_call("dialog.message").unwrap()["buttons"], json!([]));
}

#[test]
fn only_its_own_channels_are_claimed() {
    let (s, _p, _d) = rig("own");
    assert!(DesktopShell::owns("shell:open-external") && !DesktopShell::owns("terminal:create") && !DesktopShell::owns("native.trayQuit"));
    assert!(s.invoke("local-db:sessions:list", &[]).is_none());
    assert!(!s.send("window-behavior:nope"));
}
