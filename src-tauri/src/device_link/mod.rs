//! 设备互联（device-link）被控端的 Rust 实现。
//! 纯协议层（允许列表 / topic 路由 / 可靠传输编解码）逐项对照上游 `packages/device-link` 的黄金数据。
pub mod allowlist;
pub mod host;
#[cfg(test)]
mod e2e;
pub mod model_speed;
pub mod runtime;
pub mod topics;
pub mod transport;
#[cfg(test)]
mod golden_tests;
pub mod engine;
