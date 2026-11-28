//! 用法：cindy-server  （环境变量：CINDY_SERVER_ADDR 默认 127.0.0.1:8787，CINDY_SERVER_DB 默认 ./cindy-server.db，
//! CINDY_SERVER_ALLOWED_EMAILS 逗号分隔的允许邮箱，必须设置）。
use cindy_server::{app, Config};

#[tokio::main]
async fn main() {
    let addr = std::env::var("CINDY_SERVER_ADDR").unwrap_or_else(|_| "127.0.0.1:8787".into());
    let db_path = std::env::var("CINDY_SERVER_DB").unwrap_or_else(|_| "./cindy-server.db".into());
    let allowed: Vec<String> = std::env::var("CINDY_SERVER_ALLOWED_EMAILS")
        .unwrap_or_default()
        .split(',')
        .map(|e| e.trim().to_lowercase())
        .filter(|e| !e.is_empty())
        .collect();
    if allowed.is_empty() {
        eprintln!("必须设置 CINDY_SERVER_ALLOWED_EMAILS（逗号分隔的允许登录邮箱），否则任何人都能注册。");
        std::process::exit(2);
    }
    let router = match app(Config { db_path, allowed_emails: allowed, fixed_code: None, preset_tokens: preset_from_env() }) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("启动失败：{e}");
            std::process::exit(1);
        }
    };
    let listener = match tokio::net::TcpListener::bind(&addr).await {
        Ok(l) => l,
        Err(e) => {
            eprintln!("监听 {addr} 失败：{e}");
            std::process::exit(1);
        }
    };
    eprintln!("[cindy-server] 监听 {addr}");
    let _ = axum::serve(listener, router).with_graceful_shutdown(async { let _ = tokio::signal::ctrl_c().await; }).await;
}

/// 仅供 e2e：`CINDY_SERVER_TEST_TOKENS` = JSON 数组 [[token, email, deviceId], ...]。
/// 只有同时设置 `CINDY_SERVER_TEST_MODE=1` 才生效，且服务只监听回环地址，避免生产环境被误用。
fn preset_from_env() -> Vec<(String, String, String)> {
    if std::env::var("CINDY_SERVER_TEST_MODE").as_deref() != Ok("1") {
        return vec![];
    }
    let addr = std::env::var("CINDY_SERVER_ADDR").unwrap_or_default();
    if !addr.starts_with("127.0.0.1:") {
        eprintln!("CINDY_SERVER_TEST_MODE 只允许监听 127.0.0.1");
        std::process::exit(2);
    }
    std::env::var("CINDY_SERVER_TEST_TOKENS").ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default()
}
