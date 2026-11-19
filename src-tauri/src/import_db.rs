//! 桌面版导入的底层：找 profile、给桌面版库拍只读快照、按列交集读写、目录复制。
//! 对应 sidecar/src/desktop-import.mjs 的前半部分。桌面版数据只读：数据库先拍快照再读，文件只复制不移动。

use regex::Regex;
use rusqlite::{backup::Backup, Connection, OpenFlags};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// 桌面版 userData 目录名（desktop main/regionUserData.ts：global=CindyGlobal，cn=Cindy）。
const PROFILE_NAMES: [&str; 2] = ["CindyGlobal", "Cindy"];
/// 只导入 Tauri 版能继续使用的任务：本机 Pi 任务，排除远程主机 / 设备 / 团队 / IM 来源。
const EXCLUDE_IF_SET: [&str; 5] = ["remote_host_id", "agent_device_id", "orca_role", "feishu_open_id", "im_bot_context_id"];

#[derive(Debug, Clone)]
pub struct Candidate {
    pub profile_dir: PathBuf,
    pub db_file: String,
    pub owner_id: String,
    pub sessions: i64,
    pub providers: i64,
    pub modified_ms: f64,
}

impl Candidate {
    pub fn to_json(&self) -> Value {
        json!({
            "profileDir": self.profile_dir.to_string_lossy(),
            "dbFile": self.db_file,
            "ownerId": self.owner_id,
            "sessions": self.sessions,
            "providers": self.providers,
        })
    }
}

pub fn quote(c: &str) -> String {
    format!("\"{}\"", c.replace('"', "\"\""))
}

pub fn columns_of(conn: &Connection, table: &str, schema: &str) -> Vec<String> {
    let Ok(mut stmt) = conn.prepare("SELECT name FROM pragma_table_info(?1, ?2)") else { return Vec::new() };
    stmt.query_map([table, schema], |r| r.get::<_, String>(0)).map(|rows| rows.filter_map(Result::ok).collect()).unwrap_or_default()
}

pub fn table_exists(conn: &Connection, table: &str) -> bool {
    conn.query_row("SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1", [table], |_| Ok(())).is_ok()
}

/// 一个桌面版库里可导入任务的 WHERE 条件（按实际存在的列拼）。
pub fn importable_where(cols: &[String]) -> String {
    let mut w = vec!["agent_kind = 'pi'".to_string(), "status != 'deleted'".to_string()];
    for c in EXCLUDE_IF_SET {
        if cols.iter().any(|x| x == c) {
            w.push(format!("({q} IS NULL OR {q} = '')", q = quote(c)));
        }
    }
    w.join(" AND ")
}

/// Electron app.getPath('appData')：macOS ~/Library/Application Support，Windows %APPDATA%，其余 XDG。
fn app_data_root() -> PathBuf {
    let home = || std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" }).map(PathBuf::from).unwrap_or_default();
    if cfg!(target_os = "macos") {
        home().join("Library").join("Application Support")
    } else if cfg!(windows) {
        std::env::var_os("APPDATA").map(PathBuf::from).unwrap_or_else(|| home().join("AppData").join("Roaming"))
    } else {
        std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from).unwrap_or_else(|| home().join(".config"))
    }
}

pub fn profile_dirs() -> Vec<PathBuf> {
    match std::env::var_os("CINDY_TAURI_DESKTOP_PROFILE_DIRS") {
        Some(v) => std::env::split_paths(&v).filter(|p| !p.as_os_str().is_empty()).collect(),
        None => PROFILE_NAMES.iter().map(|n| app_data_root().join(n)).collect(),
    }
}

pub fn remove_db(file: &Path) {
    for ext in ["", "-wal", "-shm", "-journal"] {
        let mut p = file.as_os_str().to_owned();
        p.push(ext);
        let _ = std::fs::remove_file(PathBuf::from(p));
    }
}

/// 给桌面版库拍一份一致的快照到 dst，源目录里不留任何新文件：
/// - 没有未合并的 WAL（桌面版没在运行）：直接复制主库文件，quick_check 通过就用；
/// - 有 WAL（桌面版可能正在运行）：用只读连接走 SQLite backup，拿到一致快照。
pub fn snapshot_db(src: &Path, dst: &Path) -> Result<(), String> {
    remove_db(dst);
    let mut wal = src.as_os_str().to_owned();
    wal.push("-wal");
    let wal_bytes = std::fs::metadata(PathBuf::from(wal)).map(|m| m.len()).unwrap_or(0);
    if wal_bytes == 0 {
        std::fs::copy(src, dst).map_err(|e| format!("copy {}: {e}", src.display()))?;
        let ok = Connection::open_with_flags(dst, OpenFlags::SQLITE_OPEN_READ_ONLY)
            .and_then(|c| c.query_row("PRAGMA quick_check", [], |r| r.get::<_, String>(0)))
            .map(|s| s == "ok")
            .unwrap_or(false);
        if ok {
            return Ok(());
        }
        remove_db(dst);
    }
    let from = Connection::open_with_flags(src, OpenFlags::SQLITE_OPEN_READ_ONLY).map_err(|e| e.to_string())?;
    let _ = from.busy_timeout(Duration::from_millis(5000));
    let mut to = Connection::open(dst).map_err(|e| e.to_string())?;
    let backup = Backup::new(&from, &mut to).map_err(|e| e.to_string())?;
    backup.run_to_completion(256, Duration::from_millis(20), None).map_err(|e| e.to_string())
}

/// 找出这台电脑上的桌面版 profile（只读快照，不弹任何授权）。`tmp_dir` 放临时快照。
pub fn scan_profiles(tmp_dir: &Path) -> Vec<Candidate> {
    let db_re = Regex::new(r"^cindy-([A-Za-z0-9_-]{1,64})\.db$").unwrap();
    let mut out = Vec::new();
    for dir in profile_dirs() {
        let Ok(rd) = std::fs::read_dir(&dir) else { continue };
        let mut names: Vec<String> = rd.filter_map(|e| e.ok()).filter_map(|e| e.file_name().into_string().ok()).collect();
        names.sort();
        for name in names {
            let Some(m) = db_re.captures(&name) else { continue };
            let file = dir.join(&name);
            let snap = tmp_dir.join(format!("scan-{}-{}.db", std::process::id(), std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0)));
            let _ = std::fs::create_dir_all(tmp_dir);
            let counted = (|| -> Result<Option<(i64, i64)>, String> {
                snapshot_db(&file, &snap)?;
                let sql = Connection::open_with_flags(&snap, OpenFlags::SQLITE_OPEN_READ_ONLY).map_err(|e| e.to_string())?;
                if !table_exists(&sql, "sessions") {
                    return Ok(None);
                }
                let cols = columns_of(&sql, "sessions", "main");
                let sessions: i64 = sql
                    .query_row(&format!("SELECT COUNT(*) FROM sessions WHERE {}", importable_where(&cols)), [], |r| r.get(0))
                    .map_err(|e| e.to_string())?;
                let providers: i64 = if table_exists(&sql, "custom_providers") {
                    sql.query_row("SELECT COUNT(*) FROM custom_providers", [], |r| r.get(0)).map_err(|e| e.to_string())?
                } else {
                    0
                };
                Ok(Some((sessions, providers)))
            })();
            remove_db(&snap);
            match counted {
                Ok(Some((sessions, providers))) => {
                    let modified_ms = std::fs::metadata(&file)
                        .and_then(|m| m.modified())
                        .ok()
                        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                        .map(|d| d.as_secs_f64() * 1000.0)
                        .unwrap_or(0.0);
                    out.push(Candidate { profile_dir: dir.clone(), db_file: name.clone(), owner_id: m[1].to_string(), sessions, providers, modified_ms });
                }
                Ok(None) => {}
                Err(e) => eprintln!("[desktop-import] cannot read {name}: {e}"),
            }
        }
    }
    // 最近用过的账号排前面。
    out.sort_by(|a, b| b.modified_ms.partial_cmp(&a.modified_ms).unwrap_or(std::cmp::Ordering::Equal));
    out
}

/// 目录总大小；超过 limit 提前停止返回 None。
pub fn dir_size_within(dir: &Path, limit: u64) -> Option<u64> {
    let mut total = 0u64;
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else { continue };
        for e in rd.flatten() {
            let Ok(ft) = e.file_type() else { continue };
            if ft.is_dir() {
                stack.push(e.path());
            } else if ft.is_file() {
                total += e.metadata().map(|m| m.len()).unwrap_or(0);
                if total > limit {
                    return None;
                }
            }
        }
    }
    Some(total)
}

/// 递归复制，已存在的文件不覆盖（对应 cp force:false）。符号链接按链接复制（Windows 上跳过）。
pub fn copy_tree(src: &Path, dst: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dst)?;
    for e in std::fs::read_dir(src)? {
        let e = e?;
        let to = dst.join(e.file_name());
        let ft = e.file_type()?;
        if ft.is_dir() {
            copy_tree(&e.path(), &to)?;
        } else if ft.is_symlink() {
            #[cfg(unix)]
            if std::fs::symlink_metadata(&to).is_err() {
                std::os::unix::fs::symlink(std::fs::read_link(e.path())?, &to)?;
            }
        } else if !to.exists() {
            std::fs::copy(e.path(), &to)?;
        }
    }
    Ok(())
}

/// 桌面版 dialogue 工作区 → Tauri 版同结构路径；不在 profile 的 dialogues/ 下返回 None（项目目录原样保留）。
pub fn relocate_dialogue(profile_dir: &Path, data_dir: &Path, working_dir: &str) -> Option<PathBuf> {
    if working_dir.is_empty() {
        return None;
    }
    let rel = Path::new(working_dir).strip_prefix(profile_dir).ok()?;
    if rel.as_os_str().is_empty() {
        return None;
    }
    let parts: Vec<_> = rel.components().map(|c| c.as_os_str().to_owned()).collect();
    if parts.iter().any(|p| p == "..") {
        return None;
    }
    let i = parts.iter().position(|p| p == "dialogues")?;
    if i == parts.len() - 1 {
        return None;
    }
    let mut out = data_dir.join("dialogues");
    for p in &parts[i + 1..] {
        out.push(p);
    }
    Some(out)
}

/// desktop main/appSessionState.ts dataOwnerStorageKey：sha256(ownerId) 的前 20 个十六进制字符。
pub fn owner_storage_key(owner_id: &str) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(owner_id.as_bytes());
    digest.iter().map(|b| format!("{b:02x}")).collect::<String>()[..20].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("cindy-import-db-{name}-{}-{}", std::process::id(), std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn owner_key_matches_the_electron_derivation() {
        // node: sha256('cmfixtureuser0000000000001').hex.slice(0, 20)
        assert_eq!(owner_storage_key("cmfixtureuser0000000000001"), "9ec5625b128b390a2246");
    }

    #[test]
    fn dialogue_workdirs_move_under_the_new_data_dir_and_nothing_else_does() {
        let p = Path::new("/p/CindyGlobal");
        let d = Path::new("/data");
        assert_eq!(relocate_dialogue(p, d, "/p/CindyGlobal/owners/abc/dialogues/2026-10-01/id1"), Some(PathBuf::from("/data/dialogues/2026-10-01/id1")));
        for w in ["/p/CindyGlobal", "/p/CindyGlobal/owners/abc", "/p/CindyGlobal/dialogues", "/elsewhere/dialogues/x", "/p/CindyGlobal/../x/dialogues/y", ""] {
            assert_eq!(relocate_dialogue(p, d, w), None, "{w}");
        }
    }

    #[test]
    fn copy_tree_never_overwrites_and_size_check_stops_early() {
        let root = temp("copy");
        let src = root.join("src");
        std::fs::create_dir_all(src.join("a/b")).unwrap();
        std::fs::write(src.join("a/b/f.txt"), "new").unwrap();
        std::fs::write(src.join("top.txt"), "12345").unwrap();
        let dst = root.join("dst");
        std::fs::create_dir_all(dst.join("a/b")).unwrap();
        std::fs::write(dst.join("a/b/f.txt"), "keep").unwrap();
        copy_tree(&src, &dst).unwrap();
        assert_eq!(std::fs::read_to_string(dst.join("a/b/f.txt")).unwrap(), "keep");
        assert_eq!(std::fs::read_to_string(dst.join("top.txt")).unwrap(), "12345");
        assert_eq!(dir_size_within(&src, 100), Some(8));
        assert_eq!(dir_size_within(&src, 6), None, "超过上限就停");
        assert_eq!(dir_size_within(&root.join("missing"), 6), Some(0));
    }

    #[test]
    fn snapshots_of_a_live_wal_database_are_consistent_and_leave_the_source_alone() {
        let root = temp("snap");
        let src = root.join("cindy-u1.db");
        let live = Connection::open(&src).unwrap();
        live.execute_batch("PRAGMA journal_mode=WAL; PRAGMA wal_autocheckpoint=0; CREATE TABLE t(x); INSERT INTO t VALUES (1),(2);").unwrap();
        let mut wal = src.as_os_str().to_owned();
        wal.push("-wal");
        assert!(std::fs::metadata(PathBuf::from(wal)).unwrap().len() > 0, "夹具里有未合并的 WAL");
        let dst = root.join("snap.db");
        snapshot_db(&src, &dst).unwrap();
        let n: i64 = Connection::open(&dst).unwrap().query_row("SELECT COUNT(*) FROM t", [], |r| r.get(0)).unwrap();
        assert_eq!(n, 2, "只在 WAL 里的数据也在快照里");
        // 没有 WAL 的库：走复制路径
        drop(live);
        let dst2 = root.join("snap2.db");
        snapshot_db(&src, &dst2).unwrap();
        assert_eq!(Connection::open(&dst2).unwrap().query_row::<i64, _, _>("SELECT COUNT(*) FROM t", [], |r| r.get(0)).unwrap(), 2);
    }

    #[test]
    fn importable_where_adapts_to_the_columns_that_exist() {
        let all: Vec<String> = ["remote_host_id", "orca_role", "title"].iter().map(|s| s.to_string()).collect();
        let w = importable_where(&all);
        assert!(w.contains("agent_kind = 'pi'") && w.contains("\"remote_host_id\" IS NULL") && w.contains("\"orca_role\"") && !w.contains("feishu_open_id"));
        assert_eq!(importable_where(&[]), "agent_kind = 'pi' AND status != 'deleted'");
    }
}
