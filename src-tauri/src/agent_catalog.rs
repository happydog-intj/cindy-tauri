//! maker-core 的静态查询：能力表、斜杠命令、skills、agent 状态、可用 agent 列表。
//! 原来由 sidecar 里的 `@cindy/maker-core`（PiAgent）回答；本机只有 Pi 一个引擎，这些都是固定值或文件系统扫描，
//! 在这里重写后 sidecar 不再需要加载 maker-core。skills 的扫描规则对齐上游
//! packages/maker-core/src/agents/{pi,shared}/customization-scanner.ts，之后上游改了要手动同步。

use serde_json::{json, Map, Value};
use std::fs;
use std::path::{Path, PathBuf};

const SKILL_PRUNED_DIRS: [&str; 8] = ["node_modules", "dist", "build", "out", "coverage", "target", "__macosx", "__pycache__"];

fn fail<T>(code: &str, msg: String) -> Result<T, String> {
    Err(format!("[{code}] {msg}"))
}

/// Pi 的能力表（不含 availableModels，那个来自已连接的 provider，由 sidecar 经 `host.pi-models` 提供）。
pub fn pi_capabilities() -> Value {
    let ok = || json!({ "supported": true });
    json!({
        "switchModel": ok(),
        "hasFastMode": true,
        "effort": ok(),
        "effortLevels": [
            { "id": "minimal", "displayName": "Minimal" }, { "id": "low", "displayName": "Low" },
            { "id": "medium", "displayName": "Medium" }, { "id": "high", "displayName": "High" },
            { "id": "xhigh", "displayName": "Extra High" }, { "id": "max", "displayName": "Maximum" },
        ],
        "reasoningDisplay": ["off", "full"],
        "permissionModes": [
            { "id": "ask", "displayName": "Default permissions", "description": "Read-only tools run directly; writing files, running commands, and MCP tools ask each time." },
            { "id": "auto", "displayName": "Auto-review", "description": "In-workspace writes and safe commands run automatically; out-of-workspace writes, risky commands, and MCP tools still ask." },
            { "id": "bypassPermissions", "displayName": "Full access", "description": "Allows editing any file and running networked commands without another confirmation. Highest risk; use only for trusted tasks." },
        ],
        "setPermissionModeMidSession": ok(),
        "turnPermissionPolicy": { "supported": ok(), "unsupportedPermissionModes": ["bypassPermissions"] },
        "planMode": ok(),
        "multimodal": { "text": ok(), "image": ok(), "file": ok() },
        "fork": ok(), "rewind": ok(), "sessionTree": ok(), "abort": ok(), "sameTurnSteer": ok(),
        "memory": {
            "supported": ok(), "displayName": "Pi Auto Memory",
            "description": "Preserve compacted context as searchable Cindy memory.",
            "stage": "stable", "defaultEnabled": true, "resettable": true,
            "setEnabledMidSession": { "supported": false, "reason": "not-implemented", "message": "The updated Pi Auto Memory setting applies to new sessions." },
        },
        "extraDirs": ok(), "writableDirs": ok(), "sessionHtmlExport": ok(), "manualCompact": ok(), "runtimeCapabilities": ok(),
        "supportsSessionAgentSwitch": false,
        "supportsModelWindowSwitchGuard": false,
    })
}

/// claude-code / codex 在本机版里没有：返回「全不支持」的合法能力，否则渲染层整份快照作废。
pub fn unsupported_capabilities() -> Value {
    let no = || json!({ "supported": false, "reason": "not-implemented", "message": "Only Pi is available in the Tauri build" });
    json!({
        "switchModel": no(), "availableModels": [], "hasFastMode": false, "effort": no(), "effortLevels": [],
        "reasoningDisplay": ["off"], "permissionModes": [], "setPermissionModeMidSession": no(), "planMode": no(),
        "multimodal": { "text": no(), "image": no(), "file": no() },
        "fork": no(), "rewind": no(), "abort": no(), "sameTurnSteer": no(), "memory": { "supported": no() }, "extraDirs": no(),
        "supportsSessionAgentSwitch": false, "supportsModelWindowSwitchGuard": false,
    })
}

/// `maker:get-capabilities` 的结果；`models` 是已连接 provider 投影出的可选模型（为空时用空列表）。
pub fn capabilities(agent_kind: &Value, models: Vec<Value>) -> Result<Value, String> {
    match agent_kind.as_str() {
        Some("claude-code" | "codex") => Ok(unsupported_capabilities()),
        Some("pi") => {
            let mut c = pi_capabilities();
            c["availableModels"] = Value::Array(models);
            // 保持与原实现相同的键序：availableModels 紧跟 switchModel。
            let mut out = Map::new();
            let mut obj = c.as_object().cloned().unwrap_or_default();
            if let Some(v) = obj.shift_remove("switchModel") {
                out.insert("switchModel".into(), v);
            }
            if let Some(v) = obj.shift_remove("availableModels") {
                out.insert("availableModels".into(), v);
            }
            out.extend(obj);
            Ok(Value::Object(out))
        }
        _ => fail("INVALID_PARAMS", format!("unknown agentKind {}", describe(agent_kind))),
    }
}

fn describe(v: &Value) -> String {
    match v {
        Value::Null => "undefined".into(),
        Value::String(s) => s.clone(),
        o => o.to_string(),
    }
}

/// `maker:agent:status`：二进制就绪与否以随包 pi-rs 是否存在为准。
pub fn agent_status(agent_kind: &Value, pi_binary: Option<&Path>) -> Value {
    if agent_kind.as_str() != Some("pi") {
        return json!({ "binaryReady": false, "binaryPath": null, "authReady": false });
    }
    let ready = pi_binary.map(|p| p.is_file()).unwrap_or(false);
    // authReady：PiAgent.getAgentStatus 恒为 false（鉴权按会话的 provider 判断），identity 缺省。
    json!({ "binaryReady": ready, "binaryPath": if ready { json!(pi_binary.unwrap().to_string_lossy()) } else { Value::Null }, "authReady": false })
}

/// `maker:list-agent-commands`：Pi 没有内置的 palette 命令；其他引擎本机没有注册。
pub fn list_agent_commands(agent_kind: &Value) -> Value {
    match agent_kind.as_str() {
        Some("pi") => json!({ "success": true, "commands": [] }),
        _ => json!({ "success": false, "error": format!("Agent '{}' is not registered (available: pi)", describe(agent_kind)) }),
    }
}

// ── skills 扫描 ───────────────────────────────────────────────────────────────

struct Source {
    scope: &'static str,
    dir: PathBuf,
    working_dir: Option<String>,
    contain_within: Option<PathBuf>,
}

struct Found {
    scope: &'static str,
    name: String,
    description: Option<String>,
    abs_path: PathBuf,
    md_path: PathBuf,
    working_dir: Option<String>,
}

fn canonical(p: &Path) -> PathBuf {
    fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf())
}

fn has_git_marker(dir: &Path) -> bool {
    match fs::metadata(dir.join(".git")) {
        Ok(m) => m.is_dir() || m.is_file(),
        // ENOENT / ENOTDIR 才算没有；权限等其他错误按「有」处理（与上游一致）。
        Err(e) => !matches!(e.kind(), std::io::ErrorKind::NotFound) && e.raw_os_error() != Some(20),
    }
}

fn nearest_git_root(dir: &Path) -> Option<PathBuf> {
    let mut cur = canonical(dir);
    loop {
        if has_git_marker(&cur) {
            return Some(cur);
        }
        let parent = cur.parent()?.to_path_buf();
        cur = parent;
    }
}

fn skill_ancestors(dir: &Path) -> Vec<PathBuf> {
    let start = canonical(dir);
    let repo = nearest_git_root(&start);
    let mut out = Vec::new();
    let mut cur = start;
    loop {
        out.push(cur.clone());
        if repo.as_ref().map(|r| *r == cur).unwrap_or(true) {
            break;
        }
        match cur.parent() {
            Some(p) => cur = p.to_path_buf(),
            None => break,
        }
    }
    out
}

fn build_sources(home: &Path, working_dirs: &[String]) -> Vec<Source> {
    let mut sources = vec![Source { scope: "user", dir: home.join(".agents").join("skills"), working_dir: None, contain_within: None }];
    let mut seen = std::collections::HashSet::new();
    for input in working_dirs {
        let p = Path::new(input);
        if input.is_empty() || !p.is_absolute() || !p.is_dir() {
            continue;
        }
        let working = input.trim_end_matches('/').to_string();
        let scan_root = canonical(p);
        let boundary = nearest_git_root(&scan_root).unwrap_or_else(|| scan_root.clone());
        let mut add = |dir: PathBuf| {
            if seen.insert(format!("{working}\0{}", canonical(&dir).display())) {
                sources.push(Source { scope: "repo", dir, working_dir: Some(working.clone()), contain_within: Some(boundary.clone()) });
            }
        };
        add(scan_root.join(".pi").join("skills"));
        for a in skill_ancestors(&scan_root) {
            add(a.join(".agents").join("skills"));
        }
    }
    sources
}

fn is_inside(parent: &Path, child: &Path) -> bool {
    child.starts_with(parent)
}
fn real_inside(root: &Path, cand: &Path) -> bool {
    fs::canonicalize(cand).map(|c| is_inside(root, &c)).unwrap_or(false)
}

fn is_backup_name(name: &str) -> bool {
    // /\.bak\.\d+$/
    match name.rfind(".bak.") {
        Some(i) => {
            let tail = &name[i + 5..];
            !tail.is_empty() && tail.chars().all(|c| c.is_ascii_digit())
        }
        None => false,
    }
}

fn find_skill_md(folder: &Path) -> Option<PathBuf> {
    ["SKILL.md", "skill.md"].iter().map(|n| folder.join(n)).find(|c| fs::metadata(c).map(|m| m.is_file()).unwrap_or(false))
}

/// frontmatter 里的 description（trim 后截 500 个 UTF-16 单元）；没有 frontmatter 或解析失败返回 None。
fn frontmatter_description(raw: &str) -> Option<String> {
    let rest = raw.strip_prefix('\u{feff}').unwrap_or(raw);
    let rest = rest.strip_prefix("---")?;
    let rest = rest.strip_prefix("\r\n").or_else(|| rest.strip_prefix('\n'))?;
    let mut end = None;
    let mut pos = 0;
    for line in rest.split_inclusive('\n') {
        if line.trim_end_matches(['\r', '\n']).trim_end() == "---" {
            end = Some(pos);
            break;
        }
        pos += line.len();
    }
    let yaml = &rest[..end?];
    let parsed: serde_yaml::Value = serde_yaml::from_str(yaml).ok()?;
    let desc = parsed.get("description")?.as_str()?.trim();
    if desc.is_empty() {
        return None;
    }
    let units: Vec<u16> = desc.encode_utf16().take(500).collect();
    Some(String::from_utf16_lossy(&units))
}

fn read_dir_sorted(dir: &Path, errors: &mut Vec<Value>) -> Option<Vec<(String, PathBuf, fs::FileType)>> {
    match fs::read_dir(dir) {
        Ok(rd) => {
            let mut v: Vec<_> = rd.filter_map(|e| e.ok()).filter_map(|e| Some((e.file_name().to_string_lossy().to_string(), e.path(), e.file_type().ok()?))).collect();
            v.sort_by(|a, b| a.0.cmp(&b.0));
            Some(v)
        }
        Err(e) => {
            errors.push(json!({ "path": dir.to_string_lossy(), "message": e.to_string() }));
            None
        }
    }
}

fn scan_source(src: &Source, found: &mut Vec<Found>, errors: &mut Vec<Value>) {
    if !src.dir.is_dir() {
        return;
    }
    let mut contain = None;
    if let Some(w) = &src.contain_within {
        match fs::canonicalize(w) {
            Ok(c) => contain = Some(c),
            Err(_) => return,
        }
        if !real_inside(contain.as_ref().unwrap(), &src.dir) {
            return;
        }
    }
    let mut push = |folder: &Path, md: &Path| {
        if let Some(c) = &contain {
            if !real_inside(c, folder) || !real_inside(c, md) {
                return;
            }
        }
        found.push(Found {
            scope: src.scope,
            name: folder.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default(),
            description: fs::read_to_string(md).ok().and_then(|raw| frontmatter_description(&raw)),
            abs_path: folder.to_path_buf(),
            md_path: md.to_path_buf(),
            working_dir: src.working_dir.clone(),
        });
    };
    let Some(top) = read_dir_sorted(&src.dir, errors) else { return };
    for (name, folder, ft) in top {
        if name.starts_with('.') || is_backup_name(&name) {
            continue;
        }
        if let Some(md) = find_skill_md(&folder) {
            push(&folder, &md);
            continue;
        }
        let is_symlink = ft.is_symlink() || fs::symlink_metadata(&folder).map(|m| m.file_type().is_symlink()).unwrap_or(false);
        if is_symlink || !ft.is_dir() || SKILL_PRUNED_DIRS.contains(&name.to_lowercase().as_str()) {
            continue;
        }
        // 最多再下一层：<root>/<namespace>/<skill>。
        let Some(nested) = read_dir_sorted(&folder, errors) else { continue };
        for (n, nf, _) in nested {
            if n.starts_with('.') || is_backup_name(&n) {
                continue;
            }
            if let Some(md) = find_skill_md(&nf) {
                push(&nf, &md);
            }
        }
    }
}

/// JS `localeCompare`（ICU root 排序规则）的近似：标点 < 数字 < 字母，字母大小写不敏感、重音次之、小写先于大写，
/// 非拉丁字符按码点排在拉丁字母之后；数字逐字符比较（不是数值比较）。覆盖 skill 名字的实际用法，不是完整的 UCA。
pub fn locale_compare(a: &str, b: &str) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    const PUNCT_ORDER: &str = "_-,;:!?.'\"()[]{}@*/\\&#%`^+<=>|~$";
    fn base(c: char) -> (char, bool) {
        // (去重音后的小写字母, 是否带重音)
        let lower: char = c.to_lowercase().next().unwrap_or(c);
        let plain = match lower {
            'à' | 'á' | 'â' | 'ã' | 'ä' | 'å' | 'ā' => 'a',
            'ç' | 'ć' | 'č' => 'c',
            'è' | 'é' | 'ê' | 'ë' | 'ē' | 'ę' | 'ě' => 'e',
            'ì' | 'í' | 'î' | 'ï' | 'ī' => 'i',
            'ñ' | 'ń' | 'ň' => 'n',
            'ò' | 'ó' | 'ô' | 'õ' | 'ö' | 'ø' | 'ō' => 'o',
            'ù' | 'ú' | 'û' | 'ü' | 'ū' | 'ů' => 'u',
            'ý' | 'ÿ' => 'y',
            'š' | 'ś' => 's',
            'ž' | 'ź' | 'ż' => 'z',
            other => other,
        };
        (plain, plain != lower)
    }
    // 一级权重：(类别, 值)。
    fn primary(c: char) -> (u8, u32) {
        if c.is_whitespace() {
            return (0, 0);
        }
        if let Some(i) = PUNCT_ORDER.find(c) {
            return (1, i as u32);
        }
        if c.is_ascii_digit() {
            return (3, c as u32);
        }
        let (b, _) = base(c);
        if b.is_ascii_alphabetic() {
            return (4, b as u32);
        }
        if c.is_alphanumeric() {
            return (5, c as u32);
        }
        (2, c as u32)
    }
    let (ca, cb): (Vec<char>, Vec<char>) = (a.chars().collect(), b.chars().collect());
    let p: Ordering = ca.iter().map(|c| primary(*c)).cmp(cb.iter().map(|c| primary(*c)));
    if p != Ordering::Equal {
        return p;
    }
    // 二级：重音（无重音在前）。
    let sec = ca.iter().map(|c| base(*c).1).cmp(cb.iter().map(|c| base(*c).1));
    if sec != Ordering::Equal {
        return sec;
    }
    // 三级：小写在前。
    ca.iter().map(|c| c.is_uppercase()).cmp(cb.iter().map(|c| c.is_uppercase()))
}

fn path_depth(p: &Path) -> usize {
    p.to_string_lossy().split(['/', '\\']).count()
}

/// `maker:list-agent-skills` 里 Pi 的部分（不含 managed skills：本机没有 SkillHub 托管项）。
pub fn list_pi_skills(home: &Path, params: &Value) -> Value {
    let working: Vec<String> = params["workingDir"].as_str().filter(|s| !s.is_empty()).map(|s| vec![s.to_string()]).unwrap_or_default();
    let mut found = Vec::new();
    let mut errors = Vec::new();
    for src in build_sources(home, &working) {
        scan_source(&src, &mut found, &mut errors);
    }
    // 去重：同 scope + workingDir + 规范路径只留一条。
    let mut seen = std::collections::HashSet::new();
    found.retain(|f| seen.insert(format!("{}\0{}\0{}", f.scope, f.working_dir.clone().unwrap_or_default(), canonical(&f.abs_path).display())));
    // 排序：scope、名字、路径深度（直接项优先）、路径。
    found.sort_by(|a, b| {
        locale_compare(a.scope, b.scope)
            .then_with(|| locale_compare(&a.name, &b.name))
            .then_with(|| path_depth(&a.abs_path).cmp(&path_depth(&b.abs_path)))
            .then_with(|| locale_compare(&a.abs_path.to_string_lossy(), &b.abs_path.to_string_lossy()))
    });
    let mut skills: Vec<Value> = found
        .iter()
        .map(|f| {
            let mut s = json!({ "kind": "agent-skill", "name": f.name, "source": "skill", "path": f.md_path.to_string_lossy(), "scope": if f.scope == "repo" { "repo" } else { "user" }, "enabled": true, "runtimeCommandName": format!("skill:{}", f.name) });
            if let Some(d) = &f.description {
                s["description"] = json!(d);
            }
            if f.scope == "repo" {
                s["runtimeStatus"] = json!("discovered");
            }
            s
        })
        .collect();
    // 最终列表按名字排序（稳定，同名保持上面的顺序）。
    skills.sort_by(|a, b| locale_compare(a["name"].as_str().unwrap_or(""), b["name"].as_str().unwrap_or("")));
    let mut out = json!({ "success": true, "skills": skills });
    if !errors.is_empty() {
        out["errors"] = Value::Array(errors);
    }
    out
}

pub fn list_agent_skills(agent_kind: &Value, params: &Value, home: &Path) -> Value {
    match agent_kind.as_str() {
        Some("pi") => list_pi_skills(home, if params.is_object() { params } else { &Value::Null }),
        _ => json!({ "success": false, "error": format!("Agent '{}' is not registered (available: pi)", describe(agent_kind)) }),
    }
}

pub fn list_available_agents() -> Value {
    json!(["pi"])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("agent-catalog-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }
    fn skill(root: &Path, rel: &str, body: &str) {
        let d = root.join(rel);
        fs::create_dir_all(&d).unwrap();
        fs::write(d.join("SKILL.md"), body).unwrap();
    }
    fn names(v: &Value) -> Vec<String> {
        v["skills"].as_array().unwrap().iter().map(|s| s["name"].as_str().unwrap().to_string()).collect()
    }

    #[test]
    fn capabilities_match_the_pi_agent_table_and_put_models_after_switch_model() {
        let c = capabilities(&json!("pi"), vec![json!({ "id": "m1" })]).unwrap();
        let keys: Vec<&String> = c.as_object().unwrap().keys().collect();
        assert_eq!(&keys[..3], &["switchModel", "availableModels", "hasFastMode"]);
        assert_eq!(c["availableModels"], json!([{ "id": "m1" }]));
        assert_eq!(c["effortLevels"].as_array().unwrap().len(), 6);
        assert_eq!(c["permissionModes"][2]["id"], "bypassPermissions");
        assert_eq!(c["turnPermissionPolicy"]["unsupportedPermissionModes"], json!(["bypassPermissions"]));
        assert_eq!(c["supportsSessionAgentSwitch"], false);
        assert_eq!(c["memory"]["setEnabledMidSession"]["supported"], false);
        // 其他引擎：全不支持但形状合法；未知引擎报错。
        let n = capabilities(&json!("codex"), vec![]).unwrap();
        assert_eq!((n["switchModel"]["supported"].clone(), n["availableModels"].clone()), (json!(false), json!([])));
        assert!(capabilities(&json!("zzz"), vec![]).unwrap_err().contains("[INVALID_PARAMS] unknown agentKind zzz"));
        assert!(capabilities(&Value::Null, vec![]).unwrap_err().contains("unknown agentKind undefined"));
    }

    #[test]
    fn agent_status_commands_and_agent_list() {
        let exe = std::env::current_exe().unwrap();
        let s = agent_status(&json!("pi"), Some(&exe));
        assert_eq!((s["binaryReady"].clone(), s["authReady"].clone(), s["binaryPath"].clone()), (json!(true), json!(false), json!(exe.to_string_lossy())));
        let missing = agent_status(&json!("pi"), Some(Path::new("/nonexistent/pi")));
        assert_eq!((missing["binaryReady"].clone(), missing["binaryPath"].clone()), (json!(false), Value::Null));
        assert_eq!(agent_status(&json!("codex"), Some(&exe)), json!({ "binaryReady": false, "binaryPath": null, "authReady": false }));
        assert_eq!(list_agent_commands(&json!("pi")), json!({ "success": true, "commands": [] }));
        assert!(list_agent_commands(&json!("codex"))["error"].as_str().unwrap().contains("not registered"));
        assert_eq!(list_available_agents(), json!(["pi"]));
    }

    #[test]
    fn user_skills_are_scanned_with_frontmatter_descriptions_and_namespaces() {
        let home = tmp("user");
        let root = home.join(".agents/skills");
        skill(&root, "zeta", "---\nname: zeta\ndescription: \"  Quoted: desc  \"\n---\nbody");
        skill(&root, "alpha", "---\ndescription: >\n  folded\n  text\n---\n");
        skill(&root, "ns/inner", "---\ndescription: nested one\n---\n");
        skill(&root, "node_modules/hidden", "---\ndescription: pruned\n---\n");
        skill(&root, ".dot/skip", "---\ndescription: dot\n---\n");
        skill(&root, "old.bak.12", "---\ndescription: backup\n---\n");
        skill(&root, "ns/deeper/too-deep", "---\ndescription: too deep\n---\n");
        fs::create_dir_all(root.join("nomd")).unwrap();
        fs::write(root.join("nomd/readme.md"), "x").unwrap();
        fs::write(root.join("plain"), "file").unwrap();
        skill(&root, "nodesc", "no frontmatter at all");
        let out = list_pi_skills(&home, &json!({}));
        assert_eq!(names(&out), vec!["alpha", "inner", "nodesc", "zeta"]);
        let by = |n: &str| out["skills"].as_array().unwrap().iter().find(|s| s["name"] == n).unwrap().clone();
        assert_eq!(by("zeta")["description"], "Quoted: desc");
        assert_eq!(by("alpha")["description"], "folded text");
        assert!(by("nodesc").get("description").is_none());
        assert_eq!(by("zeta")["scope"], "user");
        assert_eq!(by("zeta")["runtimeCommandName"], "skill:zeta");
        assert!(by("zeta")["path"].as_str().unwrap().ends_with("zeta/SKILL.md"));
        assert!(by("zeta").get("runtimeStatus").is_none());
        assert!(out.get("errors").is_none());
        // 小写 skill.md 也认；描述按 500 个 UTF-16 单元截断。
        let long = "字".repeat(600);
        let d = root.join("lower");
        fs::create_dir_all(&d).unwrap();
        fs::write(d.join("skill.md"), format!("---\ndescription: {long}\n---\n")).unwrap();
        let out = list_pi_skills(&home, &json!({}));
        let lower = out["skills"].as_array().unwrap().iter().find(|s| s["name"] == "lower").unwrap().clone();
        assert_eq!(lower["description"].as_str().unwrap().chars().count(), 500);
    }

    #[test]
    fn project_skills_stop_at_the_git_root_and_are_marked_discovered() {
        let home = tmp("proj-home");
        skill(&home.join(".agents/skills"), "global", "---\ndescription: g\n---\n");
        let repo = tmp("proj-repo");
        fs::create_dir_all(repo.join(".git")).unwrap();
        skill(&repo.join(".agents/skills"), "root-skill", "---\ndescription: r\n---\n");
        let sub = repo.join("packages/app");
        // .pi/skills 只看工作目录本身（不向上找）；.agents/skills 才一路找到 Git 根。
        skill(&repo.join(".pi/skills"), "pi-at-root", "---\ndescription: p\n---\n");
        skill(&sub.join(".pi/skills"), "pi-skill", "---\ndescription: p\n---\n");
        skill(&sub.join(".agents/skills"), "app-skill", "---\ndescription: a\n---\n");
        // 仓库之外的上层目录不扫。
        skill(&repo.parent().unwrap().join(".agents/skills"), "outside", "---\ndescription: o\n---\n");
        let out = list_pi_skills(&home, &json!({ "workingDir": sub.to_string_lossy() }));
        assert_eq!(names(&out), vec!["app-skill", "global", "pi-skill", "root-skill"]);
        let by = |n: &str| out["skills"].as_array().unwrap().iter().find(|s| s["name"] == n).unwrap().clone();
        assert_eq!((by("app-skill")["scope"].clone(), by("app-skill")["runtimeStatus"].clone()), (json!("repo"), json!("discovered")));
        assert_eq!(by("global")["scope"], "user");
        // 相对路径、不存在的目录：只剩全局。
        assert_eq!(names(&list_pi_skills(&home, &json!({ "workingDir": "relative/dir" }))), vec!["global"]);
        assert_eq!(names(&list_pi_skills(&home, &json!({ "workingDir": "/nonexistent/dir" }))), vec!["global"]);
        let _ = fs::remove_dir_all(repo.parent().unwrap().join(".agents"));
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_cannot_escape_the_project_boundary_or_recurse() {
        let home = tmp("link-home");
        let repo = tmp("link-repo");
        fs::create_dir_all(repo.join(".git")).unwrap();
        let outside = tmp("link-outside");
        skill(&outside, "evil", "---\ndescription: escaped\n---\n");
        let skills = repo.join(".agents/skills");
        fs::create_dir_all(&skills).unwrap();
        std::os::unix::fs::symlink(outside.join("evil"), skills.join("evil")).unwrap();
        // 命名空间层是符号链接：不进入。
        std::os::unix::fs::symlink(&outside, skills.join("nsl")).unwrap();
        skill(&skills, "real", "---\ndescription: ok\n---\n");
        let out = list_pi_skills(&home, &json!({ "workingDir": repo.to_string_lossy() }));
        assert_eq!(names(&out), vec!["real"]);
        // 用户目录没有这个边界限制：指向外部的链接照常识别。
        let uroot = home.join(".agents/skills");
        fs::create_dir_all(&uroot).unwrap();
        std::os::unix::fs::symlink(outside.join("evil"), uroot.join("linked")).unwrap();
        assert_eq!(names(&list_pi_skills(&home, &json!({}))), vec!["linked"]);
    }

    #[test]
    fn unreadable_skill_roots_are_reported_as_errors_and_other_agents_are_rejected() {
        let out = list_agent_skills(&json!("codex"), &Value::Null, Path::new("/nonexistent"));
        assert_eq!(out["success"], false);
        assert!(out["error"].as_str().unwrap().contains("Agent 'codex' is not registered"));
        let ok = list_agent_skills(&json!("pi"), &json!("junk"), Path::new("/nonexistent-home"));
        assert_eq!(ok, json!({ "success": true, "skills": [] }));
    }

    #[test]
    fn skill_names_sort_like_javascript_locale_compare() {
        // 期望顺序来自 Node 对同一批目录的 maker:list-agent-skills 实测。
        let home = tmp("collate");
        let root = home.join(".agents/skills");
        for n in ["Zed", "alpha", "Beta", "beta2", "_under", "ns/Inner", "ns/alpha", "éclair", "中文", "a-b", "a_b", "a.b", "A1", "a10", "a2"] {
            skill(&root, n, "---\ndescription: d\n---\n");
        }
        assert_eq!(
            names(&list_pi_skills(&home, &json!({}))),
            vec!["_under", "a_b", "a-b", "a.b", "A1", "a10", "a2", "alpha", "alpha", "Beta", "beta2", "éclair", "Inner", "Zed", "中文"]
        );
        // 同名：直接项（路径更浅）在命名空间项之前。
        let alphas: Vec<String> = list_pi_skills(&home, &json!({}))["skills"].as_array().unwrap().iter().filter(|s| s["name"] == "alpha").map(|s| s["path"].as_str().unwrap().to_string()).collect();
        assert!(alphas[0].ends_with("skills/alpha/SKILL.md") && alphas[1].ends_with("ns/alpha/SKILL.md"), "{alphas:?}");
        assert_eq!(locale_compare("a", "A"), std::cmp::Ordering::Less);
        assert_eq!(locale_compare("e", "é"), std::cmp::Ordering::Less);
        assert_eq!(locale_compare("é", "f"), std::cmp::Ordering::Less);
    }
}
