// 管理 Node sidecar 子进程：按行发送 JSON 请求，读取一行 JSON 响应。
// P1 为串行调用（一次只处理一个请求），后续再做并发与超时。
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

struct Io {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
}

pub struct Sidecar {
    io: Mutex<Io>,
    next_id: AtomicU64,
}

impl Sidecar {
    pub fn spawn(node: &Path, script: &Path, data_dir: &Path) -> std::io::Result<Self> {
        let mut cmd = Command::new(node);
        cmd.arg(script)
            .env("CINDY_TAURI_DATA_DIR", data_dir)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            // CREATE_NO_WINDOW：不为 sidecar 弹出控制台窗口。
            cmd.creation_flags(0x0800_0000);
        }
        let mut child = cmd.spawn()?;
        let stdin = child.stdin.take().ok_or_else(|| std::io::Error::other("no stdin"))?;
        let stdout = child.stdout.take().ok_or_else(|| std::io::Error::other("no stdout"))?;
        Ok(Self {
            io: Mutex::new(Io { child, stdin, stdout: BufReader::new(stdout) }),
            next_id: AtomicU64::new(1),
        })
    }

    pub fn call(&self, method: &str, params: Value) -> Result<Value, String> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let request = json!({ "id": id, "method": method, "params": params });

        let mut io = self.io.lock().map_err(|_| "sidecar lock poisoned".to_string())?;
        writeln!(io.stdin, "{request}").map_err(|e| format!("write failed: {e}"))?;
        io.stdin.flush().map_err(|e| format!("flush failed: {e}"))?;

        let mut line = String::new();
        let n = io.stdout.read_line(&mut line).map_err(|e| format!("read failed: {e}"))?;
        if n == 0 {
            return Err("sidecar closed stdout".into());
        }
        let response: Value = serde_json::from_str(line.trim()).map_err(|e| format!("bad json: {e}"))?;
        if response.get("id") != Some(&json!(id)) {
            return Err(format!("id mismatch: {response}"));
        }
        if let Some(err) = response.get("error") {
            return Err(err.to_string());
        }
        Ok(response.get("result").cloned().unwrap_or(Value::Null))
    }
}

impl Drop for Sidecar {
    fn drop(&mut self) {
        if let Ok(mut io) = self.io.lock() {
            let _ = io.child.kill();
            let _ = io.child.wait();
        }
    }
}
