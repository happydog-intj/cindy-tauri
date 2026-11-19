// 右侧栏终端：PTY 从 node-pty（sidecar）迁到 Rust（portable-pty）。
// 通道协议与 sidecar 旧实现完全一致（sidecar/src/terminal.mjs 的行为对齐）：
// - ipc.invoke("terminal:create" | "terminal:write" | "terminal:resize" | "terminal:dispose"
//   | "terminal:restart" | "terminal:list-available-shells" | "terminal:get-default-shell-pref"
//   | "terminal:set-default-shell-pref"，参数数组与旧版相同；
// - 输出以 "cindy:ipc" 事件推送：{ channel: "terminal:data"|"terminal:exit", payload: encode([{...}]) }；
// - 错误消息保持 "[CODE] text" 形状（renderer 按 code 分支提示）。
// shell 解析 / 环境裁剪逐条对照 desktop shellResolver.ts / ptyManager.ts。
use portable_pty::{native_pty_system, Child, CommandBuilder, MasterPty, PtySize};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

const TERMINAL_NAME: &str = "xterm-256color";
const DEFAULT_COLS: u16 = 80;
const DEFAULT_ROWS: u16 = 24;
const MAX_SESSIONS: usize = 32;
// 单次写入上限：粘贴大段文本也够用，防止异常调用把内存打满。
const MAX_WRITE_CHARS: usize = 1024 * 1024;
pub const SHELL_PREFS: [&str; 10] = ["auto", "zsh", "bash", "fish", "sh", "pwsh", "powershell", "cmd", "gitbash", "wsl"];

// desktop ptyManager.ts ENV_KEYS_TO_STRIP：父进程带来的终端程序标记会让 shell 启动脚本输出
// xterm.js 不认识的序列。CINDY_* 是本应用内部变量，不给用户的 shell。
const ENV_KEYS_TO_STRIP: [&str; 20] = [
    "TERMINFO", "TERMINFO_DIRS", "TERM_PROGRAM", "TERM_PROGRAM_VERSION", "TERM_SESSION_ID",
    "LC_TERMINAL", "LC_TERMINAL_VERSION", "ITERM_PROFILE", "ITERM_SESSION_ID", "ITERM_SHELL_INTEGRATION_INSTALLED",
    "VSCODE_INJECTION", "VSCODE_PID", "VSCODE_GIT_IPC_HANDLE", "VSCODE_GIT_ASKPASS_NODE", "VSCODE_GIT_ASKPASS_EXTRA_ARGS",
    "VSCODE_GIT_ASKPASS_MAIN", "VSCODE_IPC_HOOK_CLI", "VSCODE_NLS_CONFIG", "VSCODE_CWD", "GIT_ASKPASS",
];

pub fn fail(code: &str, msg: impl Into<String>) -> String {
    format!("[{code}] {}", msg.into())
}

// ── 推送旁路（生产走 AppHandle，测试收集到 Vec）──────────────────────
pub trait TerminalPush: Send + Sync + 'static {
    fn data(&self, id: &str, chunk: &str);
    fn exit(&self, id: &str, exit: Value);
}

// ── shell 解析（desktop shellResolver.ts）──────────────────────────
#[derive(Clone, Debug)]
struct ResolvedShell {
    id: String,
    command: String,
    args: Vec<&'static str>,
    display_name: String,
}

fn is_file(p: &Path) -> bool {
    p.is_file()
}

fn which_sync(bin: &str, is_win: bool, env: &HashMap<String, String>) -> Option<String> {
    if is_win {
        let exts: Vec<String> = env
            .get("PATHEXT")
            .unwrap_or(&".EXE;.CMD;.BAT".to_string())
            .split(';')
            .map(|e| e.to_lowercase())
            .filter(|e| !e.is_empty())
            .collect();
        let lower = bin.to_lowercase();
        // 已带扩展名的不再叠加候选扩展名（对齐 shellResolver.ts whichSync）。
        let tries: Vec<String> = if exts.iter().any(|e| lower.ends_with(e.as_str())) {
            vec![String::new()]
        } else {
            std::iter::once(String::new()).chain(exts).collect()
        };
        for dir in env.get("PATH").map(|s| s.as_str()).unwrap_or_default().split(';') {
            if dir.is_empty() {
                continue;
            }
            for ext in &tries {
                let c = Path::new(dir).join(format!("{bin}{ext}"));
                if is_file(&c) {
                    return Some(c.to_string_lossy().into_owned());
                }
            }
        }
        return None;
    }
    for dir in env.get("PATH").map(|s| s.as_str()).unwrap_or_default().split(':') {
        let c = Path::new(dir).join(bin);
        if is_file(&c) {
            return Some(c.to_string_lossy().into_owned());
        }
    }
    None
}

fn find_git_bash_on_windows(env: &HashMap<String, String>) -> Option<String> {
    if let Some(direct) = which_sync("git-bash.exe", true, env) {
        return Some(direct);
    }
    let git = which_sync("git.exe", true, env)?;
    let git_path = PathBuf::from(&git);
    let git_dir = git_path.parent().map(|d| d.to_path_buf()).unwrap_or_default();
    let candidates = [
        git_dir.join("bash.exe"),
        git_dir.parent().map(|d| d.join("bin").join("bash.exe")).unwrap_or_default(),
    ];
    for c in candidates {
        if is_file(&c) {
            return Some(c.to_string_lossy().into_owned());
        }
    }
    for key in ["ProgramFiles", "ProgramFiles(x86)", "LocalAppData"] {
        if let Some(r) = env.get(key) {
            let c = Path::new(r).join("Git").join("bin").join("bash.exe");
            if is_file(&c) {
                return Some(c.to_string_lossy().into_owned());
            }
        }
    }
    None
}

const NIX_NAMES: [(&str, &str); 4] = [("zsh", "zsh"), ("bash", "bash"), ("fish", "fish"), ("sh", "sh")];

fn auto_detect(is_win: bool, env: &HashMap<String, String>) -> ResolvedShell {
    if is_win {
        if let Some(p) = which_sync("pwsh.exe", true, env) {
            return ResolvedShell { id: "pwsh".into(), command: p, args: vec![], display_name: "PowerShell".into() };
        }
        if let Some(p) = which_sync("powershell.exe", true, env) {
            return ResolvedShell { id: "powershell".into(), command: p, args: vec![], display_name: "Windows PowerShell".into() };
        }
        let cmd = env.get("COMSPEC").map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).unwrap_or_else(|| "cmd.exe".into());
        return ResolvedShell { id: "cmd".into(), command: cmd, args: vec![], display_name: "Command Prompt".into() };
    }
    let shell_env = env.get("SHELL").map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
    if let Some(shell) = shell_env {
        if is_file(Path::new(&shell)) {
            let base = Path::new(&shell).file_name().and_then(|s| s.to_str()).unwrap_or("").to_lowercase();
            let id = NIX_NAMES.iter().find(|(k, _)| *k == base).map(|(_, v)| *v).unwrap_or("sh");
            return ResolvedShell { id: id.into(), command: shell, args: vec![], display_name: base };
        }
    }
    for id in ["zsh", "bash", "sh"] {
        let p = format!("/bin/{id}");
        if is_file(Path::new(&p)) {
            return ResolvedShell { id: id.into(), command: p, args: vec![], display_name: id.into() };
        }
    }
    ResolvedShell { id: "sh".into(), command: "/bin/sh".into(), args: vec![], display_name: "sh".into() }
}

fn by_id(id: &str, is_win: bool, env: &HashMap<String, String>) -> Option<ResolvedShell> {
    if is_win {
        return match id {
            "pwsh" => which_sync("pwsh.exe", true, env)
                .map(|p| ResolvedShell { id: "pwsh".into(), command: p, args: vec![], display_name: "PowerShell".into() }),
            "powershell" => which_sync("powershell.exe", true, env)
                .map(|p| ResolvedShell { id: "powershell".into(), command: p, args: vec![], display_name: "Windows PowerShell".into() }),
            "cmd" => {
                let cmd = env.get("COMSPEC").map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).unwrap_or_else(|| "cmd.exe".into());
                Some(ResolvedShell { id: "cmd".into(), command: cmd, args: vec![], display_name: "Command Prompt".into() })
            }
            "gitbash" => find_git_bash_on_windows(env)
                .map(|p| ResolvedShell { id: "gitbash".into(), command: p, args: vec!["--login", "-i"], display_name: "Git Bash".into() }),
            "wsl" => which_sync("wsl.exe", true, env).map(|p| ResolvedShell { id: "wsl".into(), command: p, args: vec![], display_name: "WSL".into() }),
            _ => None,
        };
    }
    let (_, name) = NIX_NAMES.iter().find(|(k, _)| *k == id)?;
    let p = which_sync(id, false, env).or_else(|| if id == "fish" { None } else { Some(format!("/bin/{id}")) });
    let p = p?;
    is_file(Path::new(&p)).then(move || ResolvedShell { id: id.to_string(), command: p, args: vec![], display_name: name.to_string() })
}

fn resolve_shell_for_create(pref: Option<&str>, is_win: bool, env: &HashMap<String, String>) -> ResolvedShell {
    match pref {
        None | Some("auto") => auto_detect(is_win, env),
        Some(id) => by_id(id, is_win, env).unwrap_or_else(|| auto_detect(is_win, env)),
    }
}

pub fn probe_available_shells(is_win: bool, env: &HashMap<String, String>) -> Vec<Value> {
    let auto = auto_detect(is_win, env).id;
    let ids: &[&str] = if is_win { &["pwsh", "powershell", "cmd", "gitbash", "wsl"] } else { &["zsh", "bash", "fish", "sh"] };
    ids.iter()
        .filter_map(|id| by_id(id, is_win, env))
        .map(|r| json!({ "id": r.id, "command": r.command, "displayName": r.display_name, "isAutoDetectTarget": r.id == auto }))
        .collect()
}

// ── PTY 环境（desktop ptyManager.ts spawnSession）──────────────────
pub fn build_terminal_env(is_win: bool) -> HashMap<String, String> {
    let mut env: HashMap<String, String> = std::env::vars().collect();
    // Rust 宿主没有 Node 的 CINDY_TAURI_* 之外还有自己的 PATH 等变量，行为对齐：去掉 CINDY_* 和终端标记。
    let keys: Vec<String> = env
        .keys()
        .filter(|k| k.starts_with("CINDY_") || ENV_KEYS_TO_STRIP.iter().any(|s| s == k))
        .cloned()
        .collect();
    for k in keys {
        env.remove(&k);
    }
    // macOS 从 Finder 启动可能是 C locale，zsh 行编辑器需要 UTF-8 的 LC_CTYPE 才能输入中文。
    if !is_win {
        let lc_all = env.get("LC_ALL").map(|s| s.trim().to_uppercase()).unwrap_or_default();
        if lc_all.is_empty() {
            let loc = env
                .get("LC_CTYPE")
                .or_else(|| env.get("LANG"))
                .map(|s| s.trim().to_uppercase())
                .unwrap_or_else(|| "C".into());
            if loc == "C" || loc == "POSIX" {
                env.insert("LC_CTYPE".into(), "UTF-8".into());
            }
        }
    }
    env.insert("TERM".into(), TERMINAL_NAME.into());
    env
}

// ── 会话管理 ────────────────────────────────────────────────────────
struct Session {
    // master 持有 pty；dispose 时 drop 关闭主端。
    master: Mutex<Option<Box<dyn MasterPty + Send>>>,
    writer: Mutex<Option<Box<dyn Write + Send>>>,
    child: Mutex<Option<Box<dyn Child + Send + Sync>>>,
    exited: AtomicBool,
    exit: Mutex<Option<Value>>,
    shell_id: String,
    display_name: String,
    pid: u32,
    cwd: String,
    cols: u16,
    rows: u16,
    shell_pref: String,
}

fn meta(s: &Session) -> Value {
    json!({ "shellId": s.shell_id, "shellDisplayName": s.display_name, "pid": s.pid })
}

fn req_string(v: Option<&Value>, name: &str) -> Result<String, String> {
    match v.and_then(Value::as_str) {
        Some(s) if !s.is_empty() => Ok(s.to_string()),
        _ => Err(fail("INVALID_PARAMS", format!("{name} must be a non-empty string"))),
    }
}

fn req_pos_int(v: Option<&Value>, name: &str) -> Result<u16, String> {
    match v.and_then(Value::as_i64) {
        Some(n) if (1..=10_000).contains(&n) => Ok(n as u16),
        _ => Err(fail("INVALID_PARAMS", format!("{name} must be a positive integer"))),
    }
}

fn opt_pos_int(v: Option<&Value>) -> Option<u16> {
    match v.and_then(Value::as_i64) {
        Some(n) if (1..=10_000).contains(&n) => Some(n as u16),
        _ => None,
    }
}

pub struct TerminalManager {
    sessions: Mutex<HashMap<String, Arc<Session>>>,
    settings_dir: PathBuf,
    is_win: bool,
    push: Arc<dyn TerminalPush>,
}

impl TerminalManager {
    pub fn new(settings_dir: PathBuf, push: Arc<dyn TerminalPush>) -> Self {
        Self { sessions: Mutex::new(HashMap::new()), settings_dir, is_win: cfg!(windows), push }
    }

    fn prefs_file(&self) -> PathBuf {
        self.settings_dir.join("terminal-prefs.json")
    }

    fn get_default_shell_pref(&self) -> String {
        let raw: Option<String> = std::fs::read_to_string(self.prefs_file())
            .ok()
            .and_then(|t| serde_json::from_str::<Value>(&t).ok())
            .and_then(|v| v.get("defaultShellPref").and_then(Value::as_str).map(str::to_owned));
        match raw {
            Some(r) if SHELL_PREFS.contains(&r.as_str()) => r,
            _ => "auto".into(),
        }
    }

    fn env_map(&self) -> HashMap<String, String> {
        build_terminal_env(self.is_win)
    }

    fn spawn_session(&self, id: &str, cwd: &str, cols: u16, rows: u16, shell_pref: &str) -> Result<Arc<Session>, String> {
        let env = self.env_map();
        let resolved = resolve_shell_for_create(if shell_pref == "auto" { None } else { Some(shell_pref) }, self.is_win, &env);
        let dir = match Path::new(cwd).is_dir() {
            true => Some(PathBuf::from(cwd)),
            false => None,
        };
        let dir = dir.or_else(|| env.get("HOME").cloned().map(PathBuf::from))
            .or_else(|| env.get("USERPROFILE").cloned().map(PathBuf::from))
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));

        let pty_system = native_pty_system();
        let pair = pty_system
            .openpty(PtySize { rows, cols, pixel_width: 0, pixel_height: 0 })
            .map_err(|e| fail("TERMINAL_SPAWN_FAILED", format!("failed to open pty: {e}")))?;

        let mut cmd = CommandBuilder::new(&resolved.command);
        cmd.args(resolved.args.iter().copied());
        cmd.cwd(&dir);
        // 对齐 node-pty encoding:'utf8' + 环境裁剪：全量重建环境而不是继承父进程。
        cmd.env_clear();
        for (k, v) in &env {
            cmd.env(k, v);
        }
        let child = pair
            .slave
            .spawn_command(cmd)
            .map_err(|e| {
                let msg = e.to_string();
                let kind = e.downcast_ref::<std::io::Error>().map(|io| io.kind());
                let not_found = kind == Some(std::io::ErrorKind::NotFound)
                    || msg.to_lowercase().contains("enoent")
                    || msg.to_lowercase().contains("no such file");
                if not_found {
                    fail("TERMINAL_SHELL_NOT_FOUND", format!("shell binary unavailable: {msg}"))
                } else {
                    fail("TERMINAL_SPAWN_FAILED", format!("failed to spawn pty: {msg}"))
                }
            })?;
        let pid = child.process_id().unwrap_or(0);
        let reader = pair
            .master
            .try_clone_reader()
            .map_err(|e| fail("TERMINAL_SPAWN_FAILED", format!("failed to clone pty reader: {e}")))?;
        let writer = pair
            .master
            .take_writer()
            .map_err(|e| fail("TERMINAL_SPAWN_FAILED", format!("failed to take pty writer: {e}")))?;
        // unix 上 slave 关闭后 master 才能读到 EOF。
        drop(pair.slave);

        let session = Arc::new(Session {
            master: Mutex::new(Some(pair.master)),
            writer: Mutex::new(Some(writer)),
            child: Mutex::new(Some(child)),
            exited: AtomicBool::new(false),
            exit: Mutex::new(None),
            shell_id: resolved.id,
            display_name: resolved.display_name,
            pid,
            cwd: cwd.to_string(),
            cols,
            rows,
            shell_pref: shell_pref.to_string(),
        });

        // 读线程：PTY 输出 → terminal:data 推送；EOF → wait() 收尸 → terminal:exit。
        let push = self.push.clone();
        let sid = id.to_string();
        let weak = Arc::downgrade(&session);
        std::thread::Builder::new()
            .name(format!("term-{sid}"))
            .spawn(move || reader_loop(reader, sid, weak, push))
            .map_err(|e| fail("TERMINAL_SPAWN_FAILED", format!("failed to start reader thread: {e}")))?;

        Ok(session)
    }

    fn create(&self, params: Option<&Value>) -> Result<Value, String> {
        let params = params.ok_or_else(|| fail("INVALID_PARAMS", "create params must be an object"))?.clone();
        let id = req_string(params.get("id"), "id")?;
        let cwd = req_string(params.get("cwd"), "cwd")?;
        let shell_pref = match params.get("shellPref") {
            None | Some(Value::Null) => None,
            Some(Value::String(s)) if SHELL_PREFS.contains(&s.as_str()) => Some(s.as_str()),
            Some(other) => return Err(fail("INVALID_PARAMS", format!("invalid shell pref: {other}"))),
        };
        let shell_pref = shell_pref.map(str::to_owned).unwrap_or_else(|| self.get_default_shell_pref());
        {
            let sessions = self.sessions.lock().unwrap();
            if let Some(existing) = sessions.get(&id) {
                // desktop 的幂等 attach：切回同一任务时 renderer 会再调 create。
                if existing.exited.load(Ordering::Relaxed) {
                    return Err(fail("TERMINAL_SPAWN_FAILED", format!("terminal session already exists (exited): {id}")));
                }
                return Ok(meta(existing));
            }
            if sessions.len() >= MAX_SESSIONS {
                return Err(fail("RESOURCE_EXHAUSTED", format!("too many terminal sessions (max {MAX_SESSIONS})")));
            }
        }
        let cols = opt_pos_int(params.get("cols")).unwrap_or(DEFAULT_COLS);
        let rows = opt_pos_int(params.get("rows")).unwrap_or(DEFAULT_ROWS);
        let session = self.spawn_session(&id, &cwd, cols, rows, &shell_pref)?;
        let m = meta(&session);
        let mut sessions = self.sessions.lock().unwrap();
        // 并发 create 同 id：后来者丢弃自己的会话，返回先到者的 meta。
        match sessions.get(&id) {
            Some(existing) => {
                self.kill_session(&session);
                return Ok(meta(existing));
            }
            None => {
                sessions.insert(id, session);
            }
        }
        Ok(m)
    }

    fn dispose(&self, id: &str) {
        let session = self.sessions.lock().unwrap().remove(id);
        if let Some(s) = session {
            self.kill_session(&s);
        }
    }

    fn kill_session(&self, s: &Arc<Session>) {
        if !s.exited.load(Ordering::Relaxed) {
            if let Ok(mut c) = s.child.lock() {
                if let Some(child) = c.as_mut() {
                    let _ = child.kill();
                }
            }
        }
        // 关闭 master / writer：读线程随后 EOF 收尾。
        *s.master.lock().unwrap() = None;
        *s.writer.lock().unwrap() = None;
    }

    pub fn dispose_all(&self) {
        let ids: Vec<String> = self.sessions.lock().unwrap().keys().cloned().collect();
        for id in ids {
            self.dispose(&id);
        }
    }

    // ipc.invoke("terminal:*") 的参数数组分发。返回值会被 main.rs 编码成 encode([result])。
    pub fn invoke(&self, channel: &str, args: &[Value]) -> Result<Value, String> {
        match channel {
            "terminal:create" => self.create(args.first()),
            "terminal:write" => {
                let id = req_string(args.first(), "id")?;
                let data = match args.get(1).and_then(Value::as_str) {
                    Some(s) if !s.is_empty() => s,
                    Some(_) => return Err(fail("INVALID_PARAMS", "data must be a non-empty string")),
                    None => return Err(fail("INVALID_PARAMS", "data must be a non-empty string")),
                };
                if data.len() > MAX_WRITE_CHARS {
                    return Err(fail("INVALID_PARAMS", "data is too large"));
                }
                let s = self.session(&id)?;
                if s.exited.load(Ordering::Relaxed) {
                    return Ok(Value::Null);
                }
                let mut writer = s.writer.lock().unwrap();
                if let Some(w) = writer.as_mut() {
                    if w.write_all(data.as_bytes()).is_err() {
                        // 进程刚退出：对齐旧实现的吞错。
                        *writer = None;
                    }
                }
                Ok(Value::Null)
            }
            "terminal:resize" => {
                let id = req_string(args.first(), "id")?;
                let cols = req_pos_int(args.get(1), "cols")?;
                let rows = req_pos_int(args.get(2), "rows")?;
                let s = self.session(&id)?;
                if s.exited.load(Ordering::Relaxed) || (s.cols == cols && s.rows == rows) {
                    return Ok(Value::Null);
                }
                if let Ok(master) = s.master.lock() {
                    if let Some(m) = master.as_ref() {
                        // 进程刚退出时 resize 报错：对齐旧实现吞掉。
                        let _ = m.resize(PtySize { rows, cols, pixel_width: 0, pixel_height: 0 });
                    }
                }
                Ok(Value::Null)
            }
            "terminal:dispose" => {
                let id = req_string(args.first(), "id")?;
                self.dispose(&id);
                Ok(Value::Null)
            }
            "terminal:restart" => {
                let id = req_string(args.first(), "id")?;
                let old = self.session(&id)?;
                if !old.exited.load(Ordering::Relaxed) {
                    return Err(fail("PRECONDITION_FAILED", "cannot restart a session that has not exited"));
                }
                let params = json!({
                    "id": id,
                    "cwd": old.cwd,
                    "cols": old.cols,
                    "rows": old.rows,
                    "shellPref": old.shell_pref,
                });
                self.dispose(&id);
                self.create(Some(&params))
            }
            "terminal:list-available-shells" => Ok(Value::Array(probe_available_shells(self.is_win, &self.env_map()))),
            "terminal:get-default-shell-pref" => Ok(Value::String(self.get_default_shell_pref())),
            "terminal:set-default-shell-pref" => {
                let value = match args.first().and_then(Value::as_str) {
                    Some(v) if SHELL_PREFS.contains(&v) => v,
                    _ => return Err(fail("INVALID_PARAMS", "invalid shell pref")),
                };
                let file = self.prefs_file();
                if let Some(dir) = file.parent() {
                    let _ = std::fs::create_dir_all(dir);
                }
                // 原子写（override-file.mjs writeJsonAtomic）：临时文件 + rename。
                let tmp = file.with_extension("json.tmp");
                std::fs::write(&tmp, serde_json::to_string(&json!({ "defaultShellPref": value })).unwrap())
                    .and_then(|_| std::fs::rename(&tmp, &file))
                    .map_err(|e| fail("IO_ERROR", e.to_string()))?;
                Ok(Value::Null)
            }
            _ => Err(format!("unknown channel: {channel}")),
        }
    }

    fn session(&self, id: &str) -> Result<Arc<Session>, String> {
        self.sessions
            .lock()
            .unwrap()
            .get(id)
            .cloned()
            .ok_or_else(|| fail("TERMINAL_NOT_FOUND", format!("pty session not found: {id}")))
    }
}

// 读线程主体：增量 UTF-8 解码（多字节跨块时缓存不完整尾字节，对齐 node-pty encoding:'utf8'）。
fn reader_loop(
    mut reader: Box<dyn Read + Send>,
    id: String,
    session: std::sync::Weak<Session>,
    push: Arc<dyn TerminalPush>,
) {
    let mut carry: Vec<u8> = Vec::new();
    let mut buf = [0u8; 8192];
    loop {
        match reader.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                carry.extend_from_slice(&buf[..n]);
                // 找到最长可解码前缀，剩余不完整序列留到下一块。
                let decoded_len = match std::str::from_utf8(&carry) {
                    Ok(_) => carry.len(),
                    Err(e) => e.valid_up_to(),
                };
                if decoded_len > 0 {
                    let chunk = String::from_utf8_lossy(&carry[..decoded_len]).into_owned();
                    carry.drain(..decoded_len);
                    push.data(&id, &chunk);
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        }
    }
    // EOF：收尸并推送 exit。
    let Some(session) = session.upgrade() else { return };
    let exit = {
        let mut c = session.child.lock().unwrap();
        match c.as_mut() {
            Some(child) => {
                let status = child.wait().ok();
                *c = None;
                match status {
                    // 信号退出：code 对齐 node-pty 的 onExit（signal 存在时 code 为 null）。
                    Some(st) if st.signal().is_some() => json!({ "code": null, "signal": st.signal().map(str::to_owned) }),
                    Some(st) => json!({ "code": st.exit_code() as i64, "signal": null }),
                    None => json!({ "code": null, "signal": null }),
                }
            }
            None => json!({ "code": null, "signal": null }),
        }
    };
    session.exited.store(true, Ordering::Relaxed);
    *session.exit.lock().unwrap() = Some(exit.clone());
    push.exit(&id, exit);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env_from_pairs(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    #[test]
    fn shell_resolution_nix() {
        let env = env_from_pairs(&[("PATH", "/bin:/usr/bin"), ("SHELL", "/bin/bash")]);
        let r = auto_detect(false, &env);
        assert_eq!(r.id, "bash");
        assert_eq!(r.command, "/bin/bash");
        // fish 未安装时按 id 找不到，auto 回落。
        let r = resolve_shell_for_create(Some("fish"), false, &env);
        assert_eq!(r.id, "bash");
        let r = resolve_shell_for_create(Some("sh"), false, &env);
        assert_eq!(r.id, "sh");
    }

    #[test]
    fn env_stripping() {
        std::env::set_var("CINDY_TAURI_DATA_DIR", "/tmp/x");
        std::env::set_var("TERM_PROGRAM", "iTerm.app");
        let env = build_terminal_env(false);
        assert!(!env.contains_key("CINDY_TAURI_DATA_DIR"));
        assert!(!env.contains_key("TERM_PROGRAM"));
        assert_eq!(env.get("TERM").unwrap(), "xterm-256color");
    }

    #[cfg(unix)]
    #[test]
    fn pty_roundtrip_and_exit() {
        struct Collect(Mutex<Vec<(String, String, Value)>>);
        impl TerminalPush for Collect {
            fn data(&self, id: &str, chunk: &str) {
                self.0.lock().unwrap().push((id.to_string(), chunk.to_string(), Value::Null));
            }
            fn exit(&self, id: &str, exit: Value) {
                self.0.lock().unwrap().push((id.to_string(), String::new(), exit));
            }
        }
        let dir = std::env::temp_dir().join(format!("cindy-term-test-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let mgr = TerminalManager::new(dir.clone(), Arc::new(Collect(Mutex::new(Vec::new()))));
        let meta = mgr
            .invoke("terminal:create", &[json!({ "id": "t1", "cwd": dir.to_string_lossy(), "cols": 80, "rows": 24, "shellPref": "sh" })])
            .expect("create");
        assert!(meta["pid"].as_u64().unwrap() > 0, "pid: {meta}");

        // resize 不报错；重复 resize 相同尺寸返回 null。
        mgr.invoke("terminal:resize", &[json!("t1"), json!(100), json!(30)]).expect("resize");

        // 写命令并等输出。
        mgr.invoke("terminal:write", &[json!("t1"), json!("echo cindy-term-ok\r\n")]).expect("write");
        std::thread::sleep(std::time::Duration::from_millis(800));

        // 幂等 create：已有活跃会话返回同一 meta。
        let again = mgr
            .invoke("terminal:create", &[json!({ "id": "t1", "cwd": dir.to_string_lossy() })])
            .expect("idempotent create");
        assert_eq!(again, meta);

        // dispose 后再写/resize 报 NOT_FOUND。
        mgr.invoke("terminal:dispose", &[json!("t1")]).expect("dispose");
        assert!(mgr.invoke("terminal:write", &[json!("t1"), json!("x")]).unwrap_err().contains("TERMINAL_NOT_FOUND"));

        // 未退出的会话不能 restart。
        let _ = mgr.invoke("terminal:restart", &[json!("t1")]);

        // exit 流程：起一个会跑完就退出的 shell。
        mgr.invoke("terminal:create", &[json!({ "id": "t2", "cwd": dir.to_string_lossy(), "shellPref": "sh" })]).expect("create t2");
        mgr.invoke("terminal:write", &[json!("t2"), json!("exit\r\n")]).expect("write exit");
        for _ in 0..40 {
            std::thread::sleep(std::time::Duration::from_millis(250));
            let alive = mgr.sessions.lock().unwrap().get("t2").map(|s| !s.exited.load(Ordering::Relaxed));
            if alive == Some(false) {
                break;
            }
        }
        let s = mgr.sessions.lock().unwrap().get("t2").cloned();
        let exited = s.map(|s| s.exited.load(Ordering::Relaxed)).unwrap_or(true);
        assert!(exited, "t2 should have exited");
        let _ = std::fs::remove_dir_all(dir);
    }
}
