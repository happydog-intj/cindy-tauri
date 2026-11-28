// 账号登录纯逻辑：重放上游 TS 生成的黄金样例（scripts/app-settings/auth-entry.ts）。
use super::backoff::Backoff;
use super::flow::*;
use super::hosted::*;
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};

const U: &str = "\u{0}undef";

fn golden() -> Value {
    serde_json::from_str(include_str!("../auth_golden.json")).unwrap()
}

fn arr<'a>(g: &'a Value, k: &str) -> &'a Vec<Value> {
    g[k].as_array().unwrap_or_else(|| panic!("缺 {k}"))
}

#[test]
fn email_validation() {
    let g = golden();
    assert!(arr(&g, "emailCases").len() >= 20);
    for c in arr(&g, "emailCases") {
        let input = c["input"].as_str().unwrap();
        assert_eq!(is_valid_email(input), c["output"].as_bool().unwrap(), "{input:?}");
    }
}

#[test]
fn reduce_flow_matches() {
    let g = golden();
    assert_eq!(arr(&g, "flowCases").len(), 14);
    for c in arr(&g, "flowCases") {
        assert_eq!(reduce_auth_flow(&c["input"]), c["output"], "{}", c["input"]);
    }
}

#[test]
fn preparing_error_state_matches() {
    let g = golden();
    for c in arr(&g, "prepCases") {
        let a = c["args"].as_array().unwrap();
        let code = a.first().and_then(Value::as_str).unwrap_or("AUTH_SERVICE_UNAVAILABLE");
        let retry = a.get(1).and_then(|v| if v == U { None } else { v.as_f64() });
        // 非整数的 httpStatus 在 JS 里 Number.isInteger 为假。
        let status = a.get(2).and_then(|v| if v == U { None } else if v.is_i64() || v.is_u64() { v.as_i64() } else { None });
        assert_eq!(login_preparing_error_state(code, retry, status), c["output"], "{a:?}");
    }
}

#[test]
fn login_action_parsing_matches() {
    let g = golden();
    assert!(arr(&g, "actionCases").len() >= 45);
    for c in arr(&g, "actionCases") {
        let input = if c["input"] == U { &Value::Null } else { &c["input"] };
        let got = parse_login_action(input).unwrap_or(Value::Null);
        assert_eq!(got, c["output"], "{}", c["input"]);
    }
}

/// 回归：renderer 把 `captchaToken: undefined` 编码成 {"__cindy_t":"u"}，没有人机验证的服务端不带 token，
/// 这时 request-code 不能被当成非法动作（否则登录页点“发送验证码”没反应）。
#[test]
fn request_code_accepts_encoded_undefined_captcha_token() {
    let undef = json!({ "__cindy_t": "u" });
    let got = parse_login_action(&json!({"type":"request-code","kind":"email","identifier":"a@b.co","captchaToken": undef}));
    assert_eq!(got, Some(json!({"type":"request-code","kind":"email","identifier":"a@b.co"})));
    // null、数字、空串仍然非法（与上游一致，见 actionCases）。
    for bad in [json!(null), json!(5), json!("")] {
        assert!(parse_login_action(&json!({"type":"request-code","kind":"email","identifier":"a@b.co","captchaToken": bad})).is_none(), "{bad}");
    }
}

#[test]
fn session_record_matches() {
    let g = golden();
    for c in arr(&g, "recordCases") {
        let got = parse_session_record(c["input"].as_str());
        match (&c["output"], got) {
            (Value::Null, None) => {}
            (o, Some((realm, tok))) => {
                assert_eq!(o["realm"].as_str().unwrap(), realm);
                assert_eq!(o["refreshToken"].as_str().unwrap(), tok);
            }
            (o, got) => panic!("不一致 {} -> {o} vs {got:?}", c["input"]),
        }
    }
    for c in arr(&g, "serialized") {
        let s = serialize_session_record(c["realm"].as_str().unwrap(), c["token"].as_str().unwrap());
        assert_eq!(s, c["output"].as_str().unwrap());
    }
}

#[test]
fn hosted_pure_pieces_match() {
    let g = golden();
    for c in arr(&g, "stateCases") {
        assert_eq!(derive_client_state(c["input"].as_str().unwrap()), c["output"].as_str().unwrap());
    }
    for c in arr(&g, "delays") {
        assert_eq!(poll_delay_ms(c["input"].as_u64().unwrap()), c["output"].as_u64().unwrap());
    }
    for c in arr(&g, "mapCases") {
        let got = match map_poll(&c["input"]) {
            None => Value::Null,
            Some(Ok(code)) => json!({ "code": code }),
            Some(Err(e)) => json!({ "error": e }),
        };
        assert_eq!(got, c["output"]);
    }
    for c in arr(&g, "errCases") {
        // 上游按鸭子类型取 code：仅非空字符串生效。
        let code = c["input"].get("code").and_then(Value::as_str);
        assert_eq!(poll_error_code(code), c["output"].as_str().unwrap(), "{}", c["input"]);
    }
    let cred = create_poll_credentials();
    assert_eq!(cred.client_state, derive_client_state(&cred.poll_secret));
    assert_eq!(cred.poll_secret.len(), 43);
}

#[test]
fn backoff_replays_js_trace() {
    let g = golden();
    let randoms: Vec<f64> = g["backoff"]["randoms"].as_array().unwrap().iter().map(|v| v.as_f64().unwrap()).collect();
    let trace = g["backoff"]["trace"].as_array().unwrap();
    assert_eq!(trace.len(), 400);
    let clock = Arc::new(Mutex::new(1_000_000.0f64));
    let ri = Arc::new(Mutex::new(0usize));
    let (c2, r2, ri2) = (clock.clone(), randoms.clone(), ri.clone());
    let mut bo = Backoff::new(
        Box::new(move || *c2.lock().unwrap()),
        Box::new(move || {
            let mut i = ri2.lock().unwrap();
            let v = r2[*i % r2.len()];
            *i += 1;
            v
        }),
    );
    let mut defers = 0;
    for (n, s) in trace.iter().enumerate() {
        let epoch = s["epoch"].as_i64().unwrap();
        let now = *clock.lock().unwrap();
        let out: Value = match s["op"].as_str().unwrap() {
            "defer" => {
                defers += 1;
                let server = s["server"].as_f64().map(|o| now + o);
                json!(bo.defer(epoch, server, s["net"].as_bool().unwrap()))
            }
            "rejected" => json!(bo.server_rejected_token(epoch)),
            "start" => { bo.start_runtime_attempt(epoch); Value::Null }
            "release" => { bo.release(epoch); Value::Null }
            "clear" => { bo.clear(); Value::Null }
            "remaining" => json!(bo.remaining(epoch)),
            "runtimeRemaining" => json!(bo.runtime_remaining(epoch)),
            "advance" => { *clock.lock().unwrap() += s["adv"].as_f64().unwrap(); Value::Null }
            op => panic!("{op}"),
        };
        let want = &s["out"];
        let same = match (want.as_f64(), out.as_f64()) {
            (Some(a), Some(b)) => a == b,
            _ => want == &out,
        };
        assert!(same, "步 {n} {} 期望 {want} 实际 {out}", s["op"]);
        assert_eq!(*clock.lock().unwrap(), s["now"].as_f64().unwrap(), "步 {n} 时钟");
        assert_eq!(*ri.lock().unwrap() as u64, s["randomIndex"].as_u64().unwrap(), "步 {n} 随机数消耗");
    }
    assert!(defers > 50);
}

#[tokio::test(start_paused = true)]
async fn hosted_polling_replays_js_scenarios() {
    let g = golden();
    let cases = arr(&g, "pollCases");
    assert_eq!(cases.len(), 12);
    for (n, sc) in cases.iter().enumerate() {
        let steps = sc["steps"].as_array().unwrap().clone();
        let cancel = Cancel::new();
        let start = tokio::time::Instant::now();
        let times = Arc::new(Mutex::new(Vec::<u64>::new()));
        let idx = Arc::new(Mutex::new(0usize));
        let (t2, i2, c2) = (times.clone(), idx.clone(), cancel.clone());
        let has_abort = steps.iter().any(|s| s["abort"] == true);
        let result = run_polling(PollingDeps {
            poll: move || {
                let (t2, i2, c2, steps) = (t2.clone(), i2.clone(), c2.clone(), steps.clone());
                async move {
                    t2.lock().unwrap().push(start.elapsed().as_millis() as u64);
                    let step = {
                        let mut i = i2.lock().unwrap();
                        let s = steps.get(*i).cloned().unwrap_or(json!({"r": {"status": "pending"}}));
                        *i += 1;
                        s
                    };
                    if step["abort"] == true {
                        c2.abort();
                    }
                    if let Some(e) = step.get("e") {
                        return Err(PollFailure(e.as_str().map(String::from)));
                    }
                    Ok(step["r"].clone())
                }
            },
            now: move || start.elapsed().as_millis() as u64,
            cancel,
            timeout_ms: sc["timeoutMs"].as_u64().unwrap(),
            max_failures: sc.get("maxFailures").and_then(Value::as_u64).unwrap_or(8) as u32,
        })
        .await;
        let got = match result {
            Ok(code) => json!({ "code": code }),
            Err(e) => json!({ "error": e }),
        };
        assert_eq!(got, sc["result"], "场景 {n}");
        let want_times: Vec<u64> = sc["pollTimes"].as_array().unwrap().iter().map(|v| v.as_u64().unwrap()).collect();
        assert_eq!(*times.lock().unwrap(), want_times, "场景 {n} 轮询时刻");
        let _ = has_abort;
    }
}

#[test]
fn schemas_match_zod() {
    use super::validate as v;
    let g = golden();
    let mut total = 0;
    for set in arr(&g, "schemaCases") {
        let name = set["name"].as_str().unwrap();
        for c in set["cases"].as_array().unwrap() {
            let input = if c["input"] == U { Value::Null } else { c["input"].clone() };
            let got: Result<Value, ()> = match name {
                "membership" => v::membership(&input),
                "providerConfig" => v::provider_config(&input),
                "ssoOrgDiscovery" => v::sso_org_discovery(&input),
                "tokenPair" => v::token_pair(&input),
                "loginOutcome" => v::login_outcome(&input),
                "desktopPoll" => v::desktop_poll(&input),
                "recognizeLoginMethods" => match input.as_array() {
                    Some(a) => v::recognize_login_methods(a).map(Value::Array),
                    None => Err(()),
                },
                n => panic!("{n}"),
            };
            let want = &c["output"];
            match (want["ok"].as_bool().unwrap(), got) {
                (true, Ok(d)) => assert_eq!(&d, &want["data"], "{name} {input}"),
                (false, Err(())) => {}
                (w, g) => panic!("{name} 期望 ok={w} 实际 {g:?}\n输入 {input}"),
            }
            total += 1;
        }
    }
    assert!(total > 1000, "{total}");
}

#[test]
fn wire_helpers_match() {
    use super::wire::*;
    let g = golden();
    assert_eq!(arr(&g, "idCases").len(), 103);
    for c in arr(&g, "idCases") {
        let h = c["h"].as_object().unwrap().clone();
        let got = auth_request_id(|n| h.get(n).and_then(Value::as_str).map(String::from));
        assert_eq!(got.map(Value::from).unwrap_or(Value::Null), c["output"], "{}", c["h"]);
    }
    for c in arr(&g, "raCases") {
        let got = retry_after_deadline(c["retryAfter"].as_str(), c["date"].as_str(), c["now"].as_f64().unwrap());
        let want = c["output"].as_f64();
        assert_eq!(got, want, "{} / {}", c["retryAfter"], c["date"]);
    }
}
