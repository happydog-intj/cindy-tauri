//! 应用日志：`eprintln!` 在写 stderr 的同时追加到 `<dataDir>/logs/cindy.log`。
//! 打包后的应用没人收 stderr，「打开日志」目录看的就是这个文件。超过 5 MB 在启动或写入时轮换成 `.1`。
//! 本模块用 `#[macro_use]` 挂在最前面，同名宏盖住标准库的 `eprintln!`，其余代码不用改。
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

const MAX_BYTES: u64 = 5 * 1024 * 1024;

struct Sink {
    file: File,
    path: PathBuf,
    bytes: u64,
}

static SINK: OnceLock<Mutex<Sink>> = OnceLock::new();

macro_rules! eprintln {
    () => { $crate::applog::line("") };
    ($($arg:tt)*) => { $crate::applog::line(&format!($($arg)*)) };
}

fn open(path: &Path) -> std::io::Result<File> {
    let mut o = OpenOptions::new();
    o.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        o.mode(0o600);
    }
    o.open(path)
}

/// 日志目录里的 `cindy.log`。失败不影响启动：只是少一份文件日志。
pub fn init(dir: &Path) {
    if std::fs::create_dir_all(dir).is_err() {
        return;
    }
    let path = dir.join("cindy.log");
    if std::fs::metadata(&path).map(|m| m.len() > MAX_BYTES).unwrap_or(false) {
        let _ = std::fs::rename(&path, dir.join("cindy.log.1"));
    }
    if let Ok(file) = open(&path) {
        let bytes = file.metadata().map(|m| m.len()).unwrap_or(0);
        let _ = SINK.set(Mutex::new(Sink { file, path, bytes }));
    }
}

pub fn line(msg: &str) {
    let _ = writeln!(std::io::stderr(), "{msg}");
    let Some(sink) = SINK.get() else { return };
    let Ok(mut s) = sink.lock() else { return };
    let text = format!("{} {msg}\n", chrono::Utc::now().format("%Y-%m-%dT%H:%M:%S%.3fZ"));
    if s.file.write_all(text.as_bytes()).is_ok() {
        s.bytes += text.len() as u64;
    }
    if s.bytes > MAX_BYTES {
        let rotated = s.path.with_extension("log.1");
        let _ = std::fs::rename(&s.path, rotated);
        if let Ok(f) = open(&s.path) {
            s.file = f;
            s.bytes = 0;
        }
    }
}
