// Anthropic Messages 流式客户端（api = "anthropic-messages"）。
// 请求形态以原版 1.4.0 对 mock 发出的请求为准（baseline/mock-anthropic.mjs 记录）：
// /v1/messages?beta=true、x-api-key、system 块数组、最后一个工具和最后一条用户消息带 cache_control。
// 未实现：thinking 请求参数（目前思考档位恒为 off）、服务端 fallback。
use crate::models::Model;
use crate::openai::{Ev, Outcome};
use futures_util::StreamExt;
use serde_json::{json, Value};

fn flag(m: &Model, k: &str, default: bool) -> bool {
    m.compat.get(k).and_then(Value::as_bool).unwrap_or(default)
}

fn text_of(c: &Value) -> String {
    crate::session::text_of(c)
}

fn user_blocks(c: &Value) -> Vec<Value> {
    match c {
        Value::String(s) if !s.is_empty() => vec![json!({"type":"text","text":s})],
        Value::Array(a) => a.iter().filter_map(|b| match b["type"].as_str() {
            Some("text") if b["text"].as_str().map_or(false, |t| !t.is_empty()) => Some(json!({"type":"text","text":b["text"]})),
            Some("image") => Some(json!({"type":"image","source":{"type":"base64","media_type":b["mimeType"],"data":b["data"]}})),
            _ => None,
        }).collect(),
        _ => vec![],
    }
}

pub fn build_messages(msgs: &[Value], cache: Option<&Value>) -> Vec<Value> {
    let mut out: Vec<Value> = vec![];
    for m in msgs {
        match m["role"].as_str().unwrap_or("") {
            "user" => {
                let b = user_blocks(&m["content"]);
                if !b.is_empty() {
                    out.push(json!({"role":"user","content":b}));
                }
            }
            "assistant" => {
                if matches!(m["stopReason"].as_str(), Some("error" | "aborted")) {
                    continue;
                }
                let mut blocks = vec![];
                for b in m["content"].as_array().into_iter().flatten() {
                    match b["type"].as_str() {
                        Some("text") if b["text"].as_str().map_or(false, |t| !t.trim().is_empty()) => blocks.push(json!({"type":"text","text":b["text"]})),
                        Some("toolCall") => blocks.push(json!({"type":"tool_use","id":b["id"],"name":b["name"],"input":b["arguments"]})),
                        Some("thinking") => {
                            let sig = b["thinkingSignature"].as_str().or(b["signature"].as_str()).unwrap_or("");
                            let t = b["thinking"].as_str().unwrap_or("");
                            if !sig.is_empty() {
                                blocks.push(json!({"type":"thinking","thinking":t,"signature":sig}));
                            } else if !t.trim().is_empty() {
                                blocks.push(json!({"type":"text","text":t}));
                            }
                        }
                        _ => {}
                    }
                }
                if !blocks.is_empty() {
                    out.push(json!({"role":"assistant","content":blocks}));
                }
            }
            "toolResult" => {
                let block = json!({"type":"tool_result","tool_use_id":m["toolCallId"],"content":text_of(&m["content"]),"is_error":m["isError"].as_bool().unwrap_or(false)});
                // 连续的工具结果合并进同一条 user 消息。
                let merge = out.last().map_or(false, |l| l["role"] == "user" && l["content"].as_array().map_or(false, |c| c.iter().all(|b| b["type"] == "tool_result")));
                if merge {
                    out.last_mut().unwrap()["content"].as_array_mut().unwrap().push(block);
                } else {
                    out.push(json!({"role":"user","content":[block]}));
                }
            }
            _ => {}
        }
    }
    if let Some(c) = cache {
        if let Some(last) = out.last_mut().filter(|l| l["role"] == "user") {
            if let Some(b) = last["content"].as_array_mut().and_then(|a| a.last_mut()) {
                if matches!(b["type"].as_str(), Some("text" | "image" | "tool_result")) {
                    b["cache_control"] = c.clone();
                }
            }
        }
    }
    out
}

struct Blk { pidx: u64, idx: usize, kind: &'static str, text: String, id: String, name: String, sig: String }

pub async fn stream(
    client: &reqwest::Client,
    model: &Model,
    system: &str,
    msgs: &[Value],
    tools: &[Value],
    max_tokens: u64,
    use_cache: bool,
    level: &str,
    msgs_all: &[Value],
    on: &mut (dyn FnMut(Ev, &Value) + Send),
) -> Result<Outcome, String> {
    let cache = if flag(model, "supportsLongCacheRetention", true) { json!({"type":"ephemeral","ttl":"1h"}) } else { json!({"type":"ephemeral"}) };
    let eager = flag(model, "supportsEagerToolInputStreaming", true);
    let tool_cache = flag(model, "supportsCacheControlOnTools", true);
    let mut body = json!({
        "model": model.id, "messages": build_messages(msgs, if use_cache { Some(&cache) } else { None }), "max_tokens": max_tokens, "stream": true,
        "system": [{"type":"text","text":system}],
    });
    if use_cache { body["system"][0]["cache_control"] = cache.clone(); }
    // 思考档位：off 时模型支持推理就显式 disabled；否则 adaptive（forceAdaptiveThinking）或带预算的 enabled。
    let mut betas: Vec<String> = vec![];
    let lvl = model.clamp_level(level);
    if model.reasoning {
        if lvl == "off" {
            if model.level_map.get("off").map_or(true, |v| !v.is_null()) { body["thinking"] = json!({"type":"disabled"}); }
        } else if flag(model, "forceAdaptiveThinking", false) {
            body["thinking"] = json!({"type":"adaptive","display":"summarized"});
            body["output_config"] = json!({"effort": match model.level_map.get(lvl.as_str()) { Some(Value::String(e)) => e.as_str(), _ => match lvl.as_str() { "minimal" | "low" => "low", "medium" => "medium", _ => "high" } }});
        } else {
            // adjustMaxTokensForThinking + clampMaxTokensToContext，然后预算不超过 max_tokens-1024。
            let custom = crate::retry::thinking_budgets();
            let mut budget = crate::models::thinking_budget(&lvl, &custom);
            let adj = model.max_tokens.min(max_tokens + budget);
            if adj <= budget { budget = budget.min(adj.saturating_sub(1024)); }
            let mt = crate::compact::clamp_to_context(model, msgs_all, adj);
            let b = budget.min(mt.saturating_sub(1024));
            body["max_tokens"] = mt.into();
            body["thinking"] = json!({"type":"enabled","budget_tokens": if b == 0 { 1024 } else { b },"display":"summarized"});
            betas.push("interleaved-thinking-2025-05-14".into());
        }
    }
    if !tools.is_empty() {
        let mut ts: Vec<Value> = tools.iter().map(|t| {
            let mut o = json!({"name":t["name"],"description":t["description"]});
            if eager { o["eager_input_streaming"] = true.into(); }
            o["input_schema"] = t["parameters"].clone();
            o
        }).collect();
        if tool_cache && use_cache { if let Some(l) = ts.last_mut() { l["cache_control"] = cache.clone(); } }
        body["tools"] = ts.into();
    }
    let url = format!("{}/v1/messages?beta=true", model.base_url.trim_end_matches('/'));
    let mut req = client.post(&url).json(&body)
        .header("anthropic-version", "2023-06-01").header("anthropic-dangerous-direct-browser-access", "true")
        .header("user-agent", concat!("pi-rs/", env!("CARGO_PKG_VERSION")));
    if let Some(k) = model.api_key() { req = req.header("x-api-key", k); }
    if !betas.is_empty() { req = req.header("anthropic-beta", betas.join(",")); }
    for (k, v) in model.headers() { req = req.header(k, v); }
    let idle = crate::retry::idle_timeout();
    let resp = crate::retry::timed(idle, "response", req.send()).await?.map_err(|e| format!("Connection error: {e}"))?;
    if !resp.status().is_success() {
        let st = resp.status();
        let t = resp.text().await.unwrap_or_default();
        let msg = t;
        return Err(format!("{} {}", st.as_u16(), msg.chars().take(500).collect::<String>()));
    }
    let mut st = resp.bytes_stream();
    let mut buf: Vec<u8> = vec![];
    let (mut rid, mut raw_stop) = (String::new(), String::new());
    let mut usage = json!({"input_tokens":0,"output_tokens":0,"cache_read_input_tokens":0,"cache_creation_input_tokens":0});
    let mut blocks: Vec<Blk> = vec![];
    let mut content: Vec<Value> = vec![];
    let mut next_idx = 0usize;
    let mut done = false;
    // 出错时带出已收到的内容：已完成的块 + 进行到一半的块（去掉 partialJson 的文本/思考；未完成的工具调用丢弃，
    // 原版保留它的半截参数，但那是不可用的调用，宿主也不会执行——失败回复不会触发工具）。
    macro_rules! snapshot { () => {{
        let mut c = content.clone();
        for b in &blocks {
            match b.kind {
                "thinking" => c.push(json!({"type":"thinking","thinking":b.text,"thinkingSignature":b.sig})),
                "text" => c.push(json!({"type":"text","text":b.text})),
                _ => {}
            }
        }
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
            let Ok(j) = serde_json::from_str::<Value>(data.trim()) else { continue };
            let merge = |usage: &mut Value, u: &Value| {
                for k in ["input_tokens", "output_tokens", "cache_read_input_tokens", "cache_creation_input_tokens"] {
                    if let Some(n) = u[k].as_u64() { usage[k] = n.into(); }
                }
            };
            match j["type"].as_str().unwrap_or("") {
                "message_start" => {
                    rid = j["message"]["id"].as_str().unwrap_or("").into();
                    merge(&mut usage, &j["message"]["usage"]);
                    on(Ev::Start(rid.clone()), &usage);
                }
                "content_block_start" => {
                    let cb = &j["content_block"];
                    let pidx = j["index"].as_u64().unwrap_or(0);
                    let idx = next_idx;
                    next_idx += 1;
                    let mut b = Blk { pidx, idx, kind: "text", text: String::new(), id: String::new(), name: String::new(), sig: String::new() };
                    match cb["type"].as_str() {
                        Some("tool_use") => {
                            b.kind = "tool";
                            b.id = cb["id"].as_str().unwrap_or("").into();
                            b.name = cb["name"].as_str().unwrap_or("").into();
                            on(Ev::ToolStart(idx, b.id.clone(), b.name.clone()), &usage);
                        }
                        Some("thinking") | Some("redacted_thinking") => { b.kind = "thinking"; on(Ev::ThinkStart(idx), &usage); }
                        _ => on(Ev::TextStart(idx), &usage),
                    }
                    blocks.push(b);
                }
                "content_block_delta" => {
                    let pidx = j["index"].as_u64().unwrap_or(0);
                    let Some(b) = blocks.iter_mut().find(|b| b.pidx == pidx) else { continue };
                    let d = &j["delta"];
                    match d["type"].as_str() {
                        Some("text_delta") => { let t = d["text"].as_str().unwrap_or(""); b.text.push_str(t); on(Ev::TextDelta(b.idx, t.into()), &usage); }
                        Some("input_json_delta") => { let t = d["partial_json"].as_str().unwrap_or(""); b.text.push_str(t); on(Ev::ToolDelta(b.idx, t.into()), &usage); }
                        Some("thinking_delta") => { let t = d["thinking"].as_str().unwrap_or(""); b.text.push_str(t); on(Ev::ThinkDelta(b.idx, t.into()), &usage); }
                        Some("signature_delta") => b.sig.push_str(d["signature"].as_str().unwrap_or("")),
                        _ => {}
                    }
                }
                "content_block_stop" => {
                    let pidx = j["index"].as_u64().unwrap_or(0);
                    let Some(pos) = blocks.iter().position(|b| b.pidx == pidx) else { continue };
                    let b = blocks.remove(pos);
                    match b.kind {
                        "tool" => {
                            let args: Value = if b.text.trim().is_empty() { json!({}) } else { serde_json::from_str(&b.text).unwrap_or_else(|_| json!({})) };
                            let tc = json!({"type":"toolCall","id":b.id,"name":b.name,"arguments":args});
                            on(Ev::ToolEnd(b.idx, tc.clone()), &usage);
                            content.push(tc);
                        }
                        "thinking" => {
                            on(Ev::ThinkEnd(b.idx, b.text.clone()), &usage);
                            content.push(json!({"type":"thinking","thinking":b.text,"thinkingSignature":b.sig}));
                        }
                        _ => {
                            on(Ev::TextEnd(b.idx, b.text.clone()), &usage);
                            content.push(json!({"type":"text","text":b.text}));
                        }
                    }
                }
                "message_delta" => {
                    if let Some(s) = j["delta"]["stop_reason"].as_str() { raw_stop = s.into(); }
                    merge(&mut usage, &j["usage"]);
                }
                "message_stop" => done = true,
                "error" => { snapshot!(); return Err(j["error"]["message"].as_str().unwrap_or("provider error").to_string()); }
                _ => {}
            }
        }
    }
    if !done { snapshot!(); return Err("Anthropic stream ended before message_stop".into()); }
    Ok(Outcome { content, usage, raw_stop, response_id: rid })
}
