//! Cindy AI 云端模型（model-access）的 Rust 实现。
pub mod credsync;
pub mod manager;
pub mod models_sync;
#[cfg(test)]
mod credsync_tests;
pub mod xd_catalog;
#[cfg(test)]
mod xd_catalog_tests;
