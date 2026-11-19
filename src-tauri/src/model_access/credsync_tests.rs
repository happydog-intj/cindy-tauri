//! 凭据同步状态机：重放上游 credentialsSync.ts 的 45 个场景（脚本化依赖，记录推送、写 key、退避延迟、各步结果）。
use super::credsync::*;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tokio::sync::watch;

struct Fake {
    fetch: Vec<Value>,
    rotate: Vec<Value>,
    writes_plan: Option<Vec<bool>>,
    fetch_n: Mutex<usize>,
    rotate_n: Mutex<usize>,
    write_n: Mutex<usize>,
    key: Mutex<Option<String>>,
    store: Mutex<(Option<String>, Option<String>)>,
    events: Mutex<Vec<Value>>,
    writes: Mutex<Vec<Value>>,
    delays: Mutex<Vec<u64>>,
    gates: Mutex<HashMap<String, watch::Sender<bool>>>,
}

impl Fake {
    fn gate(&self, name: &str) -> watch::Receiver<bool> {
        self.gates.lock().unwrap().entry(name.to_string()).or_insert_with(|| watch::channel(false).0).subscribe()
    }

    fn release(&self, name: &str) {
        let _ = self.gates.lock().unwrap().entry(name.to_string()).or_insert_with(|| watch::channel(false).0).send(true);
    }
}

fn outcome(me: Arc<Fake>, o: Value) -> BoxFut<Result<Value, Value>> {
    Box::pin(async move {
        if let Some(g) = o["gate"].as_str() {
            let mut rx = me.gate(g);
            let _ = rx.wait_for(|v| *v).await;
        }
        if let Some(e) = o.get("err") {
            return Err(e.clone());
        }
        Ok(o["ok"].clone())
    })
}

fn pick(q: &[Value], i: usize) -> Value {
    q[i.min(q.len() - 1)].clone()
}

struct D(Arc<Fake>);

impl Deps for D {
    fn fetch(&self) -> BoxFut<Result<Value, Value>> {
        let n = { let mut g = self.0.fetch_n.lock().unwrap(); let v = *g; *g += 1; v };
        outcome(self.0.clone(), pick(&self.0.fetch, n))
    }
    fn rotate(&self) -> BoxFut<Result<Value, Value>> {
        let n = { let mut g = self.0.rotate_n.lock().unwrap(); let v = *g; *g += 1; v };
        let q = if self.0.rotate.is_empty() { &self.0.fetch } else { &self.0.rotate };
        outcome(self.0.clone(), pick(q, n))
    }
    fn read_key(&self) -> Option<String> {
        self.0.key.lock().unwrap().clone()
    }
    fn write_key(&self, key: String) -> BoxFut<bool> {
        let me = self.0.clone();
        Box::pin(async move {
            let ok = match &me.writes_plan {
                Some(p) => { let mut g = me.write_n.lock().unwrap(); let v = p[(*g).min(p.len() - 1)]; *g += 1; v }
                None => true,
            };
            me.writes.lock().unwrap().push(json!({ "key": key, "ok": ok }));
            if ok { *me.key.lock().unwrap() = Some(key); }
            ok
        })
    }
    fn source(&self) -> Option<String> {
        self.0.store.lock().unwrap().0.clone()
    }
    fn server_endpoint(&self) -> Option<String> {
        let g = self.0.store.lock().unwrap();
        if g.0.as_deref() == Some("server") { g.1.clone() } else { None }
    }
    fn set_server_credentials(&self, endpoint: &str) {
        *self.0.store.lock().unwrap() = (Some("server".into()), Some(endpoint.trim_end_matches('/').to_string()));
    }
    fn set_manual_source(&self) {
        *self.0.store.lock().unwrap() = (Some("manual".into()), None);
    }
    fn clear_store(&self) {
        *self.0.store.lock().unwrap() = (None, None);
    }
    fn on_status(&self, s: &Status) {
        self.0.events.lock().unwrap().push(s.to_json());
    }
    fn sleep(&self, ms: u64) -> BoxFut<()> {
        self.0.delays.lock().unwrap().push(ms);
        Box::pin(async {})
    }
}

async fn flush() {
    for _ in 0..40 {
        tokio::task::yield_now().await;
    }
}

fn track(results: &Arc<Mutex<Vec<Value>>>, fut: impl std::future::Future<Output = Value> + Send + 'static) {
    let idx = { let mut g = results.lock().unwrap(); g.push(Value::Null); g.len() - 1 };
    let r = results.clone();
    tokio::spawn(async move {
        let v = fut.await;
        r.lock().unwrap()[idx] = v;
    });
}

async fn run(sc: &Value) -> Value {
    let s = &sc["scenario"];
    let arr = |k: &str| s[k].as_array().cloned().unwrap_or_default();
    let fake = Arc::new(Fake {
        fetch: arr("fetch"),
        rotate: arr("rotate"),
        writes_plan: s["writes"].as_array().map(|a| a.iter().map(|b| b.as_bool().unwrap()).collect()),
        fetch_n: Mutex::new(0),
        rotate_n: Mutex::new(0),
        write_n: Mutex::new(0),
        key: Mutex::new(s["initialKey"].as_str().map(String::from)),
        store: Mutex::new((s["initialSource"].as_str().map(String::from), None)),
        events: Mutex::new(vec![]),
        writes: Mutex::new(vec![]),
        delays: Mutex::new(vec![]),
        gates: Mutex::new(HashMap::new()),
    });
    let sync = CredSync::new(Arc::new(D(fake.clone())), tokio::runtime::Handle::current());
    let results: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(vec![]));
    let ok = |st: Status| json!({ "ok": st.to_json() });
    for step in s["steps"].as_array().unwrap() {
        match step["op"].as_str().unwrap() {
            "auth" => sync.handle_auth_change(step["isAuthenticated"].as_bool().unwrap(), step["userId"].as_str(), step["realm"].as_str()),
            "sync" => { let f = sync.sync(); track(&results, async move { ok(f.await) }); }
            "retry" => { let f = sync.retry(); track(&results, async move { ok(f.await) }); }
            "resync" => { let f = sync.resync_after_key_rejection(); track(&results, async move { ok(f.await) }); }
            "rotate" => {
                let me = sync.clone();
                track(&results, async move {
                    match me.rotate().await {
                        Ok(st) => json!({ "ok": st.to_json() }),
                        Err(e) => json!({ "err": { "code": e["code"], "statusCode": e["statusCode"] } }),
                    }
                });
            }
            "manualSaved" => sync.note_manual_key_saved(),
            "manualRemoved" => sync.note_manual_key_removed(),
            "release" => fake.release(step["gate"].as_str().unwrap()),
            other => panic!("未知步骤 {other}"),
        }
        flush().await;
    }
    let store = fake.store.lock().unwrap().clone();
    json!({
        "events": fake.events.lock().unwrap().clone(),
        "writes": fake.writes.lock().unwrap().clone(),
        "delays": fake.delays.lock().unwrap().clone(),
        "fetchN": *fake.fetch_n.lock().unwrap(),
        "rotateN": *fake.rotate_n.lock().unwrap(),
        "results": results.lock().unwrap().clone(),
        "finalStatus": sync.status().to_json(),
        "finalKey": fake.key.lock().unwrap().clone(),
        "finalStore": { "source": store.0, "endpoint": store.1 },
    })
}

#[tokio::test(flavor = "current_thread")]
async fn replays_upstream_scenarios() {
    let g: Value = serde_json::from_str(include_str!("../credsync_golden.json")).unwrap();
    let scenarios = g["scenarios"].as_array().unwrap();
    assert!(scenarios.len() >= 45, "{}", scenarios.len());
    let mut bad = vec![];
    for sc in scenarios {
        let got = run(sc).await;
        let mut want = json!({});
        for k in ["events", "writes", "delays", "fetchN", "rotateN", "results", "finalStatus", "finalKey", "finalStore"] {
            want[k] = sc[k].clone();
        }
        if got != want {
            bad.push(format!("场景 {:?}\n 期望 {}\n 实际 {}", sc["name"], want, got));
        }
    }
    assert!(bad.is_empty(), "{} 个场景不一致:\n{}", bad.len(), bad.join("\n"));
}
