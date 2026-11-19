//! 桌面版导入的入口：desktop-import:scan / run / dismiss 三个 IPC，以及首次启动的询问。
//! 对应 sidecar/src/desktop-import.mjs 的 registerDesktopImportIpc。数据读写在 import_db / import_sessions。

use crate::import_db::{scan_profiles, snapshot_db, remove_db, Candidate};
use crate::import_sessions::{import_providers, import_sessions, Report};
use rusqlite::{Connection, OpenFlags};
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

const MARKER: &str = "desktop-import.json";
const STRINGS: &str = include_str!("import_strings.json");

/// 导入需要的外部能力。生产实现在 native.rs，测试用假实现。
pub trait ImportPorts: Send + Sync + 'static {
    /// 调 sidecar 的 host.* 方法（模型服务配置 / secret store / 本地库文件名仍在 sidecar）。
    fn sidecar(&self, method: &str, params: Value) -> Result<Value, String>;
    /// 向系统要 safeStorage 密钥（macOS 会弹钥匙串授权框）。
    fn safe_storage_key(&self, request: &Value) -> Result<Value, String>;
    /// 原生消息框，返回按下的按钮下标。
    fn dialog(&self, params: Value) -> Result<i64, String>;
    /// 向 renderer 推事件。
    fn emit(&self, channel: &str, arg: Value);
    fn locale(&self) -> String;
}

pub struct DesktopImport {
    data_dir: PathBuf,
    ports: Arc<dyn ImportPorts>,
    running: Mutex<bool>,
    locale_seen: (Mutex<bool>, Condvar),
}

pub const CHANNELS: [&str; 3] = ["desktop-import:scan", "desktop-import:run", "desktop-import:dismiss"];

fn platform() -> &'static str {
    if cfg!(target_os = "macos") { "darwin" } else if cfg!(windows) { "win32" } else { "linux" }
}

fn now_iso() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

fn fmt(template: &str, vars: &[(&str, String)]) -> String {
    let mut out = template.to_string();
    for (k, v) in vars {
        out = out.replace(&format!("{{{k}}}"), v);
    }
    out
}

impl DesktopImport {
    pub fn new(data_dir: PathBuf, ports: Arc<dyn ImportPorts>) -> Arc<Self> {
        Arc::new(DesktopImport { data_dir, ports, running: Mutex::new(false), locale_seen: (Mutex::new(false), Condvar::new()) })
    }

    pub fn owns(channel: &str) -> bool {
        CHANNELS.contains(&channel)
    }

    fn import_dir(&self) -> PathBuf {
        self.data_dir.join("import")
    }

    fn s(&self, key: &str, vars: &[(&str, String)]) -> String {
        let table: Value = serde_json::from_str(STRINGS).unwrap_or(Value::Null);
        let loc = self.ports.locale();
        let pick = if table.get(&loc).is_some() { loc.as_str() } else if loc.starts_with("zh") { "zh-CN" } else { "en" };
        let text = table[pick][key].as_str().or_else(|| table["en"][key].as_str()).unwrap_or("");
        fmt(text, vars)
    }

    // ── marker ──

    fn read_marker(&self) -> Option<Value> {
        serde_json::from_str(&std::fs::read_to_string(self.import_dir().join(MARKER)).ok()?).ok()
    }

    fn write_marker(&self, body: &Value) -> Result<(), String> {
        let dir = self.import_dir();
        std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
        let file = dir.join(MARKER);
        let tmp = dir.join(format!("{MARKER}.tmp"));
        std::fs::write(&tmp, serde_json::to_string_pretty(body).unwrap()).map_err(|e| e.to_string())?;
        std::fs::rename(&tmp, &file).map_err(|e| e.to_string())
    }

    // ── IPC ──

    fn scan(&self) -> Vec<Candidate> {
        scan_profiles(&self.import_dir())
    }

    fn resolve(&self, req: &Value) -> Result<Candidate, String> {
        let list = self.scan();
        if req.is_null() {
            return list.into_iter().next().ok_or_else(|| "[NOT_FOUND] no Cindy desktop data found".to_string());
        }
        let (Some(p), Some(f)) = (req.get("profileDir").and_then(Value::as_str), req.get("dbFile").and_then(Value::as_str)) else {
            return Err("[INVALID_PARAMS] profileDir and dbFile are required".into());
        };
        // 只接受扫描出来的候选，不让 renderer 指定任意路径。
        list.into_iter().find(|c| c.profile_dir.to_string_lossy() == p && c.db_file == f).ok_or_else(|| "[NOT_FOUND] unknown desktop profile".to_string())
    }

    pub fn invoke(&self, channel: &str, args: &[Value]) -> Result<Value, String> {
        match channel {
            "desktop-import:scan" => Ok(json!({
                "platform": platform(),
                "candidates": self.scan().iter().map(Candidate::to_json).collect::<Vec<_>>(),
                "marker": self.read_marker(),
            })),
            "desktop-import:run" => {
                let cand = self.resolve(args.first().unwrap_or(&Value::Null))?;
                self.start(&cand)
            }
            "desktop-import:dismiss" => {
                self.write_marker(&json!({ "status": "dismissed", "at": now_iso() }))?;
                Ok(json!({ "ok": true }))
            }
            other => Err(format!("[NOT_IMPLEMENTED] {other}")),
        }
    }

    fn start(&self, cand: &Candidate) -> Result<Value, String> {
        {
            let mut r = self.running.lock().unwrap();
            if *r {
                return Err("[BUSY] an import is already running".into());
            }
            *r = true;
        }
        let out = self.run_import(cand);
        *self.running.lock().unwrap() = false;
        out
    }

    fn run_import(&self, cand: &Candidate) -> Result<Value, String> {
        let mut report = Report::default();
        // 先等本机 secret store 就绪，否则 key 无处可写（不可用时只导入配置）。
        let secrets_ready = self.ports.sidecar("host.secrets-ready", json!({})).ok().and_then(|v| v.as_bool()).unwrap_or(false);
        let dir = self.import_dir();
        std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
        let snap_path = dir.join(format!("snapshot-{}-{}.db", std::process::id(), chrono::Utc::now().timestamp_millis()));
        let result = (|| -> Result<Vec<String>, String> {
            snapshot_db(&cand.profile_dir.join(&cand.db_file), &snap_path)?;
            let snap = Connection::open_with_flags(&snap_path, OpenFlags::SQLITE_OPEN_READ_ONLY).map_err(|e| e.to_string())?;
            let db_file = self.ports.sidecar("host.local-db-file", json!({}))?;
            let db_file = PathBuf::from(db_file.as_str().ok_or("[PRECONDITION_FAILED] local database is not ready")?);
            let imported = import_sessions(&self.data_dir, &db_file, &snap_path, &snap, cand, &mut report)?;
            import_providers(&snap, cand, self.ports.as_ref(), secrets_ready, &mut report)?;
            Ok(imported)
        })();
        remove_db(&snap_path);
        let imported = result?;
        for id in &imported {
            self.ports.emit("local-db:sessions:created", json!({ "sessionId": id }));
        }
        let out = report.to_json(cand);
        self.write_marker(&json!({
            "status": "imported", "at": now_iso(), "source": out["source"],
            "sessions": report.sessions_imported, "providers": report.providers_imported,
        }))?;
        Ok(out)
    }

    // ── 首次启动询问 ──

    /// 界面语言同步过来了：对话框才有语言可用。
    pub fn locale_changed(&self) {
        let (m, cv) = &self.locale_seen;
        *m.lock().unwrap() = true;
        cv.notify_all();
    }

    /// 开一个线程：等 renderer 同步语言（最多 30 秒），再等首屏渲染 3 秒，然后问一次。
    pub fn spawn_prompt(self: &Arc<Self>) {
        let me = self.clone();
        std::thread::Builder::new()
            .name("desktop-import-prompt".into())
            .spawn(move || {
                {
                    let (m, cv) = &me.locale_seen;
                    let g = m.lock().unwrap();
                    let _ = cv.wait_timeout_while(g, Duration::from_secs(30), |seen| !*seen);
                }
                std::thread::sleep(Duration::from_secs(3));
                if let Err(e) = me.prompt_once() {
                    eprintln!("[desktop-import] prompt failed: {e}");
                }
            })
            .ok();
    }

    pub fn prompt_once(&self) -> Result<(), String> {
        if self.read_marker().is_some() {
            return Ok(());
        }
        let Some(cand) = self.scan().into_iter().find(|c| c.sessions > 0 || c.providers > 0) else { return Ok(()) };
        let index = self
            .ports
            .dialog(json!({
                "kind": "info",
                "title": self.s("title", &[]),
                "message": self.s("message", &[("sessions", cand.sessions.to_string()), ("providers", cand.providers.to_string())]),
                "detail": self.s("detail", &[]),
                "buttons": [self.s("later", &[]), self.s("import", &[]), self.s("never", &[])],
            }))
            .unwrap_or(0);
        if index == 2 {
            return self.write_marker(&json!({ "status": "dismissed", "at": now_iso() }));
        }
        if index != 1 {
            return Ok(());
        }
        match self.start(&cand) {
            Ok(r) => {
                let n = |v: &Value| v.as_array().map(|a| a.len()).unwrap_or(0);
                let mut notes = Vec::new();
                if n(&r["keys"]["missing"]) > 0 {
                    notes.push(self.s("keyMissing", &[("n", n(&r["keys"]["missing"]).to_string())]));
                }
                if n(&r["workdirs"]["notCopied"]) > 0 {
                    notes.push(self.s("workdirs", &[("n", n(&r["workdirs"]["notCopied"]).to_string())]));
                }
                if n(&r["providers"]["skipped"]) > 0 {
                    notes.push(self.s("providerSkipped", &[("n", n(&r["providers"]["skipped"]).to_string())]));
                }
                let detail = if notes.is_empty() { String::new() } else { format!("{}\n{}", self.s("notes", &[]), notes.join("\n")) };
                let _ = self.ports.dialog(json!({
                    "kind": "info", "title": self.s("doneTitle", &[]),
                    "message": self.s("done", &[("sessions", r["sessions"]["imported"].to_string()), ("messages", r["messages"].to_string()), ("providers", r["providers"]["imported"].to_string())]),
                    "detail": detail,
                    "buttons": [self.s("ok", &[])],
                }));
            }
            Err(e) => {
                eprintln!("[desktop-import] failed: {e}");
                let _ = self.ports.dialog(json!({
                    "kind": "error", "title": self.s("failedTitle", &[]),
                    "message": self.s("failed", &[("error", e.chars().take(300).collect::<String>())]),
                    "buttons": [self.s("ok", &[])],
                }));
            }
        }
        Ok(())
    }
}
