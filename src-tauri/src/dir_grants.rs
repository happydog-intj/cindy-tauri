//! 附加目录授权：只读参考目录（extraDirs）与可写目录（writableDirs）。
//! 对应上游 maker-ipc 的 extraDirsValidator / writableDirectoryPickerGrant / applyDirectoryGrants。
//!
//! 可写目录必须来自系统选择器：选择器选中后登记一条授权（5 分钟有效、只能用一次），
//! 设置可写目录时对每个新增目录核对授权，并再核对一次真实路径，防止选完后把目录换成指向别处的符号链接。

use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// 只读参考目录上限（与上游 EXTRA_DIRS_MAX 一致）。
pub const EXTRA_DIRS_MAX: usize = 10;
const GRANT_TTL: Duration = Duration::from_secs(5 * 60);

struct Grant {
    real_path: PathBuf,
    expires: Instant,
}

/// (会话作用域, 词法路径) → 授权。进程内全局：选择器与设置目录是同一个主窗口里的两步。
static GRANTS: Mutex<Option<HashMap<(String, PathBuf), Grant>>> = Mutex::new(None);

fn lexical(p: &str) -> PathBuf {
    let mut out = PathBuf::new();
    for c in Path::new(p.trim()).components() {
        match c {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            o => out.push(o.as_os_str()),
        }
    }
    out
}

fn purge(map: &mut HashMap<(String, PathBuf), Grant>, now: Instant) {
    map.retain(|_, g| g.expires > now);
}

/// 选择器选中目录后登记授权。目录必须存在。
pub fn issue_picker_grant(scope: &str, directory: &str) -> Result<(), String> {
    issue_at(scope, directory, Instant::now())
}

fn issue_at(scope: &str, directory: &str, now: Instant) -> Result<(), String> {
    let lex = lexical(directory);
    let real = std::fs::canonicalize(&lex).map_err(|e| format!("[INVALID_PARAMS] directory is unavailable: {e}"))?;
    let mut g = GRANTS.lock().unwrap();
    let map = g.get_or_insert_with(HashMap::new);
    purge(map, now);
    map.insert((scope.to_string(), lex), Grant { real_path: real, expires: now + GRANT_TTL });
    Ok(())
}

/// 核对新增的可写目录（`requested` 里不在 `previous` 的那些）。全部通过才消费授权，否则一个都不消费。
pub fn consume_picker_grants(scope: &str, requested: &[String], previous: &[String]) -> Result<(), String> {
    consume_at(scope, requested, previous, Instant::now())
}

fn consume_at(scope: &str, requested: &[String], previous: &[String], now: Instant) -> Result<(), String> {
    let mut g = GRANTS.lock().unwrap();
    let map = g.get_or_insert_with(HashMap::new);
    purge(map, now);
    let prev: Vec<PathBuf> = previous.iter().map(|p| lexical(p)).collect();
    let mut matched = Vec::new();
    for dir in requested.iter().map(|p| lexical(p)).filter(|p| !prev.contains(p)) {
        let key = (scope.to_string(), dir.clone());
        let Some(grant) = map.get(&key) else {
            return Err("[PRECONDITION_FAILED] Writable directory was not authorized by the system picker".into());
        };
        match std::fs::canonicalize(&dir) {
            Err(_) => return Err("[PRECONDITION_FAILED] Writable directory is unavailable".into()),
            Ok(real) if real != grant.real_path => {
                return Err("[PRECONDITION_FAILED] Writable directory changed after it was selected".into())
            }
            Ok(_) => matched.push(key),
        }
    }
    for k in matched {
        map.remove(&k);
    }
    Ok(())
}

/// 校验目录列表：绝对路径、存在、是目录、去重；只读目录还要不在工作目录里面（多余），并限制数量。
/// 返回 (通过, 被拒的 (路径, 原因))。
pub fn validate(dirs: &[Value], working_dir: Option<&str>, limit: Option<usize>) -> (Vec<String>, Vec<(String, &'static str)>) {
    let wd = working_dir.map(str::trim).filter(|s| !s.is_empty()).map(lexical);
    let mut valid: Vec<String> = Vec::new();
    let mut rejected = Vec::new();
    for raw in dirs {
        let Some(s) = raw.as_str().map(str::trim).filter(|s| !s.is_empty()) else {
            rejected.push((raw.to_string(), "empty"));
            continue;
        };
        if !Path::new(s).is_absolute() {
            rejected.push((s.to_string(), "not-absolute"));
            continue;
        }
        if valid.iter().any(|v| v == s) {
            continue;
        }
        match std::fs::metadata(s) {
            Err(_) => rejected.push((s.to_string(), "not-exist")),
            Ok(m) if !m.is_dir() => rejected.push((s.to_string(), "not-dir")),
            Ok(_) if wd.as_ref().is_some_and(|w| lexical(s).starts_with(w)) => rejected.push((s.to_string(), "redundant-subdir")),
            Ok(_) if limit.is_some_and(|n| valid.len() >= n) => rejected.push((s.to_string(), "over-limit")),
            Ok(_) => valid.push(s.to_string()),
        }
    }
    (valid, rejected)
}

/// 去掉与 `blocked` 指向同一棵目录树的候选（同一目录不能同时只读和可写；含符号链接别名和父子关系）。
pub fn exclude_conflicts(candidates: Vec<String>, blocked: &[String]) -> Vec<String> {
    let key = |d: &str| std::fs::canonicalize(d).unwrap_or_else(|_| lexical(d));
    let blocked_keys: Vec<PathBuf> = blocked.iter().map(|d| key(d)).collect();
    let mut accepted: Vec<PathBuf> = Vec::new();
    let mut out = Vec::new();
    for c in candidates {
        let k = key(&c);
        let overlaps = |o: &PathBuf| k.starts_with(o) || o.starts_with(&k);
        if blocked_keys.iter().any(overlaps) || accepted.iter().any(overlaps) {
            continue;
        }
        accepted.push(k);
        out.push(c);
    }
    out
}

/// 每轮消息前的目录提示，对应上游 piExtraDirsPrompt。只读目录只给末级目录名（不泄露完整路径之外的信息），
/// 可写目录告诉模型“用户明确允许了额外的可写位置”，具体放行由权限门按 writableRoots 判断。
pub fn prompt(extra_dirs: &[String], writable_dirs: &[String]) -> String {
    let mut sections: Vec<String> = Vec::new();
    if !extra_dirs.is_empty() {
        let mut labels: Vec<String> = Vec::new();
        for d in extra_dirs {
            let name = Path::new(d.trim_end_matches(['/', '\\'])).file_name().map(|n| n.to_string_lossy().into_owned()).filter(|n| !n.is_empty()).unwrap_or_else(|| "reference".into());
            if !labels.contains(&name) {
                labels.push(name);
            }
        }
        sections.push("<cindy-extra-reference-directories>".into());
        sections.push("This task can read extra references as read-only.".into());
        sections.extend(labels.into_iter().map(|l| format!("- {l}")));
        sections.push("</cindy-extra-reference-directories>".into());
    }
    if !writable_dirs.is_empty() {
        sections.push("<cindy-extra-writable-directories>".into());
        sections.push("The user explicitly allowed additional writable locations for this task. Stay inside those grants.".into());
        sections.extend(writable_dirs.iter().map(|d| format!("- {d}")));
        sections.push("</cindy-extra-writable-directories>".into());
    }
    sections.join("\n")
}

/// 把整条设置结果转成 IPC 返回值（上游返回最终保留的目录数组）。
pub fn result(dirs: &[String]) -> Value {
    json!(dirs)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("cindy-dg-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }
    fn s(p: &Path) -> String {
        p.to_string_lossy().into_owned()
    }

    #[test]
    fn writable_dir_needs_a_picker_grant_and_it_is_single_use() {
        let d = tmp("single");
        assert!(consume_at("s1", &[s(&d)], &[], Instant::now()).is_err(), "没选过的目录不放行");
        issue_at("s1", &s(&d), Instant::now()).unwrap();
        assert!(consume_at("s2", &[s(&d)], &[], Instant::now()).is_err(), "别的会话用不了");
        consume_at("s1", &[s(&d)], &[], Instant::now()).unwrap();
        assert!(consume_at("s1", &[s(&d)], &[], Instant::now()).is_err(), "授权只能用一次");
    }

    #[test]
    fn retained_dirs_need_no_new_grant() {
        let d = tmp("retain");
        consume_at("s1", &[s(&d)], &[s(&d)], Instant::now()).unwrap();
    }

    #[test]
    fn grant_expires_and_failed_batch_consumes_nothing() {
        let (a, b) = (tmp("ex-a"), tmp("ex-b"));
        let t0 = Instant::now();
        issue_at("s3", &s(&a), t0).unwrap();
        issue_at("s3", &s(&b), t0).unwrap();
        // 第二个目录没授权过：整批失败，第一个授权保留
        let c = tmp("ex-c");
        assert!(consume_at("s3", &[s(&a), s(&c)], &[], t0).is_err());
        consume_at("s3", &[s(&a), s(&b)], &[], t0).unwrap();
        issue_at("s3", &s(&a), t0).unwrap();
        assert!(consume_at("s3", &[s(&a)], &[], t0 + GRANT_TTL + Duration::from_secs(1)).is_err(), "过期");
    }

    #[cfg(unix)]
    #[test]
    fn swapped_symlink_is_rejected() {
        let (real1, real2) = (tmp("sw-1"), tmp("sw-2"));
        let link = std::env::temp_dir().join(format!("cindy-dg-link-{}", std::process::id()));
        let _ = std::fs::remove_file(&link);
        std::os::unix::fs::symlink(&real1, &link).unwrap();
        issue_at("s4", &s(&link), Instant::now()).unwrap();
        std::fs::remove_file(&link).unwrap();
        std::os::unix::fs::symlink(&real2, &link).unwrap();
        let e = consume_at("s4", &[s(&link)], &[], Instant::now()).unwrap_err();
        assert!(e.contains("changed after"), "{e}");
        let _ = std::fs::remove_file(&link);
    }

    #[test]
    fn validate_filters_and_limits() {
        let (wd, inner, other, file) = (tmp("v-wd"), tmp("v-wd").join("in"), tmp("v-other"), tmp("v-file").join("f.txt"));
        std::fs::create_dir_all(&inner).unwrap();
        std::fs::write(&file, "x").unwrap();
        let input = vec![
            json!(s(&other)), json!(s(&other)), json!(""), json!("rel/path"), json!(s(&inner)), json!(s(&file)), json!("/no/such/dir-xyz"), json!(5),
        ];
        let (valid, rej) = validate(&input, Some(&s(&wd)), Some(10));
        assert_eq!(valid, vec![s(&other)]);
        let reasons: Vec<&str> = rej.iter().map(|r| r.1).collect();
        assert_eq!(reasons, vec!["empty", "not-absolute", "redundant-subdir", "not-dir", "not-exist", "empty"]);
        let many: Vec<Value> = (0..3).map(|i| json!(s(&tmp(&format!("v-m{i}"))))).collect();
        let (v, r) = validate(&many, None, Some(2));
        assert_eq!((v.len(), r.len(), r[0].1), (2, 1, "over-limit"));
    }

    #[test]
    fn conflicts_between_readonly_and_writable_are_dropped() {
        let (a, b) = (tmp("c-a"), tmp("c-b"));
        let sub = a.join("sub");
        std::fs::create_dir_all(&sub).unwrap();
        assert_eq!(exclude_conflicts(vec![s(&sub), s(&b)], &[s(&a)]), vec![s(&b)], "子目录与已授权目录冲突");
        assert_eq!(exclude_conflicts(vec![s(&a), s(&sub)], &[]), vec![s(&a)], "候选之间也去重");
    }

    #[test]
    fn prompt_lists_dirs() {
        assert_eq!(prompt(&[], &[]), "");
        let p = prompt(&["/a/refs".into(), "/b/refs".into()], &["/w/out".into()]);
        assert_eq!(p.matches("- refs").count(), 1, "同名只读目录合并");
        assert!(p.contains("- /w/out") && p.contains("<cindy-extra-writable-directories>"));
    }
}
