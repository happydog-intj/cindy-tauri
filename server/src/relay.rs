//! 设备互联中继：同一账号下的设备通过 WebSocket 互相转发帧。线协议同 mock-device-link-relay.mjs
//! （即上游 packages/device-link-relay/src/hub.ts）：
//! - 首帧必须是 hello，回 hello-ack；deviceId 取自令牌，不信客户端自报。
//! - 路由帧改写 src 后原样转发；目标不在线回 DEVICE_OFFLINE；控制类帧目标未开远控回 REMOTE_DISABLED。
//! - 同一设备新连接顶掉旧连接（关闭码 4409）。
//! - 只在同一 user 内路由，不同账号互相不可见。
use crate::{auth::{authed, error}, db::Db};
use axum::{
    extract::{ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade}, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::get,
    Json, Router,
};
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use std::{collections::HashMap, sync::{Arc, Mutex}};
use tokio::sync::mpsc::{unbounded_channel, UnboundedSender};

const PROTOCOL_VERSION: i64 = 1;
const ROUTED: [&str; 8] = ["link-open", "link-accept", "link-close", "invoke", "invoke-result", "push", "transport-ack", "transport-reset"];
const CONTROL: [&str; 2] = ["link-open", "invoke"];

struct Conn {
    id: u64,
    tx: UnboundedSender<Message>,
    remote_control: bool,
    busy: bool,
}

pub struct Hub {
    pub db: Arc<Db>,
    conns: Mutex<HashMap<(String, String), Conn>>,
    next: Mutex<u64>,
}

impl Hub {
    pub fn new(db: Arc<Db>) -> Self {
        Self { db, conns: Mutex::new(HashMap::new()), next: Mutex::new(0) }
    }

    fn presence(&self, user: &str, device: &str, conns: &HashMap<(String, String), Conn>) -> Value {
        let row = self.db.devices(user).into_iter().find(|d| d.device_id == device);
        let c = conns.get(&(user.to_string(), device.to_string()));
        let name = row.as_ref().map(|d| d.name.clone()).unwrap_or_default();
        json!({
            "deviceId": device, "online": c.is_some(), "deviceName": name, "selfName": name, "deviceInfo": null,
            "platform": row.as_ref().map(|d| d.platform.clone()).unwrap_or_default(),
            "appVersion": row.as_ref().map(|d| d.app_version.clone()).unwrap_or_default(),
            "lastSeenAt": if c.is_some() { crate::db::now_ms() } else { row.map(|d| d.last_seen).unwrap_or(0) },
            "remoteControlEnabled": c.map_or(false, |c| c.remote_control), "busy": c.map_or(false, |c| c.busy),
        })
    }

    fn broadcast(&self, user: &str, device: &str, conns: &HashMap<(String, String), Conn>) {
        let frame = json!({ "v": PROTOCOL_VERSION, "kind": "presence-changed", "payload": self.presence(user, device, conns) });
        for ((u, d), c) in conns {
            if u == user && d != device {
                let _ = c.tx.send(Message::Text(frame.to_string().into()));
            }
        }
    }
}

fn frame(kind: &str, id: Option<&Value>, payload: Value) -> Message {
    let mut m = json!({ "v": PROTOCOL_VERSION, "kind": kind, "payload": payload });
    if let Some(id) = id {
        m["id"] = id.clone();
    }
    Message::Text(m.to_string().into())
}

async fn devices(State(h): State<Arc<Hub>>, headers: HeaderMap) -> Response {
    let Some(me) = authed(&h.db, &headers) else { return error(StatusCode::UNAUTHORIZED, "UNAUTHORIZED") };
    let conns = h.conns.lock().unwrap();
    let list: Vec<Value> = h
        .db
        .devices(&me.user_id)
        .into_iter()
        .map(|d| {
            let p = h.presence(&me.user_id, &d.device_id, &conns);
            json!({
                "deviceId": d.device_id, "name": d.name, "selfName": d.name, "deviceInfo": null, "platform": d.platform, "appVersion": d.app_version,
                "lastSeenAt": chrono_iso(p["lastSeenAt"].as_i64().unwrap_or(0)), "online": p["online"], "busy": p["busy"],
                "remoteControlEnabled": p["remoteControlEnabled"], "isSelf": d.device_id == me.device_id,
            })
        })
        .collect();
    Json(json!({ "devices": list })).into_response()
}

/// 毫秒时间戳 → ISO 8601（UTC，毫秒精度）。不引入时间库：用民用日历算法。
fn chrono_iso(ms: i64) -> String {
    let ms = if ms <= 0 { crate::db::now_ms() } else { ms };
    let (days, rem) = (ms.div_euclid(86_400_000), ms.rem_euclid(86_400_000));
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}.{:03}Z", rem / 3_600_000, rem / 60_000 % 60, rem / 1000 % 60, rem % 1000)
}

async fn ws_upgrade(State(h): State<Arc<Hub>>, headers: HeaderMap, ws: WebSocketUpgrade) -> Response {
    let Some(me) = authed(&h.db, &headers) else { return error(StatusCode::UNAUTHORIZED, "UNAUTHORIZED") };
    ws.on_upgrade(move |sock| session(h, me.user_id, me.device_id, sock))
}

async fn session(h: Arc<Hub>, user: String, device: String, sock: WebSocket) {
    let (mut sink, mut stream) = sock.split();
    let (tx, mut rx) = unbounded_channel::<Message>();
    let writer = tokio::spawn(async move {
        while let Some(m) = rx.recv().await {
            let close = matches!(m, Message::Close(_));
            if sink.send(m).await.is_err() || close {
                break;
            }
        }
    });
    let key = (user.clone(), device.clone());
    let my_id = {
        let mut n = h.next.lock().unwrap();
        *n += 1;
        *n
    };
    let mut ready = false;
    while let Some(Ok(msg)) = stream.next().await {
        let Message::Text(raw) = msg else {
            if matches!(msg, Message::Close(_)) {
                break;
            }
            continue;
        };
        let Ok(env) = serde_json::from_str::<Value>(&raw) else { continue };
        let kind = env["kind"].as_str().unwrap_or("");
        if !ready {
            if kind != "hello" {
                let _ = tx.send(Message::Close(Some(CloseFrame { code: 4401, reason: "hello required".into() })));
                break;
            }
            let p = &env["payload"];
            let s = |k: &str| p[k].as_str().unwrap_or("").chars().take(128).collect::<String>();
            h.db.upsert_device(&user, &device, &s("deviceName"), &s("platform"), &s("appVersion"));
            let mut conns = h.conns.lock().unwrap();
            if let Some(old) = conns.insert(key.clone(), Conn { id: my_id, tx: tx.clone(), remote_control: p["remoteControlEnabled"] == true, busy: p["busy"] == true }) {
                let _ = old.tx.send(Message::Close(Some(CloseFrame { code: 4409, reason: "replaced by a newer connection".into() })));
            }
            ready = true;
            let _ = tx.send(frame("hello-ack", None, json!({ "serverProtocolVersion": PROTOCOL_VERSION, "deviceId": device, "userId": user, "capabilities": [] })));
            let others: Vec<String> = conns.keys().filter(|(u, d)| *u == user && *d != device).map(|(_, d)| d.clone()).collect();
            for d in others {
                let _ = tx.send(frame("presence-changed", None, h.presence(&user, &d, &conns)));
            }
            h.broadcast(&user, &device, &conns);
            continue;
        }
        match kind {
            "ping" => {
                let mut m = json!({ "v": PROTOCOL_VERSION, "kind": "pong" });
                if let Some(id) = env.get("id") {
                    m["id"] = id.clone();
                }
                let _ = tx.send(Message::Text(m.to_string().into()));
            }
            "presence-set" => {
                let mut conns = h.conns.lock().unwrap();
                if let Some(c) = conns.get_mut(&key).filter(|c| c.id == my_id) {
                    if let Some(b) = env["payload"]["remoteControlEnabled"].as_bool() {
                        c.remote_control = b;
                    }
                    if let Some(b) = env["payload"]["busy"].as_bool() {
                        c.busy = b;
                    }
                }
                h.broadcast(&user, &device, &conns);
            }
            k if ROUTED.contains(&k) => {
                let dst = env["dst"].as_str().unwrap_or("").to_string();
                let conns = h.conns.lock().unwrap();
                let err = |code: &str, msg: &str| {
                    let _ = tx.send(frame("relay-error", env.get("id"), json!({ "code": code, "message": msg, "dst": dst })));
                };
                match conns.get(&(user.clone(), dst.clone())) {
                    None => err("DEVICE_OFFLINE", "target device is offline"),
                    Some(t) if CONTROL.contains(&k) && !t.remote_control => err("REMOTE_DISABLED", "target device does not allow remote control"),
                    Some(t) => {
                        let mut fwd = json!({ "v": PROTOCOL_VERSION, "kind": k, "src": device, "dst": dst });
                        for f in ["id", "payload", "auth"] {
                            if let Some(v) = env.get(f) {
                                fwd[f] = v.clone();
                            }
                        }
                        let _ = t.tx.send(Message::Text(fwd.to_string().into()));
                    }
                }
            }
            _ => {}
        }
    }
    {
        let mut conns = h.conns.lock().unwrap();
        if conns.get(&key).is_some_and(|c| c.id == my_id) {
            conns.remove(&key);
            h.db.touch_device(&user, &device);
            h.broadcast(&user, &device, &conns);
        }
    }
    // 放掉发送端，让写任务把排队的帧（含关闭帧）发完再结束；设个上限防止对端不读而挂住。
    drop(tx);
    let _ = tokio::time::timeout(std::time::Duration::from_secs(2), writer).await;
}

pub fn routes(h: Arc<Hub>) -> Router {
    Router::new().route("/api/device-link/devices", get(devices)).route("/api/device-link/ws", get(ws_upgrade)).with_state(h)
}
