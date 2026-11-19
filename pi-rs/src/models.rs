// 模型注册表：读 <agentDir>/models.json 里的自定义 provider（Cindy 只用这一种来源）。
// 字段默认值照抄原版 provider-composer.ts：name=id、reasoning=false、input=["text"]、
// cost 全 0、contextWindow=128000、maxTokens=16384。
use crate::util::resolve_config_value;
use serde_json::{json, Map, Value};
use std::path::PathBuf;

#[derive(Debug, Clone)]
pub struct Model {
    pub id: String,
    pub name: String,
    pub api: String,
    pub provider: String,
    pub base_url: String,
    pub reasoning: bool,
    pub input: Vec<String>,
    pub cost: Value,
    pub context_window: u64,
    pub max_tokens: u64,
    /// 以下是请求时才用到的，不出现在 RPC 输出的 model 对象里。
    pub api_key_raw: Option<String>,
    /// apiKeyFile：从文件读 key（每次请求现读，父进程改写文件即可轮换，无需重启会话）。
    /// 这是 Rust 宿主直接引导会话后的凭据供给方式，也供网关 key 轮换用；空文件等同缺 key。
    pub api_key_file: Option<PathBuf>,
    /// apiKeyHeaders：把 key 写进这些请求头（网关要同时认 authorization 与 x-api-key 时用）。
    /// 空表示走协议默认头（openai: Authorization: Bearer，anthropic: x-api-key）。
    /// key 仍每次请求现读，所以轮换对这些头同样生效。
    pub api_key_headers: Vec<String>,
    pub headers_raw: Map<String, Value>,
    pub compat: Value,
    /// thinkingLevelMap：级别 → 请求里的取值；null 表示该档位不支持，xhigh/max 只有显式给出才可用。
    pub level_map: Value,
}

pub const LEVELS: [&str; 7] = ["off", "minimal", "low", "medium", "high", "xhigh", "max"];

impl Model {
    /// getSupportedThinkingLevels：不支持推理的模型只有 off。
    pub fn levels(&self) -> Vec<&'static str> {
        if !self.reasoning { return vec!["off"]; }
        LEVELS.iter().copied().filter(|l| {
            let m = &self.level_map[*l];
            if self.level_map.get(*l).map_or(false, Value::is_null) { return false; }
            if *l == "xhigh" || *l == "max" { return !m.is_null() && self.level_map.get(*l).is_some(); }
            true
        }).collect()
    }

    /// clampThinkingLevel：请求的档位不可用时，先往高档找，再往低档找。
    pub fn clamp_level(&self, level: &str) -> String {
        let av = self.levels();
        if av.contains(&level) { return level.into(); }
        let Some(i) = LEVELS.iter().position(|l| *l == level) else { return av.first().copied().unwrap_or("off").into() };
        for c in &LEVELS[i..] { if av.contains(c) { return (*c).into(); } }
        for c in LEVELS[..i].iter().rev() { if av.contains(c) { return (*c).into(); } }
        av.first().copied().unwrap_or("off").into()
    }


    /// RPC 里的 model 对象（get_state / set_model / get_available_models），字段顺序与原版一致。
    pub fn to_json(&self) -> Value {
        json!({
            "id": self.id, "name": self.name, "api": self.api, "provider": self.provider,
            "baseUrl": self.base_url, "reasoning": self.reasoning, "input": self.input,
            "cost": self.cost, "contextWindow": self.context_window, "maxTokens": self.max_tokens,
        })
    }

    /// api_key()：每次请求调用（anthropic.rs / openai.rs 的请求构造里），所以
    /// apiKeyFile 的轮换与 $ENV 的重读都在这里生效。空文件等同缺 key。
    pub fn api_key(&self) -> Option<String> {
        if let Some(path) = &self.api_key_file {
            // 每次请求现读：父进程改写文件即可轮换 key（与 JS 转发器 getUpstream 同语义）。
            let k = std::fs::read_to_string(path).map(|s| s.trim().to_string()).unwrap_or_default();
            return (!k.is_empty()).then_some(k);
        }
        self.api_key_raw.as_deref().map(resolve_config_value).filter(|k| !k.is_empty())
    }

    /// 认证头：apiKeyHeaders 非空时按列表写（authorization 自动带 Bearer 前缀），
    /// 否则返回 None，调用方走各自协议的默认头。无 key 时返回空列表（不发空凭据）。
    pub fn auth_headers(&self) -> Option<Vec<(String, String)>> {
        if self.api_key_headers.is_empty() {
            return None;
        }
        let Some(k) = self.api_key() else { return Some(Vec::new()) };
        Some(
            self.api_key_headers
                .iter()
                .map(|h| if h == "authorization" { (h.clone(), format!("Bearer {k}")) } else { (h.clone(), k.clone()) })
                .collect(),
        )
    }

    pub fn headers(&self) -> Vec<(String, String)> {
        self.headers_raw
            .iter()
            .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), resolve_config_value(s))))
            .collect()
    }
}

#[derive(Debug, Default, Clone)]
pub struct Registry {
    pub models: Vec<Model>,
}

impl Registry {
    pub fn load(agent_dir: &std::path::Path) -> Result<Registry, String> {
        let path = agent_dir.join("models.json");
        let text = match std::fs::read_to_string(&path) {
            Ok(t) => t,
            Err(_) => return Ok(Registry::default()),
        };
        let root: Value = serde_json::from_str(&text).map_err(|e| format!("Invalid {}: {e}", path.display()))?;
        let mut models = Vec::new();
        if let Some(providers) = root.get("providers").and_then(Value::as_object) {
            for (pid, p) in providers {
                let base_url = p.get("baseUrl").and_then(Value::as_str).unwrap_or_default();
                let api = p.get("api").and_then(Value::as_str).unwrap_or("openai-completions");
                for m in p.get("models").and_then(Value::as_array).into_iter().flatten() {
                    let Some(id) = m.get("id").and_then(Value::as_str) else { continue };
                    let base_url = m.get("baseUrl").and_then(Value::as_str).filter(|u| !u.is_empty()).unwrap_or(base_url);
                    if base_url.is_empty() {
                        return Err(format!("Provider {pid}: \"baseUrl\" is required when defining custom models."));
                    }
                    models.push(Model {
                        id: id.to_string(),
                        name: m.get("name").and_then(Value::as_str).unwrap_or(id).to_string(),
                        api: m.get("api").and_then(Value::as_str).unwrap_or(api).to_string(),
                        provider: pid.clone(),
                        base_url: base_url.to_string(),
                        reasoning: m.get("reasoning").and_then(Value::as_bool).unwrap_or(false),
                        input: m
                            .get("input")
                            .and_then(Value::as_array)
                            .map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect())
                            .unwrap_or_else(|| vec!["text".into()]),
                        cost: m
                            .get("cost")
                            .cloned()
                            .unwrap_or_else(|| json!({"input":0,"output":0,"cacheRead":0,"cacheWrite":0})),
                        context_window: m.get("contextWindow").and_then(Value::as_u64).unwrap_or(128000),
                        max_tokens: m.get("maxTokens").and_then(Value::as_u64).unwrap_or(16384),
                        api_key_raw: p.get("apiKey").and_then(Value::as_str).map(String::from),
                        // apiKeyFile：模型级覆盖 provider 级；模型级显式 null 表示覆盖掉 provider 级。
                        api_key_file: match m.get("apiKeyFile") {
                            Some(Value::Null) => None,
                            Some(v) => v
                                .as_str()
                                .filter(|s| !s.is_empty())
                                .map(PathBuf::from)
                                .or_else(|| p.get("apiKeyFile").and_then(Value::as_str).filter(|s| !s.is_empty()).map(PathBuf::from)),
                            None => p.get("apiKeyFile").and_then(Value::as_str).filter(|s| !s.is_empty()).map(PathBuf::from),
                        },
                        api_key_headers: p
                            .get("apiKeyHeaders")
                            .and_then(Value::as_array)
                            .map(|a| a.iter().filter_map(|x| x.as_str().map(|s| s.to_ascii_lowercase())).filter(|s| !s.is_empty()).collect())
                            .unwrap_or_default(),
                        headers_raw: p.get("headers").and_then(Value::as_object).cloned().unwrap_or_default(),
                        compat: merge_compat(p.get("compat"), m.get("compat")),
                        level_map: m.get("thinkingLevelMap").cloned().unwrap_or(Value::Null),
                    });
                }
            }
        }
        Ok(Registry { models })
    }

    pub fn find(&self, provider: &str, id: &str) -> Option<&Model> {
        self.models.iter().find(|m| m.provider == provider && m.id == id)
    }
}

fn merge_compat(a: Option<&Value>, b: Option<&Value>) -> Value {
    let mut out = a.and_then(Value::as_object).cloned().unwrap_or_default();
    if let Some(bm) = b.and_then(Value::as_object) {
        for (k, v) in bm {
            out.insert(k.clone(), v.clone());
        }
    }
    Value::Object(out)
}

pub const THINKING_BUDGETS: [(&str, u64); 4] = [("minimal", 1024), ("low", 2048), ("medium", 8192), ("high", 16384)];

/// thinkingBudgetForLevel：xhigh/max 按 high 算；settings 的 thinkingBudgets 可覆盖。
pub fn thinking_budget(level: &str, custom: &Value) -> u64 {
    let l = if level == "xhigh" || level == "max" { "high" } else { level };
    custom[l].as_u64().or_else(|| THINKING_BUDGETS.iter().find(|(k, _)| *k == l).map(|(_, v)| *v)).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_models(dir: &std::path::Path, body: &str) {
        std::fs::write(dir.join("models.json"), body).unwrap();
    }

    // apiKeyFile：加载 → 每次请求现读 → 改写文件即轮换 → 空文件等同缺 key。
    #[test]
    fn api_key_file_read_per_request_and_rotation() {
        let dir = std::env::temp_dir().join(format!("pi-rs-keyfile-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let key_file = dir.join("gateway.key");
        std::fs::write(&key_file, " key-v1 \n").unwrap();
        write_models(
            &dir,
            r#"{"providers":{"cindy":{"baseUrl":"http://127.0.0.1:1","apiKeyFile":"GATEWAY_KEY_FILE_PLACEHOLDER","api":"openai-completions","models":[{"id":"m1"}]}}}"#,
        );
        // apiKeyFile 值应是绝对路径；占位串会被当相对路径读不到 → None。先测绝对路径行为。
        let reg = Registry::load(&dir).unwrap();
        assert_eq!(reg.models.len(), 1);
        let m = &reg.models[0];
        // 相对路径读不到 → None（占位场景）。
        assert_eq!(m.api_key(), None);

        // 绝对路径 + 每次请求现读 + 轮换。
        let key_file = dir.join("rotating.key");
        std::fs::write(&key_file, "key-v1").unwrap();
        let mut m = reg.models[0].clone();
        m.api_key_file = Some(key_file.clone());
        assert_eq!(m.api_key().as_deref(), Some("key-v1"), "trim 两侧空白");
        std::fs::write(&key_file, "key-v2").unwrap();
        assert_eq!(m.api_key().as_deref(), Some("key-v2"), "改写文件即轮换");
        std::fs::write(&key_file, "   \n").unwrap();
        assert_eq!(m.api_key(), None, "空文件等同缺 key");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    // apiKeyFile 是 provider 级字段，模型级可覆盖；env 插值行为不变；to_json 不含 key 字段。
    #[test]
    fn api_key_precedence_and_env_still_work() {
        let dir = std::env::temp_dir().join(format!("pi-rs-keyfile2-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let kf = dir.join("m1.key");
        std::fs::write(&kf, "from-file").unwrap();
        write_models(
            &dir,
            &format!(
                r#"{{"providers":{{"cindy":{{"baseUrl":"http://127.0.0.1:1","apiKey":"$CINDY_TEST_KEY_X","apiKeyFile":"{}","api":"openai-completions","models":[{{"id":"m1"}},{{"id":"m2","apiKeyFile":null}}]}}}}}}"#,
                kf.display().to_string().replace('\\', "\\\\")
            ),
        );
        let reg = Registry::load(&dir).unwrap();
        let m1 = &reg.models[0];
        // 文件优先于 env 插值（apiKeyFile 是显式供给方式）。
        assert_eq!(m1.api_key().as_deref(), Some("from-file"));
        // 模型级 null 覆盖 provider 级 → 回退 env 插值。
        std::env::set_var("CINDY_TEST_KEY_X", "from-env");
        let m2 = &reg.models[1];
        assert_eq!(m2.api_key().as_deref(), Some("from-env"));
        std::env::remove_var("CINDY_TEST_KEY_X");
        // RPC 输出不含任何 key 形状。
        let j = m1.to_json();
        assert!(j.to_string().find("key").is_none(), "{j}");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    // apiKeyHeaders：key 写进指定头（authorization 自动带 Bearer），仍每次请求现读，所以轮换对每个头都生效。
    #[test]
    fn api_key_headers_follow_the_key_file_and_default_to_protocol_header() {
        let dir = std::env::temp_dir().join(format!("pi-rs-keyheaders-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let key_file = dir.join("gw.key");
        std::fs::write(&key_file, "k1").unwrap();
        let body = format!(
            r#"{{"providers":{{"cindy":{{"baseUrl":"http://127.0.0.1:1","apiKeyFile":{},"apiKeyHeaders":["Authorization","X-Api-Key"],"api":"openai-completions","models":[{{"id":"m1"}}]}},"plain":{{"baseUrl":"http://127.0.0.1:1","apiKey":"sk-x","api":"openai-completions","models":[{{"id":"m2"}}]}}}}}}"#,
            serde_json::to_string(&key_file.to_string_lossy()).unwrap()
        );
        write_models(&dir, &body);
        let reg = Registry::load(&dir).unwrap();
        let gw = reg.find("cindy", "m1").unwrap();
        assert_eq!(
            gw.auth_headers(),
            Some(vec![("authorization".into(), "Bearer k1".into()), ("x-api-key".into(), "k1".into())])
        );
        std::fs::write(&key_file, "k2").unwrap();
        assert_eq!(gw.auth_headers().unwrap()[1].1, "k2", "轮换后新 key 立刻写进每个头");
        std::fs::write(&key_file, "").unwrap();
        assert_eq!(gw.auth_headers(), Some(Vec::new()), "没有 key 就不发空凭据");
        assert_eq!(reg.find("plain", "m2").unwrap().auth_headers(), None, "未配置时走协议默认头");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
