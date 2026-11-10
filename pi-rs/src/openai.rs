// OpenAI Chat Completions 流式客户端（对应原版的 openai-completions 协议）。
// 只做 SSE 解析和消息格式转换；把增量通过回调交给 agent，由 agent 组装 assistant 消息。
use crate::models::Model;
use futures_util::StreamExt;
use serde_json::{json, Value};

pub enum Ev {
    Start(String),
    TextStart(usize),
    TextDelta(usize, String),
    TextEnd(usize, String),
    ToolStart(usize, String, String),
    ToolDelta(usize, String),
    ToolEnd(usize, Value),
    ThinkStart(usize),
    ThinkDelta(usize, String),
    ThinkEnd(usize, String),
    /// 流出错前的快照（已完成的内容块、usage、response id、原始 stop reason）。原版出错时保留这些，
    /// 所以流函数在每个返回 Err 的位置之前先发这个事件，由调用方存起来放进失败回复里。
    Partial(Vec<Value>, String, String),
}

pub struct Outcome {
    pub content: Vec<Value>,
    pub usage: Value,
    pub raw_stop: String,
    pub response_id: String,
}

pub fn to_wire(system: &str, msgs: &[Value], model: &Model) -> Vec<Value> {
    let mut out = vec![json!({"role":"system","content":system})];
    for m in msgs {
        let text_of = |c: &Value| -> String {
            match c {
                Value::String(s) => s.clone(),
                Value::Array(a) => a.iter().filter(|b| b["type"] == "text").filter_map(|b| b["text"].as_str()).collect::<Vec<_>>().join(""),
                _ => String::new(),
            }
        };
        match m["role"].as_str().unwrap_or("") {
            "user" => out.push(json!({"role":"user","content":text_of(&m["content"])})),
            "toolResult" => out.push(json!({"role":"tool","tool_call_id":m["toolCallId"],"content":text_of(&m["content"])})),
            "assistant" => {
                let calls: Vec<Value> = m["content"].as_array().into_iter().flatten().filter(|b| b["type"] == "toolCall")
                    .map(|b| json!({"id":b["id"],"type":"function","function":{"name":b["name"],"arguments":b["arguments"].to_string()}})).collect();
                let text = text_of(&m["content"]);
                let mut o = json!({"role":"assistant","content": if text.is_empty() && !calls.is_empty() { Value::Null } else { text.clone().into() }});
                // 思考内容回传：写回流里读到它的那个字段（reasoning_content / reasoning / reasoning_text）；
                // requiresThinkingAsText 的模型把思考拼进正文。
                let thinks: Vec<&Value> = m["content"].as_array().into_iter().flatten().filter(|b| b["type"] == "thinking" && b["thinking"].as_str().map_or(false, |t| !t.trim().is_empty())).collect();
                if !thinks.is_empty() {
                    if compat_flag(model, "requiresThinkingAsText").unwrap_or(false) {
                        let tt = thinks.iter().filter_map(|b| b["thinking"].as_str()).collect::<Vec<_>>().join("\n\n");
                        let mut parts = vec![json!({"type":"text","text":tt})];
                        if !text.is_empty() { parts.push(json!({"type":"text","text":text})); }
                        o["content"] = parts.into();
                    } else if let Some(sig) = thinks[0]["thinkingSignature"].as_str().filter(|s| ["reasoning_content", "reasoning", "reasoning_text"].contains(s)) {
                        o[sig] = thinks.iter().filter_map(|b| b["thinking"].as_str()).collect::<Vec<_>>().join("\n").into();
                    }
                }
                let is_ds = model.provider == "deepseek" || model.base_url.to_lowercase().contains("deepseek.com");
                if compat_flag(model, "requiresReasoningContentOnAssistantMessages").unwrap_or(is_ds) && model.reasoning && o.get("reasoning_content").is_none() {
                    o["reasoning_content"] = "".into();
                }
                if !calls.is_empty() {
                    o["tool_calls"] = calls.into();
                }
                out.push(o);
            }
            _ => {}
        }
    }
    out
}

fn compat_flag(m: &Model, k: &str) -> Option<bool> {
    m.compat.get(k).and_then(Value::as_bool)
}

/// 思考档位 → 请求字段，逐分支对应原版 buildParams 里的 thinkingFormat 判断。
fn thinking_params(body: &mut Value, m: &Model, level: &str) {
    if !m.reasoning { return; }
    let lvl = m.clamp_level(level);
    let on = lvl != "off";
    let u = m.base_url.to_lowercase();
    let p = m.provider.as_str();
    let is_deepseek = p == "deepseek" || u.contains("deepseek.com");
    let is_zai = p == "zai" || p == "zai-coding-cn" || u.contains("api.z.ai") || u.contains("open.bigmodel.cn");
    let is_together = p == "together" || u.contains("api.together.ai") || u.contains("api.together.xyz");
    let is_moonshot = p == "moonshotai" || p == "moonshotai-cn" || u.contains("api.moonshot.");
    let is_openrouter = p == "openrouter" || u.contains("openrouter.ai");
    let is_grok = p == "xai" || u.contains("api.x.ai");
    let is_cf_gw = p == "cloudflare-ai-gateway" || u.contains("gateway.ai.cloudflare.com");
    let is_nvidia = p == "nvidia" || u.contains("integrate.api.nvidia.com");
    let is_ling = p == "ant-ling" || u.contains("api.ant-ling.com");
    let fmt = m.compat.get("thinkingFormat").and_then(Value::as_str).map(String::from).unwrap_or_else(|| {
        if is_deepseek { "deepseek" } else if is_zai { "zai" } else if is_together { "together" } else if is_ling { "ant-ling" } else if is_openrouter { "openrouter" } else { "openai" }.into()
    });
    let supports_effort = compat_flag(m, "supportsReasoningEffort").unwrap_or(!is_grok && !is_zai && !is_moonshot && !is_together && !is_cf_gw && !is_nvidia && !is_ling);
    let off_unsupported = m.level_map.get("off").map_or(false, Value::is_null);
    let off_str = || m.level_map.get("off").and_then(Value::as_str).map(String::from);
    // thinkingLevelMap 命中字符串用它，未配置用档位名，null 则不发。
    let mapped = |l: &str| -> Option<String> { match m.level_map.get(l) { Some(Value::String(v)) => Some(v.clone()), Some(Value::Null) => None, _ => Some(l.to_string()) } };
    let eff = |body: &mut Value| { if on && supports_effort { if let Some(e) = mapped(&lvl) { body["reasoning_effort"] = e.into(); } } };
    match fmt.as_str() {
        "zai" => { body["thinking"] = if on { json!({"type":"enabled","clear_thinking":false}) } else { json!({"type":"disabled"}) }; eff(body); }
        "qwen" => { body["enable_thinking"] = on.into(); eff(body); }
        "qwen-chat-template" => { body["chat_template_kwargs"] = json!({"enable_thinking": on, "preserve_thinking": true}); }
        "deepseek" => {
            if on { body["thinking"] = json!({"type":"enabled"}); } else if !off_unsupported { body["thinking"] = json!({"type":"disabled"}); }
            eff(body);
        }
        "openrouter" => {
            if on { if let Some(e) = mapped(&lvl) { body["reasoning"] = json!({"effort": e}); } }
            else if !off_unsupported { body["reasoning"] = json!({"effort": off_str().unwrap_or_else(|| "none".into())}); }
        }
        "ant-ling" => { if on { if let Some(Value::String(e)) = m.level_map.get(lvl.as_str()) { body["reasoning"] = json!({"effort": e}); } } }
        "together" => { body["reasoning"] = json!({"enabled": on}); eff(body); }
        "string-thinking" => {
            if on { if let Some(e) = mapped(&lvl) { body["thinking"] = e.into(); } }
            else if !off_unsupported { body["thinking"] = off_str().unwrap_or_else(|| "none".into()).into(); }
        }
        _ => {
            if on && supports_effort { if let Some(e) = mapped(&lvl) { body["reasoning_effort"] = e.into(); } }
            else if !on && supports_effort { if let Some(o) = off_str() { body["reasoning_effort"] = o.into(); } }
        }
    }
}

pub async fn stream(
    client: &reqwest::Client,
    model: &Model,
    system: &str,
    msgs: &[Value],
    tools: &[Value],
    max_tokens: u64,
    level: &str,
    session_id: Option<&str>,
    on: &mut (dyn FnMut(Ev, &Value) + Send),
) -> Result<Outcome, String> {
    let mut body = json!({
        "model": model.id, "messages": to_wire(system, msgs, model), "stream": true,
        "stream_options": {"include_usage": true},
    });
    // max_tokens 字段名由 compat.maxTokensField 决定；默认 max_completion_tokens，非标准厂商用 max_tokens。
    let max_field = model.compat.get("maxTokensField").and_then(Value::as_str).map(String::from).unwrap_or_else(|| {
        let u = model.base_url.to_lowercase();
        let non_std = ["api.x.ai", "chutes.ai", "deepseek.com", "api.z.ai", "open.bigmodel.cn", "api.moonshot.", "opencode.ai", "api.together.", "cerebras.ai", "integrate.api.nvidia.com"].iter().any(|h| u.contains(h));
        if non_std { "max_tokens".into() } else { "max_completion_tokens".into() }
    });
    body[max_field.as_str()] = max_tokens.into();
    thinking_params(&mut body, model, level);
    // 缓存相关字段（compat 缺省值与原版 detectCompat 一致）。
    let u = model.base_url.to_lowercase();
    let non_standard = ["api.x.ai", "chutes.ai", "deepseek.com", "api.z.ai", "open.bigmodel.cn", "api.moonshot.", "opencode.ai", "api.together.", "cerebras.ai", "integrate.api.nvidia.com", "api.cloudflare.com", "gateway.ai.cloudflare.com", "api.ant-ling.com"].iter().any(|h| u.contains(h))
        || ["xai", "deepseek", "zai", "zai-coding-cn", "moonshotai", "moonshotai-cn", "nvidia", "cerebras", "together", "opencode", "ant-ling", "cloudflare-workers-ai", "cloudflare-ai-gateway"].contains(&model.provider.as_str());
    if compat_flag(model, "supportsStore").unwrap_or(!non_standard) { body["store"] = false.into(); }
    let long_ok = compat_flag(model, "supportsLongCacheRetention").unwrap_or(!(["api.together.", "api.cloudflare.com", "gateway.ai.cloudflare.com", "integrate.api.nvidia.com", "api.ant-ling.com"].iter().any(|h| u.contains(h))));
    let long = std::env::var("PI_CACHE_RETENTION").map_or(false, |v| v == "long");
    if let Some(sid) = session_id {
        if (u.contains("api.openai.com")) || (long && long_ok) {
            body["prompt_cache_key"] = sid.chars().take(64).collect::<String>().into();
        }
    }
    // session_id 为 None 表示 cacheRetention=none（摘要请求）：不带任何缓存字段。
    if long && long_ok && session_id.is_some() { body["prompt_cache_retention"] = "24h".into(); }
    if !tools.is_empty() {
        body["tools"] = tools.iter().map(|t| json!({"type":"function","function":{"name":t["name"],"description":t["description"],"parameters":t["parameters"]}})).collect();
    }
    let url = format!("{}/chat/completions", model.base_url.trim_end_matches('/'));
    let mut req = client.post(&url).json(&body);
    if let Some(k) = model.api_key() {
        req = req.bearer_auth(k);
    }
    for (k, v) in model.headers() {
        req = req.header(k, v);
    }
    let idle = crate::retry::idle_timeout();
    let resp = crate::retry::timed(idle, "response", req.send()).await?.map_err(|e| format!("Connection error: {e}"))?;
    if !resp.status().is_success() {
        let st = resp.status();
        let t = resp.text().await.unwrap_or_default();
        return Err(format!("{} {}", st.as_u16(), t.chars().take(500).collect::<String>()));
    }
    let mut st = resp.bytes_stream();
    let mut buf: Vec<u8> = vec![];
    let (mut text, mut text_idx): (Option<String>, usize) = (None, 0);
    // 工具调用：provider index -> (内容块序号, id, name, 参数串)
    let mut calls: Vec<(u64, usize, String, String, String)> = vec![];
    let mut next_idx = 0usize;
    let (mut raw_stop, mut rid, mut usage) = (String::new(), String::new(), Value::Null);
    let mut started = false;
    let mut saw_done = false;
    let mut think: Option<(usize, String, String)> = None; // (内容块序号, 来源字段, 文本)
    let mut finished: Vec<(usize, Value)> = vec![];
    // 出错时带出已收到的文本/思考（原版出错前会先把进行中的块收尾再抛错）；半截的工具调用丢弃。
    macro_rules! snapshot { () => {{
        let mut c = finished.iter().map(|(_, b)| b.clone()).collect::<Vec<_>>();
        if let Some((_, f, t)) = &think { c.push(json!({"type":"thinking","thinking":t,"thinkingSignature":f})); }
        if let Some(t) = &text { c.push(json!({"type":"text","text":t})); }
        on(Ev::Partial(c, rid.clone(), raw_stop.clone()), &usage);
    }}; }
    loop {
        let next = match crate::retry::timed(idle, "data", st.next()).await { Ok(n) => n, Err(e) => { snapshot!(); return Err(e); } };
        let Some(chunk) = next else { break };
        let chunk = match chunk { Ok(c) => c, Err(e) => { snapshot!(); return Err(format!("Stream was interrupted: {e}")); } };
        buf.extend_from_slice(&chunk);
        while let Some(p) = buf.iter().position(|b| *b == b'\n') {
            let line = String::from_utf8_lossy(&buf[..p]).trim_end_matches('\r').to_string();
            buf.drain(..=p);
            let Some(data) = line.strip_prefix("data:") else { continue };
            let data = data.trim();
            if data == "[DONE]" || data.is_empty() {
                if data == "[DONE]" { saw_done = true; }
                continue;
            }
            let Ok(j) = serde_json::from_str::<Value>(data) else { continue };
            if !started {
                started = true;
                rid = j["id"].as_str().unwrap_or("").into();
                on(Ev::Start(rid.clone()), &usage);
            }
            if j["usage"].is_object() {
                usage = j["usage"].clone();
            }
            let Some(ch) = j["choices"].get(0) else { continue };
            let rfield = ["reasoning_content", "reasoning", "reasoning_text"].into_iter().find(|f| ch["delta"][*f].as_str().map_or(false, |v| !v.is_empty()));
            if let Some(f) = rfield {
                let d = ch["delta"][f].as_str().unwrap_or("");
                if think.is_none() {
                    think = Some((next_idx, f.to_string(), String::new()));
                    on(Ev::ThinkStart(next_idx), &usage);
                    next_idx += 1;
                }
                let t = think.as_mut().unwrap();
                t.2.push_str(d);
                on(Ev::ThinkDelta(t.0, d.into()), &usage);
            }
            if let Some(c) = ch["delta"]["content"].as_str().filter(|s| !s.is_empty()) {
                if text.is_none() {
                    text = Some(String::new());
                    text_idx = next_idx;
                    next_idx += 1;
                    on(Ev::TextStart(text_idx), &usage);
                }
                text.as_mut().unwrap().push_str(c);
                on(Ev::TextDelta(text_idx, c.into()), &usage);
            }
            for tc in ch["delta"]["tool_calls"].as_array().into_iter().flatten() {
                let pi = tc["index"].as_u64().unwrap_or(0);
                if !calls.iter().any(|c| c.0 == pi) {
                    if let Some(t) = text.take() {
                        finished.push((text_idx, json!({"type":"text","text":t})));
                    }
                    let name = tc["function"]["name"].as_str().unwrap_or("").to_string();
                    let id = tc["id"].as_str().unwrap_or("").to_string();
                    on(Ev::ToolStart(next_idx, id.clone(), name.clone()), &usage);
                    calls.push((pi, next_idx, id, name, String::new()));
                    next_idx += 1;
                }
                let c = calls.iter_mut().find(|c| c.0 == pi).unwrap();
                let d = tc["function"]["arguments"].as_str().unwrap_or("");
                c.4.push_str(d);
                on(Ev::ToolDelta(c.1, d.into()), &usage);
            }
            if let Some(f) = ch["finish_reason"].as_str() {
                raw_stop = f.into();
            }
        }
    }
    if let Some(t) = text.take() {
        finished.push((text_idx, json!({"type":"text","text":t})));
    }
    if let Some((idx, f, t)) = think.take() {
        finished.push((idx, json!({"type":"thinking","thinking":t,"thinkingSignature":f})));
    }
    for (_, idx, id, name, args) in calls {
        let a: Value = if args.trim().is_empty() { json!({}) } else { serde_json::from_str(&args).unwrap_or_else(|_| json!({})) };
        let tc = json!({"type":"toolCall","id":id,"name":name,"arguments":a});
        finished.push((idx, tc));
    }
    if raw_stop.is_empty() && !saw_done {
        let mut c = finished.iter().map(|(_, b)| b.clone()).collect::<Vec<_>>(); c.sort_by_key(|b| b["type"] != "thinking");
        on(Ev::Partial(c, rid.clone(), raw_stop.clone()), &usage);
        return Err("Stream ended without finish_reason".into());
    }
    finished.sort_by_key(|(i, _)| *i);
    for (i, b) in &finished {
        if b["type"] == "thinking" {
            on(Ev::ThinkEnd(*i, b["thinking"].as_str().unwrap_or("").into()), &usage);
        } else if b["type"] == "text" {
            on(Ev::TextEnd(*i, b["text"].as_str().unwrap_or("").into()), &usage);
        } else {
            on(Ev::ToolEnd(*i, b.clone()), &usage);
        }
    }
    Ok(Outcome { content: finished.into_iter().map(|(_, b)| b).collect(), usage, raw_stop, response_id: rid })
}
