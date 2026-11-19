// Cindy 账号登录（auth.mjs 的 Rust 版）。纯逻辑与金样数据对照，HTTP 与会话编排在后续文件。
pub mod backoff;
pub mod client;
pub mod env;
pub mod flow;
pub mod hosted;
pub mod manager;
pub mod validate;
pub mod wire;
#[cfg(test)]
mod client_tests;
#[cfg(test)]
mod manager_tests;
#[cfg(test)]
mod tests;
