//! 桌面版导入的数据部分：任务（含消息 / 页签 / 工作区 / Pi 会话文件）和自定义模型服务。
//! 对应 sidecar/src/desktop-import.mjs 的 importSessions / importProviders。
//! 本地库归 Rust：这里直接开一个 rusqlite 连接写同一个库文件。模型服务配置和 API key 归 sidecar（providers.json 与
//! secret store），经 `ImportPorts::sidecar` 的 host.* 回调落地，auth / secret-store 迁完后改为本地写。

use crate::import_crypto::Decryptor;
use crate::import_db::{columns_of, copy_tree, dir_size_within, importable_where, owner_storage_key, quote, relocate_dialogue, table_exists, Candidate};
use crate::import_run::ImportPorts;
use regex::Regex;
use rusqlite::{backup::Backup, types::Value as Sql, Connection};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// 单个 dialogue 工作区超过这个大小就不复制（新建空目录，在报告里列出原路径）。
const MAX_WORKDIR_COPY_BYTES: u64 = 1024 * 1024 * 1024;

#[derive(Default)]
pub struct Report {
    pub sessions_imported: i64,
    pub sessions_existing: i64,
    pub messages: i64,
    pub tabs: i64,
    pub recent_workdirs: Option<i64>,
    pub project_aliases: Option<i64>,
    pub providers_imported: i64,
    pub providers_exists: i64,
    pub providers_skipped: Vec<(String, String)>,
    pub keys_imported: i64,
    pub keys_missing: Vec<String>,
    pub keys_unavailable: Option<String>,
    pub workdirs_copied: i64,
    pub workdirs_not_copied: Vec<(String, String)>,
    pub pi_sessions: BTreeMap<String, i64>,
    pub backup: Option<String>,
}

impl Report {
    pub fn to_json(&self, cand: &Candidate) -> Value {
        let mut v = json!({
            "ok": true,
            "source": { "profileDir": cand.profile_dir.to_string_lossy(), "dbFile": cand.db_file },
            "sessions": { "imported": self.sessions_imported, "existing": self.sessions_existing },
            "messages": self.messages,
            "tabs": self.tabs,
            "providers": {
                "imported": self.providers_imported,
                "exists": self.providers_exists,
                "skipped": self.providers_skipped.iter().map(|(id, reason)| json!({ "id": id, "reason": reason })).collect::<Vec<_>>(),
            },
            "keys": { "imported": self.keys_imported, "missing": self.keys_missing, "unavailable": self.keys_unavailable },
            "workdirs": {
                "copied": self.workdirs_copied,
                "notCopied": self.workdirs_not_copied.iter().map(|(id, p)| json!({ "sessionId": id, "path": p })).collect::<Vec<_>>(),
            },
            "piSessions": self.pi_sessions,
            "backup": self.backup,
        });
        if let Some(n) = self.recent_workdirs {
            v["recent_workdirs"] = json!(n);
        }
        if let Some(n) = self.project_aliases {
            v["project_aliases"] = json!(n);
        }
        v
    }
}

fn now_ms() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0)
}

fn err<E: std::fmt::Display>(e: E) -> String {
    e.to_string()
}

// ── Pi 会话文件 ──

/// Pi 会话文件（sdk_session_id）在 profile 的 pi-agent-home/sessions 下：复制到本机 agentHome，并改写首行 cwd。
/// 返回（新路径, 状态）；状态是 copied / exists / external / missing。
pub fn copy_pi_session(profile_dir: &Path, data_dir: &Path, sdk_path: Option<&str>, new_cwd: Option<&Path>) -> (Option<String>, &'static str) {
    let Some(sdk) = sdk_path.filter(|p| p.ends_with(".jsonl")) else { return (None, "missing") };
    let src_root = profile_dir.join("pi-agent-home").join("sessions");
    let sdk_p = Path::new(sdk);
    let Ok(rel) = sdk_p.strip_prefix(&src_root) else {
        // 不在桌面版目录里（例如用户自定义的 agent home）：只要文件还在就原样引用。
        return if sdk_p.exists() { (Some(sdk.to_string()), "external") } else { (None, "missing") };
    };
    if rel.as_os_str().is_empty() || rel.components().any(|c| c.as_os_str() == "..") {
        return if sdk_p.exists() { (Some(sdk.to_string()), "external") } else { (None, "missing") };
    }
    if !sdk_p.exists() {
        return (None, "missing");
    }
    let dst = data_dir.join("pi-agent-home").join("sessions").join(rel);
    if dst.exists() {
        return (Some(dst.to_string_lossy().into_owned()), "exists");
    }
    let Ok(text) = std::fs::read_to_string(sdk_p) else { return (None, "missing") };
    let (head_line, rest) = match text.find('\n') {
        Some(i) => (&text[..i], &text[i..]),
        None => (text.as_str(), ""),
    };
    let mut out = text.clone();
    if let (Some(cwd), Ok(Value::Object(mut head))) = (new_cwd, serde_json::from_str::<Value>(head_line)) {
        if head.get("type").and_then(Value::as_str) == Some("session") && head.get("cwd").is_some_and(Value::is_string) {
            head.insert("cwd".into(), json!(cwd.to_string_lossy()));
            out = format!("{}{}", Value::Object(head), rest);
        }
    }
    let ok = (|| -> std::io::Result<()> {
        std::fs::create_dir_all(dst.parent().unwrap())?;
        let mut tmp = dst.as_os_str().to_owned();
        tmp.push(format!(".tmp-{}", std::process::id()));
        let tmp = PathBuf::from(tmp);
        std::fs::write(&tmp, out)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))?;
        }
        std::fs::rename(&tmp, &dst)
    })();
    match ok {
        Ok(()) => (Some(dst.to_string_lossy().into_owned()), "copied"),
        Err(e) => {
            eprintln!("[desktop-import] copy pi session failed: {e}");
            (None, "missing")
        }
    }
}

// ── 任务 ──

/// 把桌面版快照里的可导入任务写进本机库：先复制文件，再在一个事务里写库。返回新导入的任务 id。
pub fn import_sessions(data_dir: &Path, db_file: &Path, snap_path: &Path, snap: &Connection, cand: &Candidate, report: &mut Report) -> Result<Vec<String>, String> {
    if !db_file.exists() {
        return Err("[PRECONDITION_FAILED] local database is not ready".into());
    }
    let dst = Connection::open(db_file).map_err(err)?;
    dst.execute_batch("PRAGMA busy_timeout = 5000; PRAGMA foreign_keys = ON;").map_err(err)?;
    if !table_exists(&dst, "sessions") {
        return Err("[PRECONDITION_FAILED] local database is not ready".into());
    }
    let src_cols = columns_of(snap, "sessions", "main");
    let dst_cols = columns_of(&dst, "sessions", "main");
    let cols: Vec<String> = dst_cols.iter().filter(|c| src_cols.contains(c)).cloned().collect();
    // 目标列 NOT NULL 而源值为 NULL 时用列默认值（没有默认值的时间列用当前时间）。
    let mut info = std::collections::HashMap::new();
    {
        let mut st = dst.prepare("SELECT name, \"notnull\", dflt_value FROM pragma_table_info('sessions')").map_err(err)?;
        let rows = st.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?, r.get::<_, Option<String>>(2)?))).map_err(err)?;
        for r in rows.flatten() {
            info.insert(r.0, (r.1 != 0, r.2));
        }
    }
    let value_sql = |c: &str| -> String {
        match info.get(c) {
            Some((true, d)) => format!("COALESCE(?, {})", d.clone().unwrap_or_else(|| if c.ends_with("_at") { now_ms().to_string() } else { "''".into() })),
            _ => "?".into(),
        }
    };
    let idx = |name: &str| cols.iter().position(|c| c == name);
    let select = format!("SELECT {} FROM sessions WHERE {}", cols.iter().map(|c| quote(c)).collect::<Vec<_>>().join(", "), importable_where(&src_cols));
    let mut rows: Vec<Vec<Sql>> = {
        let mut st = snap.prepare(&select).map_err(err)?;
        let it = st.query_map([], |r| (0..cols.len()).map(|i| r.get::<_, Sql>(i)).collect::<Result<Vec<_>, _>>()).map_err(err)?;
        it.collect::<Result<_, _>>().map_err(err)?
    };
    let id_i = idx("id").ok_or("sessions has no id column")?;
    let text_at = |row: &Vec<Sql>, i: Option<usize>| -> Option<String> {
        match i.map(|i| &row[i]) {
            Some(Sql::Text(s)) => Some(s.clone()),
            _ => None,
        }
    };
    let exists = |id: &str| dst.query_row("SELECT 1 FROM sessions WHERE id = ?1", [id], |_| Ok(())).is_ok();
    let total = rows.len();
    rows.retain(|r| matches!(&r[id_i], Sql::Text(id) if !exists(id)));
    report.sessions_existing = (total - rows.len()) as i64;

    for row in rows.iter_mut() {
        let id = text_at(row, Some(id_i)).unwrap_or_default();
        let working_dir = text_at(row, idx("working_dir"));
        let is_dialogue = text_at(row, idx("workspace_kind")).as_deref() == Some("dialogue");
        let new_dir = if is_dialogue { working_dir.as_deref().and_then(|w| relocate_dialogue(&cand.profile_dir, data_dir, w)) } else { None };
        let mut moved: Option<PathBuf> = None;
        if let (Some(nd), Some(src)) = (new_dir, working_dir.as_deref()) {
            let src_p = Path::new(src);
            let size = if src_p.exists() { dir_size_within(src_p, MAX_WORKDIR_COPY_BYTES) } else { Some(0) };
            match size {
                // 太大不复制：任务继续用桌面版目录里的原文件（Pi 会话里的路径也都指向那里）。
                None => report.workdirs_not_copied.push((id.clone(), src.to_string())),
                Some(n) => {
                    std::fs::create_dir_all(&nd).map_err(err)?;
                    if n > 0 {
                        copy_tree(src_p, &nd).map_err(err)?;
                        report.workdirs_copied += 1;
                    }
                    if let Some(i) = idx("working_dir") {
                        row[i] = Sql::Text(nd.to_string_lossy().into_owned());
                    }
                    moved = Some(nd);
                }
            }
        }
        let sdk_old = text_at(row, idx("sdk_session_id"));
        let (sdk_new, status) = copy_pi_session(&cand.profile_dir, data_dir, sdk_old.as_deref(), moved.as_deref());
        if sdk_old.is_some() {
            *report.pi_sessions.entry(status.to_string()).or_insert(0) += 1;
        }
        if let Some(i) = idx("sdk_session_id") {
            row[i] = sdk_new.map(Sql::Text).unwrap_or(Sql::Null);
        }
        // 桌面版中断的回合不再续跑，避免导入后弹“任务被中断”。
        if let Some(i) = idx("active_turn_started_at") {
            row[i] = Sql::Null;
        }
    }
    if rows.is_empty() {
        return Ok(Vec::new());
    }

    // 本地库先整库备份。
    let backup_dir = data_dir.join("backups");
    std::fs::create_dir_all(&backup_dir).map_err(err)?;
    let stem = db_file.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    let backup_file = backup_dir.join(format!("{stem}.pre-import-{}.db", now_ms()));
    {
        let mut to = Connection::open(&backup_file).map_err(err)?;
        let b = Backup::new(&dst, &mut to).map_err(err)?;
        b.run_to_completion(256, Duration::from_millis(20), None).map_err(err)?;
    }
    report.backup = Some(backup_file.to_string_lossy().into_owned());

    dst.execute("ATTACH DATABASE ?1 AS src", [snap_path.to_string_lossy().as_ref()]).map_err(err)?;
    let result = (|| -> Result<Vec<String>, String> {
        dst.execute_batch("BEGIN IMMEDIATE").map_err(err)?;
        let inner = (|| -> Result<Vec<String>, String> {
            dst.execute_batch("CREATE TEMP TABLE IF NOT EXISTS import_ids (id TEXT PRIMARY KEY); DELETE FROM temp.import_ids;").map_err(err)?;
            let ins_sql = format!(
                "INSERT OR IGNORE INTO main.sessions ({}) VALUES ({})",
                cols.iter().map(|c| quote(c)).collect::<Vec<_>>().join(", "),
                cols.iter().map(|c| value_sql(c)).collect::<Vec<_>>().join(", ")
            );
            let mut ins = dst.prepare(&ins_sql).map_err(err)?;
            let mut mark = dst.prepare("INSERT OR IGNORE INTO temp.import_ids (id) VALUES (?1)").map_err(err)?;
            for row in &rows {
                if ins.execute(rusqlite::params_from_iter(row.iter())).map_err(err)? > 0 {
                    if let Sql::Text(id) = &row[id_i] {
                        mark.execute([id]).map_err(err)?;
                    }
                    report.sessions_imported += 1;
                }
            }
            let copy_table = |table: &str, scoped: bool| -> Result<Option<i64>, String> {
                if !table_exists(snap, table) {
                    return Ok(None);
                }
                let src = columns_of(&dst, table, "src");
                let tc: Vec<String> = columns_of(&dst, table, "main").into_iter().filter(|c| src.contains(c)).collect();
                let list = tc.iter().map(|c| quote(c)).collect::<Vec<_>>().join(", ");
                let filter = if scoped { " WHERE session_id IN (SELECT id FROM temp.import_ids)" } else { "" };
                let n = dst.execute(&format!("INSERT OR IGNORE INTO main.{table} ({list}) SELECT {list} FROM src.{table}{filter}"), []).map_err(err)?;
                Ok(Some(n as i64))
            };
            report.messages = copy_table("messages", true)?.unwrap_or(0);
            report.tabs = copy_table("right_sidebar_tabs", true)?.unwrap_or(0);
            // 最近工作目录 / 项目别名是锦上添花：列对不上就跳过（单条语句失败不影响事务里其它写入）。
            for table in ["recent_workdirs", "project_aliases"] {
                match copy_table(table, false) {
                    Ok(Some(n)) => {
                        if table == "recent_workdirs" { report.recent_workdirs = Some(n) } else { report.project_aliases = Some(n) }
                    }
                    Ok(None) => {}
                    Err(e) => eprintln!("[desktop-import] {table} skipped: {e}"),
                }
            }
            let mut st = dst.prepare("SELECT id FROM temp.import_ids").map_err(err)?;
            let ids = st.query_map([], |r| r.get::<_, String>(0)).map_err(err)?.flatten().collect();
            Ok(ids)
        })();
        match inner {
            Ok(ids) => {
                dst.execute_batch("COMMIT").map_err(err)?;
                Ok(ids)
            }
            Err(e) => {
                let _ = dst.execute_batch("ROLLBACK");
                Err(e)
            }
        }
    })();
    let _ = dst.execute_batch("DETACH DATABASE src");
    result
}

// ── 模型服务 ──

/// 向系统要 safeStorage 密钥的参数：macOS 只需钥匙串条目；
/// Windows 读 profile 的 Local State，把 os_crypt.encrypted_key 去掉 "DPAPI" 前缀后以 hex 交给 DPAPI 解包。
fn safe_storage_request(profile_dir: &Path) -> Result<Value, String> {
    if !cfg!(windows) {
        return Ok(json!({}));
    }
    use base64::Engine;
    let text = std::fs::read_to_string(profile_dir.join("Local State")).map_err(err)?;
    let state: Value = serde_json::from_str(&text).map_err(err)?;
    let b64 = state["os_crypt"]["encrypted_key"].as_str().filter(|s| !s.is_empty()).ok_or("[NOT_FOUND] Local State has no os_crypt key")?;
    let raw = base64::engine::general_purpose::STANDARD.decode(b64).map_err(err)?;
    if raw.len() < 5 || &raw[..5] != b"DPAPI" {
        return Err("[INVALID_STATE] unexpected os_crypt key format".into());
    }
    Ok(json!({ "encryptedKey": raw[5..].iter().map(|b| format!("{b:02x}")).collect::<String>() }))
}

struct ProviderIn {
    id: String,
    value: Value,
    uses_key: bool,
}

pub fn import_providers(snap: &Connection, cand: &Candidate, ports: &dyn ImportPorts, secrets_ready: bool, report: &mut Report) -> Result<(), String> {
    if !table_exists(snap, "custom_providers") {
        return Ok(());
    }
    let id_re = Regex::new(r"^[a-z0-9_-]{1,64}$").unwrap();
    let rows: Vec<(String, String, Option<String>, Option<String>)> = {
        let mut st = snap.prepare("SELECT id, name, runtimes, auth FROM custom_providers ORDER BY sort_order ASC, created_at ASC").map_err(err)?;
        let it = st.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, Option<String>>(2)?, r.get::<_, Option<String>>(3)?))).map_err(err)?;
        it.flatten().collect()
    };
    let mut inputs: Vec<ProviderIn> = Vec::new();
    for (id, name, runtimes, auth) in rows {
        let runtimes: Value = runtimes.and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_else(|| json!({}));
        let auth: Option<Value> = auth.filter(|a| !a.is_empty()).and_then(|t| serde_json::from_str(&t).ok());
        let pi = runtimes.get("pi").filter(|v| !v.is_null() && v.as_bool() != Some(false)).cloned();
        let (true, Some(pi)) = (id_re.is_match(&id), pi) else {
            report.providers_skipped.push((id, "no Pi runtime".into()));
            continue;
        };
        let uses_key = auth.as_ref().and_then(|a| a.get("method")).and_then(Value::as_str).unwrap_or("apiKey") == "apiKey";
        let mut v = json!({ "id": id, "name": name });
        if let Some(a) = auth.filter(|a| !a.is_null()) {
            v["auth"] = a;
        }
        v["runtimes"] = json!({ "pi": pi });
        inputs.push(ProviderIn { id, value: v, uses_key });
    }
    // 已经存在的 provider 不导入，也就不需要它的 key（避免无谓的钥匙串授权框）。
    let need_key: Vec<&ProviderIn> = inputs
        .iter()
        .filter(|p| p.uses_key && !ports.sidecar("host.has-provider", json!({ "id": p.id })).ok().and_then(|v| v.as_bool()).unwrap_or(false))
        .collect();
    let mut keys: BTreeMap<String, String> = BTreeMap::new();
    if !need_key.is_empty() && !secrets_ready {
        report.keys_unavailable = Some("local secure storage is unavailable".into());
    } else if !need_key.is_empty() {
        match read_provider_keys(cand, &need_key, ports) {
            Ok(k) => keys = k,
            Err(unavailable) => report.keys_unavailable = Some(unavailable),
        }
    }
    for p in &inputs {
        let key = if p.uses_key { keys.get(&p.id).cloned() } else { None };
        match ports.sidecar("host.import-provider", json!({ "input": p.value, "apiKey": key })) {
            Ok(Value::String(s)) if s == "exists" => report.providers_exists += 1,
            Ok(_) => {
                report.providers_imported += 1;
                if p.uses_key {
                    if key.is_some() { report.keys_imported += 1 } else { report.keys_missing.push(p.id.clone()) }
                }
            }
            Err(e) => report.providers_skipped.push((p.id.clone(), e.chars().take(200).collect())),
        }
    }
    Ok(())
}

/// 读各 provider 的 key 文件并解密。Err 里是「key 不可用」的原因（配置仍会导入）。
fn read_provider_keys(cand: &Candidate, providers: &[&ProviderIn], ports: &dyn ImportPorts) -> Result<BTreeMap<String, String>, String> {
    // 只有确实有 key 文件时才向系统要 safeStorage 密钥（macOS 会弹钥匙串授权框）。
    let dir = cand.profile_dir.join("safe-storage");
    let owner = owner_storage_key(&cand.owner_id);
    let mut files: Vec<(&str, PathBuf)> = Vec::new();
    for p in providers {
        let names = [format!("owner_{owner}_provider_key_{}_pi.enc", p.id), format!("provider_key_{}_pi.enc", p.id)];
        if let Some(hit) = names.iter().map(|n| dir.join(n)).find(|f| f.exists()) {
            files.push((p.id.as_str(), hit));
        }
    }
    let mut keys = BTreeMap::new();
    if files.is_empty() {
        return Ok(keys);
    }
    let material = safe_storage_request(&cand.profile_dir).and_then(|req| ports.safe_storage_key(&req))?;
    let decryptor = Decryptor::from_material(&material).ok_or("unsupported safe storage key")?;
    for (id, file) in files {
        if let Some(plain) = std::fs::read_to_string(file).ok().and_then(|t| decryptor.decrypt_enc_text(&t)) {
            keys.insert(id.to_string(), plain);
        }
    }
    Ok(keys)
}
