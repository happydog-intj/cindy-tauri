//! AgentEvent → messages 行，对应 sidecar/src/local-db/persist.mjs（逐函数移植，注释里的函数名同 JS）。
//! 写库原语走 `Store`，测试与生产用同一份逻辑。

use super::mapper::{js_trim, truthy, utf16_len};
use serde_json::{json, Map, Value};
use std::collections::HashMap;

const TOOL_RESULT_PERSIST_CONTENT_LIMIT: usize = 8 * 1024;
const TOOL_RESULT_PERSIST_TRUNCATION_SUFFIX: &str = "\n\n[tool result truncated: stored first 8KB]";

pub fn cap_tool_result_text_for_persist(text: &str) -> String {
    let units: Vec<u16> = text.encode_utf16().collect();
    if units.len() <= TOOL_RESULT_PERSIST_CONTENT_LIMIT {
        return text.to_string();
    }
    let mut cut = TOOL_RESULT_PERSIST_CONTENT_LIMIT.saturating_sub(utf16_len(TOOL_RESULT_PERSIST_TRUNCATION_SUFFIX));
    if cut > 0 && (0xd800..=0xdbff).contains(&units[cut - 1]) {
        cut -= 1;
    }
    format!("{}{}", String::from_utf16_lossy(&units[..cut]), TOOL_RESULT_PERSIST_TRUNCATION_SUFFIX)
}

pub struct NewRow {
    pub client_id: String,
    pub role: String,
    pub content: Value,
    pub tool_use_id: Option<String>,
    /// `row.agentMeta ? JSON.stringify : null`：Some(对象) 即使是空对象也写入。
    pub agent_meta: Option<Value>,
    pub created_at: i64,
}

/// 同步写库原语（对应 createEventPersister 的 store 参数）。
pub trait Store {
    fn insert(&mut self, sid: &str, row: NewRow) -> Option<Value>;
    fn update_content(&mut self, sid: &str, client_id: &str, content: &Value) -> Option<Value>;
    fn latest_created_at(&mut self, sid: &str) -> Option<i64>;
    fn provider_id_of(&mut self, sid: &str) -> Option<String>;
    fn note_turn_ended(&mut self, sid: &str, at: i64);
    fn emit_error(&mut self, sid: &str, persist_id: &str);
    fn emit_row(&mut self, sid: &str, message: &Value);
    fn now_ms(&self) -> i64;
    /// 24 字节随机数（createId 用）。
    fn random_bytes(&mut self, n: usize) -> Vec<u8>;
}

/// cuid 形态 id：小写字母开头、24 位 [a-z0-9]。
pub fn create_id(st: &mut dyn Store) -> String {
    const A: &[u8] = b"abcdefghijklmnopqrstuvwxyz0123456789";
    let b = st.random_bytes(24);
    let mut out = String::with_capacity(24);
    out.push(A[(b[0] % 26) as usize] as char);
    for x in &b[1..24] {
        out.push(A[(*x % 36) as usize] as char);
    }
    out
}

const PROXY_SERVER_CARRYING_TOOLS: [&str; 5] =
    ["cindy_browser", "cindy_mcp_call_tool", "mcp__cindy_browser__call_tool", "mcp:cindy_browser:call_tool", "mcp:cindy_browser"];

fn redact_tool_input(tool_name: &str, input: Option<&Value>) -> Option<Value> {
    let input = input?;
    if !PROXY_SERVER_CARRYING_TOOLS.contains(&tool_name) {
        return Some(input.clone());
    }
    match input {
        Value::String(s) if s.contains("proxyServer") => Some(json!("[redacted]")),
        Value::Object(o) if o.contains_key("proxyServer") => {
            let mut o = o.clone();
            o.insert("proxyServer".into(), json!("[redacted]"));
            Some(Value::Object(o))
        }
        other => Some(other.clone()),
    }
}

fn mark_failed_tool_result_text(text: &str, is_error: bool) -> String {
    if !is_error || text.contains("<tool_use_error>") {
        return text.to_string();
    }
    format!("<tool_use_error>{text}</tool_use_error>")
}

pub fn is_terminal_error(data: &Map<String, Value>) -> bool {
    if let Some(b) = data.get("isTerminal").and_then(Value::as_bool) {
        return b;
    }
    if let Some(b) = data.get("willRetry").and_then(Value::as_bool) {
        return !b;
    }
    true
}

fn has_meta(v: Option<&Value>) -> bool {
    match v {
        Some(Value::Object(o)) => !o.is_empty(),
        Some(Value::Array(a)) => !a.is_empty(),
        _ => false,
    }
}

fn str_of<'a>(m: &'a Map<String, Value>, k: &str) -> Option<&'a str> {
    m.get(k).and_then(Value::as_str)
}

#[derive(Clone, Default)]
struct Last {
    role: &'static str,
    persist_id: String,
    text: Option<String>,
    agent_message_id: Option<String>,
}
struct Block {
    persist_id: String,
    text: String,
    agent_message_id: Option<String>,
    agent_meta: Option<Value>,
    created_at: i64,
}
struct Boundary {
    persist_id: String,
    text: String,
    agent_message_id: Option<String>,
}
struct Interaction {
    persist_id: String,
    kind: String,
    request: Value,
    resolved: bool,
}

#[derive(Default)]
struct SessionState {
    block: Option<Block>,
    last_agent_meta: Option<Value>,
    last_persisted: Option<Last>,
    boundary_flushed: Option<Boundary>,
    tool_use_created_at: HashMap<String, i64>,
    tool_use_persist_id: HashMap<String, String>,
    tool_result_id_by_tool_use_id: HashMap<String, String>,
    /// 保持插入顺序（JS Map）：toolUseId → (全文, createdAt)。
    pending_full_text: Vec<(String, (String, i64))>,
    tool_result_content: HashMap<String, String>,
    interactions: HashMap<String, Interaction>,
}

#[derive(Default)]
pub struct Persister {
    states: HashMap<String, SessionState>,
}

fn spread_with(meta: Option<&Value>, key: &str, val: Value) -> Value {
    let mut o = match meta {
        Some(Value::Object(o)) => o.clone(),
        _ => Map::new(),
    };
    o.insert(key.to_string(), val);
    Value::Object(o)
}

fn clamp_after_tool_use(s: &SessionState, tool_use_id: &str, created_at: i64) -> i64 {
    match s.tool_use_created_at.get(tool_use_id) {
        Some(t) if created_at <= *t => t + 1,
        _ => created_at,
    }
}

fn persist_row(st: &mut dyn Store, sid: &str, row: NewRow, broadcast: bool) -> Option<Value> {
    let msg = st.insert(sid, row);
    if let (Some(m), true) = (&msg, broadcast) {
        st.emit_row(sid, m);
    }
    msg
}

impl Persister {
    fn state(&mut self, sid: &str) -> &mut SessionState {
        self.states.entry(sid.to_string()).or_default()
    }
    pub fn forget(&mut self, sid: &str) {
        self.states.remove(sid);
    }

    /// enqueuePersistAssistant。
    fn persist_assistant(&mut self, st: &mut dyn Store, sid: &str, persist_id: &str, text: &str, agent_meta: Option<&Value>, created_at: i64, agent_message_id: Option<&str>) {
        let meta = match agent_message_id {
            Some(id) => Some(spread_with(agent_meta, "agentMessageId", json!(id))),
            None => agent_meta.cloned(),
        };
        persist_row(st, sid, NewRow { client_id: persist_id.into(), role: "assistant".into(), content: json!(text), tool_use_id: None, agent_meta: meta, created_at }, true);
        self.state(sid).last_persisted =
            Some(Last { role: "assistant", persist_id: persist_id.into(), text: Some(text.into()), agent_message_id: agent_message_id.map(String::from) });
    }

    /// flushAssistantBlockInternal。
    fn flush_assistant_block(&mut self, st: &mut dyn Store, sid: &str, fallback: Option<&Value>) -> Option<String> {
        let block = self.state(sid).block.take()?;
        if block.text.is_empty() {
            return None;
        }
        let meta = block.agent_meta.clone().or_else(|| fallback.cloned()).or_else(|| self.state(sid).last_agent_meta.clone());
        self.persist_assistant(st, sid, &block.persist_id, &block.text, meta.as_ref(), block.created_at, block.agent_message_id.as_deref());
        self.state(sid).boundary_flushed =
            Some(Boundary { persist_id: block.persist_id.clone(), text: block.text, agent_message_id: block.agent_message_id });
        Some(block.persist_id)
    }

    /// assembleAssistantTextEvent。
    fn on_text(&mut self, st: &mut dyn Store, sid: &str, data: &Map<String, Value>, agent_meta: Option<Value>) -> Option<String> {
        let mut agent_meta = agent_meta;
        if data.get("phase").map(Value::is_string).unwrap_or(false) || data.get("runtimeRecovery") == Some(&json!(true)) {
            let phase = if data.get("runtimeRecovery") == Some(&json!(true)) { json!("commentary") } else { data["phase"].clone() };
            agent_meta = Some(spread_with(agent_meta.as_ref(), "assistantPhase", phase));
        }
        let raw_text = str_of(data, "text").unwrap_or("").to_string();
        let is_final = data.get("isFinal") == Some(&json!(true));
        let is_full_text = data.get("isFullText") == Some(&json!(true));
        let agent_message_id = str_of(data, "agentMessageId").filter(|s| !s.is_empty()).map(String::from);

        let differs = {
            let s = self.state(sid);
            matches!((&s.block, &agent_message_id), (Some(b), Some(id)) if b.agent_message_id.as_deref().map(|x| !x.is_empty()).unwrap_or(false) && b.agent_message_id.as_deref() != Some(id.as_str()))
        };
        if differs {
            self.flush_assistant_block(st, sid, None);
        }

        if is_final {
            let s = self.state(sid);
            if let Some(block) = s.block.as_mut() {
                // 流式确认：不落库，留给边界 flush；权威全文或更长前缀才覆盖累积文本。
                let longer_prefix = utf16_len(&raw_text) > utf16_len(&block.text) && raw_text.starts_with(&block.text);
                if is_full_text || longer_prefix {
                    block.text = raw_text;
                }
                if agent_meta.is_some() {
                    block.agent_meta = agent_meta;
                }
                return Some(block.persist_id.clone());
            }
            // 交互边界紧随 flush 落库时，同源全文快照复用已落库行。
            let at_boundary = matches!(s.last_persisted.as_ref().map(|l| l.role), Some("ask_user") | Some("plan_review"));
            if let Some(bf) = s.boundary_flushed.as_mut() {
                let same_id = agent_message_id.is_none() || bf.agent_message_id == agent_message_id;
                if !raw_text.is_empty() && at_boundary && same_id && (bf.text == raw_text || is_full_text) {
                    let bf_id = bf.persist_id.clone();
                    if is_full_text && bf.text != raw_text {
                        bf.text = raw_text.clone();
                        if let Some(updated) = st.update_content(sid, &bf_id, &json!(raw_text)) {
                            st.emit_row(sid, &updated);
                        }
                    }
                    return Some(bf_id);
                }
            }
            if !raw_text.is_empty() {
                if let Some(last) = &self.state(sid).last_persisted {
                    if last.role == "assistant" && last.text.as_deref() == Some(raw_text.as_str()) && (agent_message_id.is_none() || last.agent_message_id == agent_message_id) {
                        return Some(last.persist_id.clone()); // 相邻 DUP-SKIP
                    }
                }
                let persist_id = create_id(st);
                let now = st.now_ms();
                self.persist_assistant(st, sid, &persist_id, &raw_text, agent_meta.as_ref(), now, agent_message_id.as_deref());
                return Some(persist_id);
            }
            return None;
        }

        if self.state(sid).block.is_none() {
            self.state(sid).boundary_flushed = None;
            let persist_id = create_id(st);
            let now = st.now_ms();
            self.state(sid).block = Some(Block { persist_id, text: raw_text, agent_message_id, agent_meta, created_at: now });
        } else {
            let b = self.state(sid).block.as_mut().unwrap();
            b.text.push_str(&raw_text);
            if agent_meta.is_some() {
                b.agent_meta = agent_meta;
            }
        }
        Some(self.state(sid).block.as_ref().unwrap().persist_id.clone())
    }

    /// onToolUseEvent。
    fn on_tool_use(&mut self, st: &mut dyn Store, sid: &str, data: &Map<String, Value>, agent_meta: Option<Value>) -> String {
        let created_at = st.now_ms();
        let tool_use_id = str_of(data, "toolUseId").unwrap_or("").to_string();
        let tool_name = str_of(data, "toolName").unwrap_or("").to_string();
        let mut content = Map::new();
        content.insert("toolUseId".into(), json!(tool_use_id));
        content.insert("toolName".into(), json!(tool_name));
        // input 为 undefined 时 JSON.stringify 会省掉这个键。
        if let Some(i) = redact_tool_input(&tool_name, data.get("input")) {
            content.insert("input".into(), i);
        }
        let content = Value::Object(content);
        let existing = if tool_use_id.is_empty() { None } else { self.state(sid).tool_use_persist_id.get(&tool_use_id).cloned() };
        if let Some(existing) = existing {
            // 同一 toolUseId 再次到达：改写原行。
            st.update_content(sid, &existing, &content);
            self.state(sid).last_persisted = Some(Last { role: "tool_use", persist_id: existing.clone(), ..Default::default() });
            return existing;
        }
        let persist_id = create_id(st);
        let meta = agent_meta.or_else(|| self.state(sid).last_agent_meta.clone());
        persist_row(
            st,
            sid,
            NewRow { client_id: persist_id.clone(), role: "tool_use".into(), content, tool_use_id: (!tool_use_id.is_empty()).then(|| tool_use_id.clone()), agent_meta: meta, created_at },
            true,
        );
        let s = self.state(sid);
        if !tool_use_id.is_empty() {
            s.tool_use_created_at.insert(tool_use_id.clone(), created_at);
            s.tool_use_persist_id.insert(tool_use_id, persist_id.clone());
        }
        s.last_persisted = Some(Last { role: "tool_use", persist_id: persist_id.clone(), ..Default::default() });
        persist_id
    }

    /// onToolResultEvent（turn scope）。
    fn on_tool_result(&mut self, st: &mut dyn Store, sid: &str, data: &Map<String, Value>, agent_meta: Option<Value>) -> String {
        let summary = str_of(data, "summary").unwrap_or("").to_string();
        let ids: Vec<String> = data
            .get("toolUseIds")
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(Value::as_str).filter(|s| !s.is_empty()).map(String::from).collect())
            .unwrap_or_default();
        let mut created_at = st.now_ms();
        let mut content = summary;
        {
            let s = self.state(sid);
            for id in &ids {
                if let Some(pos) = s.pending_full_text.iter().position(|(k, _)| k == id) {
                    let (text, at) = s.pending_full_text[pos].1.clone();
                    if utf16_len(&text) > utf16_len(&content) {
                        content = text;
                        created_at = clamp_after_tool_use(s, id, at);
                    }
                    s.pending_full_text.remove(pos);
                }
            }
            for id in &ids {
                created_at = clamp_after_tool_use(s, id, created_at);
            }
        }
        let existing = {
            let s = self.state(sid);
            ids.iter().find_map(|id| s.tool_result_id_by_tool_use_id.get(id).cloned())
        };
        if let Some(existing) = existing {
            let prev = {
                let s = self.state(sid);
                for id in &ids {
                    s.tool_result_id_by_tool_use_id.insert(id.clone(), existing.clone());
                }
                s.tool_result_content.get(&existing).cloned()
            };
            match prev {
                Some(prev) if utf16_len(&content) > utf16_len(&prev) => {
                    self.state(sid).tool_result_content.insert(existing.clone(), content.clone());
                    if cap_tool_result_text_for_persist(&content) != cap_tool_result_text_for_persist(&prev) {
                        st.update_content(sid, &existing, &json!(cap_tool_result_text_for_persist(&content)));
                    }
                    self.state(sid).last_persisted = Some(Last { role: "tool_result", persist_id: existing.clone(), ..Default::default() });
                    return existing;
                }
                _ => return existing, // prev 缺失或没变长：不写
            }
        }
        let persist_id = create_id(st);
        let meta = agent_meta.or_else(|| self.state(sid).last_agent_meta.clone());
        {
            let s = self.state(sid);
            for id in &ids {
                s.tool_result_id_by_tool_use_id.insert(id.clone(), persist_id.clone());
            }
            s.tool_result_content.insert(persist_id.clone(), content.clone());
        }
        persist_row(
            st,
            sid,
            NewRow { client_id: persist_id.clone(), role: "tool_result".into(), content: json!(cap_tool_result_text_for_persist(&content)), tool_use_id: ids.first().cloned(), agent_meta: meta, created_at },
            true,
        );
        self.state(sid).last_persisted = Some(Last { role: "tool_result", persist_id: persist_id.clone(), ..Default::default() });
        persist_id
    }

    /// onToolResultFullEvent（turn scope）。
    fn on_tool_result_full(&mut self, st: &mut dyn Store, sid: &str, data: &Map<String, Value>, agent_meta: Option<Value>) -> Option<String> {
        let tool_use_id = str_of(data, "toolUseId").unwrap_or("").to_string();
        let raw = str_of(data, "fullText")?.to_string();
        if tool_use_id.is_empty() {
            return None;
        }
        let full = mark_failed_tool_result_text(&raw, data.get("isError") == Some(&json!(true)));
        let created_at = st.now_ms();
        let target = self.state(sid).tool_result_id_by_tool_use_id.get(&tool_use_id).cloned();
        let Some(target) = target else {
            if self.state(sid).tool_use_persist_id.contains_key(&tool_use_id) {
                // tool_use 已到、摘要未到：eager-create。
                let persist_id = create_id(st);
                let meta = agent_meta.or_else(|| self.state(sid).last_agent_meta.clone());
                let at = {
                    let s = self.state(sid);
                    s.tool_result_id_by_tool_use_id.insert(tool_use_id.clone(), persist_id.clone());
                    s.pending_full_text.retain(|(k, _)| k != &tool_use_id);
                    s.tool_result_content.insert(persist_id.clone(), full.clone());
                    clamp_after_tool_use(s, &tool_use_id, created_at)
                };
                persist_row(
                    st,
                    sid,
                    NewRow { client_id: persist_id.clone(), role: "tool_result".into(), content: json!(cap_tool_result_text_for_persist(&full)), tool_use_id: Some(tool_use_id), agent_meta: meta, created_at: at },
                    true,
                );
                self.state(sid).last_persisted = Some(Last { role: "tool_result", persist_id: persist_id.clone(), ..Default::default() });
                return Some(persist_id);
            }
            let s = self.state(sid);
            match s.pending_full_text.iter_mut().find(|(k, _)| k == &tool_use_id) {
                Some(e) => e.1 = (full, created_at),
                None => s.pending_full_text.push((tool_use_id, (full, created_at))),
            }
            return None;
        };
        let prev = self.state(sid).tool_result_content.get(&target).cloned();
        if prev.as_deref() == Some(full.as_str()) {
            return None;
        }
        self.state(sid).tool_result_content.insert(target.clone(), full.clone());
        let capped = cap_tool_result_text_for_persist(&full);
        if prev.is_none() || prev.as_deref().map(|p| cap_tool_result_text_for_persist(p) != capped).unwrap_or(true) {
            st.update_content(sid, &target, &json!(capped));
        }
        self.state(sid).last_persisted = Some(Last { role: "tool_result", persist_id: target.clone(), ..Default::default() });
        Some(target)
    }

    /// onThinkingEvent：只有 final / redacted 落库，clientId = blockId。
    fn on_thinking(&mut self, st: &mut dyn Store, sid: &str, data: &Map<String, Value>, agent_meta: Option<Value>) {
        let block_id = str_of(data, "blockId").unwrap_or("").to_string();
        if block_id.is_empty() {
            return;
        }
        let meta = agent_meta.or_else(|| self.state(sid).last_agent_meta.clone());
        let finished_at = st.now_ms();
        match str_of(data, "stage") {
            Some("final") => {
                let text = str_of(data, "text").unwrap_or("");
                let duration = data.get("durationMs").filter(|v| v.is_number()).cloned().unwrap_or(json!(0));
                let d = duration.as_f64().unwrap_or(0.0).max(0.0);
                persist_row(
                    st,
                    sid,
                    NewRow {
                        client_id: block_id.clone(),
                        role: "thinking".into(),
                        content: json!({ "kind": "thinking", "text": text, "durationMs": duration, "isRedacted": false, "finishedAt": finished_at }),
                        tool_use_id: None,
                        agent_meta: meta,
                        created_at: (finished_at as f64 - d) as i64,
                    },
                    true,
                );
                self.state(sid).last_persisted = Some(Last { role: "thinking", persist_id: block_id, ..Default::default() });
            }
            Some("redacted") => {
                persist_row(
                    st,
                    sid,
                    NewRow {
                        client_id: block_id.clone(),
                        role: "thinking".into(),
                        content: json!({ "kind": "thinking", "text": "", "durationMs": 0, "isRedacted": true, "finishedAt": finished_at }),
                        tool_use_id: None,
                        agent_meta: meta,
                        created_at: finished_at,
                    },
                    true,
                );
                self.state(sid).last_persisted = Some(Last { role: "thinking", persist_id: block_id, ..Default::default() });
            }
            _ => {}
        }
    }

    /// flushOrphanToolResults：done 时把只有全文、没有摘要的结果落成行。
    fn flush_orphan_tool_results(&mut self, st: &mut dyn Store, sid: &str, agent_meta: Option<&Value>) {
        let pending = std::mem::take(&mut self.state(sid).pending_full_text);
        for (tool_use_id, (text, at)) in pending {
            let persist_id = create_id(st);
            let meta = agent_meta.cloned().or_else(|| self.state(sid).last_agent_meta.clone());
            let created_at = {
                let s = self.state(sid);
                s.tool_result_id_by_tool_use_id.insert(tool_use_id.clone(), persist_id.clone());
                s.tool_result_content.insert(persist_id.clone(), text.clone());
                clamp_after_tool_use(s, &tool_use_id, at)
            };
            persist_row(
                st,
                sid,
                NewRow { client_id: persist_id, role: "tool_result".into(), content: json!(cap_tool_result_text_for_persist(&text)), tool_use_id: Some(tool_use_id), agent_meta: meta, created_at },
                true,
            );
        }
    }

    /// onTurnErrorEvent + buildTurnErrorContent + enqueueTurnErrorRow。error 行不广播 messages:created。
    fn on_turn_error(&mut self, st: &mut dyn Store, sid: &str, data: Option<&Value>, agent_meta: Option<&Value>) -> Option<String> {
        let data = data.and_then(Value::as_object);
        let message = data.and_then(|d| str_of(d, "message")).unwrap_or("");
        if message.is_empty() {
            return None;
        }
        let data = data.unwrap();
        let ordered_at = st.now_ms();
        self.flush_assistant_block(st, sid, agent_meta);
        let mut content = Map::new();
        content.insert("message".into(), json!(message));
        if let Some(r) = str_of(data, "reason").filter(|s| !s.is_empty()) {
            content.insert("reason".into(), json!(r));
        }
        if let Some(r) = str_of(data, "sdkError").filter(|s| !s.is_empty()) {
            content.insert("sdkError".into(), json!(r));
        }
        if let Some(t) = data.get("toolLoop").filter(|v| v.is_object() || v.is_array()) {
            content.insert("toolLoop".into(), t.clone());
        }
        if let Some(p) = st.provider_id_of(sid).filter(|s| !s.is_empty()) {
            content.insert("providerId".into(), json!(p));
        }
        let latest = st.latest_created_at(sid);
        let created_at = latest.map(|l| l.min(ordered_at)).unwrap_or(ordered_at) + 1;
        let persist_id = create_id(st);
        let meta = agent_meta.cloned().or_else(|| self.state(sid).last_agent_meta.clone());
        persist_row(st, sid, NewRow { client_id: persist_id.clone(), role: "error".into(), content: Value::Object(content), tool_use_id: None, agent_meta: meta, created_at }, false);
        st.emit_error(sid, &persist_id);
        Some(persist_id)
    }

    /// resetTurnPersistState 的子集。
    fn reset_turn(&mut self, sid: &str) {
        let s = self.state(sid);
        s.block = None;
        s.boundary_flushed = None;
        s.tool_use_created_at.clear();
        s.tool_use_persist_id.clear();
        s.tool_result_id_by_tool_use_id.clear();
        s.pending_full_text.clear();
        s.tool_result_content.clear();
    }

    /// sessionEventTerminal：flush assistant → orphan tool_result → error 行 → reset → 记 turn 结束。
    fn on_terminal(&mut self, st: &mut dyn Store, sid: &str, event: &Value, agent_meta: Option<&Value>) -> Option<String> {
        self.flush_assistant_block(st, sid, agent_meta);
        self.flush_orphan_tool_results(st, sid, agent_meta);
        let persist_id = if event["type"] == "error" { self.on_turn_error(st, sid, event.get("data"), agent_meta) } else { None };
        self.reset_turn(sid);
        let now = st.now_ms();
        st.note_turn_ended(sid, now);
        persist_id
    }

    /// onInteractionMessage：ask_user / plan_review 落 pending 行，permission 不建行。
    pub fn persist_interaction_request(&mut self, st: &mut dyn Store, sid: &str, req: &Value) -> Option<String> {
        let request_id = req.get("requestId").and_then(Value::as_str).unwrap_or("").to_string();
        if request_id.is_empty() {
            return None;
        }
        if let Some(k) = self.state(sid).interactions.get(&request_id) {
            return Some(k.persist_id.clone());
        }
        let kind = req.get("kind").and_then(Value::as_str).unwrap_or("");
        let plan_ok = kind == "plan_review" && req.get("plan").and_then(Value::as_str).map(|p| !p.is_empty()).unwrap_or(false);
        if kind != "ask_user_question" && !plan_ok {
            return None;
        }
        self.flush_assistant_block(st, sid, None);
        let mut meta = match self.state(sid).last_agent_meta.clone() {
            Some(Value::Object(o)) => o,
            _ => Map::new(),
        };
        meta.remove("autoReviewUserText");
        let persist_id = create_id(st);
        let created_at = st.now_ms();
        let (role, content) = if kind == "ask_user_question" {
            let q = req.get("questions").filter(|v| !v.is_null()).cloned().unwrap_or(json!([]));
            ("ask_user", json!({ "requestId": request_id, "questions": q, "status": "pending", "answers": null }))
        } else {
            let pfp = req.get("planFilePath").and_then(Value::as_str).unwrap_or("");
            ("plan_review", json!({ "requestId": request_id, "plan": req["plan"], "planFilePath": pfp, "status": "pending", "feedback": null }))
        };
        persist_row(st, sid, NewRow { client_id: persist_id.clone(), role: role.into(), content, tool_use_id: None, agent_meta: Some(Value::Object(meta)), created_at }, true);
        let s = self.state(sid);
        s.interactions.insert(request_id, Interaction { persist_id: persist_id.clone(), kind: kind.to_string(), request: req.clone(), resolved: false });
        s.last_persisted = Some(Last { role, persist_id: persist_id.clone(), ..Default::default() });
        Some(persist_id)
    }

    /// onInteractionResolved：把 pending 行回写成最终状态并广播权威行。
    pub fn persist_interaction_resolved(&mut self, st: &mut dyn Store, sid: &str, request_id: &str, decision: &Value) -> Option<Value> {
        let (persist_id, kind, request) = {
            let e = self.state(sid).interactions.get_mut(request_id)?;
            if e.resolved {
                return None;
            }
            e.resolved = true;
            (e.persist_id.clone(), e.kind.clone(), e.request.clone())
        };
        let d = decision.as_object().cloned().unwrap_or_default();
        let content = if kind == "ask_user_question" {
            let q = request.get("questions").filter(|v| !v.is_null()).cloned().unwrap_or(json!([]));
            let answers = d.get("answers").filter(|v| !v.is_null()).cloned().unwrap_or(json!({}));
            json!({ "requestId": request_id, "questions": q, "status": if d.get("dismissed") == Some(&json!(true)) { "cancelled" } else { "answered" }, "answers": answers })
        } else {
            let allow = d.get("behavior") == Some(&json!("allow"));
            let dismissed = !allow && d.get("dismissed") == Some(&json!(true));
            let plan = match d.get("editedPlan") {
                Some(Value::String(s)) => json!(s),
                _ => request.get("plan").filter(|v| !v.is_null()).cloned().unwrap_or(json!("")),
            };
            let pfp = request.get("planFilePath").and_then(Value::as_str).unwrap_or("");
            let feedback = if !allow && !dismissed { d.get("reason").filter(|v| !v.is_null()).cloned().unwrap_or(Value::Null) } else { Value::Null };
            json!({
                "requestId": request_id, "plan": plan, "planFilePath": pfp,
                "status": if allow { "approved" } else if dismissed { "cancelled" } else { "revised" },
                "feedback": feedback,
            })
        };
        let updated = st.update_content(sid, &persist_id, &content);
        if let Some(u) = &updated {
            st.emit_row(sid, u);
        }
        updated
    }

    /// persistSessionStreamEvent + sessionEventTerminal 的分派。返回 persistId 或 None。
    pub fn persist_maker_event(&mut self, st: &mut dyn Store, sid: &str, event: &Value) -> Option<String> {
        let ev = event.as_object()?;
        let ty = ev.get("type").and_then(Value::as_str)?;
        if ev.get("turnScope").and_then(Value::as_str) == Some("background") {
            return None;
        }
        let agent_meta = has_meta(ev.get("agentMeta")).then(|| ev["agentMeta"].clone());
        if let Some(m) = &agent_meta {
            self.state(sid).last_agent_meta = Some(m.clone());
        }
        let empty = Map::new();
        let data = ev.get("data").and_then(Value::as_object).unwrap_or(&empty);
        match ty {
            "text" => {
                if ev.get("standaloneText") == Some(&json!(true)) {
                    let text = str_of(data, "text").unwrap_or("");
                    if text.is_empty() {
                        return None;
                    }
                    self.flush_assistant_block(st, sid, agent_meta.as_ref());
                    let persist_id = create_id(st);
                    let meta = agent_meta.clone().or_else(|| self.state(sid).last_agent_meta.clone());
                    let now = st.now_ms();
                    self.persist_assistant(st, sid, &persist_id, text, meta.as_ref(), now, None);
                    return Some(persist_id);
                }
                let persist_id = self.on_text(st, sid, data, agent_meta.clone());
                if ev.get("source").and_then(Value::as_str) == Some("pi") && data.get("isFinal") == Some(&json!(true)) && data.get("isFullText") == Some(&json!(true)) {
                    self.flush_assistant_block(st, sid, agent_meta.as_ref());
                }
                persist_id
            }
            "tool_use" => {
                self.flush_assistant_block(st, sid, agent_meta.as_ref());
                Some(self.on_tool_use(st, sid, data, agent_meta))
            }
            "tool_result" => Some(self.on_tool_result(st, sid, data, agent_meta)),
            "tool_result_full" => self.on_tool_result_full(st, sid, data, agent_meta),
            "thinking" => {
                self.on_thinking(st, sid, data, agent_meta);
                None
            }
            "interaction_request" => self.persist_interaction_request(st, sid, &Value::Object(data.clone())),
            "done" => self.on_terminal(st, sid, event, agent_meta.as_ref()),
            "error" => {
                if is_terminal_error(data) {
                    self.on_terminal(st, sid, event, agent_meta.as_ref())
                } else {
                    None
                }
            }
            _ => None,
        }
    }
}

#[allow(dead_code)]
fn _unused(v: &Value) -> bool {
    truthy(v) && !js_trim("").is_empty()
}
