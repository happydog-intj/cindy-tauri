// Manager 对假 Env + 本机 mock auth-server：登录、续期、失效、退出、本机模式、并发语义。
use super::backoff::Backoff;
use super::client_tests::{membership, ok, serve, Handler};
use super::manager::*;
use crate::owner::{Mode, Owner};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

struct FakeEnv {
    owner: Owner,
    base: String,
    secrets: Mutex<HashMap<String, String>>,
    ready: Mutex<bool>,
    pushes: Mutex<Vec<(String, Value)>>,
    opened: Mutex<Vec<String>>,
    open_hook: Mutex<Option<Box<dyn Fn(&str) + Send + Sync>>>,
}

impl Env for FakeEnv {
    fn mode(&self) -> Mode { self.owner.mode() }
    fn cached_user(&self) -> Option<Value> { self.owner.cached_user() }
    fn stamp(&self) -> Value { self.owner.stamp() }
    fn commit(&self, m: Mode, u: Option<&Value>) -> Result<(), String> { self.owner.commit(m, u).map(|_| ()) }
    fn secrets_ready(&self) -> BoxFut<bool> {
        let r = *self.ready.lock().unwrap();
        Box::pin(async move { r })
    }
    fn secret_get(&self, n: &str) -> Option<String> { self.secrets.lock().unwrap().get(n).cloned() }
    fn secret_has(&self, n: &str) -> bool { self.secrets.lock().unwrap().contains_key(n) }
    fn secret_set(&self, n: &str, v: &str) -> Result<(), String> {
        if !*self.ready.lock().unwrap() { return Err("locked".into()); }
        self.secrets.lock().unwrap().insert(n.into(), v.into());
        Ok(())
    }
    fn secret_remove(&self, n: &str) -> Result<(), String> { self.secrets.lock().unwrap().remove(n); Ok(()) }
    fn emit(&self, c: &str, a: Value) { self.pushes.lock().unwrap().push((c.into(), a)); }
    fn open_url(&self, url: &str) -> BoxFut<Result<(), String>> {
        self.opened.lock().unwrap().push(url.into());
        if let Some(h) = self.open_hook.lock().unwrap().as_ref() { h(url); }
        Box::pin(async { Ok(()) })
    }
    fn locale(&self) -> String { "en".into() }
    fn auth_base_url(&self) -> String { self.base.clone() }
    fn desktop_callback_url(&self) -> String { format!("{}/api/auth/desktop/callback", self.base) }
    fn device_id(&self) -> String { "dev-1".into() }
}

fn jwt(exp: i64) -> String {
    let p = URL_SAFE_NO_PAD.encode(json!({ "exp": exp, "orgSlug": "acme" }).to_string());
    format!("h.{p}.s")
}

fn now_s() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() as i64
}

fn dir(n: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("auth-mgr-{n}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn setup(n: &str, base: &str, mode_cloud: bool) -> (Arc<Manager>, Arc<FakeEnv>) {
    let owner = Owner::new(&dir(n));
    if mode_cloud {
        owner.commit(Mode::Cloud, Some(&json!({ "id": "m1", "name": "A", "membershipKind": "personal" }))).unwrap();
    }
    let env = Arc::new(FakeEnv {
        owner, base: base.into(), secrets: Mutex::new(HashMap::new()), ready: Mutex::new(true),
        pushes: Mutex::new(vec![]), opened: Mutex::new(vec![]), open_hook: Mutex::new(None),
    });
    let m = Manager::new(env.clone(), Backoff::system(), tokio::runtime::Handle::current());
    (m, env)
}

fn providers() -> Value {
    json!({"region":"global","attribution":"email","email":true,"phone":false,"social":["google"]})
}

fn pair(access: &str, refresh: &str) -> Value {
    json!({"accessToken": access, "refreshToken": refresh, "membership": membership()})
}

#[tokio::test]
async fn email_code_login_logout_and_local_mode() {
    let access = jwt(now_s() + 3600);
    let a2 = access.clone();
    let h: Handler = Arc::new(move |r| match r.path.as_str() {
        "/api/auth/providers" => ok(providers()),
        "/api/auth/discovery" => ok(json!({"methods":[{"type":"email_code"}]})),
        "/api/auth/sso/discovery" => (404, vec![], json!({"error":{"code":"ORG_SSO_NOT_FOUND"}}).to_string()),
        "/api/auth/email/request-code" => ok(json!({"status":"sent"})),
        "/api/auth/email/verify-code" => ok({ let mut o = pair(&a2, "rt-1"); o["status"] = json!("ok"); o }),
        "/api/auth/logout" => ok(json!({"status":"ok"})),
        _ => (404, vec![], "{}".into()),
    });
    let (base, seen) = serve(h).await;
    let (m, env) = setup("login", &base, false);
    env.owner.commit(Mode::SignedOut, None).unwrap();

    let r = m.dispatch_login_action(&json!({"type":"discover","email":" A@B.co "})).await;
    assert_eq!(r["success"], true, "{r}");
    assert_eq!(r["state"], json!({"step":"method-choice","email":"a@b.co","methods":[{"type":"email_code"}]}));
    let r = m.dispatch_login_action(&json!({"type":"request-code","kind":"email","identifier":"a@b.co","captchaToken":"cap"})).await;
    assert_eq!(r["state"]["step"], "verification-code");
    let r = m.dispatch_login_action(&json!({"type":"verify-code","kind":"email","identifier":"a@b.co","code":"123456"})).await;
    assert_eq!(r["state"]["step"], "completed", "{r}");

    let snap = m.snapshot();
    assert_eq!((snap["mode"].as_str(), snap["isAuthenticated"].clone()), (Some("cloud"), json!(true)));
    assert_eq!(snap["user"]["orgSlug"], "acme");
    assert_eq!(snap["user"]["name"], "A");
    assert_eq!(m.access_token().as_deref(), Some(access.as_str()));
    let info = m.info();
    assert!(info.authenticated && info.user_id.as_deref() == Some("m1"));
    // refresh token 只落密钥库，且是 session record 格式。
    let rec = env.secret_get("cindy_auth_session_v1").unwrap();
    assert_eq!(super::flow::parse_session_record(Some(&rec)), Some(("global".into(), "rt-1".into())));
    assert!(env.pushes.lock().unwrap().iter().any(|(c, _)| c == "auth:state-change"));
    // 请求体：captcha 随 request-code 带上。
    assert!(seen.lock().unwrap().iter().any(|s| s.path.ends_with("request-code") && s.body.contains("\"captchaToken\":\"cap\"")));

    m.logout().await.unwrap();
    assert_eq!(env.mode(), Mode::SignedOut);
    assert!(env.secret_get("cindy_auth_session_v1").is_none());
    assert!(seen.lock().unwrap().iter().any(|s| s.path == "/api/auth/logout" && s.auth.as_deref() == Some(&format!("Bearer {access}"))));
    assert!(!m.info().authenticated);
    assert!(m.logout().await.unwrap_err().starts_with("[UNAUTHENTICATED]"));

    let snap = m.enter_local();
    assert_eq!(snap["mode"], "local");
    assert_eq!(m.exit_local().unwrap()["mode"], "signed-out");
    assert!(m.exit_local().unwrap_err().starts_with("[PRECONDITION_FAILED]"));
}

#[tokio::test]
async fn cold_start_refresh_rotates_and_expires() {
    let access = jwt(now_s() + 3600);
    let calls = Arc::new(Mutex::new(0));
    let (c2, a2) = (calls.clone(), access.clone());
    let h: Handler = Arc::new(move |r| {
        assert_eq!(r.path, "/api/auth/refresh");
        let mut n = c2.lock().unwrap();
        *n += 1;
        if *n <= 2 { ok(pair(&a2, "rt-new")) } else { (401, vec![], json!({"error":{"code":"INVALID_REFRESH_TOKEN"}}).to_string()) }
    });
    let (base, seen) = serve(h).await;
    let (m, env) = setup("cold", &base, true);
    env.secret_set("cindy_auth_session_v1", &super::flow::serialize_session_record("global", "rt-old")).unwrap();

    let snap = m.initialize().await;
    assert_eq!(snap["isAuthenticated"], true, "{snap}");
    let body: Value = serde_json::from_str(&seen.lock().unwrap()[0].body).unwrap();
    assert_eq!(body, json!({"refreshToken":"rt-old","deviceId":"dev-1"}));
    let rec = env.secret_get("cindy_auth_session_v1").unwrap();
    assert_eq!(super::flow::parse_session_record(Some(&rec)).unwrap().1, "rt-new");

    // 并发 refresh 共用同一次请求。
    let (a, b) = tokio::join!(m.refresh(), m.refresh());
    assert!(a && b);
    assert_eq!(*calls.lock().unwrap(), 2);

    // 第三次：服务端判定失效 → 回到未登录，删 refresh token，发 session-expired。
    assert!(!m.refresh().await);
    assert_eq!(env.mode(), Mode::SignedOut);
    assert!(env.secret_get("cindy_auth_session_v1").is_none());
    let pushes = env.pushes.lock().unwrap().clone();
    let expired = pushes.iter().find(|(c, _)| c == "auth:session-expired").expect("session-expired");
    assert_eq!(expired.1, json!({"message":"","reason":"replaced-elsewhere"}));
}

#[tokio::test]
async fn cold_start_missing_credential_and_transient_failures() {
    let h: Handler = Arc::new(|_| (503, vec![], json!({"code":"BUSY"}).to_string()));
    let (base, _) = serve(h).await;
    // 云端模式但没有 refresh token：凭据丢失，静默回到未登录（冷启动不发 session-expired）。
    let (m, env) = setup("missing", &base, true);
    let snap = m.initialize().await;
    assert_eq!(snap["mode"], "signed-out");
    assert!(!env.pushes.lock().unwrap().iter().any(|(c, _)| c == "auth:session-expired"));
    // 瞬时故障（503）：保持云端模式与 refresh token，不踢人。
    let (m, env) = setup("transient", &base, true);
    env.secret_set("cindy_auth_session_v1", &super::flow::serialize_session_record("global", "rt")).unwrap();
    let snap = m.initialize().await;
    assert_eq!((snap["mode"].as_str(), snap["isAuthenticated"].clone()), (Some("cloud"), json!(false)));
    assert_eq!(snap["user"]["name"], "A", "冷启动用缓存的展示信息");
    assert!(env.secret_get("cindy_auth_session_v1").is_some());
    // 密钥库不可用：标记 credentialStoreUnavailable，同样不踢人。
    let (m, env) = setup("locked", &base, true);
    *env.ready.lock().unwrap() = false;
    let snap = m.initialize().await;
    assert_eq!(snap["credentialStoreUnavailable"], true);
    assert_eq!(snap["mode"], "cloud");
}

#[tokio::test]
async fn browser_login_polls_hosted_callback() {
    let access = jwt(now_s() + 3600);
    let polls = Arc::new(Mutex::new(0));
    let (p2, a2) = (polls.clone(), access.clone());
    let h: Handler = Arc::new(move |r| match r.path.as_str() {
        "/api/auth/providers" => ok(providers()),
        "/api/auth/desktop/callback/poll" => {
            let mut n = p2.lock().unwrap();
            *n += 1;
            if *n < 2 { ok(json!({"status":"pending"})) } else { ok(json!({"status":"ok","code":"auth-code"})) }
        }
        "/api/auth/token" => ok({ let mut o = pair(&a2, "rt-b"); o["status"] = json!("ok"); o }),
        _ => (404, vec![], "{}".into()),
    });
    let (base, seen) = serve(h).await;
    let (m, env) = setup("browser", &base, false);
    env.owner.commit(Mode::SignedOut, None).unwrap();
    let r = m.dispatch_login_action(&json!({"type":"start-browser","kind":"social","providerOrConnectionId":"google","label":"Google"})).await;
    assert_eq!(r["state"]["step"], "completed", "{r}");
    let url = url::Url::parse(&env.opened.lock().unwrap()[0]).unwrap();
    let q: HashMap<_, _> = url.query_pairs().into_owned().collect();
    assert_eq!(url.path(), "/api/auth/social/google/authorize");
    assert_eq!(q["code_challenge_method"], "S256");
    // client_state 是 pollSecret 的哈希：poll 请求里的 pollSecret 能还原它。
    let poll = seen.lock().unwrap().iter().find(|s| s.path.ends_with("/poll")).cloned().unwrap();
    let secret = serde_json::from_str::<Value>(&poll.body).unwrap()["pollSecret"].as_str().unwrap().to_string();
    assert_eq!(super::hosted::derive_client_state(&secret), q["client_state"]);
    // PKCE：token 请求里的 verifier 对应 authorize 里的 challenge。
    let tok = seen.lock().unwrap().iter().find(|s| s.path == "/api/auth/token").cloned().unwrap();
    let verifier = serde_json::from_str::<Value>(&tok.body).unwrap()["codeVerifier"].as_str().unwrap().to_string();
    use sha2::{Digest, Sha256};
    assert_eq!(URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes())), q["code_challenge"]);
    assert_eq!(m.snapshot()["mode"], "cloud");
}

#[tokio::test]
async fn busy_back_and_cancel_browser() {
    let h: Handler = Arc::new(|r| match r.path.as_str() {
        "/api/auth/providers" => ok(providers()),
        "/api/auth/desktop/callback/poll" => ok(json!({"status":"pending"})),
        _ => (404, vec![], "{}".into()),
    });
    let (base, _) = serve(h).await;
    let (m, env) = setup("busy", &base, false);
    env.owner.commit(Mode::SignedOut, None).unwrap();
    assert_eq!(m.dispatch_login_action(&json!({"type":"cancel-browser"})).await["code"], "NO_BROWSER_AUTH_IN_PROGRESS");
    assert_eq!(m.dispatch_login_action(&json!({"type":"nope"})).await["code"], "INVALID_AUTH_ACTION");

    let m2 = m.clone();
    let start = tokio::spawn(async move {
        m2.dispatch_login_action(&json!({"type":"start-browser","kind":"social","providerOrConnectionId":"google","label":"G"})).await
    });
    for _ in 0..100 {
        if !env.opened.lock().unwrap().is_empty() { break; }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    // 动作进行中：其他动作 LOGIN_BUSY，back 在没封口时可用。
    assert_eq!(m.dispatch_login_action(&json!({"type":"discover","email":"a@b.co"})).await["code"], "LOGIN_BUSY");
    let r = m.dispatch_login_action(&json!({"type":"cancel-browser"})).await;
    assert_eq!(r["success"], true, "{r}");
    let done = start.await.unwrap();
    assert_eq!((done["success"].clone(), done["code"].as_str()), (json!(false), Some("USER_CANCELLED")));
    let r = m.dispatch_login_action(&json!({"type":"back"})).await;
    assert_eq!(r["state"]["step"], "identifier");
}

#[tokio::test]
async fn provider_failure_and_unsupported_channels() {
    let h: Handler = Arc::new(|_| (429, vec![("retry-after".into(), "30".into())], json!({"code":"X"}).to_string()));
    let (base, _) = serve(h).await;
    let (m, env) = setup("fail", &base, false);
    env.owner.commit(Mode::SignedOut, None).unwrap();
    let r = m.get_login_state().await;
    assert_eq!((r["success"].clone(), r["code"].as_str()), (json!(false), Some("RATE_LIMITED")));
    assert!(r["retryAt"].as_f64().unwrap() > 0.0);
    assert_eq!(r["state"]["step"], "error");
    assert_eq!(m.invoke("auth:accounts:list", &[]).await.unwrap().unwrap(), json!({"accounts":[],"mutationAllowed":false}));
    assert!(m.invoke("auth:accounts:switch", &[]).await.unwrap().unwrap_err().starts_with("[NOT_SUPPORTED]"));
    assert_eq!(m.invoke("auth:account-deletion:get-status", &[]).await.unwrap().unwrap(), json!({"success":true,"value":null}));
    assert!(m.invoke("auth:nope", &[]).await.is_none());
    assert_eq!(m.invoke_sync("auth:has-persisted-session-hint-sync"), Some(json!(false)));
}
