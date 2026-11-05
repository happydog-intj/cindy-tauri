// 管理 Node sidecar 子进程，stdin/stdout 按行收发 JSON。
// - 请求：{"id":1,"method":"...","params":{...}}
// - 响应：{"id":1,"result":...} 或 {"id":1,"error":"..."}
// - 推送：{"push":{...}}（无 id，交给 on_push，例如转发成 renderer 事件）
// - 反向调用：{"native":{"id"?:n,"method":"...","params":{...}}}（交给 on_native；带 id 的用 reply_native 回）
// 读线程按 id 分发响应，多个请求可以同时在途，长任务不再阻塞其他调用。
use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

type Reply = Result<Value, String>;
type Pending = Arc<Mutex<HashMap<u64, mpsc::Sender<Reply>>>>;

pub struct Sidecar {
    child: Mutex<Child>,
    // shutdown 后为 None：关掉 stdin 让 sidecar 自己收尾退出。
    stdin: Mutex<Option<ChildStdin>>,
    pending: Pending,
    next_id: AtomicU64,
}

impl Sidecar {
    pub fn spawn<F, N>(node: &Path, script: &Path, env: &[(&str, String)], on_push: F, on_native: N) -> std::io::Result<Self>
    where
        F: Fn(Value) + Send + 'static,
        N: Fn(Value) + Send + 'static,
    {
        let mut cmd = Command::new(node);
        cmd.arg(script).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::inherit());
        for (k, v) in env {
            cmd.env(k, v);
        }
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            // CREATE_NO_WINDOW：不为 sidecar 弹出控制台窗口。
            cmd.creation_flags(0x0800_0000);
        }
        let mut child = cmd.spawn()?;
        let stdin = child.stdin.take().ok_or_else(|| std::io::Error::other("no stdin"))?;
        let stdout = child.stdout.take().ok_or_else(|| std::io::Error::other("no stdout"))?;
        let pending: Pending = Arc::new(Mutex::new(HashMap::new()));

        let reader_pending = pending.clone();
        thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                let Ok(msg) = serde_json::from_str::<Value>(line.trim()) else {
                    eprintln!("[sidecar] bad json line: {}", line.chars().take(200).collect::<String>());
                    continue;
                };
                if let Some(push) = msg.get("push") {
                    on_push(push.clone());
                    continue;
                }
                if let Some(native) = msg.get("native") {
                    on_native(native.clone());
                    continue;
                }
                let Some(id) = msg.get("id").and_then(Value::as_u64) else { continue };
                let tx = reader_pending.lock().ok().and_then(|mut p| p.remove(&id));
                if let Some(tx) = tx {
                    let reply = match msg.get("error") {
                        Some(err) => Err(err.as_str().map(str::to_owned).unwrap_or_else(|| err.to_string())),
                        None => Ok(msg.get("result").cloned().unwrap_or(Value::Null)),
                    };
                    let _ = tx.send(reply);
                }
            }
            // sidecar 退出：让所有在途请求立即失败。
            if let Ok(mut p) = reader_pending.lock() {
                for (_, tx) in p.drain() {
                    let _ = tx.send(Err("sidecar exited".into()));
                }
            }
        });

        Ok(Self { child: Mutex::new(child), stdin: Mutex::new(Some(stdin)), pending, next_id: AtomicU64::new(1) })
    }

    fn write_line(&self, line: &str) -> Result<(), String> {
        let mut guard = self.stdin.lock().map_err(|_| "sidecar lock poisoned".to_string())?;
        let w = guard.as_mut().ok_or_else(|| "sidecar stopped".to_string())?;
        writeln!(w, "{line}").and_then(|_| w.flush()).map_err(|e| format!("write failed: {e}"))
    }

    // 不等回复的通知（Rust → sidecar），例如更新下载进度。
    pub fn notify(&self, method: &str, params: Value) {
        let line = json!({ "method": method, "params": params }).to_string();
        if let Err(e) = self.write_line(&line) {
            eprintln!("[sidecar] notify {method} failed: {e}");
        }
    }

    // 关 stdin，等 sidecar 处理完在途请求、关数据库、杀掉终端后自己退出；超时就强杀。
    // 安装更新前调用：Windows 安装程序要覆盖 cindy-node.exe，进程还在就会写失败。
    pub fn shutdown(&self, timeout: Duration) {
        if let Ok(mut s) = self.stdin.lock() {
            s.take();
        }
        let Ok(mut c) = self.child.lock() else { return };
        let deadline = Instant::now() + timeout;
        loop {
            match c.try_wait() {
                Ok(Some(_)) => return,
                Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(50)),
                _ => break,
            }
        }
        eprintln!("[sidecar] did not exit within {timeout:?}, killing");
        let _ = c.kill();
        let _ = c.wait();
    }

    // 回复 sidecar 的反向调用。
    pub fn reply_native(&self, id: u64, reply: Result<Value, String>) {
        let body = match reply {
            Ok(result) => json!({ "nativeReply": { "id": id, "result": result } }),
            Err(error) => json!({ "nativeReply": { "id": id, "error": error } }),
        };
        if let Err(e) = self.write_line(&body.to_string()) {
            eprintln!("[sidecar] native reply failed: {e}");
        }
    }

    pub fn call(&self, method: &str, params: Value) -> Reply {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = mpsc::channel();
        self.pending.lock().map_err(|_| "sidecar lock poisoned")?.insert(id, tx);
        let line = json!({ "id": id, "method": method, "params": params }).to_string();
        if let Err(e) = self.write_line(&line) {
            if let Ok(mut p) = self.pending.lock() {
                p.remove(&id);
            }
            return Err(e);
        }
        rx.recv().map_err(|_| "sidecar closed".to_string())?
    }
}

impl Drop for Sidecar {
    fn drop(&mut self) {
        if let Ok(mut c) = self.child.lock() {
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}
