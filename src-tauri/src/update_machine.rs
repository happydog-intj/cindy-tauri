// 自动更新状态机（原 sidecar update.mjs 的 Rust 版）：
//   idle → checking → downloading(progress) → ready ──(用户点重启 / 空闲自动重启)──→ install
//   ready 之后再检查到更新的版本 → superseding（banner 保持可见），失败回到 ready。
// 机制（check / download / install / idleSeconds）由 updater.rs（tauri-plugin-updater）提供，
// 经 UpdateBackend 注入；忙闲（会话回合中）和「空闲自动重启」开关留在 sidecar，经 HostProbe 反查。
// 行为规格 = scripts/update-e2e.mjs 的 34 步（已逐条移植为本文件底部测试）。
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

// ── 注入点 ──────────────────────────────────────────────────────────
pub trait Push: Send + Sync + 'static {
    fn push(&self, channel: &str, arg: Value);
}

/// 后端机制。check / download / install 都是阻塞调用（内部 block_on），由状态机线程发起。
pub trait UpdateBackend: Send + Sync + 'static {
    fn check(&self) -> Result<Value, String>;
    /// 下载；进度回调 received / total。失败返回 Err。
    fn download(&self, version: &str, progress: &dyn Fn(u64, u64)) -> Result<Value, String>;
    /// 安装并重启。成功时宿主进程会被替换，不会返回（测试返回 Ok）。
    fn install(&self, version: &str) -> Result<Value, String>;
    fn idle_seconds(&self) -> Result<f64, String>;
}

/// sidecar 反查：会话忙闲 / 空闲自动重启开关。
pub trait HostProbe: Send + Sync + 'static {
    fn busy(&self) -> Result<bool, ()>;
    fn auto_relaunch_enabled(&self) -> Result<bool, ()>;
}

#[derive(Clone, Copy)]
pub struct Timing {
    pub first_check_ms: u64,
    pub poll_ms: u64,
    pub auto_relaunch_poll_ms: u64,
    pub idle_threshold_seconds: f64,
    pub busy_quiet_ms: u64,
}

pub const DEFAULT_TIMING: Timing = Timing {
    first_check_ms: 10_000,
    poll_ms: 30 * 60 * 1000,
    auto_relaunch_poll_ms: 30_000,
    idle_threshold_seconds: 10.0 * 60.0,
    busy_quiet_ms: 60 * 1000,
};

/// desktop updateService.ts isVersionlessAppVersion：占位版本不参与更新。
pub fn is_versionless_app_version(version: &str) -> bool {
    version == "0.0.0" || version.starts_with("0.0.0-")
}

struct Inner {
    status: &'static str,
    ready_version: Option<String>,
    last_error_code: Option<String>,
    last_progress: Option<i64>,
    downloading_version: Option<String>,
    notes: HashMap<String, (String, Value)>,
    relaunching: bool,
    last_busy_at: Option<Instant>,
    // 在途检查的共享结果（对齐 JS：并发调用复用同一个 Promise）。
    in_flight: bool,
    last_check_result: Option<String>,
}

pub struct UpdateMachine {
    inner: Mutex<Inner>,
    cv: Condvar,
    push: Arc<dyn Push>,
    backend: Arc<dyn UpdateBackend>,
    probe: Arc<dyn HostProbe>,
    mode: &'static str,
    smoke: bool,
    timing: Timing,
    // enabled=false（off 模式 / 占位版本）时不调度计时器、不碰后端。
    enabled: bool,
    log: Mutex<Box<dyn Fn(&str) + Send + Sync>>,
    stopping: AtomicBool,
}

impl UpdateMachine {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        mode: &'static str,
        app_version: &str,
        smoke: bool,
        timing: Timing,
        push: Arc<dyn Push>,
        backend: Arc<dyn UpdateBackend>,
        probe: Arc<dyn HostProbe>,
    ) -> Arc<Self> {
        let enabled = (mode == "auto" || mode == "manual") && !is_versionless_app_version(app_version);
        Arc::new(Self {
            inner: Mutex::new(Inner {
                status: "idle",
                ready_version: None,
                last_error_code: None,
                last_progress: None,
                downloading_version: None,
                notes: HashMap::new(),
                relaunching: false,
                last_busy_at: None,
                in_flight: false,
                last_check_result: None,
            }),
            cv: Condvar::new(),
            push,
            backend,
            probe,
            mode,
            smoke,
            timing,
            enabled,
            log: Mutex::new(Box::new(|msg| eprintln!("[update] {msg}"))),
            stopping: AtomicBool::new(false),
        })
    }

    fn log(&self, msg: impl std::fmt::Display) {
        (self.log.lock().unwrap())(&msg.to_string());
    }

    // ── 状态推送 ────────────────────────────────────────────────────
    // JS setStatus(next, extra)：downloading/superseding 之外清 downloading_version，
    // 记录 errorCode/progress，推送 update-status；ready 且无错时触发空闲自动重启评估。
    fn set_status(self: &Arc<Self>, inner: &mut Inner, next: &'static str, extra: Value) {
        inner.status = next;
        if next != "downloading" && next != "superseding" {
            inner.downloading_version = None;
        }
        inner.last_error_code = extra.get("errorCode").and_then(Value::as_str).map(str::to_owned);
        inner.last_progress = extra.get("progress").and_then(Value::as_i64);
        let mut payload = json!({ "status": next });
        if let Value::Object(map) = extra {
            for (k, v) in map {
                payload[&k] = v;
            }
        }
        self.push.push("update-status", payload);
        if next == "ready" && inner.last_error_code.is_none() {
            // JS `void evaluateAutoRelaunch('status-ready')`：并发评估。
            let machine = self.clone();
            std::thread::spawn(move || machine.evaluate_auto_relaunch("status-ready"));
        }
    }

    // ── 检查 ────────────────────────────────────────────────────────
    /// update-check-now 命令。downloading/superseding → downloading；ready → ready；
    /// 其余走检查（并发时等在途检查完成并复用其结果）。
    pub fn check_now(self: &Arc<Self>) -> Value {
        {
            let inner = self.inner.lock().unwrap();
            if inner.status == "downloading" || inner.status == "superseding" {
                return json!({ "result": "downloading" });
            }
            if inner.status == "ready" {
                return json!({ "result": "ready" });
            }
        }
        json!({ "result": self.check_for_update() })
    }

    /// checkForUpdate：同一时间只跑一次；enabled=false 直接 idle，不碰后端。
    pub fn check_for_update(self: &Arc<Self>) -> String {
        if !self.enabled {
            return "idle".into();
        }
        {
            let mut inner = self.inner.lock().unwrap();
            if inner.in_flight {
                // 对齐 JS：等在途检查完成，复用同一个结果。
                let guard = self.cv.wait_while(inner, |g| g.in_flight).unwrap();
                return guard.last_check_result.clone().unwrap_or_else(|| "idle".into());
            }
            inner.in_flight = true;
        }
        let result = self.do_check();
        let mut inner = self.inner.lock().unwrap();
        inner.in_flight = false;
        inner.last_check_result = Some(result.clone());
        self.cv.notify_all();
        result
    }

    fn do_check(self: &Arc<Self>) -> String {
        let (was_ready, previous_ready) = {
            let inner = self.inner.lock().unwrap();
            let was_ready = inner.status == "ready";
            (was_ready, inner.ready_version.clone().filter(|_| was_ready))
        };
        if !was_ready {
            let mut inner = self.inner.lock().unwrap();
            self.set_status(&mut inner, "checking", json!({}));
        }
        let found = match self.backend.check() {
            Ok(v) => v,
            Err(e) => {
                self.log(format!("check failed: {e}"));
                let mut inner = self.inner.lock().unwrap();
                if was_ready {
                    self.set_status(&mut inner, "ready", json!({ "version": previous_ready }));
                    return "ready".into();
                }
                self.set_status(&mut inner, "idle", json!({}));
                return "manifest_failed".into();
            }
        };
        if !found.get("available").and_then(Value::as_bool).unwrap_or(false) {
            // Rust 只在远端版本更高时报 available；同版本 / 降级都当作已是最新。
            let mut inner = self.inner.lock().unwrap();
            if was_ready {
                self.set_status(&mut inner, "ready", json!({ "version": previous_ready }));
                return "ready".into();
            }
            self.set_status(&mut inner, "idle", json!({}));
            return "idle".into();
        }
        let version = found.get("version").and_then(Value::as_str).unwrap_or_default().to_string();
        if let Some(body) = found.get("body").and_then(Value::as_str).filter(|s| !s.trim().is_empty()) {
            self.inner.lock().unwrap().notes.insert(
                version.clone(),
                (body.to_string(), found.get("date").cloned().unwrap_or(Value::Null)),
            );
        }
        {
            let mut inner = self.inner.lock().unwrap();
            if was_ready && inner.ready_version.as_deref() == Some(version.as_str()) {
                self.set_status(&mut inner, "ready", json!({ "version": version }));
                return "ready".into();
            }
            inner.downloading_version = Some(version.clone());
            if was_ready {
                let v = previous_ready.clone();
                self.set_status(&mut inner, "superseding", json!({ "version": v, "progress": 0 }));
            } else {
                self.set_status(&mut inner, "downloading", json!({ "version": version, "progress": 0 }));
            }
        }
        match self.download_with_progress(&version, was_ready, previous_ready) {
            Ok(()) => {
                let mut inner = self.inner.lock().unwrap();
                inner.ready_version = Some(version.clone());
                self.set_status(&mut inner, "ready", json!({ "version": version }));
                "ready".into()
            }
            Err(()) => {
                if was_ready {
                    "ready".into()
                } else {
                    "download_failed".into()
                }
            }
        }
    }

    // 下载 + 进度 + 失败回退（JS doCheck 的 download 段）。
    fn download_with_progress(self: &Arc<Self>, version: &str, was_ready: bool, previous_ready: Option<String>) -> Result<(), ()> {
        let machine = self.clone();
        let progress = move |received: u64, total: u64| machine.on_progress(received, total);
        match self.backend.download(version, &progress) {
            Ok(_) => Ok(()),
            Err(e) => {
                self.log(format!("download failed: {e}"));
                let mut inner = self.inner.lock().unwrap();
                let lp = inner.last_progress.unwrap_or(0);
                self.push.push(
                    "app-update-progress",
                    json!({ "progress": lp, "received": 0, "total": 0, "failed": true, "error": "DOWNLOAD_FAILED" }),
                );
                if was_ready {
                    self.set_status(&mut inner, "ready", json!({ "version": previous_ready }));
                } else {
                    self.set_status(&mut inner, "idle", json!({}));
                }
                Err(())
            }
        }
    }

    /// Rust 下载回调（updater 的进度钩子），按整数百分比去重后推给 renderer。
    pub fn on_progress(self: &Arc<Self>, received: u64, total: u64) {
        let mut inner = self.inner.lock().unwrap();
        if inner.status != "downloading" && inner.status != "superseding" {
            return;
        }
        let pct = if total > 0 { Some((received as f64 / total as f64 * 100.0).min(100.0).floor() as i64) } else { None };
        self.push.push(
            "app-update-progress",
            json!({ "progress": pct.unwrap_or(0), "received": received, "total": total }),
        );
        if pct.is_none() || pct == inner.last_progress {
            return;
        }
        let version = if inner.status == "superseding" {
            inner.ready_version.clone()
        } else {
            inner.downloading_version.clone()
        };
        let st = inner.status;
        self.set_status(&mut inner, st, json!({ "version": version, "progress": pct }));
    }

    // ── 忙闲 / 自动重启 ─────────────────────────────────────────────
    fn is_busy(&self) -> bool {
        match self.probe.busy() {
            Ok(busy) => {
                if busy {
                    self.inner.lock().unwrap().last_busy_at = Some(Instant::now());
                }
                busy
            }
            Err(()) => {
                // desktop：忙闲探测失败按忙处理。
                self.inner.lock().unwrap().last_busy_at = Some(Instant::now());
                true
            }
        }
    }

    fn execute_relaunch(self: &Arc<Self>, reason: &str) -> bool {
        let (status, ready_version, relaunching) = {
            let inner = self.inner.lock().unwrap();
            (inner.status, inner.ready_version.clone(), inner.relaunching)
        };
        if status != "ready" || relaunching {
            return false;
        }
        self.inner.lock().unwrap().relaunching = true;
        let version = ready_version.unwrap_or_default();
        self.log(format!("relaunching to install {version} ({reason})"));
        match self.backend.install(&version) {
            Ok(_) => true,
            Err(e) => {
                self.log(format!("install failed: {e}"));
                self.inner.lock().unwrap().relaunching = false;
                let mut inner = self.inner.lock().unwrap();
                self.set_status(&mut inner, "error", json!({ "errorCode": "updater_spawn_failed", "version": version }));
                false
            }
        }
    }

    fn auto_relaunch_block_reason(self: &Arc<Self>) -> Option<String> {
        match self.probe.auto_relaunch_enabled() {
            Ok(true) => {}
            _ => return Some("disabled".into()),
        }
        {
            let inner = self.inner.lock().unwrap();
            if inner.status != "ready" {
                return Some("not-ready".into());
            }
            if inner.relaunching {
                return Some("relaunching".into());
            }
        }
        if self.is_busy() {
            return Some("busy".into());
        }
        {
            let inner = self.inner.lock().unwrap();
            if let Some(at) = inner.last_busy_at {
                if (at.elapsed().as_millis() as u64) < self.timing.busy_quiet_ms {
                    return Some("recent-busy".into());
                }
            }
        }
        let idle = match self.backend.idle_seconds() {
            Ok(v) => v,
            Err(_) => return Some("screen-state-unknown".into()),
        };
        if idle < self.timing.idle_threshold_seconds {
            return Some("user-active".into());
        }
        // 探测期间状态可能变了，落地前再看一眼。
        {
            let inner = self.inner.lock().unwrap();
            if inner.status != "ready" || inner.relaunching {
                return Some("not-ready".into());
            }
        }
        if self.is_busy() {
            return Some("not-ready".into());
        }
        None
    }

    fn evaluate_auto_relaunch(self: &Arc<Self>, reason: &str) {
        // JS：仅 auto 模式（或冒烟）评估。
        if self.mode != "auto" && !self.smoke {
            return;
        }
        if self.auto_relaunch_block_reason().is_none() {
            self.execute_relaunch(&format!("auto:{reason}"));
        }
    }

    // ── 通道 ────────────────────────────────────────────────────────
    pub fn get_status(&self) -> Value {
        let inner = self.inner.lock().unwrap();
        json!({ "status": inner.status, "version": inner.ready_version, "errorCode": inner.last_error_code })
    }

    pub fn relaunch_auto(self: &Arc<Self>) -> Value {
        let (status, relaunching) = {
            let inner = self.inner.lock().unwrap();
            (inner.status, inner.relaunching)
        };
        if status != "ready" {
            return json!({ "accepted": false, "blockReason": "not-ready" });
        }
        if relaunching {
            return json!({ "accepted": false, "blockReason": "relaunching" });
        }
        let machine = self.clone();
        std::thread::spawn(move || machine.execute_relaunch("startup"));
        json!({ "accepted": true })
    }

    pub fn relaunch_send(self: &Arc<Self>) {
        let machine = self.clone();
        std::thread::spawn(move || machine.execute_relaunch("user"));
    }

    pub fn blocking_activity(&self) -> bool {
        self.is_busy()
    }

    pub fn blocking_summary(&self) -> Value {
        let busy = self.is_busy();
        json!({
            "busy": busy,
            "categories": if busy { json!(["conversation"]) } else { json!([]) },
            "companionNames": [],
        })
    }

    pub fn release_notes(&self, version: &str) -> Value {
        let inner = self.inner.lock().unwrap();
        match inner.notes.get(version) {
            Some((body, date)) => json!({
                "version": version,
                "date": date.as_str().unwrap_or(""),
                "topics": [{ "title": format!("v{version}"), "text": body }],
            }),
            None => Value::Null,
        }
    }

    // ── 计时器驱动（生产由线程轮询；测试直接调用）────────────────────
    pub fn tick_first_check(self: &Arc<Self>) {
        if self.enabled {
            self.check_for_update();
        }
    }

    pub fn tick_poll(self: &Arc<Self>) {
        if self.enabled {
            self.check_for_update();
        }
    }

    pub fn tick_relaunch_poll(self: &Arc<Self>) {
        if self.enabled {
            self.evaluate_auto_relaunch("poll");
        }
    }

    /// 生产：起计时线程（首次检查 + 轮询 + 自动重启轮询）。测试不起，直接调 tick_*。
    pub fn start_timers(self: &Arc<Self>) -> Vec<std::thread::JoinHandle<()>> {
        let mut handles = Vec::new();
        if self.enabled && self.mode == "auto" {
            let machine = self.clone();
            let (t_first, t_poll) = (self.timing.first_check_ms, self.timing.poll_ms);
            handles.push(std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(t_first));
                machine.tick_first_check();
                loop {
                    std::thread::sleep(Duration::from_millis(t_poll));
                    if machine.stopping.load(Ordering::Relaxed) {
                        break;
                    }
                    machine.tick_poll();
                }
            }));
        }
        // 自动重启轮询：manual 模式（冒烟）也开，方便测试空闲重启；没打开设置时评估直接返回。
        if self.enabled && (self.mode == "auto" || self.smoke) {
            let machine = self.clone();
            let t = self.timing.auto_relaunch_poll_ms;
            handles.push(std::thread::spawn(move || {
                loop {
                    std::thread::sleep(Duration::from_millis(t));
                    if machine.stopping.load(Ordering::Relaxed) {
                        break;
                    }
                    machine.tick_relaunch_poll();
                }
            }));
        }
        handles
    }

    #[allow(dead_code)]
    pub fn stop(&self) {
        self.stopping.store(true, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicU64;

    // 可编程 mock：fakeRelease（update-e2e.mjs）的 Rust 版。
    struct MockBackend {
        state: Mutex<MockState>,
        checks: AtomicU64,
        downloads: AtomicU64,
        installs: Mutex<Vec<String>>,
        idles: AtomicU64,
    }

    #[derive(Default)]
    struct MockState {
        latest: Option<String>,
        check_error: Option<String>,
        download_error: Option<String>,
        install_error: Option<String>,
        idle: f64,
        check_delay_ms: u64,
    }

    impl MockBackend {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                state: Mutex::new(MockState::default()),
                checks: AtomicU64::new(0),
                downloads: AtomicU64::new(0),
                installs: Mutex::new(Vec::new()),
                idles: AtomicU64::new(0),
            })
        }
    }

    impl UpdateBackend for MockBackend {
        fn check(&self) -> Result<Value, String> {
            self.checks.fetch_add(1, Ordering::Relaxed);
            let delay = self.state.lock().unwrap().check_delay_ms;
            if delay > 0 {
                std::thread::sleep(Duration::from_millis(delay));
            }
            let st = self.state.lock().unwrap();
            if let Some(e) = &st.check_error {
                return Err(e.clone());
            }
            Ok(match &st.latest {
                Some(v) => json!({ "available": true, "version": v, "body": format!("notes for {v}"), "date": "2026-11-05 00:00:00 +00:00" }),
                None => json!({ "available": false }),
            })
        }
        fn download(&self, version: &str, progress: &dyn Fn(u64, u64)) -> Result<Value, String> {
            self.downloads.fetch_add(1, Ordering::Relaxed);
            let latest = self.state.lock().unwrap().latest.clone().unwrap_or_default();
            if version != latest {
                return Err(format!("unexpected version {version}"));
            }
            // fakeRelease：先推进度再完成 / 失败。
            for received in [0u64, 500, 1000] {
                progress(received, 1000);
            }
            let st = self.state.lock().unwrap();
            if let Some(e) = &st.download_error {
                return Err(e.clone());
            }
            Ok(json!({ "version": version, "size": 1000 }))
        }
        fn install(&self, version: &str) -> Result<Value, String> {
            let st = self.state.lock().unwrap();
            let err = st.install_error.clone();
            self.installs.lock().unwrap().push(version.to_string());
            match err {
                Some(e) => Err(e),
                None => Ok(Value::Null), // 真实 Rust 安装成功后进程被替换，不会返回。
            }
        }
        fn idle_seconds(&self) -> Result<f64, String> {
            self.idles.fetch_add(1, Ordering::Relaxed);
            Ok(self.state.lock().unwrap().idle)
        }
    }

    // 可编程 probe：busy 恒 false；autoRelaunchOnIdle 可切换。
    struct MockProbe(Mutex<bool>);
    impl HostProbe for MockProbe {
        fn busy(&self) -> Result<bool, ()> {
            Ok(false)
        }
        fn auto_relaunch_enabled(&self) -> Result<bool, ()> {
            Ok(*self.0.lock().unwrap())
        }
    }

    struct Collect(Mutex<Vec<(String, Value)>>);
    impl Push for Collect {
        fn push(&self, channel: &str, arg: Value) {
            self.0.lock().unwrap().push((channel.to_string(), arg));
        }
    }

    impl Collect {
        fn statuses(&self) -> Vec<String> {
            self.0
                .lock()
                .unwrap()
                .iter()
                .filter(|(c, _)| c == "update-status")
                .map(|(_, a)| a["status"].as_str().unwrap_or("").to_string())
                .collect()
        }
        fn last_status(&self) -> Value {
            self.0
                .lock()
                .unwrap()
                .iter()
                .rev()
                .find(|(c, _)| c == "update-status")
                .map(|(_, a)| a.clone())
                .unwrap_or(Value::Null)
        }
        fn progress_pushes(&self) -> Vec<Value> {
            self.0
                .lock()
                .unwrap()
                .iter()
                .filter(|(c, _)| c == "app-update-progress")
                .map(|(_, a)| a.clone())
                .collect()
        }
    }

    fn timing_fast() -> Timing {
        Timing {
            first_check_ms: 10_000,   // 测试手动 tick，计时器不用
            poll_ms: 10_000,
            auto_relaunch_poll_ms: 10_000,
            idle_threshold_seconds: 600.0,
            busy_quiet_ms: 60_000,
        }
    }

    fn machine(
        version: &str,
        mode: &'static str,
        backend: Arc<MockBackend>,
        probe: Arc<MockProbe>,
    ) -> (Arc<UpdateMachine>, Arc<Collect>) {
        let collect = Arc::new(Collect(Mutex::new(Vec::new())));
        let m = UpdateMachine::new(mode, version, true, timing_fast(), collect.clone(), backend, probe);
        (m, collect)
    }

    fn wait_until(pred: impl Fn() -> bool, ms: u64) -> bool {
        let end = Instant::now() + Duration::from_millis(ms);
        while !pred() {
            if Instant::now() > end {
                return false;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        true
    }

    // ── 1. 占位版本 0.0.0：完全不检查 ──
    #[test]
    fn versionless_build_reports_idle_without_checking() {
        let backend = MockBackend::new();
        let (m, _) = machine("0.0.0", "manual", backend.clone(), Arc::new(MockProbe(Mutex::new(false))));
        assert_eq!(m.check_now()["result"], "idle");
        assert_eq!(backend.checks.load(Ordering::Relaxed), 0);
    }

    // ── 2. 手动检查：无更新 / 检查失败 / 下载失败 / 下载成功 / 安装失败 ──
    #[test]
    fn manual_check_lifecycle() {
        let backend = MockBackend::new();
        let probe = Arc::new(MockProbe(Mutex::new(false)));
        let (m, collect) = machine("0.0.1", "manual", backend.clone(), probe);
        assert_eq!(m.get_status()["status"], "idle");

        // 无更新 → idle，推送 checking,idle
        assert_eq!(m.check_now()["result"], "idle");
        assert_eq!(collect.statuses(), vec!["checking", "idle"]);

        // 检查失败 → manifest_failed，状态回 idle
        backend.state.lock().unwrap().check_error = Some("network down".into());
        assert_eq!(m.check_now()["result"], "manifest_failed");
        assert_eq!(m.get_status()["status"], "idle");
        backend.state.lock().unwrap().check_error = None;

        // 下载失败 → download_failed，广播 failed 进度，状态回 idle
        backend.state.lock().unwrap().latest = Some("0.0.2".into());
        backend.state.lock().unwrap().download_error = Some("signature mismatch".into());
        assert_eq!(m.check_now()["result"], "download_failed");
        let failed = collect.progress_pushes().into_iter().find(|p| p["failed"] == true);
        assert!(failed.is_some(), "failed progress broadcast: {:?}", collect.progress_pushes());
        assert_eq!(m.get_status()["status"], "idle");
        backend.state.lock().unwrap().download_error = None;

        // 下载成功 → ready；downloading 推送带 version+progress，进度到 100
        let mark = collect.0.lock().unwrap().len();
        assert_eq!(m.check_now()["result"], "ready");
        let seq: Vec<Value> = collect
            .0
            .lock()
            .unwrap()
            .iter()
            .skip(mark)
            .filter(|(c, _)| c == "update-status")
            .map(|(_, a)| a.clone())
            .collect();
        assert!(seq.iter().any(|s| s["status"] == "downloading" && s["version"] == "0.0.2" && s["progress"] == 50), "{seq:?}");
        assert!(seq.iter().any(|s| s["status"] == "downloading" && s["progress"] == 100), "{seq:?}");
        assert_eq!(seq.last().unwrap()["status"], "ready");
        assert_eq!(seq.last().unwrap()["version"], "0.0.2");
        let st = m.get_status();
        assert_eq!(st["status"], "ready");
        assert_eq!(st["version"], "0.0.2");

        // ready 时再 check 短路，不碰后端
        let checks = backend.checks.load(Ordering::Relaxed);
        assert_eq!(m.check_now()["result"], "ready");
        assert_eq!(backend.checks.load(Ordering::Relaxed), checks);

        // release notes
        let notes = m.release_notes("0.0.2");
        assert_eq!(notes["topics"][0]["text"], "notes for 0.0.2");
        assert!(m.release_notes("9.9.9").is_null());

        // 忙闲汇总
        let summary = m.blocking_summary();
        assert_eq!(summary["busy"], false);
        assert!(summary["categories"].is_array());
        assert_eq!(m.blocking_activity(), false);

        // 自动重启默认关闭：等两个轮询周期也不应安装
        m.tick_relaunch_poll();
        m.tick_relaunch_poll();
        assert_eq!(backend.installs.lock().unwrap().len(), 0);

        // 安装失败 → updater_spawn_failed；安装目标是 ready 版本
        backend.state.lock().unwrap().install_error = Some("ShellExecuteW failed".into());
        m.relaunch_send();
        assert!(wait_until(|| m.get_status()["errorCode"] == "updater_spawn_failed", 2000), "{:?}", m.get_status());
        assert_eq!(backend.installs.lock().unwrap().last().unwrap(), "0.0.2");

        // 启动页自动重启：not-ready 拒绝
        assert_eq!(m.relaunch_auto()["accepted"], false);
    }

    // ── 3. 后台轮询：首查 → ready → superseding → ready；空闲自动重启 ──
    #[test]
    fn background_poll_and_auto_relaunch() {
        let backend = MockBackend::new();
        backend.state.lock().unwrap().latest = Some("0.0.2".into());
        backend.state.lock().unwrap().idle = 5.0;
        let probe = Arc::new(MockProbe(Mutex::new(false)));
        let (m, collect) = machine("0.0.1", "auto", backend.clone(), probe.clone());

        m.tick_first_check();
        assert!(wait_until(|| collect.statuses().iter().any(|s| s == "ready"), 2000), "{:?}", collect.statuses());

        backend.state.lock().unwrap().latest = Some("0.0.3".into());
        m.tick_poll();
        assert!(wait_until(|| collect.0.lock().unwrap().iter().any(|(_, a)| a["status"] == "superseding" && a["version"] == "0.0.2"), 2000), "superseding");
        assert!(wait_until(|| collect.last_status()["status"] == "ready" && collect.last_status()["version"] == "0.0.3", 2000), "back to ready on new version");

        // 打开空闲自动重启
        *probe.0.lock().unwrap() = true;
        m.tick_relaunch_poll();
        // 用户活跃（idle=5 < 600s）→ 只查 idle 不安装
        assert!(wait_until(|| backend.idles.load(Ordering::Relaxed) > 0, 2000), "idleSeconds queried");
        std::thread::sleep(Duration::from_millis(50));
        assert_eq!(backend.installs.lock().unwrap().len(), 0);

        // 空闲 → 自动安装最新版本
        backend.state.lock().unwrap().idle = 3600.0;
        // 轮询语义：对齐 JS 的定时器，反复 tick 直到安装。
        assert!(wait_until(|| { m.tick_relaunch_poll(); backend.installs.lock().unwrap().len() == 1 }, 3000), "auto relaunch installs");
        assert_eq!(backend.installs.lock().unwrap().last().unwrap(), "0.0.3");
    }

    // ── 4. off 模式（开发包）：不检查，接口仍可用 ──
    #[test]
    fn off_mode_stays_idle_without_native_calls() {
        let backend = MockBackend::new();
        let (m, _) = machine("0.0.1", "off", backend.clone(), Arc::new(MockProbe(Mutex::new(false))));
        assert_eq!(m.check_now()["result"], "idle");
        assert_eq!(backend.checks.load(Ordering::Relaxed), 0);
        assert_eq!(backend.downloads.load(Ordering::Relaxed), 0);
    }

    // ── 5. 在途检查去重：并发调用复用同一结果 ──
    #[test]
    fn concurrent_checks_share_result() {
        let backend = MockBackend::new();
        backend.state.lock().unwrap().latest = Some("0.0.2".into());
        let (m, _) = machine("0.0.1", "manual", backend.clone(), Arc::new(MockProbe(Mutex::new(false))));
        // check 挂起 100ms，模拟慢网络（否则在途窗口太窄，去重不触发）。
        backend.state.lock().unwrap().check_error = Some("slow".into());
        backend.state.lock().unwrap().check_delay_ms = 100;
        let m2 = m.clone();
        let t = std::thread::spawn(move || m2.check_for_update());
        // 等到第一线程真的进入 backend.check（而不是睡固定时间：CI 机器忙时 20ms 内线程可能还没起来）。
        let started = std::time::Instant::now();
        while backend.checks.load(Ordering::Relaxed) == 0 {
            assert!(started.elapsed() < Duration::from_secs(5), "第一线程没有进入 backend.check");
            std::thread::sleep(Duration::from_millis(1));
        }
        let r = m.check_for_update(); // 在途：应等到第一线程的结果
        assert_eq!(r, "manifest_failed");
        assert_eq!(t.join().unwrap(), "manifest_failed");
        assert_eq!(backend.checks.load(Ordering::Relaxed), 1, "backend.check called once");
    }
}
