// 公共小工具：stdout 写 JSON 行、时间戳、ID、配置值里的 $ENV 插值。
use serde_json::Value;
use std::io::Write;
use std::sync::Mutex;

static STDOUT: Mutex<()> = Mutex::new(());

/// 原版每条 RPC 输出是一行紧凑 JSON；加锁保证并发任务写出的行不交错。
pub fn emit_line(v: &Value) {
    let _g = STDOUT.lock().unwrap_or_else(|e| e.into_inner());
    let mut out = std::io::stdout().lock();
    let _ = serde_json::to_writer(&mut out, v);
    let _ = out.write_all(b"\n");
    let _ = out.flush();
}

pub fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

/// 会话文件里的 ISO 时间：毫秒精度、Z 结尾，和 JS 的 toISOString() 一致。
pub fn iso_now() -> String {
    chrono::Utc::now().format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string()
}

/// 会话条目 ID：8 位十六进制（原版用随机 UUID 的前 8 位）。
pub fn short_id() -> String {
    uuid::Uuid::new_v4().simple().to_string()[..8].to_string()
}

/// 解析 models.json 里的值：`$VAR` / `${VAR}` 取环境变量，`$$` 转义成 `$`，其余原样保留。
/// 只支持模板形式；原版的 `!command` 命令插值 Cindy 不用，这里不实现。
pub fn resolve_config_value(raw: &str) -> String {
    let b = raw.as_bytes();
    let mut out = String::new();
    let mut i = 0;
    while i < b.len() {
        if b[i] != b'$' {
            let next = raw[i..].find('$').map(|p| i + p).unwrap_or(b.len());
            out.push_str(&raw[i..next]);
            i = next;
            continue;
        }
        match b.get(i + 1) {
            Some(b'$') | Some(b'!') => {
                out.push(b[i + 1] as char);
                i += 2;
            }
            Some(b'{') => match raw[i + 2..].find('}') {
                Some(end) if is_env_name(&raw[i + 2..i + 2 + end]) => {
                    out.push_str(&std::env::var(&raw[i + 2..i + 2 + end]).unwrap_or_default());
                    i += end + 3;
                }
                _ => {
                    out.push('$');
                    i += 1;
                }
            },
            _ => {
                let rest = &raw[i + 1..];
                let n = rest
                    .char_indices()
                    .take_while(|(k, c)| c.is_ascii_alphanumeric() || *c == '_' && (*k > 0 || true))
                    .count();
                let name = &rest[..n];
                if is_env_name(name) {
                    out.push_str(&std::env::var(name).unwrap_or_default());
                    i += 1 + n;
                } else {
                    out.push('$');
                    i += 1;
                }
            }
        }
    }
    out
}

fn is_env_name(s: &str) -> bool {
    let mut c = s.chars();
    matches!(c.next(), Some(ch) if ch.is_ascii_alphabetic() || ch == '_')
        && c.all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
}

/// PI_CODING_AGENT_DIR，默认 ~/.pi/agent（与原版一致）。
pub fn agent_dir() -> std::path::PathBuf {
    if let Ok(d) = std::env::var("PI_CODING_AGENT_DIR") {
        if !d.is_empty() {
            return d.into();
        }
    }
    let home = std::env::var("HOME").or_else(|_| std::env::var("USERPROFILE")).unwrap_or_default();
    std::path::Path::new(&home).join(".pi").join("agent")
}

/// 写 settings.json 里的一个嵌套字段（保留其余内容），用于 set_auto_compaction。
pub fn write_setting(path: &[&str], val: Value) {
    let f = agent_dir().join("settings.json");
    let mut root: Value = std::fs::read_to_string(&f).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_else(|| serde_json::json!({}));
    if !root.is_object() { root = serde_json::json!({}); }
    let mut cur = &mut root;
    for k in &path[..path.len() - 1] {
        if !cur[*k].is_object() { cur[*k] = serde_json::json!({}); }
        cur = &mut cur[*k];
    }
    cur[path[path.len() - 1]] = val;
    let _ = std::fs::create_dir_all(agent_dir());
    let _ = std::fs::write(&f, serde_json::to_string_pretty(&root).unwrap_or_default());
}
