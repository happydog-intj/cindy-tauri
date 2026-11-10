// 会话：完整的条目树（id/parentId）+ 当前叶子；messages 是活动分支上 message 条目的缓存。
// 文件格式为 version 3 的 JSONL。文件在第一条消息产生时才落盘（空会话不留文件）。
use crate::util::{iso_now, short_id};
use serde_json::{json, Map, Value};
use std::io::Write;
use std::path::PathBuf;

pub struct Session {
    pub id: String,
    pub file: PathBuf,
    pub cwd: String,
    pub header: Value,
    pub entries: Vec<Value>,
    pub leaf: Option<String>,
    pub messages: Vec<Value>,
    pub name: Option<String>,
    flushed: usize,
    header_written: bool,
    dir: PathBuf,
}

fn session_dir(dir: Option<&str>) -> PathBuf {
    match dir {
        Some(d) if !d.is_empty() => d.into(),
        _ => crate::util::agent_dir().join("sessions"),
    }
}

impl Session {
    pub fn new(dir: Option<&str>, cwd: &str, provider: &str, model: &str, thinking: &str) -> Session {
        Self::create(dir, cwd, None, provider, model, thinking)
    }

    fn create(dir: Option<&str>, cwd: &str, parent: Option<&str>, provider: &str, model: &str, thinking: &str) -> Session {
        let id = uuid::Uuid::now_v7().to_string();
        let ts = iso_now();
        let dir = session_dir(dir);
        let file = dir.join(format!("{}_{}.jsonl", ts.replace([':', '.'], "-"), id));
        let mut header = json!({"type":"session","version":3,"id":id,"timestamp":ts,"cwd":cwd});
        if let Some(p) = parent {
            header["parentSession"] = p.into();
        }
        let mut s = Session { id, file, cwd: cwd.into(), header, entries: vec![], leaf: None, messages: vec![], name: None, flushed: 0, header_written: false, dir };
        s.entry("model_change", json!({"provider":provider,"modelId":model}));
        s.entry("thinking_level_change", json!({"thinkingLevel":thinking}));
        s
    }

    /// 追加一个条目：type、id、parentId、timestamp 在前，extra 的字段在后。返回新条目 id。
    pub fn entry(&mut self, kind: &str, extra: Value) -> String {
        let id = short_id();
        let mut m = Map::new();
        m.insert("type".into(), kind.into());
        m.insert("id".into(), id.clone().into());
        m.insert("parentId".into(), self.leaf.clone().map(Value::from).unwrap_or(Value::Null));
        m.insert("timestamp".into(), iso_now().into());
        if let Value::Object(e) = extra {
            m.extend(e);
        }
        self.entries.push(Value::Object(m));
        self.leaf = Some(id.clone());
        id
    }

    pub fn append_message(&mut self, m: Value) {
        self.messages.push(m.clone());
        self.entry("message", json!({ "message": m }));
        self.flush();
    }

    /// 追加压缩条目（字段顺序同原版），随后按投影重建消息。systemMessage 取压缩前的当前系统消息。
    pub fn append_compaction(&mut self, summary: &str, first_kept: &str, tokens_before: u64, details: Value, usage: Value) -> String {
        let mut extra = json!({"summary":summary,"firstKeptEntryId":first_kept,"tokensBefore":tokens_before,"details":details,"usage":usage,"fromHook":false});
        if let Some(mut sm) = crate::compact::current_system(&self.messages) {
            sm["timestamp"] = crate::util::now_ms().into();
            extra["systemMessage"] = sm;
        }
        let id = self.entry("compaction", extra);
        self.rebuild();
        self.flush();
        id
    }

    /// 追加 context_edit：replacement 为 None 表示从上下文中去掉目标条目。
    pub fn append_context_edit(&mut self, target: &str, replacement: Option<Value>) -> Value {
        let id = self.entry("context_edit", json!({"targetId":target,"replacement":replacement.unwrap_or(Value::Null)}));
        self.rebuild();
        self.flush();
        self.entries.iter().find(|e| e["id"] == id.as_str()).cloned().unwrap_or(Value::Null)
    }

    pub fn path(&self) -> Vec<&Value> {
        self.branch(self.leaf.as_deref())
    }

    pub fn set_name(&mut self, name: &str) {
        self.name = Some(name.into());
        self.entry("session_info", json!({ "name": name }));
        if self.header_written {
            self.flush();
        }
    }

    pub fn flush(&mut self) {
        if self.flushed >= self.entries.len() && self.header_written {
            return;
        }
        let _ = std::fs::create_dir_all(&self.dir);
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&self.file) {
            if !self.header_written {
                let _ = writeln!(f, "{}", self.header);
                self.header_written = true;
            }
            for e in &self.entries[self.flushed..] {
                let _ = writeln!(f, "{e}");
            }
            self.flushed = self.entries.len();
        }
    }

    /// 从某个条目回溯到根的路径（根在前）。
    pub fn branch(&self, leaf: Option<&str>) -> Vec<&Value> {
        let mut out = vec![];
        let mut cur = leaf.map(String::from);
        while let Some(id) = cur {
            let Some(e) = self.entries.iter().find(|e| e["id"] == id.as_str()) else { break };
            out.push(e);
            cur = e["parentId"].as_str().map(String::from);
        }
        out.reverse();
        out
    }

    fn rebuild(&mut self) {
        self.messages = {
            let b = self.branch(self.leaf.as_deref());
            crate::compact::proj_messages(&crate::compact::project(&b))
        };
        self.name = self.entries.iter().rev().find(|e| e["type"] == "session_info").and_then(|e| e["name"].as_str().map(String::from));
    }

    /// 新会话，带上 upto 之前（含）的活动分支条目；条目 id 保持不变。
    fn derive(&self, upto: Option<&str>) -> Session {
        let (p, m) = self.current_model();
        let mut s = Session::create(Some(self.dir.to_str().unwrap_or("")), &self.cwd, Some(&self.file.to_string_lossy()), &p, &m, "off");
        s.entries.clear();
        s.entries = self.branch(upto).into_iter().cloned().collect();
        s.leaf = upto.map(String::from);
        s.rebuild();
        s
    }

    /// 导航到某个条目（无摘要）：用户消息/自定义消息停在其父条目上并返回消息文本，其他条目停在自己上面。
    /// 返回 (editorText, 是否变化)。
    pub fn navigate(&mut self, target: &str) -> Result<(Option<String>, bool), String> {
        if self.leaf.as_deref() == Some(target) { return Ok((None, false)); }
        let e = self.entries.iter().find(|e| e["id"] == target).cloned().ok_or_else(|| format!("Entry {target} not found"))?;
        let (new_leaf, text) = if e["type"] == "message" && e["message"]["role"] == "user" {
            (e["parentId"].as_str().map(String::from), Some(text_of(&e["message"]["content"])))
        } else if e["type"] == "custom_message" {
            (e["parentId"].as_str().map(String::from), Some(text_of(&e["content"])))
        } else { (Some(target.to_string()), None) };
        self.leaf = new_leaf;
        self.rebuild();
        Ok((text, true))
    }

    pub fn current_model(&self) -> (String, String) {
        self.entries.iter().rev().find(|e| e["type"] == "model_change")
            .map(|e| (e["provider"].as_str().unwrap_or("").into(), e["modelId"].as_str().unwrap_or("").into())).unwrap_or_default()
    }

    pub fn current_thinking(&self) -> String {
        self.entries.iter().rev().find(|e| e["type"] == "thinking_level_change").and_then(|e| e["thinkingLevel"].as_str().map(String::from)).unwrap_or_else(|| "off".into())
    }

    pub fn clone_branch(&self) -> Session {
        self.derive(self.leaf.as_deref())
    }

    /// 从某条用户消息分叉：新会话停在它的父条目上，返回该消息文本。
    pub fn fork_at(&self, entry_id: &str) -> Result<(Session, String), String> {
        let e = self.entries.iter().find(|e| e["id"] == entry_id && e["type"] == "message" && e["message"]["role"] == "user").ok_or_else(|| "Invalid entry ID for forking".to_string())?;
        let text = text_of(&e["message"]["content"]);
        Ok((self.derive(e["parentId"].as_str()), text))
    }

    pub fn fork_messages(&self) -> Vec<Value> {
        self.branch(self.leaf.as_deref()).iter().filter(|e| e["type"] == "message" && e["message"]["role"] == "user")
            .map(|e| json!({"entryId": e["id"], "text": text_of(&e["message"]["content"])})).collect()
    }

    pub fn load(path: &str) -> Result<Session, String> {
        let text = std::fs::read_to_string(path).map_err(|e| format!("Cannot read session file: {e}"))?;
        let mut lines = text.lines().filter(|l| !l.trim().is_empty());
        let header: Value = serde_json::from_str(lines.next().ok_or("Empty session file")?).map_err(|e| e.to_string())?;
        if header["type"] != "session" {
            return Err("Invalid session header".into());
        }
        let entries: Vec<Value> = lines.filter_map(|l| serde_json::from_str(l).ok()).collect();
        let file = PathBuf::from(path);
        let mut s = Session {
            id: header["id"].as_str().unwrap_or("").into(), cwd: header["cwd"].as_str().unwrap_or("").into(), header,
            leaf: entries.last().and_then(|e| e["id"].as_str().map(String::from)), flushed: entries.len(), entries,
            messages: vec![], name: None, header_written: true, dir: file.parent().map(|p| p.to_path_buf()).unwrap_or_default(), file,
        };
        s.rebuild();
        Ok(s)
    }

    pub fn tree(&self) -> Vec<Value> {
        fn build(entries: &[Value], parent: Option<&str>, known: &std::collections::HashSet<&str>, root: bool) -> Vec<Value> {
            entries.iter().filter(|e| {
                let p = e["parentId"].as_str();
                if root { p.map_or(true, |p| !known.contains(p)) } else { p == parent }
            }).map(|e| json!({"entry": e, "children": build(entries, e["id"].as_str(), known, false)})).collect()
        }
        let known: std::collections::HashSet<&str> = self.entries.iter().filter_map(|e| e["id"].as_str()).collect();
        build(&self.entries, None, &known, true)
    }

    pub fn stats(&self, context_window: u64) -> Value {
        let path = self.path();
        let msgs: Vec<&Value> = path.iter().filter(|e| e["type"] == "message").map(|e| &e["message"]).collect();
        let count = |role: &str| msgs.iter().filter(|m| m["role"] == role).count();
        let (mut i, mut o, mut cr, mut cw, mut cost) = (0u64, 0u64, 0u64, 0u64, 0f64);
        let mut tool_calls = 0;
        let mut add = |u: &Value| {
            i += u["input"].as_u64().unwrap_or(0);
            o += u["output"].as_u64().unwrap_or(0);
            cr += u["cacheRead"].as_u64().unwrap_or(0);
            cw += u["cacheWrite"].as_u64().unwrap_or(0);
            cost += u["cost"]["total"].as_f64().unwrap_or(0.0);
        };
        for e in &path {
            match e["type"].as_str() {
                Some("message") => {
                    let m = &e["message"];
                    if m["role"] == "assistant" {
                        add(&m["usage"]);
                        tool_calls += m["content"].as_array().map_or(0, |c| c.iter().filter(|b| b["type"] == "toolCall").count());
                    } else if m["role"] == "toolResult" && m["usage"].is_object() {
                        add(&m["usage"]);
                    }
                }
                Some("compaction") | Some("branch_summary") | Some("usage") if e["usage"].is_object() => add(&e["usage"]),
                _ => {}
            }
        }
        let mut o_json = json!({
            "sessionFile": self.file, "sessionId": self.id,
            "userMessages": count("user"), "assistantMessages": count("assistant"),
            "toolCalls": tool_calls, "toolResults": count("toolResult"), "totalMessages": msgs.len(),
            "tokens": {"input":i,"output":o,"cacheRead":cr,"cacheWrite":cw,"total":i+o+cr+cw},
            "cost": cost,
        });
        if context_window > 0 { o_json["contextUsage"] = self.context_usage(context_window); }
        o_json
    }

    /// getContextUsage：压缩后在出现新的带 usage 的回复之前，tokens 和 percent 为 null。
    fn context_usage(&self, cw: u64) -> Value {
        let path = self.path();
        let proj = crate::compact::project(&path);
        if let Some(ci) = path.iter().rposition(|e| e["type"] == "compaction") {
            let ok = |m: &Value| m["role"] == "assistant" && !matches!(m["stopReason"].as_str(), Some("aborted" | "error")) && crate::compact::calc_ctx(&m["usage"]) > 0;
            let ids: std::collections::HashSet<&str> = proj.iter().filter(|e| e.msgs.iter().any(ok)).filter_map(|e| e.src["id"].as_str()).collect();
            if !path[ci + 1..].iter().any(|e| e["id"].as_str().map_or(false, |i| ids.contains(i))) {
                return json!({"tokens": null, "contextWindow": cw, "percent": null});
            }
        }
        let t = crate::compact::estimate_projected(&proj, &path);
        json!({"tokens": t, "contextWindow": cw, "percent": t as f64 / cw as f64 * 100.0})
    }
}

pub fn text_of(c: &Value) -> String {
    match c {
        Value::String(s) => s.clone(),
        Value::Array(a) => a.iter().filter(|b| b["type"] == "text").filter_map(|b| b["text"].as_str()).collect(),
        _ => String::new(),
    }
}
