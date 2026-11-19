//! 自定义模型服务（model-config）的 Rust 实现。
pub mod classify;
pub mod config;
pub mod discovery;
pub mod host;
pub mod ipc;
pub mod manager;
pub mod metadata;
pub mod registry;
pub mod view;
#[cfg(test)]
mod view_tests;
#[cfg(test)]
mod e2e;
