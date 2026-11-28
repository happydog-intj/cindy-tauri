//! 端到端：Rust 账号会话 + mock 云端 / mock 网关。
//! 迁自 scripts/auth-e2e.mjs：登录归 Rust 后，旧脚本改由这里驱动。需要 NODE_BIN / MOCK_OPENAI，缺一就跳过。

#![cfg(test)]

use crate::rust_session_e2e::{env, Env, Rig};
use serde_json::{json, Value};
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

fn spawn_mock(e: &Env, script: &Path, envs: &[(&str, String)]) -> (std::process::Child, u16) {
    let mut cmd = Command::new(&e.node);
    cmd.arg(script).stdout(Stdio::piped()).stderr(Stdio::null());
    for (k, v) in envs {
        cmd.env(k, v);
    }
    let mut child = cmd.spawn().expect("spawn mock");
    let mut line = String::new();
    BufReader::new(child.stdout.as_mut().unwrap()).read_line(&mut line).unwrap();
    let port = serde_json::from_str::<Value>(&line).unwrap()["port"].as_u64().unwrap() as u16;
    (child, port)
}

fn temp(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("auth-e2e-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn wait<F: FnMut() -> bool>(what: &str, secs: u64, mut f: F) {
    let t0 = Instant::now();
    while !f() {
        assert!(t0.elapsed() < Duration::from_secs(secs), "timeout: {what}");
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn http(port: u16, method: &str, path: &str, body: Option<&str>) -> Value {
    use std::io::{Read, Write};
    let mut s = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
    let b = body.unwrap_or("");
    write!(s, "{method} {path} HTTP/1.1\r\nHost: x\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{b}", b.len()).unwrap();
    let mut out = String::new();
    s.read_to_string(&mut out).unwrap();
    serde_json::from_str(out.split("\r\n\r\n").nth(1).unwrap_or("null")).unwrap_or(Value::Null)
}

#[test]
fn account_login_lifecycle_against_the_mock_cloud() {
    let Some(e) = env() else { eprintln!("skip: PI_RS_BIN/MOCK_OPENAI not set"); return };
    const GW_KEY: &str = "gw-auth-e2e-0123456789ab";
    const GW_MODEL: &str = "mock-gw-model";
    let scripts = Path::new(&e.mock).parent().unwrap().to_path_buf();
    let (mut gateway, gw_port) = spawn_mock(&e, Path::new(&e.mock), &[("MOCK_API_KEY", GW_KEY.into()), ("MOCK_MODEL_ID", GW_MODEL.into())]);
    let (mut cloud, cloud_port) = spawn_mock(
        &e,
        &scripts.join("mock-cindy-cloud.mjs"),
        &[("MOCK_GATEWAY_ENDPOINT", format!("http://127.0.0.1:{gw_port}/keyed")), ("MOCK_GATEWAY_KEY", GW_KEY.into())],
    );
    let data = temp("lifecycle");
    let base = format!("http://127.0.0.1:{cloud_port}");
    let r = Rig::start_with_env(&e, gw_port, &data, &[("CINDY_TAURI_AUTH_BASE_URL", base.clone()), ("CINDY_TAURI_MODEL_ACCESS_BASE_URL", base.clone())]);
    let ipc = |c: &str, a: Value| r.ipc(c, a);
    let act = |a: Value| ipc("auth:dispatch-login-action", json!([a])).unwrap();
    let state = |p: &str| http(cloud_port, "GET", p, None);

    // 初始：本机模式。
    let s0 = ipc("auth:initialize", json!([])).unwrap();
    assert_eq!((s0["mode"].as_str(), s0["dataOwnerId"].as_str(), s0["isAuthenticated"].clone()), (Some("local"), Some("local-v1"), json!(false)));
    assert_eq!(ipc("auth:has-persisted-session-hint-sync", json!([])).ok(), None, "同步通道不走 invoke");
    // 退出本机模式 → 登录页。
    let s1 = ipc("auth:exit-local", json!([])).unwrap();
    assert_eq!((s1["mode"].as_str(), s1["dataOwnerId"].clone()), (Some("signed-out"), Value::Null));
    assert_eq!(s1["ownerGeneration"], s0["ownerGeneration"].as_i64().unwrap() + 1);
    let lp = ipc("auth:get-login-state", json!([])).unwrap();
    assert_eq!(lp["state"]["step"], "identifier", "{lp}");
    assert_eq!(ipc("auth:get-captcha-challenge-url", json!([])).unwrap(), json!(format!("{base}/captcha/turnstile")));

    // 邮箱验证码登录。
    let email = "e2e@example.com";
    assert_eq!(act(json!({"type":"discover","email":email}))["success"], true);
    let no_captcha = act(json!({"type":"request-code","kind":"email","identifier":email}));
    assert_eq!((no_captcha["success"].clone(), no_captcha["code"].as_str()), (json!(false), Some("CAPTCHA_REQUIRED")));
    let rc = act(json!({"type":"request-code","kind":"email","identifier":email,"captchaToken":"turnstile-ok"}));
    assert_eq!(rc["state"]["step"], "verification-code");
    let bad = act(json!({"type":"verify-code","kind":"email","identifier":email,"code":"000000"}));
    assert_eq!((bad["success"].clone(), bad["code"].as_str()), (json!(false), Some("INVALID_CODE")));
    let vc = act(json!({"type":"verify-code","kind":"email","identifier":email,"code":"123456"}));
    assert_eq!(vc["state"]["step"], "completed", "{vc}");
    let s2 = ipc("auth:initialize", json!([])).unwrap();
    assert_eq!((s2["mode"].as_str(), s2["isAuthenticated"].clone(), s2["user"]["email"].as_str()), (Some("cloud"), json!(true), Some(email)));
    assert_eq!(s2["ownerGeneration"], s1["ownerGeneration"].as_i64().unwrap() + 1);
    assert!(r.pushed("auth:state-change", |a| a["mode"] == "cloud"));
    let app_session = std::fs::read_to_string(data.join("app-session.json")).unwrap();
    assert!(app_session.contains("\"cloud\"") && !app_session.contains("accessToken") && !app_session.contains("refreshToken") && !app_session.contains("rt-"));

    // 云端模型：登录后同步凭据、模型清单、余额。
    wait("model-access ok", 30, || {
        let st = ipc("model-access:get-status", json!([])).unwrap_or(Value::Null);
        st["state"] == "ok" && st["accountTier"] == "paid"
    });
    assert_eq!(r.secrets.get("api_key").as_deref(), Some(GW_KEY), "renderer 能读到网关 key（desktop 对齐）");
    wait("xd provider with gateway model", 30, || {
        ipc("maker:provider:list", json!([{}])).map(|v| v.to_string().contains(GW_MODEL)).unwrap_or(false)
    });
    let bal = ipc("billing:get-balance", json!([])).unwrap();
    assert_eq!((bal["available"].as_str(), bal["scale"].clone()), (Some("4"), json!(9)));
    let usage = ipc("billing:get-credit-usage", json!([])).unwrap();
    assert_eq!((usage["available"].as_str(), usage["plan"]["total"].as_str()), (Some("4"), Some("2")));
    let launch = r.models.call("host.resolve-launch", &json!({ "providerId": "xd", "model": GW_MODEL })).unwrap().unwrap();
    assert!(launch["providerId"] == "cindy" && launch["keyKind"] == "gateway" && launch["key"] == GW_KEY, "{launch}");
    assert_eq!(launch["provider"]["apiKeyHeaders"], json!(["authorization", "x-api-key"]));
    assert!(!launch["provider"].to_string().contains(GW_KEY));

    // 退出。
    ipc("auth:logout", json!([])).unwrap();
    let out = ipc("auth:initialize", json!([])).unwrap();
    assert_eq!((out["mode"].as_str(), out["dataOwnerId"].clone()), (Some("signed-out"), Value::Null));
    assert_eq!(out["ownerGeneration"], s2["ownerGeneration"].as_i64().unwrap() + 1);
    let st = state("/__mock/state");
    assert_eq!((st["logout"].clone(), st["liveRefreshTokens"].clone()), (json!(1), json!(0)));
    wait("gateway key removed", 10, || r.secrets.get("api_key").is_none());
    assert!(!ipc("maker:provider:list", json!([{}])).unwrap().to_string().contains(GW_MODEL), "退出后 xd 的模型消失");
    assert!(ipc("billing:get-balance", json!([])).unwrap_err().contains("PERMISSION_DENIED"));

    // 浏览器登录（托管回调轮询）。
    ipc("auth:get-login-state", json!([])).unwrap();
    let before = r.opened.lock().unwrap().len();
    let a2 = r.auth.clone();
    let browser = std::thread::spawn(move || {
        tauri::async_runtime::block_on(a2.dispatch_login_action(&json!({"type":"start-browser","kind":"social","providerOrConnectionId":"google","label":"Google"})))
    });
    wait("open.url", 10, || r.opened.lock().unwrap().len() > before);
    let url = url::Url::parse(r.opened.lock().unwrap().last().unwrap()).unwrap();
    assert_eq!(url.path(), "/api/auth/social/google/authorize");
    let q: std::collections::HashMap<_, _> = url.query_pairs().into_owned().collect();
    assert_eq!(q["redirect_uri"], format!("{base}/api/auth/desktop/callback"));
    // 模拟浏览器打开授权页（mock 在这一步登记 clientState），再点同意。
    http(cloud_port, "GET", &format!("{}?{}", url.path(), url.query().unwrap_or("")), None);
    http(cloud_port, "POST", "/__mock/authorize", Some(&json!({ "clientState": q["client_state"] }).to_string()));
    let br = browser.join().unwrap();
    assert_eq!(br["state"]["step"], "completed", "{br}");
    let s3 = ipc("auth:initialize", json!([])).unwrap();
    assert_eq!((s3["mode"].as_str(), s3["user"]["email"].as_str()), (Some("cloud"), Some("browser@example.com")));
    wait("model-access ok again", 30, || ipc("model-access:get-status", json!([])).map(|v| v["state"] == "ok").unwrap_or(false));
    assert_eq!(r.secrets.get("api_key").as_deref(), Some(GW_KEY));

    // 续期：运行时续期换新 token；服务端判定失效 → 回到未登录并推 session-expired。
    let refresh_before = state("/__mock/state")["refresh"].as_i64().unwrap();
    assert_eq!(ipc("auth:refresh", json!([])).unwrap(), json!(true));
    let st = state("/__mock/state");
    assert_eq!((st["refresh"].as_i64().unwrap(), st["liveRefreshTokens"].clone()), (refresh_before + 1, json!(1)));
    http(cloud_port, "POST", "/__mock/refresh-fail", Some(r#"{"code":"INVALID_REFRESH_TOKEN"}"#));
    assert_eq!(ipc("auth:refresh", json!([])).unwrap(), json!(false));
    assert!(r.pushed("auth:session-expired", |a| a["reason"] == "replaced-elsewhere"));
    assert_eq!(ipc("auth:initialize", json!([])).unwrap()["mode"], "signed-out");
    wait("gateway key removed after expiry", 10, || r.secrets.get("api_key").is_none());

    // 本机模式与 add-account 登录。
    let s6 = ipc("auth:enter-local", json!([])).unwrap();
    assert_eq!((s6["mode"].as_str(), s6["dataOwnerId"].as_str()), (Some("local"), Some("local-v1")));
    assert!(ipc("auth:logout", json!([])).unwrap_err().contains("UNAUTHENTICATED"));
    let ba = ipc("auth:accounts:begin-add", json!([])).unwrap();
    assert_eq!((ba["success"].clone(), ba["state"]["step"].as_str()), (json!(true), Some("identifier")));
    assert_eq!(ipc("auth:initialize", json!([])).unwrap()["mode"], "local");
    ipc("auth:accounts:cancel-add", json!([])).unwrap();
    ipc("auth:accounts:begin-add", json!([])).unwrap();
    act(json!({"type":"request-code","kind":"email","identifier":email,"captchaToken":"turnstile-ok"}));
    let add = act(json!({"type":"verify-code","kind":"email","identifier":email,"code":"123456"}));
    assert_eq!(add["success"], true);
    assert_eq!(ipc("auth:initialize", json!([])).unwrap()["mode"], "cloud");
    ipc("auth:logout", json!([])).unwrap();
    ipc("auth:enter-local", json!([])).unwrap();
    assert_eq!(ipc("auth:accounts:list", json!([])).unwrap(), json!({"accounts":[],"mutationAllowed":false}));

    r.stop();
    let _ = gateway.kill();
    let _ = cloud.kill();
    let _ = std::fs::remove_dir_all(&data);
}

#[test]
fn cold_start_restores_the_session_from_the_stored_refresh_token() {
    let Some(e) = env() else { eprintln!("skip: PI_RS_BIN/MOCK_OPENAI not set"); return };
    let scripts = Path::new(&e.mock).parent().unwrap().to_path_buf();
    let (mut gateway, gw_port) = spawn_mock(&e, Path::new(&e.mock), &[("MOCK_API_KEY", "gw-cold-0123456789abcdef".into()), ("MOCK_MODEL_ID", "mock-gw-model".into())]);
    let (mut cloud, cloud_port) = spawn_mock(
        &e,
        &scripts.join("mock-cindy-cloud.mjs"),
        &[("MOCK_GATEWAY_ENDPOINT", format!("http://127.0.0.1:{gw_port}/keyed")), ("MOCK_GATEWAY_KEY", "gw-cold-0123456789abcdef".into())],
    );
    let data = temp("cold");
    let base = format!("http://127.0.0.1:{cloud_port}");
    let envs = [("CINDY_TAURI_AUTH_BASE_URL", base.clone()), ("CINDY_TAURI_MODEL_ACCESS_BASE_URL", base.clone())];

    let r = Rig::start_with_env(&e, gw_port, &data, &envs);
    r.ipc("auth:exit-local", json!([])).unwrap();
    r.ipc("auth:get-login-state", json!([])).unwrap();
    let act = |a: Value| r.ipc("auth:dispatch-login-action", json!([a])).unwrap();
    act(json!({"type":"request-code","kind":"email","identifier":"c@example.com","captchaToken":"turnstile-ok"}));
    assert_eq!(act(json!({"type":"verify-code","kind":"email","identifier":"c@example.com","code":"123456"}))["state"]["step"], "completed");
    let refresh_before = http(cloud_port, "GET", "/__mock/state", None)["refresh"].as_i64().unwrap();
    r.stop();

    // 重启：同一个数据目录，新的 Manager。冷启动用密钥库里的 refresh token 续期。
    let r2 = Rig::start_with_env(&e, gw_port, &data, &envs);
    let snap = r2.ipc("auth:initialize", json!([])).unwrap();
    assert_eq!((snap["mode"].as_str(), snap["isAuthenticated"].clone(), snap["user"]["email"].as_str()), (Some("cloud"), json!(true), Some("c@example.com")), "{snap}");
    assert_eq!(http(cloud_port, "GET", "/__mock/state", None)["refresh"].as_i64().unwrap(), refresh_before + 1, "冷启动只续期一次");
    assert_eq!(r2.ipc("auth:has-persisted-session-hint-sync", json!([])).is_err(), true);
    wait("model-access ok after restart", 30, || r2.ipc("model-access:get-status", json!([])).map(|v| v["state"] == "ok").unwrap_or(false));
    r2.stop();
    let _ = gateway.kill();
    let _ = cloud.kill();
    let _ = std::fs::remove_dir_all(&data);
}

/// 对着真实的 cindy-server 走一遍 request-code → verify-code（验证码取自服务端 stderr）。
/// 设置 `CINDY_SERVER_BIN` 才跑，默认跳过。
#[test]
fn login_against_cindy_server() {
    let Some(bin) = std::env::var("CINDY_SERVER_BIN").ok() else { eprintln!("skip: CINDY_SERVER_BIN not set"); return };
    let Some(e) = env() else { eprintln!("skip: PI_RS_BIN/MOCK_OPENAI not set"); return };
    let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    let db = std::env::temp_dir().join(format!("auth-e2e-server-{}-{port}.db", std::process::id()));
    let _ = std::fs::remove_file(&db);
    let mut server = std::process::Command::new(bin)
        .env("CINDY_SERVER_ADDR", format!("127.0.0.1:{port}"))
        .env("CINDY_SERVER_DB", db)
        .env("CINDY_SERVER_ALLOWED_EMAILS", "me@example.com")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawn cindy-server");
    let stderr = server.stderr.take().unwrap();
    let (tx, rx) = std::sync::mpsc::channel::<String>();
    std::thread::spawn(move || {
        use std::io::BufRead;
        for l in std::io::BufReader::new(stderr).lines().map_while(Result::ok) {
            let _ = tx.send(l);
        }
    });
    let t0 = std::time::Instant::now();
    while std::net::TcpStream::connect(("127.0.0.1", port)).is_err() {
        assert!(t0.elapsed() < Duration::from_secs(10), "cindy-server 没起来");
        std::thread::sleep(Duration::from_millis(50));
    }
    let base = format!("http://127.0.0.1:{port}");
    let data = temp("srv-login");
    let r = Rig::start_with_env(&e, 9, &data, &[("CINDY_TAURI_AUTH_BASE_URL", base.clone())]);
    r.ipc("auth:exit-local", json!([])).unwrap();
    let st = r.ipc("auth:get-login-state", json!([])).unwrap();
    assert_eq!(st["success"], true, "{st}");
    let act = |a: Value| r.ipc("auth:dispatch-login-action", json!([a])).unwrap();
    let d = act(json!({"type":"discover","email":"me@example.com"}));
    assert_eq!(d["state"]["step"], "method-choice", "{d}");
    let sent = act(json!({"type":"request-code","kind":"email","identifier":"me@example.com"}));
    assert_eq!(sent["state"]["step"], "verification-code", "request-code 应进入输码页：{sent}");
    let code = loop {
        let l = rx.recv_timeout(Duration::from_secs(5)).expect("服务端日志里没有验证码");
        if let Some(c) = l.split("登录验证码").nth(1).and_then(|r| r.split(": ").nth(1)).map(|r| r.chars().take(6).collect::<String>()) {
            break c;
        }
    };
    let done = act(json!({"type":"verify-code","kind":"email","identifier":"me@example.com","code":code}));
    assert_eq!(done["state"]["step"], "completed", "{done}");
    r.stop();
    let _ = server.kill();
}
