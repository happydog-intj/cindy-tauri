// AuthClient 对本机 mock HTTP 服务：请求形状、错误归类、429、非法 JSON、超时、取消。
use super::client::*;
use super::hosted::Cancel;
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

#[derive(Clone, Debug)]
pub(super) struct Seen {
    pub(super) method: String,
    pub(super) path: String,
    pub(super) auth: Option<String>,
    pub(super) body: String,
}

pub(super) type Handler = Arc<dyn Fn(&Seen) -> (u16, Vec<(String, String)>, String) + Send + Sync>;

pub(super) async fn serve(handler: Handler) -> (String, Arc<Mutex<Vec<Seen>>>) {
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let s2 = seen.clone();
    tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = l.accept().await else { return };
            let (h, s2) = (handler.clone(), s2.clone());
            tokio::spawn(async move {
                let mut buf = Vec::new();
                let mut tmp = [0u8; 4096];
                let (head_end, clen) = loop {
                    let n = sock.read(&mut tmp).await.unwrap_or(0);
                    if n == 0 { return; }
                    buf.extend_from_slice(&tmp[..n]);
                    if let Some(p) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                        let head = String::from_utf8_lossy(&buf[..p]).to_lowercase();
                        let cl = head.lines().find_map(|l| l.strip_prefix("content-length:")).and_then(|v| v.trim().parse::<usize>().ok()).unwrap_or(0);
                        break (p + 4, cl);
                    }
                };
                while buf.len() < head_end + clen {
                    let n = sock.read(&mut tmp).await.unwrap_or(0);
                    if n == 0 { break; }
                    buf.extend_from_slice(&tmp[..n]);
                }
                let head = String::from_utf8_lossy(&buf[..head_end]).to_string();
                let mut lines = head.lines();
                let first = lines.next().unwrap_or("").to_string();
                let mut parts = first.split(' ');
                let seen_req = Seen {
                    method: parts.next().unwrap_or("").into(),
                    path: parts.next().unwrap_or("").into(),
                    auth: lines.find_map(|l| l.to_lowercase().starts_with("authorization:").then(|| l[14..].trim().to_string())),
                    body: String::from_utf8_lossy(&buf[head_end..]).to_string(),
                };
                s2.lock().unwrap().push(seen_req.clone());
                let (status, headers, body) = h(&seen_req);
                if status == 0 {
                    // 约定：挂起不回复（超时 / 取消用例）。
                    tokio::time::sleep(std::time::Duration::from_secs(60)).await;
                    return;
                }
                let mut resp = format!("HTTP/1.1 {status} X\r\nContent-Length: {}\r\nConnection: close\r\n", body.len());
                for (k, v) in headers { resp += &format!("{k}: {v}\r\n"); }
                resp += "\r\n";
                resp += &body;
                let _ = sock.write_all(resp.as_bytes()).await;
            });
        }
    });
    (format!("http://{addr}"), seen)
}

fn client(base: &str, timeout_ms: u64) -> (AuthClient, Arc<Mutex<Vec<String>>>) {
    let logs = Arc::new(Mutex::new(Vec::new()));
    let l2 = logs.clone();
    let c = AuthClient::new(ClientOptions {
        base_url: format!("{base}//"),
        region: "global".into(),
        device_id: "dev-1".into(),
        client_type: "desktop".into(),
        locale: Some("en".into()),
        timeout_ms,
        on_request_error: Some(Arc::new(move |p, s, n| l2.lock().unwrap().push(format!("{p} {s} {n}")))),
    });
    (c, logs)
}

pub(super) fn ok(v: Value) -> (u16, Vec<(String, String)>, String) {
    (200, vec![], v.to_string())
}

pub(super) fn membership() -> Value {
    json!({"id":"m1","kind":"personal","role":"owner","displayName":"A","email":"a@b.co","orgId":null,"orgName":null})
}

#[tokio::test]
async fn request_shapes_and_success() {
    let h: Handler = Arc::new(|r| match r.path.as_str() {
        "/api/auth/refresh" => ok(json!({"accessToken":"a","refreshToken":"r2","membership":membership(),"extra":1})),
        "/api/auth/providers" => ok(json!({"region":"global","attribution":"email","email":true,"phone":false,"social":["google"]})),
        "/api/auth/email/request-code" => ok(json!({"status":"sent"})),
        "/api/auth/logout" => ok(json!({"status":"ok"})),
        _ => (404, vec![], json!({"error":{"code":"NOPE","message":"m"}}).to_string()),
    });
    let (base, seen) = serve(h).await;
    let (c, _) = client(&base, 5000);
    let pair = c.refresh("rt").await.unwrap();
    assert_eq!(pair["refreshToken"], "r2");
    assert!(pair.get("extra").is_none(), "未知字段被剥掉，与 zod 一致");
    let p = c.get_providers().await.unwrap();
    assert_eq!(p["social"], json!(["google"]));
    c.request_code("email", "a@b.co", Some("cap")).await.unwrap();
    c.request_code("email", "a@b.co", None).await.unwrap();
    c.logout("acc").await.unwrap();
    let s = seen.lock().unwrap().clone();
    assert_eq!(s[0].method, "POST");
    assert_eq!(serde_json::from_str::<Value>(&s[0].body).unwrap(), json!({"refreshToken":"rt","deviceId":"dev-1"}));
    assert_eq!((s[1].method.as_str(), s[1].body.as_str()), ("GET", ""));
    // 键序与 JS 对象展开一致：email, locale, captchaToken。
    assert_eq!(s[2].body, r#"{"email":"a@b.co","locale":"en","captchaToken":"cap"}"#);
    assert_eq!(s[3].body, r#"{"email":"a@b.co","locale":"en"}"#);
    assert_eq!(s[4].auth.as_deref(), Some("Bearer acc"));
    assert_eq!(s[4].body, "{}");
}

#[tokio::test]
async fn error_classification() {
    let h: Handler = Arc::new(|r| match r.path.as_str() {
        "/api/auth/refresh" => (401, vec![("x-request-id".into(), "req-9".into())], json!({"error":{"code":"INVALID_REFRESH_TOKEN","message":"bad"}}).to_string()),
        "/api/auth/logout" => (429, vec![("retry-after".into(), "30".into())], json!({"code":"DAILY_QUOTA"}).to_string()),
        "/api/auth/token" => (500, vec![], "<html>".into()),
        "/api/auth/select-account" => (429, vec![], "oops".into()),
        "/api/auth/sso/discovery" => ok(json!({"region":"cn","orgName":"O","connections":[{"connectionId":"c","protocol":"oidc","connectionName":"N"}]})),
        "/api/auth/binding/verify" => ok(json!({"status":"ok","bad":true})),
        "/api/auth/providers" => (403, vec![], json!({"code":"sk-secret"}).to_string()),
        _ => ok(json!({"region":"global","orgName":"O","connections":[]})),
    });
    let (base, _) = serve(h).await;
    let (c, logs) = client(&base, 5000);
    let e = c.refresh("x").await.unwrap_err();
    assert_eq!((e.code.as_str(), e.status, e.message.as_str(), e.request_id.as_deref()), ("INVALID_REFRESH_TOKEN", 401, "bad", Some("req-9")));
    let e = c.logout("t").await.unwrap_err();
    assert_eq!((e.code.as_str(), e.status), ("RATE_LIMITED", 429));
    assert_eq!(e.server_code.as_deref(), Some("DAILY_QUOTA"));
    assert!(e.retry_at.unwrap() > 0.0);
    let e = c.exchange_authorization_code("c", "v").await.unwrap_err();
    assert_eq!((e.code.as_str(), e.status), ("INVALID_RESPONSE", 500));
    let e = c.select_account("t", "a").await.unwrap_err();
    assert_eq!((e.code.as_str(), e.status), ("RATE_LIMITED", 429));
    let e = c.discover_sso_org("o").await.unwrap_err();
    assert_eq!(e.code, "REGION_MISMATCH");
    let e = c.verify_binding("t", "email", "a", "1").await.unwrap_err();
    assert_eq!((e.code.as_str(), e.status), ("INVALID_RESPONSE", 200));
    let e = c.get_providers().await.unwrap_err();
    assert_eq!(e.code, "AUTH_REQUEST_FAILED", "带密钥前缀的服务端错误码不外泄");
    assert!(logs.lock().unwrap().iter().any(|l| l.contains("SyntaxError")));
    assert!(logs.lock().unwrap().iter().all(|l| !l.contains("secret")));
}

#[tokio::test]
async fn sso_discovery_empty_connections() {
    let h: Handler = Arc::new(|_| ok(json!({"region":"global","orgName":"O","connections":[]})));
    let (base, _) = serve(h).await;
    let (c, _) = client(&base, 5000);
    assert_eq!(c.discover_sso_org("o").await.unwrap_err().code, "ORG_SSO_NOT_FOUND");
}

#[tokio::test]
async fn discover_methods() {
    let h: Handler = Arc::new(|r| {
        let b: Value = serde_json::from_str(&r.body).unwrap();
        match b["email"].as_str().unwrap() {
            "known" => ok(json!({"methods":[{"type":"future"},{"type":"email_code"}]})),
            "unknown" => ok(json!({"methods":[{"type":"future"}]})),
            "bad" => ok(json!({"methods":[{"type":"sso"}]})),
            _ => ok(json!({"methods":[]})),
        }
    });
    let (base, _) = serve(h).await;
    let (c, _) = client(&base, 5000);
    assert_eq!(c.discover("known").await.unwrap(), vec![json!({"type":"email_code"})]);
    assert_eq!(c.discover("unknown").await.unwrap_err().code, "INVALID_RESPONSE");
    assert_eq!(c.discover("bad").await.unwrap_err().code, "INVALID_RESPONSE");
    assert!(c.discover("empty").await.unwrap().is_empty());
}

#[tokio::test]
async fn timeout_cancel_and_network() {
    let h: Handler = Arc::new(|_| (0, vec![], String::new()));
    let (base, _) = serve(h).await;
    let (c, _) = client(&base, 150);
    assert_eq!(c.get_providers().await.unwrap_err().code, "REQUEST_TIMEOUT");
    // refresh 的 timeoutMs=0 表示不设超时：靠外部取消以外都不会返回，这里只验证 poll 的取消。
    let cancel = Cancel::new();
    let c2 = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        c2.abort();
    });
    let e = c.poll_desktop_authorization("s", &cancel).await.unwrap_err();
    assert_eq!(e.code, "USER_CANCELLED");
    // 已取消的 signal：立即 USER_CANCELLED。
    let e = c.poll_desktop_authorization("s", &cancel).await.unwrap_err();
    assert_eq!(e.code, "USER_CANCELLED");
    // 连不上：NETWORK_ERROR。
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let dead = format!("http://{}", l.local_addr().unwrap());
    drop(l);
    let (c3, logs) = client(&dead, 2000);
    let e = c3.get_providers().await.unwrap_err();
    assert_eq!((e.code.as_str(), e.status), ("NETWORK_ERROR", 0));
    assert!(!logs.lock().unwrap().is_empty());
}

#[test]
fn authorize_url_matches_js() {
    let g: Value = serde_json::from_str(include_str!("../auth_golden.json")).unwrap();
    let cases = g["urlCases"].as_array().unwrap();
    assert_eq!(cases.len(), 40);
    for c in cases {
        let locale = c["locale"].as_str().filter(|l| *l != "\u{0}undef").map(String::from);
        let cl = AuthClient::new(ClientOptions {
            base_url: c["base"].as_str().unwrap().into(),
            region: "global".into(),
            device_id: "dev-1".into(),
            client_type: "desktop".into(),
            locale,
            timeout_ms: 1,
            on_request_error: None,
        });
        let got = cl.build_authorize_url(c["kind"].as_str().unwrap(), c["id"].as_str().unwrap(), c["redirect"].as_str().unwrap(), c["challenge"].as_str().unwrap(), c["state"].as_str().unwrap());
        assert_eq!(got, c["output"].as_str().unwrap(), "{c}");
    }
}
