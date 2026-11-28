//! 自用的最小 Cindy 后端：邮箱验证码登录 + 设备互联中继。
pub mod auth;
pub mod db;
pub mod relay;

use axum::Router;
use std::sync::Arc;

pub struct Config {
    pub db_path: String,
    pub allowed_emails: Vec<String>,
    /// 测试用：固定验证码。生产必须为 None。
    pub fixed_code: Option<String>,
    /// 测试用：预置的 (访问令牌, 邮箱, 设备 id)，让外部 e2e 不必走验证码流程。生产必须为空。
    pub preset_tokens: Vec<(String, String, String)>,
}

pub fn app(cfg: Config) -> Result<Router, String> {
    let db = Arc::new(db::Db::open(&cfg.db_path)?);
    for (token, email, device) in &cfg.preset_tokens {
        let user = db.user_for_email(email)?;
        db.issue(&user, device, token, i64::MAX / 2, &format!("rt-preset-{token}"))?;
    }
    let hub = Arc::new(relay::Hub::new(db.clone()));
    let auth = Arc::new(auth::AuthState::new(db, cfg.allowed_emails, cfg.fixed_code));
    Ok(auth::routes(auth).merge(relay::routes(hub)))
}
