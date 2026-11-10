// 权限门：对应 cindy-bridge 的 tool_call 拦截（Ask / Bypass / ReviewOnly）。
// 权限档每次从 CINDY_PI_PERMISSION_FILE 现读；读不到按 ask（fail-closed）。
// 询问走扩展 UI 子协议：input 请求（title=cindy:permission，payload 放 placeholder），
// 宿主回 extension_ui_response，value=="allow" 才放行。
// 凭证证据见 cred.rs。未移植：MCP 网关调用的真实身份。
use crate::agent::Shared;
use crate::util::emit_line;
use serde_json::{json, Value};
use std::path::{Component, Path, PathBuf};

const READONLY: [&str; 4] = ["read", "grep", "find", "ls"];
const FILE_WRITE: [&str; 2] = ["edit", "write"];

struct Perm { bypass: bool, review_only: bool, writable_roots: Vec<String>, review_read: Vec<String> }

fn load() -> Perm {
    let by_start = std::env::var("CINDY_PI_REVIEW_ONLY").as_deref() == Ok("1");
    let parsed = std::env::var("CINDY_PI_PERMISSION_FILE").ok().filter(|f| !f.is_empty())
        .and_then(|f| std::fs::read_to_string(f).ok()).and_then(|t| serde_json::from_str::<Value>(&t).ok());
    let strs = |v: &Value| v.as_array().map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect()).unwrap_or_default();
    match parsed {
        Some(p) => Perm { bypass: p["mode"] == "bypassPermissions", review_only: p["reviewOnly"] == true || by_start,
            writable_roots: strs(&p["writableRoots"]), review_read: strs(&p["reviewReadPaths"]) },
        None => Perm { bypass: false, review_only: by_start, writable_roots: vec![], review_read: vec![] },
    }
}

fn normalize(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::ParentDir => { out.pop(); }
            Component::CurDir => {}
            o => out.push(o.as_os_str()),
        }
    }
    out
}

/// 目标路径的真实位置：存在则 canonicalize，否则 canonicalize 最近的已存在父目录再拼回去（解析父链上的 symlink）。
fn resolve_target(cwd: &str, p: &str) -> PathBuf {
    let p = p.strip_prefix('@').unwrap_or(p);
    let full = normalize(&if Path::new(p).is_absolute() { PathBuf::from(p) } else { Path::new(cwd).join(p) });
    let mut tail = vec![];
    let mut cur = full.clone();
    loop {
        if let Ok(real) = cur.canonicalize() {
            return tail.iter().rev().fold(real, |a, t: &std::ffi::OsString| a.join(t));
        }
        match (cur.file_name().map(|n| n.to_os_string()), cur.parent().map(|p| p.to_path_buf())) {
            (Some(n), Some(par)) => { tail.push(n); cur = par; }
            _ => return full,
        }
    }
}

fn inside(p: &Path, root: &str) -> bool {
    let r = Path::new(root).canonicalize().unwrap_or_else(|_| normalize(Path::new(root)));
    p.starts_with(&r) || p.starts_with(normalize(Path::new(root)))
}

/// 向宿主发一个对话框请求并等待回复；None 表示被取消。
async fn ask(sh: &Shared, mut req: Value) -> Option<Value> {
    let id = uuid::Uuid::new_v4().to_string();
    let (tx, rx) = tokio::sync::oneshot::channel();
    sh.ui.lock().unwrap().insert(id.clone(), tx);
    req["type"] = "extension_ui_request".into();
    req["id"] = id.into();
    emit_line(&req);
    rx.await.ok()
}

fn denial(decision: Option<&str>) -> String {
    let src = decision.and_then(|d| d.split(':').next());
    let label = match src {
        Some("auto-review-deny") => "Cindy Auto-review denied this tool call",
        Some("user-deny") => "User denied this tool call via Cindy",
        _ => "Cindy could not approve this tool call",
    };
    let known = matches!(src, Some("auto-review-deny" | "user-deny" | "system-deny"));
    match decision.and_then(|d| d.split_once(':')).filter(|_| known) {
        Some((_, d)) if !d.trim().is_empty() => format!("{label}: {}", d.trim().chars().take(240).collect::<String>()),
        _ => format!("{label}."),
    }
}

/// Ok(()) 放行；Err(原因) 拦截，原因作为工具错误结果返回给模型。
pub async fn gate(sh: &Shared, name: &str, args: &Value, call_id: &str) -> Result<(), String> {
    if name == "ask_user_question" { return Ok(()); }
    let perm = load();
    let cwd = &sh.cfg.cwd;
    let target = args["path"].as_str().unwrap_or("");
    let is_write = FILE_WRITE.contains(&name);
    let is_ro = READONLY.contains(&name);
    if perm.review_only {
        let ok = is_ro && !crate::cred::evidence(name, args, cwd).0 && (target.is_empty() || perm.review_read.iter().any(|r| inside(&resolve_target(cwd, target), r)));
        return if ok { Ok(()) } else { Err("Cindy Review only permits read-only access to this task and its explicit artifacts.".into()) };
    }
    let resolved = (!target.is_empty() && is_write).then(|| resolve_target(cwd, target));
    // 运行时目录（models.json、权限档等）是控制面：即使 Full Access 也要用户确认。
    let control_plane = resolved.as_ref().map_or(false, |r| std::env::var("PI_CODING_AGENT_DIR").ok().filter(|d| !d.is_empty()).map_or(false, |d| inside(r, &d)));
    if is_write {
        // 内部同步点：宿主在写入前给目标文件做快照；结果忽略，失败不影响工具执行。
        let _ = ask(sh, json!({"method":"confirm","title":"cindy:turn-change-capture",
            "message": json!({"toolName":name,"toolUseId":call_id,"input":args}).to_string()})).await;
    }
    if perm.bypass && !control_plane { return Ok(()); }
    // 读凭证（含经 symlink 指向凭证）时只读工具也要审批；触及但解析不到真实路径时上报 null（无法核实）。
    let (cred_read, cred_paths) = crate::cred::evidence(name, args, cwd);
    if is_ro && !cred_read { return Ok(()); }
    let cred_value = if cred_read && cred_paths.is_empty() { Value::Null } else { Value::from(cred_paths) };
    let mut payload = json!({"toolName":name,"input":args,"resolvedCredentialPaths":cred_value});
    if is_write {
        let roots: Vec<String> = std::iter::once(cwd.clone()).chain(perm.writable_roots.iter().cloned()).collect();
        payload["resolvedWritePath"] = resolved.map(|p| Value::from(p.to_string_lossy().into_owned())).unwrap_or(Value::Null);
        payload["resolvedWritableRoots"] = roots.into();
        if control_plane { payload["controlPlaneWrite"] = true.into(); }
    }
    let resp = ask(sh, json!({"method":"input","title":"cindy:permission","placeholder":payload.to_string()})).await;
    let decision = resp.as_ref().and_then(|r| (r["cancelled"] != true).then(|| r["value"].as_str().map(String::from)).flatten());
    if decision.as_deref() == Some("allow") { Ok(()) } else { Err(denial(decision.as_deref())) }
}

/// ask_user_question：逐题向宿主发 select / input 请求，结果为 {answers, cancelled} 的 JSON 文本（与 bridge 一致）。
pub async fn ask_user_question(sh: &Shared, args: &Value) -> (String, bool) {
    let bad = || ("Provide one to three distinct questions with optional distinct answer choices.".to_string(), true);
    let Some(qs) = args["questions"].as_array().filter(|q| (1..=3).contains(&q.len())) else { return bad() };
    let mut seen = std::collections::HashSet::new();
    for q in qs {
        let Some(t) = q["question"].as_str().filter(|t| !t.trim().is_empty() && t.chars().count() <= 512) else { return bad() };
        if !seen.insert(t.to_string()) { return bad(); }
        if let Some(o) = q.get("options").filter(|o| !o.is_null()) {
            let Some(o) = o.as_array().filter(|o| (2..=6).contains(&o.len())) else { return bad() };
            let mut u = std::collections::HashSet::new();
            for v in o { match v.as_str().filter(|v| !v.trim().is_empty() && v.chars().count() <= 256) { Some(v) if u.insert(v.to_string()) => {}, _ => return bad() } }
        }
    }
    let (mut answers, mut cancelled) = (serde_json::Map::new(), false);
    for q in qs {
        let title = q["question"].as_str().unwrap_or("");
        let req = if q["options"].is_array() { json!({"method":"select","title":title,"options":q["options"]}) } else { json!({"method":"input","title":title}) };
        let r = ask(sh, req).await;
        let a = r.as_ref().and_then(|r| (r["cancelled"] != true).then(|| r["value"].as_str().map(String::from)).flatten()).filter(|a| !a.trim().is_empty());
        match a { Some(a) => { answers.insert(title.to_string(), a.into()); } None => { cancelled = true; break; } }
    }
    (json!({"answers": answers, "cancelled": cancelled}).to_string(), false)
}
