//! 端到端：真实 sidecar + 真实 HostFactory/MakerRegistry + 真实 pi-rs + mock 模型。
//! 覆盖 sidecar 路径 e2e 没覆盖的 Rust 会话：goal 多轮、换模型重建并续接历史、用户消息落库、重启后续接。
//! 需要 PI_RS_BIN / MOCK_OPENAI / NODE_BIN / SIDECAR_JS，缺一就跳过。

#![cfg(test)]

use crate::maker_host::{Emitter, HostBackend, HostFactory, HostSink};
use crate::maker_wiring::handle_native;
use crate::maker_registry::MakerRegistry;
use serde_json::{json, Value};
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

#[derive(Default)]
struct Pushes(Mutex<Vec<(String, Value)>>);
impl Emitter for Pushes {
    fn emit(&self, channel: &str, arg: Value) {
        self.0.lock().unwrap().push((channel.to_string(), arg));
    }
}
/// 本地库的推送：进同一份记录。
struct DbPush(Arc<Pushes>);
impl crate::local_db::Push for DbPush {
    fn push(&self, channel: &str, payload: Value) {
        self.0.emit(channel, payload);
    }
}

pub(crate) struct Env {
    pub(crate) pi: String,
    pub(crate) mock: String,
    pub(crate) node: String,
}
pub(crate) fn env() -> Option<Env> {
    Some(Env {
        pi: std::env::var("PI_RS_BIN").ok()?,
        mock: std::env::var("MOCK_OPENAI").ok()?,
        node: std::env::var("NODE_BIN").unwrap_or_else(|_| "node".into()),
    })
}

/// 账号登录的测试端口：推送进同一份记录；打开浏览器只记下地址。
struct AuthPorts {
    pushes: Arc<Pushes>,
    opened: Arc<Mutex<Vec<String>>>,
}
impl crate::auth::env::Ports for AuthPorts {
    fn emit(&self, channel: &str, arg: Value) {
        self.pushes.emit(channel, arg);
    }
    fn open_url(&self, url: &str) -> Result<(), String> {
        self.opened.lock().unwrap().push(url.to_string());
        Ok(())
    }
    fn locale(&self) -> String {
        "en".into()
    }
}

pub(crate) struct Rig {
    pub(crate) auth: Arc<crate::auth::manager::Manager>,
    pub(crate) opened: Arc<Mutex<Vec<String>>>,
    reg: Arc<MakerRegistry>,
    pushes: Arc<Pushes>,
    pub(crate) data: PathBuf,
    pub(crate) secrets: Arc<crate::secret_store::SecretStore>,
    pub(crate) db: Arc<crate::local_db::LocalDb>,
    pub(crate) models: Arc<crate::model_config::host::ModelHost>,
}

impl Rig {
    pub(crate) fn start(e: &Env, port: u16, data: &Path) -> Rig {
        Rig::start_with(e, port, data, &[])
    }

    /// `extra`：额外的环境参数（云端 mock 的 auth / model-access 地址等）。
    pub(crate) fn start_with_env(e: &Env, port: u16, data: &Path, extra: &[(&str, String)]) -> Rig {
        Rig::start_with(e, port, data, extra)
    }

    pub(crate) fn start_with(e: &Env, port: u16, data: &Path, extra: &[(&str, String)]) -> Rig {
        // 与生产同一套：密钥库、应用会话、本地库都在宿主里。
        let store = Arc::new(crate::secret_store::SecretStore::new(data));
        let owner = Arc::new(crate::owner::Owner::new(data));
        let pushes = Arc::new(Pushes::default());
        let db = Arc::new(crate::local_db::LocalDb::open(data, "local-v1", Arc::new(crate::local_db::SystemEnv), Arc::new(DbPush(pushes.clone()))).expect("open local db"));
        let _ = crate::native::LOCAL_DB.set(db.clone());
        // 账号登录：与生产同一套 Manager。
        let opened = Arc::new(Mutex::new(Vec::new()));
        let auth_env = Arc::new(crate::auth::env::AppEnv {
            owner: owner.clone(),
            secrets: store.clone(),
            ports: Arc::new(AuthPorts { pushes: pushes.clone(), opened: opened.clone() }),
            device_id: crate::boot_stubs::device_id(data),
            auth_base: extra.iter().find(|(k, _)| *k == "CINDY_TAURI_AUTH_BASE_URL").map(|(_, v)| v.clone()),
        });
        let tokio_handle = match tauri::async_runtime::handle() {
            tauri::async_runtime::RuntimeHandle::Tokio(h) => h.clone(),
        };
        let auth = crate::auth::manager::Manager::new(auth_env, crate::auth::backoff::Backoff::system(), tokio_handle);
        // 云端模型 / 自定义模型服务：与生产同一套 Rust 管理器（e2e 的 smoke provider 指向 mock 模型）。
        let emit_pushes = pushes.clone();
        let emit: crate::model_access::manager::Emit = Arc::new(move |ch, v| emit_pushes.emit(ch, v));
        let access_base = extra.iter().find(|(k, _)| *k == "CINDY_TAURI_MODEL_ACCESS_BASE_URL").map(|(_, v)| v.clone()).unwrap_or_else(crate::auth::env::model_access_base_url);
        let tokio_handle = match tauri::async_runtime::handle() {
            tauri::async_runtime::RuntimeHandle::Tokio(h) => h.clone(),
        };
        let access = crate::model_access::manager::Manager::new(data, access_base, auth.clone(), store.clone(), owner.clone(), emit.clone(), Arc::new(|| "en".to_string()), tokio_handle);
        access.start();
        let smoke = crate::model_config::manager::smoke_provider(Some(&format!("http://127.0.0.1:{port}")), Some("mock-model"));
        let models = crate::model_config::manager::Manager::new(data, store.clone(), owner.clone(), access.clone(), emit, smoke);
        models.start();
        let models = crate::model_config::host::ModelHost::new(access, models);
        // 主密钥：任意 32 字节十六进制即可（e2e 的 provider 不带 key）。
        store.unlock(&"00".repeat(32)).expect("unlock");

        let backend: Arc<dyn HostBackend> = Arc::new(crate::maker_wiring::DbBackend { db: db.clone(), models: Some(models.clone()) });
        let sink = HostSink::new(backend.clone(), pushes.clone());
        let factory = Arc::new(HostFactory { backend: backend.clone(), sink: sink.clone(), data_dir: data.to_path_buf(), pi_binary: PathBuf::from(&e.pi) });
        let reg = MakerRegistry::new(factory);
        // 与生产同一个接线函数：goal 控制器吃 sink 的事件、经注册表发送。
        crate::maker_wiring::attach_goal(&reg, backend.clone(), sink.clone(), Some(Duration::from_secs(2)));
        let weak = Arc::downgrade(&reg);
        sink.set_on_exit(move |id| {
            if let Some(r) = weak.upgrade() {
                r.on_exited(id);
            }
        });
        Rig { auth, opened, reg, pushes, data: data.to_path_buf(), secrets: store, db, models }
    }

    pub(crate) fn ipc(&self, channel: &str, args: Value) -> Result<Value, String> {
        if crate::auth::manager::Manager::owns(channel) {
            return tauri::async_runtime::block_on(self.auth.invoke(channel, args.as_array().unwrap())).unwrap_or_else(|| Err(format!("[NOT_IMPLEMENTED] {channel}")));
        }
        if MakerRegistry::owns(channel) {
            return self.reg.invoke(channel, args.as_array().unwrap());
        }
        if crate::model_access::manager::Manager::owns(channel) {
            return tauri::async_runtime::block_on(self.models.access.invoke(channel, args.as_array().unwrap())).unwrap_or_else(|| Err(format!("[NOT_IMPLEMENTED] {channel}")));
        }
        if crate::model_config::manager::Manager::owns(channel) {
            return tauri::async_runtime::block_on(self.models.models.invoke(channel, args.as_array().unwrap())).unwrap_or_else(|| Err(format!("[NOT_IMPLEMENTED] {channel}")));
        }
        if crate::local_db::LocalDb::owns(channel) {
            return self.db.invoke(channel, args.as_array().unwrap());
        }
        if channel == "local-db:messages:view" {
            // 与 main.rs 同路径：Rust 分页并投影。
            let a = args.as_array().unwrap();
            let mut page = self.db.view_page(&a[0], a.get(1).filter(|v| v.is_object()).unwrap_or(&json!({})))?;
            page["streaming"] = json!(false);
            let rows = page["page"].as_array().cloned().unwrap_or_default();
            let items = crate::history_view::project::project_history_view(&rows, false, false);
            return Ok(json!({ "version": 1, "items": items, "hasMore": page["hasMore"], "nextCursor": page["nextCursor"] }));
        }
        if crate::secret_store::SecretStore::owns(channel) {
            return self.secrets.invoke(channel, args.as_array().unwrap()).expect("owned channel");
        }
        crate::fallback::invoke(channel, args.as_array().unwrap())
    }

    /// 推送记录里是否有满足条件的 (channel, arg)。
    pub(crate) fn pushed(&self, channel: &str, f: impl Fn(&Value) -> bool) -> bool {
        self.pushes.0.lock().unwrap().iter().any(|(c, a)| c == channel && f(a))
    }

    fn new_session(&self, title: &str) -> String {
        let r = self
            .ipc("local-db:sessions:create", json!([{ "agentKind": "pi", "title": title, "workspaceKind": "dialogue", "model": "mock-model", "providerId": "smoke", "permissionMode": "ask" }]))
            .expect("create session row");
        r["id"].as_str().unwrap().to_string()
    }

    fn goal_status(&self, sid: &str) -> Value {
        self.ipc("maker:goal:get-status", json!([sid])).unwrap_or(Value::Null)
    }

    fn wait<F: FnMut() -> bool>(&self, what: &str, secs: u64, f: F) {
        let t0 = Instant::now();
        let mut f = f;
        while !f() {
            if t0.elapsed() >= Duration::from_secs(secs) {
                let tail: Vec<String> = self.pushes.0.lock().unwrap().iter().rev().take(6).map(|(c, a)| format!("{c} {}", a.to_string().chars().take(160).collect::<String>())).collect();
                panic!("timeout: {what}; 最近推送: {tail:#?}");
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    pub(crate) fn stop(self) {
        self.auth.dispose();
        self.reg.shutdown_all();
    }
}

fn start_mock(e: &Env) -> (std::process::Child, u16) {
    let mut child = Command::new(&e.node).arg(&e.mock).stdout(Stdio::piped()).stderr(Stdio::null()).spawn().expect("spawn mock");
    let mut line = String::new();
    BufReader::new(child.stdout.as_mut().unwrap()).read_line(&mut line).unwrap();
    let port = serde_json::from_str::<Value>(&line).unwrap()["port"].as_u64().unwrap() as u16;
    (child, port)
}

pub(crate) fn temp(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("cindy-rs-e2e-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn done_count(r: &Rig, sid: &str) -> usize {
    r.pushes.0.lock().unwrap().iter().filter(|(c, a)| c == "maker:event" && a["sessionId"] == sid && a["event"]["type"] == "done").count()
}

#[test]
fn rust_sessions_drive_goal_model_switch_and_restart_resume() {
    let Some(e) = env() else { eprintln!("skip: PI_RS_BIN/MOCK_OPENAI not set"); return };
    let (mut mock, port) = start_mock(&e);
    let data = temp("main");
    let r = Rig::start(&e, port, &data);

    // 1) 普通消息：Rust 会话回复，用户消息与事件落库。
    let sid = r.new_session("rust e2e");
    let opts = json!({ "agentKind": "pi", "id": sid, "model": "mock-model", "providerId": "smoke", "permissionMode": "bypassPermissions", "workspaceKind": "dialogue" });
    let p = r.ipc("maker:input:enqueue", json!([sid, { "clientId": "u1", "text": "Hello", "persistedContent": "Hello", "createOpts": opts }])).unwrap();
    assert_eq!(p["turnInProgress"], true, "{p}");
    r.wait("first turn done", 60, || done_count(&r, &sid) >= 1);
    let msgs = r.ipc("local-db:messages:list", json!([sid, { "limit": 50 }])).unwrap();
    let roles: Vec<String> = msgs.as_array().cloned().or_else(|| msgs["messages"].as_array().cloned()).unwrap_or_default().iter().filter_map(|m| m["role"].as_str().map(String::from)).collect();
    assert!(roles.contains(&"user".to_string()) && roles.contains(&"assistant".to_string()), "落库的消息角色: {roles:?} / {msgs}");
    assert!(r.data.join("pi-sessions").join(&sid).read_dir().unwrap().any(|f| f.unwrap().path().extension().is_some_and(|x| x == "jsonl")), "历史文件在 pi-sessions 下");

    // 2) 换模型：空闲会话被重建，历史续上，下一轮仍能回复；库里模型已更新。
    let rr = r.ipc("maker:set-model", json!([sid, "mock-model", "smoke", null, { "effort": null, "fastMode": false }])).unwrap();
    assert_eq!(rr["deferred"], false, "{rr}");
    assert!(!r.reg.invoke("maker:list-active", &[]).unwrap().as_array().unwrap().iter().any(|s| s["sessionId"] == sid.as_str()), "旧进程已关闭");
    let before = done_count(&r, &sid);
    r.ipc("maker:input:enqueue", json!([sid, { "clientId": "u2", "text": "second", "persistedContent": "second", "createOpts": opts }])).unwrap();
    r.wait("turn after model switch", 60, || done_count(&r, &sid) > before);

    // 3) goal：多轮自主续跑直到 complete，经 sidecar 适配器 → native → Rust 会话。
    let gid = r.new_session("rust goal");
    let gopts = json!({ "agentKind": "pi", "id": gid, "model": "mock-model", "providerId": "smoke", "permissionMode": "bypassPermissions", "workspaceKind": "dialogue" });
    // goal 的 ensureSession 按库里的模型 / 来源建会话；这里先让会话在 Rust 里存在即可。
    r.ipc("maker:create-session", json!([gopts])).unwrap();
    let set = r.ipc("maker:goal:set", json!([{ "sessionId": gid, "objective": "ship it MOCK_GOAL_COMPLETE_AFTER=3" }])).unwrap();
    assert_eq!(set["ok"], true, "{set}");
    r.wait("goal complete", 40, || {
        let s = r.goal_status(&gid);
        eprintln!("goal status: {}", s.to_string().chars().take(200).collect::<String>());
        s["status"] == "complete"
    });
    let st = r.goal_status(&gid);
    assert_eq!(st["turnsUsed"], 3, "{st}");
    std::thread::sleep(Duration::from_millis(1500));
    assert_eq!(r.goal_status(&gid)["turnsUsed"], 3, "complete 之后不再多跑");
    // goal 回合不应写 user 行。
    let gmsgs = r.ipc("local-db:messages:list", json!([gid, { "limit": 50 }])).unwrap();
    let grole_user = gmsgs.as_array().cloned().or_else(|| gmsgs["messages"].as_array().cloned()).unwrap_or_default().iter().filter(|m| m["role"] == "user").count();
    assert_eq!(grole_user, 0, "goal 回合不写 user 行: {gmsgs}");

    r.stop();

    // 4) 重启：新 sidecar + 新注册表，同一数据目录，会话按库里记录续上历史并继续回复。
    let r2 = Rig::start(&e, port, &data);
    let opts2 = json!({ "agentKind": "pi", "id": sid, "model": "mock-model", "providerId": "smoke", "permissionMode": "bypassPermissions", "workspaceKind": "dialogue" });
    r2.ipc("maker:input:enqueue", json!([sid, { "clientId": "u3", "text": "after restart", "persistedContent": "after restart", "createOpts": opts2 }])).unwrap();
    r2.wait("turn after restart", 60, || done_count(&r2, &sid) >= 1);
    let file = std::fs::read_dir(r2.data.join("pi-sessions").join(&sid)).unwrap().filter_map(|f| f.ok()).map(|f| f.path()).find(|p| p.extension().is_some_and(|x| x == "jsonl")).unwrap();
    let text = std::fs::read_to_string(&file).unwrap();
    assert!(text.contains("Hello") && text.contains("after restart"), "重启后的回合续写进同一份历史文件");
    r2.stop();
    let _ = mock.kill();
    let _ = std::fs::remove_dir_all(&data);
}

/// 在 Rust 会话上跑一个 goal，返回 goal 会话 id。
fn start_goal(r: &Rig, title: &str, objective: &str, limits: Value) -> String {
    let gid = r.new_session(title);
    let opts = json!({ "agentKind": "pi", "id": gid, "model": "mock-model", "providerId": "smoke", "permissionMode": "bypassPermissions", "workspaceKind": "dialogue" });
    r.ipc("maker:create-session", json!([opts])).unwrap();
    let mut input = json!({ "sessionId": gid, "objective": objective });
    if !limits.is_null() {
        input["limits"] = limits;
    }
    let set = r.ipc("maker:goal:set", json!([input])).unwrap();
    assert_eq!(set["ok"], true, "{set}");
    gid
}

#[test]
fn rust_sessions_cover_goal_terminal_states_pause_resume_and_effort() {
    let Some(e) = env() else { eprintln!("skip: PI_RS_BIN/MOCK_OPENAI not set"); return };
    let (mut mock, port) = start_mock(&e);
    let data = temp("goal-states");
    let r = Rig::start(&e, port, &data);

    // 1) blocked：模型回报 blocked，goal 停在 blocked，只跑一轮。
    let g1 = start_goal(&r, "goal blocked", "cannot do this MOCK_GOAL_BLOCKED", Value::Null);
    r.wait("goal blocked", 40, || r.goal_status(&g1)["status"] == "blocked");
    std::thread::sleep(Duration::from_millis(1200));
    let st = r.goal_status(&g1);
    assert_eq!(st["status"], "blocked", "{st}");
    assert_eq!(st["turnsUsed"], 1, "blocked 之后不再续跑: {st}");

    // 2) 轮数上限：mock 永远回 continue，maxTurns=2 时第 2 轮后进入 budgetLimited。
    let g2 = start_goal(&r, "goal limit", "keep going", json!({ "maxTurns": 2, "budgetTokens": null, "noProgressLimit": null }));
    r.wait("goal budgetLimited", 40, || r.goal_status(&g2)["status"] == "budgetLimited");
    std::thread::sleep(Duration::from_millis(1200));
    let st = r.goal_status(&g2);
    assert_eq!(st["turnsUsed"], 2, "到上限即停: {st}");
    assert!(st["lastReason"].as_str().unwrap_or("").contains("max turns"), "{st}");

    // 3) 暂停 / 恢复：暂停后不再续跑；恢复后接着跑到完成。
    let g3 = start_goal(&r, "goal pause", "pause me MOCK_GOAL_COMPLETE_AFTER=4", Value::Null);
    r.wait("goal first turn", 40, || r.goal_status(&g3)["turnsUsed"].as_i64().unwrap_or(0) >= 1);
    assert_eq!(r.ipc("maker:goal:pause", json!([g3])).unwrap()["ok"], true);
    // 暂停时可能有一轮在途：等它落定，再确认不再增长。
    r.wait("no turn in flight", 30, || !r.reg.invoke("maker:any-session-in-turn", &[]).map(|v| v == json!(true)).unwrap_or(false));
    let paused_at = r.goal_status(&g3);
    assert_eq!(paused_at["status"], "paused", "{paused_at}");
    std::thread::sleep(Duration::from_millis(1500));
    assert_eq!(r.goal_status(&g3)["turnsUsed"], paused_at["turnsUsed"], "暂停期间不续跑");
    assert_eq!(r.ipc("maker:goal:resume", json!([g3])).unwrap()["ok"], true);
    r.wait("goal complete after resume", 60, || r.goal_status(&g3)["status"] == "complete");
    assert_eq!(r.goal_status(&g3)["turnsUsed"], 4, "恢复后接着数轮");

    // 4) effort：持久化并发给 pi-rs；设置后会话仍可用（下一轮正常回复）。
    let sid = r.new_session("effort");
    let opts = json!({ "agentKind": "pi", "id": sid, "model": "mock-model", "providerId": "smoke", "permissionMode": "bypassPermissions", "workspaceKind": "dialogue" });
    r.ipc("maker:create-session", json!([opts])).unwrap();
    let res = r.ipc("maker:set-effort", json!([sid, "high"]));
    assert!(res.is_ok(), "{res:?}");
    r.ipc("maker:input:enqueue", json!([sid, { "clientId": "e1", "text": "hi", "persistedContent": "hi", "createOpts": opts }])).unwrap();
    r.wait("turn after effort", 60, || done_count(&r, &sid) >= 1);
    let row = r.ipc("local-db:sessions:get", json!([sid])).unwrap();
    assert_eq!(row["effort"], "high", "effort 已落库: {row}");

    r.stop();
    let _ = mock.kill();
    let _ = std::fs::remove_dir_all(&data);
}

fn jsonl_of(r: &Rig, sid: &str) -> String {
    let mut all = String::new();
    if let Ok(rd) = std::fs::read_dir(r.data.join("pi-sessions").join(sid)) {
        for f in rd.filter_map(|f| f.ok()).map(|f| f.path()).filter(|p| p.extension().is_some_and(|x| x == "jsonl")) {
            all.push_str(&std::fs::read_to_string(f).unwrap_or_default());
        }
    }
    all
}

#[test]
fn rust_sessions_defer_model_switch_until_the_running_turn_settles() {
    let Some(e) = env() else { eprintln!("skip: PI_RS_BIN/MOCK_OPENAI not set"); return };
    let (mut mock, port) = start_mock(&e);
    let data = temp("deferred");
    let r = Rig::start(&e, port, &data);

    let sid = r.new_session("deferred switch");
    let opts = json!({ "agentKind": "pi", "id": sid, "model": "mock-model", "providerId": "smoke", "permissionMode": "bypassPermissions", "workspaceKind": "dialogue" });
    // 慢回复（约 4 秒）：先让回合真的在跑。
    r.ipc("maker:input:enqueue", json!([sid, { "clientId": "s1", "text": "MOCK_SLOW first", "persistedContent": "MOCK_SLOW first", "createOpts": opts }])).unwrap();
    r.wait("turn is running", 20, || r.ipc("maker:session-in-turn", json!([sid])).map(|v| v == json!(true)).unwrap_or(false));
    let pid_before = r.reg.invoke("maker:list-active", &[]).unwrap().as_array().unwrap().iter().find(|s| s["sessionId"] == sid.as_str()).cloned().expect("会话在活跃列表里");

    // 回合进行中换模型：返回 deferred，不打断当前回合，进程也还是原来那个。
    let rr = r.ipc("maker:set-model", json!([sid, "mock-model", "smoke", null, { "effort": null, "fastMode": false }])).unwrap();
    assert_eq!(rr["deferred"], true, "{rr}");
    assert_eq!(r.ipc("maker:session-in-turn", json!([sid])).unwrap(), json!(true), "set-model 不能打断在途回合");
    let still = r.reg.invoke("maker:list-active", &[]).unwrap().as_array().unwrap().iter().find(|s| s["sessionId"] == sid.as_str()).cloned().expect("回合中进程不被关闭");
    assert_eq!(still["pid"], pid_before["pid"], "回合中没有重建（同一个 pi-rs 进程）");
    let old_pid = pid_before["pid"].as_u64().unwrap();

    // 排队一条后续消息：慢回合自然结束，不丢；之后重建并续上历史。
    r.ipc("maker:input:enqueue", json!([sid, { "clientId": "s2", "text": "second", "persistedContent": "second", "createOpts": opts }])).unwrap();
    r.wait("both turns done", 60, || done_count(&r, &sid) >= 2);
    // 回合结束后的下一次使用才重建：第二轮跑在新进程里。
    let after = r.reg.invoke("maker:list-active", &[]).unwrap().as_array().unwrap().iter().find(|s| s["sessionId"] == sid.as_str()).cloned().expect("第二轮后会话仍在");
    assert_ne!(after["pid"].as_u64().unwrap(), old_pid, "回合结束后已换成新进程: {after}");
    let history = jsonl_of(&r, &sid);
    assert!(history.contains("MOCK_SLOW first") && history.contains("second"), "重建后第二轮续写进同一份历史，且第一轮没被打断");
    assert!(history.matches("slow reply.").count() >= 1, "慢回复完整落进历史");
    // 第一轮的回复是完整的（没有被 abort 截断）。
    let msgs = r.ipc("local-db:messages:list", json!([sid, { "limit": 50 }])).unwrap();
    let list = msgs.as_array().cloned().or_else(|| msgs["messages"].as_array().cloned()).unwrap_or_default();
    let user_rows = list.iter().filter(|m| m["role"] == "user").count();
    assert_eq!(user_rows, 2, "两条用户消息都落库: {msgs}");

    r.stop();
    let _ = mock.kill();
    let _ = std::fs::remove_dir_all(&data);
}

// ── soak：持续加压，观察内存 / 文件句柄 / 子进程是否随时间只增不减 ─────────────────────────
// 默认忽略。运行：SOAK_SECS=1800 cargo test --release soak -- --ignored --nocapture
// 输出 CSV 到 SOAK_OUT（默认 /tmp/cindy-soak.csv）。

#[derive(Clone, Debug, Default)]
struct SoakSample {
    t: u64,
    host_kb: u64,
    node_kb: u64,
    pirs_kb: u64,
    pirs_n: usize,
    fds: usize,
    data_kb: u64,
    turns: u64,
}

fn ps_children(parent: u32) -> Vec<(u32, u64, String)> {
    let out = Command::new("ps").args(["-axo", "pid=,ppid=,rss=,command="]).output().ok();
    let text = out.map(|o| String::from_utf8_lossy(&o.stdout).into_owned()).unwrap_or_default();
    text.lines()
        .filter_map(|l| {
            let mut it = l.split_whitespace();
            let pid: u32 = it.next()?.parse().ok()?;
            let ppid: u32 = it.next()?.parse().ok()?;
            let rss: u64 = it.next()?.parse().ok()?;
            let cmd = it.collect::<Vec<_>>().join(" ");
            (ppid == parent).then_some((pid, rss, cmd))
        })
        .collect()
}

fn own_rss_kb() -> u64 {
    let out = Command::new("ps").args(["-o", "rss=", "-p", &std::process::id().to_string()]).output().ok();
    out.and_then(|o| String::from_utf8_lossy(&o.stdout).trim().parse().ok()).unwrap_or(0)
}

fn dir_kb(p: &Path) -> u64 {
    fn walk(p: &Path) -> u64 {
        std::fs::read_dir(p).map(|rd| rd.filter_map(|e| e.ok()).map(|e| match e.metadata() {
            Ok(m) if m.is_dir() => walk(&e.path()),
            Ok(m) => m.len(),
            Err(_) => 0,
        }).sum()).unwrap_or(0)
    }
    walk(p) / 1024
}

fn soak_sample(e: &Env, data: &Path, t: u64, turns: u64) -> SoakSample {
    let kids = ps_children(std::process::id());
    let mut s = SoakSample { t, host_kb: own_rss_kb(), fds: std::fs::read_dir("/dev/fd").map(|d| d.count()).unwrap_or(0), data_kb: dir_kb(data), turns, ..Default::default() };
    for (_, rss, cmd) in kids {
        if cmd.contains(&e.pi) {
            s.pirs_kb += rss;
            s.pirs_n += 1;
        }
    }
    s
}

fn mean(v: impl Iterator<Item = f64>) -> f64 {
    let (n, sum) = v.fold((0usize, 0.0), |(n, s), x| (n + 1, s + x));
    if n == 0 { 0.0 } else { sum / n as f64 }
}

#[test]
#[ignore]
fn soak_memory_and_process_stability() {
    let Some(e) = env() else { eprintln!("skip: PI_RS_BIN/MOCK_OPENAI not set"); return };
    let secs: u64 = std::env::var("SOAK_SECS").ok().and_then(|v| v.parse().ok()).unwrap_or(1800);
    let interval: u64 = std::env::var("SOAK_INTERVAL").ok().and_then(|v| v.parse().ok()).unwrap_or(20);
    let out = std::env::var("SOAK_OUT").unwrap_or_else(|_| "/tmp/cindy-soak.csv".to_string());
    let (mut mock, port) = start_mock(&e);
    let data = temp("soak");
    let mut csv = String::from("t_sec,host_kb,node_kb,pirs_kb,pirs_n,fds,data_kb,turns\n");
    let mut samples: Vec<SoakSample> = Vec::new();

    for round in 0.. {
        // 每一轮用一个新数据目录：模拟长时间使用里反复开会话，旧会话全部关闭。
        let r = Rig::start(&e, port, &data);
        let opts = json!({ "agentKind": "pi", "id": "", "model": "mock-model", "providerId": "smoke", "permissionMode": "bypassPermissions", "workspaceKind": "dialogue" });
        let mut turns = round as u64 * 4;
        let sid = r.new_session("soak");
        let sid2 = r.new_session("soak-2");
        let mut opt = opts.clone();
        opt["id"] = json!(sid);
        let mut opt2 = opts.clone();
        opt2["id"] = json!(sid2);
        let t0 = Instant::now();
        while t0.elapsed().as_secs() < secs {
            // 两条会话交替跑普通回合；第二条带工具调用（触发 pi-rs 的工具循环）。
            for (id, o, text) in [(&sid, &opt, "hello"), (&sid2, &opt2, "MOCK_WRITE_FILE please")] {
                let before = done_count(&r, id);
                r.ipc("maker:input:enqueue", json!([id, { "clientId": format!("soak-{turns}"), "text": text, "persistedContent": text, "createOpts": o }])).unwrap();
                r.wait("soak turn", 90, || done_count(&r, id) > before);
                turns += 1;
            }
            let s = soak_sample(&e, &r.data, t0.elapsed().as_secs(), turns);
            let line = format!("{},{},{},{},{},{},{},{}\n", s.t, s.host_kb, s.node_kb, s.pirs_kb, s.pirs_n, s.fds, s.data_kb, s.turns);
            csv.push_str(&line);
            // 边跑边写：长跑中途出问题也有数据可看。
            let _ = std::fs::write(&out, &csv);
            samples.push(s);
            // Rig 会把每个推送都存进 pushes 供断言用；长跑里不清理它本身就是「泄漏」，会污染 host 的 RSS 读数。
            r.pushes.0.lock().unwrap().clear();
            std::thread::sleep(Duration::from_secs(interval));
        }
        // 关闭全部会话：子进程必须随之消失。
        r.reg.invoke("maker:close-session", &[json!(sid)]).unwrap();
        r.reg.invoke("maker:close-session", &[json!(sid2)]).unwrap();
        r.stop();
        // 给操作系统的 wait / fd 回收一点时间再采样。
        std::thread::sleep(Duration::from_secs(2));
        let s = soak_sample(&e, &data, 0, turns);
        csv.push_str(&format!("close,{},{},{},{},{},{},{}\n", s.host_kb, s.node_kb, s.pirs_kb, s.pirs_n, s.fds, s.data_kb, s.turns));
        samples.push(s);
        if t0.elapsed().as_secs() >= secs {
            break;
        }
    }

    std::fs::write(&out, &csv).unwrap();
    let _ = mock.kill();

    // 报告：前 1/4 与后 1/4 的均值对比（跳过最后的 close 采样）。
    let n = samples.len();
    let body = &samples[..n.saturating_sub(1)];
    let k = (body.len() / 4).max(1);
    let head = &body[..k];
    let tail = &body[body.len().saturating_sub(k)..];
    let report = |name: &str, f: &dyn Fn(&SoakSample) -> f64| {
        let (a, b) = (mean(head.iter().map(|s| f(s))), mean(tail.iter().map(f)));
        eprintln!("[soak] {name}: first={a:.0} last={b:.0} delta={:+.1}%", if a > 0.0 { (b - a) / a * 100.0 } else { 0.0 });
        (a, b)
    };
    eprintln!("[soak] samples={n} turns={} csv={out}", samples.last().map(|s| s.turns).unwrap_or(0));
    let (host_a, host_b) = report("host_kb", &|s| s.host_kb as f64);
    report("node_kb", &|s| s.node_kb as f64);
    let (pirs_a, pirs_b) = report("pi_rs_kb", &|s| s.pirs_kb as f64);
    let (fd_a, fd_b) = report("fds", &|s| s.fds as f64);
    let (pirs_n_a, pirs_n_b) = report("pi_rs_procs", &|s| s.pirs_n as f64);

    // 断言：只抓确定性泄漏，不抓 GC / 分配器的正常波动。
    assert!((pirs_n_b - pirs_n_a).abs() <= 1.0, "pi-rs 子进程数稳定（{pirs_n_a} → {pirs_n_b}）");
    assert!(fd_b <= fd_a * 1.5 + 20.0, "文件句柄不随时间无界增长（{fd_a} → {fd_b}）");
    assert!(host_b <= host_a * 1.5 + 20_000.0, "宿主 RSS 不随时间无界增长（{host_a} KB → {host_b} KB）");
    assert!(pirs_b <= pirs_a * 1.5 + 30_000.0, "pi-rs RSS 不随时间无界增长（{pirs_a} KB → {pirs_b} KB）");
    // 关闭后不留孤儿进程。
    let left = ps_children(std::process::id()).into_iter().filter(|(_, _, c)| c.contains(&e.pi)).count();
    assert_eq!(left, 0, "关闭会话后不留孤儿进程");
    let _ = std::fs::remove_dir_all(&data);
}

// ── 迁自 scripts/*-e2e.mjs 的会话场景（旧 sidecar 会话路径删除后，由 Rust 宿主覆盖）────────────────

/// 带日志的 mock：stderr 每行（`[mock-openai] {json}`）收进共享 Vec，供断言鉴权 / 模型 / 消息角色。
pub(crate) fn start_mock_logged(e: &Env, envs: &[(&str, &str)]) -> (std::process::Child, u16, Arc<Mutex<Vec<String>>>) {
    let mut cmd = Command::new(&e.node);
    cmd.arg(&e.mock).stdout(Stdio::piped()).stderr(Stdio::piped());
    for (k, v) in envs {
        cmd.env(k, v);
    }
    let mut child = cmd.spawn().expect("spawn mock");
    let mut line = String::new();
    BufReader::new(child.stdout.as_mut().unwrap()).read_line(&mut line).unwrap();
    let port = serde_json::from_str::<Value>(&line).unwrap()["port"].as_u64().unwrap() as u16;
    let log = Arc::new(Mutex::new(Vec::<String>::new()));
    let (sink, stderr) = (log.clone(), child.stderr.take().unwrap());
    std::thread::spawn(move || {
        for l in BufReader::new(stderr).lines().map_while(Result::ok) {
            sink.lock().unwrap().push(l);
        }
    });
    (child, port, log)
}

/// mock 日志里的聊天请求（解析后的 JSON），从第 `from` 行起。
fn chat_requests(log: &Arc<Mutex<Vec<String>>>, from: usize) -> Vec<Value> {
    log.lock().unwrap().iter().skip(from).filter_map(|l| serde_json::from_str::<Value>(l.trim_start_matches("[mock-openai] ")).ok()).filter(|e| e["path"].as_str().is_some_and(|p| p.ends_with("/chat/completions"))).collect()
}

impl Rig {
    /// 建会话行并返回整行（要用到 workingDir）。
    fn new_row(&self, title: &str, provider: &str) -> Value {
        self.ipc("local-db:sessions:create", json!([{ "agentKind": "pi", "title": title, "workspaceKind": "dialogue", "model": "mock-model", "providerId": provider, "permissionMode": "ask" }])).expect("create session row")
    }

    /// renderer 入队的 AgentInputQueuedMessage 形状。
    fn enqueue_text(&self, sid: &str, client_id: &str, text: &str, create_opts: &Value) -> Result<Value, String> {
        self.ipc("maker:input:enqueue", json!([sid, {
            "clientId": client_id, "text": text, "persistedContent": json!({ "text": text }).to_string(),
            "model": create_opts["model"], "effort": "medium", "permissionMode": create_opts["permissionMode"],
            "workingDir": create_opts["workingDir"], "chatMessage": { "clientId": client_id, "role": "user", "content": text },
            "createOpts": create_opts,
        }]))
    }

    /// 等第一条未处理过的审批请求，返回 requestId。
    fn wait_approval(&self, sid: &str, seen: &[String]) -> String {
        let mut found = String::new();
        self.wait("interaction request", 60, || {
            let g = self.pushes.0.lock().unwrap();
            let hit = g.iter().find(|(c, a)| c == "maker:interaction-request" && a["sessionId"] == sid && !seen.iter().any(|s| Some(s.as_str()) == a["request"]["requestId"].as_str()));
            if let Some((_, a)) = hit {
                found = a["request"]["requestId"].as_str().unwrap_or("").to_string();
            }
            !found.is_empty()
        });
        found
    }

    fn allow(&self, request_id: &str) {
        let r = self.ipc("maker:resolve-interaction", json!([request_id, { "kind": "permission", "behavior": "allow" }])).unwrap();
        assert_eq!(r["accepted"], true, "{r}");
    }
}

fn dir_contains(dir: &Path, needle: &str) -> Vec<PathBuf> {
    let mut hits = Vec::new();
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.filter_map(|e| e.ok()) {
            let p = e.path();
            match e.metadata() {
                Ok(m) if m.is_dir() => hits.extend(dir_contains(&p, needle)),
                Ok(m) if m.len() < 8 * 1024 * 1024 => {
                    if std::fs::read(&p).map(|b| b.windows(needle.len()).any(|w| w == needle.as_bytes())).unwrap_or(false) {
                        hits.push(p);
                    }
                }
                _ => {}
            }
        }
    }
    hits
}

#[test]
fn rust_sessions_stream_approve_persist_authenticate_and_resume_after_restart() {
    let Some(e) = env() else { eprintln!("skip: PI_RS_BIN/MOCK_OPENAI not set"); return };
    const KEY: &str = "sk-e2e-0123456789abcdef";
    let (mut mock, port, log) = start_mock_logged(&e, &[("MOCK_API_KEY", KEY)]);
    let data = temp("session");
    let r = Rig::start(&e, port, &data);

    // 1) 流式回复：多段 text 事件，落库用户与助手行。
    let row = r.new_row("E2E", "smoke");
    let sid = row["id"].as_str().unwrap().to_string();
    let wd = row["workingDir"].as_str().unwrap().to_string();
    let opts = json!({ "agentKind": "pi", "id": sid, "model": "mock-model", "providerId": "smoke", "permissionMode": "ask", "workingDir": wd, "workspaceKind": "dialogue" });
    r.enqueue_text(&sid, "u1", "hi", &opts).unwrap();
    r.wait("first done", 60, || done_count(&r, &sid) >= 1);
    let texts = r.pushes.0.lock().unwrap().iter().filter(|(c, a)| c == "maker:event" && a["sessionId"] == sid.as_str() && a["event"]["type"] == "text").count();
    assert!(texts >= 2, "流式文本事件 {texts}");

    // 2) Ask 模式：写文件要审批，放行后工具真的写了。
    r.enqueue_text(&sid, "u2", "MOCK_WRITE_FILE please", &opts).unwrap();
    let req = r.wait_approval(&sid, &[]);
    r.allow(&req);
    r.wait("second done", 60, || done_count(&r, &sid) >= 2);
    let written = Path::new(&wd).join("mock-ok.txt");
    assert_eq!(std::fs::read_to_string(&written).unwrap().trim(), "ok", "审批后工具写了文件");
    let hist = r.ipc("local-db:messages:list", json!([sid, { "limit": 100 }])).unwrap();
    let roles: Vec<String> = hist.as_array().unwrap().iter().map(|m| m["role"].as_str().unwrap_or("").to_string()).collect();
    assert!(roles.len() >= 4, "历史落库 {roles:?}");

    // 3) 带 API key 的 provider：key 只经 0600 文件交给 pi-rs，请求带 Bearer，磁盘与 sidecar 日志里没有明文。
    let keyed_url = format!("http://127.0.0.1:{port}/keyed");
    let cfg = json!({ "id": "keyed", "name": "Keyed", "auth": { "method": "apiKey" },
        "runtimes": { "pi": { "baseUrl": keyed_url, "wireProtocol": "openai-chat", "models": [{ "id": "mock-model", "name": "Mock (keyed)" }] } } });
    let created = r.ipc("maker:provider:custom:create", json!([cfg, { "pi": KEY }, {}])).unwrap();
    assert_eq!(created["ok"], true, "{created}");
    let krow = r.new_row("E2E keyed", "keyed");
    let ksid = krow["id"].as_str().unwrap().to_string();
    let kopts = json!({ "agentKind": "pi", "id": ksid, "model": "mock-model", "providerId": "keyed", "permissionMode": "ask", "workingDir": krow["workingDir"], "workspaceKind": "dialogue" });
    let mark = log.lock().unwrap().len();
    r.enqueue_text(&ksid, "k1", "hi", &kopts).unwrap();
    r.wait("keyed done", 60, || done_count(&r, &ksid) >= 1);
    let authed: Vec<Value> = chat_requests(&log, mark).into_iter().filter(|q| q["path"].as_str().is_some_and(|p| p.contains("/keyed/")) && q["auth"] == "ok").collect();
    assert!(!authed.is_empty(), "带 key 的回合用 Bearer 鉴权通过");
    let errored = r.pushes.0.lock().unwrap().iter().any(|(c, a)| c == "maker:event" && a["sessionId"] == ksid.as_str() && a["event"]["type"] == "error");
    assert!(!errored, "带 key 的回合没有报错");
    // 会话存活期间，明文 key 只允许出现在运行目录里的 provider.key，且权限为 0600；关闭会话后整个磁盘上都不该有。
    {
        #[cfg(unix)]
        use std::os::unix::fs::PermissionsExt;
        let hits = dir_contains(&data, KEY);
        assert!(!hits.is_empty() && hits.iter().all(|p| p.file_name().is_some_and(|n| n == "provider.key") && p.to_string_lossy().contains("pi-runtime")), "key 只在运行目录的 key 文件里：{hits:?}");
        // 0600 只在 Unix 有意义；Windows 靠用户目录 ACL。
        #[cfg(unix)]
        for p in &hits {
            assert_eq!(std::fs::metadata(p).unwrap().permissions().mode() & 0o777, 0o600, "key 文件权限 0600: {p:?}");
        }
    }
    r.stop();
    assert!(dir_contains(&data, KEY).is_empty(), "关闭会话后磁盘上没有明文 key：{:?}", dir_contains(&data, KEY));

    // 4) 重启后：会话还在、历史视图可读、没有误报中断，上下文接得上，工作目录以库里存的为准。
    let r = Rig::start(&e, port, &data);
    let list = r.ipc("local-db:sessions:list", json!([20])).unwrap();
    assert!(list.as_array().unwrap().iter().any(|s| s["id"] == sid.as_str()), "重启后会话在列表里");
    let view = r.ipc("local-db:messages:view", json!([sid, {}])).unwrap();
    assert!(view["version"] == 1 && !view["items"].as_array().unwrap().is_empty(), "重启后历史视图可读");
    let interrupted = r.ipc("local-db:sessions:interrupted-pending", json!([])).unwrap();
    assert!(!interrupted.as_array().unwrap().iter().any(|s| s == sid.as_str()), "没有误报中断: {interrupted}");
    let after = r.ipc("local-db:sessions:get", json!([sid])).unwrap();
    assert!(after["sdkSessionId"].as_str().is_some_and(|s| !s.is_empty()), "Pi 会话文件路径已落库: {after}");

    let stray = data.join("stray-workdir");
    let _ = std::fs::remove_file(&written);
    let mark = log.lock().unwrap().len();
    let mut stray_opts = opts.clone();
    stray_opts["workingDir"] = json!(stray.to_string_lossy());
    r.enqueue_text(&sid, "u9", "MOCK_WRITE_FILE after restart", &stray_opts).unwrap();
    let req2 = r.wait_approval(&sid, &[]);
    r.allow(&req2);
    r.wait("resumed turn done", 60, || done_count(&r, &sid) >= 1);
    let first = chat_requests(&log, mark).into_iter().next().expect("续接回合发了聊天请求");
    let users = first["messages"].as_str().unwrap_or("").split(',').filter(|m| *m == "user").count();
    assert!(users >= 3, "上下文接上了（前两轮 + 本轮）: {}", first["messages"]);
    assert!(written.exists() && !stray.join("mock-ok.txt").exists(), "工作目录没被入队项里的旧值带偏");

    r.stop();
    let _ = mock.kill();
    let _ = std::fs::remove_dir_all(&data);
}

#[test]
fn rust_sessions_switch_models_across_providers_persist_settings_and_reject_bad_input() {
    let Some(e) = env() else { eprintln!("skip: PI_RS_BIN/MOCK_OPENAI not set"); return };
    let (mut mock, port, log) = start_mock_logged(&e, &[]);
    let data = temp("modelswitch");
    let r = Rig::start(&e, port, &data);

    // 第二个来源：无 key 的自定义 provider，两个模型，同一个 mock 端点。
    let alt = json!({ "id": "alt", "name": "Alt", "auth": { "method": "none" },
        "runtimes": { "pi": { "baseUrl": format!("http://127.0.0.1:{port}/v1"), "wireProtocol": "openai-chat", "models": [{ "id": "alt-a", "name": "Alt A" }, { "id": "alt-b", "name": "Alt B" }] } } });
    assert_eq!(r.ipc("maker:provider:custom:create", json!([alt, {}, {}])).unwrap()["ok"], true);

    let row = r.new_row("Switch E2E", "smoke");
    let sid = row["id"].as_str().unwrap().to_string();
    let mut opts = json!({ "agentKind": "pi", "id": sid, "model": "mock-model", "providerId": "smoke", "permissionMode": "ask", "workingDir": row["workingDir"], "workspaceKind": "dialogue" });
    let mut n = 0usize;
    // 跑一轮，返回这一轮 mock 收到的模型集合。renderer 的 createOpts 是建会话时的快照，切换后也不变：宿主必须以 set-model 为准。
    let turn = |text: &str, n: &mut usize, opts: &Value| -> Vec<String> {
        *n += 1;
        let mark = log.lock().unwrap().len();
        r.enqueue_text(&sid, &format!("u{n}"), text, opts).unwrap();
        r.wait("turn done", 60, || done_count(&r, &sid) >= *n);
        let mut models: Vec<String> = chat_requests(&log, mark).iter().filter_map(|q| q["model"].as_str().map(String::from)).collect();
        models.sort();
        models.dedup();
        models
    };
    assert_eq!(turn("hi", &mut n, &opts), ["mock-model"]);

    // 1) 跨来源换模型：空闲时立即生效，已落库。
    let sel = json!({ "effort": null, "fastMode": false });
    let r1 = r.ipc("maker:set-model", json!([sid, "alt-a", "alt", null, sel])).unwrap();
    assert_eq!(r1["deferred"], false, "{r1}");
    let db1 = r.ipc("local-db:sessions:get", json!([sid])).unwrap();
    assert!(db1["model"] == "alt-a" && db1["providerId"] == "alt", "{db1}");
    assert_eq!(turn("after cross-provider switch", &mut n, &opts), ["alt-a"]);

    // 2) 同来源换模型（不带 providerId）。
    let r2 = r.ipc("maker:set-model", json!([sid, "alt-b", null, null, sel])).unwrap();
    assert_eq!(r2["deferred"], false, "{r2}");
    assert_eq!(turn("after same-provider switch", &mut n, &opts), ["alt-b"]);

    // 3) 其它运行时设置落库。
    r.ipc("maker:set-effort", json!([sid, "low"])).unwrap();
    r.ipc("maker:set-fast-mode", json!([sid, true])).unwrap();
    r.ipc("maker:set-permission-mode", json!([sid, "auto"])).unwrap();
    let db2 = r.ipc("local-db:sessions:get", json!([sid])).unwrap();
    assert!(db2["effort"] == "low" && db2["fastMode"] == true && db2["permissionMode"] == "auto", "{db2}");

    // 4) 非法参数与未配置的模型被拒，会话行不变。
    let bad = r.ipc("maker:set-model", json!([sid, "nope-model", "alt"])).unwrap_err();
    assert!(bad.contains("[MODEL_NOT_CONFIGURED]"), "{bad}");
    let empty = r.ipc("maker:set-model", json!([sid, ""])).unwrap_err();
    assert!(empty.contains("[INVALID_PARAMS]"), "{empty}");
    let db3 = r.ipc("local-db:sessions:get", json!([sid])).unwrap();
    assert!(db3["model"] == "alt-b" && db3["providerId"] == "alt", "被拒的切换不改会话: {db3}");

    // 5) 换回 smoke 后重启：懒创建按库里的模型走，而不是入队项里的旧 createOpts。
    r.ipc("maker:set-model", json!([sid, "mock-model", "smoke", null, sel])).unwrap();
    r.stop();
    let r = Rig::start(&e, port, &data);
    let persisted = r.ipc("local-db:sessions:get", json!([sid])).unwrap();
    assert!(persisted["model"] == "mock-model" && persisted["providerId"] == "smoke", "{persisted}");
    // 先切到 alt-a 再重启，确认库里的选择优先于入队项里的旧快照。
    r.ipc("maker:set-model", json!([sid, "alt-a", "alt", null, sel])).unwrap();
    r.stop();
    let r = Rig::start(&e, port, &data);
    opts["model"] = json!("mock-model");
    opts["providerId"] = json!("smoke"); // 故意给旧快照
    let mark = log.lock().unwrap().len();
    r.enqueue_text(&sid, "after-restart", "after restart", &opts).unwrap();
    r.wait("restart turn done", 60, || done_count(&r, &sid) >= 1);
    let models: Vec<String> = chat_requests(&log, mark).iter().filter_map(|q| q["model"].as_str().map(String::from)).collect();
    assert!(!models.is_empty() && models.iter().all(|m| m == "alt-a"), "重启后按库里的 alt-a 跑: {models:?}");

    r.stop();
    let _ = mock.kill();
    let _ = std::fs::remove_dir_all(&data);
}

#[test]
fn rust_sessions_cover_goal_limits_validation_slash_command_and_restart() {
    let Some(e) = env() else { eprintln!("skip: PI_RS_BIN/MOCK_OPENAI not set"); return };
    let (mut mock, port) = start_mock(&e);
    let data = temp("goal-more");
    let r = Rig::start(&e, port, &data);
    let bad = |res: Result<Value, String>| res.err().unwrap_or_default();

    // 1) 连续空轮 → paused；改上限后恢复，数到新上限。
    let d = start_goal(&r, "goal no progress", "chat only", json!({ "maxTurns": null, "budgetTokens": null, "noProgressLimit": 2 }));
    r.wait("goal paused", 40, || r.goal_status(&d)["status"] == "paused");
    let st = r.goal_status(&d);
    assert!(st["turnsUsed"] == 2 && st["lastReason"].as_str().unwrap_or("").contains("no tool use"), "{st}");
    r.ipc("maker:goal:update", json!([{ "sessionId": d, "patch": { "noProgressLimit": null, "maxTurns": 4 } }])).unwrap();
    assert_eq!(r.goal_status(&d)["maxTurns"], 4);
    r.ipc("maker:goal:resume", json!([d])).unwrap();
    r.wait("goal budgetLimited", 40, || r.goal_status(&d)["status"] == "budgetLimited");
    assert_eq!(r.goal_status(&d)["turnsUsed"], 4, "恢复后数到新上限");

    // 2) 改目标、清除；清除后推 goal: null。
    let g = start_goal(&r, "goal clear", "slow burn", json!({ "maxTurns": 50, "budgetTokens": null, "noProgressLimit": null }));
    r.ipc("maker:goal:pause", json!([g])).unwrap();
    r.wait("no turn in flight", 30, || !r.reg.invoke("maker:any-session-in-turn", &[]).map(|v| v == json!(true)).unwrap_or(false));
    r.ipc("maker:goal:update", json!([{ "sessionId": g, "patch": { "objective": "changed objective" } }])).unwrap();
    assert_eq!(r.goal_status(&g)["objective"], "changed objective");
    r.ipc("maker:goal:clear", json!([g])).unwrap();
    assert!(r.goal_status(&g).is_null(), "清除后状态为 null");
    r.wait("clear push", 10, || r.pushes.0.lock().unwrap().iter().any(|(c, a)| c == "maker:goal:status-changed" && a["sessionId"] == g.as_str() && a["goal"].is_null()));

    // 3) 参数校验。
    assert!(bad(r.ipc("maker:goal:set", json!([{ "sessionId": d, "objective": "   " }]))).contains("INVALID_PARAMS"));
    assert!(bad(r.ipc("maker:goal:set", json!([{ "sessionId": "no-such-session", "objective": "x" }]))).contains("NOT_FOUND"));
    assert!(bad(r.ipc("maker:goal:update", json!([{ "sessionId": d, "patch": { "maxTurns": -1 } }]))).contains("INVALID_PARAMS"));
    assert!(bad(r.ipc("maker:goal:update", json!([{ "sessionId": g, "patch": { "maxTurns": 3 } }]))).contains("GOAL_NOT_FOUND"));

    // 4) /goal 命令：无会话、裸命令、带文本、clear。
    let cmds = r.ipc("maker:list-desktop-commands", json!([])).unwrap();
    assert!(cmds["commands"].as_array().unwrap().iter().any(|c| c["name"] == "goal"), "命令面板里有 /goal");
    let f = r.new_session("goal command");
    r.ipc("maker:create-session", json!([{ "agentKind": "pi", "id": f, "model": "mock-model", "providerId": "smoke", "permissionMode": "bypassPermissions", "workspaceKind": "dialogue" }])).unwrap();
    let triggered = |pred: &dyn Fn(&Value) -> bool| r.pushes.0.lock().unwrap().iter().any(|(c, a)| c == "maker:desktop-command-triggered" && pred(a));
    r.ipc("maker:execute-desktop-command", json!(["goal", { "args": "x" }])).unwrap();
    assert!(triggered(&|x| x["error"] == "goal-no-session"), "无会话的 /goal → goal-no-session");
    r.ipc("maker:execute-desktop-command", json!(["goal", { "sessionId": f, "args": "" }])).unwrap();
    assert!(triggered(&|x| x["goalAction"] == "open-dialog" && x["sessionId"] == f.as_str()), "裸 /goal → open-dialog");
    r.ipc("maker:execute-desktop-command", json!(["goal", { "sessionId": f, "args": "via command MOCK_GOAL_COMPLETE_AFTER=2" }])).unwrap();
    assert!(triggered(&|x| x["goalAction"] == "set" && x["sessionId"] == f.as_str()), "/goal <text> → set");
    r.wait("goal by command complete", 40, || r.goal_status(&f)["status"] == "complete");
    assert_eq!(r.goal_status(&f)["turnsUsed"], 2);
    r.ipc("maker:execute-desktop-command", json!(["goal", { "sessionId": f, "args": "clear" }])).unwrap();
    assert!(r.goal_status(&f).is_null(), "/goal clear 清掉目标");
    assert!(triggered(&|x| x["goalAction"] == "cleared"), "/goal clear → cleared");

    // 5) 重启：active 目标接着跑到完成（关掉默认的无进展暂停，只测续跑）。
    let h = start_goal(&r, "goal restart", "survive restart MOCK_GOAL_COMPLETE_AFTER=4", json!({ "maxTurns": null, "budgetTokens": null, "noProgressLimit": null }));
    r.wait("first goal turn", 40, || r.goal_status(&h)["turnsUsed"].as_i64().unwrap_or(0) >= 1);
    r.stop();
    let r = Rig::start(&e, port, &data);
    r.wait("goal resumed and complete", 90, || r.goal_status(&h)["status"] == "complete");
    // 停进程时第 2 轮的 prompt 可能已进 Pi 历史但还没被控制器记作一轮；mock 按历史里的 [Goal] 消息数轮，所以允许差 1。
    let used = r.goal_status(&h)["turnsUsed"].as_i64().unwrap_or(0);
    assert!((3..=4).contains(&used), "重启后接着数轮并完成: {used}");

    r.stop();
    let _ = mock.kill();
    let _ = std::fs::remove_dir_all(&data);
}

fn start_cloud_mock(e: &Env, script_dir: &Path, gateway_port: u16, gateway_key: &str) -> (std::process::Child, u16) {
    let mut child = Command::new(&e.node)
        .arg(script_dir.join("mock-cindy-cloud.mjs"))
        .env("MOCK_GATEWAY_ENDPOINT", format!("http://127.0.0.1:{gateway_port}/keyed"))
        .env("MOCK_GATEWAY_KEY", gateway_key)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn cloud mock");
    let mut line = String::new();
    BufReader::new(child.stdout.as_mut().unwrap()).read_line(&mut line).unwrap();
    let port = serde_json::from_str::<Value>(&line).unwrap()["port"].as_u64().unwrap() as u16;
    (child, port)
}

#[test]
fn rust_sessions_reach_the_gateway_directly_with_the_real_key_and_no_internal_headers() {
    let Some(e) = env() else { eprintln!("skip: PI_RS_BIN/MOCK_OPENAI not set"); return };
    const GW_KEY: &str = "gw-e2e-0123456789abcdef";
    const GW_MODEL: &str = "mock-gw-model";
    let scripts = Path::new(&e.mock).parent().unwrap().to_path_buf();
    let (mut gateway, gw_port, gw_log) = start_mock_logged(&e, &[("MOCK_API_KEY", GW_KEY), ("MOCK_MODEL_ID", GW_MODEL), ("MOCK_LOG_HEADER_NAMES", "1")]);
    let (mut cloud, cloud_port) = start_cloud_mock(&e, &scripts, gw_port, GW_KEY);
    let data = temp("gateway");
    let base = format!("http://127.0.0.1:{cloud_port}");
    let r = Rig::start_with(&e, gw_port, &data, &[("CINDY_TAURI_AUTH_BASE_URL", base.clone()), ("CINDY_TAURI_MODEL_ACCESS_BASE_URL", base)]);

    // 登录（邮箱验证码），等凭据与模型清单同步回来。
    let email = "e2e@example.com";
    r.ipc("auth:exit-local", json!([])).unwrap();
    r.ipc("auth:get-login-state", json!([])).unwrap();
    let act = |a: Value| r.ipc("auth:dispatch-login-action", json!([a])).unwrap();
    assert_eq!(act(json!({ "type": "discover", "email": email }))["success"], true);
    assert_eq!(act(json!({ "type": "request-code", "kind": "email", "identifier": email, "captchaToken": "turnstile-ok" }))["success"], true);
    assert_eq!(act(json!({ "type": "verify-code", "kind": "email", "identifier": email, "code": "123456" }))["success"], true);
    r.wait("model-access ok", 30, || {
        let st = r.ipc("model-access:get-status", json!([])).unwrap_or(Value::Null);
        st["state"] == "ok" && st["accountTier"].is_string()
    });
    r.wait("gateway model listed", 30, || {
        r.ipc("maker:provider:list", json!([{}])).map(|v| v.to_string().contains(GW_MODEL)).unwrap_or(false)
    });

    // 网关会话（providerId 为 xd 与为空两种写法都要路由到 Cindy AI）。
    for (title, provider) in [("Gateway E2E", json!("xd")), ("Gateway null provider", Value::Null)] {
        let row = r.ipc("local-db:sessions:create", json!([{ "agentKind": "pi", "title": title, "workspaceKind": "dialogue", "model": GW_MODEL, "providerId": provider, "permissionMode": "ask" }])).unwrap();
        let sid = row["id"].as_str().unwrap().to_string();
        let mut opts = json!({ "agentKind": "pi", "id": sid, "model": GW_MODEL, "permissionMode": "ask", "workingDir": row["workingDir"], "workspaceKind": "dialogue" });
        if !provider.is_null() {
            opts["providerId"] = provider.clone();
        }
        let mark = gw_log.lock().unwrap().len();
        r.enqueue_text(&sid, "g1", "hi gateway", &opts).unwrap();
        r.wait("gateway turn", 90, || done_count(&r, &sid) >= 1 || r.pushes.0.lock().unwrap().iter().any(|(c, a)| c == "maker:event" && a["sessionId"] == sid.as_str() && a["event"]["type"] == "error"));
        let errored = r.pushes.0.lock().unwrap().iter().find(|(c, a)| c == "maker:event" && a["sessionId"] == sid.as_str() && a["event"]["type"] == "error").map(|(_, a)| a.clone());
        assert!(errored.is_none(), "网关回合没有报错: {errored:?}");
        assert!(done_count(&r, &sid) >= 1);
        let chats = chat_requests(&gw_log, mark);
        assert!(!chats.is_empty(), "网关收到了聊天请求");
        assert!(chats.iter().all(|q| q["auth"] == "ok" && q["model"] == GW_MODEL), "用真实网关 key 鉴权并带对模型: {chats:?}");
        let leaked: Vec<String> = chats.iter().flat_map(|q| q["headers"].as_array().cloned().unwrap_or_default()).filter_map(|h| h.as_str().map(String::from)).filter(|h| h.starts_with("x-cindy-pi-")).collect();
        assert!(leaked.is_empty(), "内部 Pi 头没有漏给网关: {leaked:?}");
    }

    // key 只在运行目录的 0600 文件里；pi-rs 的 models.json 里没有明文。
    {
        #[cfg(unix)]
        use std::os::unix::fs::PermissionsExt;
        let hits = dir_contains(&data, GW_KEY);
        let outside: Vec<_> = hits.iter().filter(|p| !p.to_string_lossy().contains("pi-runtime")).collect();
        assert!(outside.is_empty(), "网关 key 不落在运行目录以外（secrets 文件是加密的）: {outside:?}");
        assert!(!hits.is_empty());
        #[cfg(unix)]
        for p in &hits {
            assert_eq!(std::fs::metadata(p).unwrap().permissions().mode() & 0o777, 0o600, "{p:?}");
        }
        for p in hits.iter().filter(|p| p.extension().is_some_and(|x| x == "json")) {
            panic!("models.json 里不应含明文 key: {p:?}");
        }
    }
    r.stop();
    assert!(dir_contains(&data, GW_KEY).is_empty(), "关闭后磁盘上没有网关明文 key：{:?}", dir_contains(&data, GW_KEY));
    let _ = gateway.kill();
    let _ = cloud.kill();
    let _ = std::fs::remove_dir_all(&data);
}

#[test]
fn remote_controller_invokes_reach_rust_sessions_and_receive_their_pushes() {
    let Some(e) = env() else { eprintln!("skip: PI_RS_BIN/MOCK_OPENAI not set"); return };
    let (mut mock, port) = start_mock(&e);
    let data = temp("remote");
    let r = Rig::start(&e, port, &data);
    // 手机端经设备互联的 invoke：与生产同一个入口（本地库通道归本地库，其余按注册表分发）。
    let remote = |channel: &str, args: Value| -> Result<Value, String> {
        if crate::local_db::LocalDb::owns(channel) {
            return r.db.invoke(channel, args.as_array().unwrap());
        }
        handle_native(&r.reg, "maker.invoke", &json!({ "channel": channel, "args": args })).unwrap()
    };

    // 手机端发消息：必须落到 Rust 会话。
    let row = r.new_row("remote", "smoke");
    let sid = row["id"].as_str().unwrap().to_string();
    let opts = json!({ "agentKind": "pi", "id": sid, "model": "mock-model", "providerId": "smoke", "permissionMode": "ask", "workingDir": row["workingDir"], "workspaceKind": "dialogue" });
    let item = |id: &str, text: &str| json!({ "clientId": id, "text": text, "persistedContent": text, "createOpts": opts });
    remote("maker:input:enqueue", json!([sid, item("p1", "hi from phone")])).expect("远程入队");
    r.wait("phone turn done", 60, || done_count(&r, &sid) >= 1);
    assert!(r.reg.invoke("maker:list-active", &[]).unwrap().as_array().unwrap().iter().any(|s| s["sessionId"] == sid.as_str()), "会话在 Rust 注册表里");

    // 远程审批：手机看到待审批快照并放行。
    remote("maker:input:enqueue", json!([sid, item("p2", "MOCK_WRITE_FILE please")])).unwrap();
    let req = r.wait_approval(&sid, &[]);
    let snap = remote("maker:get-pending-interactions", json!([sid])).unwrap();
    assert!(snap.as_array().unwrap().iter().any(|x| x["request"]["requestId"] == req.as_str()), "待审批快照含该请求: {snap}");
    let resolved = remote("maker:resolve-interaction", json!([req, { "kind": "permission", "behavior": "allow" }])).unwrap();
    assert_eq!(resolved["accepted"], true, "{resolved}");
    r.wait("approved turn done", 60, || done_count(&r, &sid) >= 2);
    assert!(Path::new(row["workingDir"].as_str().unwrap()).join("mock-ok.txt").exists(), "手机放行后工具真的写了文件");
    let projection = remote("maker:input:get-projection", json!([sid])).unwrap();
    assert_eq!(projection["turnInProgress"], false);
    assert_eq!(remote("maker:session-in-turn", json!([sid])).unwrap(), json!(false));

    r.stop();
    let _ = mock.kill();
    let _ = std::fs::remove_dir_all(&data);
}

#[test]
fn rust_sessions_continue_an_imported_desktop_task_from_its_pi_session_file_with_the_imported_key() {
    let Some(e) = env() else { eprintln!("skip: PI_RS_BIN/MOCK_OPENAI not set"); return };
    const KEY: &str = "sk-import-0123456789abcdef";
    let (mut mock, port, log) = start_mock_logged(&e, &[("MOCK_API_KEY", KEY)]);
    let data = temp("import-continue");
    let r = Rig::start(&e, port, &data);

    // 导入后的状态：带 key 的 provider、会话行指向复制过来的桌面版 Pi 会话文件（首行 header 的 cwd 已改写成本机工作区）。
    let cfg = json!({ "id": "keyed", "name": "Keyed", "auth": { "method": "apiKey" },
        "runtimes": { "pi": { "baseUrl": format!("http://127.0.0.1:{port}/keyed"), "wireProtocol": "openai-chat", "models": [{ "id": "mock-model", "name": "Mock (keyed)" }] } } });
    assert_eq!(r.ipc("maker:provider:custom:create", json!([cfg, { "pi": KEY }, {}])).unwrap()["ok"], true);
    let row = r.new_row("Task imported", "keyed");
    let sid = row["id"].as_str().unwrap().to_string();
    let wd = row["workingDir"].as_str().unwrap().to_string();
    let sessions = data.join("pi-agent-home").join("sessions");
    std::fs::create_dir_all(&sessions).unwrap();
    let file = sessions.join("2026-10-01T08-00-00-000Z_imported.jsonl");
    let at = "2026-10-01T08:00:00.000Z";
    let lines = [
        json!({ "type": "session", "version": 3, "id": "imported", "timestamp": at, "cwd": wd }),
        json!({ "type": "model_change", "id": "m1", "parentId": null, "timestamp": at, "provider": "gw", "modelId": "mock-model" }),
        json!({ "type": "message", "id": "u1", "parentId": "m1", "timestamp": at, "message": { "role": "user", "content": [{ "type": "text", "text": "hello dialogue" }], "timestamp": 1790841600000i64 } }),
        json!({ "type": "message", "id": "a1", "parentId": "u1", "timestamp": at, "message": { "role": "assistant", "content": [{ "type": "text", "text": "reply dialogue" }], "api": "openai-completions", "provider": "gw", "model": "mock-model",
            "usage": { "input": 1, "output": 1, "cacheRead": 0, "cacheWrite": 0, "totalTokens": 2, "cost": { "input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0, "total": 0 } }, "stopReason": "stop", "timestamp": 1790841600001i64 } }),
    ];
    std::fs::write(&file, lines.iter().map(|l| l.to_string()).collect::<Vec<_>>().join("\n") + "\n").unwrap();
    r.db.update_session(&sid, &json!({ "sdkSessionId": file.to_string_lossy() })).unwrap();

    let opts = json!({ "agentKind": "pi", "id": sid, "model": "mock-model", "providerId": "keyed", "permissionMode": "ask", "workingDir": wd, "workspaceKind": "dialogue" });
    let mark = log.lock().unwrap().len();
    r.enqueue_text(&sid, "c1", "continue", &opts).unwrap();
    r.wait("imported task turn", 60, || done_count(&r, &sid) >= 1);
    let errored = r.pushes.0.lock().unwrap().iter().any(|(c, a)| c == "maker:event" && a["sessionId"] == sid.as_str() && a["event"]["type"] == "error");
    assert!(!errored, "导入的任务续接没有报错");
    let authed: Vec<Value> = chat_requests(&log, mark).into_iter().filter(|q| q["path"].as_str().is_some_and(|p| p.contains("/keyed/")) && q["auth"] == "ok").collect();
    assert!(!authed.is_empty(), "用导入的 key 鉴权通过");
    let roles = authed.last().unwrap()["messages"].as_str().unwrap_or("").to_string();
    assert!(roles.contains("user,assistant,user"), "续接的回合带着桌面版的历史: {roles}");
    assert!(dir_contains(&data, KEY).iter().all(|p| p.to_string_lossy().contains("pi-runtime")), "key 只在运行目录里: {:?}", dir_contains(&data, KEY));

    r.stop();
    let _ = mock.kill();
    let _ = std::fs::remove_dir_all(&data);
}

/// 附加目录：设置落库、可写目录需要选择器授权、提示词带进每轮消息、权限档带 writableRoots，重启后仍然生效。
#[test]
fn rust_sessions_extra_and_writable_dirs_are_persisted_prompted_and_granted() {
    let Some(e) = env() else { eprintln!("skip: PI_RS_BIN/MOCK_OPENAI not set"); return };
    let (mut mock, port, log) = start_mock_logged(&e, &[]);
    let data = temp("extradirs");
    let r = Rig::start(&e, port, &data);
    let row = r.new_row("Dirs E2E", "smoke");
    let sid = row["id"].as_str().unwrap().to_string();
    let opts = json!({ "agentKind": "pi", "id": sid, "model": "mock-model", "providerId": "smoke", "permissionMode": "ask", "workingDir": row["workingDir"], "workspaceKind": "dialogue" });
    let refs = data.join("refs-dir");
    let out = data.join("out-dir");
    std::fs::create_dir_all(&refs).unwrap();
    std::fs::create_dir_all(&out).unwrap();
    let (refs_s, out_s) = (refs.to_string_lossy().to_string(), out.to_string_lossy().to_string());
    let last_user = |mark: usize| -> String { chat_requests(&log, mark).last().and_then(|q| q["lastUser"].as_str().map(String::from)).unwrap_or_default() };

    // 1) 没设目录：消息原样发出。
    let mark = log.lock().unwrap().len();
    r.enqueue_text(&sid, "d1", "hello", &opts).unwrap();
    r.wait("turn 1", 60, || done_count(&r, &sid) >= 1);
    assert_eq!(last_user(mark), "hello");

    // 2) 只读目录：不需要选择器；运行中的会话下一轮就带上提示，库里已落库。
    let kept = r.ipc("maker:set-extra-dirs", json!([sid, [refs_s, refs_s, "relative/x", "/no/such/dir-xyz"]])).unwrap();
    assert_eq!(kept, json!([refs_s]), "去重并丢掉非法项");
    let db = r.ipc("local-db:sessions:get", json!([sid])).unwrap();
    assert_eq!(db["extraDirs"], json!([refs_s]), "{db}");
    let mark = log.lock().unwrap().len();
    r.enqueue_text(&sid, "d2", "second", &opts).unwrap();
    r.wait("turn 2", 60, || done_count(&r, &sid) >= 2);
    let msg = last_user(mark);
    assert!(msg.contains("<cindy-extra-reference-directories>") && msg.contains("- refs-dir") && msg.ends_with("second"), "{msg}");

    // 3) 可写目录：没经过选择器的一律拒绝，库里不变。
    let denied = r.ipc("maker:set-writable-dirs", json!([sid, [out_s]])).unwrap_err();
    assert!(denied.contains("[PRECONDITION_FAILED]") && denied.contains("system picker"), "{denied}");
    assert_eq!(r.ipc("local-db:sessions:get", json!([sid])).unwrap()["writableDirs"], json!([]));

    // 4) 经选择器选中后放行（选择器本身需要窗口，这里直接登记授权）。授权只能用一次。
    crate::dir_grants::issue_picker_grant(&sid, &out_s).unwrap();
    assert_eq!(r.ipc("maker:set-writable-dirs", json!([sid, [out_s]])).unwrap(), json!([out_s]));
    assert_eq!(r.ipc("local-db:sessions:get", json!([sid])).unwrap()["writableDirs"], json!([out_s]));
    let perm: Value = serde_json::from_str(&std::fs::read_to_string(data.join("pi-runtime").join(&sid).join("permission.json")).unwrap()).unwrap();
    assert_eq!(perm["writableRoots"], json!([out_s]), "{perm}");
    assert_eq!(perm["mode"], "ask", "改可写目录不能冲掉权限档位: {perm}");
    r.ipc("maker:set-permission-mode", json!([sid, "auto"])).unwrap();
    let perm: Value = serde_json::from_str(&std::fs::read_to_string(data.join("pi-runtime").join(&sid).join("permission.json")).unwrap()).unwrap();
    assert!(perm["mode"] == "auto" && perm["writableRoots"] == json!([out_s]), "改档位不能冲掉可写目录: {perm}");
    let mark = log.lock().unwrap().len();
    r.enqueue_text(&sid, "d3", "third", &opts).unwrap();
    r.wait("turn 3", 60, || done_count(&r, &sid) >= 3);
    let msg = last_user(mark);
    assert!(msg.contains("<cindy-extra-writable-directories>") && msg.contains(&format!("- {out_s}")), "{msg}");

    // 5) 只读和可写不能指向同一棵树。
    let none = r.ipc("maker:set-extra-dirs", json!([sid, [refs_s, out_s]])).unwrap();
    assert_eq!(none, json!([refs_s]), "已是可写目录的不能再当只读: {none}");

    // 6) 重启后按库里的目录重建：权限档与提示词都在。
    r.stop();
    let r = Rig::start(&e, port, &data);
    let mark = log.lock().unwrap().len();
    r.enqueue_text(&sid, "d4", "after restart", &opts).unwrap();
    r.wait("turn after restart", 60, || done_count(&r, &sid) >= 1);
    let msg = last_user(mark);
    assert!(msg.contains("- refs-dir") && msg.contains(&format!("- {out_s}")) && msg.ends_with("after restart"), "{msg}");
    let perm: Value = serde_json::from_str(&std::fs::read_to_string(data.join("pi-runtime").join(&sid).join("permission.json")).unwrap()).unwrap();
    assert_eq!(perm["writableRoots"], json!([out_s]), "{perm}");

    // 7) 撤销：空数组清掉授权，提示消失。
    assert_eq!(r.ipc("maker:set-writable-dirs", json!([sid, []])).unwrap(), json!([]));
    assert_eq!(r.ipc("maker:set-extra-dirs", json!([sid, []])).unwrap(), json!([]));
    let mark = log.lock().unwrap().len();
    r.enqueue_text(&sid, "d5", "plain", &opts).unwrap();
    r.wait("turn after revoke", 60, || done_count(&r, &sid) >= 2);
    assert_eq!(last_user(mark), "plain");

    r.stop();
    let _ = mock.kill();
    let _ = std::fs::remove_dir_all(&data);
}

/// 排队中的消息可以编辑：加锁、替换内容，下一轮用的是编辑后的内容；已经派发的消息不受影响。
#[test]
fn rust_sessions_queued_message_can_be_edited() {
    let Some(e) = env() else { eprintln!("skip: PI_RS_BIN/MOCK_OPENAI not set"); return };
    let (mut mock, port, log) = start_mock_logged(&e, &[]);
    let data = temp("queue-edit");
    let r = Rig::start(&e, port, &data);
    let sid = r.new_session("queue edit");
    let opts = json!({ "agentKind": "pi", "id": sid, "model": "mock-model", "providerId": "smoke", "permissionMode": "bypassPermissions", "workspaceKind": "dialogue" });
    let item = |cid: &str, text: &str| json!({ "clientId": cid, "text": text, "persistedContent": text, "createOpts": opts });

    r.ipc("maker:input:enqueue", json!([sid, item("q1", "MOCK_SLOW first")])).unwrap();
    r.wait("turn is running", 20, || r.ipc("maker:session-in-turn", json!([sid])).map(|v| v == json!(true)).unwrap_or(false));
    let p = r.ipc("maker:input:enqueue", json!([sid, item("q2", "original")])).unwrap();
    assert_eq!(p["pendingQueue"].as_array().map(|a| a.len()), Some(1), "{p}");

    // 编辑：加锁 → 替换 → 解锁，每一步都不报错，队列里的文本变了。
    let locked = r.ipc("maker:input:set-edit-lock", json!([sid, "q2", true])).unwrap();
    assert_eq!(locked["pendingQueue"][0]["clientId"], "q2", "{locked}");
    let upd = r.ipc("maker:input:update-content", json!([sid, "q2", item("q2", "edited")])).unwrap();
    assert!(!upd.is_null(), "update-content 要返回投影: {upd}");
    assert_eq!(upd["pendingQueue"][0]["text"], "edited", "{upd}");
    r.ipc("maker:input:set-edit-lock", json!([sid, "q2", false])).unwrap();

    let mark = log.lock().unwrap().len();
    r.wait("both turns done", 60, || done_count(&r, &sid) >= 2);
    let last = chat_requests(&log, mark).last().and_then(|q| q["lastUser"].as_str().map(String::from)).unwrap_or_default();
    assert_eq!(last, "edited", "第二轮发的是编辑后的内容");

    r.stop();
    let _ = mock.kill();
    let _ = std::fs::remove_dir_all(&data);
}
