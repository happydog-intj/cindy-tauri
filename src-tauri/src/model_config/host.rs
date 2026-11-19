//! 原 sidecar 的模型相关 `host.*` 回调：Rust 会话宿主、桌面导入直接向这里要答案。
use super::manager::Manager as Models;
use crate::model_access::manager::Manager as Access;
use serde_json::{json, Value};
use std::sync::Arc;

pub struct ModelHost {
    pub access: Arc<Access>,
    pub models: Arc<Models>,
}

impl ModelHost {
    pub fn new(access: Arc<Access>, models: Arc<Models>) -> Arc<Self> {
        Arc::new(Self { access, models })
    }

    /// None = 不是模型相关的方法（调用方转给 sidecar）。
    pub fn call(&self, method: &str, p: &Value) -> Option<Result<Value, String>> {
        Some(match method {
            "host.resolve-launch" => self.models.resolve_launch(p["providerId"].as_str(), p["model"].as_str()),
            "host.pi-models" => {
                self.models.when_secrets_ready();
                Ok(json!(self.models.pi_model_descriptors()))
            }
            "host.gateway-rejected" => {
                self.access.gateway_key_rejected();
                Ok(Value::Null)
            }
            "host.secrets-ready" => Ok(json!(self.models.when_secrets_ready())),
            "host.has-provider" => Ok(json!(self.models.has_custom_provider(p["id"].as_str().unwrap_or("")))),
            "host.import-provider" => tauri::async_runtime::block_on(self.models.import_provider(&p["input"], p["apiKey"].as_str())).map(|s| json!(s)),
            _ => return None,
        })
    }
}
