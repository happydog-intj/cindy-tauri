// 登录流程的纯逻辑：邮箱校验、reduceAuthFlow、loginPreparingErrorState、parseDesktopLoginAction、
// session record。逐行对应上游 auth-client（email.ts / types.ts / sessionRealm.ts）与 desktop shared/authIpc.ts。
use serde_json::{json, Map, Value};

fn is_ws(c: char) -> bool {
    crate::local_db::mapper::is_js_ws(c)
}

/// JS `String.length`（UTF-16 码元数）。
fn js_len(s: &str) -> usize {
    s.encode_utf16().count()
}

pub fn js_trim(s: &str) -> &str {
    crate::local_db::mapper::js_trim(s)
}

/// 等价 isValidEmail：trim 后 local@domain.tld，不含空白。
pub fn is_valid_email(value: &str) -> bool {
    let email = js_trim(value);
    if email.is_empty() || email.chars().any(is_ws) {
        return false;
    }
    let Some(at) = email.find('@') else { return false };
    if at == 0 || email[at + 1..].contains('@') {
        return false;
    }
    let domain = &email[at + 1..];
    // 比较用 UTF-16 下标：dot>0 且 dot<len-1 与字节下标等价（'.' 是单字节）。
    match domain.rfind('.') {
        Some(dot) => dot > 0 && dot < domain.len() - 1,
        None => false,
    }
}

/// JS 数字序列化：整数值不带小数点。
pub fn js_number(n: f64) -> Value {
    if n.fract() == 0.0 && n.abs() < 9_007_199_254_740_992.0 {
        json!(n as i64)
    } else {
        json!(n)
    }
}

/// 新建小对象。
fn obj(pairs: Vec<(&str, Value)>) -> Value {
    let mut m = Map::new();
    for (k, v) in pairs {
        m.insert(k.to_string(), v);
    }
    Value::Object(m)
}

fn get<'a>(v: &'a Value, k: &str) -> &'a Value {
    v.get(k).unwrap_or(&Value::Null)
}

/// reduceAuthFlow(state, action)；`state` 不参与结果（上游同样忽略）。
pub fn reduce_auth_flow(action: &Value) -> Value {
    let t = get(action, "type").as_str().unwrap_or("");
    match t {
        "providers-loaded" => obj(vec![("step", json!("identifier")), ("providers", get(action, "providers").clone())]),
        "realm-switch-required" => {
            let mut m = Map::new();
            m.insert("step".into(), json!("realm-confirmation"));
            m.insert("targetRegion".into(), get(action, "targetRegion").clone());
            if let Some(e) = action.get("email") {
                m.insert("email".into(), e.clone());
            }
            if get(action, "personalLoginAvailable").as_bool() == Some(true) {
                m.insert("personalLoginAvailable".into(), json!(true));
            }
            m.insert("providers".into(), get(action, "providers").clone());
            m.insert("methods".into(), get(action, "methods").clone());
            Value::Object(m)
        }
        "discovery-loaded" => obj(vec![
            ("step", json!("method-choice")),
            ("email", get(action, "email").clone()),
            ("methods", get(action, "methods").clone()),
        ]),
        "code-requested" => obj(vec![
            ("step", json!("verification-code")),
            ("kind", get(action, "kind").clone()),
            ("identifier", get(action, "identifier").clone()),
        ]),
        "browser-started" => obj(vec![("step", json!("browser-redirect")), ("label", get(action, "label").clone())]),
        "binding-code-requested" => obj(vec![
            ("step", json!("binding")),
            ("bindType", get(action, "bindType").clone()),
            ("codeRequested", json!(true)),
            ("contact", get(action, "contact").clone()),
        ]),
        "sso-verification-code-requested" => obj(vec![
            ("step", json!("sso-verification")),
            ("channel", get(action, "channel").clone()),
            ("targetMasked", get(action, "targetMasked").clone()),
            ("codeRequested", json!(true)),
        ]),
        "failed" => obj(vec![
            ("step", json!("error")),
            ("code", get(action, "code").clone()),
            ("recoverTo", get(action, "recoverTo").clone()),
        ]),
        _ => {
            // outcome
            let o = get(action, "outcome");
            match get(o, "status").as_str() {
                Some("ok") => obj(vec![("step", json!("completed")), ("membership", get(o, "membership").clone())]),
                Some("select_account") => {
                    obj(vec![("step", json!("account-selection")), ("accounts", get(o, "accounts").clone())])
                }
                Some("sso_verification_required") => obj(vec![
                    ("step", json!("sso-verification")),
                    ("channel", get(o, "channel").clone()),
                    ("targetMasked", get(o, "targetMasked").clone()),
                    ("codeRequested", json!(false)),
                ]),
                _ => obj(vec![
                    ("step", json!("binding")),
                    ("bindType", get(o, "bindType").clone()),
                    ("codeRequested", json!(false)),
                ]),
            }
        }
    }
}

/// loginPreparingErrorState(code, retryAt?, httpStatus?)。
pub fn login_preparing_error_state(code: &str, retry_at: Option<f64>, http_status: Option<i64>) -> Value {
    let mut m = Map::new();
    m.insert("step".into(), json!("error"));
    m.insert("code".into(), json!(code));
    m.insert("recoverTo".into(), json!("identifier"));
    if code == "INVALID_RESPONSE" {
        if let Some(s) = http_status {
            if (400..=599).contains(&s) {
                m.insert("httpStatus".into(), json!(s));
            }
        }
    }
    if code == "RATE_LIMITED" {
        if let Some(r) = retry_at {
            m.insert("retryAt".into(), js_number(r));
        }
    }
    Value::Object(m)
}

fn bounded<'a>(v: &'a Value, max: usize) -> Option<&'a str> {
    let s = v.as_str()?;
    let n = js_len(s);
    (n > 0 && n <= max).then_some(s)
}

fn kind_ok(v: &Value) -> Option<&str> {
    match v.as_str() {
        Some(s @ ("email" | "phone")) => Some(s),
        _ => None,
    }
}

const MAX_IDENT: usize = 320;
const MAX_OPAQUE: usize = 256;
const MAX_CODE: usize = 32;
const MAX_CAPTCHA: usize = 2048;
const MAX_ORG: usize = 253;

/// IPC 编码里的 undefined。
fn is_undefined_tag(v: &Value) -> bool {
    v.as_object().and_then(|o| o.get("__cindy_t")).and_then(Value::as_str) == Some("u")
}

/// parseDesktopLoginAction：只保留各 action 认识的字段；非法返回 None。
pub fn parse_login_action(value: &Value) -> Option<Value> {
    let m = value.as_object()?;
    let t = m.get("type")?.as_str()?;
    let g = |k: &str| m.get(k).unwrap_or(&Value::Null);
    Some(match t {
        "reset" | "back" | "cancel-browser" | "confirm-sso-realm" | "cancel-sso-realm"
        | "request-sso-verification-code" => json!({ "type": t }),
        "discover" => json!({"type": t, "email": bounded(g("email"), MAX_IDENT)?}),
        "discover-sso-org" => json!({"type": t, "org": bounded(g("org"), MAX_ORG)?}),
        "request-code" => {
            let kind = kind_ok(g("kind"))?;
            let identifier = bounded(g("identifier"), MAX_IDENT)?;
            // renderer 把值为 undefined 的键编码成 {"__cindy_t":"u"}（Electron 的结构化克隆里它就是 undefined），按“没有这个键”处理；null 仍然非法。
            if m.get("captchaToken").map_or(true, is_undefined_tag) {
                json!({"type": t, "kind": kind, "identifier": identifier})
            } else {
                json!({"type": t, "kind": kind, "identifier": identifier,
                       "captchaToken": bounded(g("captchaToken"), MAX_CAPTCHA)?})
            }
        }
        "verify-code" => json!({"type": t, "kind": kind_ok(g("kind"))?,
            "identifier": bounded(g("identifier"), MAX_IDENT)?, "code": bounded(g("code"), MAX_CODE)?}),
        "start-browser" => {
            let kind = match g("kind").as_str() {
                Some(s @ ("social" | "sso")) => s,
                _ => return None,
            };
            json!({"type": t, "kind": kind,
                "providerOrConnectionId": bounded(g("providerOrConnectionId"), MAX_OPAQUE)?,
                "label": bounded(g("label"), MAX_OPAQUE)?})
        }
        "select-account" => json!({"type": t, "accountId": bounded(g("accountId"), MAX_OPAQUE)?}),
        "verify-sso-verification" => json!({"type": t, "code": bounded(g("code"), MAX_CODE)?}),
        "request-binding-code" => json!({"type": t, "contact": bounded(g("contact"), MAX_IDENT)?}),
        "verify-binding" => json!({"type": t, "contact": bounded(g("contact"), MAX_IDENT)?,
            "code": bounded(g("code"), MAX_CODE)?}),
        _ => return None,
    })
}

/// parseAuthSessionRecord：只认 {version:1, realm:cn|global, refreshToken 非空}。
pub fn parse_session_record(raw: Option<&str>) -> Option<(String, String)> {
    let v: Value = serde_json::from_str(raw?).ok()?;
    let m = v.as_object()?;
    if m.get("version")?.as_f64()? != 1.0 {
        return None;
    }
    let realm = m.get("realm")?.as_str()?;
    if realm != "cn" && realm != "global" {
        return None;
    }
    let tok = m.get("refreshToken")?.as_str()?;
    (js_len(tok) > 0).then(|| (realm.to_string(), tok.to_string()))
}

pub fn serialize_session_record(realm: &str, refresh_token: &str) -> String {
    json!({"version": 1, "realm": realm, "refreshToken": refresh_token}).to_string()
}
