//! pi-rs 子进程包装：拉起 `pi --mode rpc`，按行读 stdout 帧，按行写 stdin。
//!
//! 与 `pi_session_core` 配合：读线程把每个帧交给 `PiSession::on_frame`，
//! 产生的动作（回 pi-rs / 推事件）在这里执行。事件出口用 `SessionSink` 抽象，
//! 生产实现走 Tauri 事件，测试实现收集到 Vec。

#![allow(dead_code)]

use crate::pi_session_core::{Action, SessionCore};
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Instant;

/// 事件出口。三个方法对应 renderer 的三个推送通道。
pub trait SessionSink: Send + Sync + 'static {
    fn maker_event(&self, session_id: &str, event: Value);
    fn interaction_request(&self, payload: Value);
    fn interaction_dismissed(&self, payload: Value);
    /// `maker:input:projection`：队列 / 回合状态变了。
    fn projection(&self, payload: Value);
    /// pi-rs 进程退出（正常或崩溃）。
    fn exited(&self, session_id: &str, code: Option<i32>);
    /// 用户消息已派发：落库。默认不处理（测试用 sink）。
    fn user_message(&self, _session_id: &str, _client_id: &str, _content: &str) {}
}

struct Inner {
    core: SessionCore,
    stdin: Option<ChildStdin>,
    /// 权限档文件所在的运行目录；pi-rs 每次现读，改写即生效。
    runtime_dir: Option<std::path::PathBuf>,
}

pub struct PiSession {
    session_id: String,
    inner: Arc<Mutex<Inner>>,
    sink: Arc<dyn SessionSink>,
    child: Mutex<Option<Child>>,
    /// 子进程 id：spawn 时记下（child 句柄会在进程退出时被回收），诊断与测试用它区分「同一个进程」与「重建后的进程」。
    pid: u32,
    /// 读线程句柄：shutdown 等它收尾，保证 `sink.exited` 在 shutdown 返回前已跑完（热切重建时旧进程的清理不会误伤新会话）。
    reader: Mutex<Option<std::thread::JoinHandle<()>>>,
    exited: std::sync::atomic::AtomicBool,
    /// shutdown 已开始：之后的退出是我们自己关的，不报错。
    closing: std::sync::atomic::AtomicBool,
    /// 回合收尾时一次性触发的重建钩子：设置后，收尾不再派发队列里的下一条，交给钩子把队列迁到重建后的会话。
    rebuild_hook: Mutex<Option<Box<dyn FnOnce() + Send>>>,
    /// 会话工作目录：`!cmd` 在这里执行。
    cwd: std::path::PathBuf,
    /// `!cmd` 的结果，附在下一条发给模型的 prompt 前面（发出后清空）。
    bang_pending: Mutex<Vec<(String, crate::bang_cmd::Outcome)>>,
    /// 有一条 `!cmd` 正在跑：同一会话一次只跑一条，避免输出事件交错。
    bang_running: std::sync::atomic::AtomicBool,
    /// 附加目录提示（只读参考目录 + 可写目录），每轮 prompt 前面都带一份；目录变了就换成新的。
    dirs_prompt: Mutex<String>,
}

/// 退出原因的可读描述。被信号杀掉（如 macOS 的签名校验失败 SIGKILL）时没有退出码，要说出信号。
fn describe_exit(s: std::process::ExitStatus) -> String {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(sig) = s.signal() {
            return format!("killed by signal {sig}");
        }
    }
    match s.code() {
        Some(c) => format!("exit code {c}"),
        None => "unknown".to_string(),
    }
}

fn write_line(stdin: &mut ChildStdin, v: &Value) -> std::io::Result<()> {
    let mut line = serde_json::to_string(v).map_err(std::io::Error::other)?;
    line.push('\n');
    stdin.write_all(line.as_bytes())?;
    stdin.flush()
}

impl PiSession {
    pub fn spawn(
        session_id: &str,
        binary: &Path,
        args: &[String],
        env: &[(String, String)],
        cwd: &Path,
        sink: Arc<dyn SessionSink>,
    ) -> std::io::Result<Arc<Self>> {
        let mut cmd = Command::new(binary);
        cmd.args(args).current_dir(cwd).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
        for (k, v) in env {
            cmd.env(k, v);
        }
        let mut child = cmd.spawn()?;
        let pid = child.id();
        let stdin = child.stdin.take();
        let stdout = child.stdout.take().ok_or_else(|| std::io::Error::other("no stdout"))?;
        let sid = session_id.to_string();
        // pi-rs 的 stderr 转进应用日志（以前直接继承终端，退出原因查不到）。
        if let Some(stderr) = child.stderr.take() {
            let tag = sid.clone();
            let _ = std::thread::Builder::new().name(format!("pi-stderr-{sid}")).spawn(move || {
                for line in BufReader::new(stderr).lines() {
                    let Ok(line) = line else { break };
                    eprintln!("[pi:{tag}] {line}");
                }
            });
        }
        let log_sid = sid.clone();
        let core = SessionCore::new(&sid, Box::new(move |m| eprintln!("[maker:{log_sid}] {m}")));
        let inner = Arc::new(Mutex::new(Inner { core, stdin, runtime_dir: None }));
        let session = Arc::new(Self { session_id: sid.clone(), inner: inner.clone(), sink: sink.clone(), child: Mutex::new(Some(child)), pid, reader: Mutex::new(None), exited: std::sync::atomic::AtomicBool::new(false), closing: std::sync::atomic::AtomicBool::new(false), rebuild_hook: Mutex::new(None), cwd: cwd.to_path_buf(), bang_pending: Mutex::new(Vec::new()), bang_running: std::sync::atomic::AtomicBool::new(false), dirs_prompt: Mutex::new(String::new()) });

        let reader_session = session.clone();
        let handle = std::thread::Builder::new().name(format!("pi-reader-{sid}")).spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                if line.trim().is_empty() {
                    continue;
                }
                match serde_json::from_str::<Value>(&line) {
                    Ok(frame) => reader_session.handle_frame(&frame),
                    Err(e) => eprintln!("[maker:{}] bad frame: {e}", reader_session.session_id),
                }
            }
            // stdout 关闭 = 进程结束。回收退出码，并让所有悬挂的审批以拒绝收尾。
            let status = reader_session.child.lock().ok().and_then(|mut g| g.take()).and_then(|mut c| c.wait().ok());
            let code = status.and_then(|s| s.code());
            let reason = status.map(describe_exit);
            reader_session.on_exit(reason.as_deref());
            reader_session.sink.exited(&reader_session.session_id, code);
        })?;
        *session.reader.lock().unwrap() = Some(handle);
        // 看门狗：每 5 秒检查一次；持弱引用，会话释放或进程退出后自行结束。
        let weak = Arc::downgrade(&session);
        std::thread::Builder::new().name(format!("pi-watchdog-{sid}")).spawn(move || loop {
            std::thread::sleep(std::time::Duration::from_secs(5));
            let Some(s) = weak.upgrade() else { break };
            if s.exited.load(std::sync::atomic::Ordering::SeqCst) {
                break;
            }
            s.check_stall(crate::pi_session_core::STALL_TIMEOUT);
        })?;
        Ok(session)
    }

    fn handle_frame(&self, frame: &Value) {
        let actions = {
            let mut g = self.inner.lock().unwrap();
            g.core.on_frame(frame, Instant::now())
        };
        self.run(actions);
    }

    fn run(&self, actions: Vec<Action>) {
        let mut settled_projection = false;
        for a in actions {
            match a {
                Action::Projection(p) => {
                    // 回合已收尾的投影之后要接着派发下一条（队列里有的话）。
                    settled_projection = p["turnInProgress"] == json!(false);
                    self.sink.projection(p);
                }
                Action::SendToPi(mut v) => {
                    // 把 `!cmd` 的结果带给模型：只在发 prompt 时附上一次。
                    if v["type"] == "prompt" {
                        let dirs = self.dirs_prompt.lock().unwrap().clone();
                        if !dirs.is_empty() {
                            let msg = v["message"].as_str().unwrap_or("");
                            v["message"] = json!(format!("{dirs}\n\n{msg}"));
                        }
                        let runs = std::mem::take(&mut *self.bang_pending.lock().unwrap());
                        if !runs.is_empty() {
                            let msg = v["message"].as_str().unwrap_or("");
                            v["message"] = json!(format!("{}{msg}", crate::bang_cmd::context_block(&runs)));
                        }
                    }
                    let _ = self.send(&v);
                }
                Action::MakerEvent(e) => self.sink.maker_event(&self.session_id, e),
                Action::UserMessage { client_id, content } => self.sink.user_message(&self.session_id, &client_id, &content),
                Action::InteractionRequest(p) => self.sink.interaction_request(p),
                Action::InteractionDismissed(p) => self.sink.interaction_dismissed(p),
            }
        }
        if settled_projection {
            // 有待应用的换模型：不在旧进程上派发下一条，交给钩子重建并迁移队列。钩子在新线程里跑，
            // 因为重建要 shutdown 本会话，而这里是读线程，不能等自己。
            let hook = self.rebuild_hook.lock().unwrap().take();
            if let Some(h) = hook {
                std::thread::spawn(h);
                return;
            }
            let next = self.inner.lock().unwrap().core.after_settled();
            if !next.is_empty() {
                self.run(next);
            }
        }
    }

    /// 登记「回合收尾后重建」钩子（回合进行中收到的换模型请求用）。
    pub fn on_settled_rebuild(&self, f: impl FnOnce() + Send + 'static) {
        *self.rebuild_hook.lock().unwrap() = Some(Box::new(f));
    }

    /// 取走队列里待派发的项。
    pub fn take_pending(&self) -> Vec<Value> {
        self.inner.lock().unwrap().core.take_pending()
    }

    // ── 输入队列入口（对应 maker:input:* 通道）──────────────────────

    pub fn enqueue(&self, item: Value) -> Result<Value, String> {
        let actions = self.inner.lock().unwrap().core.enqueue(item)?;
        self.run(actions);
        Ok(self.projection())
    }

    pub fn projection(&self) -> Value {
        self.inner.lock().unwrap().core.projection()
    }

    /// `!cmd`：不经模型，在工作目录里直接执行。用户消息先落库，再在后台线程跑命令，结果以工具卡片事件推出。
    /// 回合进行中拒绝：此时插入工具事件会和助手输出交错。
    pub fn run_bang(self: &Arc<Self>, item: &Value, cmd: String) -> Result<Value, String> {
        use std::sync::atomic::Ordering;
        if self.inner.lock().unwrap().core.turn_in_progress() {
            return Err("[INVALID_PARAMS] 模型正在回答，等它结束后再用 ! 执行命令".to_string());
        }
        if self.bang_running.swap(true, Ordering::SeqCst) {
            return Err("[INVALID_PARAMS] 上一条 ! 命令还在执行".to_string());
        }
        let text = item["text"].as_str().unwrap_or("");
        let content = item["persistedContent"].as_str().unwrap_or(text).to_string();
        self.sink.user_message(&self.session_id, item["clientId"].as_str().unwrap_or(""), &content);
        let me = self.clone();
        let spawned = std::thread::Builder::new().name(format!("bang-{}", self.session_id)).spawn(move || {
            let out = crate::bang_cmd::run(&cmd, &me.cwd, crate::bang_cmd::TIMEOUT);
            for ev in crate::bang_cmd::events(&cmd, &out) {
                me.sink.maker_event(&me.session_id, ev);
            }
            me.bang_pending.lock().unwrap().push((cmd, out));
            me.bang_running.store(false, Ordering::SeqCst);
        });
        if let Err(e) = spawned {
            self.bang_running.store(false, Ordering::SeqCst);
            return Err(format!("[INTERNAL] 无法启动命令线程：{e}"));
        }
        Ok(self.projection())
    }

    pub fn stop(&self, keep_queue: bool, pause_queue: bool) -> Value {
        let actions = self.inner.lock().unwrap().core.stop(keep_queue, pause_queue);
        self.run(actions);
        self.projection()
    }

    pub fn resume(&self) -> Value {
        let actions = self.inner.lock().unwrap().core.resume();
        self.run(actions);
        self.projection()
    }

    pub fn remove_queued(&self, client_id: &str) -> Value {
        let actions = self.inner.lock().unwrap().core.remove_queued(client_id);
        self.run(actions);
        self.projection()
    }

    pub fn set_edit_lock(&self, client_id: &str, locked: bool) -> Value {
        let actions = self.inner.lock().unwrap().core.set_edit_lock(client_id, locked);
        self.run(actions);
        self.projection()
    }

    pub fn update_content(&self, client_id: &str, item: Value) -> Result<Value, String> {
        let actions = self.inner.lock().unwrap().core.update_content(client_id, item)?;
        self.run(actions);
        Ok(self.projection())
    }

    pub fn clear_error(&self) -> Value {
        let actions = self.inner.lock().unwrap().core.clear_error();
        self.run(actions);
        self.projection()
    }

    pub fn pid(&self) -> u32 {
        self.pid
    }

    pub fn turn_in_progress(&self) -> bool {
        self.inner.lock().unwrap().core.turn_in_progress()
    }

    /// 登记运行目录（权限档文件在里面）。工厂在 spawn 之后调用。
    pub fn set_runtime_dir(&self, dir: std::path::PathBuf) {
        self.inner.lock().unwrap().runtime_dir = Some(dir);
    }

    /// 附加目录变更：可写目录写进权限档（立即对下一次工具调用生效），提示词随之更新（下一轮起带上）。
    /// 没有运行目录（测试用假进程）时只更新提示词。
    pub fn set_dirs(&self, extra_dirs: &[String], writable_dirs: &[String]) -> std::io::Result<()> {
        *self.dirs_prompt.lock().unwrap() = crate::dir_grants::prompt(extra_dirs, writable_dirs);
        let dir = self.inner.lock().unwrap().runtime_dir.clone();
        match dir {
            Some(d) => crate::pi_launch::set_writable_roots(&d, writable_dirs),
            None => Ok(()),
        }
    }

    /// `maker:set-permission-mode`：改写权限档文件，下一次工具调用就按新档位。没有运行目录（测试用假进程）时为 no-op。
    pub fn set_permission_mode(&self, mode: &str) -> std::io::Result<()> {
        let dir = self.inner.lock().unwrap().runtime_dir.clone();
        match dir {
            Some(d) => crate::pi_launch::set_permission_mode(&d, mode),
            None => Ok(()),
        }
    }

    /// 看门狗入口（测试可传短超时）。
    pub fn check_stall(&self, timeout: std::time::Duration) {
        let actions = self.inner.lock().unwrap().core.check_stall(Instant::now(), timeout);
        if !actions.is_empty() {
            self.run(actions);
        }
    }

    fn on_exit(&self, reason: Option<&str>) {
        self.exited.store(true, std::sync::atomic::Ordering::SeqCst);
        // 我们自己关的（shutdown）不算意外；子进程已被 shutdown 取走时 reason 也是 None。
        let unexpected = if self.closing.load(std::sync::atomic::Ordering::SeqCst) { None } else { Some(reason.unwrap_or("unknown")) };
        let actions = self.inner.lock().unwrap().core.on_exit(unexpected);
        self.run(actions);
    }

    /// 写一条命令 / 应答到 pi-rs。
    pub fn send(&self, v: &Value) -> std::io::Result<()> {
        let mut g = self.inner.lock().unwrap();
        match g.stdin.as_mut() {
            Some(s) => write_line(s, v),
            None => Err(std::io::Error::new(std::io::ErrorKind::BrokenPipe, "pi-rs stdin closed")),
        }
    }

    /// `maker:resolve-interaction`。返回 accepted。
    pub fn resolve(&self, request_id: &str, decision: Value) -> bool {
        let actions = self.inner.lock().unwrap().core.resolve(request_id, decision);
        match actions {
            Some(a) => {
                self.run(a);
                true
            }
            None => false,
        }
    }

    pub fn pending(&self) -> Vec<Value> {
        let g = self.inner.lock().unwrap();
        g.core.pending(&self.session_id)
    }

    pub fn cancel_all(&self, reason: &str) {
        let actions = self.inner.lock().unwrap().core.cancel_all(reason);
        self.run(actions);
    }

    pub fn expire(&self) {
        let actions = self.inner.lock().unwrap().core.expire(Instant::now());
        self.run(actions);
    }

    /// 关闭：先拒绝所有待处理审批，再关 stdin，等进程自行退出；超时则杀掉。
    pub fn shutdown(&self) {
        self.closing.store(true, std::sync::atomic::Ordering::SeqCst);
        self.cancel_all("session_closed");
        self.inner.lock().unwrap().stdin.take();
        let child = self.child.lock().unwrap().take();
        if let Some(mut c) = child {
            let t0 = Instant::now();
            loop {
                match c.try_wait() {
                    Ok(Some(_)) => break,
                    Ok(None) if t0.elapsed().as_secs() < 3 => std::thread::sleep(std::time::Duration::from_millis(50)),
                    _ => {
                        let _ = c.kill();
                        let _ = c.wait();
                        break;
                    }
                }
            }
        }
        let h = self.reader.lock().unwrap().take();
        if let Some(h) = h {
            if h.thread().id() != std::thread::current().id() {
                let _ = h.join();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::time::Duration;

    #[derive(Default)]
    struct Collect {
        events: Mutex<Vec<Value>>,
        requests: Mutex<Vec<Value>>,
        dismissed: Mutex<Vec<Value>>,
        projections: Mutex<Vec<Value>>,
        exit: Mutex<Option<Option<i32>>>,
        tx: Mutex<Option<mpsc::Sender<()>>>,
    }
    impl Collect {
        fn ping(&self) {
            if let Some(tx) = self.tx.lock().unwrap().as_ref() {
                let _ = tx.send(());
            }
        }
    }
    impl SessionSink for Collect {
        fn maker_event(&self, _: &str, e: Value) {
            self.events.lock().unwrap().push(e);
            self.ping();
        }
        fn interaction_request(&self, p: Value) {
            self.requests.lock().unwrap().push(p);
            self.ping();
        }
        fn interaction_dismissed(&self, p: Value) {
            self.dismissed.lock().unwrap().push(p);
            self.ping();
        }
        fn projection(&self, p: Value) {
            self.projections.lock().unwrap().push(p);
            self.ping();
        }
        fn exited(&self, _: &str, code: Option<i32>) {
            *self.exit.lock().unwrap() = Some(code);
            self.ping();
        }
    }

    fn wait_until(rx: &mpsc::Receiver<()>, mut cond: impl FnMut() -> bool) -> bool {
        let t0 = Instant::now();
        while t0.elapsed() < Duration::from_secs(10) {
            if cond() {
                return true;
            }
            let _ = rx.recv_timeout(Duration::from_millis(100));
        }
        cond()
    }

    // 假 pi：先发一条权限请求，读到应答后把应答原样回显成一帧，然后退出。
    const FAKE_PI: &str = r#"
printf '%s\n' '{"type":"extension_ui_request","id":"cap1","method":"confirm","title":"cindy:turn-change-capture"}'
IFS= read -r ack
printf '%s\n' '{"type":"extension_ui_request","id":"p1","method":"input","title":"cindy:permission","placeholder":"{\"tool\":\"write\"}"}'
IFS= read -r answer
printf '%s\n' "{\"type\":\"echo\",\"ack\":$ack,\"answer\":$answer}"
"#;

    fn start(script: &str) -> (Arc<PiSession>, Arc<Collect>, mpsc::Receiver<()>) {
        let sink = Arc::new(Collect::default());
        let (tx, rx) = mpsc::channel();
        *sink.tx.lock().unwrap() = Some(tx);
        let s = PiSession::spawn("s1", Path::new("/bin/sh"), &["-c".into(), script.into()], &[], Path::new("/tmp"), sink.clone()).unwrap();
        (s, sink, rx)
    }

    #[test]
    fn permission_round_trip_over_real_pipes() {
        let (s, sink, rx) = start(FAKE_PI);
        assert!(wait_until(&rx, || !sink.requests.lock().unwrap().is_empty()), "renderer 应收到权限请求");
        let req = sink.requests.lock().unwrap()[0].clone();
        assert_eq!(req["request"]["requestId"], "p1");
        assert_eq!(s.pending().len(), 1);
        assert!(!s.resolve("nope", json!({ "kind": "permission", "behavior": "allow" })));
        assert!(s.resolve("p1", json!({ "kind": "permission", "behavior": "deny", "reason": "nope" })));
        assert!(wait_until(&rx, || sink.exit.lock().unwrap().is_some()));
        // 假 pi 把收到的两条应答回显成 echo 帧：turn-change-capture 的 ok 与权限拒绝。
        // echo 不是 AgentEvent，翻译器不产生输出；这里验证退出码与审批收尾即可。
        assert_eq!(*sink.exit.lock().unwrap(), Some(Some(0)));
        assert_eq!(sink.dismissed.lock().unwrap().len(), 1);
        assert_eq!(sink.dismissed.lock().unwrap()[0]["resolvedAs"], "deny");
        assert!(s.pending().is_empty());
    }

    #[test]
    fn process_exit_denies_pending_requests() {
        // 发出权限请求后不等应答直接退出：审批必须被收尾，不能残留在表里。
        let script = r#"printf '%s\n' '{"type":"extension_ui_request","id":"p1","title":"cindy:permission","placeholder":"{}"}'"#;
        let (s, sink, rx) = start(script);
        assert!(wait_until(&rx, || sink.exit.lock().unwrap().is_some()));
        assert!(s.pending().is_empty(), "进程退出后不留待处理审批");
        assert_eq!(sink.dismissed.lock().unwrap().len(), 1);
        assert_eq!(sink.dismissed.lock().unwrap()[0]["reason"], "session_closed");
    }

    // ── 真实 pi-rs + mock-openai 的端到端 ─────────────────────────────
    // 需要 PI_RS_BIN（pi-rs 二进制）与 MOCK_OPENAI（scripts/mock-openai.mjs 路径）和 node；任一缺失就跳过，
    // 这样普通 `cargo test` 不依赖外部构建。CI 的 pi-rs 阶段会设置它们。
    fn e2e_env() -> Option<(String, String, String)> {
        let bin = std::env::var("PI_RS_BIN").ok()?;
        let mock = std::env::var("MOCK_OPENAI").ok()?;
        let node = std::env::var("NODE_BIN").unwrap_or_else(|_| "node".into());
        Some((bin, mock, node))
    }

    fn start_mock(node: &str, mock: &str) -> (std::process::Child, u16) {
        let mut child = Command::new(node).arg(mock).stdout(Stdio::piped()).stderr(Stdio::null()).spawn().expect("spawn mock");
        let mut line = String::new();
        BufReader::new(child.stdout.as_mut().unwrap()).read_line(&mut line).unwrap();
        let port = serde_json::from_str::<Value>(&line).unwrap()["port"].as_u64().unwrap() as u16;
        (child, port)
    }

    fn e2e_session(bin: &str, port: u16, tag: &str) -> (Arc<PiSession>, Arc<Collect>, mpsc::Receiver<()>, std::path::PathBuf) {
        let tmp = std::env::temp_dir().join(format!("pi-proc-e2e-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let (agent, work, sessions) = (tmp.join("agent"), tmp.join("work"), tmp.join("sessions"));
        for d in [&agent, &work, &sessions] {
            std::fs::create_dir_all(d).unwrap();
        }
        std::fs::write(
            agent.join("models.json"),
            json!({ "providers": { "smoke": { "baseUrl": format!("http://127.0.0.1:{port}"), "api": "openai-completions",
                "apiKey": "sk-none", "models": [{ "id": "mock-model", "name": "Mock" }] } } }).to_string(),
        )
        .unwrap();
        let sink = Arc::new(Collect::default());
        let (tx, rx) = mpsc::channel();
        *sink.tx.lock().unwrap() = Some(tx);
        // 与 permission-test 一致：--extension 的 cindy-bridge 名字让 pi-rs 进入 bridge 模式（启用权限门），不带 --no-approve。
        let args: Vec<String> = ["--mode", "rpc", "--extension", "cindy-bridge.ts", "--no-skills", "--no-context-files", "--no-prompt-templates",
            "--session-dir", sessions.to_str().unwrap(), "--provider", "smoke", "--model", "mock-model"].iter().map(|s| s.to_string()).collect();
        let env = vec![("PI_CODING_AGENT_DIR".into(), agent.to_string_lossy().into()), ("PI_OFFLINE".into(), "1".into())];
        let s = PiSession::spawn("e2e", Path::new(bin), &args, &env, &work, sink.clone()).unwrap();
        (s, sink, rx, tmp)
    }

    fn has_event(sink: &Collect, ty: &str) -> bool {
        sink.events.lock().unwrap().iter().any(|e| e["type"] == ty)
    }

    #[test]
    fn real_pi_rs_streams_events_for_a_plain_prompt() {
        let Some((bin, mock, node)) = e2e_env() else { eprintln!("skip: PI_RS_BIN/MOCK_OPENAI not set"); return };
        let (mut mock_child, port) = start_mock(&node, &mock);
        let (s, sink, rx, tmp) = e2e_session(&bin, port, "plain");
        s.send(&json!({ "id": "p1", "type": "prompt", "message": "hello" })).unwrap();
        let settled = wait_until(&rx, || has_event(&sink, "done"));
        let kinds: Vec<String> = sink.events.lock().unwrap().iter().filter_map(|e| e["type"].as_str().map(String::from)).collect();
        s.shutdown();
        let _ = mock_child.kill();
        let _ = std::fs::remove_dir_all(&tmp);
        assert!(settled, "应收到 done 事件；实际事件类型: {kinds:?}");
        assert!(kinds.iter().any(|k| k == "text"), "应有 text 事件: {kinds:?}");
        assert!(kinds.iter().any(|k| k == "status"), "应有 status 事件: {kinds:?}");
    }

    #[test]
    fn real_pi_rs_permission_gate_allow_and_deny() {
        let Some((bin, mock, node)) = e2e_env() else { eprintln!("skip: PI_RS_BIN/MOCK_OPENAI not set"); return };
        let (mut mock_child, port) = start_mock(&node, &mock);
        let (s, sink, rx, tmp) = e2e_session(&bin, port, "perm");
        let work_file = tmp.join("work").join("mock-ok.txt");

        // 允许：文件被写出。
        s.send(&json!({ "id": "p1", "type": "prompt", "message": "MOCK_WRITE_FILE please" })).unwrap();
        assert!(wait_until(&rx, || !sink.requests.lock().unwrap().is_empty()), "应出现权限请求");
        let rid = sink.requests.lock().unwrap()[0]["request"]["requestId"].as_str().unwrap().to_string();
        assert!(s.resolve(&rid, json!({ "kind": "permission", "behavior": "allow" })));
        assert!(wait_until(&rx, || work_file.exists()), "允许后文件应被写出");

        // 拒绝：第二次写入不落盘。
        std::fs::remove_file(&work_file).unwrap();
        let before = sink.requests.lock().unwrap().len();
        // 等第一轮收尾再发第二轮，避免排队语义干扰本测试的目的。
        std::thread::sleep(Duration::from_millis(800));
        s.send(&json!({ "id": "p2", "type": "prompt", "message": "MOCK_WRITE_FILE again" })).unwrap();
        assert!(wait_until(&rx, || sink.requests.lock().unwrap().len() > before), "第二次也应询问");
        let rid2 = sink.requests.lock().unwrap().last().unwrap()["request"]["requestId"].as_str().unwrap().to_string();
        assert!(s.resolve(&rid2, json!({ "kind": "permission", "behavior": "deny", "reason": "no" })));
        std::thread::sleep(Duration::from_millis(1500));
        let wrote = work_file.exists();
        let saw_events = has_event(&sink, "tool_use");
        s.shutdown();
        let _ = mock_child.kill();
        let _ = std::fs::remove_dir_all(&tmp);
        assert!(!wrote, "拒绝后文件不应存在");
        assert!(saw_events, "应翻译出 tool_use 事件");
    }

    #[test]
    fn real_pi_rs_queue_runs_two_messages_serially_with_tokens() {
        let Some((bin, mock, node)) = e2e_env() else { eprintln!("skip: PI_RS_BIN/MOCK_OPENAI not set"); return };
        let (mut mock_child, port) = start_mock(&node, &mock);
        let (s, sink, rx, tmp) = e2e_session(&bin, port, "queue");
        // 两条一起入队：第二条必须等第一条收尾才派发。
        s.enqueue(json!({ "clientId": "c1", "text": "first" })).unwrap();
        let proj = s.enqueue(json!({ "clientId": "c2", "text": "second" })).unwrap();
        assert_eq!(proj["pendingQueue"].as_array().unwrap().len(), 1, "第二条在排队");
        assert_eq!(proj["turnInProgress"], true);
        let done2 = wait_until(&rx, || sink.events.lock().unwrap().iter().filter(|e| e["type"] == "done").count() >= 2);
        let evs = sink.events.lock().unwrap().clone();
        let proj_after = s.projection();
        s.shutdown();
        let _ = mock_child.kill();
        let _ = std::fs::remove_dir_all(&tmp);
        assert!(done2, "两个回合都应收尾: {:?}", evs.iter().map(|e| e["type"].clone()).collect::<Vec<_>>());
        let token_of = |ty: &str, n: usize| evs.iter().filter(|e| e["type"] == ty).nth(n).and_then(|e| e["turnAttemptToken"].as_i64());
        assert_eq!(token_of("done", 0), Some(1));
        assert_eq!(token_of("done", 1), Some(2));
        assert_eq!(proj_after["turnInProgress"], false);
        assert!(proj_after["pendingQueue"].as_array().unwrap().is_empty());
        assert!(proj_after["error"].is_null());
    }

    #[test]
    fn real_pi_rs_stop_aborts_turn_clears_queue_and_denies_pending_permission() {
        let Some((bin, mock, node)) = e2e_env() else { eprintln!("skip: PI_RS_BIN/MOCK_OPENAI not set"); return };
        let (mut mock_child, port) = start_mock(&node, &mock);
        let (s, sink, rx, tmp) = e2e_session(&bin, port, "stop");
        let work_file = tmp.join("work").join("mock-ok.txt");
        s.enqueue(json!({ "clientId": "c1", "text": "MOCK_WRITE_FILE please" })).unwrap();
        s.enqueue(json!({ "clientId": "c2", "text": "queued behind" })).unwrap();
        assert!(wait_until(&rx, || !sink.requests.lock().unwrap().is_empty()), "应卡在权限请求上");
        let proj = s.stop(false, false);
        assert_eq!(proj["turnInProgress"], false);
        assert!(proj["pendingQueue"].as_array().unwrap().is_empty(), "stop 默认清空队列");
        assert!(s.pending().is_empty(), "stop 后不留待处理审批");
        assert_eq!(sink.dismissed.lock().unwrap().len(), 1);
        assert_eq!(sink.dismissed.lock().unwrap()[0]["reason"], "session_aborted");
        std::thread::sleep(Duration::from_millis(1200));
        let wrote = work_file.exists();
        let started_second = sink.projections.lock().unwrap().iter().any(|p| p["turnInProgress"] == true && p["pendingQueue"].as_array().map(|a| a.is_empty()).unwrap_or(false) && p["turnInProgress"] == true)
            && sink.events.lock().unwrap().iter().any(|e| e["turnAttemptToken"] == 2);
        s.shutdown();
        let _ = mock_child.kill();
        let _ = std::fs::remove_dir_all(&tmp);
        assert!(!wrote, "中止后文件不应被写出");
        assert!(!started_second, "被清掉的第二条不应开始");
    }

    #[test]
    fn real_pi_rs_resumes_history_through_switch_session() {
        let Some((bin, mock, node)) = e2e_env() else { eprintln!("skip: PI_RS_BIN/MOCK_OPENAI not set"); return };
        let (mut mock_child, port) = start_mock(&node, &mock);
        let (s1, sink1, rx1, tmp) = e2e_session(&bin, port, "resume");
        s1.enqueue(json!({ "clientId": "c1", "text": "remember-me-please" })).unwrap();
        assert!(wait_until(&rx1, || has_event(&sink1, "done")), "第一轮应收尾");
        // 会话文件由 pi-rs 写在 --session-dir 下。
        let sessions = tmp.join("sessions");
        let file = std::fs::read_dir(&sessions).unwrap().filter_map(|e| e.ok()).map(|e| e.path())
            .find(|p| p.extension().is_some_and(|x| x == "jsonl")).expect("session file");
        s1.shutdown();

        // 新进程（同一 agent 目录与 session-dir）→ switch_session 续接。
        let sink2 = Arc::new(Collect::default());
        let (tx, rx2) = mpsc::channel();
        *sink2.tx.lock().unwrap() = Some(tx);
        let args: Vec<String> = ["--mode", "rpc", "--extension", "cindy-bridge.ts", "--no-skills", "--no-context-files", "--no-prompt-templates",
            "--session-dir", sessions.to_str().unwrap(), "--provider", "smoke", "--model", "mock-model"].iter().map(|s| s.to_string()).collect();
        let env = vec![("PI_CODING_AGENT_DIR".into(), tmp.join("agent").to_string_lossy().into()), ("PI_OFFLINE".into(), "1".into())];
        let s2 = PiSession::spawn("e2e2", Path::new(&bin), &args, &env, &tmp.join("work"), sink2.clone()).unwrap();
        s2.send(&json!({ "id": "resume", "type": "switch_session", "sessionPath": file.to_string_lossy() })).unwrap();
        s2.send(&json!({ "id": "msgs", "type": "get_messages" })).unwrap();
        // get_messages 的应答帧不经翻译器；直接看会话文件被新进程续写：再发一轮后文件里应有两条 user 消息。
        s2.enqueue(json!({ "clientId": "c2", "text": "second-turn" })).unwrap();
        assert!(wait_until(&rx2, || has_event(&sink2, "done")), "续接后的第二轮应收尾");
        s2.shutdown();
        let text = std::fs::read_to_string(&file).unwrap();
        let _ = mock_child.kill();
        let _ = std::fs::remove_dir_all(&tmp);
        assert!(text.contains("remember-me-please"), "第一轮历史仍在");
        assert!(text.contains("second-turn"), "第二轮续写进同一个文件");
    }

    /// `!cmd` 是否被注册表接走由 maker_registry 决定；这里直接测会话层：命令不经模型、结果成事件、下一轮 prompt 带上输出。
    #[cfg(unix)]
    #[test]
    fn bang_command_runs_without_model_and_feeds_next_turn() {
        let Some((bin, mock, node)) = e2e_env() else { eprintln!("skip: PI_RS_BIN/MOCK_OPENAI not set"); return };
        let dump = std::env::temp_dir().join(format!("pi-proc-bang-dump-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dump);
        let mut mock_child = Command::new(&node).arg(&mock).env("MOCK_DUMP", "1").env("MOCK_DUMP_DIR", &dump)
            .stdout(Stdio::piped()).stderr(Stdio::null()).spawn().expect("spawn mock");
        let mut line = String::new();
        BufReader::new(mock_child.stdout.as_mut().unwrap()).read_line(&mut line).unwrap();
        let port = serde_json::from_str::<Value>(&line).unwrap()["port"].as_u64().unwrap() as u16;
        let (s, sink, rx, tmp) = e2e_session(&bin, port, "bang");

        let proj = s.run_bang(&json!({ "clientId": "b1", "text": "!echo BANG_OUT_42" }), "echo BANG_OUT_42".to_string()).unwrap();
        assert_eq!(proj["turnInProgress"], false, "不开模型回合");
        assert!(wait_until(&rx, || has_event(&sink, "text")), "命令结果应以独立文本消息推出");
        let evs = sink.events.lock().unwrap().clone();
        let msg = evs.iter().find(|e| e["type"] == "text").unwrap();
        assert_eq!(msg["standaloneText"], true);
        assert!(msg["data"]["text"].as_str().unwrap().contains("BANG_OUT_42"));
        assert!(!has_event(&sink, "tool_use"), "不再用工具卡片（默认折叠）");
        assert!(!has_event(&sink, "done"), "没有模型回合就不会有 done");
        assert!(std::fs::read_dir(&dump).map(|d| d.count()).unwrap_or(0) == 0, "没有请求到过模型");

        // 下一轮对话：prompt 前面带着刚才的命令与输出，且只带一次。
        s.enqueue(json!({ "clientId": "c1", "text": "what was that" })).unwrap();
        assert!(wait_until(&rx, || has_event(&sink, "done")), "正常回合应收尾");
        let body = std::fs::read_dir(&dump).unwrap().filter_map(|e| e.ok())
            .map(|e| std::fs::read_to_string(e.path()).unwrap_or_default()).collect::<Vec<_>>().join("\n");
        assert!(body.contains("$ echo BANG_OUT_42") && body.contains("BANG_OUT_42") && body.contains("what was that"), "模型应看到命令和输出");
        assert!(s.bang_pending.lock().unwrap().is_empty(), "发出后清空，不重复带");

        s.shutdown();
        let _ = mock_child.kill();
        let _ = std::fs::remove_dir_all(&tmp);
        let _ = std::fs::remove_dir_all(&dump);
    }

    #[cfg(unix)]
    #[test]
    fn bang_command_is_refused_during_a_model_turn() {
        let Some((bin, mock, node)) = e2e_env() else { eprintln!("skip: PI_RS_BIN/MOCK_OPENAI not set"); return };
        let (mut mock_child, port) = start_mock(&node, &mock);
        let (s, sink, rx, tmp) = e2e_session(&bin, port, "bang-busy");
        s.enqueue(json!({ "clientId": "c1", "text": "MOCK_SLOW please" })).unwrap();
        let err = s.run_bang(&json!({ "clientId": "b1", "text": "!echo x" }), "echo x".into()).unwrap_err();
        assert!(err.contains("模型正在回答"), "{err}");
        assert!(!has_event(&sink, "text"));
        assert!(wait_until(&rx, || has_event(&sink, "done")));
        // 回合结束后可以跑，且 running 标记已复位（可以连着跑第二条）。
        for i in 0..2 {
            let before = sink.events.lock().unwrap().iter().filter(|e| e["type"] == "text").count();
            s.run_bang(&json!({ "clientId": format!("b{i}"), "text": "!true" }), "true".into()).unwrap();
            assert!(wait_until(&rx, || sink.events.lock().unwrap().iter().filter(|e| e["type"] == "text").count() > before));
            assert!(wait_until(&rx, || !s.bang_running.load(std::sync::atomic::Ordering::SeqCst)));
        }
        s.shutdown();
        let _ = mock_child.kill();
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn shutdown_closes_stdin_and_reaps_child() {
        let (s, sink, rx) = start("cat >/dev/null");
        s.shutdown();
        assert!(wait_until(&rx, || sink.exit.lock().unwrap().is_some()));
        assert!(s.send(&json!({ "type": "abort" })).is_err(), "关闭后不能再写");
    }
}
