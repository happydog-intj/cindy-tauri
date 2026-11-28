use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio_tungstenite::{connect_async, tungstenite::{client::IntoClientRequest, Message}};

async fn start(allowed: &[&str]) -> String {
    let dir = std::env::temp_dir().join(format!("cindy-server-{}-{}", std::process::id(), rand_id()));
    std::fs::create_dir_all(&dir).unwrap();
    let app = cindy_server::app(cindy_server::Config {
        db_path: dir.join("t.db").to_string_lossy().into(),
        allowed_emails: allowed.iter().map(|s| s.to_string()).collect(),
        fixed_code: Some("123456".into()),
        preset_tokens: vec![],
    })
    .unwrap();
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    format!("127.0.0.1:{}", addr.port())
}

fn rand_id() -> u64 {
    static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    N.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
}

async fn post(base: &str, path: &str, body: Value, token: Option<&str>) -> (u16, Value) {
    let mut r = reqwest::Client::new().post(format!("http://{base}{path}")).json(&body);
    if let Some(t) = token {
        r = r.bearer_auth(t);
    }
    let res = r.send().await.unwrap();
    (res.status().as_u16(), res.json().await.unwrap_or(Value::Null))
}

async fn login(base: &str, email: &str, device: &str) -> Value {
    assert_eq!(post(base, "/api/auth/email/request-code", json!({ "email": email }), None).await.0, 200);
    let (st, v) = post(base, "/api/auth/email/verify-code", json!({ "email": email, "code": "123456", "deviceId": device, "clientType": "desktop" }), None).await;
    assert_eq!(st, 200, "{v}");
    v
}

type Ws = tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

async fn connect(base: &str, token: &str) -> Result<Ws, String> {
    let mut req = format!("ws://{base}/api/device-link/ws").into_client_request().unwrap();
    req.headers_mut().insert("authorization", format!("Bearer {token}").parse().unwrap());
    connect_async(req).await.map(|(w, _)| w).map_err(|e| e.to_string())
}

async fn hello(ws: &mut Ws, name: &str, remote: bool) {
    ws.send(Message::text(json!({ "v": 1, "kind": "hello", "payload": { "deviceName": name, "platform": "test", "appVersion": "1", "remoteControlEnabled": remote } }).to_string())).await.unwrap();
}

async fn next_kind(ws: &mut Ws, kind: &str) -> Value {
    for _ in 0..20 {
        let m = tokio::time::timeout(std::time::Duration::from_secs(3), ws.next()).await.expect("超时").unwrap().unwrap();
        if let Message::Text(t) = m {
            let v: Value = serde_json::from_str(&t).unwrap();
            if v["kind"] == kind {
                return v;
            }
        }
    }
    panic!("没等到 {kind}");
}

#[tokio::test]
async fn login_refresh_rotation_and_logout() {
    let base = start(&["me@example.com"]).await;
    let a = login(&base, "Me@Example.com", "dev-1").await;
    assert_eq!(a["membership"]["email"], "me@example.com");
    let at = a["accessToken"].as_str().unwrap();
    assert_eq!(at.split('.').count(), 3);

    let rt = a["refreshToken"].as_str().unwrap();
    let (st, b) = post(&base, "/api/auth/refresh", json!({ "refreshToken": rt }), None).await;
    assert_eq!(st, 200);
    // 刷新令牌一次性：旧的立刻失效
    assert_eq!(post(&base, "/api/auth/refresh", json!({ "refreshToken": rt }), None).await.0, 401);
    assert_ne!(b["refreshToken"], a["refreshToken"]);

    assert_eq!(post(&base, "/api/auth/logout", json!({}), Some(b["accessToken"].as_str().unwrap())).await.0, 200);
    assert_eq!(post(&base, "/api/auth/logout", json!({}), Some(b["accessToken"].as_str().unwrap())).await.0, 401);
    assert_eq!(post(&base, "/api/auth/refresh", json!({ "refreshToken": b["refreshToken"] }), None).await.0, 401);
}

#[tokio::test]
async fn code_rules() {
    let base = start(&["me@example.com"]).await;
    let req = |e: &str| post(&base, "/api/auth/email/request-code", json!({ "email": e }), None);
    assert_eq!(req("me@example.com").await.0, 200);
    assert_eq!(req("me@example.com").await.0, 429, "30 秒内不能重复请求");
    assert_eq!(req("not-an-email").await.0, 400);
    let verify = |c: &str| post(&base, "/api/auth/email/verify-code", json!({ "email": "me@example.com", "code": c, "deviceId": "d", "clientType": "desktop" }), None);
    for _ in 0..5 {
        assert_eq!(verify("000000").await.1["error"]["code"], "INVALID_CODE");
    }
    // 错满 5 次后验证码作废，正确码也不行
    assert_eq!(verify("123456").await.0, 400);
    // 不在名单：表面照常 sent，但没有验证码可用
    assert_eq!(req("other@example.com").await.0, 200);
    let (st, _) = post(&base, "/api/auth/email/verify-code", json!({ "email": "other@example.com", "code": "123456", "deviceId": "d", "clientType": "desktop" }), None).await;
    assert_eq!(st, 400);
    assert_eq!(post(&base, "/api/auth/email/verify-code", json!({ "email": "me@example.com", "code": "123456", "deviceId": "d" }), None).await.0, 400);
}

#[tokio::test]
async fn relay_routes_between_devices_of_one_account() {
    let base = start(&["me@example.com", "you@example.com"]).await;
    let desk = login(&base, "me@example.com", "desk").await;
    let phone = login(&base, "me@example.com", "phone").await;
    let other = login(&base, "you@example.com", "other").await;
    let (dt, pt, ot) = (desk["accessToken"].as_str().unwrap(), phone["accessToken"].as_str().unwrap(), other["accessToken"].as_str().unwrap());

    assert!(connect(&base, "bad").await.is_err(), "无效令牌不能建立连接");

    let mut d = connect(&base, dt).await.unwrap();
    hello(&mut d, "桌面", true).await;
    let ack = next_kind(&mut d, "hello-ack").await;
    assert_eq!(ack["payload"]["deviceId"], "desk");

    let mut p = connect(&base, pt).await.unwrap();
    hello(&mut p, "手机", false).await;
    next_kind(&mut p, "hello-ack").await;
    // 桌面收到手机上线；手机收到桌面的在线状态
    assert_eq!(next_kind(&mut d, "presence-changed").await["payload"]["deviceId"], "phone");
    assert_eq!(next_kind(&mut p, "presence-changed").await["payload"]["deviceId"], "desk");

    // 手机 → 桌面：src 由服务端改写，不信客户端自报
    p.send(Message::text(json!({ "v": 1, "kind": "link-open", "id": "1", "src": "forged", "dst": "desk", "payload": { "x": 1 } }).to_string())).await.unwrap();
    let got = next_kind(&mut d, "link-open").await;
    assert_eq!((got["src"].as_str(), got["dst"].as_str(), got["id"].as_str()), (Some("phone"), Some("desk"), Some("1")));
    assert_eq!(got["payload"]["x"], 1);

    // 桌面 → 手机：手机没开远控，控制类帧被拒，普通帧放行
    d.send(Message::text(json!({ "v": 1, "kind": "invoke", "id": "2", "dst": "phone" }).to_string())).await.unwrap();
    assert_eq!(next_kind(&mut d, "relay-error").await["payload"]["code"], "REMOTE_DISABLED");
    d.send(Message::text(json!({ "v": 1, "kind": "push", "dst": "phone", "payload": "hi" }).to_string())).await.unwrap();
    assert_eq!(next_kind(&mut p, "push").await["payload"], "hi");

    // 目标不在线
    d.send(Message::text(json!({ "v": 1, "kind": "push", "id": "3", "dst": "ghost" }).to_string())).await.unwrap();
    assert_eq!(next_kind(&mut d, "relay-error").await["payload"]["code"], "DEVICE_OFFLINE");

    // 跨账号隔离：别的账号既看不到设备，也发不进来
    let mut o = connect(&base, ot).await.unwrap();
    hello(&mut o, "别人", true).await;
    next_kind(&mut o, "hello-ack").await;
    o.send(Message::text(json!({ "v": 1, "kind": "push", "id": "4", "dst": "desk" }).to_string())).await.unwrap();
    assert_eq!(next_kind(&mut o, "relay-error").await["payload"]["code"], "DEVICE_OFFLINE");

    // 设备列表只含本账号设备，isSelf 正确
    let res = reqwest::Client::new().get(format!("http://{base}/api/device-link/devices")).bearer_auth(dt).send().await.unwrap();
    let list: Value = res.json().await.unwrap();
    let ids: Vec<&str> = list["devices"].as_array().unwrap().iter().map(|x| x["deviceId"].as_str().unwrap()).collect();
    assert!(ids.contains(&"desk") && ids.contains(&"phone") && !ids.contains(&"other"), "{ids:?}");
    let me = list["devices"].as_array().unwrap().iter().find(|x| x["deviceId"] == "desk").unwrap();
    assert_eq!((me["isSelf"].clone(), me["online"].clone()), (json!(true), json!(true)));
    assert!(me["lastSeenAt"].as_str().unwrap().ends_with('Z'));

    // 同一设备重连会顶掉旧连接（4409）
    let mut d2 = connect(&base, dt).await.unwrap();
    hello(&mut d2, "桌面", true).await;
    next_kind(&mut d2, "hello-ack").await;
    let closed = tokio::time::timeout(std::time::Duration::from_secs(3), async {
        loop {
            match d.next().await {
                Some(Ok(Message::Close(Some(f)))) => break u16::from(f.code),
                Some(Ok(_)) => continue,
                _ => break 0,
            }
        }
    })
    .await
    .unwrap();
    assert_eq!(closed, 4409);

    // 先发 hello 之前发别的帧会被关闭（4401）
    let mut bad = connect(&base, pt).await.unwrap();
    bad.send(Message::text(json!({ "v": 1, "kind": "ping" }).to_string())).await.unwrap();
    let code = loop {
        match bad.next().await {
            Some(Ok(Message::Close(Some(f)))) => break u16::from(f.code),
            Some(Ok(_)) => continue,
            _ => break 0,
        }
    };
    assert_eq!(code, 4401);
}
