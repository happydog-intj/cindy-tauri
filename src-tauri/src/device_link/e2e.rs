//! 端到端：Rust 设备互联被控端（真实引擎 + 运行时 + 宿主）⇄ mock 中继 ⇄ 上游 TS `DeviceLinkClient`（手机）。
//! 迁自 scripts/device-link-e2e.mjs。需要 NODE_BIN；缺失时跳过。

#![cfg(test)]

use super::host::{DeviceLink, Ports};
use super::runtime::BoxFut;
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const DESKTOP_ID: &str = "tauri-device-0001";
const PHONE_ID: &str = "phone-device-0001";
const PHONE_TOKEN: &str = "phone-token-e2e";
const DESKTOP_TOKEN: &str = "desktop-token-e2e";

struct FakePorts {
    relay_port: u16,
    authed: AtomicBool,
    emitted: Mutex<Vec<(String, Value)>>,
    invoked: Mutex<Vec<String>>,
    logs: Mutex<Vec<String>>,
}

impl Ports for FakePorts {
    fn token(&self) -> BoxFut<Option<String>> {
        let ok = self.authed.load(Ordering::SeqCst);
        Box::pin(async move { ok.then(|| DESKTOP_TOKEN.to_owned()) })
    }
    fn authenticated(&self) -> (bool, Option<String>) {
        (self.authed.load(Ordering::SeqCst), Some("user-1".into()))
    }
    fn ws_url(&self) -> String {
        format!("ws://127.0.0.1:{}/api/device-link/ws", self.relay_port)
    }
    fn http_base(&self) -> String {
        format!("http://127.0.0.1:{}", self.relay_port)
    }
    fn emit(&self, channel: &str, arg: Value) {
        self.emitted.lock().unwrap().push((channel.to_owned(), arg));
    }
    fn owner_stamp(&self) -> Value {
        json!({ "dataOwnerId": "local-v1", "ownerGeneration": 1 })
    }
    fn invoke(&self, channel: String, _args: Vec<Value>) -> BoxFut<Result<Value, String>> {
        self.invoked.lock().unwrap().push(channel.clone());
        Box::pin(async move {
            match channel.as_str() {
                "maker:list-active" => Ok(json!([{ "sessionId": "e2e-session-1" }])),
                "maker:schedule:list" => Err("[NOT_IMPLEMENTED] x".into()),
                "maker:session-in-turn" => Err("[PERMISSION_DENIED] nope".into()),
                _ => Ok(Value::Null),
            }
        })
    }
    fn log(&self, level: &str, msg: &str) {
        self.logs.lock().unwrap().push(format!("{level} {msg}"));
    }
    fn device_name(&self) -> String {
        "E2E Desktop (Tauri)".into()
    }
    fn app_version(&self) -> String {
        "1.0.0".into()
    }
}

fn spawn_relay(node: &Path, scripts: &Path) -> (Child, u16) {
    let tokens = json!({ DESKTOP_TOKEN: DESKTOP_ID, PHONE_TOKEN: PHONE_ID }).to_string();
    let mut child = Command::new(node).arg(scripts.join("mock-device-link-relay.mjs")).env("MOCK_TOKENS", tokens).stdout(Stdio::piped()).stderr(Stdio::null()).spawn().expect("spawn relay");
    let mut line = String::new();
    BufReader::new(child.stdout.as_mut().unwrap()).read_line(&mut line).unwrap();
    let port = serde_json::from_str::<Value>(&line).unwrap()["port"].as_u64().unwrap() as u16;
    (child, port)
}

struct Phone {
    child: Child,
    stdin: ChildStdin,
    out: std::sync::mpsc::Receiver<Value>,
    seq: u64,
}

impl Phone {
    fn start(node: &Path, scripts: &Path, relay_port: u16) -> Phone {
        let mut child = Command::new(node).arg(scripts.join("device-link-phone.mjs")).arg(relay_port.to_string()).arg(PHONE_TOKEN).current_dir(scripts).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::inherit()).spawn().expect("spawn phone");
        let stdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            for l in BufReader::new(stdout).lines().map_while(Result::ok) {
                if let Ok(v) = serde_json::from_str::<Value>(&l) {
                    let _ = tx.send(v);
                }
            }
        });
        let ready = rx.recv_timeout(Duration::from_secs(60)).expect("phone ready");
        assert_eq!(ready["ready"], true);
        Phone { child, stdin, out: rx, seq: 0 }
    }
    fn cmd(&mut self, op: &str, mut extra: Value) -> Value {
        self.seq += 1;
        let id = self.seq;
        extra["id"] = json!(id);
        extra["op"] = json!(op);
        writeln!(self.stdin, "{extra}").unwrap();
        loop {
            let v = self.out.recv_timeout(Duration::from_secs(60)).unwrap_or_else(|_| panic!("phone op {op} timed out"));
            if v["id"] == json!(id) {
                assert!(v.get("error").is_none(), "phone op {op}: {v}");
                return v["result"].clone();
            }
        }
    }
    fn invoke(&mut self, channel: &str, args: Value) -> Value {
        self.cmd("invoke", json!({ "dst": DESKTOP_ID, "channel": channel, "args": args, "timeoutMs": 20000 }))
    }
}

fn wait<F: FnMut() -> bool>(what: &str, secs: u64, mut f: F) {
    let t0 = Instant::now();
    while !f() {
        assert!(t0.elapsed() < Duration::from_secs(secs), "timeout: {what}");
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn cindy_dir() -> PathBuf {
    match std::env::var("CINDY_DIR") {
        Ok(d) => PathBuf::from(d),
        Err(_) => Path::new(env!("CARGO_MANIFEST_DIR")).join("../../cindy"),
    }
}

fn node_and_scripts() -> Option<(PathBuf, PathBuf)> {
    let node = PathBuf::from(std::env::var("NODE_BIN").ok()?);
    let scripts = Path::new(env!("CARGO_MANIFEST_DIR")).join("../scripts");
    cindy_dir().join("node_modules/ws/wrapper.mjs").exists().then_some((node, scripts))
}

#[test]
fn controlled_side_serves_the_upstream_client_over_the_reliable_transport() {
    let Some((node, scripts)) = node_and_scripts() else { eprintln!("skip: NODE_BIN / cindy checkout missing"); return };
    let (mut relay, relay_port) = spawn_relay(&node, &scripts);
    let dir = std::env::temp_dir().join(format!("dl-e2e-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let ports = Arc::new(FakePorts { relay_port, authed: AtomicBool::new(false), emitted: Default::default(), invoked: Default::default(), logs: Default::default() });
    let dl = DeviceLink::new(dir.clone(), ports.clone());
    let rt = tokio::runtime::Handle::try_current().ok();
    let _ = rt;
    let invoke = |ch: &str, a: Value| -> Result<Value, String> {
        let dl = dl.clone();
        let ch = ch.to_owned();
        let args = a.as_array().cloned().unwrap_or_default();
        tauri::async_runtime::block_on(async move { dl.invoke(&ch, &args).await.unwrap() })
    };

    // 未登录：不连中继
    assert_eq!(invoke("device-link:get-state", json!([])).unwrap()["linkStatus"], "stopped");

    // 登录 → 上线
    ports.authed.store(true, Ordering::SeqCst);
    dl.sync_with_auth();
    wait("desktop online", 20, || invoke("device-link:get-state", json!([])).unwrap()["linkStatus"] == "online");
    assert!(ports.emitted.lock().unwrap().iter().any(|(c, a)| c == "device-link:status-changed" && a["status"] == "online"));

    // 手机上线；被控开关默认关闭：中继拒绝 link-open
    let mut phone = Phone::start(&node, &scripts, relay_port);
    phone.cmd("start", json!({}));
    wait("phone online", 20, || phone.cmd("status", json!({})) == "online");
    let refused = phone.cmd("open", json!({ "dst": DESKTOP_ID }));
    assert_eq!(refused["ok"], false, "{refused}");

    // 设备列表标出自己（带 Tauri 后缀）
    let devices = invoke("device-link:list-devices", json!([])).unwrap();
    let me = devices["devices"].as_array().unwrap().iter().find(|d| d["isSelf"] == true).cloned().expect("self in list");
    assert_eq!(me["deviceId"], DESKTOP_ID);
    assert!(me["name"].as_str().unwrap().ends_with("(Tauri)"), "{me}");

    // 打开被控 → 接入，协商可靠传输
    let on = invoke("device-link:set-enabled", json!([true])).unwrap();
    assert_eq!(on["remoteControlEnabled"], true);
    std::thread::sleep(Duration::from_millis(300));
    let opened = phone.cmd("open", json!({ "dst": DESKTOP_ID }));
    assert_eq!(opened["ok"], true, "{opened}");
    let caps = opened["accept"]["capabilities"].as_array().unwrap().iter().filter_map(Value::as_str).collect::<Vec<_>>();
    for c in ["history-view-v1", "reliable-transport-v1", "reliable-link-confirm-v1", "transport-ack-v2"] {
        assert!(caps.contains(&c), "{c} in {caps:?}");
    }
    assert_eq!(opened["accept"]["allowlistHash"], super::allowlist::allowlist_hash());

    // 订阅 + 远程 invoke
    let sub = phone.invoke("device-link:subscribe", json!([{ "topics": ["sessions", "session:e2e-session-1"], "controllerName": "E2E Phone" }]));
    assert_eq!(sub["ok"], true, "{sub}");
    wait("controlledBy", 10, || invoke("device-link:get-state", json!([])).unwrap()["controlledBy"].as_array().unwrap().iter().any(|c| c["deviceId"] == PHONE_ID));
    let list = phone.invoke("maker:list-active", json!([]));
    assert_eq!(list["ok"], true, "{list}");
    assert_eq!(list["result"][0]["sessionId"], "e2e-session-1");
    // 本机错误码原样回传
    let denied = phone.invoke("maker:session-in-turn", json!(["x"]));
    assert_eq!((denied["ok"].clone(), denied["error"]["code"].clone()), (json!(false), json!("PERMISSION_DENIED")), "{denied}");
    let missing = phone.invoke("maker:schedule:list", json!([]));
    assert_eq!(missing["error"]["code"], "NOT_IMPLEMENTED", "{missing}");
    // 白名单外：拒绝，且根本没进本机 invoke
    let forbidden = phone.invoke("safe-storage-read", json!(["api_key"]));
    assert_eq!(forbidden["error"]["code"], "FORBIDDEN", "{forbidden}");
    assert!(!ports.invoked.lock().unwrap().iter().any(|c| c == "safe-storage-read"));

    // 推送：订阅了的会话才收到
    dl.on_push("maker:event", json!({ "sessionId": "e2e-session-1", "event": { "type": "text", "data": { "text": "hello" } }, "persistId": "pid-1" }));
    dl.on_push("maker:event", json!({ "sessionId": "someone-else", "event": { "type": "text", "data": { "text": "x" } } }));
    wait("push on phone", 10, || phone.cmd("pushes", json!({})).as_array().unwrap().iter().any(|p| p["channel"] == "maker:event" && p["payload"]["persistId"] == "pid-1"));
    std::thread::sleep(Duration::from_millis(400));
    let pushes = phone.cmd("pushes", json!({}));
    assert!(!pushes.to_string().contains("someone-else"), "未订阅的会话不转发");
    // 带着本机数据所有者戳
    let got = pushes.as_array().unwrap().iter().find(|p| p["payload"]["persistId"] == "pid-1").unwrap();
    assert_eq!(got["ownerStamp"]["dataOwnerId"], "local-v1", "{got}");

    // 大结果：走分片传输（>128 KiB 的 invoke-result）
    let big = "x".repeat(400 * 1024);
    let ports2 = ports.clone();
    let _ = ports2;
    // maker:list-active 的返回固定；这里借 push 路径验证大帧分片：400 KiB 文本事件
    dl.on_push("maker:event", json!({ "sessionId": "e2e-session-1", "event": { "type": "text", "data": { "text": big } }, "persistId": "pid-big" }));
    wait("big push on phone", 20, || phone.cmd("pushes", json!({})).as_array().unwrap().iter().any(|p| p["payload"]["persistId"] == "pid-big"));

    // 撤销 → 链路关闭，无法重开
    let revoked = invoke("device-link:revoke", json!([PHONE_ID])).unwrap();
    assert!(revoked["revokedControllers"].as_array().unwrap().iter().any(|d| d == PHONE_ID));
    assert!(!revoked["controlledBy"].as_array().unwrap().iter().any(|c| c["deviceId"] == PHONE_ID));
    let reopen = phone.cmd("open", json!({ "dst": DESKTOP_ID }));
    assert_eq!(reopen["ok"], false, "{reopen}");
    invoke("device-link:restore", json!([PHONE_ID])).unwrap();

    // 重新接入后，本机重启式的关远控 → 全部断开
    std::thread::sleep(Duration::from_millis(300));
    let again = phone.cmd("open", json!({ "dst": DESKTOP_ID }));
    assert_eq!(again["ok"], true, "{again}");
    phone.invoke("device-link:subscribe", json!([{ "topics": ["sessions"] }]));
    let off = invoke("device-link:set-enabled", json!([false])).unwrap();
    assert_eq!(off["controlledBy"], json!([]));

    // 退出登录 → 下线
    ports.authed.store(false, Ordering::SeqCst);
    dl.sync_with_auth();
    wait("desktop offline", 15, || invoke("device-link:get-state", json!([])).unwrap()["linkStatus"] == "stopped");

    let logs = ports.logs.lock().unwrap().join("\n");
    assert!(!logs.contains("dropping"), "引擎没有丢弃过帧:\n{logs}");
    assert!(!logs.contains(PHONE_TOKEN) && !logs.contains(DESKTOP_TOKEN), "日志不含令牌");
    let _ = phone.cmd("stop", json!({}));
    let _ = phone.child.kill();
    let _ = relay.kill();
    dl.dispose();
    let _ = std::fs::remove_dir_all(&dir);
}
