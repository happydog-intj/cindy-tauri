//! 设备互联被控端宿主（原 sidecar `device-link.mjs`）：设置、订阅表、允许列表内的 invoke 分发、推送转发。
//! 不做：文件传输、媒体、远程桌面、共享任务、provider share、直连（WebRTC）、事件批量、离线积压队列。
use super::allowlist::{invoke_allowed, push_forwarded};
use super::engine::{Env, Issue, Status, Timing};
use super::runtime::{self, BoxFut, Handle, Hooks};
use super::topics::{is_topic, topic_for_push};
use super::transport::{CAP_BACKGROUND_LINK_V1, CAP_HISTORY_VIEW_V1};
use crate::override_file::{read_json, write_json_atomic};
use serde_json::{json, Map, Value};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

pub const DL_SUBSCRIBE: &str = "device-link:subscribe";
pub const DL_UNSUBSCRIBE: &str = "device-link:unsubscribe";
const LEGACY_TOPIC: &str = "*";
const MAX_TOPICS_PER_CONTROLLER: usize = 256;
const LIST_TIMEOUT_MS: u64 = 15_000;
const SEEN_TTL_MS: u64 = 120_000;

/// 宿主对外部世界的依赖：本机 invoke 分发、renderer 推送、账号令牌、本机已实现判定。
pub trait Ports: Send + Sync + 'static {
    fn token(&self) -> BoxFut<Option<String>>;
    fn authenticated(&self) -> (bool, Option<String>);
    fn ws_url(&self) -> String;
    fn http_base(&self) -> String;
    fn emit(&self, channel: &str, arg: Value);
    fn owner_stamp(&self) -> Value;
    /// 本机 invoke（与 renderer 同一套路由）。Err 为 `[CODE] message`。
    fn invoke(&self, channel: String, args: Vec<Value>) -> BoxFut<Result<Value, String>>;
    fn log(&self, level: &str, msg: &str);
    fn device_name(&self) -> String;
    fn app_version(&self) -> String;
}

#[derive(Default)]
struct Sub {
    name: String,
    topics: BTreeSet<String>,
    capabilities: BTreeSet<String>,
}

#[derive(Default)]
struct Subs(BTreeMap<String, Sub>);

fn short(id: &str) -> String {
    id.chars().take(8).collect()
}

impl Subs {
    fn ensure(&mut self, src: &str, name: Option<&str>, caps: Option<&[String]>) -> &mut Sub {
        let e = self.0.entry(src.to_owned()).or_insert_with(|| Sub { name: name.map(str::to_owned).unwrap_or_else(|| short(src)), ..Default::default() });
        if let Some(n) = name {
            e.name = n.to_owned();
        }
        if let Some(c) = caps {
            e.capabilities.extend(c.iter().cloned());
        }
        e
    }
    fn subscribe(&mut self, src: &str, topics: &[String], name: Option<&str>) {
        let e = self.ensure(src, name, None);
        for t in topics {
            if e.topics.len() < MAX_TOPICS_PER_CONTROLLER {
                e.topics.insert(t.clone());
            }
        }
        if !topics.is_empty() {
            e.topics.remove(LEGACY_TOPIC);
        }
    }
    fn unsubscribe(&mut self, src: &str, topics: &[String]) {
        if let Some(e) = self.0.get_mut(src) {
            for t in topics {
                e.topics.remove(t);
            }
        }
    }
    fn targets(&self, topic: &str) -> Vec<String> {
        self.0.iter().filter(|(_, e)| e.topics.contains(LEGACY_TOPIC) || e.topics.contains(topic)).map(|(k, _)| k.clone()).collect()
    }
}

#[derive(Default)]
struct Inner {
    subs: Subs,
    accepted: BTreeSet<String>,
    status: &'static str,
    issue: Option<Value>,
    seen: HashMap<String, u64>,
    want_online: bool,
    last_user: Option<String>,
}

pub struct DeviceLink {
    settings_file: PathBuf,
    ports: Arc<dyn Ports>,
    inner: Mutex<Inner>,
    handle: Mutex<Option<Handle>>,
}

fn now_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

fn failure(code: &str, message: &str) -> Value {
    json!({ "ok": false, "error": { "code": code, "message": format!("[{code}] {message}") } })
}

/// `[CODE] message` → (code, message)；其余归 INTERNAL。
fn error_code_of(msg: &str) -> (String, String) {
    if let Some(rest) = msg.strip_prefix('[') {
        if let Some((code, tail)) = rest.split_once(']') {
            if !code.is_empty() && code.chars().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_') {
                return (code.to_owned(), msg.to_owned());
            }
            let _ = tail;
        }
    }
    ("INTERNAL".into(), format!("[INTERNAL] {msg}"))
}

fn trimmed_name(v: Option<&Value>) -> Option<String> {
    let s = v?.as_str()?;
    let t = crate::auth::flow::js_trim(s);
    if t.is_empty() { None } else { Some(t.chars().take(120).collect()) }
}

impl DeviceLink {
    pub fn new(settings_dir: PathBuf, ports: Arc<dyn Ports>) -> Arc<Self> {
        let me = Arc::new(DeviceLink { settings_file: settings_dir.join("device-link-settings.json"), ports, inner: Mutex::new(Inner { status: "stopped", ..Default::default() }), handle: Mutex::new(None) });
        let hooks: Arc<dyn Hooks> = Arc::new(HostHooks(Arc::downgrade(&me)));
        let h = runtime::spawn(hooks, me.hello(), Timing { ping_interval_ms: 15_000, ..Timing::default() });
        *me.handle.lock().unwrap() = Some(h);
        me
    }

    fn handle(&self) -> Handle {
        self.handle.lock().unwrap().clone().expect("runtime handle")
    }

    // ── 设置（与设置页同一份文件）──
    fn read_settings(&self) -> Map<String, Value> {
        match read_json(&self.settings_file, None) {
            Ok(Some(Value::Object(o))) => o,
            _ => Map::new(),
        }
    }
    fn write_settings(&self, patch: Value) -> Result<(), String> {
        let mut s = self.read_settings();
        if let Value::Object(p) = patch {
            for (k, v) in p {
                s.insert(k, v);
            }
        }
        write_json_atomic(&self.settings_file, &Value::Object(s), None)
    }
    fn remote_enabled(&self) -> bool {
        self.read_settings().get("remoteControlEnabled") == Some(&Value::Bool(true))
    }
    fn revoked(&self) -> BTreeSet<String> {
        self.read_settings().get("revokedControllers").and_then(Value::as_array).map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_owned)).collect()).unwrap_or_default()
    }

    fn hello(&self) -> Value {
        json!({
            "deviceName": self.ports.device_name(),
            "platform": if cfg!(target_os = "macos") { "darwin" } else if cfg!(windows) { "win32" } else { "linux" },
            "appVersion": self.ports.app_version(),
            "remoteControlEnabled": self.remote_enabled(),
            "busy": false,
            "capabilities": [],
        })
    }

    fn controlled_by(&self) -> Vec<Value> {
        let i = self.inner.lock().unwrap();
        i.subs.0.iter().filter(|(k, _)| i.accepted.contains(*k)).map(|(k, e)| json!({ "deviceId": k, "name": e.name })).collect()
    }
    fn emit_controlled(&self) {
        self.ports.emit("device-link:controlled-state", json!({ "controllers": self.controlled_by() }));
    }

    pub fn state(&self) -> Value {
        let s = self.read_settings();
        let arr = |k: &str| s.get(k).filter(|v| v.is_array()).cloned().unwrap_or(json!([]));
        let (status, issue) = {
            let i = self.inner.lock().unwrap();
            (i.status, i.issue.clone())
        };
        json!({
            "remoteControlEnabled": s.get("remoteControlEnabled") == Some(&Value::Bool(true)),
            "keepAwake": s.get("keepAwake") == Some(&Value::Bool(true)),
            "linkStatus": status,
            "connectionIssue": issue,
            "standby": false,
            "controlledBy": self.controlled_by(),
            "revokedControllers": arr("revokedControllers"),
            "disabledControlDeviceIds": arr("disabledControlDeviceIds"),
            "unresponsiveDeviceIds": [],
        })
    }

    // ── 引擎回调 ──
    fn on_status(&self, st: Status) {
        let s = st.as_str();
        {
            let mut i = self.inner.lock().unwrap();
            i.status = s;
        }
        self.ports.log("info", &format!("status {s}"));
        self.ports.emit("device-link:status-changed", json!({ "status": s }));
        if st != Status::Online {
            self.inner.lock().unwrap().accepted.clear();
            self.emit_controlled();
        }
    }

    fn on_issue(&self, issue: Option<Issue>) {
        let v = issue.map(|i| {
            let mut m = Map::new();
            m.insert("kind".into(), json!(i.kind));
            if let Some(c) = i.close_code {
                m.insert("closeCode".into(), json!(c));
            }
            m.insert("at".into(), json!(i.at));
            Value::Object(m)
        });
        self.inner.lock().unwrap().issue = v.clone();
        self.ports.emit("device-link:connection-issue", json!({ "issue": v }));
    }

    fn on_app(self: &Arc<Self>, h: &Handle, env: Env) {
        match env.kind.as_str() {
            "link-open" => self.handle_link_open(h, &env),
            "link-close" => self.handle_link_close(&env),
            "invoke" => {
                let me = self.clone();
                let h = h.clone();
                tauri::async_runtime::spawn(async move { me.handle_invoke(h, env).await });
            }
            _ => {}
        }
    }

    fn handle_link_open(&self, h: &Handle, env: &Env) {
        let (Some(src), Some(id)) = (env.src.clone(), env.id.clone()) else { return };
        if !self.remote_enabled() {
            self.ports.log("warn", &format!("link-open from {} rejected: remote control disabled locally", short(&src)));
            return;
        }
        if self.revoked().contains(&src) {
            self.ports.log("warn", &format!("link-open from {} rejected: access revoked", short(&src)));
            let s = src.clone();
            h.run(move |e| e.close_link(&s, "revoked"));
            return;
        }
        let payload = env.payload.clone().unwrap_or(Value::Null);
        let name = trimmed_name(payload.get("controllerName"));
        let caps: Vec<String> = payload.get("capabilities").and_then(Value::as_array).map(|a| a.iter().filter_map(|c| c.as_str().map(str::to_owned)).collect()).unwrap_or_default();
        let accept = json!({
            "appVersion": self.ports.app_version(),
            "allowlistHash": super::allowlist::allowlist_hash(),
            "capabilities": [CAP_HISTORY_VIEW_V1, CAP_BACKGROUND_LINK_V1],
        });
        let (s2, id2) = (src.clone(), id.clone());
        let me_log = self.ports.clone();
        h.run(move |e| {
            if let Err(err) = e.send_link_accept(&s2, &id2, accept) {
                me_log.log("warn", &format!("link-accept to {} failed: {}", short(&s2), err.message));
            }
        });
        {
            let mut i = self.inner.lock().unwrap();
            let e = i.subs.ensure(&src, name.as_deref(), Some(&caps));
            // 不声明 background-link 的老控制端靠全量订阅（desktop 同口径）。
            if !caps.iter().any(|c| c == CAP_BACKGROUND_LINK_V1) && e.topics.is_empty() {
                e.topics.insert(LEGACY_TOPIC.into());
            }
            i.accepted.insert(src.clone());
        }
        self.ports.log("info", &format!("controller {} linked", short(&src)));
        self.emit_controlled();
    }

    fn handle_link_close(&self, env: &Env) {
        let Some(src) = env.src.clone() else { return };
        let reason = env.payload.as_ref().and_then(|p| p.get("reason")).and_then(Value::as_str).unwrap_or("");
        if reason == "transport-timeout" {
            return; // 链路瞬断：保留订阅，等对端重开
        }
        {
            let mut i = self.inner.lock().unwrap();
            i.subs.0.remove(&src);
            i.accepted.remove(&src);
        }
        self.emit_controlled();
    }

    fn handle_subscription(&self, src: &str, payload: &Value) -> Value {
        if !self.remote_enabled() {
            return failure("REMOTE_DISABLED", "remote control disabled");
        }
        if self.revoked().contains(src) {
            return failure("ACCESS_REVOKED", "access revoked by target device");
        }
        let arg = payload.get("args").and_then(Value::as_array).and_then(|a| a.first()).cloned().unwrap_or(Value::Null);
        let topics: Vec<String> = arg.get("topics").and_then(Value::as_array).map(|a| a.iter().filter_map(|t| t.as_str().filter(|_| is_topic(t)).map(str::to_owned)).collect()).unwrap_or_default();
        let name = trimmed_name(arg.get("controllerName"));
        let newly = {
            let mut i = self.inner.lock().unwrap();
            if payload.get("channel").and_then(Value::as_str) == Some(DL_SUBSCRIBE) {
                i.subs.subscribe(src, &topics, name.as_deref());
                if let Some(c) = arg.get("capabilities").and_then(Value::as_array) {
                    let caps: Vec<String> = c.iter().filter_map(|x| x.as_str().map(str::to_owned)).collect();
                    i.subs.ensure(src, None, Some(&caps));
                }
            } else {
                i.subs.unsubscribe(src, &topics);
            }
            i.accepted.insert(src.to_owned())
        };
        if newly {
            self.emit_controlled();
        }
        json!({ "ok": true, "result": { "ok": true } })
    }

    fn result(&self, h: &Handle, src: &str, id: &str, payload: Value) {
        let (s, i, p) = (src.to_owned(), id.to_owned(), payload);
        let ports = self.ports.clone();
        h.run(move |e| {
            if let Err(err) = e.send_invoke_result(&s, &i, p) {
                ports.log("warn", &format!("invoke-result to {} failed: {}", short(&s), err.message));
            }
        });
    }

    async fn handle_invoke(self: Arc<Self>, h: Handle, env: Env) {
        let (Some(src), Some(id)) = (env.src.clone(), env.id.clone()) else { return };
        let now = now_ms();
        {
            let mut i = self.inner.lock().unwrap();
            let key = format!("{src}\0{id}");
            if i.seen.get(&key).copied().unwrap_or(0) > now {
                return; // 同一请求重发：结果已在路上
            }
            i.seen.insert(key, now + SEEN_TTL_MS);
            if i.seen.len() > 2000 {
                i.seen.retain(|_, exp| *exp > now);
            }
        }
        let payload = env.payload.clone().unwrap_or(Value::Null);
        let channel = payload.get("channel").and_then(Value::as_str).unwrap_or("").to_owned();
        if channel == DL_SUBSCRIBE || channel == DL_UNSUBSCRIBE {
            let r = self.handle_subscription(&src, &payload);
            return self.result(&h, &src, &id, r);
        }
        if !self.remote_enabled() {
            return self.result(&h, &src, &id, failure("REMOTE_DISABLED", "remote control disabled"));
        }
        if self.revoked().contains(&src) {
            return self.result(&h, &src, &id, failure("ACCESS_REVOKED", "access revoked by target device"));
        }
        if !invoke_allowed(&channel) {
            return self.result(&h, &src, &id, failure("FORBIDDEN", &format!("channel {channel} is not remotely invokable")));
        }
        let args = payload.get("args").and_then(Value::as_array).cloned().unwrap_or_default();
        let out = match self.ports.invoke(channel.clone(), args).await {
            Ok(v) => json!({ "ok": true, "result": if v.is_null() { Value::Null } else { v } }),
            Err(msg) => {
                let (code, message) = error_code_of(&msg);
                if code == "NOT_IMPLEMENTED" {
                    failure("NOT_IMPLEMENTED", &format!("{channel} is not available in the Tauri build yet"))
                } else {
                    self.ports.log("warn", &format!("remote invoke {channel} from {} failed: {code}", short(&src)));
                    json!({ "ok": false, "error": { "code": code, "message": message.chars().take(2000).collect::<String>() } })
                }
            }
        };
        self.result(&h, &src, &id, out);
    }

    /// 推送转发：只发给已接入、订阅了对应 topic 的控制端。
    pub fn on_push(&self, channel: &str, payload: Value) {
        if !push_forwarded(channel) || self.inner.lock().unwrap().status != "online" {
            return;
        }
        let Some(topic) = topic_for_push(channel, &payload) else { return };
        let targets: Vec<String> = {
            let i = self.inner.lock().unwrap();
            i.subs.targets(&topic).into_iter().filter(|d| i.accepted.contains(d)).collect()
        };
        if targets.is_empty() {
            return;
        }
        let stamp = self.ports.owner_stamp();
        let stamp = if stamp.get("dataOwnerId").is_some_and(|v| !v.is_null() && v != "") { Some(stamp) } else { None };
        let (ch, ports) = (channel.to_owned(), self.ports.clone());
        self.handle().run(move |e| {
            for dst in targets {
                if let Err(err) = e.send_push(&dst, &ch, payload.clone(), stamp.clone()) {
                    ports.log("warn", &format!("push {ch} to {} failed: {}", short(&dst), err.message));
                }
            }
        });
    }

    // ── 登录态 ──
    pub fn sync_with_auth(&self) {
        let (authed, user) = self.ports.authenticated();
        let h = self.handle();
        let mut i = self.inner.lock().unwrap();
        if authed {
            if !i.want_online {
                i.want_online = true;
                i.last_user = user;
                h.run(|e| e.start());
            } else if user != i.last_user {
                i.last_user = user;
                i.subs.0.clear();
                i.accepted.clear();
                h.run(|e| e.restart_connection("account-changed"));
            }
        } else if i.want_online {
            i.want_online = false;
            i.last_user = None;
            i.subs.0.clear();
            i.accepted.clear();
            drop(i);
            h.run(|e| e.stop());
            self.emit_controlled();
        }
    }

    // ── renderer 通道 ──
    pub fn owns(channel: &str) -> bool {
        matches!(channel, "device-link:get-state" | "device-link:set-keep-awake" | "device-link:set-enabled" | "device-link:list-devices" | "device-link:revoke" | "device-link:restore" | "device-link:disconnect-all")
    }

    fn close_all(&self, reason: &'static str) {
        let ids: Vec<String> = {
            let mut i = self.inner.lock().unwrap();
            let ids = i.accepted.iter().cloned().collect();
            i.subs.0.clear();
            i.accepted.clear();
            ids
        };
        self.handle().run(move |e| {
            for id in ids {
                e.close_link(&id, reason);
            }
        });
        self.emit_controlled();
    }

    pub async fn invoke(self: &Arc<Self>, channel: &str, args: &[Value]) -> Option<Result<Value, String>> {
        let a0 = args.first().cloned().unwrap_or(Value::Null);
        Some(match channel {
            "device-link:get-state" => Ok(self.state()),
            "device-link:set-keep-awake" => {
                let Some(b) = a0.as_bool() else { return Some(Err("[INVALID_PARAMS] enabled must be a boolean".into())) };
                if let Err(e) = self.write_settings(json!({ "keepAwake": b })) {
                    return Some(Err(e));
                }
                self.ports.emit("device-link:keep-awake-changed", json!({ "keepAwake": b }));
                Ok(json!({ "keepAwake": b }))
            }
            "device-link:set-enabled" => {
                let Some(b) = a0.as_bool() else { return Some(Err("[INVALID_PARAMS] enabled must be a boolean".into())) };
                if let Err(e) = self.write_settings(json!({ "remoteControlEnabled": b })) {
                    return Some(Err(e));
                }
                self.handle().run(move |e| {
                    let mut m = Map::new();
                    m.insert("remoteControlEnabled".into(), json!(b));
                    e.send_presence(m);
                });
                if !b {
                    self.close_all("toggle-off"); // 关闭被控：断开所有控制端
                }
                Ok(self.state())
            }
            "device-link:revoke" => {
                let Some(id) = a0.as_str().filter(|s| !s.is_empty()).map(str::to_owned) else { return Some(Err("[INVALID_PARAMS] deviceId required".into())) };
                let mut r = self.revoked();
                r.insert(id.clone());
                if let Err(e) = self.write_settings(json!({ "revokedControllers": r.into_iter().collect::<Vec<_>>() })) {
                    return Some(Err(e));
                }
                {
                    let mut i = self.inner.lock().unwrap();
                    i.subs.0.remove(&id);
                    i.accepted.remove(&id);
                }
                let i2 = id.clone();
                self.handle().run(move |e| e.close_link(&i2, "revoked"));
                self.emit_controlled();
                Ok(self.state())
            }
            "device-link:restore" => {
                let Some(id) = a0.as_str().filter(|s| !s.is_empty()).map(str::to_owned) else { return Some(Err("[INVALID_PARAMS] deviceId required".into())) };
                let mut r = self.revoked();
                r.remove(&id);
                if let Err(e) = self.write_settings(json!({ "revokedControllers": r.into_iter().collect::<Vec<_>>() })) {
                    return Some(Err(e));
                }
                Ok(self.state())
            }
            "device-link:disconnect-all" => {
                self.close_all("user");
                Ok(self.state())
            }
            "device-link:list-devices" => self.list_devices().await,
            _ => return None,
        })
    }

    async fn list_devices(&self) -> Result<Value, String> {
        let Some(token) = self.ports.token().await else { return Err("[PRECONDITION_FAILED] Device Link requires a Cindy account.".into()) };
        let url = format!("{}/api/device-link/devices", self.ports.http_base());
        let client = crate::auth::client::http();
        let res = client.get(url).bearer_auth(token).header("Accept", "application/json").timeout(std::time::Duration::from_millis(LIST_TIMEOUT_MS)).send().await.map_err(|_| "[NETWORK_ERROR] device directory is unreachable".to_string())?;
        if !res.status().is_success() {
            let code = if res.status().as_u16() == 401 { "PERMISSION_DENIED" } else { "DEVICE_LINK_FAILED" };
            return Err(format!("[{code}] device directory request failed ({})", res.status().as_u16()));
        }
        let body: Value = res.json().await.unwrap_or(Value::Null);
        let devices = body.get("devices").and_then(Value::as_array).cloned().unwrap_or_default();
        let s = self.read_settings();
        let disabled: BTreeSet<String> = s.get("disabledControlDeviceIds").and_then(Value::as_array).map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_owned)).collect()).unwrap_or_default();
        let self_id = self.handle().with(|e| e.self_device_id().map(str::to_owned)).await.flatten();
        let out: Vec<Value> = devices
            .into_iter()
            .filter(|d| d.get("deviceId").is_some_and(Value::is_string))
            .map(|mut d| {
                let id = d["deviceId"].as_str().unwrap_or("").to_owned();
                let is_self = match &self_id {
                    Some(me) => &id == me,
                    None => d.get("isSelf") == Some(&Value::Bool(true)),
                };
                d["isSelf"] = json!(is_self);
                d["controlEnabled"] = json!(!disabled.contains(&id));
                d
            })
            .collect();
        Ok(json!({ "devices": out }))
    }

    pub fn dispose(&self) {
        self.handle().run(|e| e.stop());
    }

    #[cfg(test)]
    pub fn subscriptions(&self) -> Vec<(String, Vec<String>)> {
        self.inner.lock().unwrap().subs.0.iter().map(|(k, v)| (k.clone(), v.topics.iter().cloned().collect())).collect()
    }
}

struct HostHooks(std::sync::Weak<DeviceLink>);

impl Hooks for HostHooks {
    fn token(&self) -> BoxFut<Option<String>> {
        match self.0.upgrade() {
            Some(d) => d.ports.token(),
            None => Box::pin(async { None }),
        }
    }
    fn ws_url(&self) -> String {
        self.0.upgrade().map(|d| d.ports.ws_url()).unwrap_or_default()
    }
    fn log(&self, level: &str, msg: &str) {
        if level == "debug" {
            return;
        }
        if let Some(d) = self.0.upgrade() {
            d.ports.log(level, &msg.chars().take(300).collect::<String>());
        }
    }
    fn on_app(&self, h: &Handle, env: Env) {
        if let Some(d) = self.0.upgrade() {
            d.on_app(h, env);
        }
    }
    fn on_status(&self, status: Status) {
        if let Some(d) = self.0.upgrade() {
            d.on_status(status);
        }
    }
    fn on_issue(&self, issue: Option<Issue>) {
        if let Some(d) = self.0.upgrade() {
            d.on_issue(issue);
        }
    }
    fn on_presence(&self, snapshot: Value) {
        if let Some(d) = self.0.upgrade() {
            d.ports.emit("device-link:presence-changed", snapshot);
        }
    }
}

/// 本机名：macOS 用「电脑名称」（与 desktop deviceName.ts 同源），其余用 hostname；加「(Tauri)」后缀，
/// 避免同一台电脑上官方版与这个版本在手机里同名。
pub fn read_computer_name() -> String {
    let fallback = || {
        let h = std::process::Command::new("hostname").output().ok().and_then(|o| String::from_utf8(o.stdout).ok()).unwrap_or_default();
        let h = h.trim().trim_end_matches(".local").to_owned();
        if h.is_empty() { "Unknown Device".to_owned() } else { h }
    };
    let name = if cfg!(target_os = "macos") {
        std::process::Command::new("scutil").args(["--get", "ComputerName"]).output().ok().and_then(|o| String::from_utf8(o.stdout).ok()).map(|s| s.trim().to_owned()).filter(|s| !s.is_empty()).unwrap_or_else(fallback)
    } else {
        fallback()
    };
    format!("{name} (Tauri)")
}
