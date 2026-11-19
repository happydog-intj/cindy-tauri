// auth-client 的 zod schema 在 Rust 里的等价物（types.ts）。
// 语义对齐 zod：z.object 剥掉未知字段；`.optional()` 允许缺省但不接受 null；`.nullable()` 必须出现。
// 失败只返回 Err(())，上层统一映射成 INVALID_RESPONSE（与上游一致，不暴露细节）。
use serde_json::{json, Map, Value};

pub type R<T> = Result<T, ()>;

fn object(v: &Value) -> R<&Map<String, Value>> {
    v.as_object().ok_or(())
}

fn str_min1(m: &Map<String, Value>, k: &str) -> R<Value> {
    match m.get(k) {
        Some(Value::String(s)) if !s.is_empty() => Ok(json!(s)),
        _ => Err(()),
    }
}

fn str_any(m: &Map<String, Value>, k: &str) -> R<Value> {
    match m.get(k) {
        Some(Value::String(s)) => Ok(json!(s)),
        _ => Err(()),
    }
}

/// z.string().nullable()：必须出现，null 或字符串。
fn nullable_str(m: &Map<String, Value>, k: &str) -> R<Value> {
    match m.get(k) {
        Some(v @ (Value::Null | Value::String(_))) => Ok(v.clone()),
        _ => Err(()),
    }
}

fn one_of(m: &Map<String, Value>, k: &str, allowed: &[&str]) -> R<Value> {
    match m.get(k) {
        Some(Value::String(s)) if allowed.contains(&s.as_str()) => Ok(json!(s)),
        _ => Err(()),
    }
}

fn boolean(m: &Map<String, Value>, k: &str) -> R<Value> {
    match m.get(k) {
        Some(Value::Bool(b)) => Ok(json!(b)),
        _ => Err(()),
    }
}

/// 可缺省字段：缺省不输出；出现则必须通过 `f`。
fn optional(m: &Map<String, Value>, out: &mut Map<String, Value>, k: &str, f: impl Fn(&Map<String, Value>, &str) -> R<Value>) -> R<()> {
    if m.contains_key(k) {
        out.insert(k.to_string(), f(m, k)?);
    }
    Ok(())
}

fn put(out: &mut Map<String, Value>, m: &Map<String, Value>, k: &str, f: impl Fn(&Map<String, Value>, &str) -> R<Value>) -> R<()> {
    out.insert(k.to_string(), f(m, k)?);
    Ok(())
}

pub fn region(m: &Map<String, Value>, k: &str) -> R<Value> {
    one_of(m, k, &["cn", "global"])
}

pub fn membership(v: &Value) -> R<Value> {
    let m = object(v)?;
    let mut o = Map::new();
    put(&mut o, m, "id", str_min1)?;
    optional(m, &mut o, "passportId", str_min1)?;
    put(&mut o, m, "kind", |m, k| one_of(m, k, &["personal", "org"]))?;
    put(&mut o, m, "role", |m, k| one_of(m, k, &["owner", "admin", "member"]))?;
    put(&mut o, m, "displayName", str_any)?;
    optional(m, &mut o, "avatarUrl", nullable_str)?;
    put(&mut o, m, "email", nullable_str)?;
    put(&mut o, m, "orgId", nullable_str)?;
    put(&mut o, m, "orgName", nullable_str)?;
    optional(m, &mut o, "orgLogoUrl", nullable_str)?;
    Ok(Value::Object(o))
}

fn captcha(v: &Value) -> R<Value> {
    let m = object(v)?;
    let mut o = Map::new();
    if m.get("provider") != Some(&json!("turnstile")) {
        return Err(());
    }
    o.insert("provider".into(), json!("turnstile"));
    put(&mut o, m, "siteKey", str_min1)?;
    let Some(Value::Array(items)) = m.get("requiredFor") else { return Err(()) };
    let mut kept = Vec::new();
    for it in items {
        let Value::String(s) = it else { return Err(()) };
        if s == "email_request_code" || s == "phone_request_code" {
            kept.push(json!(s));
        }
    }
    o.insert("requiredFor".into(), Value::Array(kept));
    Ok(Value::Object(o))
}

pub fn provider_config(v: &Value) -> R<Value> {
    let m = object(v)?;
    let mut o = Map::new();
    put(&mut o, m, "region", region)?;
    put(&mut o, m, "attribution", |m, k| one_of(m, k, &["phone", "email"]))?;
    put(&mut o, m, "email", boolean)?;
    put(&mut o, m, "phone", boolean)?;
    let Some(Value::Array(social)) = m.get("social") else { return Err(()) };
    let mut s = Vec::new();
    for it in social {
        match it.as_str() {
            Some(x @ ("apple" | "google" | "wechat")) => s.push(json!(x)),
            _ => return Err(()),
        }
    }
    o.insert("social".into(), Value::Array(s));
    if let Some(c) = m.get("captcha") {
        o.insert("captcha".into(), captcha(c)?);
    }
    Ok(Value::Object(o))
}

pub fn sso_org_discovery(v: &Value) -> R<Value> {
    let m = object(v)?;
    let mut o = Map::new();
    put(&mut o, m, "region", region)?;
    put(&mut o, m, "orgName", str_any)?;
    let Some(Value::Array(conns)) = m.get("connections") else { return Err(()) };
    let mut out = Vec::new();
    for c in conns {
        let cm = object(c)?;
        let mut co = Map::new();
        put(&mut co, cm, "connectionId", str_min1)?;
        put(&mut co, cm, "protocol", |m, k| one_of(m, k, &["oidc", "saml", "cas"]))?;
        put(&mut co, cm, "connectionName", str_any)?;
        out.push(Value::Object(co));
    }
    o.insert("connections".into(), Value::Array(out));
    Ok(Value::Object(o))
}

fn login_method(v: &Value) -> R<Value> {
    let m = object(v)?;
    match m.get("type").and_then(Value::as_str) {
        Some("email_code") => Ok(json!({"type": "email_code"})),
        Some("sso") => {
            let mut o = Map::new();
            o.insert("type".into(), json!("sso"));
            put(&mut o, m, "connectionId", str_min1)?;
            put(&mut o, m, "protocol", |m, k| one_of(m, k, &["oidc", "saml", "cas"]))?;
            put(&mut o, m, "orgName", str_any)?;
            put(&mut o, m, "connectionName", str_any)?;
            put(&mut o, m, "ssoRequired", boolean)?;
            Ok(Value::Object(o))
        }
        _ => Err(()),
    }
}

/// recognizeLoginMethods：有非空字符串 type 且不在已知集的项向前兼容丢弃，其余严格解析。
pub fn recognize_login_methods(items: &[Value]) -> R<Vec<Value>> {
    let mut out = Vec::new();
    for it in items {
        if let Some(t) = it.as_object().and_then(|m| m.get("type")).and_then(Value::as_str) {
            if !t.is_empty() && t != "email_code" && t != "sso" {
                continue;
            }
        }
        out.push(login_method(it)?);
    }
    Ok(out)
}

pub fn token_pair(v: &Value) -> R<Value> {
    let m = object(v)?;
    let mut o = Map::new();
    put(&mut o, m, "accessToken", str_min1)?;
    put(&mut o, m, "refreshToken", str_min1)?;
    o.insert("membership".into(), membership(m.get("membership").ok_or(())?)?);
    Ok(Value::Object(o))
}

fn account_fields(m: &Map<String, Value>, o: &mut Map<String, Value>) -> R<()> {
    optional(m, o, "accountToken", str_min1)?;
    optional(m, o, "accountRefreshToken", str_min1)?;
    if m.contains_key("accountDeletionRestored") {
        if m.get("accountDeletionRestored") != Some(&json!(true)) {
            return Err(());
        }
        o.insert("accountDeletionRestored".into(), json!(true));
    }
    // superRefine：两个账号 token 必须同时出现。
    if o.contains_key("accountToken") != o.contains_key("accountRefreshToken") {
        return Err(());
    }
    Ok(())
}

pub fn login_outcome(v: &Value) -> R<Value> {
    let m = object(v)?;
    let mut o = Map::new();
    match m.get("status").and_then(Value::as_str) {
        Some("ok") => {
            o.insert("status".into(), json!("ok"));
            put(&mut o, m, "accessToken", str_min1)?;
            put(&mut o, m, "refreshToken", str_min1)?;
            o.insert("membership".into(), membership(m.get("membership").ok_or(())?)?);
            account_fields(m, &mut o)?;
        }
        Some("select_account") => {
            o.insert("status".into(), json!("select_account"));
            put(&mut o, m, "loginTicket", str_min1)?;
            let Some(Value::Array(accts)) = m.get("accounts") else { return Err(()) };
            if accts.is_empty() {
                return Err(());
            }
            o.insert("accounts".into(), Value::Array(accts.iter().map(membership).collect::<R<Vec<_>>>()?));
            account_fields(m, &mut o)?;
        }
        Some("binding_required") => {
            o.insert("status".into(), json!("binding_required"));
            put(&mut o, m, "bindType", |m, k| one_of(m, k, &["phone", "email"]))?;
            put(&mut o, m, "bindTicket", str_min1)?;
        }
        Some("sso_verification_required") => {
            o.insert("status".into(), json!("sso_verification_required"));
            put(&mut o, m, "verificationTicket", str_min1)?;
            put(&mut o, m, "channel", |m, k| one_of(m, k, &["email", "sms"]))?;
            put(&mut o, m, "targetMasked", str_min1)?;
        }
        _ => return Err(()),
    }
    Ok(Value::Object(o))
}

pub fn desktop_poll(v: &Value) -> R<Value> {
    let m = object(v)?;
    match m.get("status").and_then(Value::as_str) {
        Some("pending") => Ok(json!({"status": "pending"})),
        Some("expired") => Ok(json!({"status": "expired"})),
        Some("ok") => Ok(json!({"status": "ok", "code": str_min1(m, "code")?})),
        Some("error") => Ok(json!({"status": "error", "error": str_min1(m, "error")?})),
        _ => Err(()),
    }
}

/// z.object({status: z.literal(lit)})
pub fn status_literal(v: &Value, lit: &str) -> R<Value> {
    let m = object(v)?;
    if m.get("status") == Some(&json!(lit)) {
        Ok(json!({"status": lit}))
    } else {
        Err(())
    }
}
