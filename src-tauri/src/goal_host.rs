//! Goal 控制器（会话内自主续跑 /goal）。原 sidecar/src/goal.mjs 的移植，纯函数部分在 goal_core.rs。
//!
//! 数据流：HostSink 把每个 maker 事件交给 `on_event`、回合收尾时调 `note_session_idle`；回合结束时按裁决决定
//! 要不要用定时器发下一轮（经注册表的 maker:input:enqueue，与 renderer 同路径）。状态落 `session_goals` 表（同一个
//! 本地库文件，Rust 是它唯一的写入方）。锁只在读写内存状态时短暂持有，发送 / 落库期间不持锁：
//! 会话入队可能同步回调 sink，再进到这里。

use crate::goal_core::*;
use crate::maker_registry::fail;
use regex::Regex;
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

pub const GOAL_CHANNELS: [&str; 8] = [
    "maker:goal:set",
    "maker:goal:clear",
    "maker:goal:pause",
    "maker:goal:resume",
    "maker:goal:update",
    "maker:goal:get-status",
    "maker:list-desktop-commands",
    "maker:execute-desktop-command",
];

const STATUS_CHANGED: &str = "maker:goal:status-changed";
const FIRE_DEBOUNCE_MS: u64 = 150;
const MAX_OBJECTIVE_CHARS: usize = 4000;
const GOAL_DESCRIPTION: &str = include_str!("goal_desc.txt");

/// 控制器需要的外部能力。生产实现在 maker_wiring.rs，测试用假实现。
pub trait GoalPorts: Send + Sync + 'static {
    /// 本地库文件（sidecar 建表与迁移，这里只读写 session_goals / 读 sessions 行）。
    fn db_path(&self) -> Result<PathBuf, String>;
    fn in_turn(&self, session_id: &str) -> bool;
    /// 入队一条消息（与 renderer 的 maker:input:enqueue 同路径）。
    fn enqueue(&self, session_id: &str, item: Value) -> Result<(), String>;
    fn abort(&self, session_id: &str);
    /// 推给 renderer（同时转给设备互联的手机端）。
    fn emit(&self, channel: &str, payload: Value);
    fn now_ms(&self) -> i64 {
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0)
    }
}

#[derive(Debug, Clone)]
pub struct GoalState {
    pub session_id: String,
    pub objective: String,
    pub status: String,
    pub budget_tokens: Option<i64>,
    pub max_turns: Option<i64>,
    pub no_progress_limit: Option<i64>,
    pub turns_used: i64,
    pub tokens_used: i64,
    pub no_progress_streak: i64,
    pub usage_reset_at: Option<i64>,
    pub last_reason: Option<String>,
    pub agent_kind: String,
    pub started_at: i64,
    pub updated_at: i64,
}

impl GoalState {
    /// renderer 看到的扁平状态（preload getGoalStatus 的返回形状）。
    pub fn to_payload(&self) -> Value {
        json!({
            "sessionId": self.session_id, "status": self.status, "objective": self.objective,
            "turnsUsed": self.turns_used, "tokensUsed": self.tokens_used, "maxTurns": self.max_turns,
            "noProgressLimit": self.no_progress_limit, "budgetTokens": self.budget_tokens,
            "usageResetAt": self.usage_reset_at, "lastReason": self.last_reason,
        })
    }
    fn prev(&self) -> Prev {
        Prev {
            turns_used: self.turns_used, tokens_used: self.tokens_used, no_progress_streak: self.no_progress_streak,
            budget_tokens: self.budget_tokens, max_turns: self.max_turns, no_progress_limit: self.no_progress_limit,
        }
    }
}

fn row_to_state(r: &rusqlite::Row) -> rusqlite::Result<GoalState> {
    Ok(GoalState {
        session_id: r.get("session_id")?, objective: r.get("objective")?, status: r.get("status")?,
        budget_tokens: r.get("budget_tokens")?, max_turns: r.get("max_turns")?, no_progress_limit: r.get("no_progress_limit")?,
        turns_used: r.get("turns_used")?, tokens_used: r.get("tokens_used")?, no_progress_streak: r.get("no_progress_streak")?,
        usage_reset_at: r.get("usage_reset_at")?, last_reason: r.get("last_reason")?, agent_kind: r.get("agent_kind")?,
        started_at: r.get("started_at")?, updated_at: r.get("updated_at")?,
    })
}

#[derive(Default)]
struct TurnAcc {
    text: String,
    saw_tool_use: bool,
    tokens: i64,
    finalized: bool,
}

#[derive(Default)]
struct Inner {
    turns: HashMap<String, TurnAcc>,
    /// 已经挂上事件观察的会话（JS 里的 attach：第一次发目标回合时挂上）。
    watching: HashSet<String>,
    /// 会话 → 当前有效的定时器令牌；令牌对不上的旧定时器醒来后什么都不做。
    timers: HashMap<String, u64>,
    firing: HashSet<String>,
    in_flight: HashSet<String>,
    overload: HashMap<String, u32>,
    next_token: u64,
    disposed: bool,
}

pub struct GoalController {
    ports: Arc<dyn GoalPorts>,
    conn: Mutex<Option<Connection>>,
    inner: Mutex<Inner>,
    me: Weak<GoalController>,
}

struct SessionRow {
    model: String,
    provider_id: Option<String>,
    working_dir: Option<String>,
    permission_mode: Option<String>,
    workspace_kind: Option<String>,
}

fn num(v: &Value) -> i64 {
    v.as_f64().map(|n| n as i64).unwrap_or(0)
}

fn normalize_limit(v: &Value, name: &str) -> Result<Option<i64>, String> {
    if v.is_null() {
        return Ok(None);
    }
    match v.as_f64().filter(|n| n.is_finite() && *n > 0.0) {
        Some(n) => Ok(Some(n.floor() as i64)),
        None => Err(fail("INVALID_PARAMS", format!("{name} must be a positive number or null"))),
    }
}

fn check_objective(text: &str) -> Result<(), String> {
    if text.is_empty() {
        return Err(fail("INVALID_PARAMS", "objective must not be empty"));
    }
    if text.encode_utf16().count() > MAX_OBJECTIVE_CHARS {
        return Err(fail("INVALID_PARAMS", format!("objective must be at most {MAX_OBJECTIVE_CHARS} characters")));
    }
    Ok(())
}

impl GoalController {
    pub fn new(ports: Arc<dyn GoalPorts>) -> Arc<Self> {
        Arc::new_cyclic(|me| GoalController { ports, conn: Mutex::new(None), inner: Mutex::new(Inner::default()), me: me.clone() })
    }

    // ── 存取 ──

    fn with_conn<T>(&self, f: impl FnOnce(&Connection) -> rusqlite::Result<T>) -> Result<T, String> {
        let mut g = self.conn.lock().unwrap();
        if g.is_none() {
            let path = self.ports.db_path()?;
            let c = Connection::open(path).map_err(|e| e.to_string())?;
            c.execute_batch("PRAGMA busy_timeout = 5000; PRAGMA foreign_keys = ON;").map_err(|e| e.to_string())?;
            *g = Some(c);
        }
        f(g.as_ref().unwrap()).map_err(|e| e.to_string())
    }

    pub fn get(&self, sid: &str) -> Option<GoalState> {
        match self.with_conn(|c| c.query_row("SELECT * FROM session_goals WHERE session_id = ?1", [sid], row_to_state).optional()) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("[goal] read failed: {e}");
                None
            }
        }
    }

    pub fn get_status(&self, sid: &str) -> Option<Value> {
        self.get(sid).map(|s| s.to_payload())
    }

    fn session_exists(&self, sid: &str) -> Result<bool, String> {
        self.with_conn(|c| c.query_row("SELECT 1 FROM sessions WHERE id = ?1", [sid], |_| Ok(())).optional().map(|v| v.is_some()))
    }

    fn session_row(&self, sid: &str) -> Result<Option<SessionRow>, String> {
        self.with_conn(|c| {
            c.query_row("SELECT model, provider_id, working_dir, permission_mode, workspace_kind FROM sessions WHERE id = ?1", [sid], |r| {
                Ok(SessionRow { model: r.get(0)?, provider_id: r.get(1)?, working_dir: r.get(2)?, permission_mode: r.get(3)?, workspace_kind: r.get(4)? })
            })
            .optional()
        })
    }

    /// 落库并推 status-changed。
    fn save(&self, mut s: GoalState) -> Result<GoalState, String> {
        s.updated_at = self.ports.now_ms();
        self.with_conn(|c| {
            c.execute(
                "INSERT INTO session_goals (session_id, objective, status, budget_tokens, max_turns, no_progress_limit, turns_used, tokens_used,
                   no_progress_streak, usage_reset_at, last_reason, agent_kind, started_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)
                 ON CONFLICT(session_id) DO UPDATE SET objective=excluded.objective, status=excluded.status, budget_tokens=excluded.budget_tokens,
                   max_turns=excluded.max_turns, no_progress_limit=excluded.no_progress_limit, turns_used=excluded.turns_used,
                   tokens_used=excluded.tokens_used, no_progress_streak=excluded.no_progress_streak, usage_reset_at=excluded.usage_reset_at,
                   last_reason=excluded.last_reason, agent_kind=excluded.agent_kind, started_at=excluded.started_at, updated_at=excluded.updated_at",
                params![s.session_id, s.objective, s.status, s.budget_tokens, s.max_turns, s.no_progress_limit, s.turns_used, s.tokens_used,
                    s.no_progress_streak, s.usage_reset_at, s.last_reason, s.agent_kind, s.started_at, s.updated_at],
            )
        })?;
        self.ports.emit(STATUS_CHANGED, json!({ "sessionId": s.session_id, "goal": s.to_payload() }));
        Ok(s)
    }

    /// 内部路径（事件回调、定时器）里落库失败只记日志，不能让回合收尾半途而废。
    fn save_logged(&self, s: GoalState) {
        if let Err(e) = self.save(s) {
            eprintln!("[goal] save failed: {e}");
        }
    }

    fn internal<T>(r: Result<T, String>) -> Result<T, String> {
        r.map_err(|e| {
            if e.starts_with('[') {
                e
            } else {
                eprintln!("[goal] unexpected error: {e}");
                "[INTERNAL] goal operation failed".to_string()
            }
        })
    }

    // ── 定时器 / 挂接 ──

    fn clear_timer(&self, sid: &str) {
        self.inner.lock().unwrap().timers.remove(sid);
    }

    fn detach(&self, sid: &str) {
        let mut g = self.inner.lock().unwrap();
        g.timers.remove(sid);
        g.watching.remove(sid);
        g.turns.remove(sid);
        g.in_flight.remove(sid);
    }

    fn schedule(&self, sid: &str, delay_ms: u64) {
        let token = {
            let mut g = self.inner.lock().unwrap();
            if g.disposed {
                return;
            }
            g.next_token += 1;
            let t = g.next_token;
            g.timers.insert(sid.to_string(), t);
            t
        };
        let me = self.me.clone();
        let sid = sid.to_string();
        let _ = std::thread::Builder::new().name("goal-timer".into()).spawn(move || {
            std::thread::sleep(Duration::from_millis(delay_ms));
            let Some(this) = me.upgrade() else { return };
            {
                let mut g = this.inner.lock().unwrap();
                if g.timers.get(&sid) != Some(&token) {
                    return;
                }
                g.timers.remove(&sid);
            }
            // 限流 / 过载等待结束：先回到 active 才会发下一轮。
            if let Some(w) = this.get(&sid).filter(|w| w.status == "usageLimited") {
                this.save_logged(GoalState { status: "active".into(), last_reason: None, usage_reset_at: None, ..w });
            }
            this.fire_turn(&sid);
        });
    }
}

// ── 事件 / 回合收尾 ──

fn tokens_of(event: &Value) -> i64 {
    let d = &event["data"];
    if let Some(u) = d.get("usage").filter(|u| u.is_object()) {
        let pick = |a: &str, b: &str| if !u[a].is_null() { num(&u[a]) } else if !u[b].is_null() { num(&u[b]) } else { 0 };
        let parts = pick("inputTokens", "promptTokens") + pick("outputTokens", "completionTokens");
        if parts > 0 {
            return parts;
        }
    }
    if let Some(models) = d["requestUsage"]["models"].as_array() {
        return models.iter().map(|m| num(&m["inputTokens"]) + num(&m["outputTokens"])).sum();
    }
    if d["tokenUsage"].is_number() { num(&d["tokenUsage"]) } else { 0 }
}

impl GoalController {
    /// HostSink 对每个 maker 事件调用一次（只有挂上的会话、active 的目标才关心）。
    pub fn on_event(&self, sid: &str, event: &Value) {
        {
            let g = self.inner.lock().unwrap();
            if g.disposed || event["turnScope"] == "background" || !g.watching.contains(sid) {
                return;
            }
        }
        if !self.get(sid).is_some_and(|s| s.status == "active") {
            return;
        }
        let goal_origin = event["turnOrigin"]["kind"] == "goal";
        match event["type"].as_str().unwrap_or("") {
            "text" => {
                if let Some(t) = event["data"]["text"].as_str() {
                    let mut g = self.inner.lock().unwrap();
                    let turn = g.turns.entry(sid.to_string()).or_default();
                    if event["data"]["isFinal"].as_bool().unwrap_or(false) {
                        turn.text = t.to_string();
                    } else {
                        turn.text.push_str(t);
                    }
                }
            }
            "tool_use" => {
                self.inner.lock().unwrap().turns.entry(sid.to_string()).or_default().saw_tool_use = true;
            }
            "done" => {
                {
                    let mut g = self.inner.lock().unwrap();
                    if goal_origin {
                        g.in_flight.remove(sid);
                    }
                    let turn = g.turns.entry(sid.to_string()).or_default();
                    let t = tokens_of(event);
                    if t > 0 {
                        turn.tokens = t;
                    }
                    // done.data.result 是整轮最终回复，比拼接的流式文本更可靠。
                    if let Some(r) = event["data"]["result"].as_str().filter(|r| !r.is_empty()) {
                        turn.text = r.to_string();
                    }
                }
                self.finalize_turn(sid, event, false);
            }
            "error" => {
                if goal_origin {
                    self.inner.lock().unwrap().in_flight.remove(sid);
                }
                self.finalize_turn(sid, event, true);
            }
            _ => {}
        }
    }

    fn finalize_turn(&self, sid: &str, event: &Value, errored: bool) {
        let (text, saw_tool_use, tokens) = {
            let mut g = self.inner.lock().unwrap();
            let Some(turn) = g.turns.get_mut(sid) else { return };
            if turn.finalized {
                return;
            }
            turn.finalized = true;
            (turn.text.clone(), turn.saw_tool_use, turn.tokens)
        };
        let Some(state) = self.get(sid).filter(|s| s.status == "active") else {
            self.detach(sid);
            return;
        };
        let origin_goal = event["turnOrigin"]["kind"] == "goal";
        let kind = if errored { classify_turn_error(&event["data"]) } else { None };
        let is_overload = errored && kind == Some("overload");
        let streak = {
            let mut g = self.inner.lock().unwrap();
            if !is_overload {
                g.overload.remove(sid);
                0
            } else {
                let n = g.overload.get(sid).copied().unwrap_or(0) + 1;
                g.overload.insert(sid.to_string(), n);
                n
            }
        };
        let exhausted = streak >= MAX_CONSECUTIVE_OVERLOAD_TURNS;
        let error_message = errored.then(|| {
            let d = &event["data"];
            let m = d["message"].as_str().map(String::from).or_else(|| d["error"]["message"].as_str().map(String::from)).unwrap_or_else(|| "unknown error".into());
            m.chars().take(300).collect::<String>()
        });
        let error_kind = if errored && !(is_overload && exhausted) { kind.map(String::from) } else { None };
        let outcome = Outcome {
            goal_turn: origin_goal,
            saw_tool_use,
            tokens_this_turn: tokens,
            verdict: if origin_goal { parse_verdict(&text) } else { None },
            errored,
            error_message,
            error_kind: error_kind.clone(),
        };
        let decision = decide_next_goal_state(&state.prev(), &outcome);
        self.inner.lock().unwrap().turns.insert(sid.to_string(), TurnAcc::default());
        // 模型澄清后回报的更具体的目标：替换存下的目标。
        let refined = outcome.verdict.as_ref().and_then(|v| v.refined.clone());
        let now = self.ports.now_ms();
        let reset_at = (decision.status == "usageLimited").then(|| {
            now + if error_kind.as_deref() == Some("overload") { OVERLOAD_RESUME_DELAY_MS } else { USAGE_LIMIT_DEFAULT_WAIT_MS }
        });
        let objective = match (&refined, origin_goal) {
            (Some(r), true) => String::from_utf16_lossy(&r.encode_utf16().take(MAX_OBJECTIVE_CHARS).collect::<Vec<u16>>()),
            _ => state.objective.clone(),
        };
        self.save_logged(GoalState {
            objective,
            status: decision.status.clone(),
            last_reason: decision.last_reason.clone(),
            turns_used: decision.turns_used,
            tokens_used: decision.tokens_used,
            no_progress_streak: decision.no_progress_streak,
            usage_reset_at: reset_at,
            ..state
        });
        if decision.should_fire {
            self.schedule(sid, FIRE_DEBOUNCE_MS);
        } else if decision.status == "usageLimited" {
            self.schedule(sid, (reset_at.unwrap_or(now) - now).max(1_000) as u64);
        } else if is_terminal(&decision.status) || decision.status == "paused" || decision.status == "blocked" {
            self.clear_timer(sid);
        }
    }

    /// 发下一轮：目标 active、会话空闲时才发；用户正在输入的回合结束后会再被调起。
    /// 先占位 firing 再做检查（检查要查库 / 问注册表，不能在持有内部锁时做）。
    fn fire_turn(&self, sid: &str) {
        {
            let mut g = self.inner.lock().unwrap();
            if g.disposed || !g.firing.insert(sid.to_string()) {
                return;
            }
        }
        if self.get(sid).is_some_and(|s| s.status == "active") && !self.ports.in_turn(sid) {
            self.fire_inner(sid);
        }
        self.inner.lock().unwrap().firing.remove(sid);
    }

    fn fire_inner(&self, sid: &str) {
        let Some(state) = self.get(sid) else { return };
        let block = |s: GoalState, reason: String| {
            self.save_logged(GoalState { status: "blocked".into(), last_reason: Some(reason), ..s });
            self.detach(sid);
        };
        let row = match self.session_row(sid) {
            Ok(Some(r)) => r,
            Ok(None) => return block(state, "unable to restore the agent session for Goal".into()),
            Err(e) => return block(state, format!("turn dispatch failed: {}", e.chars().take(200).collect::<String>())),
        };
        {
            let mut g = self.inner.lock().unwrap();
            g.watching.insert(sid.to_string());
            g.turns.entry(sid.to_string()).or_default();
        }
        let Some(current) = self.get(sid).filter(|s| s.status == "active") else { return };
        let content = if current.turns_used == 0 {
            first_turn_directive(&current.objective, current.max_turns)
        } else {
            continuation_directive(&current.objective, current.last_reason.as_deref())
        };
        {
            let mut g = self.inner.lock().unwrap();
            g.turns.insert(sid.to_string(), TurnAcc::default());
            g.in_flight.insert(sid.to_string());
        }
        let item = json!({
            "clientId": format!("goal:{}", uuid::Uuid::new_v4()),
            "text": content,
            "origin": { "kind": "goal", "goalSessionId": sid },
            "createOpts": {
                "agentKind": "pi", "id": sid, "model": row.model, "providerId": row.provider_id,
                "workingDir": row.working_dir, "permissionMode": row.permission_mode.unwrap_or_else(|| "ask".into()),
                "workspaceKind": if row.workspace_kind.as_deref() == Some("project") { "project" } else { "dialogue" },
            },
        });
        if let Err(reason) = self.ports.enqueue(sid, item) {
            self.inner.lock().unwrap().in_flight.remove(sid);
            // 会话刚好有别的回合：不算失败，回合结束会再调起。
            static BUSY: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
            if BUSY.get_or_init(|| Regex::new(r"(?i)busy|running|in.?turn").unwrap()).is_match(&reason) {
                return;
            }
            block(current, format!("turn dispatch failed: {reason}"));
        }
    }
}

// ── 对外操作 ──

fn str_arg<'a>(v: &'a Value, name: &str) -> Result<&'a str, String> {
    v.as_str().filter(|s| !s.is_empty()).ok_or_else(|| fail("INVALID_PARAMS", format!("{name} required")))
}

fn obj_arg<'a>(v: &'a Value, name: &str) -> Result<&'a serde_json::Map<String, Value>, String> {
    v.as_object().ok_or_else(|| fail("INVALID_PARAMS", format!("{name} must be an object")))
}

/// 与 @cindy/maker-shared/goal-command 的 parseGoalCommand 同规则。
#[derive(Debug, PartialEq)]
pub enum GoalCommand {
    OpenDialog,
    Clear,
    Set,
}

pub fn parse_goal_command(args: &str) -> GoalCommand {
    let o = args.trim();
    if o.is_empty() {
        GoalCommand::OpenDialog
    } else if ["clear", "stop", "off", "cancel", "reset", "none"].contains(&o.to_lowercase().as_str()) {
        GoalCommand::Clear
    } else {
        GoalCommand::Set
    }
}

pub fn owns(channel: &str) -> bool {
    GOAL_CHANNELS.contains(&channel)
}

impl GoalController {
    pub fn set_goal(&self, session_id: &str, objective: &str, limits: Option<&serde_json::Map<String, Value>>) -> Result<GoalState, String> {
        if self.inner.lock().unwrap().disposed {
            return Err(fail("PRECONDITION_FAILED", "goal controller has been disposed"));
        }
        let text = objective.trim();
        check_objective(text)?;
        if !self.session_exists(session_id)? {
            return Err(fail("NOT_FOUND", "session not found"));
        }
        // 默认限制（desktop 同口径）：不限轮数 / token，连续 3 轮没用工具就暂停。
        let (max_turns, budget_tokens, no_progress_limit) = match limits {
            Some(l) => (
                normalize_limit(l.get("maxTurns").unwrap_or(&Value::Null), "maxTurns")?,
                normalize_limit(l.get("budgetTokens").unwrap_or(&Value::Null), "budgetTokens")?,
                normalize_limit(l.get("noProgressLimit").unwrap_or(&Value::Null), "noProgressLimit")?,
            ),
            None => (None, None, Some(3)),
        };
        if self.inner.lock().unwrap().in_flight.contains(session_id) {
            self.ports.abort(session_id);
        }
        self.detach(session_id);
        self.inner.lock().unwrap().overload.remove(session_id);
        let t = self.ports.now_ms();
        let saved = self.save(GoalState {
            session_id: session_id.into(), objective: text.into(), status: "active".into(), budget_tokens, max_turns, no_progress_limit,
            turns_used: 0, tokens_used: 0, no_progress_streak: 0, usage_reset_at: None, last_reason: None, agent_kind: "pi".into(), started_at: t, updated_at: t,
        })?;
        self.schedule(session_id, FIRE_DEBOUNCE_MS);
        Ok(saved)
    }

    pub fn update_goal(&self, sid: &str, patch: &serde_json::Map<String, Value>) -> Result<Option<GoalState>, String> {
        let Some(mut next) = self.get(sid) else { return Ok(None) };
        if let Some(o) = patch.get("objective") {
            let text = o.as_str().unwrap_or("").trim().to_string();
            check_objective(&text)?;
            next.objective = text;
        }
        for key in ["maxTurns", "budgetTokens", "noProgressLimit"] {
            if let Some(v) = patch.get(key) {
                let n = normalize_limit(v, key)?;
                match key {
                    "maxTurns" => next.max_turns = n,
                    "budgetTokens" => next.budget_tokens = n,
                    _ => next.no_progress_limit = n,
                }
            }
        }
        self.save(next).map(Some)
    }

    pub fn clear_goal(&self, sid: &str) -> Result<(), String> {
        let state = self.get(sid);
        if self.inner.lock().unwrap().in_flight.contains(sid) {
            self.ports.abort(sid);
        }
        self.detach(sid);
        self.inner.lock().unwrap().overload.remove(sid);
        if state.is_none() {
            return Ok(());
        }
        self.with_conn(|c| c.execute("DELETE FROM session_goals WHERE session_id = ?1", [sid]))?;
        self.ports.emit(STATUS_CHANGED, json!({ "sessionId": sid, "goal": null }));
        Ok(())
    }

    pub fn pause_goal(&self, sid: &str, reason: Option<&str>) -> Result<(), String> {
        let Some(state) = self.get(sid).filter(|s| s.status == "active") else { return Ok(()) };
        if self.inner.lock().unwrap().in_flight.contains(sid) {
            self.ports.abort(sid);
        }
        self.clear_timer(sid);
        self.inner.lock().unwrap().turns.insert(sid.to_string(), TurnAcc::default());
        self.save(GoalState { status: "paused".into(), last_reason: Some(reason.unwrap_or("paused by user").into()), ..state })?;
        Ok(())
    }

    pub fn resume_goal(&self, sid: &str) -> Result<(), String> {
        let Some(state) = self.get(sid) else { return Ok(()) };
        if state.status == "active" {
            self.schedule(sid, FIRE_DEBOUNCE_MS);
            return Ok(());
        }
        if is_terminal(&state.status) {
            return Ok(());
        }
        self.inner.lock().unwrap().overload.remove(sid);
        self.save(GoalState { status: "active".into(), last_reason: None, usage_reset_at: None, no_progress_streak: 0, ..state })?;
        self.schedule(sid, FIRE_DEBOUNCE_MS);
        Ok(())
    }

    /// 打开会话时：active 目标没有在跑就再踢一轮；限流到点的自动续。
    pub fn resume_on_open(&self, sid: &str) -> Result<(), String> {
        let Some(state) = self.get(sid) else { return Ok(()) };
        let now = self.ports.now_ms();
        if state.status == "usageLimited" && state.usage_reset_at.is_some_and(|t| t <= now) {
            self.save(GoalState { status: "active".into(), last_reason: None, usage_reset_at: None, ..state })?;
            self.schedule(sid, FIRE_DEBOUNCE_MS);
            return Ok(());
        }
        let idle = {
            let g = self.inner.lock().unwrap();
            !g.timers.contains_key(sid) && !g.firing.contains(sid)
        };
        if state.status == "active" && idle && !self.ports.in_turn(sid) {
            self.schedule(sid, FIRE_DEBOUNCE_MS);
        }
        Ok(())
    }

    /// 启动：所有 active 目标接着跑，限流中的按重置时刻排队。返回恢复的个数。
    pub fn resume_active_goals(&self) -> usize {
        let rows: Vec<GoalState> = self
            .with_conn(|c| {
                let mut st = c.prepare("SELECT * FROM session_goals WHERE status IN ('active', 'usageLimited')")?;
                let it = st.query_map([], row_to_state)?;
                it.collect()
            })
            .unwrap_or_default();
        let now = self.ports.now_ms();
        for s in &rows {
            if s.status == "usageLimited" {
                self.schedule(&s.session_id, (s.usage_reset_at.unwrap_or(now) - now).max(1_000) as u64);
            } else {
                self.schedule(&s.session_id, 1_000);
            }
        }
        rows.len()
    }

    /// 用户输入的回合结束后，active 目标要接着发下一轮。
    pub fn note_session_idle(&self, sid: &str) {
        let idle = {
            let g = self.inner.lock().unwrap();
            !g.timers.contains_key(sid) && !g.firing.contains(sid)
        };
        if idle && self.get(sid).is_some_and(|s| s.status == "active") {
            self.schedule(sid, FIRE_DEBOUNCE_MS);
        }
    }

    pub fn dispose(&self) {
        let mut g = self.inner.lock().unwrap();
        g.disposed = true;
        g.timers.clear();
        g.watching.clear();
        g.turns.clear();
    }

    /// `ipc.invoke("maker:goal:*")` 与 /goal 命令的分发。
    pub fn invoke(&self, channel: &str, args: &[Value]) -> Result<Value, String> {
        let a0 = args.first().unwrap_or(&Value::Null);
        match channel {
            "maker:goal:set" => {
                let o = obj_arg(a0, "goal")?;
                let sid = str_arg(o.get("sessionId").unwrap_or(&Value::Null), "sessionId")?;
                let objective = str_arg(o.get("objective").unwrap_or(&Value::Null), "objective")?;
                let limits = match o.get("limits") {
                    None => None,
                    Some(v) => {
                        let l = obj_arg(v, "limits")?;
                        for k in ["maxTurns", "budgetTokens", "noProgressLimit"] {
                            let v = l.get(k).unwrap_or(&Value::Null);
                            if l.get(k).is_none() || !(v.is_null() || v.is_number()) {
                                return Err(fail("INVALID_PARAMS", format!("{k} must be a number or null")));
                            }
                        }
                        Some(l)
                    }
                };
                Self::internal(self.set_goal(sid, objective, limits))?;
                Ok(json!({ "ok": true }))
            }
            "maker:goal:clear" => {
                Self::internal(self.clear_goal(str_arg(a0, "sessionId")?))?;
                Ok(json!({ "ok": true }))
            }
            "maker:goal:pause" => {
                Self::internal(self.pause_goal(str_arg(a0, "sessionId")?, None))?;
                Ok(json!({ "ok": true }))
            }
            "maker:goal:resume" => {
                Self::internal(self.resume_goal(str_arg(a0, "sessionId")?))?;
                Ok(json!({ "ok": true }))
            }
            "maker:goal:update" => {
                let o = obj_arg(a0, "goal")?;
                let sid = str_arg(o.get("sessionId").unwrap_or(&Value::Null), "sessionId")?;
                let raw = obj_arg(o.get("patch").unwrap_or(&Value::Null), "patch")?;
                let mut patch = serde_json::Map::new();
                if let Some(v) = raw.get("objective") {
                    str_arg(v, "objective")?;
                    patch.insert("objective".into(), v.clone());
                }
                for k in ["maxTurns", "budgetTokens", "noProgressLimit"] {
                    if let Some(v) = raw.get(k) {
                        if !(v.is_null() || v.is_number()) {
                            return Err(fail("INVALID_PARAMS", format!("{k} must be a number or null")));
                        }
                        patch.insert(k.into(), v.clone());
                    }
                }
                match Self::internal(self.update_goal(sid, &patch))? {
                    Some(_) => Ok(json!({ "ok": true })),
                    None => Err(fail("GOAL_NOT_FOUND", "goal not found")),
                }
            }
            "maker:goal:get-status" => {
                let sid = str_arg(a0, "sessionId")?;
                let status = self.get_status(sid);
                // 打开会话时顺带把没在跑的 active 目标接着跑（desktop resumeOnOpen）。
                if status.as_ref().is_some_and(|s| s["status"] == "active" || s["status"] == "usageLimited") {
                    Self::internal(self.resume_on_open(sid))?;
                }
                Ok(self.get_status(sid).unwrap_or(Value::Null))
            }
            "maker:list-desktop-commands" => Ok(json!({
                "success": true,
                "commands": [{ "kind": "desktop", "name": "goal", "description": GOAL_DESCRIPTION }],
            })),
            "maker:execute-desktop-command" => self.execute_command(a0, args.get(1).unwrap_or(&Value::Null)),
            other => Err(fail("NOT_IMPLEMENTED", format!("{other} is not handled by the goal host"))),
        }
    }

    fn execute_command(&self, name: &Value, ctx: &Value) -> Result<Value, String> {
        let name = str_arg(name, "name")?;
        if name != "goal" {
            return Ok(json!({ "success": false, "error": "NOT_IMPLEMENTED" }));
        }
        let c = ctx.as_object().cloned().unwrap_or_default();
        let arg = c.get("args").and_then(Value::as_str).unwrap_or("").trim().to_string();
        let sid = c.get("sessionId").and_then(Value::as_str).filter(|s| !s.is_empty()).map(String::from);
        let trigger = |extra: Value| {
            let mut p = json!({ "command": "goal" });
            if let Some(s) = &sid {
                p["sessionId"] = json!(s);
            }
            if let Some(w) = c.get("workingDir").filter(|w| w.as_str().is_some_and(|s| !s.is_empty())) {
                p["workingDir"] = w.clone();
            }
            if !arg.is_empty() {
                p["args"] = json!(arg);
            }
            if let (Some(o), Some(e)) = (p.as_object_mut(), extra.as_object()) {
                o.extend(e.clone());
            }
            self.ports.emit("maker:desktop-command-triggered", p);
        };
        let Some(sid) = sid.clone() else {
            trigger(json!({ "error": "goal-no-session" }));
            return Ok(json!({ "success": true }));
        };
        let result = match parse_goal_command(&arg) {
            GoalCommand::Clear => self.clear_goal(&sid).map(|_| "cleared"),
            GoalCommand::OpenDialog => Ok("open-dialog"),
            GoalCommand::Set => self.set_goal(&sid, &arg, None).map(|_| "set"),
        };
        match result {
            Ok(action) => trigger(json!({ "goalAction": action })),
            Err(e) => {
                eprintln!("[goal] /goal failed: {}", e.lines().next().unwrap_or("").chars().take(200).collect::<String>());
                trigger(json!({ "error": "goal-failed" }));
            }
        }
        Ok(json!({ "success": true }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};

    struct Fake {
        db: PathBuf,
        in_turn: AtomicBool,
        enqueued: Mutex<Vec<(String, Value)>>,
        aborted: Mutex<Vec<String>>,
        emitted: Mutex<Vec<(String, Value)>>,
        enqueue_error: Mutex<Option<String>>,
        clock: AtomicI64,
    }
    impl GoalPorts for Fake {
        fn db_path(&self) -> Result<PathBuf, String> {
            Ok(self.db.clone())
        }
        fn in_turn(&self, _: &str) -> bool {
            self.in_turn.load(Ordering::SeqCst)
        }
        fn enqueue(&self, sid: &str, item: Value) -> Result<(), String> {
            if let Some(e) = self.enqueue_error.lock().unwrap().clone() {
                return Err(e);
            }
            self.enqueued.lock().unwrap().push((sid.into(), item));
            Ok(())
        }
        fn abort(&self, sid: &str) {
            self.aborted.lock().unwrap().push(sid.into());
        }
        fn emit(&self, channel: &str, payload: Value) {
            self.emitted.lock().unwrap().push((channel.into(), payload));
        }
        fn now_ms(&self) -> i64 {
            self.clock.load(Ordering::SeqCst)
        }
    }

    fn rig(tag: &str) -> (Arc<GoalController>, Arc<Fake>) {
        let db = std::env::temp_dir().join(format!("goal-host-{tag}-{}.db", std::process::id()));
        let _ = std::fs::remove_file(&db);
        let c = Connection::open(&db).unwrap();
        c.execute_batch(
            "CREATE TABLE sessions (id TEXT PRIMARY KEY, model TEXT NOT NULL, provider_id TEXT, working_dir TEXT, permission_mode TEXT, workspace_kind TEXT);
             CREATE TABLE session_goals (session_id TEXT PRIMARY KEY REFERENCES sessions(id) ON DELETE CASCADE, objective TEXT NOT NULL, status TEXT NOT NULL DEFAULT 'active',
               budget_tokens INTEGER, max_turns INTEGER, no_progress_limit INTEGER, turns_used INTEGER NOT NULL DEFAULT 0, tokens_used INTEGER NOT NULL DEFAULT 0,
               no_progress_streak INTEGER NOT NULL DEFAULT 0, usage_reset_at INTEGER, last_reason TEXT, agent_kind TEXT NOT NULL, started_at INTEGER NOT NULL, updated_at INTEGER NOT NULL);
             INSERT INTO sessions VALUES ('s1', 'm', 'p', '/w', 'ask', 'project');",
        )
        .unwrap();
        let f = Arc::new(Fake {
            db, in_turn: AtomicBool::new(false), enqueued: Default::default(), aborted: Default::default(), emitted: Default::default(),
            enqueue_error: Default::default(), clock: AtomicI64::new(1_000_000),
        });
        (GoalController::new(f.clone()), f)
    }

    fn wait_for(what: &str, f: impl Fn() -> bool) {
        let t0 = std::time::Instant::now();
        while !f() {
            assert!(t0.elapsed() < Duration::from_secs(5), "timeout: {what}");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// 一个完整的目标回合：先发出去，再喂事件。
    fn goal_turn(c: &GoalController, sid: &str, text: &str, tool: bool) {
        let o = json!({ "kind": "goal" });
        if tool {
            c.on_event(sid, &json!({ "type": "tool_use", "turnOrigin": o, "data": {} }));
        }
        c.on_event(sid, &json!({ "type": "text", "turnOrigin": o, "data": { "text": text } }));
        c.on_event(sid, &json!({ "type": "done", "turnOrigin": o, "data": { "usage": { "inputTokens": 10, "outputTokens": 5 } } }));
    }

    fn verdict(status: &str, reason: &str) -> String {
        format!("work\n```json\n{{\"goal_status\":\"{status}\",\"reason\":\"{reason}\"}}\n```")
    }

    #[test]
    fn a_goal_runs_turns_until_the_model_says_complete_and_then_stops_for_good() {
        let (c, f) = rig("complete");
        c.set_goal("s1", "ship it", None).unwrap();
        wait_for("first turn sent", || f.enqueued.lock().unwrap().len() == 1);
        {
            let q = f.enqueued.lock().unwrap();
            let item = &q[0].1;
            assert_eq!(item["origin"], json!({ "kind": "goal", "goalSessionId": "s1" }));
            assert_eq!(item["createOpts"]["model"], "m");
            assert_eq!(item["createOpts"]["workspaceKind"], "project", "按库里的会话行建会话");
            assert!(item["text"].as_str().unwrap().starts_with("[Goal] Work autonomously"), "首轮带自检与裁决约定");
        }
        goal_turn(&c, "s1", &verdict("continue", "more"), true);
        wait_for("second turn sent", || f.enqueued.lock().unwrap().len() == 2);
        assert!(f.enqueued.lock().unwrap()[1].1["text"].as_str().unwrap().contains("Last status note: more"));
        goal_turn(&c, "s1", &verdict("complete", "all green"), true);
        let st = c.get_status("s1").unwrap();
        assert_eq!((st["status"].clone(), st["turnsUsed"].clone(), st["tokensUsed"].clone()), (json!("complete"), json!(2), json!(30)), "{st}");
        assert_eq!(st["lastReason"], "all green");
        std::thread::sleep(Duration::from_millis(400));
        assert_eq!(f.enqueued.lock().unwrap().len(), 2, "complete 之后不再发");
    }

    #[test]
    fn guard_rails_pause_after_idle_turns_and_stop_at_the_turn_budget() {
        let (c, f) = rig("guards");
        c.set_goal("s1", "x", None).unwrap();
        wait_for("sent", || f.enqueued.lock().unwrap().len() == 1);
        for i in 0..3 {
            goal_turn(&c, "s1", &verdict("continue", "still"), false);
            if i < 2 {
                wait_for("next", || f.enqueued.lock().unwrap().len() == i + 2);
            }
        }
        let st = c.get_status("s1").unwrap();
        assert_eq!(st["status"], "paused", "默认连续 3 轮没用工具就暂停: {st}");
        assert!(st["lastReason"].as_str().unwrap().contains("3 turns with no tool use"));

        let limits = json!({ "maxTurns": 1, "budgetTokens": null, "noProgressLimit": null });
        c.set_goal("s1", "y", limits.as_object()).unwrap();
        let n = f.enqueued.lock().unwrap().len();
        wait_for("sent again", || f.enqueued.lock().unwrap().len() == n + 1);
        goal_turn(&c, "s1", &verdict("continue", "more"), true);
        assert_eq!(c.get_status("s1").unwrap()["status"], "budgetLimited");
    }

    #[test]
    fn busy_sessions_are_not_poked_and_a_settled_user_turn_resumes_the_goal() {
        let (c, f) = rig("busy");
        f.in_turn.store(true, Ordering::SeqCst);
        c.set_goal("s1", "x", None).unwrap();
        std::thread::sleep(Duration::from_millis(500));
        assert!(f.enqueued.lock().unwrap().is_empty(), "用户回合在跑时不发");
        f.in_turn.store(false, Ordering::SeqCst);
        c.note_session_idle("s1");
        wait_for("sent after idle", || f.enqueued.lock().unwrap().len() == 1);
        // 没有 active 目标的会话：收尾通知什么都不做
        c.clear_goal("s1").unwrap();
        c.note_session_idle("s1");
        std::thread::sleep(Duration::from_millis(400));
        assert_eq!(f.enqueued.lock().unwrap().len(), 1);
    }

    #[test]
    fn rate_limits_wait_for_the_reset_time_and_overload_gives_up_after_three_in_a_row() {
        let (c, f) = rig("limits");
        c.set_goal("s1", "x", Some(json!({ "maxTurns": null, "budgetTokens": null, "noProgressLimit": null }).as_object().unwrap())).unwrap();
        wait_for("sent", || f.enqueued.lock().unwrap().len() == 1);
        let err = |msg: &str| c.on_event("s1", &json!({ "type": "error", "turnOrigin": { "kind": "goal" }, "data": { "message": msg } }));
        err("HTTP 429 rate limit");
        let st = c.get_status("s1").unwrap();
        assert_eq!(st["status"], "usageLimited");
        assert_eq!(st["usageResetAt"], 1_000_000 + USAGE_LIMIT_DEFAULT_WAIT_MS, "按默认 5 分钟排队: {st}");
        // 打开会话时：没到点不动，到点自动续
        c.resume_on_open("s1").unwrap();
        assert_eq!(c.get_status("s1").unwrap()["status"], "usageLimited");
        f.clock.store(1_000_000 + USAGE_LIMIT_DEFAULT_WAIT_MS + 1, Ordering::SeqCst);
        c.resume_on_open("s1").unwrap();
        assert_eq!(c.get_status("s1").unwrap()["status"], "active");
        wait_for("resent", || f.enqueued.lock().unwrap().len() == 2);
        // 过载连续 3 次 → 不再按过载排队，当作普通失败
        for _ in 0..2 {
            c.on_event("s1", &json!({ "type": "text", "turnOrigin": { "kind": "goal" }, "data": { "text": "x" } }));
            err("529 overloaded");
            let st = c.get_status("s1").unwrap();
            assert_eq!((st["status"].clone(), st["usageResetAt"].clone()), (json!("usageLimited"), json!(f.clock.load(Ordering::SeqCst) + OVERLOAD_RESUME_DELAY_MS)), "{st}");
            // 到点自动续（不是用户手动恢复：手动恢复会清掉连续过载计数）。
            f.clock.fetch_add(OVERLOAD_RESUME_DELAY_MS + 1, Ordering::SeqCst);
            c.resume_on_open("s1").unwrap();
        }
        c.on_event("s1", &json!({ "type": "text", "turnOrigin": { "kind": "goal" }, "data": { "text": "x" } }));
        err("529 overloaded");
        let st = c.get_status("s1").unwrap();
        assert_eq!(st["status"], "blocked", "第 3 次过载放弃: {st}");
        // 用户手动恢复会清掉计数，重新开始数。
        c.resume_goal("s1").unwrap();
        assert_eq!(c.get_status("s1").unwrap()["status"], "active");
    }

    #[test]
    fn dispatch_failures_block_the_goal_except_when_the_session_is_just_busy() {
        let (c, f) = rig("dispatch");
        *f.enqueue_error.lock().unwrap() = Some("[SESSION_BUSY] session is running a turn".into());
        c.set_goal("s1", "x", None).unwrap();
        std::thread::sleep(Duration::from_millis(500));
        assert_eq!(c.get_status("s1").unwrap()["status"], "active", "忙不算失败");
        *f.enqueue_error.lock().unwrap() = Some("[MODEL_NOT_AVAILABLE] gone".into());
        c.set_goal("s1", "x", None).unwrap();
        wait_for("blocked", || c.get_status("s1").unwrap()["status"] == "blocked");
        assert!(c.get_status("s1").unwrap()["lastReason"].as_str().unwrap().contains("turn dispatch failed"));
        // 会话行不存在（比如被删）
        let (c2, _f2) = rig("dispatch2");
        c2.with_conn(|x| x.execute_batch("PRAGMA foreign_keys = OFF; INSERT INTO session_goals VALUES ('ghost','o','active',NULL,NULL,NULL,0,0,0,NULL,NULL,'pi',1,1);")).unwrap();
        assert_eq!(c2.resume_active_goals(), 1);
        wait_for("ghost blocked", || c2.get_status("ghost").unwrap()["status"] == "blocked");
        assert_eq!(c2.get_status("ghost").unwrap()["lastReason"], "unable to restore the agent session for Goal");
    }

    #[test]
    fn pause_resume_update_clear_and_validation() {
        let (c, f) = rig("ops");
        c.set_goal("s1", "x", None).unwrap();
        wait_for("sent", || f.enqueued.lock().unwrap().len() == 1);
        c.pause_goal("s1", None).unwrap();
        assert_eq!(c.get_status("s1").unwrap()["lastReason"], "paused by user");
        assert_eq!(f.aborted.lock().unwrap().as_slice(), ["s1"], "有目标回合在跑时暂停会中断它");
        std::thread::sleep(Duration::from_millis(400));
        assert_eq!(f.enqueued.lock().unwrap().len(), 1, "暂停期间不续跑");
        c.resume_goal("s1").unwrap();
        wait_for("resumed", || f.enqueued.lock().unwrap().len() == 2);
        let upd = c.update_goal("s1", json!({ "objective": "  changed  ", "maxTurns": 9 }).as_object().unwrap()).unwrap().unwrap();
        assert_eq!((upd.objective.as_str(), upd.max_turns), ("changed", Some(9)));
        assert!(c.update_goal("nope", &serde_json::Map::new()).unwrap().is_none());
        c.clear_goal("s1").unwrap();
        assert!(c.get_status("s1").is_none());
        let em = f.emitted.lock().unwrap();
        assert!(em.iter().any(|(ch, p)| ch == STATUS_CHANGED && p["goal"].is_null()), "清除推 goal: null");
        drop(em);
        for (args, code) in [
            (json!([{ "sessionId": "s1", "objective": "  " }]), "INVALID_PARAMS"),
            (json!([{ "sessionId": "nope", "objective": "x" }]), "NOT_FOUND"),
            (json!([{ "sessionId": "s1", "objective": "x", "limits": { "maxTurns": 0, "budgetTokens": null, "noProgressLimit": null } }]), "INVALID_PARAMS"),
            (json!([{ "sessionId": "s1", "objective": "x", "limits": { "maxTurns": null } }]), "INVALID_PARAMS"),
            (json!([{ "sessionId": "s1", "objective": "x".repeat(4001) }]), "INVALID_PARAMS"),
        ] {
            let e = c.invoke("maker:goal:set", args.as_array().unwrap()).unwrap_err();
            assert!(e.contains(code), "{e}");
        }
        let e = c.invoke("maker:goal:update", &[json!({ "sessionId": "s1", "patch": { "maxTurns": 3 } })]).unwrap_err();
        assert!(e.contains("GOAL_NOT_FOUND"), "{e}");
    }

    #[test]
    fn slash_goal_maps_to_set_clear_and_dialog_and_refined_objectives_replace_the_goal() {
        assert_eq!(parse_goal_command(""), GoalCommand::OpenDialog);
        assert_eq!(parse_goal_command("   "), GoalCommand::OpenDialog);
        for a in ["clear", "STOP", " Off ", "cancel", "reset", "none"] {
            assert_eq!(parse_goal_command(a), GoalCommand::Clear, "{a}");
        }
        assert_eq!(parse_goal_command("clear the cache"), GoalCommand::Set);
        let (c, f) = rig("slash");
        let run = |args: &str| c.invoke("maker:execute-desktop-command", &[json!("goal"), json!({ "sessionId": "s1", "args": args })]).unwrap();
        assert_eq!(run("")["success"], true);
        assert_eq!(c.invoke("maker:execute-desktop-command", &[json!("other"), json!({})]).unwrap(), json!({ "success": false, "error": "NOT_IMPLEMENTED" }));
        run("make it fast");
        assert_eq!(c.get_status("s1").unwrap()["objective"], "make it fast");
        wait_for("sent", || f.enqueued.lock().unwrap().len() == 1);
        let text = format!("```json\n{{\"goal_status\":\"continue\",\"reason\":\"r\",\"refined_objective\":\"make the build 2x faster\"}}\n```");
        goal_turn(&c, "s1", &text, true);
        assert_eq!(c.get_status("s1").unwrap()["objective"], "make the build 2x faster", "澄清后的目标替换存下的目标");
        let triggers: Vec<Value> = f.emitted.lock().unwrap().iter().filter(|(ch, _)| ch == "maker:desktop-command-triggered").map(|(_, p)| p.clone()).collect();
        assert_eq!(triggers.iter().map(|t| t["goalAction"].clone()).collect::<Vec<_>>(), vec![json!("open-dialog"), json!("set")]);
        run("clear");
        assert!(c.get_status("s1").is_none());
        let none = c.invoke("maker:execute-desktop-command", &[json!("goal"), json!({ "args": "x" })]).unwrap();
        assert_eq!(none["success"], true);
        assert!(f.emitted.lock().unwrap().iter().any(|(_, p)| p["error"] == "goal-no-session"));
    }

    #[test]
    fn a_disposed_controller_never_fires_again() {
        let (c, f) = rig("dispose");
        c.set_goal("s1", "x", None).unwrap();
        c.dispose();
        std::thread::sleep(Duration::from_millis(500));
        assert!(f.enqueued.lock().unwrap().is_empty(), "dispose 后定时器不再触发");
        assert!(c.set_goal("s1", "y", None).unwrap_err().contains("PRECONDITION_FAILED"));
    }
}
