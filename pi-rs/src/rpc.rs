// RPC 主循环：按 LF 切分 stdin 的 JSON 命令，逐条分发；stdin 关闭后等当前运行结束再退出。
use crate::agent::{self, Cfg, Shared, State};
use crate::args::Args;
use crate::models::Registry;
use crate::session::Session;
use crate::util::emit_line;
use serde_json::{json, Value};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::Notify;

fn ok(id: &Value, cmd: &str, data: Option<Value>) -> Value {
    let mut r = json!({"type":"response","command":cmd,"success":true});
    if let Some(d) = data { r["data"] = d; }
    with_id(r, id)
}
fn fail(id: &Value, cmd: &str, e: impl Into<String>) -> Value {
    with_id(json!({"type":"response","command":cmd,"success":false,"error":e.into()}), id)
}
/// id 放在最前面，与原版输出的字段顺序一致。
fn with_id(r: Value, id: &Value) -> Value {
    if id.is_null() { return r; }
    let mut m = serde_json::Map::new();
    m.insert("id".into(), id.clone());
    if let Value::Object(o) = r { m.extend(o); }
    Value::Object(m)
}

pub async fn run(args: Args) -> Result<(), String> {
    let models = Registry::load(&crate::util::agent_dir())?;
    let cwd = std::env::current_dir().map_err(|e| e.to_string())?.canonicalize().map_err(|e| e.to_string())?.to_string_lossy().into_owned();
    let model = match (&args.provider, &args.model) {
        (Some(p), Some(m)) => Some(models.find(p, m).cloned().ok_or_else(|| format!("Model not found: {p}/{m}"))?),
        (None, Some(m)) => {
            let (p, id) = m.split_once('/').unwrap_or(("", m));
            models.models.iter().find(|x| x.id == id && (p.is_empty() || x.provider == p)).cloned()
        }
        _ => models.models.first().cloned(),
    };
    let thinking = default_level(model.as_ref());
    let session = Session::new(args.session_dir.as_deref(), &cwd,
        model.as_ref().map(|m| m.provider.as_str()).unwrap_or(""), model.as_ref().map(|m| m.id.as_str()).unwrap_or(""), &thinking);
    let append: Vec<String> = args.append_system_prompt.iter().map(|a| crate::prompt::read_append(a)).collect();
    let sh = Arc::new(Shared {
        st: Mutex::new(State { session, model, thinking, streaming: false, steering: VecDeque::new(), follow_up: VecDeque::new(),
            steering_mode: "one-at-a-time".into(), follow_up_mode: "one-at-a-time".into(), auto_compaction: crate::compact::enabled_from_settings(), auto_retry: crate::retry::enabled_from_settings(),
            system_emitted: false, abort: None, task: None, compact_abort: None, text_only: false }),
        cfg: Cfg { cwd, tools_allow: args.tools.clone(), append, bridge: args.extensions.iter().any(|e| e.contains("cindy-bridge")), context_files: !args.no_context_files,
            text_only_token: if args.extensions.iter().any(|e| e.contains("cindy-bridge")) { std::env::var("CINDY_PI_TURN_TOOL_POLICY").ok().filter(|t| !t.is_empty()) } else { None } },
        client: reqwest::Client::builder().connect_timeout(std::time::Duration::from_secs(30)).tcp_keepalive(std::time::Duration::from_secs(60)).build().map_err(|e| e.to_string())?,
        models, session_dir: args.session_dir.clone(), ui: Mutex::new(Default::default()),
    });
    // 纯文本轮策略：宿主据此知道这个运行时支持 [CINDY_TEXT_ONLY_INPUT] 前缀（与 bridge 扩展同一约定）。
    if let Some(t) = &sh.cfg.text_only_token {
        emit_line(&json!({"type":"extension_ui_request","id":uuid::Uuid::new_v4().to_string(),"method":"notify","message":format!("cindy:text-only-ready:{t}"),"notifyType":"info"}));
    }
    let mut lines = BufReader::new(tokio::io::stdin()).split(b'\n');
    while let Some(raw) = lines.next_segment().await.map_err(|e| e.to_string())? {
        let raw = String::from_utf8_lossy(&raw);
        let line = raw.trim_end_matches('\r');
        if line.trim().is_empty() { continue; }
        let cmd: Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(e) => { emit_line(&fail(&Value::Null, "parse", format!("Failed to parse command: {e}"))); continue; }
        };
        if cmd["type"] == "extension_ui_response" {
            if let Some(tx) = cmd["id"].as_str().and_then(|i| sh.ui.lock().unwrap().remove(i)) { let _ = tx.send(cmd.clone()); }
            continue;
        }
        handle(&sh, &cmd).await;
    }
    let task = sh.st.lock().unwrap().task.take();
    if let Some(t) = task { let _ = t.await; }
    Ok(())
}

/// 新会话的默认档位：原版默认 medium，再按模型支持的档位夹紧（不支持推理的模型是 off）。
fn default_level(m: Option<&crate::models::Model>) -> String {
    m.map_or("off".into(), |m| m.clamp_level("medium"))
}

fn state_json(sh: &Shared) -> Value {
    let s = sh.st.lock().unwrap();
    let mut o = json!({
        "model": s.model.as_ref().map(|m| m.to_json()), "thinkingLevel": s.thinking, "isStreaming": s.streaming, "isCompacting": s.compact_abort.is_some(),
        "steeringMode": s.steering_mode, "followUpMode": s.follow_up_mode, "sessionFile": s.session.file, "sessionId": s.session.id,
        "autoCompactionEnabled": s.auto_compaction, "messageCount": s.session.messages.len(),
        "pendingMessageCount": s.steering.len() + s.follow_up.len(),
    });
    if let Some(n) = &s.session.name {
        o.as_object_mut().unwrap().insert("sessionName".into(), n.clone().into());
    }
    o
}

async fn handle(sh: &Arc<Shared>, cmd: &Value) {
    let id = cmd.get("id").cloned().unwrap_or(Value::Null);
    let t = cmd["type"].as_str().unwrap_or("");
    let resp = match t {
        "get_state" => ok(&id, t, Some(state_json(sh))),
        "get_messages" => ok(&id, t, Some(json!({"messages": sh.st.lock().unwrap().session.messages}))),
        "get_available_models" => ok(&id, t, Some(json!({"models": sh.models.models.iter().map(|m| m.to_json()).collect::<Vec<_>>()}))),
        "get_commands" => ok(&id, t, Some(json!({"commands": commands(sh)}))),
        "get_last_assistant_text" => {
            let s = sh.st.lock().unwrap();
            let text = s.session.messages.iter().rev().find(|m| m["role"] == "assistant")
                .map(|m| m["content"].as_array().into_iter().flatten().filter(|b| b["type"] == "text").filter_map(|b| b["text"].as_str()).collect::<String>());
            ok(&id, t, Some(json!({"text": text})))
        }
        "export_html" => {
            let built = {
                let s = sh.st.lock().unwrap();
                if !s.session.file.exists() {
                    None
                } else {
                    let sys = crate::compact::current_system(&s.session.messages).unwrap_or(json!({}));
                    let tools = crate::prompt::tools_enabled(&sh.cfg.tools_allow);
                    let html = crate::export::generate_html(&s.session.header, &s.session.entries, s.session.leaf.as_deref(), &crate::prompt::render_system(&sys), &tools);
                    let base = s.session.file.file_stem().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                    Some((html, format!("pi-session-{base}.html")))
                }
            };
            match built {
                None => fail(&id, t, "Nothing to export yet - start a conversation first"),
                Some((html, default_name)) => {
                    let out = match cmd["outputPath"].as_str().filter(|p| !p.is_empty()) {
                        Some(p) => match p.strip_prefix("~/") { Some(r) => dirs_home().join(r).to_string_lossy().into_owned(), None => p.to_string() },
                        None => default_name,
                    };
                    match std::fs::write(&out, html) {
                        Ok(()) => ok(&id, t, Some(json!({"path": out}))),
                        Err(e) => fail(&id, t, e.to_string()),
                    }
                }
            }
        }
        "get_session_stats" => {
            let s = sh.st.lock().unwrap();
            let cw = s.model.as_ref().map(|m| m.context_window).unwrap_or(0);
            ok(&id, t, Some(s.session.stats(cw)))
        }
        "set_model" => {
            let (p, m) = (cmd["provider"].as_str().unwrap_or(""), cmd["modelId"].as_str().unwrap_or(""));
            match sh.models.find(p, m) {
                Some(found) => {
                    let mut s = sh.st.lock().unwrap();
                    s.model = Some(found.clone());
                    s.session.entry("model_change", json!({"provider":p,"modelId":m}));
                    // 换模型后沿用当前档位（settings 里有 defaultThinkingLevel 则以它为准），再按新模型夹紧。
                    let want = crate::retry::default_thinking_level().unwrap_or_else(|| s.thinking.clone());
                    let eff = found.clamp_level(&want);
                    if eff != s.thinking {
                        s.thinking = eff.clone();
                        s.session.entry("thinking_level_change", json!({"thinkingLevel":eff}));
                        emit_line(&json!({"type":"thinking_level_changed","level":eff}));
                    }
                    ok(&id, t, Some(found.to_json()))
                }
                None => fail(&id, t, format!("Model not found: {p}/{m}")),
            }
        }
        "set_thinking_level" => {
            let mut s = sh.st.lock().unwrap();
            let want = cmd["level"].as_str().unwrap_or("off");
            let l = s.model.as_ref().map_or("off".to_string(), |m| m.clamp_level(want));
            if s.thinking != l {
                s.thinking = l.clone();
                s.session.entry("thinking_level_change", json!({"thinkingLevel":l}));
                emit_line(&json!({"type":"thinking_level_changed","level":l}));
            }
            ok(&id, t, None)
        }
        "get_available_thinking_levels" => {
            let s = sh.st.lock().unwrap();
            let levels: Vec<&str> = s.model.as_ref().map_or(crate::models::LEVELS.to_vec(), |m| m.levels());
            ok(&id, t, Some(json!({"levels": levels})))
        }
        "cycle_thinking_level" => {
            let mut s = sh.st.lock().unwrap();
            match s.model.as_ref().filter(|m| m.reasoning).map(|m| m.levels()) {
                None => ok(&id, t, Some(Value::Null)),
                Some(levels) => {
                    let i = levels.iter().position(|l| *l == s.thinking).unwrap_or(0);
                    let next = levels[(i + 1) % levels.len()].to_string();
                    if next != s.thinking {
                        s.thinking = next.clone();
                        s.session.entry("thinking_level_change", json!({"thinkingLevel":next}));
                        emit_line(&json!({"type":"thinking_level_changed","level":next}));
                    }
                    ok(&id, t, Some(json!({"level": next})))
                }
            }
        }
        "set_steering_mode" | "set_follow_up_mode" => {
            let m = cmd["mode"].as_str().unwrap_or("one-at-a-time").to_string();
            let mut s = sh.st.lock().unwrap();
            if t == "set_steering_mode" { s.steering_mode = m } else { s.follow_up_mode = m }
            ok(&id, t, None)
        }
        "set_auto_compaction" => {
            let en = cmd["enabled"].as_bool().unwrap_or(true);
            sh.st.lock().unwrap().auto_compaction = en;
            crate::util::write_setting(&["compaction", "enabled"], json!(en));
            ok(&id, t, None)
        }
        "compact" => {
            if sh.st.lock().unwrap().streaming { let a = sh.st.lock().unwrap().abort.clone(); if let Some(a) = a { a.notify_one(); } let tk = sh.st.lock().unwrap().task.take(); if let Some(tk) = tk { let _ = tk.await; } }
            let a = Arc::new(Notify::new());
            agent::set_compact_abort(sh, Some(a.clone()));
            emit_line(&json!({"type":"compaction_start","reason":"manual"}));
            let r = agent::compact_run(sh, &a, "manual", cmd["customInstructions"].as_str().map(String::from), false).await;
            agent::set_compact_abort(sh, None);
            match r { Ok(Some(d)) => ok(&id, t, Some(d)), Ok(None) => ok(&id, t, None), Err(e) => fail(&id, t, e) }
        }
        "set_auto_retry" => {
            let en = cmd["enabled"].as_bool().unwrap_or(true);
            sh.st.lock().unwrap().auto_retry = en;
            crate::util::write_setting(&["retry", "enabled"], json!(en));
            ok(&id, t, None)
        }
        "set_session_name" => {
            let n = cmd["name"].as_str().unwrap_or("").trim().to_string();
            if n.is_empty() { fail(&id, t, "Session name cannot be empty") } else {
                sh.st.lock().unwrap().session.set_name(&n);
                emit_line(&json!({"type":"session_info_changed","name":n}));
                ok(&id, t, None)
            }
        }
        "clear_queue" => {
            let mut s = sh.st.lock().unwrap();
            let d = json!({"steering": s.steering.drain(..).collect::<Vec<_>>(), "followUp": s.follow_up.drain(..).collect::<Vec<_>>()});
            agent::queue_update(&s);
            ok(&id, t, Some(d))
        }
        "prompt" | "steer" | "follow_up" => {
            let mut msg = cmd["message"].as_str().unwrap_or("").to_string();
            if t == "prompt" && sh.cfg.bridge {
                if let Some(r) = extension_command(sh, &id, &msg).await { emit_line(&r); return; }
            }
            let mut text_only = false;
            if t == "prompt" {
                if let Some(tok) = &sh.cfg.text_only_token {
                    let prefix = format!("[CINDY_TEXT_ONLY_INPUT]:{tok}\n");
                    if let Some(rest) = msg.strip_prefix(&prefix) { msg = rest.to_string(); text_only = true; }
                }
            }
            let mut s = sh.st.lock().unwrap();
            if s.streaming {
                let beh = if t == "prompt" { cmd["streamingBehavior"].as_str().unwrap_or("") } else if t == "steer" { "steer" } else { "followUp" };
                match beh {
                    "steer" => s.steering.push_back(msg),
                    "followUp" => s.follow_up.push_back(msg),
                    _ => { drop(s); emit_line(&fail(&id, t, "Agent is already processing. Specify streamingBehavior ('steer' or 'followUp') to queue the message.")); return; }
                }
                agent::queue_update(&s);
                ok(&id, t, Some(json!({"disposition":"queued"})))
            } else if t != "prompt" {
                fail(&id, t, "Agent is not running")
            } else {
                s.streaming = true;
                s.text_only = text_only;
                let abort = Arc::new(Notify::new());
                s.abort = Some(abort.clone());
                let sh2 = sh.clone();
                // 响应要先于该运行的事件输出：先写响应再起任务，否则新任务可能抢先写出 agent_start。
                emit_line(&ok(&id, t, Some(json!({"disposition":"started"}))));
                s.task = Some(tokio::spawn(agent::run(sh2, abort, msg)));
                return;
            }
        }
        "get_entries" => {
            let s = sh.st.lock().unwrap();
            let since = cmd["since"].as_str();
            let start = match since {
                None => Some(0),
                Some(c) => s.session.entries.iter().position(|e| e["id"] == c).map(|p| p + 1),
            };
            match start {
                None => fail(&id, t, format!("Entry not found: {}", since.unwrap_or(""))),
                Some(st) => ok(&id, t, Some(json!({"entries": &s.session.entries[st..], "leafId": s.session.leaf}))),
            }
        }
        "get_tree" => {
            let s = sh.st.lock().unwrap();
            ok(&id, t, Some(json!({"tree": s.session.tree(), "leafId": s.session.leaf})))
        }
        "get_fork_messages" => ok(&id, t, Some(json!({"messages": sh.st.lock().unwrap().session.fork_messages()}))),
        "new_session" | "clone" | "fork" | "switch_session" => {
            let mut s = sh.st.lock().unwrap();
            if s.streaming { fail(&id, t, "Cannot change session while the agent is running") } else {
                let r: Result<(Session, Option<String>), String> = match t {
                    "new_session" => {
                        let (p, m) = s.model.as_ref().map(|m| (m.provider.clone(), m.id.clone())).unwrap_or_default();
                        let mut n = Session::new(sh.session_dir.as_deref(), &sh.cfg.cwd, &p, &m, &s.thinking);
                        if let Some(par) = cmd["parentSession"].as_str() { n.header["parentSession"] = par.into(); }
                        Ok((n, None))
                    }
                    "clone" => Ok((s.session.clone_branch(), None)),
                    "fork" => s.session.fork_at(cmd["entryId"].as_str().unwrap_or("")).map(|(n, text)| (n, Some(text))),
                    _ => Session::load(cmd["sessionPath"].as_str().unwrap_or("")).map(|n| (n, None)),
                };
                match r {
                    Err(e) => fail(&id, t, e),
                    Ok((mut n, text)) => {
                        if t == "clone" || t == "fork" { n.flush(); }
                        // 切换后恢复该会话记录的模型与思考档位。
                        let (p, m) = n.current_model();
                        if let Some(found) = sh.models.find(&p, &m) { s.model = Some(found.clone()); }
                        s.thinking = n.current_thinking();
                        s.system_emitted = n.messages.iter().any(|x| x["role"] == "system");
                        s.steering.clear();
                        s.follow_up.clear();
                        s.session = n;
                        let mut d = json!({"cancelled": false});
                        if let Some(tx) = text { d["text"] = tx.into(); }
                        if t == "new_session" || t == "switch_session" { d = json!({"cancelled": false}); }
                        ok(&id, t, Some(d))
                    }
                }
            }
        }
        "abort" => {
            let ca = sh.st.lock().unwrap().compact_abort.clone();
            if let Some(c) = ca { c.notify_one(); }
            let (a, task) = { let mut s = sh.st.lock().unwrap(); (s.abort.clone(), s.task.take()) };
            if let Some(a) = a { a.notify_one(); }
            if let Some(t) = task { let _ = t.await; }
            ok(&id, t, None)
        }
        other => fail(&id, other, format!("Unknown command: {other}")),
    };
    emit_line(&resp);
}

fn dirs_home() -> std::path::PathBuf {
    std::env::var("HOME").map(Into::into).unwrap_or_default()
}

/// bridge 扩展注册的命令（宿主用 get_commands 判断能力是否可用）。cindy-native-provider-refresh 没有实现，故意不列出：宿主会立刻报"扩展不可用"，而不是等 10 秒回执超时。
fn commands(sh: &Shared) -> Vec<Value> {
    if !sh.cfg.bridge { return vec![]; }
    let info = |d: &str, n: &str| json!({"name":n,"description":d,"source":"extension","sourceInfo":{"path":"cindy-bridge.ts","source":"cli","scope":"temporary","origin":"top-level"}});
    vec![info("Cindy internal session branch navigation", "cindy-branch-switch")]
}

/// 处理以 / 开头的扩展命令；不是已知命令返回 None（照常当作用户消息）。
/// 成功/失败都按原版返回 prompt 响应：disposition=handled。
async fn extension_command(sh: &Arc<Shared>, id: &Value, msg: &str) -> Option<Value> {
    let rest = msg.strip_prefix('/')?;
    let (name, args) = rest.split_once(' ').map(|(n, a)| (n, a.trim())).unwrap_or((rest, ""));
    match name {
        "cindy-branch-switch" => {
            let payload: Value = urlencoding_decode(args).and_then(|t| serde_json::from_str(&t).ok()).unwrap_or(Value::Null);
            let entry = payload["entryId"].as_str().unwrap_or("").to_string();
            let mut s = sh.st.lock().unwrap();
            if s.streaming { return Some(fail(id, "prompt", "Wait for the current response to finish before navigating the session tree.")); }
            if payload["summarize"] == true {
                return Some(fail(id, "prompt", "pi-rs does not support branch summaries; reopen this task with the original Pi runtime."));
            }
            match s.session.navigate(&entry) {
                Ok((_, changed)) => {
                    if changed {
                        if let Some(l) = payload["label"].as_str().filter(|l| !l.is_empty()) { s.session.entry("label", json!({"targetId":entry,"label":l})); }
                    }
                    Some(ok(id, "prompt", Some(json!({"disposition":"handled"}))))
                }
                Err(e) => Some(fail(id, "prompt", e)),
            }
        }
        _ => None,
    }
}

fn urlencoding_decode(s: &str) -> Option<String> {
    let b = s.as_bytes();
    let (mut out, mut i) = (Vec::with_capacity(b.len()), 0);
    while i < b.len() {
        if b[i] == b'%' {
            let h = std::str::from_utf8(b.get(i + 1..i + 3)?).ok()?;
            out.push(u8::from_str_radix(h, 16).ok()?);
            i += 3;
        } else { out.push(b[i]); i += 1; }
    }
    String::from_utf8(out).ok()
}
