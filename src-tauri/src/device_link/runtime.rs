//! 引擎的 IO 外壳：一个 actor 任务独占 `Engine`，socket / 令牌 / 定时器都在引擎外。
//! 输入全部经 `Cmd` 通道串行进入，输出（`Out`）按顺序执行。
use super::engine::{Engine, Env, Issue, Out, Status, Timing};
use futures_util::{SinkExt, StreamExt};
use serde_json::Value;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::sync::{mpsc, oneshot};
use tokio_tungstenite::tungstenite::protocol::{frame::coding::CloseCode, CloseFrame, Message};

pub type BoxFut<T> = Pin<Box<dyn Future<Output = T> + Send>>;

/// 宿主给运行时的回调。
pub trait Hooks: Send + Sync + 'static {
    /// 取 access token；None = 未登录或取不到。
    fn token(&self) -> BoxFut<Option<String>>;
    fn ws_url(&self) -> String;
    fn log(&self, level: &str, msg: &str);
    /// 引擎交给应用层的入站帧。
    fn on_app(&self, h: &Handle, env: Env);
    fn on_status(&self, status: Status);
    fn on_issue(&self, issue: Option<Issue>);
    fn on_presence(&self, snapshot: Value);
}

enum Cmd {
    With(Box<dyn FnOnce(&mut Engine) + Send>),
    Token(u64, Option<String>),
    SocketOpen(u64),
    SocketMsg(u64, String),
    SocketClosed(u64, Option<u16>, String),
    OpenFailed(u64, String),
}

#[derive(Clone)]
pub struct Handle {
    tx: mpsc::UnboundedSender<Cmd>,
}

impl Handle {
    /// 在 actor 内对引擎做一次操作并取回结果。
    pub async fn with<R: Send + 'static>(&self, f: impl FnOnce(&mut Engine) -> R + Send + 'static) -> Option<R> {
        let (tx, rx) = oneshot::channel();
        self.tx.send(Cmd::With(Box::new(move |e| {
            let _ = tx.send(f(e));
        }))).ok()?;
        rx.await.ok()
    }
    /// 不等结果。
    pub fn run(&self, f: impl FnOnce(&mut Engine) + Send + 'static) {
        let _ = self.tx.send(Cmd::With(Box::new(f)));
    }
}

struct Sock {
    epoch: u64,
    tx: mpsc::UnboundedSender<WsOut>,
    task: tauri::async_runtime::JoinHandle<()>,
}

enum WsOut {
    Text(String),
    Close(u16, String),
}

fn ensure_tls_provider() {
    if rustls::crypto::CryptoProvider::get_default().is_none() {
        let _ = rustls::crypto::ring::default_provider().install_default();
    }
}

pub fn spawn(hooks: Arc<dyn Hooks>, hello: Value, timing: Timing) -> Handle {
    ensure_tls_provider();
    let (tx, rx) = mpsc::unbounded_channel();
    let handle = Handle { tx };
    let h2 = handle.clone();
    tauri::async_runtime::spawn(async move { actor(hooks, hello, timing, h2, rx).await });
    handle
}

fn clocks(origin: Instant) -> (u64, u64) {
    let wall = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0);
    (origin.elapsed().as_millis() as u64 + 1, wall)
}

async fn actor(hooks: Arc<dyn Hooks>, hello: Value, timing: Timing, handle: Handle, mut rx: mpsc::UnboundedReceiver<Cmd>) {
    let origin = Instant::now();
    let seed = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_nanos() as u64).unwrap_or(1);
    let mut engine = Engine::new(timing, seed);
    engine.set_hello(hello);
    let mut sock: Option<Sock> = None;
    let buffered = Arc::new(AtomicUsize::new(0));
    loop {
        let sleep = match engine.next_deadline() {
            Some(at) => {
                let (now, _) = clocks(origin);
                Duration::from_millis(at.saturating_sub(now))
            }
            None => Duration::from_secs(3600),
        };
        let cmd = tokio::select! {
            c = rx.recv() => match c { Some(c) => Some(c), None => break },
            _ = tokio::time::sleep(sleep) => None,
        };
        let (mono, wall) = clocks(origin);
        engine.set_clock(mono, wall);
        engine.set_buffered(buffered.load(Ordering::Relaxed));
        match cmd {
            None => engine.tick(),
            Some(Cmd::With(f)) => f(&mut engine),
            Some(Cmd::Token(e, t)) => engine.on_token(e, t),
            Some(Cmd::SocketOpen(e)) => engine.on_socket_open(e),
            Some(Cmd::SocketMsg(e, m)) => engine.on_message(e, &m),
            Some(Cmd::SocketClosed(e, c, r)) => {
                if sock.as_ref().is_some_and(|s| s.epoch == e) {
                    sock = None;
                }
                engine.on_socket_closed(e, c, &r)
            }
            Some(Cmd::OpenFailed(e, m)) => engine.on_open_failed(e, &m),
        }
        // 输出可能再触发输入（例如 On token），循环到队列空
        for out in engine.drain() {
            match out {
                Out::Write(text) => {
                    if let Some(s) = &sock {
                        buffered.fetch_add(text.len(), Ordering::Relaxed);
                        let _ = s.tx.send(WsOut::Text(text));
                    }
                }
                Out::RequestToken { epoch } => {
                    let (hk, h) = (hooks.clone(), handle.clone());
                    tauri::async_runtime::spawn(async move {
                        let t = hk.token().await;
                        let _ = h.tx.send(Cmd::Token(epoch, t));
                    });
                }
                Out::OpenSocket { epoch, token } => {
                    if let Some(old) = sock.take() {
                        old.task.abort();
                    }
                    let (wtx, wrx) = mpsc::unbounded_channel();
                    let url = hooks.ws_url();
                    let task = tauri::async_runtime::spawn(socket_task(url, token, epoch, handle.clone(), wrx, buffered.clone()));
                    sock = Some(Sock { epoch, tx: wtx, task });
                }
                Out::CloseSocket { code, reason } => {
                    if let Some(s) = sock.take() {
                        let _ = s.tx.send(WsOut::Close(code, reason));
                        // 给 close 帧一点时间发出；任务自己结束
                    }
                }
                Out::App(env) => hooks.on_app(&handle, env),
                Out::PresenceChanged(v) => hooks.on_presence(v),
                Out::Status(s) => hooks.on_status(s),
                Out::Issue(i) => hooks.on_issue(i),
                Out::Log(level, msg) => hooks.log(level, &msg),
            }
        }
    }
    if let Some(s) = sock {
        s.task.abort();
    }
}

async fn socket_task(url: String, token: String, epoch: u64, h: Handle, mut wrx: mpsc::UnboundedReceiver<WsOut>, buffered: Arc<AtomicUsize>) {
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;
    let mut req = match url.as_str().into_client_request() {
        Ok(r) => r,
        Err(e) => {
            let _ = h.tx.send(Cmd::OpenFailed(epoch, e.to_string()));
            return;
        }
    };
    match format!("Bearer {token}").parse() {
        Ok(v) => {
            req.headers_mut().insert("Authorization", v);
        }
        Err(_) => {
            let _ = h.tx.send(Cmd::OpenFailed(epoch, "invalid token".into()));
            return;
        }
    }
    let ws = match tokio_tungstenite::connect_async(req).await {
        Ok((ws, _)) => ws,
        Err(e) => {
            let _ = h.tx.send(Cmd::OpenFailed(epoch, e.to_string()));
            return;
        }
    };
    let _ = h.tx.send(Cmd::SocketOpen(epoch));
    let (mut sink, mut stream) = ws.split();
    loop {
        tokio::select! {
            out = wrx.recv() => match out {
                Some(WsOut::Text(t)) => {
                    let n = t.len();
                    let r = sink.send(Message::Text(t.into())).await;
                    buffered.fetch_sub(n.min(buffered.load(Ordering::Relaxed)), Ordering::Relaxed);
                    if r.is_err() {
                        let _ = h.tx.send(Cmd::SocketClosed(epoch, None, "write failed".into()));
                        return;
                    }
                }
                Some(WsOut::Close(code, reason)) => {
                    let _ = sink.send(Message::Close(Some(CloseFrame { code: CloseCode::from(code), reason: reason.into() }))).await;
                    let _ = sink.close().await;
                    return;
                }
                None => return,
            },
            msg = stream.next() => match msg {
                Some(Ok(Message::Text(t))) => { let _ = h.tx.send(Cmd::SocketMsg(epoch, t.to_string())); }
                Some(Ok(Message::Close(f))) => {
                    let (code, reason) = f.map(|f| (Some(u16::from(f.code)), f.reason.to_string())).unwrap_or((None, String::new()));
                    let _ = h.tx.send(Cmd::SocketClosed(epoch, code, reason));
                    return;
                }
                Some(Ok(_)) => {}
                Some(Err(e)) => {
                    let _ = h.tx.send(Cmd::SocketClosed(epoch, None, e.to_string()));
                    return;
                }
                None => {
                    let _ = h.tx.send(Cmd::SocketClosed(epoch, None, String::new()));
                    return;
                }
            },
        }
    }
}
