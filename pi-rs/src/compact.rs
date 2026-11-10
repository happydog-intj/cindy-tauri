// 上下文压缩：与原版 1.4.0 的 dist/core/compaction/*、session-manager 投影和 ai/utils/estimate 一致。
// 这里只放纯函数（投影、token 估算、切点、提示词、溢出判定）；调用模型和发事件在 agent.rs。
use crate::models::Model;
use serde_json::{json, Map, Value};
use std::collections::{BTreeSet, HashMap, HashSet};

pub const COMPACTION_PREFIX: &str = "The conversation history before this point was compacted into the following summary:\n\n<summary>\n";
pub const COMPACTION_SUFFIX: &str = "\n</summary>";
pub const BRANCH_PREFIX: &str = "The following is a summary of a branch that this conversation came back from:\n\n<summary>\n";
pub const BRANCH_SUFFIX: &str = "</summary>";
const TOOL_RESULT_MAX_CHARS: usize = 2000;
const IMAGE_CHARS: usize = 4800;
const CONTEXT_SAFETY_TOKENS: i64 = 4096;

pub struct Prompts {
    pub system: String,
    pub summarize: String,
    pub update: String,
    pub turn_prefix: String,
}

/// 提示词原文从原版二进制按字节提取（src/static/compaction.json）。
pub fn prompts() -> Prompts {
    let v: Value = serde_json::from_str(include_str!("static/compaction.json")).expect("compaction.json");
    let s = |k: &str| v[k].as_str().unwrap_or("").to_string();
    Prompts { system: s("system"), summarize: s("summarize"), update: format!("{}{}", s("updatePrefix"), s("updateInstructions")), turn_prefix: s("turnPrefix") }
}

// ---------- 设置 ----------

pub struct Settings {
    pub enabled: bool,
    pub reserve: u64,
    pub keep: u64,
}

fn settings_json() -> Value {
    std::fs::read_to_string(crate::util::agent_dir().join("settings.json")).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or(Value::Null)
}

/// settings.json 里的 compaction.enabled，缺省 true。
pub fn enabled_from_settings() -> bool {
    settings_json()["compaction"]["enabled"].as_bool().unwrap_or(true)
}

/// 每次检查时重读 settings.json：Cindy 会在会话中途改写 modelOverrides。
/// 优先级：modelOverrides["provider/id"] > 顶层字段 > 默认值（16384 / 20000）。
pub fn settings(enabled: bool, model: Option<&Model>) -> Settings {
    let v = settings_json();
    let c = &v["compaction"];
    let key = model.map(|m| format!("{}/{}", m.provider, m.id));
    let get = |f: &str, d: u64| key.as_ref().and_then(|k| c["modelOverrides"][k.as_str()][f].as_u64()).or(c[f].as_u64()).unwrap_or(d);
    Settings { enabled, reserve: get("reserveTokens", 16384), keep: get("keepRecentTokens", 20000) }
}

// ---------- 长度与文本 ----------

/// JS 的 string.length（UTF-16 码元数）。
pub fn js_len(s: &str) -> usize {
    s.encode_utf16().count()
}

pub fn content_text(c: &Value, sep: &str) -> String {
    match c {
        Value::String(s) => s.clone(),
        Value::Array(a) => a.iter().filter(|b| b["type"] == "text").map(|b| b["text"].as_str().unwrap_or("")).collect::<Vec<_>>().join(sep),
        _ => String::new(),
    }
}

fn ms_of(iso: &Value) -> Value {
    iso.as_str().and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok()).map(|d| Value::from(d.timestamp_millis())).unwrap_or(Value::Null)
}

// ---------- 条目 → 上下文消息（sessionEntryToContextMessages） ----------

pub fn entry_messages(e: &Value) -> Vec<Value> {
    match e["type"].as_str().unwrap_or("") {
        "message" => {
            let mut m = e["message"].clone();
            let role = m["role"].as_str().unwrap_or("").to_string();
            if m.get("content").map_or(true, Value::is_null) {
                if role == "system" { m["content"] = "".into(); } else if matches!(role.as_str(), "user" | "assistant" | "toolResult") { m["content"] = json!([]); }
            }
            vec![m]
        }
        "custom_message" => vec![json!({"role":"custom","customType":e["customType"],"content":if e["content"].is_null() { json!([]) } else { e["content"].clone() },
            "display":e["display"],"details":e["details"],"timestamp":ms_of(&e["timestamp"])})],
        "branch_summary" if e["summary"].as_str().map_or(false, |s| !s.is_empty()) =>
            vec![json!({"role":"branchSummary","summary":e["summary"],"fromId":e["fromId"],"timestamp":ms_of(&e["timestamp"])})],
        "compaction" => {
            let s = json!({"role":"compactionSummary","summary":e["summary"],"tokensBefore":e["tokensBefore"],"timestamp":ms_of(&e["timestamp"])});
            match e.get("systemMessage").filter(|v| !v.is_null()) { Some(sm) => vec![sm.clone(), s], None => vec![s] }
        }
        _ => vec![],
    }
}

// ---------- 投影（buildContextEntries / buildSessionProjection） ----------

pub struct PEntry<'a> {
    pub src: &'a Value,
    pub msgs: Vec<Value>,
}

fn kind(e: &Value) -> &str {
    e["type"].as_str().unwrap_or("")
}

/// 路径上最近一次压缩：压缩条目在前，再接 firstKeptEntryId 起（不含 system 消息）到压缩条目之前的条目，再接压缩之后的条目。
fn context_entries<'a>(path: &[&'a Value]) -> Vec<&'a Value> {
    let Some(ci) = path.iter().rposition(|e| kind(e) == "compaction") else { return path.to_vec() };
    let c = path[ci];
    let mut out = vec![c];
    let mut found = false;
    for e in &path[..ci] {
        if e["id"] == c["firstKeptEntryId"] { found = true; }
        if found && !(kind(e) == "message" && e["message"]["role"] == "system") { out.push(e); }
    }
    out.extend_from_slice(&path[ci + 1..]);
    out
}

pub fn project<'a>(path: &[&'a Value]) -> Vec<PEntry<'a>> {
    let ce = context_entries(path);
    let mut edits: HashMap<&str, &Value> = HashMap::new();
    for e in &ce {
        if kind(e) == "context_edit" {
            if let Some(t) = e["targetId"].as_str() { edits.insert(t, e); }
        }
    }
    ce.iter().enumerate().map(|(i, e)| {
        let msgs = if kind(e) == "compaction" && i > 0 { vec![] } else {
            let ms = entry_messages(e);
            match e["id"].as_str().and_then(|id| edits.get(id)) {
                None => ms,
                Some(ed) => {
                    let r = &ed["replacement"];
                    if r.is_null() { vec![] } else {
                        ms.into_iter().map(|mut m| {
                            let role = m["role"].as_str().unwrap_or("").to_string();
                            if matches!(role.as_str(), "user" | "assistant" | "toolResult" | "custom") {
                                m["content"] = match &r["content"] {
                                    Value::String(s) if role == "assistant" || role == "toolResult" => json!([{"type":"text","text":s}]),
                                    c => c.clone(),
                                };
                            }
                            m
                        }).collect()
                    }
                }
            }
        };
        PEntry { src: e, msgs }
    }).collect()
}

pub fn proj_messages(p: &[PEntry]) -> Vec<Value> {
    p.iter().flat_map(|e| e.msgs.iter().cloned()).collect()
}

// ---------- 系统消息合并（getCurrentSystemMessage） ----------

fn current_tools(msgs: &[Value]) -> Vec<Value> {
    let mut tools: Vec<(String, Value)> = vec![];
    for m in msgs.iter().filter(|m| m["role"] == "system") {
        for t in m["toolsRemoved"].as_array().into_iter().flatten() {
            tools.retain(|(n, _)| Some(n.as_str()) != t["name"].as_str());
        }
        for t in m["toolsAdded"].as_array().into_iter().flatten() {
            let n = t["name"].as_str().unwrap_or("").to_string();
            match tools.iter_mut().find(|(k, _)| *k == n) { Some(slot) => slot.1 = t.clone(), None => tools.push((n, t.clone())) }
        }
    }
    tools.into_iter().map(|(_, t)| t).collect()
}

pub fn current_system(msgs: &[Value]) -> Option<Value> {
    let mut content = vec![];
    let mut sections = Map::new();
    let mut ts: Option<Value> = None;
    for m in msgs.iter().filter(|m| m["role"] == "system") {
        if ts.is_none() { ts = Some(m["timestamp"].clone()); }
        let t = content_text(&m["content"], "\n");
        if !t.is_empty() { content.push(t); }
        for (k, v) in m["sections"].as_object().into_iter().flatten() {
            if v.is_null() { sections.shift_remove(k); } else { sections.insert(k.clone(), v.clone()); }
        }
    }
    let tools = current_tools(msgs);
    if ts.is_none() && tools.is_empty() { return None; }
    let mut o = json!({"role":"system","content":content.join("\n\n")});
    if !sections.is_empty() { o["sections"] = Value::Object(sections); }
    if !tools.is_empty() { o["toolsAdded"] = tools.into(); }
    o["timestamp"] = ts.unwrap_or(0.into());
    Some(o)
}

// ---------- token 估算 ----------

fn chars4(c: &Value) -> usize {
    match c {
        Value::String(s) => js_len(s),
        Value::Array(a) => a.iter().map(|b| match b["type"].as_str() {
            Some("text") => b["text"].as_str().map_or(0, js_len),
            Some("image") => IMAGE_CHARS,
            _ => 0,
        }).sum(),
        _ => 0,
    }
}

fn assistant_chars(m: &Value) -> usize {
    m["content"].as_array().into_iter().flatten().map(|b| match b["type"].as_str() {
        Some("text") => b["text"].as_str().map_or(0, js_len),
        Some("thinking") => b["thinking"].as_str().map_or(0, js_len),
        Some("toolCall") => b["name"].as_str().map_or(0, js_len) + js_len(&b["arguments"].to_string()),
        _ => 0,
    }).sum()
}

/// 压缩模块的估算（estimateTokens2）：字符数 / 4 向上取整。
pub fn est4(m: &Value) -> u64 {
    let chars = match m["role"].as_str().unwrap_or("") {
        "system" => {
            chars4(&m["content"])
                + m["sections"].as_object().map_or(0, |s| s.values().filter_map(Value::as_str).map(js_len).sum())
                + m.get("toolsAdded").filter(|t| !t.is_null()).map_or(0, |t| js_len(&t.to_string()))
        }
        "user" | "custom" | "toolResult" => chars4(&m["content"]),
        "assistant" => assistant_chars(m),
        "bashExecution" => m["command"].as_str().map_or(0, js_len) + m["output"].as_str().map_or(0, js_len),
        "branchSummary" | "compactionSummary" => m["summary"].as_str().map_or(0, js_len),
        _ => return 0,
    };
    chars.div_ceil(4) as u64
}

pub fn calc_ctx(u: &Value) -> u64 {
    let t = u["totalTokens"].as_u64().unwrap_or(0);
    if t > 0 { t } else { ["input", "output", "cacheRead", "cacheWrite"].iter().map(|k| u[*k].as_u64().unwrap_or(0)).sum() }
}

fn usable_assistant(m: &Value) -> bool {
    m["role"] == "assistant" && !matches!(m["stopReason"].as_str(), Some("aborted" | "error")) && calc_ctx(&m["usage"]) > 0
}

/// estimateContextTokens2：最近一条有 usage 的 assistant 的总数 + 其后消息的估算。返回 (tokens, 该 assistant 下标)。
pub fn estimate_context(msgs: &[Value]) -> (u64, Option<usize>) {
    match msgs.iter().rposition(usable_assistant) {
        None => (msgs.iter().map(est4).sum(), None),
        Some(i) => (calc_ctx(&msgs[i]["usage"]) + msgs[i + 1..].iter().map(est4).sum::<u64>(), Some(i)),
    }
}

/// estimateProjectedContextTokens：usage 只有在其后没有压缩 / context_edit 时才可信，否则整段按字符估算。
pub fn estimate_projected(proj: &[PEntry], path: &[&Value]) -> u64 {
    let msgs = proj_messages(proj);
    let (tokens, last) = estimate_context(&msgs);
    if let Some(li) = last {
        let mut k = 0;
        let mut uid = None;
        for e in proj {
            let next = k + e.msgs.len();
            if li < next { uid = e.src["id"].as_str(); break; }
            k = next;
        }
        let uidx = uid.and_then(|u| path.iter().position(|e| e["id"] == u)).map_or(-1, |x| x as i64);
        let inv = path.iter().rposition(|e| matches!(kind(e), "context_edit" | "compaction")).map_or(-1, |x| x as i64);
        if uidx > inv { return tokens; }
    }
    current_system(&msgs).map_or(0, |s| est4(&s)) + msgs.iter().filter(|m| m["role"] != "system").map(est4).sum::<u64>()
}

pub fn should_compact(tokens: u64, window: u64, s: &Settings) -> bool {
    s.enabled && (tokens as i64) > window as i64 - s.reserve as i64
}

// ai/utils/estimate.ts：请求前夹紧 max_tokens 用的估算（字符数 / 3.5）。
fn ai_chars(c: &Value) -> usize {
    match c {
        Value::String(s) => js_len(s),
        Value::Array(a) => a.iter().map(|b| if b["type"] == "text" { b["text"].as_str().map_or(0, js_len) } else { IMAGE_CHARS }).sum(),
        _ => 0,
    }
}
fn ceil35(n: usize) -> i64 {
    (n as f64 / 3.5).ceil() as i64
}
fn ai_tools(t: &Value) -> i64 {
    match t.as_array() { Some(a) if !a.is_empty() => ceil35(js_len(&t.to_string())), _ => 0 }
}
fn system_text(m: &Value) -> String {
    let mut parts = vec![content_text(&m["content"], "\n")];
    for v in m["sections"].as_object().into_iter().flatten().map(|(_, v)| v) {
        if let Some(s) = v.as_str() { parts.push(s.to_string()); }
    }
    parts.into_iter().filter(|p| !p.is_empty()).collect::<Vec<_>>().join("\n\n")
}
fn ai_est(m: &Value) -> i64 {
    match m["role"].as_str().unwrap_or("") {
        "system" => ceil35(js_len(&system_text(m))) + ai_tools(&m["toolsAdded"]) + ai_tools(&m["toolsRemoved"]),
        "user" | "toolResult" => ceil35(ai_chars(&m["content"])),
        _ => ceil35(assistant_chars(m)),
    }
}

/// clampMaxTokensToContext：max_tokens 不超过 contextWindow − 上下文估算 − 4096，至少为 1。
/// msgs 是发给模型的 LLM 消息（含 system）。usage 只认时间戳不早于其前所有消息的 assistant。
pub fn clamp_max_tokens(model: &Model, msgs: &[Value]) -> u64 {
    clamp_to_context(model, msgs, model.max_tokens)
}

/// 同上，但上限用调用方给的 max_tokens（开启思考时已加上预算）。
pub fn clamp_to_context(model: &Model, msgs: &[Value], max_tokens: u64) -> u64 {
    let mt = max_tokens as i64;
    if model.context_window == 0 { return mt.max(1) as u64; }
    let mut latest = f64::NEG_INFINITY;
    let mut info = None;
    for (i, m) in msgs.iter().enumerate() {
        let ts = m["timestamp"].as_f64().unwrap_or(f64::NAN);
        if m["role"] == "assistant" && ts >= latest && usable_assistant(m) { info = Some(i); }
        latest = if ts.is_nan() || latest.is_nan() { f64::NAN } else { latest.max(ts) };
    }
    let tokens = match info {
        Some(i) => calc_ctx(&msgs[i]["usage"]) as i64 + msgs[i + 1..].iter().map(ai_est).sum::<i64>(),
        None => msgs.iter().map(ai_est).sum(),
    };
    let available = model.context_window as i64 - tokens - CONTEXT_SAFETY_TOKENS;
    mt.min(available.max(1)) as u64
}

pub fn estimate_messages(msgs: &[Value]) -> u64 {
    msgs.iter().map(est4).sum()
}

// ---------- 发给模型前的转换（convertToLlm） ----------

pub fn to_llm(msgs: &[Value]) -> Vec<Value> {
    msgs.iter().filter_map(|m| {
        let text = |t: String| json!({"role":"user","content":[{"type":"text","text":t}],"timestamp":m["timestamp"]});
        match m["role"].as_str().unwrap_or("") {
            "compactionSummary" => Some(text(format!("{COMPACTION_PREFIX}{}{COMPACTION_SUFFIX}", m["summary"].as_str().unwrap_or("")))),
            "branchSummary" => Some(text(format!("{BRANCH_PREFIX}{}{BRANCH_SUFFIX}", m["summary"].as_str().unwrap_or("")))),
            "custom" => {
                let c = match &m["content"] { Value::String(s) => json!([{"type":"text","text":s}]), c => c.clone() };
                Some(json!({"role":"user","content":c,"timestamp":m["timestamp"]}))
            }
            "system" | "user" | "assistant" | "toolResult" => Some(m.clone()),
            _ => None,
        }
    }).collect()
}

// ---------- 溢出判定（isContextOverflow / isRecoverableLength） ----------

const OVERFLOW: &[&str] = &[
    r"prompt (?:is )?too long", r"prompt exceeds max length", r"request_too_large", r"input is too long for requested model",
    r"exceeds the context window", r"exceeds (?:the )?(?:model'?s )?maximum context length(?: of [\d,]+ tokens?|\s*\([\d,]+\))",
    r"input token count.*exceeds the maximum", r"maximum prompt length is \d+", r"reduce the length of the messages",
    r"maximum context length is \d+ tokens", r"exceeds (?:the )?maximum allowed input length of [\d,]+ tokens?",
    r"input \(\d+ tokens\) is longer than the model'?s context length \(\d+ tokens\)", r"exceeds the limit of \d+",
    r"exceeds the available context size", r"greater than the context length", r"context window exceeds limit",
    r"exceeded model token limit", r"too large for model with \d+ maximum context length",
    r"prompt has [\d,]+ tokens?, but the configured context size is [\d,]+ tokens?", r"model_context_window_exceeded",
    r"prompt too long; exceeded (?:max )?context length", r"range of input length should be", r"context[_ ]length[_ ]exceeded",
    r"too many tokens", r"token limit exceeded", r"^(?:[\w -]+ API error \(413\):|413(?:\s|:|$))",
];
const NON_OVERFLOW: &[&str] = &[r"^(Throttling error|Service unavailable):", r"rate limit", r"too many requests"];

fn any_match(pats: &[&str], s: &str) -> bool {
    pats.iter().any(|p| regex_lite::Regex::new(&format!("(?i){p}")).map_or(false, |r| r.is_match(s)))
}

/// window 为 0 表示不按 usage 判断，只看错误文本。
pub fn is_context_overflow(m: &Value, window: u64) -> bool {
    let stop = m["stopReason"].as_str().unwrap_or("");
    if stop == "error" {
        if let Some(e) = m["errorMessage"].as_str().filter(|e| !e.is_empty()) {
            if !any_match(NON_OVERFLOW, e) {
                if any_match(OVERFLOW, e) { return true; }
                if m["provider"] == "cerebras" && any_match(&[r"^4(?:00|13)\s*(?:status code)?\s*\(no body\)"], e) { return true; }
            }
        }
    }
    let u = &m["usage"];
    let input = u["input"].as_u64().unwrap_or(0) + u["cacheRead"].as_u64().unwrap_or(0);
    if window > 0 && stop == "stop" && input > window { return true; }
    if window > 0 && stop == "length" && u["output"].as_u64().unwrap_or(0) == 0 && input as f64 >= window as f64 * 0.99 { return true; }
    false
}

// ---------- 切点与准备（findProjectedCutPoint / prepareCompaction） ----------

fn is_cut_point(m: &Value) -> bool {
    matches!(m["role"].as_str(), Some("user" | "assistant" | "bashExecution" | "custom" | "branchSummary" | "compactionSummary"))
}
fn is_turn_start_msg(m: &Value) -> bool {
    matches!(m["role"].as_str(), Some("user" | "bashExecution" | "custom" | "branchSummary" | "compactionSummary"))
}
fn is_turn_start(e: &PEntry) -> bool {
    kind(e.src) != "compaction" && e.msgs.iter().any(is_turn_start_msg)
}

/// 返回 (首个保留条目下标, 回合起点下标, 是否切在回合中间)。
fn find_cut(es: &[PEntry], start: usize, end: usize, keep: u64) -> (usize, Option<usize>, bool) {
    let cuts: Vec<usize> = (start..end).filter(|&i| kind(es[i].src) != "compaction" && es[i].msgs.iter().any(is_cut_point)).collect();
    if cuts.is_empty() { return (start, None, false); }
    let (mut acc, mut exceeded, mut cut) = (0u64, false, cuts[0]);
    for i in (start..end).rev() {
        let t: u64 = es[i].msgs.iter().map(est4).sum();
        if t == 0 { continue; }
        acc += t;
        if acc >= keep {
            exceeded = true;
            cut = cuts.iter().copied().find(|&c| c >= i).unwrap_or(*cuts.last().unwrap());
            break;
        }
    }
    let suffix = &es[(cut + 1).min(end)..end];
    let visible = |e: &PEntry| kind(e.src) != "context_edit" && !entry_messages(e.src).is_empty();
    let omitted = |e: &PEntry| visible(e) && e.msgs.is_empty();
    let omitted_ids: HashSet<&str> = suffix.iter().filter(|e| omitted(e)).filter_map(|e| e.src["id"].as_str()).collect();
    let has_ext = suffix.iter().any(|e| kind(e.src) == "context_edit" && !e.src["replacement"].is_null()
        && !e.src["targetId"].as_str().map_or(false, |t| omitted_ids.contains(t)));
    let recovery = exceeded && !has_ext
        && suffix.iter().any(|e| kind(e.src) == "message" && e.src["message"]["role"] == "assistant" && omitted(e))
        && suffix.iter().all(|e| kind(e.src) != "compaction" && (!visible(e) || omitted(e)));
    if recovery { cut += 1; }
    while cut > start {
        let p = &es[cut - 1];
        if kind(p.src) == "compaction" || !p.msgs.is_empty() { break; }
        cut -= 1;
    }
    if es.get(cut).map_or(false, is_turn_start) { return (cut, None, false); }
    let ts = (start..=cut.min(es.len().saturating_sub(1))).rev().find(|&i| is_turn_start(&es[i]));
    (cut, ts, ts.is_some())
}

pub struct Prep {
    pub first_kept: String,
    pub to_summarize: Vec<Value>,
    pub turn_prefix: Vec<Value>,
    pub split: bool,
    pub tokens_before: u64,
    pub prev_summary: Option<String>,
    pub read: BTreeSet<String>,
    pub modified: BTreeSet<String>,
}

fn add_file_ops(m: &Value, read: &mut BTreeSet<String>, modified: &mut BTreeSet<String>) {
    let mut add = |name: &str, args: &Value| {
        if let Some(p) = args["path"].as_str().filter(|p| !p.is_empty()) {
            match name { "read" => { read.insert(p.into()); } "write" | "edit" => { modified.insert(p.into()); } _ => {} }
        }
    };
    match m["role"].as_str() {
        Some("toolResult") => for c in m["nestedCalls"]["calls"].as_array().into_iter().flatten() { add(c["name"].as_str().unwrap_or(""), &c["arguments"]); },
        Some("assistant") => for b in m["content"].as_array().into_iter().flatten().filter(|b| b["type"] == "toolCall") { add(b["name"].as_str().unwrap_or(""), &b["arguments"]); },
        _ => {}
    }
}

/// prepareCompaction。None 表示没有可压缩的内容（叶子已是压缩条目，或历史太短）。
pub fn prepare(path: &[&Value], s: &Settings) -> Option<Prep> {
    if path.last().map_or(false, |e| kind(e) == "compaction") { return None; }
    let proj = project(path);
    let prev = proj.iter().position(|e| kind(e.src) == "compaction" && !e.msgs.is_empty());
    let (prev_summary, start) = match prev { Some(i) => (proj[i].src["summary"].as_str().map(String::from), i + 1), None => (None, 0) };
    let tokens_before = estimate_projected(&proj, path);
    let (fk, ts, split) = find_cut(&proj, start, proj.len(), s.keep);
    let first_kept = proj.get(fk)?.src["id"].as_str()?.to_string();
    let take = |a: usize, b: usize| -> Vec<Value> {
        proj[a..b].iter().filter(|e| kind(e.src) != "compaction").flat_map(|e| e.msgs.iter().filter(|m| m["role"] != "system").cloned()).collect()
    };
    let hist_end = if split { ts.unwrap() } else { fk };
    let to_summarize = take(start, hist_end);
    let turn_prefix = if split { take(ts.unwrap(), fk) } else { vec![] };
    if to_summarize.is_empty() && turn_prefix.is_empty() { return None; }
    let (mut read, mut modified) = (BTreeSet::new(), BTreeSet::new());
    if let Some(i) = prev {
        let c = proj[i].src;
        if !c["fromHook"].as_bool().unwrap_or(false) {
            for f in c["details"]["readFiles"].as_array().into_iter().flatten().filter_map(Value::as_str) { read.insert(f.to_string()); }
            for f in c["details"]["modifiedFiles"].as_array().into_iter().flatten().filter_map(Value::as_str) { modified.insert(f.to_string()); }
        }
    }
    for m in to_summarize.iter().chain(turn_prefix.iter()) { add_file_ops(m, &mut read, &mut modified); }
    Some(Prep { first_kept, to_summarize, turn_prefix, split, tokens_before, prev_summary, read, modified })
}

impl Prep {
    /// computeFileLists：只读文件去掉被修改过的，两组都按字典序。
    pub fn file_lists(&self) -> (Vec<String>, Vec<String>) {
        (self.read.iter().filter(|f| !self.modified.contains(*f)).cloned().collect(), self.modified.iter().cloned().collect())
    }
}

pub fn format_file_ops(read: &[String], modified: &[String]) -> String {
    let mut s = vec![];
    if !read.is_empty() { s.push(format!("<read-files>\n{}\n</read-files>", read.join("\n"))); }
    if !modified.is_empty() { s.push(format!("<modified-files>\n{}\n</modified-files>", modified.join("\n"))); }
    if s.is_empty() { String::new() } else { format!("\n\n{}", s.join("\n\n")) }
}

// ---------- 摘要提示词（serializeConversation 等） ----------

fn truncate(t: &str, max: usize) -> String {
    let u: Vec<u16> = t.encode_utf16().collect();
    if u.len() <= max { return t.to_string(); }
    format!("{}\n\n[... {} more characters truncated]", String::from_utf16_lossy(&u[..max]), u.len() - max)
}

pub fn serialize(msgs: &[Value]) -> String {
    let mut parts = vec![];
    for m in to_llm(msgs) {
        match m["role"].as_str().unwrap_or("") {
            "user" => { let c = content_text(&m["content"], ""); if !c.is_empty() { parts.push(format!("[User]: {c}")); } }
            "assistant" => {
                let blocks = m["content"].as_array().cloned().unwrap_or_default();
                let calls: Vec<String> = blocks.iter().filter(|b| b["type"] == "toolCall").map(|b| {
                    let args = b["arguments"].as_object().map(|o| o.iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>().join(", ")).unwrap_or_default();
                    format!("{}({args})", b["name"].as_str().unwrap_or(""))
                }).collect();
                if blocks.iter().any(|b| b["type"] == "text") { parts.push(format!("[Assistant]: {}", content_text(&m["content"], "\n"))); }
                if !calls.is_empty() { parts.push(format!("[Assistant tool calls]: {}", calls.join("; "))); }
            }
            "toolResult" => { let c = content_text(&m["content"], ""); if !c.is_empty() { parts.push(format!("[Tool result]: {}", truncate(&c, TOOL_RESULT_MAX_CHARS))); } }
            _ => {}
        }
    }
    parts.join("\n\n")
}

pub fn summary_prompt(p: &Prompts, msgs: &[Value], prev: Option<&str>, custom: Option<&str>) -> String {
    let mut base = if prev.is_some() { p.update.clone() } else { p.summarize.clone() };
    if let Some(c) = custom.filter(|c| !c.is_empty()) { base = format!("{base}\n\nAdditional focus: {c}"); }
    let mut t = format!("<conversation>\n{}\n</conversation>\n\n", serialize(msgs));
    if let Some(pv) = prev.filter(|s| !s.is_empty()) { t += &format!("<previous-summary>\n{pv}\n</previous-summary>\n\n"); }
    t + &base
}

pub fn turn_prefix_prompt(p: &Prompts, msgs: &[Value], custom: Option<&str>) -> String {
    let t = format!("# Conversation\n{}\n\n# Instructions\n{}", serialize(msgs), p.turn_prefix);
    match custom.filter(|c| !c.is_empty()) { Some(c) => format!("{t}\n\nAdditional focus: {c}"), None => t }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msg(id: &str, parent: Option<&str>, m: Value) -> Value {
        json!({"type":"message","id":id,"parentId":parent,"timestamp":"2026-01-01T00:00:00.000Z","message":m})
    }

    #[test]
    fn small_session_has_nothing_to_compact() {
        let es = vec![
            json!({"type":"model_change","id":"a","parentId":null,"timestamp":"2026-01-01T00:00:00.000Z"}),
            msg("b", Some("a"), json!({"role":"system","content":"","sections":{"p":"x"},"timestamp":1})),
            msg("c", Some("b"), json!({"role":"user","content":[{"type":"text","text":"hi"}],"timestamp":2})),
            msg("d", Some("c"), json!({"role":"assistant","content":[{"type":"text","text":"yo"}],"usage":{"totalTokens":10},"stopReason":"stop","timestamp":3})),
        ];
        let path: Vec<&Value> = es.iter().collect();
        assert!(prepare(&path, &Settings { enabled: true, reserve: 16384, keep: 20000 }).is_none());
        // keep 很小时，最后一条用户消息之前的内容被摘要。
        let big = "x".repeat(400);
        let mut es2 = es.clone();
        es2.push(msg("e", Some("d"), json!({"role":"user","content":[{"type":"text","text":big}],"timestamp":4})));
        let path: Vec<&Value> = es2.iter().collect();
        let p = prepare(&path, &Settings { enabled: true, reserve: 16384, keep: 50 }).unwrap();
        assert_eq!(p.first_kept, "e");
        assert_eq!(p.to_summarize.len(), 2);
        assert_eq!(serialize(&p.to_summarize), "[User]: hi\n\n[Assistant]: yo");
    }

    #[test]
    fn overflow_patterns() {
        let e = json!({"stopReason":"error","errorMessage":"400 prompt is too long: 210000 tokens > 200000 maximum","usage":{}});
        assert!(is_context_overflow(&e, 0));
        let r = json!({"stopReason":"error","errorMessage":"429 rate limit: too many tokens","usage":{}});
        assert!(!is_context_overflow(&r, 0));
    }
}
