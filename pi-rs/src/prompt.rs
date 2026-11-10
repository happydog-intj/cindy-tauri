// 系统消息：段落文本和工具定义取自原版 1.4.0 的录制结果（src/static），cwd 段按当前目录生成。
// --append-system-prompt 的内容（文本或文件路径）追加到系统提示末尾。
use serde_json::{json, Value};

pub fn tools_all() -> Vec<Value> {
    serde_json::from_str(include_str!("static/tools.json")).expect("tools.json")
}

/// 内置工具的默认启用集合（顺序即声明顺序）。
pub const DEFAULT_TOOLS: [&str; 4] = ["read", "bash", "edit", "write"];

/// 按 --tools 白名单选工具：顺序按白名单给出的顺序，未知名字忽略；None 表示默认的四个。
pub fn tools_enabled(allow: &Option<Vec<String>>) -> Vec<Value> {
    let all = tools_all();
    let names: Vec<String> = match allow {
        Some(a) => a.clone(),
        None => DEFAULT_TOOLS.iter().map(|s| s.to_string()).collect(),
    };
    let mut out: Vec<Value> = vec![];
    for n in names {
        if out.iter().any(|t| t["name"] == n.as_str()) { continue; }
        if let Some(t) = all.iter().find(|t| t["name"] == n.as_str()) { out.push(t.clone()); }
    }
    out
}

/// 发给模型的工具定义。带 cindy-bridge 时与原版一致：read,bash(Cindy 版描述),edit,write,ask_user_question,grep,find,ls；
/// review 模式（--tools 给定）下只保留白名单里的，且不含 ask_user_question。
pub fn request_tools(allow: &Option<Vec<String>>, bridge: bool) -> Vec<Value> {
    if !bridge { return tools_enabled(allow); }
    let extra: serde_json::Map<String, Value> = serde_json::from_str(include_str!("static/bridge_tools.json")).expect("bridge_tools.json");
    let all = tools_all();
    let order = ["read", "bash", "edit", "write", "ask_user_question", "grep", "find", "ls"];
    let review = allow.is_some();
    order.iter().filter(|n| match allow { Some(a) => a.iter().any(|x| x == *n), None => true })
        .filter(|n| !(review && **n == "ask_user_question"))
        .filter_map(|n| extra.get(*n).cloned().or_else(|| all.iter().find(|t| t["name"] == *n).cloned())).collect()
}

/// 原版 buildRules：按已选工具拼规则，去重，末尾固定两条。
fn build_rules(names: &[&str], bridge: bool) -> String {
    let has = |n: &str| names.contains(&n);
    let mut rules: Vec<&str> = vec![];
    let add = |r: &'static str, v: &mut Vec<&str>| { if !v.contains(&r) { v.push(r); } };
    if !bridge && has("bash") && !has("grep") && !has("find") && !has("ls") { add("Use bash for file operations like ls, rg, find", &mut rules); }
    for n in names {
        match *n {
            "read" => add("Use read to examine files instead of cat or sed.", &mut rules),
            "bash" if !bridge => add("You can inspect PI_* environment variables for current model and session details.", &mut rules),
            "edit" => {
                add("Use edit for precise changes (edits[].oldText must match exactly)", &mut rules);
                add("When changing multiple separate locations in one file, use one edit call with multiple entries in edits[] instead of multiple edit calls", &mut rules);
                add("Each edits[].oldText is matched against the original file, not after earlier edits are applied. Do not emit overlapping or nested edits. Merge nearby changes into one edit.", &mut rules);
                add("Keep edits[].oldText as small as possible while still being unique in the file. Do not pad with large unchanged regions.", &mut rules);
            }
            "write" => add("Use write only for new files or complete rewrites.", &mut rules),
            _ => {}
        }
    }
    add("Be concise in your responses", &mut rules);
    add("Show file paths clearly when working with files", &mut rules);
    format!("<rules>\n{}\n</rules>", rules.iter().map(|r| format!("- {r}")).collect::<Vec<_>>().join("\n"))
}

fn build_tools_section(names: &[&str]) -> String {
    let snip: serde_json::Map<String, Value> = serde_json::from_str(include_str!("static/tool_snippets.json")).expect("tool_snippets.json");
    let lines: Vec<String> = names.iter().filter_map(|n| snip.get(*n).and_then(Value::as_str).map(|d| format!("- {n}: {d}"))).collect();
    let body = if lines.is_empty() { "(none)".to_string() } else { lines.join("\n") };
    format!("<tools>\n{body}\n\nIn addition to the tools above, you may have access to other custom tools depending on the project.\n</tools>")
}

/// 项目上下文文件：全局目录一份，加上 cwd 到根每层目录里的第一份（AGENTS.override.md > AGENTS.md > AGENTS.MD > CLAUDE.md > CLAUDE.MD）。
/// 顺序：全局，然后从根到 cwd。git worktree 的影子文件规则不实现。
fn context_file_in(dir: &std::path::Path) -> Option<(String, String)> {
    for name in ["AGENTS.override.md", "AGENTS.md", "AGENTS.MD", "CLAUDE.md", "CLAUDE.MD"] {
        let p = dir.join(name);
        if p.is_file() {
            if let Ok(t) = std::fs::read_to_string(&p) {
                return Some((p.to_string_lossy().into_owned(), t.strip_prefix('\u{feff}').unwrap_or(&t).to_string()));
            }
        }
    }
    None
}

pub fn project_context(cwd: &str) -> Option<String> {
    let mut files: Vec<(String, String)> = vec![];
    let global = context_file_in(&crate::util::agent_dir());
    if let Some(g) = global { files.push(g); }
    let mut chain: Vec<(String, String)> = vec![];
    let mut cur = std::path::PathBuf::from(cwd);
    loop {
        if let Some(f) = context_file_in(&cur) {
            if !files.iter().any(|x| x.0 == f.0) { chain.insert(0, f); }
        }
        match cur.parent() { Some(p) if p != cur => cur = p.to_path_buf(), _ => break }
    }
    files.extend(chain);
    if files.is_empty() { return None; }
    let body = files.iter().map(|(p, c)| format!("<project_instructions path=\"{p}\">\n{c}\n</project_instructions>")).collect::<Vec<_>>().join("\n\n");
    Some(format!("<project_context>\nProject-specific instructions and guidelines:\n\n{body}\n</project_context>"))
}

pub fn system_message(cwd: &str, tools: Vec<Value>, append: &[String], ts: i64, with_context: bool, bridge: bool) -> Value {
    let names: Vec<&str> = tools.iter().filter_map(|t| t["name"].as_str()).collect();
    let mut sections: serde_json::Map<String, Value> = serde_json::from_str(include_str!("static/sections.json")).expect("sections.json");
    sections.insert("tools".into(), build_tools_section(&names).into());
    sections.insert("rules".into(), build_rules(&names, bridge).into());
    // 文档段里的路径是原版的安装目录（PI_PACKAGE_DIR，否则可执行文件所在目录），和 config.ts getPackageDir 一致。
    if let Some(Value::String(d)) = sections.get_mut("docs") {
        *d = d.replace("{{PACKAGE_DIR}}", &package_dir());
    }
    if !append.is_empty() {
        sections.insert("addendum".into(), format!("<addendum>\n{}\n</addendum>", append.join("\n\n")).into());
    }
    if with_context {
        if let Some(pc) = project_context(cwd) { sections.insert("project_context".into(), pc.into()); }
    }
    sections.insert("cwd".into(), format!("<cwd>\n{cwd}\n</cwd>").into());
    json!({"role":"system","content":"","sections":sections,"timestamp":ts,"toolsAdded":tools})
}

/// 发给模型的系统提示文本：各段按在系统消息里的顺序用空行连接（getSystemMessageText）。
pub fn render_system(msg: &Value) -> String {
    let mut parts: Vec<String> = vec![];
    if let Some(c) = msg["content"].as_str().filter(|c| !c.is_empty()) { parts.push(c.to_string()); }
    if let Some(o) = msg["sections"].as_object() {
        for (_, v) in o { if let Some(t) = v.as_str() { parts.push(t.to_string()); } }
    }
    parts.join("\n\n")
}

pub fn read_append(arg: &str) -> String {
    std::fs::read_to_string(arg).unwrap_or_else(|_| arg.to_string())
}

fn package_dir() -> String {
    if let Ok(d) = std::env::var("PI_PACKAGE_DIR") {
        if !d.is_empty() {
            return d;
        }
    }
    // 带回退二进制（宿主传入的原版 Pi）时，文档、示例就在它旁边；pi-rs 自己的目录里没有这些文件。
    if let Ok(f) = std::env::var("CINDY_PI_FALLBACK_BINARY") {
        if let Some(d) = std::path::Path::new(&f).canonicalize().ok().and_then(|p| p.parent().map(|d| d.to_string_lossy().into_owned())) {
            if std::path::Path::new(&d).join("docs").is_dir() { return d; }
        }
    }
    // Bun 的 process.execPath 是解析过符号链接的真实路径（/tmp → /private/tmp），这里同样取 canonical。
    std::env::current_exe()
        .ok()
        .and_then(|p| p.canonicalize().ok())
        .and_then(|p| p.parent().map(|d| d.to_string_lossy().into_owned()))
        .unwrap_or_default()
}
