//! 端到端：桌面版导入。Rust 本地库 / 模型服务 / secret store + import_*，
//! 夹具是临时目录里造的桌面版 profile（与 desktop schema 同列的 SQLite、Pi 会话文件、dialogue 工作区、safeStorage 格式的 .enc）。
//! 对应原 scripts/desktop-import-e2e.mjs。需要 NODE_BIN / PI_RS_BIN / MOCK_OPENAI，缺一就跳过。
//! .enc 用 Node crypto 生成的参考向量（import_crypto 的测试同源），所以 macOS 路径可测；Windows 的 DPAPI 路径不在这里测。

#![cfg(test)]

use crate::import_db::owner_storage_key;
use crate::import_run::{DesktopImport, ImportPorts};
use crate::rust_session_e2e::{env, Rig};
use rusqlite::Connection;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// 两个测试都要改进程级的 profile 目录环境变量，串行跑。
static SERIAL: Mutex<()> = Mutex::new(());

const USER_ID: &str = "cmfixtureuser0000000000001";
const MAC_PW: &str = "vector-password-Zk3";
const MAC_ENC: &str = "djEwa/+7n0XBGB+8M/hSbtmdaiCJGZpV5dyNIX9l1JZIWgE=";
const API_KEY: &str = "sk-vector-mac-123";

const DESKTOP_SCHEMA: &str = "
CREATE TABLE sessions (
  id text PRIMARY KEY NOT NULL, title text DEFAULT 'New CCS' NOT NULL, working_dir text,
  model text DEFAULT 'claude-sonnet-4-6' NOT NULL, effort text DEFAULT 'high' NOT NULL,
  permission_mode text DEFAULT 'ask' NOT NULL, status text DEFAULT 'active' NOT NULL, sdk_session_id text,
  total_token_usage integer DEFAULT 0 NOT NULL, total_cost_usd real DEFAULT 0 NOT NULL,
  context_tokens integer DEFAULT 0 NOT NULL, context_window integer DEFAULT 0 NOT NULL,
  fast_mode integer DEFAULT false NOT NULL, cleared_at integer, pinned_at integer,
  created_at integer NOT NULL, updated_at integer NOT NULL, agent_kind text DEFAULT 'cc' NOT NULL,
  user_send_at integer, parent_session_id text, source text DEFAULT 'desktop' NOT NULL,
  extra_dirs text DEFAULT '[]' NOT NULL, orca_role text, workspace_kind text DEFAULT 'project' NOT NULL,
  remote_host_id text, summary text, provider_id text, plan_mode_enabled integer DEFAULT false NOT NULL,
  active_turn_started_at integer, active_turn_pid integer, last_turn_ended_at integer,
  writable_dirs text DEFAULT '[]' NOT NULL, agent_device_id text, hidden_at integer,
  last_activity_at integer, notifications_muted integer DEFAULT false NOT NULL, project_dir text
);
CREATE TABLE messages (
  id text PRIMARY KEY NOT NULL, client_id text NOT NULL, session_id text NOT NULL, role text NOT NULL,
  content text NOT NULL, tool_use_id text, created_at integer NOT NULL, agent_meta text, rewind_at integer, agent_kind text,
  FOREIGN KEY (session_id) REFERENCES sessions(id) ON DELETE cascade
);
CREATE TABLE right_sidebar_tabs (
  id text PRIMARY KEY NOT NULL, session_id text NOT NULL, kind text NOT NULL, position integer NOT NULL,
  is_active integer DEFAULT false NOT NULL, state text DEFAULT '{}' NOT NULL, created_at integer NOT NULL, updated_at integer NOT NULL
);
CREATE TABLE custom_providers (
  id text PRIMARY KEY NOT NULL, name text NOT NULL, runtimes text DEFAULT '{}' NOT NULL, sort_order integer DEFAULT 0 NOT NULL,
  created_at integer NOT NULL, updated_at integer NOT NULL, auth text
);
CREATE TABLE recent_workdirs (path text PRIMARY KEY NOT NULL, last_used_at integer NOT NULL);
CREATE TABLE project_aliases (project_key text PRIMARY KEY NOT NULL, alias text NOT NULL, updated_at integer NOT NULL);
";

struct FakePorts {
    db: Arc<crate::local_db::LocalDb>,
    models: Arc<crate::model_config::host::ModelHost>,
    deny_keychain: bool,
    keychain_asks: Mutex<u32>,
    answers: Mutex<VecDeque<i64>>,
    dialogs: Mutex<Vec<Value>>,
    emitted: Mutex<Vec<Value>>,
    locale: String,
}

impl FakePorts {
    fn new(db: Arc<crate::local_db::LocalDb>, models: Arc<crate::model_config::host::ModelHost>, locale: &str, deny: bool, answers: &[i64]) -> Arc<Self> {
        Arc::new(FakePorts {
            db,
            models,
            deny_keychain: deny,
            keychain_asks: Mutex::new(0),
            answers: Mutex::new(answers.iter().copied().collect()),
            dialogs: Mutex::new(Vec::new()),
            emitted: Mutex::new(Vec::new()),
            locale: locale.to_string(),
        })
    }
}

impl ImportPorts for FakePorts {
    fn sidecar(&self, method: &str, params: Value) -> Result<Value, String> {
        if method == "host.local-db-file" {
            return Ok(json!(self.db.file.to_string_lossy()));
        }
        match self.models.call(method, &params) {
            Some(r) => r,
            None => Err(format!("[NOT_IMPLEMENTED] {method}")),
        }
    }
    fn safe_storage_key(&self, _request: &Value) -> Result<Value, String> {
        *self.keychain_asks.lock().unwrap() += 1;
        if self.deny_keychain {
            return Err("[PERMISSION_DENIED] user denied keychain access".into());
        }
        Ok(json!({ "scheme": "mac-v10", "password": MAC_PW }))
    }
    fn dialog(&self, params: Value) -> Result<i64, String> {
        self.dialogs.lock().unwrap().push(params);
        Ok(self.answers.lock().unwrap().pop_front().unwrap_or(0))
    }
    fn emit(&self, channel: &str, arg: Value) {
        if channel == "local-db:sessions:created" {
            self.emitted.lock().unwrap().push(arg);
        }
    }
    fn locale(&self) -> String {
        self.locale.clone()
    }
}

fn temp(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("cindy-import-e2e-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

struct Fixture {
    profile: PathBuf,
    ids: std::collections::HashMap<&'static str, String>,
    project: PathBuf,
}

fn uuid(n: u32) -> String {
    format!("00000000-0000-4000-8000-{n:012}")
}

fn build_profile(work: &Path) -> Fixture {
    let profile = work.join("CindyGlobal");
    std::fs::create_dir_all(&profile).unwrap();
    let sql = Connection::open(profile.join(format!("cindy-{USER_ID}.db"))).unwrap();
    sql.execute_batch("PRAGMA journal_mode = WAL").unwrap();
    sql.execute_batch(DESKTOP_SCHEMA).unwrap();
    let base: i64 = 1_790_841_600_000;
    let owner = owner_storage_key(USER_ID);
    let dialogues = profile.join("owners").join(&owner).join("dialogues").join("2026-10-01");
    let pi_sessions = profile.join("pi-agent-home").join("sessions");
    std::fs::create_dir_all(&pi_sessions).unwrap();
    let project = work.join("some-project");
    let mut ids = std::collections::HashMap::new();
    // (名称, agent_kind, status, workspace, remote, orca, 桌面版里还在跑的回合)
    let specs: [(&'static str, &str, &str, &str, Option<&str>, Option<&str>, bool); 6] = [
        ("dialogue", "pi", "active", "dialogue", None, None, true),
        ("archived", "pi", "archived", "dialogue", None, None, false),
        ("project", "pi", "active", "project", None, None, false),
        ("claude", "cc", "active", "dialogue", None, None, false),
        ("remote", "pi", "active", "dialogue", Some("host-1"), None, false),
        ("team", "pi", "active", "dialogue", None, Some("lead"), false),
    ];
    for (n, (name, kind, status, workspace, remote, orca, active)) in specs.iter().enumerate() {
        let id = uuid(n as u32 + 1);
        let at = base + (n as i64 + 1) * 60_000;
        let wd = if *workspace == "dialogue" {
            let d = dialogues.join(&id);
            std::fs::create_dir_all(&d).unwrap();
            std::fs::write(d.join("notes.md"), format!("# {name}\n")).unwrap();
            d.to_string_lossy().into_owned()
        } else {
            project.to_string_lossy().into_owned()
        };
        let pi_id = format!("pi-{n}");
        let pi_file = pi_sessions.join(format!("2026-10-01T08-00-00-000Z_{pi_id}.jsonl"));
        let iso = "2026-10-01T08:00:00.000Z";
        let lines = [
            json!({ "type": "session", "version": 3, "id": pi_id, "timestamp": iso, "cwd": wd }),
            json!({ "type": "model_change", "id": "m1", "parentId": null, "timestamp": iso, "provider": "gw", "modelId": "mock-model" }),
            json!({ "type": "message", "id": "u1", "parentId": "m1", "timestamp": iso, "message": { "role": "user", "content": [{ "type": "text", "text": format!("hello {name}") }], "timestamp": at } }),
        ];
        std::fs::write(&pi_file, lines.iter().map(|l| l.to_string()).collect::<Vec<_>>().join("\n") + "\n").unwrap();
        sql.execute(
            "INSERT INTO sessions (id, title, working_dir, model, permission_mode, status, sdk_session_id, created_at, updated_at, agent_kind,
              workspace_kind, provider_id, active_turn_started_at, remote_host_id, orca_role, last_activity_at, project_dir)
             VALUES (?1, ?2, ?3, 'mock-model', 'ask', ?4, ?5, ?6, ?6, ?7, ?8, 'gw', ?9, ?10, ?11, ?6, ?12)",
            rusqlite::params![id, format!("Task {name}"), wd, status, pi_file.to_string_lossy(), at, kind, workspace, active.then_some(base), remote, orca, (*workspace == "project").then(|| project.to_string_lossy().into_owned())],
        )
        .unwrap();
        for (suffix, role, content, dt) in [("u1", "user", json!({ "text": format!("hello {name}") }).to_string(), 0), ("a1", "assistant", json!([{ "type": "text", "text": format!("reply {name}") }]).to_string(), 1)] {
            sql.execute(
                "INSERT INTO messages (id, client_id, session_id, role, content, created_at, agent_kind) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                rusqlite::params![format!("{id}-{suffix}"), format!("{name}-{suffix}"), id, role, content, at + dt, kind],
            )
            .unwrap();
        }
        ids.insert(*name, id);
    }
    sql.execute(
        "INSERT INTO right_sidebar_tabs (id, session_id, kind, position, is_active, state, created_at, updated_at) VALUES ('tab1', ?1, 'browser', 0, 1, ?2, ?3, ?3)",
        rusqlite::params![ids["dialogue"], json!({ "url": "https://example.com/" }).to_string(), base],
    )
    .unwrap();
    sql.execute("INSERT INTO recent_workdirs (path, last_used_at) VALUES (?1, ?2)", rusqlite::params![project.to_string_lossy(), base]).unwrap();
    sql.execute("INSERT INTO project_aliases (project_key, alias, updated_at) VALUES (?1, 'Some Project', ?2)", rusqlite::params![project.to_string_lossy(), base]).unwrap();
    let pi = |suffix: &str| json!({ "pi": { "baseUrl": format!("http://127.0.0.1:1{suffix}"), "wireProtocol": "openai-chat", "models": [{ "id": "mock-model", "name": "Mock" }] } }).to_string();
    let providers: [(&str, &str, String, Option<&str>); 5] = [
        ("keyed", "Keyed Gateway", pi("/keyed"), Some(r#"{"method":"apiKey"}"#)),
        ("gw", "Open Gateway", pi(""), Some(r#"{"method":"none"}"#)),
        ("nokey", "No Key", pi("/keyed"), None),
        ("oauthy", "OAuth Thing", pi(""), Some(r#"{"method":"oauth","oauth":{"flow":"device-code"}}"#)),
        ("cconly", "CC Only", json!({ "claude-code": { "baseUrl": "https://example.com" } }).to_string(), None),
    ];
    for (i, (id, name, rt, auth)) in providers.iter().enumerate() {
        sql.execute(
            "INSERT INTO custom_providers (id, name, runtimes, sort_order, created_at, updated_at, auth) VALUES (?1, ?2, ?3, ?4, ?5, ?5, ?6)",
            rusqlite::params![id, name, rt, i as i64, base, auth],
        )
        .unwrap();
    }
    drop(sql);
    let ss = profile.join("safe-storage");
    std::fs::create_dir_all(&ss).unwrap();
    std::fs::write(ss.join(format!("owner_{owner}_provider_key_keyed_pi.enc")), MAC_ENC).unwrap();
    Fixture { profile, ids, project }
}

/// 目录内容哈希。skip：桌面版运行中（有 WAL）时 -shm 里的读标记由 SQLite 自己协调，不算改动。
fn hash_tree(dir: &Path, skip_shm: bool) -> String {
    let mut h = Sha256::new();
    fn walk(d: &Path, root: &Path, h: &mut Sha256, skip_shm: bool) {
        let mut names: Vec<_> = std::fs::read_dir(d).unwrap().flatten().map(|e| e.path()).collect();
        names.sort();
        for p in names {
            if skip_shm && p.to_string_lossy().ends_with("-shm") {
                continue;
            }
            if p.is_dir() {
                walk(&p, root, h, skip_shm);
            } else {
                h.update(p.strip_prefix(root).unwrap().to_string_lossy().as_bytes());
                h.update(std::fs::read(&p).unwrap());
            }
        }
    }
    walk(dir, dir, &mut h, skip_shm);
    h.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

fn files_containing(dir: &Path, needle: &str, hits: &mut Vec<PathBuf>) {
    for e in std::fs::read_dir(dir).unwrap().flatten() {
        let p = e.path();
        if p.is_dir() {
            if p.file_name().is_some_and(|n| n != "agents") {
                files_containing(&p, needle, hits);
            }
        } else if std::fs::metadata(&p).map(|m| m.len() < 20 * 1024 * 1024).unwrap_or(false) && std::fs::read(&p).unwrap().windows(needle.len()).any(|w| w == needle.as_bytes()) {
            hits.push(p);
        }
    }
}

fn start(e: &crate::rust_session_e2e::Env, data: &Path) -> Rig {
    Rig::start(e, 1, data)
}

fn local_db(data: &Path) -> Connection {
    let f = std::fs::read_dir(data).unwrap().flatten().map(|e| e.path()).find(|p| p.file_name().is_some_and(|n| n.to_string_lossy().starts_with("cindy-tauri-") && n.to_string_lossy().ends_with(".db"))).expect("本地库文件");
    Connection::open(f).unwrap()
}

#[test]
fn desktop_import_scans_imports_is_idempotent_and_leaves_the_desktop_profile_alone() {
    let Some(e) = env() else { eprintln!("skip: PI_RS_BIN/MOCK_OPENAI not set"); return };
    if cfg!(windows) {
        return;
    }
    let _serial = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    let work = temp("main");
    let fx = build_profile(&work);
    std::env::set_var("CINDY_TAURI_DESKTOP_PROFILE_DIRS", &fx.profile);
    let source_hash = hash_tree(&fx.profile, false);
    let data = work.join("tauri-data");
    let rig = start(&e, &data);
    let ports = FakePorts::new(rig.db.clone(), rig.models.clone(), "en", false, &[]);
    let imp = DesktopImport::new(data.clone(), ports.clone());

    // 1) 扫描
    let scan = imp.invoke("desktop-import:scan", &[]).unwrap();
    let cands = scan["candidates"].as_array().unwrap();
    assert_eq!(cands.len(), 1, "{scan}");
    assert_eq!(cands[0]["ownerId"], USER_ID);
    assert_eq!(cands[0]["sessions"], 3, "只数本机 Pi 任务: {scan}");
    assert_eq!(cands[0]["providers"], 5);
    assert!(scan["marker"].is_null());
    let bad = imp.invoke("desktop-import:run", &[json!({ "profileDir": "/etc", "dbFile": "cindy-x.db" })]).unwrap_err();
    assert!(bad.contains("NOT_FOUND"), "不接受任意路径: {bad}");

    // 2) 导入
    let arg = json!({ "profileDir": cands[0]["profileDir"], "dbFile": cands[0]["dbFile"] });
    let r = imp.invoke("desktop-import:run", &[arg.clone()]).unwrap();
    assert_eq!(r["ok"], true, "{r}");
    assert_eq!(r["sessions"], json!({ "imported": 3, "existing": 0 }), "{r}");
    assert_eq!(r["messages"], 6);
    assert_eq!(r["tabs"], 1);
    assert_eq!((r["recent_workdirs"].clone(), r["project_aliases"].clone()), (json!(1), json!(1)));
    assert_eq!(r["providers"]["imported"], 3, "{r}");
    let skipped: Vec<&str> = r["providers"]["skipped"].as_array().unwrap().iter().filter_map(|p| p["id"].as_str()).collect();
    assert_eq!(skipped.len(), 2);
    assert!(skipped.contains(&"oauthy") && skipped.contains(&"cconly"), "{skipped:?}");
    assert_eq!(r["keys"]["imported"], 1, "{r}");
    assert_eq!(r["keys"]["missing"], json!(["nokey"]));
    assert_eq!(r["piSessions"]["copied"], 3, "{r}");
    assert_eq!(r["workdirs"]["copied"], 2, "{r}");
    assert!(Path::new(r["backup"].as_str().unwrap()).exists(), "导入前整库备份");
    assert_eq!(*ports.keychain_asks.lock().unwrap(), 1);
    assert_eq!(ports.emitted.lock().unwrap().len(), 3, "renderer 收到 3 个新任务");

    // 3) 导入后的数据
    let db = local_db(&data);
    let one = |sql: &str, id: &str| db.query_row(sql, [id], |r| r.get::<_, Option<String>>(0)).unwrap();
    let count = |sql: &str| db.query_row(sql, [], |r| r.get::<_, i64>(0)).unwrap();
    assert_eq!(count("SELECT COUNT(*) FROM sessions"), 3);
    for gone in ["claude", "remote", "team"] {
        assert_eq!(db.query_row("SELECT COUNT(*) FROM sessions WHERE id = ?1", [&fx.ids[gone]], |r| r.get::<_, i64>(0)).unwrap(), 0, "{gone} 不导入");
    }
    assert_eq!(one("SELECT status FROM sessions WHERE id = ?1", &fx.ids["archived"]).as_deref(), Some("archived"));
    let expected_dir = data.join("dialogues").join("2026-10-01").join(&fx.ids["dialogue"]);
    assert_eq!(one("SELECT working_dir FROM sessions WHERE id = ?1", &fx.ids["dialogue"]).as_deref(), Some(expected_dir.to_str().unwrap()));
    assert!(expected_dir.join("notes.md").exists());
    let sdk = one("SELECT sdk_session_id FROM sessions WHERE id = ?1", &fx.ids["dialogue"]).unwrap();
    assert!(sdk.starts_with(data.join("pi-agent-home").join("sessions").to_str().unwrap()) && Path::new(&sdk).exists(), "{sdk}");
    let head: Value = serde_json::from_str(std::fs::read_to_string(&sdk).unwrap().lines().next().unwrap()).unwrap();
    assert_eq!(head["cwd"], expected_dir.to_str().unwrap(), "Pi 会话文件首行 cwd 改写");
    assert_eq!(one("SELECT working_dir FROM sessions WHERE id = ?1", &fx.ids["project"]).as_deref(), Some(fx.project.to_str().unwrap()), "项目目录原样保留");
    assert_eq!(db.query_row("SELECT active_turn_started_at FROM sessions WHERE id = ?1", [&fx.ids["dialogue"]], |r| r.get::<_, Option<i64>>(0)).unwrap(), None, "桌面版未完成的回合不当作被中断");
    assert_eq!(count("SELECT COUNT(*) FROM messages"), 6);
    assert_eq!(count("SELECT COUNT(*) FROM right_sidebar_tabs"), 1);

    let list = rig.ipc("maker:provider:list", json!([])).unwrap();
    let connected = |id: &str| list["providers"].as_array().unwrap().iter().find(|p| p["id"] == id).map(|p| p["connected"].clone());
    assert_eq!(connected("keyed"), Some(json!(true)), "{list}");
    assert_eq!(connected("gw"), Some(json!(true)));
    assert_eq!(connected("nokey"), Some(json!(false)), "没有 key 的要在设置里补");
    assert_eq!(rig.ipc("safe-storage-read", json!(["provider_key_keyed_pi"])).unwrap(), json!(API_KEY), "renderer 读到导入的 key");
    assert!(!std::fs::read_to_string(data.join("providers.json")).unwrap().contains(API_KEY));
    let marker = imp.invoke("desktop-import:scan", &[]).unwrap()["marker"].clone();
    assert_eq!((marker["status"].clone(), marker["sessions"].clone()), (json!("imported"), json!(3)));

    // 4) 幂等：再导一次什么都不变，也不再要钥匙串
    let again = imp.invoke("desktop-import:run", &[arg]).unwrap();
    assert_eq!(again["sessions"], json!({ "imported": 0, "existing": 3 }), "{again}");
    assert_eq!((again["providers"]["imported"].clone(), again["providers"]["exists"].clone()), (json!(0), json!(3)));
    assert_eq!(*ports.keychain_asks.lock().unwrap(), 1, "已存在的 provider 不再要 key");
    assert_eq!(count("SELECT COUNT(*) FROM messages"), 6);

    rig.stop();
    // 5) 桌面版数据没被动；数据目录里没有明文 key
    assert_eq!(hash_tree(&fx.profile, false), source_hash, "桌面版 profile 不变");
    let mut hits = Vec::new();
    files_containing(&data, API_KEY, &mut hits);
    assert!(hits.is_empty(), "数据目录里出现明文 key: {hits:?}");
    let _ = std::fs::remove_dir_all(&work);
}

#[test]
fn desktop_import_first_launch_prompt_covers_wal_only_tasks_denied_keychain_dismiss_and_locale() {
    let Some(e) = env() else { eprintln!("skip: PI_RS_BIN/MOCK_OPENAI not set"); return };
    if cfg!(windows) {
        return;
    }
    let _serial = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    let work = temp("prompt");
    let fx = build_profile(&work);
    std::env::set_var("CINDY_TAURI_DESKTOP_PROFILE_DIRS", &fx.profile);
    // 模拟桌面版正在运行：开着写连接、关掉自动 checkpoint，新任务只在 WAL 里。
    let writer = Connection::open(fx.profile.join(format!("cindy-{USER_ID}.db"))).unwrap();
    writer.execute_batch("PRAGMA journal_mode = WAL; PRAGMA wal_autocheckpoint = 0;").unwrap();
    writer.execute("INSERT INTO sessions (id, title, created_at, updated_at, agent_kind, workspace_kind) VALUES (?1, 'Live', 1, 1, 'pi', 'project')", [uuid(99)]).unwrap();
    assert!(std::fs::metadata(fx.profile.join(format!("cindy-{USER_ID}.db-wal"))).unwrap().len() > 0, "夹具里有未合并的 WAL");
    let live_hash = hash_tree(&fx.profile, true);

    // 钥匙串被拒：配置照样导入，key 记为缺失。回答：导入、关掉结果框。
    let data = work.join("data-a");
    let rig = start(&e, &data);
    let ports = FakePorts::new(rig.db.clone(), rig.models.clone(), "zh-CN", true, &[1, 0]);
    let imp = DesktopImport::new(data.clone(), ports.clone());
    imp.prompt_once().unwrap();
    {
        let d = ports.dialogs.lock().unwrap();
        assert_eq!(d.len(), 2, "{d:?}");
        assert_eq!(d[0]["title"], "从 Cindy 桌面版导入");
        assert_eq!(d[0]["buttons"].as_array().unwrap().len(), 3);
        assert!(d[0]["message"].as_str().unwrap().contains("4 个任务"), "WAL 里的任务也算: {}", d[0]["message"]);
        assert_eq!(d[1]["title"], "导入完成");
        assert!(d[1]["message"].as_str().unwrap().contains("4 个任务"), "{}", d[1]["message"]);
        assert!(d[1]["detail"].as_str().unwrap().contains("API key"), "说明哪些 key 没导入: {}", d[1]["detail"]);
    }
    let list = rig.ipc("maker:provider:list", json!([])).unwrap();
    let keyed = list["providers"].as_array().unwrap().iter().find(|p| p["id"] == "keyed").unwrap().clone();
    assert_eq!(keyed["connected"], false, "钥匙串被拒：配置导入，未连接");
    assert_eq!(local_db(&data).query_row("SELECT COUNT(*) FROM sessions WHERE id = ?1", [uuid(99)], |r| r.get::<_, i64>(0)).unwrap(), 1, "只在 WAL 里的任务被导入");
    assert_eq!(hash_tree(&fx.profile, true), live_hash, "运行中的桌面版数据不变");
    // 导入过一次后不再问
    let before = ports.dialogs.lock().unwrap().len();
    imp.prompt_once().unwrap();
    assert_eq!(ports.dialogs.lock().unwrap().len(), before, "导入后不再询问");
    rig.stop();

    // “不再提醒”：写 dismissed，不导入；英文界面用英文文案
    let data_b = work.join("data-b");
    let rig = start(&e, &data_b);
    let ports = FakePorts::new(rig.db.clone(), rig.models.clone(), "en", false, &[2]);
    let imp = DesktopImport::new(data_b.clone(), ports.clone());
    imp.prompt_once().unwrap();
    assert_eq!(ports.dialogs.lock().unwrap()[0]["title"], "Import from Cindy desktop");
    let marker = imp.invoke("desktop-import:scan", &[]).unwrap()["marker"].clone();
    assert_eq!(marker["status"], "dismissed", "{marker}");
    assert_eq!(local_db(&data_b).query_row("SELECT COUNT(*) FROM sessions", [], |r| r.get::<_, i64>(0)).unwrap(), 0);
    imp.prompt_once().unwrap();
    assert_eq!(ports.dialogs.lock().unwrap().len(), 1, "dismissed 之后不再问");
    rig.stop();

    // “以后再说”：什么都不写，下次还会问
    let data_c = work.join("data-c");
    let rig = start(&e, &data_c);
    let ports = FakePorts::new(rig.db.clone(), rig.models.clone(), "zh-TW", false, &[0]);
    let imp = DesktopImport::new(data_c.clone(), ports.clone());
    imp.prompt_once().unwrap();
    assert_eq!(ports.dialogs.lock().unwrap()[0]["title"], "從 Cindy 桌面版匯入");
    assert!(imp.invoke("desktop-import:scan", &[]).unwrap()["marker"].is_null(), "以后再说不留标记");
    rig.stop();
    drop(writer);
    let _ = std::fs::remove_dir_all(&work);
}
