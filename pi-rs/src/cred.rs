// 凭证路径证据：对应 cindy-bridge 的 touchesCredentialPath / collectResolvedCredentialPaths。
// 正则照抄 maker-core/src/agents/shared/sensitive-credential-paths.ts 的 SENSITIVE_CREDENTIAL_PATH_PATTERN_SPECS。
// bash 用分词近似原版的输入重定向分析：任一词元命中即视为读凭证（宁多问，不漏问）。
use regex_lite::Regex;
use serde_json::Value;
use std::path::Path;
use std::sync::OnceLock;

const SPECS: &[&str] = &[
    r#"(?i)(?:^|[\\/\s'"~])\.(?:ssh|aws|gnupg|kube|docker|azure|claude|codex)\b"#,
    r#"(?i)(?:^|[\\/\s'"~])\.(?:netrc|npmrc|pgpass|pypirc|git-credentials)\b"#,
    r#"(?i)(?:^|[\\/])\.env(?:\.[^\\/]+)?$"#,
    r#"(?i)[\\/]\.cargo[\\/]credentials(?:\.toml)?\b"#,
    r#"(?i)[\\/]\.m2[\\/]settings(?:-security)?\.xml\b"#,
    r#"(?i)\bapplication_default_credentials\b"#,
    r#"(?i)\bcredentials\.json\b"#,
    r#"(?i)[\\/](?:codex|claude|gcloud|containers)[\\/]auth\.json\b"#,
    r#"(?i)[\\/]\.config[\\/](?:gh|hub|glab|op|gcloud)\b"#,
    r#"(?i)/proc/[^\s]*/environ\b"#,
    r#"(?i)\bid_rsa\b|\bid_ed25519\b|\bid_ecdsa\b|\bid_dsa\b|\.pem\b|\.p12\b|\.pfx\b|\.key\b"#,
    r#"(?i)(?:^|[\\/\s'"~])\.xdt-server\b"#,
];

fn patterns() -> &'static [Regex] {
    static P: OnceLock<Vec<Regex>> = OnceLock::new();
    P.get_or_init(|| SPECS.iter().map(|s| Regex::new(s).expect("credential pattern")).collect())
}

pub fn is_credential(s: &str) -> bool {
    patterns().iter().any(|r| r.is_match(s))
}

fn realpath(cwd: &str, s: &str) -> Option<String> {
    let p = if let Some(rest) = s.strip_prefix("~/") {
        std::env::var("HOME").ok().map(|h| Path::new(&h).join(rest))?
    } else if Path::new(s).is_absolute() {
        Path::new(s).to_path_buf()
    } else {
        Path::new(cwd).join(s)
    };
    p.canonicalize().ok().map(|p| p.to_string_lossy().into_owned())
}

/// 从 shell 命令里取出可能是路径的词元（去引号和重定向符）。
fn shell_words(cmd: &str) -> Vec<String> {
    cmd.split(|c: char| c.is_whitespace() || matches!(c, ';' | '|' | '&' | '(' | ')' | '`'))
        .map(|w| w.trim_start_matches(['<', '>', '0', '1', '2']).trim_matches(['\'', '"']))
        .map(|w| w.split_once('=').filter(|(k, _)| k.starts_with('-')).map_or(w, |(_, v)| v))
        .filter(|w| !w.is_empty() && !w.starts_with('-') || w.contains('/'))
        .map(String::from)
        .collect()
}

fn leaves(v: &Value, depth: usize, out: &mut Vec<String>) {
    if depth > 6 { return; }
    match v {
        Value::String(s) if !s.is_empty() => out.push(s.clone()),
        Value::Array(a) => a.iter().for_each(|x| leaves(x, depth + 1, out)),
        Value::Object(o) => o.values().for_each(|x| leaves(x, depth + 1, out)),
        _ => {}
    }
}

/// 返回 (是否触及凭证, 解析后的真实凭证路径)。读类工具看结构化路径字段，shell 看命令词元。
pub fn evidence(tool: &str, input: &Value, cwd: &str) -> (bool, Vec<String>) {
    let candidates: Vec<String> = match tool {
        "bash" | "powershell" => input["command"].as_str().map(shell_words).unwrap_or_default(),
        "read" | "ls" | "find" | "grep" => {
            let mut v = vec![];
            for k in ["path", "paths", "glob", "pattern"] {
                if !(tool == "grep" && k == "pattern") { leaves(&input[k], 0, &mut v); }
            }
            v
        }
        _ => return (false, vec![]),
    };
    let mut touched = candidates.iter().any(|c| is_credential(c));
    if matches!(tool, "bash" | "powershell") {
        // 原版额外检查 /proc/<pid>/environ 读取。
        touched |= input["command"].as_str().map_or(false, |c| c.contains("/proc/") && c.contains("/environ"));
    }
    let mut resolved: Vec<String> = candidates.iter().filter_map(|c| realpath(cwd, c)).filter(|r| is_credential(r)).collect();
    resolved.sort();
    resolved.dedup();
    touched |= !resolved.is_empty();
    (touched, resolved)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn patterns_match_like_bridge() {
        for p in ["/home/u/.ssh/id_rsa", "~/.aws/credentials", "a/.env", ".env.local", "/x/.config/gh/hosts.yml", "/proc/self/environ", "server.key", "~/.npmrc"] {
            assert!(is_credential(p), "{p}");
        }
        for p in ["src/main.rs", "README.md", "environment.ts", ".envrc/x", "keyboard.txt"] {
            assert!(!is_credential(p), "{p}");
        }
    }

    #[test]
    fn bash_and_read_evidence() {
        assert!(evidence("bash", &json!({"command":"cat ~/.ssh/config"}), "/").0);
        assert!(evidence("bash", &json!({"command":"grep KEY < .env"}), "/").0);
        assert!(evidence("bash", &json!({"command":"cat /proc/1/task/2/environ"}), "/").0);
        assert!(!evidence("bash", &json!({"command":"ls -la src"}), "/").0);
        assert!(evidence("read", &json!({"path":".env"}), "/").0);
        assert!(!evidence("read", &json!({"path":"src/lib.rs"}), "/").0);
        assert!(!evidence("write", &json!({"path":".env"}), "/").0);
    }

    // 符号链接需要 unix（Windows 上创建 symlink 要特权，且这个路径解析逻辑是 unix 约定）。
    #[cfg(unix)]
    #[test]
    fn symlink_to_credential_is_resolved() {
        let d = std::env::temp_dir().join(format!("cred-{}", std::process::id()));
        std::fs::create_dir_all(d.join(".ssh")).unwrap();
        std::fs::write(d.join(".ssh/id_rsa"), "x").unwrap();
        let _ = std::fs::remove_file(d.join("innocent.txt"));
        std::os::unix::fs::symlink(d.join(".ssh/id_rsa"), d.join("innocent.txt")).unwrap();
        let (t, paths) = evidence("read", &json!({"path":"innocent.txt"}), d.to_str().unwrap());
        assert!(t);
        assert_eq!(paths.len(), 1);
        assert!(paths[0].ends_with(".ssh/id_rsa"));
        std::fs::remove_dir_all(&d).unwrap();
    }
}
