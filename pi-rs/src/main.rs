// pi-rs：Cindy 用到的 Pi 1.4.0 子集（--mode rpc）的 Rust 实现原型。
// 对照基准是原版 Pi 在 mock 模型下录下的 RPC 事件流和会话文件（见 README.md）。
mod agent;
mod anthropic;
mod args;
mod compact;
mod cred;
mod export;
mod models;
mod perm;
mod openai;
mod prompt;
mod retry;
mod rpc;
mod session;
mod tools;
mod util;

use std::process::ExitCode;

#[tokio::main]
async fn main() -> ExitCode {
    let args = match args::parse(std::env::args().skip(1).collect()) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("Error: {e}");
            return ExitCode::from(1);
        }
    };
    // 一行启动记录：宿主把 Pi 的 stderr 写进 sidecar 日志，据此统计有多少会话真正跑在 pi-rs 上、回退的原因是什么。
    let sid = std::env::var("CINDY_PI_SESSION_ID").unwrap_or_default();
    let reason = fallback_reason(&args);
    if args.mode.as_deref() == Some("rpc") {
        let fb = std::env::var("CINDY_PI_FALLBACK_BINARY").map_or(false, |f| !f.is_empty() && std::path::Path::new(&f).is_file());
        match (&reason, fb) {
            (Some(why), true) => eprintln!("[pi-rs] session={sid} runtime=original reason={why}"),
            (Some(why), false) => eprintln!("[pi-rs] session={sid} runtime=pi-rs unsupported={why} (no fallback binary, running anyway)"),
            (None, _) => eprintln!("[pi-rs] session={sid} runtime=pi-rs"),
        }
    }
    if reason.is_some() {
        if let Ok(orig) = std::env::var("CINDY_PI_FALLBACK_BINARY") {
            if !orig.is_empty() && std::path::Path::new(&orig).is_file() {
                return run_original(&orig);
            }
        }
    }
    if args.version {
        println!("pi-rs {}", env!("CARGO_PKG_VERSION"));
        return ExitCode::SUCCESS;
    }
    if args.mode.as_deref() != Some("rpc") {
        eprintln!("Error: pi-rs only supports --mode rpc");
        return ExitCode::from(1);
    }
    match rpc::run(args).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            util::emit_line(&serde_json::json!({
                "type": "error", "code": "startup", "message": e, "phase": "startup", "exit_code": 1
            }));
            eprintln!("Error: {e}");
            ExitCode::from(1)
        }
    }
}

/// 这个会话需要 pi-rs 没有的能力吗？需要则返回原因，由调用方改用原版 Pi（CINDY_PI_FALLBACK_BINARY）。
/// 未设置回退二进制时照常运行，那些能力静默缺失。
fn fallback_reason(a: &args::Args) -> Option<String> {
    if a.mode.as_deref() != Some("rpc") { return None; }
    let env_set = |k: &str| std::env::var(k).map_or(false, |v| !v.trim().is_empty());
    if let Some(e) = a.extensions.iter().find(|e| !e.contains("cindy-bridge")) { return Some(format!("extension {e}")); }
    if !a.skills.is_empty() { return Some("--skill".into()); }
    if !a.prompt_templates.is_empty() { return Some("--prompt-template".into()); }
    for k in ["CINDY_PI_MCP_BRIDGE", "CINDY_PI_VISION_BRIDGE", "CINDY_PI_HOSTED"] {
        if env_set(k) { return Some(k.into()); }
    }
    if std::env::var("CINDY_LOCAL_HOST_TOOLS_ONLY").as_deref() == Ok("1") { return Some("CINDY_LOCAL_HOST_TOOLS_ONLY".into()); }
    let adapters = std::env::var("CINDY_PI_NATIVE_PROVIDER_ADAPTERS").unwrap_or_default();
    if serde_json::from_str::<Vec<serde_json::Value>>(&adapters).map_or(false, |v| !v.is_empty()) { return Some("native provider adapters".into()); }
    None
}

/// 用原版 Pi 接管本进程：unix 上直接 exec 替换进程；Windows 没有 exec，改为启动子进程，
/// 继承 stdin/stdout/stderr 并转发退出码（宿主的管道读写直接落到子进程上）。
#[cfg(unix)]
fn run_original(orig: &str) -> ExitCode {
    use std::os::unix::process::CommandExt;
    let err = std::process::Command::new(orig).args(std::env::args().skip(1)).exec();
    eprintln!("Error: cannot start original Pi at {orig}: {err}");
    ExitCode::from(1)
}

#[cfg(not(unix))]
fn run_original(orig: &str) -> ExitCode {
    match std::process::Command::new(orig).args(std::env::args().skip(1)).status() {
        Ok(st) => ExitCode::from(st.code().unwrap_or(1).clamp(0, 255) as u8),
        Err(err) => { eprintln!("Error: cannot start original Pi at {orig}: {err}"); ExitCode::from(1) }
    }
}
