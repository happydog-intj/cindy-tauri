// 内置工具：read / write / edit / bash。返回 (文本, 是否出错)。
// 输出截断规则按工具描述：最多 2000 行或 50KB。
use serde_json::Value;
use std::path::{Path, PathBuf};
use tokio::io::AsyncReadExt;

/// 没有超时限制时用的秒数：约 31 年。f64::MAX 之类的值转成 Duration 会 panic（整个进程崩溃）。
const NO_TIMEOUT: f64 = 1.0e9;
const MAX_LINES: usize = 2000;
const MAX_BYTES: usize = 50 * 1024;

fn resolve(cwd: &str, p: &str) -> PathBuf {
    let p = p.strip_prefix('@').unwrap_or(p);
    let pb = Path::new(p);
    if pb.is_absolute() { pb.to_path_buf() } else { Path::new(cwd).join(pb) }
}

pub async fn run(name: &str, args: &Value, cwd: &str, bridge: bool) -> (String, bool) {
    let r = match name {
        "read" => read(args, cwd).await,
        "write" => write(args, cwd).await,
        "edit" => edit(args, cwd).await,
        "bash" => bash(args, cwd, bridge).await,
        "ls" => ls(args, cwd).await,
        "find" => find(args, cwd).await,
        "grep" => grep(args, cwd).await,
        other => Err(format!("Tool {other} not found")),
    };
    match r {
        Ok(t) => (t, false),
        Err(e) => (e, true),
    }
}

fn s<'a>(a: &'a Value, k: &str) -> Result<&'a str, String> {
    a[k].as_str().ok_or_else(|| format!("Missing required parameter: {k}"))
}

async fn read(a: &Value, cwd: &str) -> Result<String, String> {
    let path = s(a, "path")?;
    let full = resolve(cwd, path);
    let bytes = tokio::fs::read(&full).await.map_err(|_| format!("File not found: {path}"))?;
    let text = String::from_utf8_lossy(&bytes).into_owned();
    let lines: Vec<&str> = text.split('\n').collect();
    let start = a["offset"].as_u64().map(|o| o.max(1) as usize - 1).unwrap_or(0);
    if start >= lines.len() {
        return Err(format!("Offset {} is beyond end of file ({} lines total)", start + 1, lines.len()));
    }
    let limit = a["limit"].as_u64().map(|l| l as usize).unwrap_or(MAX_LINES).min(MAX_LINES);
    let end = (start + limit).min(lines.len());
    let mut out = String::new();
    let mut n = start;
    while n < end && out.len() + lines[n].len() < MAX_BYTES {
        out.push_str(lines[n]);
        out.push('\n');
        n += 1;
    }
    out.pop();
    if n < lines.len() {
        out.push_str(&format!("\n\n[Showing lines {}-{} of {}. Use offset={} to continue.]", start + 1, n, lines.len(), n + 1));
    }
    Ok(out)
}

async fn write(a: &Value, cwd: &str) -> Result<String, String> {
    let (path, content) = (s(a, "path")?, s(a, "content")?);
    let full = resolve(cwd, path);
    if let Some(d) = full.parent() {
        tokio::fs::create_dir_all(d).await.map_err(|e| e.to_string())?;
    }
    tokio::fs::write(&full, content).await.map_err(|e| format!("{e}"))?;
    Ok(format!("Successfully wrote to {path}"))
}

async fn edit(a: &Value, cwd: &str) -> Result<String, String> {
    let path = s(a, "path")?;
    let edits = a["edits"].as_array().ok_or("Missing required parameter: edits")?;
    let full = resolve(cwd, path);
    let orig = tokio::fs::read_to_string(&full).await.map_err(|_| format!("File not found: {path}"))?;
    // 每条 oldText 都在原文件里匹配，且必须唯一、互不重叠。
    let mut spans: Vec<(usize, usize, &str)> = vec![];
    for (i, e) in edits.iter().enumerate() {
        let (old, new) = (s(e, "oldText")?, s(e, "newText")?);
        let hits: Vec<usize> = orig.match_indices(old).map(|(p, _)| p).collect();
        match hits.len() {
            0 => return Err(format!("Could not find edits[{i}].oldText in {path}. The oldText must match exactly including all whitespace and newlines.")),
            1 => spans.push((hits[0], hits[0] + old.len(), new)),
            n => return Err(format!("Found {n} occurrences of edits[{i}].oldText in {path}. The text must be unique. Please provide more context to make it unique.")),
        }
    }
    spans.sort_by_key(|x| x.0);
    if spans.windows(2).any(|w| w[0].1 > w[1].0) {
        return Err(format!("edits overlap in {path}. Merge them into one edit or target disjoint regions."));
    }
    let mut out = String::new();
    let mut at = 0;
    for (st, en, new) in &spans {
        out.push_str(&orig[at..*st]);
        out.push_str(new);
        at = *en;
    }
    out.push_str(&orig[at..]);
    tokio::fs::write(&full, out).await.map_err(|e| e.to_string())?;
    Ok(format!("Successfully replaced {} block(s) in {path}.", spans.len()))
}

async fn bash(a: &Value, cwd: &str, bridge: bool) -> Result<String, String> {
    let cmd = s(a, "command")?;
    let timeout = match &a["timeout"] {
        Value::Null if !bridge => NO_TIMEOUT,
        Value::Number(n) if !bridge => n.as_f64().filter(|t| t.is_finite() && *t > 0.0).unwrap_or(NO_TIMEOUT).min(NO_TIMEOUT),
        Value::Null => 300.0,
        Value::Number(n) => {
            let t = n.as_f64().unwrap_or(0.0);
            if t <= 0.0 { 300.0 } else if t > 1800.0 { return Err(format!("Invalid timeout: timeout is in seconds; maximum is 1800 seconds (received {t})")); } else { t }
        }
        other => return Err(format!("Invalid timeout: timeout is in seconds; maximum is 1800 seconds (received {other})")),
    };
    let mut c = tokio::process::Command::new(if cfg!(windows) { "cmd" } else { "bash" });
    if cfg!(windows) { c.args(["/C", cmd]); } else { c.args(["-c", cmd]); }
    if bridge { scrub_env(&mut c); }
    c.current_dir(cwd).stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::piped()).kill_on_drop(true);
    let mut child = c.spawn().map_err(|e| format!("Failed to spawn: {e}"))?;
    let (mut so, mut se) = (child.stdout.take().unwrap(), child.stderr.take().unwrap());
    let reader = async {
        let (mut a, mut b) = (vec![], vec![]);
        let _ = tokio::join!(so.read_to_end(&mut a), se.read_to_end(&mut b));
        a.extend(b);
        a
    };
    let run = async { let o = reader.await; let st = child.wait().await; (o, st) };
    let res = match tokio::time::timeout(std::time::Duration::from_secs_f64(timeout.clamp(0.0, NO_TIMEOUT)), run).await {
        Ok(r) => r,
        Err(_) => return Err(format!("Command timed out after {timeout} seconds")),
    };
    let (raw, st) = res;
    let text = String::from_utf8_lossy(&raw).into_owned();
    let lines: Vec<&str> = text.split('\n').collect();
    let mut out = if lines.len() > MAX_LINES { lines[lines.len() - MAX_LINES..].join("\n") } else { text.clone() };
    if out.len() > MAX_BYTES {
        let mut cut = out.len() - MAX_BYTES;
        while !out.is_char_boundary(cut) { cut += 1; }
        out = out[cut..].to_string();
    }
    let code = st.ok().and_then(|s| s.code()).unwrap_or(-1);
    if code != 0 {
        return Err(format!("{}\n\nCommand exited with code {code}", out.trim_end()).trim_start().to_string());
    }
    Ok(if out.is_empty() { "(no output)".into() } else { out })
}

// ---------- ls / find / grep（只读工具，plan 模式和 review 模式用） ----------
// 输出格式与原版一致（见 baseline/tools-test.mjs 的逐条对照）。原版靠外部 fd / rg，这里用 ignore 库
// （ripgrep 的目录遍历与 .gitignore 规则）在进程内完成，不需要另外装二进制。

const LS_LIMIT: usize = 500;
const FIND_LIMIT: usize = 1000;
const GREP_LIMIT: usize = 100;
const GREP_LINE_MAX: usize = 500;

fn format_size(b: usize) -> String {
    if b < 1024 { format!("{b}B") } else if b < 1024 * 1024 { format!("{:.1}KB", b as f64 / 1024.0) } else { format!("{:.1}MB", b as f64 / 1048576.0) }
}

/// truncateHead（只按字节）：保留整行，超出 50KB 时停在上一整行；首行就超限返回空。
/// 返回 (内容, 是否截断)。
fn truncate_head_bytes(content: &str) -> (String, bool) {
    let lines: Vec<&str> = if content.is_empty() { vec![] } else { let mut v: Vec<&str> = content.split('\n').collect(); if content.ends_with('\n') { v.pop(); } v };
    if content.len() <= MAX_BYTES { return (content.to_string(), false); }
    if lines.first().map_or(false, |l| l.len() > MAX_BYTES) { return (String::new(), true); }
    let (mut out, mut bytes) = (vec![], 0usize);
    for (i, l) in lines.iter().enumerate() {
        let lb = l.len() + if i > 0 { 1 } else { 0 };
        if bytes + lb > MAX_BYTES { break; }
        out.push(*l);
        bytes += lb;
    }
    (out.join("\n"), true)
}

fn with_notices(mut out: String, notices: Vec<String>) -> String {
    if !notices.is_empty() { out.push_str(&format!("\n\n[{}]", notices.join(". "))); }
    out
}

/// JS 的 toLowerCase().localeCompare()：这里用 Unicode 小写后逐字符比较，对 ASCII 文件名与 ICU 一致。
fn locale_key(s: &str) -> String { s.to_lowercase() }

async fn ls(a: &Value, cwd: &str) -> Result<String, String> {
    let dir = resolve(cwd, a["path"].as_str().unwrap_or("."));
    let limit = a["limit"].as_u64().map(|l| l as usize).unwrap_or(LS_LIMIT);
    let meta = tokio::fs::metadata(&dir).await.map_err(|_| format!("Path not found: {}", dir.display()))?;
    if !meta.is_dir() { return Err(format!("Not a directory: {}", dir.display())); }
    let mut rd = tokio::fs::read_dir(&dir).await.map_err(|e| format!("Cannot read directory: {e}"))?;
    let mut names: Vec<String> = vec![];
    while let Ok(Some(e)) = rd.next_entry().await { names.push(e.file_name().to_string_lossy().into_owned()); }
    names.sort_by(|x, y| locale_key(x).cmp(&locale_key(y)).then(x.cmp(y)));
    let (mut results, mut reached) = (vec![], false);
    for n in names {
        if results.len() >= limit { reached = true; break; }
        let Ok(m) = tokio::fs::metadata(dir.join(&n)).await else { continue };
        results.push(if m.is_dir() { format!("{n}/") } else { n });
    }
    if results.is_empty() { return Ok("(empty directory)".into()); }
    let (out, trunc) = truncate_head_bytes(&results.join("\n"));
    let mut notices = vec![];
    if reached { notices.push(format!("{limit} entries limit reached. Use limit={} for more", limit * 2)); }
    if trunc { notices.push(format!("{} limit reached", format_size(MAX_BYTES))); }
    Ok(with_notices(out, notices))
}

fn rel_path(base: &Path, p: &Path) -> String {
    let r = p.strip_prefix(base).unwrap_or(p);
    r.components().map(|c| c.as_os_str().to_string_lossy().into_owned()).collect::<Vec<_>>().join("/")
}

fn walker(root: &Path) -> ignore::WalkBuilder {
    let mut w = ignore::WalkBuilder::new(root);
    // 与 fd / rg 的默认一致：包含隐藏文件，尊重 .gitignore / .ignore，但 .git 目录本身不进入。
    w.hidden(false).git_ignore(true).git_global(true).git_exclude(true).ignore(true).parents(true).require_git(false);
    w.filter_entry(|e| e.file_name() != ".git");
    // fd / rg 是并行遍历、顺序不定；这里按名字排序，结果可复现。
    w.sort_by_file_name(|a, b| a.cmp(b));
    w
}

async fn find(a: &Value, cwd: &str) -> Result<String, String> {
    let pattern = s(a, "pattern")?.to_string();
    let base = resolve(cwd, a["path"].as_str().unwrap_or("."));
    let limit = a["limit"].as_u64().map(|l| l as usize).unwrap_or(FIND_LIMIT);
    if !base.is_dir() { return Err(format!("[fd error]: Search path \"{}\" is not a directory.\n[fd error]: No valid search paths given.", base.display())); }
    let full_path = pattern.contains('/');
    let eff = if full_path && !pattern.starts_with('/') && !pattern.starts_with("**/") && pattern != "**" { format!("**/{pattern}") } else { pattern.clone() };
    let glob = globset::GlobBuilder::new(&eff).literal_separator(full_path).build().map_err(|e| e.to_string())?.compile_matcher();
    let base2 = base.clone();
    let found = tokio::task::spawn_blocking(move || {
        let mut v: Vec<String> = vec![];
        for e in walker(&base2).build().flatten() {
            if e.depth() == 0 { continue; }
            let p = e.path();
            let hit = if full_path { glob.is_match(p) } else { p.file_name().map_or(false, |n| glob.is_match(n)) };
            if hit {
                let is_dir = e.file_type().map_or(false, |t| t.is_dir());
                let mut r = rel_path(&base2, p);
                if is_dir { r.push('/'); }
                v.push(r);
                if v.len() >= limit { break; }
            }
        }
        v
    }).await.map_err(|e| e.to_string())?;
    if found.is_empty() { return Ok("No files found matching pattern".into()); }
    let reached = found.len() >= limit;
    let (out, trunc) = truncate_head_bytes(&found.join("\n"));
    let mut notices = vec![];
    if reached { notices.push(format!("{limit} results limit reached. Use limit={} for more, or refine pattern", limit * 2)); }
    if trunc { notices.push(format!("{} limit reached", format_size(MAX_BYTES))); }
    Ok(with_notices(out, notices))
}

fn trunc_line(l: &str) -> (String, bool) {
    if l.chars().count() <= GREP_LINE_MAX { (l.to_string(), false) } else { (format!("{}... [truncated]", l.chars().take(GREP_LINE_MAX).collect::<String>()), true) }
}

async fn grep(a: &Value, cwd: &str) -> Result<String, String> {
    let pattern = s(a, "pattern")?.to_string();
    let base = resolve(cwd, a["path"].as_str().unwrap_or("."));
    let ic = a["ignoreCase"].as_bool().unwrap_or(false);
    let literal = a["literal"].as_bool().unwrap_or(false);
    let ctx = a["context"].as_u64().unwrap_or(0) as usize;
    let limit = a["limit"].as_u64().map(|l| (l as usize).max(1)).unwrap_or(GREP_LIMIT);
    let glob = a["glob"].as_str().map(String::from);
    let meta = tokio::fs::metadata(&base).await.map_err(|_| format!("Path not found: {}", base.display()))?;
    let is_dir = meta.is_dir();
    let src = if literal { regex_lite_escape(&pattern) } else { pattern };
    let re = regex::RegexBuilder::new(&format!("(?:{src})")).case_insensitive(ic).build().map_err(|e| format!("rg: {e}"))?;
    let base2 = base.clone();
    let res = tokio::task::spawn_blocking(move || {
        let gm = glob.as_ref().and_then(|g| globset::GlobBuilder::new(g).literal_separator(false).build().ok()).map(|g| g.compile_matcher());
        let files: Vec<PathBuf> = if is_dir {
            walker(&base2).build().flatten().filter(|e| e.file_type().map_or(false, |t| t.is_file())).map(|e| e.into_path())
                .filter(|p| gm.as_ref().map_or(true, |g| { let r = rel_path(&base2, p); g.is_match(&r) || p.file_name().map_or(false, |n| g.is_match(n)) })).collect()
        } else { vec![base2.clone()] };
        let (mut blocks, mut count, mut reached, mut lines_trunc) = (vec![] as Vec<String>, 0usize, false, false);
        'outer: for f in files {
            let Ok(bytes) = std::fs::read(&f) else { continue };
            if bytes.contains(&0) { continue; }
            let text = String::from_utf8_lossy(&bytes);
            let lines: Vec<&str> = text.replace("\r\n", "\n").replace('\r', "\n").split('\n').map(|l| l.to_string()).collect::<Vec<_>>().iter().map(|_| "").collect();
            let owned: Vec<String> = text.replace("\r\n", "\n").replace('\r', "\n").split('\n').map(String::from).collect();
            let _ = lines;
            let name = if is_dir { let r = rel_path(&base2, &f); if r.is_empty() { f.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default() } else { r } } else { f.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default() };
            let total = if owned.last().map_or(false, |l| l.is_empty()) { owned.len() - 1 } else { owned.len() };
            for (i, l) in owned.iter().enumerate().take(total) {
                if !re.is_match(l) { continue; }
                count += 1;
                let n = i + 1;
                let (start, end) = if ctx > 0 { (n.saturating_sub(ctx).max(1), (n + ctx).min(owned.len())) } else { (n, n) };
                for cur in start..=end {
                    let (t, tr) = trunc_line(owned.get(cur - 1).map(String::as_str).unwrap_or(""));
                    if tr { lines_trunc = true; }
                    blocks.push(if cur == n { format!("{name}:{cur}: {t}") } else { format!("{name}-{cur}- {t}") });
                }
                if count >= limit { reached = true; break 'outer; }
            }
        }
        (blocks, count, reached, lines_trunc)
    }).await.map_err(|e| e.to_string())?;
    let (blocks, count, reached, lines_trunc) = res;
    if count == 0 { return Ok("No matches found".into()); }
    let (out, trunc) = truncate_head_bytes(&blocks.join("\n"));
    let mut notices = vec![];
    if reached { notices.push(format!("{limit} matches limit reached. Use limit={} for more, or refine pattern", limit * 2)); }
    if trunc { notices.push(format!("{} limit reached", format_size(MAX_BYTES))); }
    if lines_trunc { notices.push(format!("Some lines truncated to {GREP_LINE_MAX} chars. Use read tool to see full lines")); }
    Ok(with_notices(out, notices))
}

fn regex_lite_escape(s: &str) -> String {
    let mut o = String::new();
    for c in s.chars() { if "\\.+*?()|[]{}^$#&-~".contains(c) { o.push('\\'); } o.push(c); }
    o
}

/// 与 bridge 的 isolatedBashEnvironment 一致：模型执行的命令看不到 Pi 的鉴权、权限档路径等内部变量。
/// PI_CODING_AGENT_DIR 指向隔离的包管理 home（取自 CINDY_PI_BASH_PACKAGE_HOME），PI_PACKAGE_DIR 去掉。
fn scrub_env(c: &mut tokio::process::Command) {
    let mut names: std::collections::HashSet<String> = [
        "CINDY_PI_SECRET_ENV_NAMES", "CINDY_PI_PERMISSION_FILE", "CINDY_PI_MODEL_REQUEST_PREFS_FILE", "CINDY_PI_FAST_MODELS",
        "CINDY_PI_TURN_TOOL_POLICY", "CINDY_PI_PACKAGE_MANAGEMENT", "CINDY_PI_BASH_PACKAGE_HOME", "CINDY_PI_MANAGED_RG_PATH",
        "CINDY_PI_SUBAGENT_RUN_DIR", "PI_CODING_AGENT_DIR", "CINDY_PI_HOSTED", "PI_PACKAGE_DIR",
        "CINDY_PI_SESSION_TOKEN", "CINDY_PI_API_KEY", "CINDY_PI_OPENAI_PROXY_KEY", "CINDY_PI_XAI_PROXY_API_KEY",
    ].iter().map(|s| s.to_string()).collect();
    if let Some(extra) = std::env::var("CINDY_PI_SECRET_ENV_NAMES").ok().and_then(|t| serde_json::from_str::<Vec<String>>(&t).ok()) { names.extend(extra); }
    for (k, _) in std::env::vars() {
        if names.contains(&k) || (k.starts_with("CINDY_PI_KEY_") && k[13..].chars().all(|ch| ch.is_ascii_uppercase() || ch.is_ascii_digit() || ch == '_')) { c.env_remove(&k); }
    }
    if let Ok(home) = std::env::var("CINDY_PI_BASH_PACKAGE_HOME") { if !home.is_empty() { c.env("PI_CODING_AGENT_DIR", home); } }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // 回归：不带 cindy-bridge 时 bash 没有超时，曾因 f64::MAX 转 Duration 而 panic。
    #[tokio::test]
    async fn bash_without_timeout_does_not_panic() {
        for bridge in [false, true] {
            let (out, err) = run("bash", &json!({"command": "echo hi"}), ".", bridge).await;
            assert!(!err, "{out}");
            assert_eq!(out.trim(), "hi");
        }
        let (out, err) = run("bash", &json!({"command": "echo hi", "timeout": 1e300}), ".", false).await;
        assert!(!err && out.trim() == "hi", "{out}");
        let (out, err) = run("bash", &json!({"command": "sleep 5", "timeout": 0.2}), ".", false).await;
        assert!(err && out.contains("timed out"), "{out}");
    }
}
