// Agent 循环：一次 prompt → 若干轮（模型回复 + 工具执行）→ agent_end → agent_settled。
// 事件顺序和字段以原版 1.4.0 的录制为准（baseline/orig.jsonl）。
use crate::models::Model;
use crate::openai::{self, Ev};
use crate::session::Session;
use crate::util::{emit_line, now_ms};
use serde_json::{json, Value};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use tokio::sync::Notify;

pub struct Cfg {
    pub cwd: String,
    pub tools_allow: Option<Vec<String>>,
    pub append: Vec<String>,
    /// 加载了 cindy-bridge 扩展：启用权限门和 bash 默认超时（原版里这两项由该扩展提供）。
    pub bridge: bool,
    /// 是否加载 AGENTS.md / CLAUDE.md（--no-context-files 关闭）。
    pub context_files: bool,
    /// 纯文本轮策略的令牌（CINDY_PI_TURN_TOOL_POLICY），只在带 cindy-bridge 时有效。
    pub text_only_token: Option<String>,
}

pub struct State {
    pub session: Session,
    pub model: Option<Model>,
    pub thinking: String,
    pub streaming: bool,
    pub steering: VecDeque<String>,
    pub follow_up: VecDeque<String>,
    pub steering_mode: String,
    pub follow_up_mode: String,
    pub auto_compaction: bool,
    pub auto_retry: bool,
    pub system_emitted: bool,
    pub abort: Option<Arc<Notify>>,
    pub task: Option<tokio::task::JoinHandle<()>>,
    pub compact_abort: Option<Arc<Notify>>,
    /// 宿主发起的纯文本轮：本轮内所有工具调用被拒绝（agent 结束时清除）。
    pub text_only: bool,
}

pub struct Shared {
    pub st: Mutex<State>,
    pub cfg: Cfg,
    pub client: reqwest::Client,
    pub models: crate::models::Registry,
    pub session_dir: Option<String>,
    pub ui: Mutex<std::collections::HashMap<String, tokio::sync::oneshot::Sender<Value>>>,
}

fn zero_usage() -> Value {
    json!({"input":0,"output":0,"cacheRead":0,"cacheWrite":0,"totalTokens":0,"cost":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0,"total":0}})
}

/// provider 的 usage → Pi 的 usage（含按每百万 token 单价算出的 cost）。
fn map_usage(u: &Value, model: &Model) -> Value {
    if !u.is_object() {
        return zero_usage();
    }
    if u.get("input_tokens").is_some() {
        // Anthropic 的 usage：input 不含缓存读写，总数是四项之和。
        let g = |k: &str| u[k].as_u64().unwrap_or(0);
        let (i, o, cr, cw) = (g("input_tokens"), g("output_tokens"), g("cache_read_input_tokens"), g("cache_creation_input_tokens"));
        let c = |k: &str| model.cost[k].as_f64().unwrap_or(0.0);
        let (ci, co, ccr, ccw) = (i as f64 * c("input") / 1e6, o as f64 * c("output") / 1e6, cr as f64 * c("cacheRead") / 1e6, cw as f64 * c("cacheWrite") / 1e6);
        return json!({"input":i,"output":o,"cacheRead":cr,"cacheWrite":cw,"totalTokens":i+o+cr+cw,
            "cost":{"input":ci,"output":co,"cacheRead":ccr,"cacheWrite":ccw,"total":ci+co+ccr+ccw},"cacheWrite1h":0});
    }
    let g = |v: &Value| v.as_u64().unwrap_or(0);
    let cached = g(&u["prompt_tokens_details"]["cached_tokens"]);
    let input = g(&u["prompt_tokens"]).saturating_sub(cached);
    let output = g(&u["completion_tokens"]);
    let reasoning = g(&u["completion_tokens_details"]["reasoning_tokens"]);
    let total = if u["total_tokens"].is_u64() { g(&u["total_tokens"]) } else { input + output + cached };
    let c = |k: &str| model.cost[k].as_f64().unwrap_or(0.0);
    let (ci, co, cr) = (input as f64 * c("input") / 1e6, output as f64 * c("output") / 1e6, cached as f64 * c("cacheRead") / 1e6);
    json!({"input":input,"output":output,"cacheRead":cached,"cacheWrite":0,"reasoning":reasoning,"totalTokens":total,
        "cost":{"input":ci,"output":co,"cacheRead":cr,"cacheWrite":0,"total":ci+co+cr}})
}

fn user_msg(text: &str) -> Value {
    json!({"role":"user","content":[{"type":"text","text":text}],"timestamp":now_ms()})
}

fn push_msg(sh: &Shared, generated: &mut Vec<Value>, m: Value) {
    emit_line(&json!({"type":"message_start","message":m}));
    emit_line(&json!({"type":"message_end","message":m}));
    sh.st.lock().unwrap().session.append_message(m.clone());
    generated.push(m);
}

pub fn queue_update(st: &State) {
    emit_line(&json!({"type":"queue_update","steering":st.steering,"followUp":st.follow_up}));
}

pub async fn run(sh: Arc<Shared>, abort: Arc<Notify>, first: String) {
    let mut aborted = false;
    let mut next_user: Option<Option<String>> = Some(Some(first));
    let mut overflow_tried = false;
    let mut retry_attempt: u64 = 0;
    while let Some(text) = next_user.take() {
        if text.is_some() { overflow_tried = false; }
        let mut first_turn = true;
        let mut last_msg = Value::Null;
        emit_line(&json!({"type":"agent_start"}));
        let mut generated: Vec<Value> = vec![];
        emit_line(&json!({"type":"turn_start"}));
        // 系统消息只在会话的第一次运行里出现一次。
        let need_sys = !std::mem::replace(&mut sh.st.lock().unwrap().system_emitted, true);
        if need_sys {
            let tools = crate::prompt::tools_enabled(&sh.cfg.tools_allow);
            let sys = crate::prompt::system_message(&sh.cfg.cwd, tools, &sh.cfg.append, now_ms(), sh.cfg.context_files, sh.cfg.bridge);
            push_msg(&sh, &mut generated, sys);
        }
        if let Some(t) = &text { push_msg(&sh, &mut generated, user_msg(t)); }
        loop {
            if !first_turn && over_threshold(&sh) { auto_compact(&sh, &abort, "threshold", false).await; }
            let (msg, calls) = match model_turn(&sh, &abort).await {
                Some(r) => r,
                None => { aborted = true; break; }
            };
            first_turn = false;
            last_msg = msg.clone();
            if retry_attempt > 0 && msg["stopReason"] != "error" {
                emit_line(&json!({"type":"auto_retry_end","success":true,"attempt":retry_attempt}));
                retry_attempt = 0;
            }
            generated.push(msg.clone());
            sh.st.lock().unwrap().session.append_message(msg.clone());
            let mut results = vec![];
            for c in &calls {
                let t0 = std::time::Instant::now();
                emit_line(&json!({"type":"tool_execution_start","toolCallId":c["id"],"toolName":c["name"],"args":c["arguments"]}));
                let allowed = crate::prompt::request_tools(&sh.cfg.tools_allow, sh.cfg.bridge).iter().any(|t| t["name"] == c["name"]);
                let name = c["name"].as_str().unwrap_or("");
                let exec = async {
                    if !allowed { return (format!("Tool {name} not found"), true); }
                    let cid = c["id"].as_str().unwrap_or("");
                    if sh.cfg.bridge && sh.st.lock().unwrap().text_only {
                        return ("Tools are disabled for this host-owned text-only turn.".to_string(), true);
                    }
                    if name == "ask_user_question" { return crate::perm::ask_user_question(&sh, &c["arguments"]).await; }
                    if sh.cfg.bridge {
                        if let Err(reason) = crate::perm::gate(&sh, name, &c["arguments"], cid).await { return (reason, true); }
                    }
                    crate::tools::run(name, &c["arguments"], &sh.cfg.cwd, sh.cfg.bridge).await
                };
                let (out, is_err) = tokio::select! { r = exec => r, _ = abort.notified() => { aborted = true; ("Aborted".to_string(), true) } };
                let dur = t0.elapsed().as_millis() as u64;
                let content = json!([{"type":"text","text":out}]);
                emit_line(&json!({"type":"tool_execution_end","toolCallId":c["id"],"toolName":c["name"],"result":{"content":content},"isError":is_err,"durationMs":dur}));
                let tr = json!({"role":"toolResult","toolCallId":c["id"],"toolName":c["name"],"content":content,"isError":is_err,"durationMs":dur,"timestamp":now_ms()});
                push_msg(&sh, &mut generated, tr.clone());
                results.push(tr);
                if aborted { break; }
            }
            emit_line(&json!({"type":"turn_end","message":msg,"toolResults":results}));
            if aborted || msg["stopReason"] == "error" || msg["stopReason"] == "aborted" { aborted = aborted || msg["stopReason"] == "aborted"; break; }
            // 工具轮结束后先检查 steering：它在下一次模型调用前作为用户消息插入。
            let steer = { let mut s = sh.st.lock().unwrap();
                let one = if s.steering_mode == "all" { s.steering.drain(..).collect::<Vec<_>>().join("\n") } else { s.steering.pop_front().unwrap_or_default() };
                if !one.is_empty() { queue_update(&s); }
                one };
            if calls.is_empty() && steer.is_empty() { break; }
            emit_line(&json!({"type":"turn_start"}));
            if !steer.is_empty() { push_msg(&sh, &mut generated, user_msg(&steer)); }
        }
        let win = sh.st.lock().unwrap().model.as_ref().map_or(0, |m| m.context_window);
        let auto = sh.st.lock().unwrap().auto_compaction;
        let check = !aborted && auto && !last_msg.is_null() && last_msg["stopReason"] != "aborted";
        let ctx_overflow = check && crate::compact::is_context_overflow(&last_msg, win);
        let will_retry = ctx_overflow && last_msg["stopReason"] != "stop";
        let give_up = will_retry && overflow_tried;
        // 请求失败的自动重试：非溢出、可重试的错误，且没用完次数。
        let pol = crate::retry::policy(sh.st.lock().unwrap().auto_retry);
        let err_text = last_msg["errorMessage"].as_str().unwrap_or("").to_string();
        let net_retry = !aborted && last_msg["stopReason"] == "error" && !ctx_overflow && pol.enabled && retry_attempt < pol.max_retries && crate::retry::is_retryable(&err_text);
        emit_line(&json!({"type":"agent_end","messages":generated,"willRetry":(will_retry && !give_up) || net_retry}));
        if net_retry {
            retry_attempt += 1;
            let delay = crate::retry::delay_ms(&pol, retry_attempt);
            emit_line(&json!({"type":"auto_retry_start","attempt":retry_attempt,"maxAttempts":pol.max_retries,"delayMs":delay,
                "errorMessage": if err_text.is_empty() { "Unknown error" } else { &err_text }}));
            let ed = { let mut s = sh.st.lock().unwrap(); let t = s.session.leaf.clone().unwrap_or_default(); s.session.append_context_edit(&t, None) };
            emit_line(&json!({"type":"entry_appended","entry":ed}));
            let cancelled = tokio::select! { _ = tokio::time::sleep(std::time::Duration::from_millis(delay)) => false, _ = abort.notified() => true };
            if cancelled {
                emit_line(&json!({"type":"auto_retry_end","success":false,"attempt":retry_attempt,"finalError":"Retry cancelled"}));
                aborted = true;
                break;
            }
            next_user = Some(None);
            continue;
        }
        if !aborted && last_msg["stopReason"] == "error" && retry_attempt > 0 {
            emit_line(&json!({"type":"auto_retry_end","success":false,"attempt":retry_attempt,"finalError":last_msg["errorMessage"]}));
            retry_attempt = 0;
        }
        if give_up {
            emit_line(&json!({"type":"compaction_end","reason":"overflow","aborted":false,"willRetry":false,
                "errorMessage":"Context overflow recovery failed after one compact-and-retry attempt. Try reducing context or switching to a larger-context model."}));
        } else if will_retry {
            // 去掉失败的回复，压缩后重试一次（不新增用户消息）。
            overflow_tried = true;
            let ed = { let mut s = sh.st.lock().unwrap(); let t = s.session.leaf.clone().unwrap_or_default(); s.session.append_context_edit(&t, None) };
            emit_line(&json!({"type":"entry_appended","entry":ed}));
            if auto_compact(&sh, &abort, "overflow", true).await { next_user = Some(None); continue; }
        } else if ctx_overflow {
            auto_compact(&sh, &abort, "overflow", false).await;
        } else if check {
            let direct = crate::compact::calc_ctx(&last_msg["usage"]);
            let tokens = if last_msg["stopReason"] == "error" || direct == 0 {
                let s = sh.st.lock().unwrap();
                crate::compact::estimate_context(&s.session.messages).0
            } else { direct };
            let st = { let s = sh.st.lock().unwrap(); crate::compact::settings(true, s.model.as_ref()) };
            if win > 0 && crate::compact::should_compact(tokens, win, &st) { auto_compact(&sh, &abort, "threshold", false).await; }
        }
        if !aborted {
            let mut s = sh.st.lock().unwrap();
            let f = if s.follow_up_mode == "all" { s.follow_up.drain(..).collect::<Vec<_>>().join("\n") } else { s.follow_up.pop_front().unwrap_or_default() };
            if !f.is_empty() { queue_update(&s); next_user = Some(Some(f)); }
        }
    }
    {
        let mut s = sh.st.lock().unwrap();
        s.streaming = false;
        s.abort = None;
        if aborted { s.steering.clear(); s.follow_up.clear(); }
    }
    sh.st.lock().unwrap().text_only = false;
    emit_line(&json!({"type":"agent_settled","aborted":aborted}));
}

/// 一次模型调用。返回 (assistant 消息, 其中的工具调用)；被中止返回 None（已发出 aborted 消息）。
async fn model_turn(sh: &Arc<Shared>, abort: &Arc<Notify>) -> Option<(Value, Vec<Value>)> {
    let (model, thinking, msgs, sid) = { let s = sh.st.lock().unwrap(); (s.model.clone(), s.thinking.clone(), s.session.messages.clone(), s.session.id.clone()) };
    let ts = now_ms();
    let t0 = std::time::Instant::now();
    let Some(model) = model else {
        let m = json!({"role":"assistant","content":[],"usage":zero_usage(),"stopReason":"error","errorMessage":"No model selected","timestamp":ts});
        emit_line(&json!({"type":"message_start","message":m}));
        emit_line(&json!({"type":"message_end","message":m}));
        return Some((m, vec![]));
    };
    let base = |rid: &str, usage: Value| json!({"role":"assistant","content":[],"api":model.api,"provider":model.provider,"model":model.id,"usage":usage,"stopReason":"pending","timestamp":ts,"responseId":rid});
    let msgs = crate::compact::to_llm(&msgs);
    let sys_msg = crate::compact::current_system(&msgs).unwrap_or(json!({}));
    let system = crate::prompt::render_system(&sys_msg);
    let max_tokens = crate::compact::clamp_max_tokens(&model, &msgs);
    let wire: Vec<Value> = msgs.iter().filter(|m| m["role"] != "system").cloned().collect();
    let tools = crate::prompt::request_tools(&sh.cfg.tools_allow, sh.cfg.bridge);
    let mut started = false;
    let mut partial: Option<(Vec<Value>, String, String, Value)> = None;
    let upd = |u: &Value, ev: Value| emit_line(&json!({"type":"message_update","usage":map_usage(u, &model),"assistantMessageEvent":ev}));
    let mut cb = |ev: Ev, u: &Value| match ev {
        Ev::Start(rid) => {
            started = true;
            // Anthropic 的 message_start 已带 input tokens，原版会直接反映在快照里。
            let us = if u.get("input_tokens").is_some() { map_usage(u, &model) } else { zero_usage() };
            emit_line(&json!({"type":"message_start","message":base(&rid, us)}));
        }
        Ev::TextStart(i) => upd(u, json!({"type":"text_start","contentIndex":i})),
        Ev::TextDelta(i, d) => upd(u, json!({"type":"text_delta","contentIndex":i,"delta":d})),
        Ev::TextEnd(i, c) => upd(u, json!({"type":"text_end","contentIndex":i,"content":c})),
        Ev::ToolStart(i, id, n) => upd(u, json!({"type":"toolcall_start","contentIndex":i,"id":id,"toolName":n})),
        Ev::ToolDelta(i, d) => upd(u, json!({"type":"toolcall_delta","contentIndex":i,"delta":d})),
        Ev::ToolEnd(i, tc) => upd(u, json!({"type":"toolcall_end","contentIndex":i,"toolCall":tc})),
        Ev::ThinkStart(i) => upd(u, json!({"type":"thinking_start","contentIndex":i})),
        Ev::ThinkDelta(i, d) => upd(u, json!({"type":"thinking_delta","contentIndex":i,"delta":d})),
        Ev::ThinkEnd(i, c) => upd(u, json!({"type":"thinking_end","contentIndex":i,"content":c})),
        Ev::Partial(c, rid, raw) => partial = Some((c, rid, raw, u.clone())),
    };
    let fut = async {
        match model.api.as_str() {
            "anthropic-messages" => crate::anthropic::stream(&sh.client, &model, &system, &wire, &tools, max_tokens, true, &thinking, &msgs, &mut cb).await,
            "openai-completions" => openai::stream(&sh.client, &model, &system, &wire, &tools, max_tokens, &thinking, Some(&sid), &mut cb).await,
            other => Err(format!("Unsupported api: {other}")),
        }
    };
    let res = tokio::select! { r = fut => Some(r), _ = abort.notified() => None };
    let dur = t0.elapsed().as_millis() as u64;
    let finish = |mut m: Value, stop: &str, extra: Value| {
        m["stopReason"] = stop.into();
        if let Value::Object(e) = extra { for (k, v) in e { m[k.as_str()] = v; } }
        m["durationMs"] = dur.into();
        m["thinkingLevel"] = thinking.clone().into();
        m
    };
    match res {
        None => {
            let m = finish(base("", zero_usage()), "aborted", json!({"errorMessage":"Request was aborted"}));
            emit_line(&json!({"type":"message_start","message":m}));
            emit_line(&json!({"type":"message_end","message":m}));
            sh.st.lock().unwrap().session.append_message(m);
            None
        }
        Some(Err(e)) => {
            // 失败回复：没有 responseId；没收到任何流事件时，message_start 就带 error 状态。
            let mut m = base("", zero_usage());
            m.as_object_mut().map(|o| o.remove("responseId"));
            if let Some((content, rid, raw, u)) = &partial {
                // 流中途出错：保留已收到的内容、usage、responseId 和原始 stop reason（原版的失败回复同样带这些）。
                m["content"] = content.clone().into();
                m["usage"] = map_usage(u, &model);
                if !rid.is_empty() { m["responseId"] = rid.clone().into(); }
                m = finish(m, "error", if raw.is_empty() { json!({"errorMessage":e}) } else { json!({"rawStopReason":raw,"errorMessage":e}) });
            } else {
                m = finish(m, "error", json!({"errorMessage":e}));
            }
            if !started { emit_line(&json!({"type":"message_start","message":m})); }
            emit_line(&json!({"type":"message_end","message":m}));
            Some((m, vec![]))
        }
        Some(Ok(o)) => {
            let calls: Vec<Value> = o.content.iter().filter(|b| b["type"] == "toolCall").cloned().collect();
            let stop = match o.raw_stop.as_str() { "tool_calls" | "function_call" | "tool_use" => "toolUse", "length" | "max_tokens" => "length", _ => "stop" };
            let mut m = base(&o.response_id, map_usage(&o.usage, &model));
            m["content"] = o.content.into();
            let m = finish(m, stop, json!({"rawStopReason": o.raw_stop}));
            emit_line(&json!({"type":"message_end","message":m}));
            Some((m, calls))
        }
    }
}

// ---------- 压缩 ----------

fn dispatch_err(e: String) -> String { e }

/// 单次摘要调用：不带 cache_control，max_tokens 按上下文夹紧。
async fn summarize(sh: &Arc<Shared>, model: &Model, abort: &Arc<Notify>, reason: &str, prompt: String, label: &str) -> Result<(String, Value), String> {
    let p = crate::compact::prompts();
    let msgs = vec![
        json!({"role":"system","content":p.system,"timestamp":0}),
        json!({"role":"user","content":[{"type":"text","text":prompt}],"timestamp":now_ms()}),
    ];
    let max_tokens = crate::compact::clamp_max_tokens(model, &msgs);
    let level = sh.st.lock().unwrap().thinking.clone();
    let wire = vec![msgs[1].clone()];
    // 与原版 retryAssistantCall 一致：可重试错误按退避重试，期间发 summarization_retry_* 事件。
    let pol = crate::retry::policy(sh.st.lock().unwrap().auto_retry);
    let max = if pol.enabled { pol.max_retries } else { 0 };
    let mut attempt = 0u64;
    let o = loop {
        let mut cb = |_: Ev, _: &Value| {};
        let fut = async {
            match model.api.as_str() {
                "anthropic-messages" => crate::anthropic::stream(&sh.client, model, &p.system, &wire, &[], max_tokens, false, &level, &msgs, &mut cb).await,
                "openai-completions" => openai::stream(&sh.client, model, &p.system, &wire, &[], max_tokens, &level, None, &mut cb).await,
                other => Err(format!("Unsupported api: {other}")),
            }
        };
        let r = tokio::select! { r = fut => r, _ = abort.notified() => {
            if attempt > 0 { emit_line(&json!({"type":"summarization_retry_finished"})); }
            return Err("Compaction cancelled".into()) } };
        match r {
            Ok(o) => { if attempt > 0 { emit_line(&json!({"type":"summarization_retry_finished"})); } break o; }
            Err(e) if attempt < max && crate::retry::is_retryable(&e) && !crate::compact::is_context_overflow(&json!({"stopReason":"error","errorMessage":e,"usage":{}}), 0) => {
                attempt += 1;
                let delay = crate::retry::delay_ms(&pol, attempt);
                emit_line(&json!({"type":"summarization_retry_scheduled","attempt":attempt,"maxAttempts":max,"delayMs":delay,"errorMessage":e}));
                let cancelled = tokio::select! { _ = tokio::time::sleep(std::time::Duration::from_millis(delay)) => false, _ = abort.notified() => true };
                if cancelled { emit_line(&json!({"type":"summarization_retry_finished"})); return Err("Compaction cancelled".into()); }
                emit_line(&json!({"type":"summarization_retry_attempt_start","source":"compaction","reason":reason}));
            }
            Err(e) => {
                if attempt > 0 { emit_line(&json!({"type":"summarization_retry_finished"})); }
                return Err(format!("{label} failed: {}", dispatch_err(e)));
            }
        }
    };
    if matches!(o.raw_stop.as_str(), "max_tokens" | "length") {
        return Err(format!("{label} failed: generation hit the token cap and the summary is incomplete"));
    }
    if o.content.iter().any(|b| b["type"] == "toolCall") {
        return Err(format!("{}attempted to call a tool", if label == "Summarization" { "Summarization " } else { "Turn prefix summarization " }));
    }
    let text = o.content.iter().filter(|b| b["type"] == "text").filter_map(|b| b["text"].as_str()).collect::<Vec<_>>().join("\n");
    Ok((text, map_usage(&o.usage, model)))
}

fn combine_usage(a: &Value, b: &Value) -> Value {
    let n = |v: &Value, k: &str| v[k].as_u64().unwrap_or(0);
    let c = |v: &Value, k: &str| v["cost"][k].as_f64().unwrap_or(0.0);
    let mut o = json!({"input":n(a,"input")+n(b,"input"),"output":n(a,"output")+n(b,"output"),"cacheRead":n(a,"cacheRead")+n(b,"cacheRead"),
        "cacheWrite":n(a,"cacheWrite")+n(b,"cacheWrite"),"totalTokens":n(a,"totalTokens")+n(b,"totalTokens"),
        "cost":{"input":c(a,"input")+c(b,"input"),"output":c(a,"output")+c(b,"output"),"cacheRead":c(a,"cacheRead")+c(b,"cacheRead"),
        "cacheWrite":c(a,"cacheWrite")+c(b,"cacheWrite"),"total":c(a,"total")+c(b,"total")}});
    if a.get("cacheWrite1h").is_some() || b.get("cacheWrite1h").is_some() { o["cacheWrite1h"] = (n(a, "cacheWrite1h") + n(b, "cacheWrite1h")).into(); }
    o
}

pub fn set_compact_abort(sh: &Shared, a: Option<Arc<Notify>>) {
    sh.st.lock().unwrap().compact_abort = a;
}

/// 运行一次压缩。Ok(None)：自动压缩时没有可压缩内容（不发事件）；Err：失败（已发 compaction_end）。
/// 手动压缩（reason=manual）的 compaction_start 由调用方先发。
pub async fn compact_run(sh: &Arc<Shared>, abort: &Arc<Notify>, reason: &str, custom: Option<String>, will_retry: bool) -> Result<Option<Value>, String> {
    let manual = reason == "manual";
    let (model, entries, enabled) = { let s = sh.st.lock().unwrap(); (s.model.clone(), s.session.path().into_iter().cloned().collect::<Vec<Value>>(), s.auto_compaction) };
    let fail = |msg: String, aborted: bool, started: bool| -> String {
        if started {
            let em = if aborted { Value::Null } else if manual { format!("Compaction failed: {msg}").into() }
                else if reason == "overflow" { format!("Context overflow recovery failed: {msg}").into() } else { format!("Auto-compaction failed: {msg}").into() };
            let mut e = json!({"type":"compaction_end","reason":reason,"aborted":aborted,"willRetry":false});
            if !em.is_null() { e["errorMessage"] = em; }
            emit_line(&e);
        }
        msg
    };
    let Some(model) = model else {
        if !manual { return Ok(None); }
        return Err(fail("No model selected.\n\nUse /login or set an API key environment variable to authenticate. Then use /model to select a model.".into(), false, true));
    };
    let settings = crate::compact::settings(manual || enabled, Some(&model));
    let refs: Vec<&Value> = entries.iter().collect();
    let Some(prep) = crate::compact::prepare(&refs, &settings) else {
        if !manual { return Ok(None); }
        let m = if entries.last().map_or(false, |e| e["type"] == "compaction") { "Already compacted" } else { "Nothing to compact (session too small)" };
        return Err(fail(m.into(), false, true));
    };
    if !manual { emit_line(&json!({"type":"compaction_start","reason":reason})); }
    let p = crate::compact::prompts();
    let custom = custom.as_deref();
    let result: Result<(String, Value), String> = async {
        if prep.split && !prep.turn_prefix.is_empty() {
            let mut hist = prep.prev_summary.clone().unwrap_or_else(|| "No prior history.".into());
            let mut hu: Option<Value> = None;
            if !prep.to_summarize.is_empty() {
                let (t, u) = summarize(sh, &model, abort, reason, crate::compact::summary_prompt(&p, &prep.to_summarize, prep.prev_summary.as_deref(), custom), "Summarization").await?;
                hist = t; hu = Some(u);
            }
            let (tp, tu) = summarize(sh, &model, abort, reason, crate::compact::turn_prefix_prompt(&p, &prep.turn_prefix, custom), "Turn prefix summarization").await?;
            Ok((format!("{hist}\n\n---\n\n**Turn Context (split turn):**\n\n{tp}"), match hu { Some(h) => combine_usage(&h, &tu), None => tu }))
        } else {
            summarize(sh, &model, abort, reason, crate::compact::summary_prompt(&p, &prep.to_summarize, prep.prev_summary.as_deref(), custom), "Summarization").await
        }
    }.await;
    let (mut summary, usage) = match result {
        Ok(r) => r,
        Err(e) => { let aborted = e == "Compaction cancelled"; return Err(fail(e, aborted, true)); }
    };
    let (read, modified) = prep.file_lists();
    summary += &crate::compact::format_file_ops(&read, &modified);
    let details = json!({"readFiles":read,"modifiedFiles":modified});
    let after = {
        let mut s = sh.st.lock().unwrap();
        s.session.append_compaction(&summary, &prep.first_kept, prep.tokens_before, details.clone(), usage.clone());
        crate::compact::estimate_messages(&s.session.messages)
    };
    let res = json!({"summary":summary,"firstKeptEntryId":prep.first_kept,"tokensBefore":prep.tokens_before,"estimatedTokensAfter":after,"usage":usage,"details":details});
    emit_line(&json!({"type":"compaction_end","reason":reason,"result":res,"aborted":false,"willRetry":will_retry}));
    Ok(Some(res))
}

/// 自动压缩期间 isCompacting 为真；run 里用这个包一层。
async fn auto_compact(sh: &Arc<Shared>, abort: &Arc<Notify>, reason: &str, will_retry: bool) -> bool {
    let a = Arc::new(Notify::new());
    set_compact_abort(sh, Some(a.clone()));
    // 用户 abort 同时终止压缩：把 run 的 abort 转发给这次压缩。
    let fwd = { let (a2, ab) = (a.clone(), abort.clone()); tokio::spawn(async move { ab.notified().await; a2.notify_one(); }) };
    let r = compact_run(sh, &a, reason, None, will_retry).await;
    fwd.abort();
    set_compact_abort(sh, None);
    matches!(r, Ok(Some(_)))
}

fn over_threshold(sh: &Shared) -> bool {
    let s = sh.st.lock().unwrap();
    let Some(m) = &s.model else { return false };
    if m.context_window == 0 { return false; }
    let path = s.session.path();
    let proj = crate::compact::project(&path);
    let st = crate::compact::settings(s.auto_compaction, Some(m));
    crate::compact::should_compact(crate::compact::estimate_projected(&proj, &path), m.context_window, &st)
}
