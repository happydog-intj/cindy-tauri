//! projectHistoryView 的 Rust 移植（上游 historyViewProjection.ts + workRunGrouping.ts）。
use super::payload::has_visible_history_result;
use super::text::{content_to_preview, is_continuation_message, is_delivery_prose_text};
use crate::local_db::mapper::js_trim;
use serde_json::{Map, Value};
use std::collections::{HashMap, HashSet};

/// 分页游标按消息 id 切，上游在这里显式关闭尾部合并。
const FOLD_ANSWER_TAIL: bool = false;
const GAP_SPLIT_MS: f64 = 30.0 * 60.0 * 1000.0;

// ───────────── 行访问与小工具 ─────────────

fn s<'a>(row: &'a Value, k: &str) -> Option<&'a str> {
    row.get(k).and_then(Value::as_str)
}
fn role(row: &Value) -> &str {
    s(row, "role").unwrap_or("")
}
fn meta(row: &Value) -> Option<&Map<String, Value>> {
    row.get("agentMeta").and_then(Value::as_object)
}
fn truthy(v: Option<&Value>) -> bool {
    match v {
        None | Some(Value::Null) | Some(Value::Bool(false)) => false,
        Some(Value::String(x)) => !x.is_empty(),
        Some(Value::Number(n)) => n.as_f64().is_some_and(|f| f != 0.0 && !f.is_nan()),
        _ => true,
    }
}
fn meta_truthy(row: &Value, k: &str) -> bool {
    truthy(meta(row).and_then(|m| m.get(k)))
}
fn parent_uuid(row: &Value) -> Option<&str> {
    meta(row).and_then(|m| m.get("parentUuid")).and_then(Value::as_str).filter(|x| !x.is_empty())
}
fn ne(v: Option<&Value>) -> Option<String> {
    v.and_then(Value::as_str).filter(|x| !x.is_empty()).map(str::to_owned)
}
fn row_tool_use_id(row: &Value) -> Option<String> {
    row.get("toolUseId").and_then(Value::as_str).map(str::to_owned)
}

fn parse_ms(text: Option<&str>) -> f64 {
    text.and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok()).map_or(f64::NAN, |d| d.timestamp_millis() as f64)
}
fn finite(v: f64) -> Option<f64> {
    v.is_finite().then_some(v)
}
fn js_max(a: f64, b: f64) -> f64 {
    if a.is_nan() || b.is_nan() { f64::NAN } else { a.max(b) }
}
fn num(v: f64) -> Value {
    if !v.is_finite() {
        Value::Null
    } else if v.fract() == 0.0 && v.abs() < 9e15 {
        Value::from(v as i64)
    } else {
        serde_json::Number::from_f64(v).map_or(Value::Null, Value::Number)
    }
}

pub struct ToolUse {
    pub id: Option<String>,
    pub name: String,
    pub input: Value,
}

/// parseMessageToolUse
pub fn parse_tool_use(row: &Value) -> ToolUse {
    let content = row.get("content").and_then(Value::as_object);
    let id = ne(row.get("toolUseId")).or_else(|| ne(content.and_then(|c| c.get("toolUseId"))));
    let mut name = ne(content.and_then(|c| c.get("toolName"))).unwrap_or_default();
    let mut input = content.and_then(|c| c.get("input")).cloned().unwrap_or(Value::Null);
    if name == "cindy_mcp_call_tool" {
        let g = input.as_object();
        let server = ne(g.and_then(|g| g.get("server")));
        let tool = ne(g.and_then(|g| g.get("tool")));
        let args = g.and_then(|g| g.get("args"));
        if let (Some(server), Some(tool)) = (server, tool) {
            if args.is_none() || args.is_some_and(Value::is_object) {
                name = format!("mcp:{server}:{tool}");
                input = args.cloned().unwrap_or_else(|| Value::Object(Map::new()));
            }
        }
    }
    ToolUse { id, name, input }
}

/// describeToolUse 里 mcp 分支的 createdPath 是否存在。
fn has_file_delivery(row: &Value) -> bool {
    if role(row) != "tool_use" {
        return false;
    }
    let t = parse_tool_use(row);
    let n = &t.name;
    let is_mcp = if let Some(rest) = n.strip_prefix("mcp__") {
        let seg: Vec<&str> = rest.split("__").collect();
        seg.len() >= 2 && !seg[0].is_empty() && !seg[1..].join("__").is_empty()
    } else if n.starts_with("mcp:") {
        let seg: Vec<&str> = n.split(':').collect();
        seg.len() >= 3 && !seg[1].is_empty() && !seg[2..].join(":").is_empty()
    } else {
        false
    };
    is_mcp && t.input.as_object().is_some_and(|o| ["outPath", "out_path", "outputPath", "output_path"].iter().any(|k| ne(o.get(*k)).is_some()))
}

fn suffix_tool(name: &str, names: &[&str]) -> bool {
    names.iter().any(|n| {
        name.strip_suffix(n).is_some_and(|pre| pre.is_empty() || pre.ends_with(':') || pre.ends_with("__"))
    })
}

fn is_orca(name: &str) -> bool {
    let n = name.strip_prefix("mcp__").map_or_else(|| name.to_owned(), |r| format!("mcp:{r}")).replace("__", ":");
    matches!(n.as_str(), "mcp:orca_worker_bridge:send_to_lead" | "mcp:orca_worker_bridge:read_lead" | "mcp:orca_worker_bridge:lead_status") || matches!(name, "send_to_lead" | "read_lead" | "lead_status")
}

pub fn is_history_detail_tool(name: &str) -> bool {
    !name.is_empty()
        && !(matches!(name, "Agent" | "Task" | "subagent") || name.starts_with("collab:"))
        && !matches!(name, "TodoWrite" | "update_plan" | "TaskCreate" | "TaskUpdate" | "TaskList" | "TaskGet")
        && !is_orca(name)
        && !matches!(name, "AskUserQuestion" | "ExitPlanMode" | "Workflow" | "Edit" | "Write" | "MultiEdit" | "edit" | "write")
        && !suffix_tool(name, &["create_worker", "create_workers", "send_to_worker", "ghost_call"])
}

pub fn completes_history_turn(row: &Value) -> bool {
    let Some(m) = meta(row) else { return false };
    let positive = |v: Option<&Value>| v.and_then(Value::as_f64).is_some_and(|f| f.is_finite() && f > 0.0);
    let money = |v: Option<&Value>| positive(v.and_then(|x| x.get("amount")));
    m.get("turnCompleted") == Some(&Value::Bool(true))
        || money(m.get("userTurnCost"))
        || money(m.get("turnCost"))
        || positive(m.get("userTurnCostUsd"))
        || positive(m.get("turnCostUsd"))
        || m.get("turnUsageDetails").is_some_and(Value::is_object)
        || m.get("turnUsageDetails").is_some_and(Value::is_array)
}

fn is_steer(row: &Value) -> bool {
    s(row, "delivery") == Some("steer") || meta(row).and_then(|m| m.get("delivery")).and_then(Value::as_str) == Some("steer")
}

fn rows_hash<'a>(rows: impl Iterator<Item = &'a Value>) -> u32 {
    let mut h: u32 = 2166136261;
    for r in rows {
        let text = serde_json::to_string(r).unwrap_or_default();
        for c in text.encode_utf16() {
            h = (h ^ c as u32).wrapping_mul(16777619);
        }
    }
    h
}

fn is_live(row: &Value) -> bool {
    s(row, "id").is_some_and(|i| i.starts_with("history-live:"))
}

fn live_fields(rows: &[&Value], out: &mut Map<String, Value>) {
    if !rows.iter().any(|r| is_live(r)) {
        return;
    }
    let id = |r: &Value| Value::String(s(r, "id").unwrap_or("").to_owned());
    if let Some(r) = rows.iter().find(|r| !is_live(r)) {
        out.insert("firstStoredMessageId".into(), id(r));
    }
    if let Some(r) = rows.iter().rev().find(|r| !is_live(r)) {
        out.insert("lastStoredMessageId".into(), id(r));
    }
    out.insert("liveMessageIds".into(), Value::Array(rows.iter().filter(|r| is_live(r)).map(|r| id(r)).collect()));
}

// ───────────── 子代理范围 ─────────────

pub fn subagent_scopes(rows: &[Value], root: Option<&str>) -> HashMap<String, String> {
    let mut parents: HashMap<String, String> = HashMap::new();
    let mut agents: HashSet<String> = root.map(str::to_owned).into_iter().collect();
    for row in rows {
        if role(row) != "tool_use" {
            continue;
        }
        let t = parse_tool_use(row);
        let Some(id) = t.id else { continue };
        if t.name == "Agent" {
            agents.insert(id.clone());
        }
        if let Some(p) = parent_uuid(row) {
            parents.insert(id, p.to_owned());
        }
    }
    let mut out = HashMap::new();
    for row in rows {
        let mut parent = parent_uuid(row).map(str::to_owned).or_else(|| row_tool_use_id(row).filter(|x| !x.is_empty()).and_then(|id| parents.get(&id).cloned()));
        let Some(mut p) = parent.take() else { continue };
        if !agents.contains(&p) {
            continue;
        }
        let mut seen = HashSet::new();
        while let Some(next) = parents.get(&p) {
            if !agents.contains(next) || seen.contains(&p) {
                break;
            }
            seen.insert(p.clone());
            p = next.clone();
        }
        out.insert(s(row, "id").unwrap_or("").to_owned(), p);
    }
    out
}

// ───────────── 投影 ─────────────

#[derive(Clone, Copy)]
struct Src {
    activity: bool,
    archivable: bool,
}

struct Group {
    summary: Map<String, Value>,
    started: f64,
    ended: f64,
    children: Option<Vec<Value>>,
}

enum Out {
    Src(usize),
    Group(Group),
}

struct P<'a> {
    rows: &'a [Value],
    src: Vec<Src>,
}

impl P<'_> {
    fn start(&self, i: usize) -> Option<f64> {
        finite(parse_ms(s(&self.rows[i], "createdAt")))
    }
    fn row_end(&self, i: usize) -> f64 {
        let r = &self.rows[i];
        let started = parse_ms(s(r, "createdAt"));
        if role(r) != "thinking" {
            return started;
        }
        r.get("content").and_then(Value::as_object).and_then(|c| c.get("finishedAt")).and_then(Value::as_f64).filter(|f| f.is_finite()).unwrap_or(started)
    }
    fn is_answer(&self, i: usize) -> bool {
        let r = &self.rows[i];
        role(r) == "assistant" && s(r, "content").is_some_and(|c| !js_trim(c).is_empty())
    }
    fn sealed(&self, i: usize) -> bool {
        completes_history_turn(&self.rows[i])
    }
    fn can_prepend(&self, i: usize) -> bool {
        meta(&self.rows[i]).and_then(|m| m.get("assistantPhase")).and_then(Value::as_str) != Some("commentary")
    }
    fn group(&self, run: &[usize], active: bool, boundary: Option<f64>) -> Group {
        let first = &self.rows[run[0]];
        let last = &self.rows[*run.last().unwrap()];
        let anchor = run.iter().find(|&&i| self.src[i].activity && role(&self.rows[i]) != "tool_result").map_or(first, |&i| &self.rows[i]);
        let started = boundary.unwrap_or_else(|| parse_ms(s(first, "createdAt")));
        let ended = run.iter().fold(0.0, |e, &i| js_max(e, self.row_end(i)));
        let hash = rows_hash(run.iter().map(|&i| &self.rows[i]));
        let aid = match s(anchor, "clientId") {
            Some(c) if !c.is_empty() => c,
            _ => s(anchor, "id").unwrap_or(""),
        };
        let mut m = Map::new();
        m.insert("key".into(), format!("work-{aid}").into());
        m.insert("anchorClientId".into(), aid.into());
        m.insert("firstMessageId".into(), s(first, "id").unwrap_or("").into());
        m.insert("lastMessageId".into(), s(last, "id").unwrap_or("").into());
        let refs: Vec<&Value> = run.iter().map(|&i| &self.rows[i]).collect();
        live_fields(&refs, &mut m);
        let (st, en) = (if started.is_finite() { started } else { 0.0 }, if ended.is_finite() { ended } else { 0.0 });
        m.insert("startedAtMs".into(), num(st));
        m.insert("endedAtMs".into(), num(en));
        if run.iter().any(|&i| self.rows[i].get("historyArtifacts").and_then(Value::as_array).is_some_and(|a| !a.is_empty())) {
            let arts: Vec<Value> = run.iter().flat_map(|&i| self.rows[i].get("historyArtifacts").and_then(Value::as_array).cloned().unwrap_or_default()).collect();
            m.insert("artifacts".into(), Value::Array(arts));
        }
        m.insert("messageCount".into(), run.len().into());
        m.insert("toolCount".into(), run.iter().filter(|&&i| role(&self.rows[i]) == "tool_use").count().into());
        m.insert("isStreaming".into(), active.into());
        m.insert("revision".into(), format!("{}:{}:{}", s(last, "id").unwrap_or(""), run.len(), hash).into());
        Group { summary: m, started: st, ended: en, children: None }
    }
    fn nest(&self, mut g: Group, children: Vec<Value>) -> Group {
        g.summary.shift_remove("artifacts");
        g.children = Some(children);
        g
    }
    fn view_src(&self, i: usize) -> Value {
        let r = &self.rows[i];
        let key = match s(r, "clientId") {
            Some(c) if !c.is_empty() => c,
            _ => s(r, "id").unwrap_or(""),
        };
        let mut m = Map::new();
        m.insert("type".into(), "messages".into());
        m.insert("key".into(), key.into());
        m.insert("messages".into(), Value::Array(vec![r.clone()]));
        Value::Object(m)
    }
    fn view_group(&self, g: Group) -> Value {
        let mut m = Map::new();
        m.insert("type".into(), "work".into());
        m.insert("key".into(), g.summary.get("key").cloned().unwrap_or(Value::Null));
        m.insert("summary".into(), Value::Object(g.summary));
        if let Some(c) = g.children {
            m.insert("children".into(), Value::Array(c));
        }
        Value::Object(m)
    }
    fn view_out(&self, o: Out) -> Value {
        match o {
            Out::Src(i) => self.view_src(i),
            Out::Group(g) => self.view_group(g),
        }
    }
    fn activity_group(&self, run: &[usize], active: bool, boundary: Option<f64>) -> Group {
        let result = self.group(run, active, boundary);
        let calls: HashSet<Option<String>> = run.iter().filter(|&&i| role(&self.rows[i]) == "tool_use").map(|&i| parse_tool_use(&self.rows[i]).id).collect();
        let unpaired = |i: usize| role(&self.rows[i]) == "tool_result" && !calls.contains(&row_tool_use_id(&self.rows[i]));
        if run.iter().any(|&i| unpaired(i)) {
            let mut children = Vec::new();
            let mut seg: Vec<usize> = Vec::new();
            for &i in run {
                if unpaired(i) {
                    if !seg.is_empty() {
                        children.push(self.view_group(self.activity_group(&seg, active, boundary)));
                        seg.clear();
                    }
                    children.push(self.view_src(i));
                } else {
                    seg.push(i);
                }
            }
            if !seg.is_empty() {
                children.push(self.view_group(self.activity_group(&seg, active, boundary)));
            }
            return self.nest(result, children);
        }
        let mut result = result;
        if active {
            let idx: Vec<usize> = run.iter().enumerate().filter(|(_, &i)| matches!(role(&self.rows[i]), "thinking" | "tool_use")).map(|(k, _)| k).collect();
            let from = idx.get(idx.len().saturating_sub(5)).copied().unwrap_or(0);
            let mut pv = self.group(&run[from..], true, boundary).summary;
            let key = format!("preview-{}", result.summary.get("key").and_then(Value::as_str).unwrap_or(""));
            pv.insert("key".into(), key.into());
            result.summary.insert("preview".into(), Value::Object(pv));
        }
        result
    }
    fn completed_group(&self, run: &[usize], boundary: Option<f64>) -> Group {
        if !run.iter().any(|&i| !self.src[i].activity) {
            return self.activity_group(run, false, boundary);
        }
        let mut result = self.group(run, false, boundary);
        let a = result.summary.get("anchorClientId").and_then(Value::as_str).unwrap_or("").to_owned();
        result.summary.insert("key".into(), format!("work-summary-{a}").into());
        let mut children = Vec::new();
        let mut acts: Vec<usize> = Vec::new();
        let mut previous = boundary;
        for &i in run {
            if self.src[i].activity {
                acts.push(i);
            } else {
                if !acts.is_empty() {
                    children.push(self.view_group(self.activity_group(&acts, false, previous)));
                    acts.clear();
                }
                children.push(self.view_src(i));
                previous = self.start(i);
            }
        }
        if !acts.is_empty() {
            children.push(self.view_group(self.activity_group(&acts, false, previous)));
        }
        self.nest(result, children)
    }

    // ── workRunGrouping ──
    fn group_runs(&self, streaming: bool) -> Vec<Out> {
        let mut out: Vec<Out> = Vec::new();
        let mut turn: Vec<usize> = Vec::new();
        let mut turn_start: Option<f64> = None;
        let mut prev_end: Option<f64> = None;
        let mut flush = |this: &Self, out: &mut Vec<Out>, turn: &mut Vec<usize>, turn_start: Option<f64>, active_tail: bool, resumed: bool| {
            if turn.is_empty() {
                return;
            }
            if active_tail && streaming {
                let completed = turn.iter().rposition(|&i| this.is_answer(i) && this.sealed(i));
                let active_start = match completed {
                    Some(c) => this.start(turn[c]),
                    None => turn_start,
                };
                if let Some(c) = completed {
                    let done = &turn[..=c];
                    match this.answered_turn(done, turn_start, false) {
                        Some(v) => out.extend(v),
                        None => out.extend(done.iter().map(|&i| Out::Src(i))),
                    }
                }
                let rest = &turn[completed.map_or(0, |c| c + 1)..];
                out.extend(this.activity_runs(rest, active_start, true));
            } else {
                match this.answered_turn(turn, turn_start, resumed) {
                    Some(v) => out.extend(v),
                    None => out.extend(this.activity_runs(turn, turn_start, false)),
                }
            }
            turn.clear();
        };
        for i in 0..self.rows.len() {
            let start = self.start(i);
            if let (Some(pe), Some(st)) = (prev_end, start) {
                if st - pe > GAP_SPLIT_MS {
                    flush(self, &mut out, &mut turn, turn_start, false, false);
                    turn_start = None;
                }
            }
            let r = &self.rows[i];
            if role(r) == "user" && !is_steer(r) {
                flush(self, &mut out, &mut turn, turn_start, false, is_continuation_message(r));
                out.push(Out::Src(i));
                prev_end = start.or(prev_end);
                turn_start = start;
                continue;
            }
            turn.push(i);
            if let Some(e) = finite(self.row_end(i)) {
                prev_end = Some(prev_end.map_or(e, |p| js_max(p, e)));
            }
        }
        flush(self, &mut out, &mut turn, turn_start, true, false);
        out
    }

    fn activity_runs(&self, items: &[usize], turn_start: Option<f64>, active: bool) -> Vec<Out> {
        let mut last_completed: isize = -1;
        if active {
            for (k, &i) in items.iter().enumerate() {
                if self.is_answer(i) || role(&self.rows[i]) == "system" {
                    last_completed = k as isize;
                }
            }
        }
        let mut out = Vec::new();
        let mut run: Vec<usize> = Vec::new();
        let mut run_last: isize = -1;
        let mut prev_boundary = turn_start;
        let flush = |out: &mut Vec<Out>, run: &mut Vec<usize>, run_last: isize, pb: Option<f64>| {
            if run.is_empty() {
                return;
            }
            out.push(Out::Group(self.activity_group(run, active && run_last > last_completed, pb)));
            run.clear();
        };
        for (k, &i) in items.iter().enumerate() {
            if self.src[i].activity {
                run.push(i);
                run_last = k as isize;
            } else {
                flush(&mut out, &mut run, run_last, prev_boundary);
                out.push(Out::Src(i));
                prev_boundary = self.start(i);
            }
        }
        flush(&mut out, &mut run, run_last, prev_boundary);
        out
    }

    fn answered_turn(&self, items: &[usize], turn_start: Option<f64>, resumed: bool) -> Option<Vec<Out>> {
        let mut answers: HashSet<usize> = HashSet::new();
        let mut sealed: Vec<usize> = Vec::new();
        let mut last_answer: isize = -1;
        for (k, &i) in items.iter().enumerate() {
            if !self.is_answer(i) {
                continue;
            }
            last_answer = k as isize;
            if self.sealed(i) {
                sealed.push(k);
            }
        }
        if last_answer < 0 {
            return None;
        }
        let act = |k: usize| self.src[items[k]].activity;
        if !sealed.is_empty() {
            let mut seg_start = 0usize;
            for &si in &sealed {
                let mut last_act: isize = -1;
                let mut k = si as isize - 1;
                while k >= seg_start as isize {
                    if act(k as usize) {
                        last_act = k;
                        break;
                    }
                    k -= 1;
                }
                let mut astart = si;
                while astart as isize > last_act + 1 && astart > seg_start && self.is_answer(items[astart - 1]) && self.can_prepend(items[astart - 1]) {
                    astart -= 1;
                }
                let mut keep = !resumed && Some(&si) == sealed.last();
                let mut k = astart;
                while k <= si && !keep {
                    keep = self.is_answer(items[k]) && !self.src[items[k]].archivable;
                    k += 1;
                }
                if keep {
                    for k in astart..=si {
                        if self.is_answer(items[k]) {
                            answers.insert(k);
                        }
                    }
                }
                seg_start = si + 1;
            }
        } else if !resumed {
            if (0..items.len()).any(|k| k as isize > last_answer && act(k)) {
                return None;
            }
            let la = last_answer as usize;
            let last_act = (0..la).rev().find(|&k| act(k)).map_or(-1, |k| k as isize);
            let mut astart = la;
            if last_act >= 0 {
                while astart as isize > last_act + 1 && self.is_answer(items[astart - 1]) && self.can_prepend(items[astart - 1]) {
                    astart -= 1;
                }
            }
            for k in astart..=la {
                if self.is_answer(items[k]) {
                    answers.insert(k);
                }
            }
        }

        struct Before {
            index: usize,
            run: Vec<usize>,
            boundary: Option<f64>,
            answer_ms: Option<f64>,
        }
        let mut out: Vec<Out> = Vec::new();
        let mut run: Vec<usize> = Vec::new();
        let mut prev_boundary = turn_start;
        let mut before: Option<Before> = None;
        for (k, &i) in items.iter().enumerate() {
            if !answers.contains(&k) && self.src[i].archivable {
                run.push(i);
                continue;
            }
            let group_index = out.len();
            // flushRun(item)
            let flushed = std::mem::take(&mut run);
            let mut grouped = false;
            if !flushed.is_empty() {
                let mut folded = false;
                if let Some(t) = &before {
                    if let (true, Some(am)) = (FOLD_ANSWER_TAIL, t.answer_ms) {
                        if flushed.iter().all(|&c| self.start(c).is_some_and(|sm| sm <= am)) {
                            let mut merged = t.run.clone();
                            merged.extend(&flushed);
                            let g = self.completed_group(&merged, t.boundary);
                            out[t.index] = Out::Group(g);
                            folded = true;
                        }
                    }
                }
                if folded {
                    before = None;
                } else {
                    out.push(Out::Group(self.completed_group(&flushed, prev_boundary)));
                    grouped = true;
                }
            }
            out.push(Out::Src(i));
            if !answers.contains(&k) {
                before = None;
            } else if grouped {
                before = Some(Before { index: group_index, run: flushed, boundary: prev_boundary, answer_ms: self.start(i) });
            } else if let Some(b) = &mut before {
                b.answer_ms = self.start(i);
            }
            prev_boundary = self.start(i);
        }
        if !run.is_empty() {
            // 末尾 flushRun()：无下一项，foldTail 仍可合并
            let flushed = std::mem::take(&mut run);
            let mut folded = false;
            if let Some(t) = &before {
                if let (true, Some(am)) = (FOLD_ANSWER_TAIL, t.answer_ms) {
                    if flushed.iter().all(|&c| self.start(c).is_some_and(|sm| sm <= am)) {
                        let mut merged = t.run.clone();
                        merged.extend(&flushed);
                        out[t.index] = Out::Group(self.completed_group(&merged, t.boundary));
                        folded = true;
                    }
                }
            }
            if !folded {
                out.push(Out::Group(self.completed_group(&flushed, prev_boundary)));
            }
        }
        Some(out)
    }
}

fn source_view(rows: &[Value], streaming: bool, preserve_mixed: bool) -> Vec<Value> {
    let mut names: HashMap<String, String> = HashMap::new();
    let mut visible_results: HashSet<String> = HashSet::new();
    let mut settled: HashSet<String> = HashSet::new();
    let mut nested_calls: HashSet<String> = HashSet::new();
    for row in rows {
        if role(row) == "tool_use" {
            let t = parse_tool_use(row);
            if let Some(id) = &t.id {
                names.insert(id.clone(), t.name.clone());
                if !preserve_mixed && has_file_delivery(row) {
                    visible_results.insert(id.clone());
                }
                if meta_truthy(row, "mcpApp") {
                    visible_results.insert(id.clone());
                }
                if meta_truthy(row, "parentUuid") {
                    nested_calls.insert(id.clone());
                }
            }
        }
        if role(row) == "tool_result" {
            if let Some(id) = ne(row.get("toolUseId")) {
                settled.insert(id.clone());
                if has_visible_history_result(row.get("content").unwrap_or(&Value::Null)) || meta_truthy(row, "mcpApp") {
                    visible_results.insert(id);
                }
            }
        }
    }
    let mut src: Vec<Src> = rows
        .iter()
        .map(|row| {
            let tool = (role(row) == "tool_use").then(|| parse_tool_use(row));
            let tool_id: Option<String> = match &tool {
                Some(t) if t.id.is_some() => t.id.clone(),
                _ => ne(row.get("toolUseId")),
            };
            let tool_name: String = match &tool {
                Some(t) => t.name.clone(),
                None => tool_id.as_ref().and_then(|i| names.get(i)).cloned().unwrap_or_default(),
            };
            let nested = parent_uuid(row).is_some() || tool_id.as_ref().is_some_and(|i| nested_calls.contains(i));
            let visible_thinking = role(row) == "thinking"
                && match row.get("content") {
                    Some(Value::String(c)) => !c.is_empty(),
                    Some(Value::Object(o)) => truthy(o.get("text")) || truthy(o.get("durationMs")) || o.get("isRedacted") == Some(&Value::Bool(true)),
                    _ => false,
                };
            let is_tool_row = matches!(role(row), "tool_use" | "tool_result");
            let edit = matches!(tool_name.as_str(), "Edit" | "Write" | "MultiEdit" | "edit" | "write");
            let ghost_settled = tool_id.as_ref().is_some_and(|i| settled.contains(i)) && suffix_tool(&tool_name, &["ghost_call"]);
            let hidden_by_visible = tool_id.as_ref().is_some_and(|i| visible_results.contains(i));
            let activity = !nested && (visible_thinking || (is_tool_row && (is_history_detail_tool(&tool_name) || (!preserve_mixed && (edit || ghost_settled))) && !hidden_by_visible));
            let plain_assistant = !nested
                && role(row) == "assistant"
                && row.get("content").is_some_and(Value::is_string)
                && !["botCollaboration", "botDirectMessage", "systemCardType", "goalCompletion", "goalNotice", "reviewRun", "pluginModelRoute"].iter().any(|k| meta_truthy(row, k));
            let archivable = activity || (plain_assistant && !is_delivery_prose_text(s(row, "content").unwrap_or("")));
            Src { activity, archivable }
        })
        .collect();
    // 媒体 / 卡片的摆放归原渲染器的整段工具序列：含非活动项的混合段整体保持为源行。
    let mut seg: Vec<usize> = Vec::new();
    let flush = |seg: &mut Vec<usize>, src: &mut Vec<Src>| {
        let has_visible = seg.iter().any(|&i| role(&rows[i]) == "tool_result" && has_visible_history_result(rows[i].get("content").unwrap_or(&Value::Null)));
        if (preserve_mixed || has_visible) && seg.iter().any(|&i| !src[i].activity) {
            for &i in seg.iter() {
                src[i] = Src { activity: false, archivable: false };
            }
        }
        seg.clear();
    };
    for (i, row) in rows.iter().enumerate() {
        if matches!(role(row), "tool_use" | "tool_result") {
            seg.push(i);
        } else {
            flush(&mut seg, &mut src);
        }
    }
    flush(&mut seg, &mut src);
    let p = P { rows, src };
    p.group_runs(streaming).into_iter().map(|o| p.view_out(o)).collect()
}

/// projectHistoryView：`lazy_details` 为 true 时把子代理行折进 `deferred`。
pub fn project_history_view(rows: &[Value], streaming: bool, lazy_details: bool) -> Vec<Value> {
    if !lazy_details {
        return source_view(rows, streaming, true);
    }
    let scopes = subagent_scopes(rows, None);
    let mut visible_tools: HashSet<String> = HashSet::new();
    for row in rows {
        if role(row) == "tool_result" && has_visible_history_result(row.get("content").unwrap_or(&Value::Null)) {
            if let Some(id) = row_tool_use_id(row) {
                visible_tools.insert(id);
            }
        }
        if has_file_delivery(row) {
            if let Some(id) = parse_tool_use(row).id {
                visible_tools.insert(id);
            }
        }
    }
    let mut children: HashMap<String, Vec<&Value>> = HashMap::new();
    let mut visible: Vec<&Value> = Vec::new();
    for row in rows {
        let parent = scopes.get(s(row, "id").unwrap_or(""));
        let tool_id = if role(row) == "tool_use" { parse_tool_use(row).id } else { row_tool_use_id(row) };
        let keep = parent.is_none() || tool_id.as_ref().is_some_and(|i| !i.is_empty() && visible_tools.contains(i));
        match (keep, parent) {
            (false, Some(p)) => children.entry(p.clone()).or_default().push(row),
            _ => visible.push(row),
        }
    }
    let mut results: HashMap<String, &Value> = HashMap::new();
    for row in rows {
        if role(row) == "tool_result" {
            if let Some(id) = row_tool_use_id(row) {
                results.insert(id, row);
            }
        }
    }
    let with_artifacts = |row: &Value| -> Value {
        let Some(arts) = row.get("historyArtifacts").and_then(Value::as_array).filter(|a| !a.is_empty()) else { return row.clone() };
        let mapped: Vec<Value> = arts
            .iter()
            .filter(|f| {
                let id = ne(f.get("toolUseId"));
                match id.and_then(|i| results.get(&i).copied()) {
                    Some(r) => !content_to_preview(r.get("content").unwrap_or(&Value::Null)).contains("<tool_use_error>"),
                    None => true,
                }
            })
            .map(|f| {
                let mut o = f.as_object().cloned().unwrap_or_default();
                let ready = match ne(f.get("toolUseId")) {
                    Some(i) => results.contains_key(&i),
                    None => true,
                };
                o.insert("ready".into(), Value::Bool(ready));
                Value::Object(o)
            })
            .collect();
        let mut r = row.clone();
        r["historyArtifacts"] = Value::Array(mapped);
        r
    };
    let visible_rows: Vec<Value> = visible.iter().map(|r| with_artifacts(r)).collect();
    let projected = source_view(&visible_rows, streaming, false);
    projected.into_iter().map(|it| attach(it, &children, streaming, &with_artifacts)).collect()
}

fn attach(item: Value, children: &HashMap<String, Vec<&Value>>, streaming: bool, with_artifacts: &dyn Fn(&Value) -> Value) -> Value {
    let mut item = item;
    if item.get("type").and_then(Value::as_str) == Some("work") {
        if let Some(Value::Array(c)) = item.get("children").cloned() {
            let next: Vec<Value> = c.into_iter().map(|x| attach(x, children, streaming, with_artifacts)).collect();
            item["children"] = Value::Array(next);
        }
        return item;
    }
    let Some(header) = item.get("messages").and_then(Value::as_array).and_then(|m| m.iter().find(|r| role(r) == "tool_use")).cloned() else { return item };
    let Some(tool_id) = parse_tool_use(&header).id else { return item };
    let Some(body) = children.get(&tool_id).filter(|b| !b.is_empty()) else { return item };
    let (first, last) = (body[0], body[body.len() - 1]);
    let model = body.iter().filter_map(|r| meta(r)).find(|m| m.get("parentUuid").and_then(Value::as_str) == Some(tool_id.as_str()) && m.get("model").and_then(Value::as_str).is_some_and(|x| !x.is_empty())).and_then(|m| m.get("model").cloned());
    let mut d = Map::new();
    let cid = s(&header, "clientId").unwrap_or("");
    d.insert("key".into(), format!("subagent-work-{cid}").into());
    d.insert("anchorClientId".into(), cid.into());
    d.insert("parentToolUseId".into(), tool_id.clone().into());
    if let Some(m) = model {
        d.insert("model".into(), m);
    }
    d.insert("firstMessageId".into(), s(first, "id").unwrap_or("").into());
    d.insert("lastMessageId".into(), s(last, "id").unwrap_or("").into());
    live_fields(body, &mut d);
    d.insert("startedAtMs".into(), num(parse_ms(s(first, "createdAt"))));
    d.insert("endedAtMs".into(), num(parse_ms(s(last, "createdAt"))));
    let arts: Vec<Value> = body.iter().flat_map(|r| with_artifacts(r).get("historyArtifacts").and_then(Value::as_array).cloned().unwrap_or_default()).collect();
    d.insert("artifacts".into(), Value::Array(arts));
    d.insert("messageCount".into(), body.len().into());
    d.insert("toolCount".into(), body.iter().filter(|r| role(r) == "tool_use").count().into());
    d.insert("isStreaming".into(), streaming.into());
    d.insert("revision".into(), format!("{}:{}:{}", s(last, "id").unwrap_or(""), body.len(), rows_hash(body.iter().copied())).into());
    item["deferred"] = Value::Object(d);
    item
}
