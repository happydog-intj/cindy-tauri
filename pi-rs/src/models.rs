// 模型注册表：读 <agentDir>/models.json 里的自定义 provider（Cindy 只用这一种来源）。
// 字段默认值照抄原版 provider-composer.ts：name=id、reasoning=false、input=["text"]、
// cost 全 0、contextWindow=128000、maxTokens=16384。
use crate::util::resolve_config_value;
use serde_json::{json, Map, Value};

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

    pub fn api_key(&self) -> Option<String> {
        self.api_key_raw.as_deref().map(resolve_config_value).filter(|k| !k.is_empty())
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
