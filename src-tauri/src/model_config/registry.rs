//! 随包模型注册表与 Pi 目录行：移植 modelRegistry.ts 的 nativeApi 解析、providerModelCatalog.ts 的行查询。
//! 数据是 scripts/app-settings/provider-entry.ts 从上游目录生成的 `provider_data.json`（编译期嵌入）。
use super::metadata::{self, Meta};
use crate::local_db::mapper::js_trim;
use crate::model_access::xd_catalog::Api;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::sync::OnceLock;

pub struct Entry {
    pub id: String,
    pub status: Option<String>,
    pub native_api: Api,
    pub mode: Option<String>,
    pub model_ref: Option<String>,
    pub routes: Vec<(String, String)>,
}

pub struct Base {
    pub id: String,
    pub aliases: Vec<String>,
    pub defaults: Meta,
}

pub struct Rule {
    pub provider_id: String,
    pub prefix: String,
    pub native_api: String,
}

/// 目录里的一行 provider 模型（只保留视图用得到的字段）。
pub struct Row {
    pub provider: String,
    pub id: String,
    pub name: String,
    pub upstream: String,
    pub api: String,
    pub off_null: bool,
    pub sig: u64,
    pub raw: Value,
}

pub struct Registry {
    pub schema_version: f64,
    pub entries: Vec<Entry>,
    pub bases: Vec<Base>,
    pub rules: Vec<Rule>,
    pub rows: Vec<Row>,
    by_endpoint_and_id: HashMap<String, Vec<usize>>,
    apis_by_endpoint: HashMap<String, HashSet<String>>,
    generation_rows: Vec<usize>,
    manufacturer_rows: Vec<usize>,
}

pub fn registry() -> &'static Registry {
    static R: OnceLock<Registry> = OnceLock::new();
    R.get_or_init(|| Registry::load(include_str!("provider_data.json")))
}

/// providerModelCatalog.ts 的 normalize：JS trim 后去掉尾部 `/`。
pub fn normalize(url: &str) -> String {
    js_trim(url).trim_end_matches('/').to_string()
}

fn api_of(v: &Value) -> Api {
    match v {
        Value::Null => Api::Null,
        Value::String(s) => Api::Known(s.clone()),
        _ => Api::Unknown,
    }
}

impl Registry {
    fn load(text: &str) -> Registry {
        let d: Value = serde_json::from_str(text).expect("provider_data.json");
        let s = |v: &Value| v.as_str().map(String::from);
        let entries = d["entries"].as_array().unwrap().iter().map(|e| Entry {
            id: s(&e["id"]).unwrap(),
            status: s(&e["status"]),
            native_api: api_of(&e["nativeApi"]),
            mode: s(&e["mode"]),
            model_ref: s(&e["modelRef"]),
            routes: e["routes"].as_array().unwrap().iter().map(|r| (s(&r["providerId"]).unwrap(), s(&r["modelId"]).unwrap())).collect(),
        }).collect();
        let bases = d["bases"].as_array().unwrap().iter().map(|b| Base {
            id: s(&b["id"]).unwrap(),
            aliases: b["aliases"].as_array().unwrap().iter().filter_map(|a| s(a)).collect(),
            defaults: b["defaults"].as_object().cloned().unwrap_or_default(),
        }).collect();
        let rules = d["rules"].as_array().unwrap().iter().map(|r| Rule {
            provider_id: s(&r["providerId"]).unwrap(),
            prefix: s(&r["modelIdPrefix"]).unwrap(),
            native_api: s(&r["nativeApi"]).unwrap(),
        }).collect();
        let rows: Vec<Row> = d["rows"].as_array().unwrap().iter().map(|r| Row {
            provider: s(&r["provider"]).unwrap(),
            id: s(&r["id"]).unwrap(),
            name: s(&r["name"]).unwrap_or_default(),
            upstream: s(&r["upstream"]).unwrap(),
            api: s(&r["api"]).unwrap(),
            off_null: r["offNull"] == true,
            sig: r["sig"].as_u64().unwrap(),
            raw: r.clone(),
        }).collect();
        let mut by: HashMap<String, Vec<usize>> = HashMap::new();
        let mut apis: HashMap<String, HashSet<String>> = HashMap::new();
        for (i, row) in rows.iter().enumerate() {
            let endpoint = normalize(&row.upstream);
            apis.entry(endpoint.clone()).or_default().insert(row.api.clone());
            let list = by.entry(format!("{endpoint}\n{}", row.id)).or_default();
            // 同一条记录在多个订阅目录里重复出现不算冲突；内容不同的记录必须保持歧义。
            if !list.iter().any(|&j| rows[j].sig == row.sig) {
                list.push(i);
            }
        }
        let generation_rows = (0..rows.len()).collect();
        let manufacturer_rows = ["openai", "anthropic", "google", "xai", "deepseek", "qwen"]
            .iter()
            .flat_map(|p| rows.iter().enumerate().filter(move |(_, r)| r.provider == *p).map(|(i, _)| i))
            .collect();
        Registry {
            schema_version: d["schemaVersion"].as_f64().unwrap_or(0.0),
            entries,
            bases,
            rules,
            rows,
            by_endpoint_and_id: by,
            apis_by_endpoint: apis,
            generation_rows,
            manufacturer_rows,
        }
    }

    // ── 注册表查询 ───────────────────────────────────────────────────────

    /// findBaseModel：id 或别名恰有一个命中才算。
    pub fn find_base_model(&self, identity: &str) -> Option<&Base> {
        let mut it = self.bases.iter().filter(|m| m.id == identity || m.aliases.iter().any(|a| a == identity));
        let first = it.next()?;
        it.next().is_none().then_some(first)
    }

    /// resolveModelNativeApi(registry, providerId, modelId, mode)
    pub fn resolve_model_native_api(&self, provider_id: &str, model_id: &str, mode: Option<&str>) -> Api {
        let raw = model_id.strip_suffix("[1m]").unwrap_or(model_id);
        let id: String = if provider_id == "openai" {
            raw.strip_prefix("chatgpt/").unwrap_or(raw).to_string()
        } else if provider_id == "xai" && !raw.starts_with("xai/") {
            format!("xai/{raw}")
        } else {
            raw.to_string()
        };
        let entries: Vec<&Entry> = self.entries.iter().filter(|e| e.routes.iter().any(|(p, m)| p == provider_id && *m == id)).collect();
        if entries.iter().any(|e| e.status.as_deref() == Some("retired") || e.native_api == Api::Null) {
            return Api::Null;
        }
        if self.schema_version < 3.0 {
            return Api::Unknown;
        }
        let mut explicit: Vec<&String> = vec![];
        for e in &entries {
            if let Api::Known(a) = &e.native_api {
                if !explicit.contains(&a) {
                    explicit.push(a);
                }
            }
        }
        if !explicit.is_empty() {
            return if explicit.len() == 1 { Api::Known(explicit[0].clone()) } else { Api::Null };
        }
        let effective: Option<String> = mode
            .map(String::from)
            .or_else(|| entries.iter().find_map(|e| e.mode.clone().or_else(|| e.model_ref.as_deref().and_then(|r| self.find_base_model(r)).and_then(|b| b.defaults.get("mode")).and_then(|m| m.as_str().map(String::from)))))
            .or_else(|| self.find_base_model(&id).and_then(|b| b.defaults.get("mode")).and_then(|m| m.as_str().map(String::from)));
        let chatty = matches!(effective.as_deref(), None | Some("chat") | Some("responses"));
        let wire = ["anthropic-messages", "openai-responses", "openai-completions", "google-generative-ai"];
        let mut best: Option<&Rule> = None;
        for r in &self.rules {
            if r.provider_id != provider_id || !id.starts_with(&r.prefix) || id[r.prefix.len()..].contains('/') {
                continue;
            }
            if !(chatty || !wire.contains(&r.native_api.as_str())) {
                continue;
            }
            if best.map_or(true, |b| r.prefix.len() > b.prefix.len()) {
                best = Some(r);
            }
        }
        match best {
            Some(r) => Api::Known(r.native_api.clone()),
            None => Api::Unknown,
        }
    }

    /// registryEntryDefaults(registry, entry).mode：条目自带 mode，否则基础模型的 mode。
    fn entry_mode(&self, e: &Entry) -> Option<String> {
        e.mode.clone()
    }

    /// resolveCatalogModelNativeApi(registry, modelId, mode)
    pub fn resolve_catalog_model_native_api(&self, model_id: &str, mode: Option<&str>) -> Api {
        let base = self.find_base_model(model_id);
        let entries: Vec<&Entry> = self.entries.iter().filter(|e| e.id == model_id || base.is_some_and(|b| e.id == b.id || e.model_ref.as_deref() == Some(&b.id))).collect();
        let target = entries.iter().find(|e| e.id == model_id);
        if target.is_some_and(|t| t.status.as_deref() == Some("retired")) {
            return Api::Null;
        }
        if self.schema_version < 3.0 {
            return Api::Unknown;
        }
        let live: Vec<&&Entry> = entries.iter().filter(|e| e.status.as_deref() != Some("retired")).collect();
        let mut decl: Vec<Api> = vec![];
        for e in &live {
            if e.native_api != Api::Unknown && !decl.contains(&e.native_api) {
                decl.push(e.native_api.clone());
            }
        }
        if !decl.is_empty() {
            return if decl.len() == 1 { decl.remove(0) } else { Api::Null };
        }
        let canonical = base.map_or(model_id, |b| b.id.as_str());
        let effective: Option<String> = mode.map(String::from).or_else(|| match target {
            Some(t) => self.entry_mode(t).or_else(|| t.model_ref.as_deref().and_then(|r| self.find_base_model(r)).and_then(|b| b.defaults.get("mode")).and_then(|m| m.as_str().map(String::from))),
            None => base.and_then(|b| b.defaults.get("mode")).and_then(|m| m.as_str().map(String::from)),
        });
        let declared = self.resolve_model_native_api("xd", canonical, effective.as_deref());
        if declared != Api::Unknown {
            return declared;
        }
        if canonical.contains('/') {
            return Api::Unknown;
        }
        let mut cands: Vec<(usize, Api)> = vec![];
        for r in &self.rules {
            let Some(slash) = r.prefix.rfind('/') else { continue };
            let prefix = &r.prefix[slash + 1..];
            if r.provider_id == "xd" && !prefix.is_empty() && canonical.starts_with(prefix) {
                let full = format!("{}{canonical}", &r.prefix[..slash + 1]);
                cands.push((metadata::utf16_len(prefix), self.resolve_model_native_api("xd", &full, effective.as_deref())));
            }
        }
        let longest = cands.iter().map(|c| c.0).max().unwrap_or(0);
        let mut matches: Vec<Api> = vec![];
        for (len, api) in &cands {
            if *len == longest && *api != Api::Unknown && !matches.contains(api) {
                matches.push(api.clone());
            }
        }
        match matches.len() {
            0 => Api::Unknown,
            1 => matches.remove(0),
            _ => Api::Null,
        }
    }

    // ── 目录行 ───────────────────────────────────────────────────────────

    /// providerModelRecord：精确路由身份（端点 + id），可按协议过滤；候选恰好一条才返回。
    pub fn provider_model_record(&self, model_id: &str, upstream: &str, protocol: Option<&str>, allow_mixed: bool) -> Option<usize> {
        let api = protocol.map(|p| if p == "openai-chat" { "openai-completions" } else { p });
        let endpoint = normalize(upstream);
        let mixed = allow_mixed && self.apis_by_endpoint.get(&endpoint).map_or(0, HashSet::len) > 1;
        let matches: Vec<usize> = self
            .by_endpoint_and_id
            .get(&format!("{endpoint}\n{model_id}"))?
            .iter()
            .copied()
            .filter(|&i| mixed || api.is_none() || Some(self.rows[i].api.as_str()) == api)
            .collect();
        (matches.len() == 1).then(|| matches[0])
    }

    /// providerModelGenerationRecord（presetId 缺省的受限情形）：新一代模型沿用旧代参数，但保留自己的连接与身份。
    pub fn provider_model_generation_record(&self, model_id: &str, upstream: &str, protocol: Option<&str>) -> Option<Generated> {
        let api = match protocol? {
            "openai-chat" => "openai-completions",
            p => p,
        };
        let endpoint = normalize(upstream);
        let endpoint_rows = self.generation_rows.iter().copied().filter(|&i| normalize(&self.rows[i].upstream) == endpoint && self.rows[i].api == api);
        let manufacturer = self.manufacturer_rows.iter().copied().filter(|&i| self.rows[i].api == api);
        let candidates: Vec<usize> = manufacturer.chain(endpoint_rows).collect();
        let previous = candidates
            .iter()
            .rev()
            .find(|&&i| self.rows[i].id == model_id)
            .copied()
            .or_else(|| metadata::previous_generations(model_id, &candidates, |&i| self.rows[i].id.clone()).last().copied())?;
        Some(Generated { row: previous, id: model_id.to_string(), inherited_from: self.rows[previous].id.clone() })
    }

    /// providerModelMetadata(row)：未校验的原始字段，消费方都会再过一遍 pick。
    pub fn row_metadata(&self, row: usize, name: Option<&str>, id_override: Option<&str>) -> Meta {
        let _ = id_override;
        let r = &self.rows[row];
        let mut m = Meta::new();
        let put = |m: &mut Meta, k: &str, v: Option<&Value>| {
            if let Some(v) = v {
                m.insert(k.to_string(), v.clone());
            }
        };
        put(&mut m, "name", Some(&json!(name.unwrap_or(&r.name))));
        put(&mut m, "contextWindow", r.raw.get("contextWindow"));
        if r.raw.get("maxOutput").is_some_and(|v| v.as_f64().is_some_and(|n| n != 0.0)) {
            put(&mut m, "maxOutputTokens", r.raw.get("maxOutput"));
        }
        put(&mut m, "modalities", r.raw.get("modalities"));
        put(&mut m, "supportsImageInput", r.raw.get("supportsImageInput"));
        put(&mut m, "supportsFastMode", r.raw.get("supportsFastMode"));
        put(&mut m, "efforts", r.raw.get("efforts"));
        put(&mut m, "defaultEffort", r.raw.get("defaultEffort"));
        if r.off_null {
            m.insert("reasoningRequired".into(), json!(true));
        }
        m
    }
}

/// 沿用旧代参数生成的记录（不带价格，名称是自己的 id）。
pub struct Generated {
    pub row: usize,
    pub id: String,
    pub inherited_from: String,
}
