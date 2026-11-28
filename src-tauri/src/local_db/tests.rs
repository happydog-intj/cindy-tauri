//! 回放 scripts/gen-localdb-golden.mjs 用原 JS 实现生成的黄金数据（local_db_golden.json）：
//! 每一步的返回值 / 错误文案 / 推送事件，以及最后全部表的内容，必须逐项相同。

use super::*;
use serde_json::{json, Value};
use std::sync::atomic::{AtomicI64, AtomicUsize, Ordering};

struct TestEnv {
    now: AtomicI64,
    uuids: AtomicUsize,
    bytes: AtomicUsize,
}
impl Env for TestEnv {
    fn now_ms(&self) -> i64 {
        self.now.load(Ordering::SeqCst)
    }
    fn uuid(&self) -> String {
        format!("uuid-{:04}", self.uuids.fetch_add(1, Ordering::SeqCst) + 1)
    }
    /// 与 crypto-shim.mjs 的 randomBytes 同一规则。
    fn random_bytes(&self, n: usize) -> Vec<u8> {
        let b = self.bytes.fetch_add(1, Ordering::SeqCst) + 1;
        (0..n).map(|i| ((b * 7 + i * 13) & 255) as u8).collect()
    }
    fn local_date(&self, at_ms: i64) -> (i32, u32, u32) {
        use chrono::Datelike;
        let d = chrono::DateTime::from_timestamp_millis(at_ms).unwrap();
        (d.year(), d.month(), d.day())
    }
}

#[derive(Default)]
struct Sink(Mutex<Vec<Value>>);
impl Push for Sink {
    fn push(&self, channel: &str, payload: Value) {
        self.0.lock().unwrap().push(json!({ "channel": channel, "payload": payload }));
    }
}

fn norm(v: &Value, data: &str) -> Value {
    serde_json::from_str(&v.to_string().replace(data, "<DATA>")).unwrap()
}

fn dump(db: &LocalDb) -> Value {
    let g = db.inner.lock().unwrap();
    let mut out = serde_json::Map::new();
    for (t, order) in [("sessions", "id"), ("messages", "rowid"), ("recent_workdirs", "path"), ("project_aliases", "project_key"), ("right_sidebar_tabs", "id"), ("session_goals", "session_id")] {
        let sel = if t == "messages" { "rowid AS _rowid, *" } else { "*" };
        let rows = sql::all(&g.conn, &format!("SELECT {sel} FROM {t} ORDER BY {order}"), &[]).unwrap();
        // extra_dirs / writable_dirs 是 Rust 版新增的列（原版 JS 库没有，附加目录原先只在 renderer 侧），不参与与 JS 的逐字对比。
        let rows = rows.into_iter().map(|mut r| {
            if t == "sessions" {
                r.remove("extra_dirs");
                r.remove("writable_dirs");
            }
            r
        });
        out.insert(t.into(), Value::Array(rows.map(Value::Object).collect()));
    }
    Value::Object(out)
}

fn first_diff(a: &Value, b: &Value, path: &str) -> Option<String> {
    match (a, b) {
        (Value::Object(x), Value::Object(y)) => {
            for k in x.keys().chain(y.keys()) {
                if let Some(d) = first_diff(x.get(k).unwrap_or(&Value::Null), y.get(k).unwrap_or(&Value::Null), &format!("{path}.{k}")) {
                    return Some(d);
                }
            }
            None
        }
        (Value::Array(x), Value::Array(y)) => {
            if x.len() != y.len() {
                return Some(format!("{path}: len {} vs {}", x.len(), y.len()));
            }
            x.iter().zip(y).enumerate().find_map(|(i, (p, q))| first_diff(p, q, &format!("{path}[{i}]")))
        }
        _ => (a != b).then(|| format!("{path}: rust={a} golden={b}")),
    }
}

#[test]
fn replays_js_golden() {
    let golden: Value = serde_json::from_str(include_str!("../local_db_golden.json")).unwrap();
    let steps = golden["steps"].as_array().unwrap();
    assert!(steps.len() > 150, "黄金样例数量: {}", steps.len());
    let dir = std::env::temp_dir().join(format!("local-db-golden-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let data = dir.to_string_lossy().into_owned();
    let env = Arc::new(TestEnv { now: AtomicI64::new(golden["base"].as_i64().unwrap()), uuids: AtomicUsize::new(0), bytes: AtomicUsize::new(0) });
    let sink = Arc::new(Sink::default());
    let mut db = LocalDb::open(&dir, "local-v1", env.clone(), sink.clone()).unwrap();
    let mut failures: Vec<String> = Vec::new();
    for (i, step) in steps.iter().enumerate() {
        env.now.store(step["now"].as_i64().unwrap(), Ordering::SeqCst);
        let before = sink.0.lock().unwrap().len();
        let kind = step["k"].as_str().unwrap();
        let (result, error): (Value, Option<String>) = match kind {
            "invoke" => {
                let args = step["args"].as_array().cloned().unwrap_or_default();
                // 黄金里 undefined 记成 null：同样当 null 传（invoke 内部也把 undefined 标记当 null）。
                match db.invoke(step["channel"].as_str().unwrap(), &args) {
                    Ok(v) => (v, None),
                    Err(e) => (Value::Null, Some(e)),
                }
            }
            "upsert" => match db.upsert_session(&step["row"]) {
                Ok(()) => (Value::Null, None),
                Err(e) => (Value::Null, Some(e)),
            },
            "persistEvent" => (db.persist_maker_event(step["sid"].as_str().unwrap(), &step["event"]).map(Value::String).unwrap_or(Value::Null), None),
            "persistUser" => (db.persist_user_message(step["sid"].as_str().unwrap(), step["clientId"].as_str().unwrap(), &step["content"]).map(Value::String).unwrap_or(Value::Null), None),
            "interactionReq" => (db.persist_interaction_request(step["sid"].as_str().unwrap(), &step["req"]).map(Value::String).unwrap_or(Value::Null), None),
            "interactionRes" => {
                (db.persist_interaction_resolved(step["sid"].as_str().unwrap(), step["requestId"].as_str().unwrap(), &step["decision"]).unwrap_or(Value::Null), None)
            }
            "reopen" => {
                drop(db);
                // 重开库：bootAt 取当前（固定）时钟，落库器状态清空，与 JS 的 close()+openLocalDb 一致。
                db = LocalDb::open(&dir, "local-v1", env.clone(), sink.clone()).unwrap();
                (Value::Null, None)
            }
            "dump" => (dump(&db), None),
            other => panic!("unknown op {other}"),
        };
        let events: Vec<Value> = sink.0.lock().unwrap()[before..].to_vec();
        let got = json!({ "result": norm(&result, &data), "error": error.map(|e| norm(&json!(e), &data)), "events": norm(&Value::Array(events), &data) });
        let want = json!({ "result": step["result"], "error": step["error"], "events": step["events"] });
        // 持久化 / 事件类 op 的 JS 返回值里 persistId 是 cuid；其余形状原样比较。
        if let Some(d) = first_diff(&got, &want, "") {
            let label = step.get("channel").and_then(Value::as_str).unwrap_or(kind);
            failures.push(format!("step {i} ({label}): {d}"));
            if failures.len() >= 8 {
                break;
            }
        }
    }
    let _ = std::fs::remove_dir_all(&dir);
    assert!(failures.is_empty(), "与 JS 实现不一致:\n{}", failures.join("\n"));
}

/// 回归：renderer 把值为 undefined 的键编码成 {"__cindy_t":"u"}，建本地任务时不能因此被当成远程 / 协同会话拒绝。
#[test]
fn create_ignores_undefined_remote_fields_but_rejects_real_ones() {
    let dir = std::env::temp_dir().join(format!("local-db-undef-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let env = Arc::new(TestEnv { now: AtomicI64::new(1_700_000_000_000), uuids: AtomicUsize::new(0), bytes: AtomicUsize::new(0) });
    let db = LocalDb::open(&dir, "local-v1", env, Arc::new(Sink::default())).unwrap();
    let undef = json!({ "__cindy_t": "u" });
    let ok = db.invoke(
        "local-db:sessions:create",
        &[json!({ "id": "u1", "agentKind": "pi", "model": "m", "workspaceKind": "dialogue", "remoteHostId": undef, "agentDeviceId": undef, "orcaRole": undef })],
    );
    assert!(ok.is_ok(), "undefined 标记被当成了远程字段: {ok:?}");
    for k in ["remoteHostId", "agentDeviceId", "orcaRole"] {
        let err = db.invoke("local-db:sessions:create", &[json!({ "id": format!("r-{k}"), "agentKind": "pi", "model": "m", k: "x" })]).unwrap_err();
        assert!(err.starts_with("[NOT_IMPLEMENTED]"), "{k}: {err}");
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// 切换工作目录：对话型任务转成项目任务；非法目录被拒绝且不改库；agentKind 仍不支持。
#[test]
fn update_working_dir_turns_dialogue_into_project_and_validates() {
    let dir = std::env::temp_dir().join(format!("local-db-wd-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let proj = dir.join("proj");
    std::fs::create_dir_all(&proj).unwrap();
    let env = Arc::new(TestEnv { now: AtomicI64::new(1_700_000_000_000), uuids: AtomicUsize::new(0), bytes: AtomicUsize::new(0) });
    let db = LocalDb::open(&dir, "local-v1", env, Arc::new(Sink::default())).unwrap();
    db.invoke("local-db:sessions:create", &[json!({ "id": "w1", "agentKind": "pi", "model": "m", "workspaceKind": "dialogue" })]).unwrap();
    let before = db.invoke("local-db:sessions:get", &[json!("w1")]).unwrap();
    assert_eq!(before["workspaceKind"], "dialogue");

    let target = format!("{}/", proj.to_string_lossy());
    let updated = db.invoke("local-db:sessions:update", &[json!("w1"), json!({ "workingDir": target })]).unwrap();
    assert_eq!(updated["workingDir"], proj.to_string_lossy().as_ref());
    assert_eq!(updated["workspaceKind"], "project");

    for (bad, why) in [(json!("relative/dir"), "相对路径"), (json!(dir.join("nope").to_string_lossy()), "不存在"), (json!(""), "空串"), (json!(7), "非字符串")] {
        let err = db.invoke("local-db:sessions:update", &[json!("w1"), json!({ "workingDir": bad })]).unwrap_err();
        assert!(err.starts_with("[INVALID_PARAMS]"), "{why}: {err}");
    }
    let after = db.invoke("local-db:sessions:get", &[json!("w1")]).unwrap();
    assert_eq!(after["workingDir"], proj.to_string_lossy().as_ref(), "非法请求不改库");

    let err = db.invoke("local-db:sessions:update", &[json!("w1"), json!({ "agentKind": "claude" })]).unwrap_err();
    assert!(err.starts_with("[NOT_IMPLEMENTED]"), "{err}");
    let _ = std::fs::remove_dir_all(&dir);
}
