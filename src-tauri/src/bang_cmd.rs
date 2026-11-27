//! 输入框以 `!` 开头的消息：直接在会话工作目录执行 shell 命令，不经过模型（与 Claude Code 的 bash 模式一致）。
//!
//! 这里只放与会话无关的部分：识别、执行、输出截断、拼出给渲染层的事件。接入点在 `pi_proc.rs` 的 `enqueue`。
//! 命令是用户亲手输入的，所以不走权限确认；输出会写进对话（工具卡片），并在下一轮对话时附在模型的提示前面，
//! 这样用户可以接着问「刚才那个报错是什么意思」。

use serde_json::{json, Value};
use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// 单条命令最长执行时间。超时会杀掉整个进程。
pub const TIMEOUT: Duration = Duration::from_secs(120);
/// 输出最多保留的字节数（保留尾部，错误信息通常在最后）。
pub const MAX_OUTPUT_BYTES: usize = 64 * 1024;
/// 带给模型的上下文里，每条命令输出最多这么多字节。
pub const MAX_CONTEXT_BYTES: usize = 16 * 1024;

/// 识别 `!cmd`。只有 `!` 后面有非空白内容才算；`!` 前允许空白。`!!`、`! ` 之类不算（避免误伤普通文本）。
pub fn parse(text: &str) -> Option<&str> {
    let rest = text.trim_start().strip_prefix('!')?;
    let cmd = rest.trim();
    if cmd.is_empty() || rest.starts_with('!') {
        return None;
    }
    Some(cmd)
}

#[derive(Debug, Clone, PartialEq)]
pub struct Outcome {
    /// stdout 与 stderr 合并后的文本（已截断）。
    pub output: String,
    /// 退出码；超时或被信号杀掉时为 None。
    pub code: Option<i32>,
    pub timed_out: bool,
    pub truncated: bool,
}

impl Outcome {
    pub fn is_error(&self) -> bool {
        self.timed_out || self.code != Some(0)
    }

    /// 给渲染层 / 模型看的完整文本：输出，加上失败时的原因。
    pub fn display_text(&self) -> String {
        let mut s = self.output.trim_end().to_string();
        if self.truncated {
            s = format!("[输出过长，只保留末尾部分]\n{s}");
        }
        let tail = if self.timed_out {
            format!("命令超过 {} 秒未结束，已终止", TIMEOUT.as_secs())
        } else {
            match self.code {
                Some(0) => String::new(),
                Some(c) => format!("退出码 {c}"),
                None => "命令被信号终止".to_string(),
            }
        };
        match (s.is_empty(), tail.is_empty()) {
            (_, true) if s.is_empty() => "（无输出）".to_string(),
            (_, true) => s,
            (true, false) => tail,
            (false, false) => format!("{s}\n\n{tail}"),
        }
    }
}

fn shell_command(cmd: &str) -> Command {
    if cfg!(windows) {
        let mut c = Command::new("cmd");
        c.args(["/C", cmd]);
        c
    } else {
        // 用户的登录 shell 会读 rc 文件、很慢；这里与 pi-rs 的 bash 工具一致，直接用 bash -c。
        let mut c = Command::new("bash");
        c.args(["-c", cmd]);
        c
    }
}

/// 保留尾部不超过 `max` 字节，并落在字符边界上。
fn tail_of(mut text: String, max: usize) -> (String, bool) {
    if text.len() <= max {
        return (text, false);
    }
    let mut cut = text.len() - max;
    while !text.is_char_boundary(cut) {
        cut += 1;
    }
    (text.split_off(cut), true)
}

/// 同步执行，阻塞到结束或超时。stdout 和 stderr 合并（先 stdout 后 stderr，与 pi-rs 的 bash 工具一致）。
pub fn run(cmd: &str, cwd: &Path, timeout: Duration) -> Outcome {
    let mut c = shell_command(cmd);
    c.current_dir(cwd).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = match c.spawn() {
        Ok(ch) => ch,
        Err(e) => return Outcome { output: format!("无法启动 shell：{e}"), code: None, timed_out: false, truncated: false },
    };
    let mut so = child.stdout.take();
    let mut se = child.stderr.take();
    let t_out = std::thread::spawn(move || {
        let mut b = Vec::new();
        if let Some(s) = so.as_mut() {
            let _ = s.read_to_end(&mut b);
        }
        b
    });
    let t_err = std::thread::spawn(move || {
        let mut b = Vec::new();
        if let Some(s) = se.as_mut() {
            let _ = s.read_to_end(&mut b);
        }
        b
    });
    let start = Instant::now();
    let (code, timed_out) = loop {
        match child.try_wait() {
            Ok(Some(st)) => break (st.code(), false),
            Ok(None) if start.elapsed() >= timeout => {
                let _ = child.kill();
                let _ = child.wait();
                break (None, true);
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(20)),
            Err(_) => break (None, false),
        }
    };
    let mut raw = t_out.join().unwrap_or_default();
    raw.extend(t_err.join().unwrap_or_default());
    let (output, truncated) = tail_of(String::from_utf8_lossy(&raw).into_owned(), MAX_OUTPUT_BYTES);
    Outcome { output, code, timed_out, truncated }
}

/// 渲染层事件：一次工具调用 + 它的结果。形状与 pi-rs 翻译 `tool_execution_*` 得到的一致，
/// 所以工具卡片、历史回放和落库都沿用现有路径。`tool_use_id` 要在会话内唯一。
pub fn events(tool_use_id: &str, cmd: &str, out: &Outcome) -> Vec<Value> {
    vec![
        json!({ "type": "tool_use", "data": { "toolUseId": tool_use_id, "toolName": "bash", "input": { "command": cmd } }, "source": "pi" }),
        json!({ "type": "tool_result_full", "data": { "toolUseId": tool_use_id, "fullText": out.display_text(), "isError": out.is_error() }, "source": "pi" }),
        json!({ "type": "tool_result", "data": { "summary": if out.is_error() { "failed" } else { "done" }, "toolUseIds": [tool_use_id] }, "source": "pi" }),
    ]
}

/// 附在下一条发给模型的消息前面的上下文。多条命令按顺序排列。
pub fn context_block(runs: &[(String, Outcome)]) -> String {
    let mut s = String::from("用户在两次对话之间直接在终端里执行了下面的命令（不是你调用的），结果如下：\n");
    for (cmd, out) in runs {
        let (text, cut) = tail_of(out.display_text(), MAX_CONTEXT_BYTES);
        s.push_str(&format!("\n$ {cmd}\n{}{text}\n", if cut { "[已截断]\n" } else { "" }));
    }
    s.push_str("\n---\n");
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_recognizes_bang_commands_only() {
        assert_eq!(parse("!ls -la"), Some("ls -la"));
        assert_eq!(parse("  !  git status  "), Some("git status"));
        assert_eq!(parse("!"), None);
        assert_eq!(parse("!   "), None);
        assert_eq!(parse("!!"), None);
        assert_eq!(parse("hello !ls"), None);
        assert_eq!(parse("ls"), None);
        assert_eq!(parse(""), None);
        // 多行命令整体保留
        assert_eq!(parse("!echo a\necho b"), Some("echo a\necho b"));
    }

    #[cfg(unix)]
    #[test]
    fn run_captures_output_exit_code_and_cwd() {
        let dir = std::env::temp_dir().join(format!("bang-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let ok = run("echo hi; pwd", &dir, Duration::from_secs(5));
        assert_eq!(ok.code, Some(0));
        assert!(ok.output.starts_with("hi\n"), "{:?}", ok.output);
        assert!(ok.output.contains(dir.file_name().unwrap().to_str().unwrap()), "{:?}", ok.output);
        assert!(!ok.is_error());

        let bad = run("echo oops >&2; exit 3", &dir, Duration::from_secs(5));
        assert_eq!(bad.code, Some(3));
        assert!(bad.is_error());
        assert!(bad.display_text().contains("oops") && bad.display_text().ends_with("退出码 3"), "{}", bad.display_text());

        let empty = run("true", &dir, Duration::from_secs(5));
        assert_eq!(empty.display_text(), "（无输出）");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn run_kills_on_timeout_and_truncates_long_output() {
        let t = run("echo before; sleep 5", Path::new("/"), Duration::from_millis(200));
        assert!(t.timed_out && t.is_error());
        assert!(t.output.contains("before"));
        assert!(t.display_text().contains("已终止"));

        let long = run("yes 0123456789 | head -c 200000", Path::new("/"), Duration::from_secs(10));
        assert!(long.truncated);
        assert!(long.output.len() <= MAX_OUTPUT_BYTES);
        assert!(long.display_text().starts_with("[输出过长"));
    }

    #[test]
    fn spawn_failure_is_reported_not_panicked() {
        let o = run("echo x", Path::new("/definitely/not/a/dir"), Duration::from_secs(1));
        assert!(o.is_error());
        assert!(o.output.contains("无法启动"), "{:?}", o.output);
    }

    #[test]
    fn events_have_the_shape_the_renderer_expects() {
        let out = Outcome { output: "a\n".into(), code: Some(0), timed_out: false, truncated: false };
        let ev = events("bang-1", "ls", &out);
        assert_eq!(ev.len(), 3);
        assert_eq!(ev[0]["type"], "tool_use");
        assert_eq!(ev[0]["data"]["input"]["command"], "ls");
        assert_eq!(ev[1]["type"], "tool_result_full");
        assert_eq!(ev[1]["data"]["isError"], false);
        assert_eq!(ev[2]["data"]["toolUseIds"][0], "bang-1");
    }

    #[test]
    fn context_block_lists_commands_in_order_and_caps_each() {
        let big = Outcome { output: "x".repeat(MAX_CONTEXT_BYTES * 2), code: Some(0), timed_out: false, truncated: false };
        let small = Outcome { output: "ok".into(), code: Some(1), timed_out: false, truncated: false };
        let s = context_block(&[("first".into(), big), ("second".into(), small)]);
        assert!(s.find("$ first").unwrap() < s.find("$ second").unwrap());
        assert!(s.contains("[已截断]") && s.contains("退出码 1"));
        assert!(s.len() < MAX_CONTEXT_BYTES + 1024, "{}", s.len());
        assert!(s.ends_with("---\n"));
    }
}
