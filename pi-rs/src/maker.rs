// maker.rs — pi RPC 帧 → AgentEvent（maker 事件）翻译层。
// 是 cindy maker-core agents/pi/translator.ts 的 Rust 移植（差距清单 #1），
// 验收 = scripts/maker-freeze.mjs 的双产出比对：同一帧流必须产出同一事件序列。
// 已移植：agent_start / turn_start / message_start / message_update（text/thinking delta、
//         token 速度计）/ message_end / tool_execution_start / tool_execution_end /
//         turn_end / agent_end / agent_settled（done + usage 结算）。
//         auto_retry_start / auto_retry_end、message_end 瞬时失败 → pendingAssistantError（错误分类见 maker_errors.rs）。
//         compaction_start / compaction_end（含空闲手动压缩的 background scope 与收口）。
//         redacted thinking、网页引用 / stop-token 剥离（maker_text.rs）、宿主主动 abort 登记（mark_host_abort_requested）、
//         silentStop。
// 不移植：subagent（agent_task_update）与 cindy cache recovery。pi-rs 没有 `subagent` 工具，也不产生
//         cindyCacheRecovery* 字段，这两条翻译路径在 pi-rs 上永远不会被触发。
use crate::maker_errors as errs;
use crate::maker_text::{hold_standalone_stop_token_delta, strip_internal_web_citations, StopHold};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};

fn now_ms() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0)
}

/// estimateWordTokens：`\w+|[^\s\w]` 的逐段计数（JS \w = ASCII 词字符）。
fn estimate_word_tokens(text: &str) -> i64 {
    let mut count = 0i64;
    let mut in_word = false;
    for c in text.chars() {
        let word = c.is_ascii_alphanumeric() || c == '_';
        let ws = c.is_whitespace();
        if word {
            if !in_word {
                count += 1;
                in_word = true;
            }
        } else {
            in_word = false;
            if !ws {
                count += 1;
            }
        }
    }
    count
}

// ── token 速度计（calibrated-token-speed.ts 的移植；不含 thinking 摘要/scale 学习）──────
// 数值（rate/average/elapsedMs）是计时产物，比对层抹掉；必须对齐的是：
//   snapshot 何时存在（backlog 内的输出不计入 live → 首请求短流时为 None）、
//   字段集合（rate/average/tokens/elapsedMs/streaming/peak?）、publishDue 的 250ms 节流。
const PUBLISH_INTERVAL_MS: i64 = 250;
const WINDOW_MS: i64 = 5_000;
const MIN_SPAN_MS: i64 = 2_000;
const MIN_WINDOW_TOKENS: i64 = 5;
const MAX_VALID_TPS: f64 = 10_000.0;
const MIN_VALID_DURATION_MS: i64 = 1_000;
const MIN_READING_REQUEST_MS: i64 = 3_000;
const MIN_BURST_SPAN_MS: i64 = 100;
const BACKLOG_MS: i64 = 300;
const NEW_REQUEST_PAUSE_MS: i64 = 500;
const SUMMARY_WINDOW_MS: i64 = 30_000;

pub struct TokenSpeedMeter {
    started: bool,
    stopped: bool,
    paused: bool,
    invalid: bool,
    last_tick: i64,
    paused_at: i64,
    clock_ms: i64,
    clock_at_settle: i64,
    flow_ms: i64,
    settled_ms: i64,
    settled_tokens: f64,
    live_tokens: f64,
    live_ms: i64,
    measuring_yes: bool,
    block_kind_visible: bool,
    backlog_until_clock: i64,
    backlog_tokens: i64,
    boundary_until: i64,
    boundary_index: isize,
    last_event_at: Option<i64>,
    events: Vec<(i64, i64)>,
    window_start_index: usize,
    tps: f64,
    peak: f64,
    last_publish_at: i64,
    last_request_tps: f64,
    raw_visible: i64,
    streamed: bool,
    provisional_tokens: i64,
    provisional_own_ms: i64,
    provisional_credit_ms: f64,
}

impl Default for TokenSpeedMeter {
    fn default() -> Self {
        // lastPublishAt = -Infinity：首帧 publish 必然到期（i64::MIN 会让差值溢出）。
        Self {
            started: false, stopped: false, paused: false, invalid: false,
            last_tick: 0, paused_at: 0, clock_ms: 0, clock_at_settle: 0, flow_ms: 0,
            settled_ms: 0, settled_tokens: 0.0, live_tokens: 0.0, live_ms: 0,
            measuring_yes: true, block_kind_visible: true,
            backlog_until_clock: 0, backlog_tokens: 0,
            boundary_until: i64::MIN, boundary_index: -1,
            last_event_at: None, events: Vec::new(), window_start_index: 0,
            tps: 0.0, peak: 0.0, last_publish_at: i64::MIN / 2, last_request_tps: 0.0,
            raw_visible: 0, streamed: false,
            provisional_tokens: 0, provisional_own_ms: 0, provisional_credit_ms: 0.0,
        }
    }
}

impl TokenSpeedMeter {
    pub fn new() -> Self {
        Self::default()
    }
    /// 新 turn：忘记测量，保留 token 比例的学习（TS reset 同款；lastPublishAt 重置为
    /// "负无穷"替身 —— 新回合的首个 delta 必然允许发布一次 status）。
    pub fn reset(&mut self) {
        let last_request_tps = self.last_request_tps;
        *self = Self::default();
        self.last_request_tps = last_request_tps;
    }
    /// host 挂起 / 时钟回退时调用（attach 到宿主事件时接入）。
    #[allow(dead_code)]
    fn invalidate(&mut self) {
        self.invalid = true;
    }
    fn tick(&mut self, now: i64) {
        if self.started && !self.stopped && !self.paused {
            let delta = (now - self.last_tick).max(0);
            self.clock_ms += delta;
            if self.measuring_yes {
                self.live_ms += delta;
                self.flow_ms += delta;
            }
        }
        self.last_tick = now;
    }
    fn open_request(&mut self) {
        self.tps = 0.0;
        self.backlog_until_clock = self.clock_ms + BACKLOG_MS;
        self.bound(self.flow_ms);
        self.boundary_until = self.flow_ms + BACKLOG_MS;
        self.last_event_at = Some(self.flow_ms);
    }
    fn enter_block(&mut self, kind_visible: bool) {
        if kind_visible != self.block_kind_visible {
            self.bound(self.flow_ms);
        }
        self.block_kind_visible = kind_visible;
        // thinking 分支（awaitingSummary / undecided measuring）未移植：fixture 全 visible。
        self.measuring_yes = kind_visible;
    }
    /// content block 开始：模型在生成，无论是否已有输出流。
    pub fn start(&mut self, now: i64, kind_visible: bool) {
        if self.stopped {
            return;
        }
        self.streamed = true;
        if !self.started {
            self.started = true;
            self.last_tick = now;
            self.open_request();
        } else if self.paused {
            self.paused = false;
            self.last_tick = now;
            if now - self.paused_at >= NEW_REQUEST_PAUSE_MS {
                self.open_request();
            }
        } else {
            self.tick(now);
        }
        self.enter_block(kind_visible);
    }
    pub fn pause(&mut self, now: i64) {
        if !self.started || self.stopped || self.paused {
            return;
        }
        self.tick(now);
        self.paused = true;
        self.paused_at = now;
    }
    fn discard_request(&mut self, now: i64) {
        if !self.started || self.stopped {
            return;
        }
        self.pause(now);
        self.clock_at_settle = self.clock_ms;
        self.clear_live();
        self.bound(self.flow_ms);
    }
    fn clear_live(&mut self) {
        self.live_tokens = 0.0;
        self.live_ms = 0;
        self.backlog_tokens = 0;
        self.streamed = false;
        self.raw_visible = 0;
    }
    /// 一条流式 delta。返回 true 时需要刷新 status。
    pub fn record_delta(&mut self, text: &str, now: i64) -> bool {
        let raw = estimate_word_tokens(text);
        if std::env::var("MAKER_DEBUG").is_ok() { eprintln!("[meter] record_delta raw={raw} now={now} started={} paused={} stopped={} last_pub={}", self.started, self.paused, self.stopped, self.last_publish_at); }
        if raw > 0 {
            self.record(raw, now, true)
        } else {
            false
        }
    }
    fn record(&mut self, raw: i64, now: i64, visible: bool) -> bool {
        if self.stopped {
            return false;
        }
        if !self.started || self.paused {
            self.start(now, visible);
        } else {
            self.tick(now);
        }
        if visible != self.block_kind_visible {
            self.enter_block(visible);
        }
        if visible {
            self.raw_visible += raw;
        }
        let tokens = raw as f64; // visibleScale.assume(1)，未学习时恒 1
        if !self.measuring_yes {
            return self.publish_due(now);
        }
        let at = self.flow_ms;
        let previous_at = self.last_event_at;
        if self.last_event_at.map(|p| at - p > WINDOW_MS).unwrap_or(false) {
            self.boundary_until = at + BACKLOG_MS;
        }
        self.last_event_at = Some(at);
        if self.clock_ms < self.backlog_until_clock {
            self.backlog_tokens += tokens as i64;
            self.bound(at);
            return self.publish_due(now);
        }
        self.live_tokens += tokens;
        let since_previous = previous_at.map(|p| at - p).unwrap_or(0);
        let arrival_ms = if since_previous > 0 {
            since_previous
        } else if tokens < MIN_WINDOW_TOKENS as f64 {
            MIN_BURST_SPAN_MS
        } else {
            1
        };
        if tokens * 1000.0 / arrival_ms as f64 > MAX_VALID_TPS {
            self.bound(at);
            return self.publish_due(now);
        }
        if at <= self.boundary_until {
            self.bound(at);
        } else {
            self.events.push((at, raw));
        }
        if let Some(reading) = self.window_tps(at) {
            self.tps = reading;
            self.peak = self.peak.max(reading);
        }
        self.publish_due(now)
    }
    /// 至多每 250ms 一次 status 刷新。
    fn publish_due(&mut self, now: i64) -> bool {
        if now - self.last_publish_at < PUBLISH_INTERVAL_MS {
            return false;
        }
        self.last_publish_at = now;
        true
    }
    fn bound(&mut self, at: i64) {
        self.events.push((at, 0));
        self.boundary_index = self.events.len() as isize - 1;
    }
    fn window_tps(&mut self, at: i64) -> Option<f64> {
        while self.window_start_index + 1 < self.events.len()
            && self.events[self.window_start_index + 1].0 <= at - SUMMARY_WINDOW_MS
        {
            self.window_start_index += 1;
        }
        if self.window_start_index >= 1_024 {
            self.events.drain(..self.window_start_index);
            self.boundary_index = (self.boundary_index - self.window_start_index as isize).max(-1);
            self.window_start_index = 0;
        }
        let lowest = (self.boundary_index.max(self.window_start_index as isize) + 1) as usize;
        let mut first = self.events.len();
        let mut tokens = 0i64;
        while first > lowest && self.events[first - 1].0 > at - WINDOW_MS {
            first -= 1;
            tokens += self.events[first].1;
        }
        if first == self.events.len() {
            return None;
        }
        let span = at - self.events[first - 1].0;
        if span < MIN_SPAN_MS || tokens < MIN_WINDOW_TOKENS {
            return None;
        }
        let rate = (tokens as f64 * 1000.0) / span as f64;
        (rate <= MAX_VALID_TPS).then_some(rate)
    }
    /// 请求结束且上报了真实 usage：替换流估计、学习差异、停钟到下一 block。
    pub fn settle(&mut self, turn_output_tokens: i64, now: i64, _thinking_tokens: Option<i64>, request_wall_ms: Option<i64>) -> Option<()> {
        if self.stopped {
            return None;
        }
        if self.started {
            self.pause(now);
        } else {
            // 本 turn 首个响应从未流式（整体到达的工具调用）。
            self.started = true;
            self.paused = true;
            self.paused_at = now;
            self.last_tick = now;
        }
        let request_tokens = turn_output_tokens - self.settled_tokens as i64;
        if request_tokens < 0 {
            return None;
        }
        if request_tokens == 0 {
            self.discard_request(now);
            return None;
        }
        let request_ms = self.clock_ms - self.clock_at_settle;
        // head（首字节前/积压里释放的输出）不计入节奏；fixture 全走 backlog → paced=0 → 无读数。
        let head = if self.streamed { request_tokens.min(self.backlog_tokens) } else { request_tokens };
        let paced = request_tokens - head;
        let mut request_tps: Option<f64> = None;
        if request_ms >= MIN_VALID_DURATION_MS && paced >= MIN_WINDOW_TOKENS {
            let rate = (paced as f64 * 1000.0) / request_ms as f64;
            if rate <= MAX_VALID_TPS {
                request_tps = Some(rate);
            }
        }
        let known_tps = request_tps.or(if self.last_request_tps > 0.0 { Some(self.last_request_tps) } else if self.tps > 0.0 { Some(self.tps) } else { None }).unwrap_or(0.0);
        let wait_ms = request_wall_ms.map(|w| (w - request_ms).max(0));
        let mut credit_ms = match request_tps {
            Some(rt) => head as f64 * 1000.0 / rt,
            None if known_tps > 0.0 => ((request_tokens as f64 * 1000.0) / known_tps - request_ms as f64).max(0.0),
            None => wait_ms.unwrap_or(0) as f64,
        };
        if let Some(w) = wait_ms {
            credit_ms = credit_ms.min(w as f64);
        }
        if known_tps <= 0.0 && credit_ms > 0.0 {
            self.provisional_tokens += request_tokens;
            self.provisional_own_ms += request_ms;
            self.provisional_credit_ms += credit_ms;
        } else if request_tps.is_some() && self.provisional_tokens > 0 {
            let credit = ((self.provisional_tokens as f64 * 1000.0) / request_tps.unwrap() - self.provisional_own_ms as f64).max(0.0);
            self.settled_ms -= ((self.provisional_credit_ms - credit).max(0.0)) as i64;
            self.provisional_tokens = 0;
            self.provisional_own_ms = 0;
            self.provisional_credit_ms = 0.0;
        }
        self.settled_tokens = turn_output_tokens as f64;
        self.settled_ms += request_ms + (credit_ms as i64);
        self.clock_at_settle = self.clock_ms;
        self.clear_live();
        if let Some(rt) = request_tps {
            self.last_request_tps = rt;
            if request_ms >= MIN_READING_REQUEST_MS {
                self.tps = rt;
                self.peak = self.peak.max(rt);
            }
        }
        Some(())
    }
    pub fn stop(&mut self, real_output_tokens: i64, now: i64) {
        if !self.started {
            return;
        }
        if !self.stopped {
            self.pause(now);
            self.stopped = true;
        }
        // 结束请求之后没有新上报：流过的部分保留估计值。
        if !(real_output_tokens as f64 > self.settled_tokens) {
            return;
        }
        self.settled_tokens = real_output_tokens as f64;
        self.settled_ms += self.clock_ms - self.clock_at_settle;
        self.clock_at_settle = self.clock_ms;
        self.clear_live();
    }
    pub fn snapshot(&self, now: i64) -> Option<Value> {
        if !self.started || self.invalid {
            return None;
        }
        let streaming = !self.stopped && !self.paused;
        let since_tick = if streaming { (now - self.last_tick).max(0) } else { 0 };
        let tokens = self.settled_tokens + self.live_tokens;
        if tokens <= 0.0 {
            return None;
        }
        let elapsed_ms = self.settled_ms + self.live_ms + if self.measuring_yes { since_tick } else { 0 };
        let average = if elapsed_ms >= MIN_VALID_DURATION_MS { tokens * 1000.0 / elapsed_ms as f64 } else { 0.0 };
        let mut out = json!({
            "rate": if streaming { self.tps } else { average },
            "average": average,
            "tokens": tokens.round() as i64,
            "elapsedMs": elapsed_ms,
            "streaming": streaming,
        });
        if self.peak > 0.0 {
            out["peak"] = json!(self.peak);
        }
        Some(out)
    }
}

// ── 翻译上下文 ──────────────────────────────────────────────────────
pub struct MakerCtx {
    pub logger: Box<dyn Fn(&str) + Send>,
    token_speed: TokenSpeedMeter,
    last_running_status: String,
    // usage 结算
    request_epoch: String,
    request_models: HashMap<String, (i64, i64, i64, i64)>,
    turn_tokens: i64,
    turn_input: i64,
    turn_output: i64,
    turn_main_output: i64,
    turn_reasoning: Option<i64>,
    turn_cache_read: i64,
    turn_cache_write: i64,
    cache_recovery_has_output: bool,
    unreported_estimated_output: i64,
    last_calibrated_tokens: i64,
    context_tokens: i64,
    cost_usd: f64,
    turn_usage_segments: Vec<Value>,
    turn_usage_segment_seq: i64,
    usage_receipts: Vec<Value>,
    usage_receipt_cursor: usize,
    usage_settlement_id: String,
    pending_price_variants: Vec<&'static str>,
    generation_duration_ms: i64,
    generation_timing_reliable: bool,
    generation_open_at: i64,
    generation_heartbeat_at: i64,
    generation_heartbeat_reliable: bool,
    // 流状态
    is_streaming: bool,
    turn_settled: bool,
    turn_generation: i64,
    final_assistant_text: String,
    final_assistant_stop_reason: Option<String>,
    pending_assistant_error: Option<Value>,
    /// auto_retry_end(失败) 已经发过终止性 error：agent_settled 不能再按「正常完成」收尾，也不重复发 error。
    terminal_assistant_error_emitted: bool,
    /// compaction_start 锁存的 turnScope（'background' | 'turn'）。end 必须复用：压缩期间可能已开始新回合。
    compact_turn_scope: Option<&'static str>,
    /// 宿主主动 abort 的登记：记录它针对的 turn_generation 与请求次数。Pi 的 aborted 终态可能先于 abort RPC 回执到达，
    /// 所以必须在发 RPC 之前登记；token 让迟到的失败回滚只能撤销自己的那一次。
    host_abort_generation: Option<i64>,
    host_abort_tokens: Vec<u64>,
    host_abort_seq: u64,
    /// prompt RPC 发出前登记其尚未到达的 agent_start。
    pending_host_turn_start: Option<u64>,
    /// compaction_start 时的 turn_generation：延迟到达的 end 不能覆盖新回合的用量。
    compact_turn_generation: Option<i64>,
    thinking_id_prefix: String,
    thinking_seq: i64,
    thinking_blocks: HashMap<i64, (String, bool, i64)>,
    /// holdStandaloneStopTokenDelta 的缓冲（引用剥离语义接入时启用）。
    #[allow(dead_code)]
    stream_stop_token_by_index: HashMap<i64, StopHold>,
    tool_names_by_call_id: HashMap<String, String>,
    turn_wall_clock_started_at: i64,
}

impl MakerCtx {
    pub fn new(logger: Box<dyn Fn(&str) + Send>) -> Self {
        Self {
            logger,
            token_speed: TokenSpeedMeter::new(),
            last_running_status: "Working…".into(),
            request_epoch: uuid_v4(),
            request_models: HashMap::new(),
            turn_tokens: 0,
            turn_input: 0,
            turn_output: 0,
            turn_main_output: 0,
            turn_reasoning: None,
            turn_cache_read: 0,
            turn_cache_write: 0,
            cache_recovery_has_output: false,
            unreported_estimated_output: 0,
            last_calibrated_tokens: 0,
            context_tokens: 0,
            cost_usd: 0.0,
            turn_usage_segments: Vec::new(),
            turn_usage_segment_seq: 0,
            usage_receipts: Vec::new(),
            usage_receipt_cursor: 0,
            usage_settlement_id: uuid_v4(),
            pending_price_variants: Vec::new(),
            generation_duration_ms: 0,
            generation_timing_reliable: true,
            generation_open_at: 0,
            generation_heartbeat_at: 0,
            generation_heartbeat_reliable: true,
            is_streaming: false,
            turn_settled: false,
            turn_generation: 0,
            final_assistant_text: String::new(),
            final_assistant_stop_reason: None,
            pending_assistant_error: None,
            terminal_assistant_error_emitted: false,
            compact_turn_scope: None,
            host_abort_generation: None,
            host_abort_tokens: Vec::new(),
            host_abort_seq: 0,
            pending_host_turn_start: None,
            compact_turn_generation: None,
            thinking_id_prefix: format!("pi-think-{}", uuid_v4()),
            thinking_seq: 0,
            thinking_blocks: HashMap::new(),
            stream_stop_token_by_index: HashMap::new(),
            tool_names_by_call_id: HashMap::new(),
            turn_wall_clock_started_at: 0,
        }
    }
}

fn uuid_v4() -> String {
    uuid::Uuid::new_v4().to_string()
}

// ── 状态快照（pushStatus → usageSnapshotOf + attachLiveGeneration）──────
fn usage_snapshot_of(ctx: &MakerCtx, now: i64) -> Value {
    let mut models = Vec::new();
    for (model, (input, output, cache_read, cache_create)) in &ctx.request_models {
        models.push(json!({
            "model": model, "inputTokens": input, "outputTokens": output,
            "cacheReadTokens": cache_read, "cacheCreateTokens": cache_create,
        }));
    }
    models.sort_by(|a, b| a["model"].as_str().unwrap_or("").cmp(b["model"].as_str().unwrap_or("")));
    let mut out = json!({
        "requestUsage": { "epoch": ctx.request_epoch, "models": models },
        "tokenUsage": ctx.turn_tokens,
        "contextTokens": ctx.context_tokens,
        "contextWindow": 0,
        "costUsd": ctx.cost_usd,
    });
    let mut timing = json!({ "outputTokens": ctx.turn_output, "reliable": ctx.generation_timing_reliable && ctx.generation_heartbeat_reliable });
    if let Some(ts) = ctx.token_speed.snapshot(now) {
        timing["tokenSpeed"] = ts;
    }
    let output_tokens = ctx.turn_output.max(0);
    out["outputTokens"] = json!(output_tokens);
    if let Some(ts) = timing.get("tokenSpeed") {
        out["tokenSpeed"] = ts.clone();
    }
    let reliable = timing["reliable"] == json!(true);
    out["generationReliable"] = json!(reliable);
    if !reliable {
        out["generationActive"] = json!(false);
    } else {
        let generation_active = ctx.generation_open_at > 0;
        out["generationActive"] = json!(generation_active);
        let duration = ctx.generation_duration_ms;
        if duration > 0 {
            out["generationDurationMs"] = json!(duration);
        }
    }
    out
}

fn push_status(events: &mut Vec<Value>, ctx: &mut MakerCtx, text: &str, is_running: bool, now: i64) {
    push_status_with(events, ctx, text, is_running, now, None, false);
}

/// 带 turnScope（`background`：空闲压缩不是产品回合，宿主 / UI 不能因它锁忙）与 contextSnapshotAuthoritative 的变体。
fn push_status_with(events: &mut Vec<Value>, ctx: &mut MakerCtx, text: &str, is_running: bool, now: i64, turn_scope: Option<&str>, snapshot_authoritative: bool) {
    if is_running {
        ctx.last_running_status = text.to_string();
    }
    let mut data = usage_snapshot_of(ctx, now);
    data["status"] = json!(text);
    data["isRunning"] = json!(is_running);
    if snapshot_authoritative {
        data["contextSnapshotAuthoritative"] = json!(true);
    }
    let mut ev = json!({ "type": "status", "data": data, "source": "pi" });
    if let Some(sc) = turn_scope {
        ev["turnScope"] = json!(sc);
    }
    events.push(ev);
}

// ── usage 结算 ──────────────────────────────────────────────────────
fn apply_usage(ctx: &mut MakerCtx, usage: Option<&Value>, model: Option<&str>, price_variant: &str, now: i64) {
    let Some(usage) = usage else { return };
    let input = usage["input"].as_i64().unwrap_or(0);
    let output = usage["output"].as_i64().unwrap_or(0);
    let cache_read = usage["cacheRead"].as_i64().unwrap_or(0);
    let cache_write = usage["cacheWrite"].as_i64().unwrap_or(0);
    ctx.turn_tokens += input + output;
    ctx.turn_input += input;
    ctx.turn_output += output;
    ctx.turn_main_output += output;
    // parseOutputLimitDetails：reasoning 是数值时进 turn 累计（0 也要记，形状才对齐）。
    let reasoning = usage["reasoning"].as_i64();
    if let Some(r) = reasoning {
        ctx.turn_reasoning = Some(ctx.turn_reasoning.unwrap_or(0) + r);
    }
    ctx.turn_cache_read += cache_read;
    ctx.turn_cache_write += cache_write;
    let request_context_tokens = input + cache_read + cache_write;
    if request_context_tokens > 0 {
        ctx.context_tokens = request_context_tokens;
    }
    if let Some(cost) = usage["cost"]["total"].as_f64() {
        ctx.cost_usd += cost;
    }
    let model_key = model.unwrap_or("unknown").to_string();
    let e = ctx.request_models.entry(model_key.clone()).or_insert((0, 0, 0, 0));
    *e = (e.0 + input, e.1 + output, e.2 + cache_read, e.3 + cache_write);
    ctx.turn_usage_segment_seq += 1;
    let mut segment = json!({
        "id": format!("pi:{}", ctx.turn_usage_segment_seq),
        "model": model_key,
        "inputTokens": input, "outputTokens": output,
        "cacheReadTokens": cache_read, "cacheCreateTokens": cache_write,
        "costUsd": usage["cost"]["total"].as_f64().unwrap_or(0.0),
        "priceVariant": price_variant,
    });
    if let Some(r) = reasoning {
        segment["reasoningTokens"] = json!(r);
    }
    let mut receipt = segment.clone();
    receipt["recordedAt"] = json!(now);
    ctx.turn_usage_segments.push(segment);
    ctx.usage_receipts.push(receipt);
}

/// piTurnUsageOf：纯快照，读它不消耗用量（宿主收尾失败时可重读）。reasoningTokens 只在上报过 reasoning 时才带。
fn turn_usage_snapshot(ctx: &MakerCtx) -> Value {
    let mut usage = json!({
        "settlementId": ctx.usage_settlement_id,
        "inputTokens": ctx.turn_input,
        "outputTokens": ctx.turn_output,
        "cacheReadTokens": ctx.turn_cache_read,
        "cacheCreationTokens": ctx.turn_cache_write,
        "segments": ctx.turn_usage_segments,
        "segmentsComplete": true,
        "receipts": ctx.usage_receipts,
    });
    if let Some(r) = ctx.turn_reasoning {
        usage["reasoningTokens"] = json!(r);
    }
    usage
}

/// agent_settled 用：快照 + 生成时长 + 回合墙钟。
fn turn_usage_of(ctx: &MakerCtx, now: i64) -> Value {
    let mut usage = turn_usage_snapshot(ctx);
    // TS：usageDurationMs 优先取校准后的 meter elapsedMs，否则取累计生成时长；仅在计时可靠且 >0 时带 durationMs。
    // durationMs 只算生成时间（排除首包等待）。
    let calibrated = ctx.token_speed.snapshot(now).and_then(|ts| ts["elapsedMs"].as_f64()).filter(|e| e.is_finite() && *e > 0.0).map(|e| e.round() as i64);
    let usage_duration_ms = calibrated.unwrap_or(ctx.generation_duration_ms);
    if ctx.generation_timing_reliable && usage_duration_ms > 0 {
        usage["durationMs"] = json!(usage_duration_ms);
    }
    if ctx.turn_wall_clock_started_at > 0 {
        usage["turnDurationMs"] = json!((now - ctx.turn_wall_clock_started_at).max(0));
    }
    usage
}

impl MakerCtx {
    /// 在 prompt RPC 发出前登记其尚未到达的 agent_start。
    pub fn mark_host_turn_start_pending(&mut self) -> u64 {
        self.host_abort_seq += 1;
        self.pending_host_turn_start = Some(self.host_abort_seq);
        self.host_abort_seq
    }

    /// 明确得知 prompt 未启动时，只撤销对应的待开始标记（以及指向那个不存在的回合的 abort）。
    pub fn rollback_host_turn_start(&mut self, token: u64) {
        if self.pending_host_turn_start != Some(token) {
            return;
        }
        self.pending_host_turn_start = None;
        if self.host_abort_generation == Some(self.turn_generation + 1) {
            self.clear_host_abort();
        }
    }

    /// 在 abort RPC 发出前登记宿主主动停止。agent_start 还没到时它针对的是下一代回合。
    pub fn mark_host_abort_requested(&mut self) -> u64 {
        let target = if !self.is_streaming && self.pending_host_turn_start.is_some() { self.turn_generation + 1 } else { self.turn_generation };
        if self.host_abort_generation != Some(target) {
            self.host_abort_tokens.clear();
            self.host_abort_generation = Some(target);
        }
        self.host_abort_seq += 1;
        self.host_abort_tokens.push(self.host_abort_seq);
        self.host_abort_seq
    }

    /// abort RPC 没被接受时回滚，防止旧标记吞掉后续真实断流。
    pub fn rollback_host_abort(&mut self, token: u64) {
        self.host_abort_tokens.retain(|t| *t != token);
        if self.host_abort_tokens.is_empty() {
            self.host_abort_generation = None;
        }
    }

    fn host_abort_requested(&self) -> bool {
        !self.host_abort_tokens.is_empty() && self.host_abort_generation == Some(self.turn_generation)
    }

    fn clear_host_abort(&mut self) {
        self.host_abort_tokens.clear();
        self.host_abort_generation = None;
    }
}

fn publish_usage_receipts(events: &mut Vec<Value>, ctx: &mut MakerCtx) {
    let receipts: Vec<Value> = ctx.usage_receipts[ctx.usage_receipt_cursor.min(ctx.usage_receipts.len())..].to_vec();
    if receipts.is_empty() {
        return;
    }
    ctx.usage_receipt_cursor = ctx.usage_receipts.len();
    let sum = |key: &str| receipts.iter().map(|r| r[key].as_i64().unwrap_or(0)).sum::<i64>();
    events.push(json!({
        "type": "account_usage",
        "source": "pi",
        "data": {
            "type": "pi/request_usage",
            "usage": {
                "settlementId": ctx.usage_settlement_id,
                "receipts": receipts,
                "inputTokens": sum("inputTokens"),
                "outputTokens": sum("outputTokens"),
                "cacheReadTokens": sum("cacheReadTokens"),
                "cacheCreationTokens": sum("cacheCreateTokens"),
                "segments": receipts,
                "segmentsComplete": true,
            },
        },
    }));
}

// ── 帧翻译 ──────────────────────────────────────────────────────────
fn assistant_text_of(message: &Value) -> String {
    let mut parts: Vec<String> = Vec::new();
    for block in message["content"].as_array().unwrap_or(&Vec::new()) {
        if block["type"] == "text" {
            if let Some(t) = block["text"].as_str() {
                let visible = strip_internal_web_citations(t);
                if !visible.is_empty() {
                    parts.push(visible);
                }
            }
        }
    }
    parts.join("\n\n")
}

fn sample_generation_heartbeat(ctx: &mut MakerCtx, now: i64) {
    // PI_GENERATION_HEARTBEAT_MS(5s) + SUSPEND_GAP：fixture 内单请求远小于间隔，恒可靠。
    ctx.generation_heartbeat_at = now;
}

fn stop_generation_heartbeat(ctx: &mut MakerCtx) {
    ctx.generation_heartbeat_at = 0;
    // 关闭生成时钟：generationActive（openStartedAt != null）随之变 false。
    ctx.generation_open_at = 0;
}

/// 单帧翻译入口（对齐 translatePiEvent）。返回本帧产生的事件。
pub fn translate_event(event: &Value, ctx: &mut MakerCtx) -> Vec<Value> {
    let mut events = Vec::new();
    let now = now_ms();
    match event["type"].as_str().unwrap_or("") {
        "agent_start" => {
            // 重入的 agent_start（Pi 重试重开同一回合）：宿主没登记待开始的 prompt 时，只清回合内临时状态，不发事件。
            if ctx.is_streaming && !ctx.turn_settled && !ctx.terminal_assistant_error_emitted && ctx.pending_host_turn_start.is_none() {
                ctx.pending_assistant_error = None;
                ctx.final_assistant_text = String::new();
                ctx.final_assistant_stop_reason = None;
                ctx.pending_price_variants.clear();
                return events;
            }
            ctx.turn_generation += 1;
            ctx.pending_host_turn_start = None;
            // 登记的 abort 针对的不是这一代回合（例如 prompt 被拒后遗留）就丢掉，别误伤新回合。
            if ctx.host_abort_generation != Some(ctx.turn_generation) {
                ctx.clear_host_abort();
            }
            ctx.cache_recovery_has_output = false;
            ctx.is_streaming = true;
            ctx.turn_tokens_reset();
            ctx.turn_settled = false;
            ctx.usage_settlement_id = uuid_v4();
            ctx.final_assistant_text = String::new();
            ctx.final_assistant_stop_reason = None;
            ctx.pending_assistant_error = None;
            ctx.terminal_assistant_error_emitted = false;
            ctx.turn_wall_clock_started_at = now;
            ctx.token_speed.reset();
            ctx.generation_duration_ms = 0;
            ctx.generation_timing_reliable = true;
            ctx.generation_heartbeat_at = 0;
            ctx.generation_open_at = 0;
            ctx.tool_names_by_call_id.clear();
            ctx.stream_stop_token_by_index.clear();
            push_status(&mut events, ctx, "Working…", true, now);
        }
        "turn_start" => {}
        "message_start" => {
            let message = &event["message"];
            let role = message["role"].as_str().unwrap_or("");
            if !role.is_empty() && role != "assistant" {
                return events;
            }
            ctx.thinking_blocks.clear();
            ctx.stream_stop_token_by_index.clear();
            let has_model = message["model"].as_str().map(|s| !s.trim().is_empty()).unwrap_or(false);
            let has_api = message["api"].as_str().map(|s| !s.trim().is_empty()).unwrap_or(false);
            if has_model || has_api {
                // setPiTranslateModel：fixture 单模型，记录即可。
            }
            ctx.pending_price_variants.push("standard");
            // startPiGenerationHeartbeat
            ctx.generation_heartbeat_reliable = true;
            ctx.generation_heartbeat_at = now;
            ctx.generation_open_at = now;
            push_status(&mut events, ctx, "Working…", true, now);
        }
        "message_update" => {
            let delta = &event["assistantMessageEvent"];
            if !delta.is_object() || delta["type"].as_str().is_none() {
                return events;
            }
            match delta["type"].as_str().unwrap_or("") {
                "text_start" | "thinking_start" | "toolcall_start" => {
                    ctx.cache_recovery_has_output = true;
                    ctx.token_speed.start(now, delta["type"] != "thinking_start");
                    if delta["type"] == "thinking_start" {
                        let redacted = is_redacted_thinking(delta);
                        ensure_thinking_block(&mut events, ctx, delta["contentIndex"].as_i64().unwrap_or(0), redacted, now);
                    }
                }
                "thinking_end" => handle_thinking_end(&mut events, ctx, delta, now),
                "text_delta" => {
                    if let Some(d) = delta["delta"].as_str() {
                        if !d.is_empty() { ctx.cache_recovery_has_output = true; }
                        if ctx.token_speed.record_delta(d, now) {
                            let st = ctx.last_running_status.clone();
                            push_status(&mut events, ctx, &st, true, now);
                        }
                    }
                    handle_text_delta(&mut events, ctx, delta);
                }
                "thinking_delta" => {
                    ctx.cache_recovery_has_output = true;
                    if let Some(d) = delta["delta"].as_str() {
                        if ctx.token_speed.record_delta(d, now) {
                            let st = ctx.last_running_status.clone();
                            push_status(&mut events, ctx, &st, true, now);
                        }
                    }
                    handle_thinking_delta(&mut events, ctx, delta, now);
                }
                "toolcall_delta" => {
                    ctx.cache_recovery_has_output = true;
                    if let Some(d) = delta["delta"].as_str() {
                        if ctx.token_speed.record_delta(d, now) {
                            let st = ctx.last_running_status.clone();
                            push_status(&mut events, ctx, &st, true, now);
                        }
                    }
                }
                "toolcall_end" => ctx.token_speed.pause(now),
                _ => {}
            }
        }
        "message_end" => {
            let message = &event["message"];
            if message["role"].as_str() != Some("assistant") {
                return events;
            }
            let pending_price_variant = ctx.pending_price_variants.pop().unwrap_or("standard");
            let usage = message.get("usage");
            apply_usage(ctx, usage, message["model"].as_str(), pending_price_variant, now);
            publish_usage_receipts(&mut events, ctx);
            let had_heartbeat = ctx.generation_heartbeat_at > 0;
            sample_generation_heartbeat(ctx, now);            let duration = message["duration"].as_i64().unwrap_or(0);
            let timestamp = message["timestamp"].as_i64().unwrap_or(0);
            let message_duration_ms = if duration > 0 {
                duration
            } else if had_heartbeat && ctx.generation_heartbeat_reliable && timestamp > 0 {
                now - timestamp
            } else {
                0
            };
            if message_duration_ms > 0 {
                ctx.generation_duration_ms += message_duration_ms;
            } else if (usage.map(|u| u["output"].as_i64().unwrap_or(0)).unwrap_or(0)) > 0 {
                ctx.generation_timing_reliable = false;
            }
            stop_generation_heartbeat(ctx);
            ctx.final_assistant_stop_reason = message["stopReason"].as_str().map(String::from);
            let full_text = assistant_text_of(message);
            // 瞬时失败（stopReason=error，或网络类 aborted）登记为待收尾错误；正常消息证明之前的失败已恢复。
            let host_initiated_abort = message["stopReason"].as_str() == Some("aborted") && ctx.host_abort_requested();
            if !host_initiated_abort && errs::is_transient_assistant_failure(message) {
                let raw = message["errorMessage"].as_str().map(str::trim).filter(|e| !e.is_empty()).map(String::from)
                    .or_else(|| Some(full_text.trim().to_string()).filter(|t| !t.is_empty()))
                    .unwrap_or_else(|| "Pi agent request failed".to_string());
                ctx.pending_assistant_error = Some(errs::assistant_error_of(&raw));
            } else {
                ctx.pending_assistant_error = None;
            }
            if !full_text.is_empty() {
                ctx.final_assistant_text = full_text.clone();
                events.push(json!({
                    "type": "text",
                    "data": { "text": full_text, "isFinal": true, "isFullText": true },
                    "source": "pi",
                    "agentMeta": { "model": message["model"], "stopReason": message["stopReason"], "usage": usage.cloned().unwrap_or(Value::Null) },
                }));
            }
            ctx.token_speed.pause(now);
            // 校准：有上报 output 时以 turn 总量锚定 meter。
            let meter_tokens = ctx.token_speed.snapshot(now).and_then(|s| s["tokens"].as_i64()).unwrap_or(0);
            if usage.map(|u| u["output"].is_i64() || u["output"].is_u64()).unwrap_or(false) {
                let target = ctx.turn_main_output + ctx.unreported_estimated_output;
                let dur = if message_duration_ms > 0 { Some(message_duration_ms) } else { None };
                ctx.token_speed.settle(target, now, None, dur);
                ctx.last_calibrated_tokens = target;
            } else {
                ctx.unreported_estimated_output += (meter_tokens - ctx.last_calibrated_tokens).max(0);
                ctx.last_calibrated_tokens = meter_tokens;
            }
            push_status(&mut events, ctx, "Working…", true, now);
        }
        "tool_execution_start" => {
            ctx.cache_recovery_has_output = true;
            let tool_use_id = event["toolCallId"].as_str().unwrap_or("").to_string();
            let tool_name = event["toolName"].as_str().unwrap_or("tool").to_string();
            let tool_args = event["args"].clone();
            if !tool_use_id.is_empty() {
                ctx.tool_names_by_call_id.insert(tool_use_id.clone(), tool_name.clone());
            }
            events.push(json!({
                "type": "tool_use",
                "data": { "toolUseId": tool_use_id, "toolName": tool_name, "input": tool_args },
                "source": "pi",
            }));
            push_status(&mut events, ctx, &format!("Running {tool_name}…"), true, now);
        }
        "tool_execution_end" => {
            let tool_use_id = event["toolCallId"].as_str().unwrap_or("").to_string();
            let is_error = event["isError"] == json!(true);
            let full_text = tool_result_full_text(&event["result"]);
            events.push(json!({
                "type": "tool_result_full",
                "data": { "toolUseId": tool_use_id, "fullText": full_text, "isError": is_error },
                "source": "pi",
            }));
            events.push(json!({
                "type": "tool_result",
                "data": { "summary": if is_error { "failed" } else { "done" }, "toolUseIds": [tool_use_id] },
                "source": "pi",
            }));
        }
        "turn_end" => {
            let message = &event["message"];
            if message["role"].as_str() == Some("assistant") && message.get("usage").is_none() {
                return events;
            }
        }
        "agent_end" => {}
        "auto_retry_start" => {
            // 退避等待不是生成时间；重试请求的首块恢复计时。
            ctx.token_speed.pause(now);
            ctx.pending_price_variants.clear();
            if ctx.host_abort_requested() {
                return events;
            }
            let (Some(attempt), Some(max)) = (event["attempt"].as_i64(), event["maxAttempts"].as_i64()) else { return events };
            if attempt < 1 || max < attempt {
                return events;
            }
            let sdk_error = event["errorMessage"].as_str().map(errs::redact);
            let raw = sdk_error.clone().filter(|e| !e.trim().is_empty())
                .or_else(|| ctx.pending_assistant_error.as_ref().and_then(|e| e["message"].as_str().map(String::from)))
                .unwrap_or_default();
            let (sig_status, _) = errs::extract_signals(&raw);
            let status = ctx.pending_assistant_error.as_ref().and_then(|e| e["errorStatus"].as_i64()).or(sig_status);
            let mut data = json!({ "isTerminal": false, "willRetry": true });
            if errs::parse_overload(&raw, status).is_some() {
                // 第 1 次不透出：单次抖动 pi 一次重试就过，提示只会闪一下。
                if attempt < 2 {
                    return events;
                }
                data["message"] = json!(errs::format_overload_retry_message(&raw, attempt, max));
                data["reason"] = json!(errs::UPSTREAM_OVERLOAD_REASON);
            } else if errs::is_networkish(&raw) {
                data["message"] = json!(format!("Reconnecting... {attempt}/{max}"));
            } else {
                // LiteLLM in-stream / 未分类 5xx：保持静默，避免误报「模型服务繁忙」。
                return events;
            }
            if let Some(e) = sdk_error {
                data["sdkError"] = json!(e);
            }
            if let Some(st) = status {
                data["errorStatus"] = json!(st);
            }
            events.push(json!({ "type": "error", "data": data, "source": "pi" }));
        }
        "auto_retry_end" => {
            if ctx.turn_settled {
                return events;
            }
            if event["success"] == json!(true) {
                ctx.pending_assistant_error = None;
                return events;
            }
            // 失败 / 中止的重试可能没有 agent_settled：这里直接收尾，turn_settled 保持到下一个 agent_start。
            ctx.turn_settled = true;
            ctx.is_streaming = false;
            ctx.pending_price_variants.clear();
            ctx.token_speed.stop(ctx.turn_main_output + ctx.unreported_estimated_output, now);
            stop_generation_heartbeat(ctx);
            if ctx.host_abort_requested() {
                ctx.pending_assistant_error = None;
                ctx.clear_host_abort();
                // 终止 done 之前不留 idle 尾巴，它会放行下一次发送。
                push_status(&mut events, ctx, "Done", false, now);
                events.push(json!({
                    "type": "done",
                    "data": { "type": "pi/auto_retry_end", "status": "cancelled", "usage": turn_usage_snapshot(ctx) },
                    "source": "pi",
                }));
                return events;
            }
            let raw_final = event["finalError"].as_str().map(str::trim).filter(|e| !e.is_empty());
            let mut final_error = match raw_final {
                Some(r) => errs::assistant_error_of(r),
                None => ctx.pending_assistant_error.clone().unwrap_or_else(|| errs::assistant_error_of("pi auto-retry failed")),
            };
            ctx.pending_assistant_error = None;
            ctx.terminal_assistant_error_emitted = true;
            // 只有 Pi 自己的重试预算用尽才挡住宿主续跑；首次 aborted 半截流没有 auto_retry_*，保持无 reason。
            let exhausted = final_error.get("reason").is_none() && errs::is_networkish(final_error["message"].as_str().unwrap_or(""));
            final_error["usage"] = turn_usage_snapshot(ctx);
            final_error["isTerminal"] = json!(true);
            if exhausted {
                final_error["reason"] = json!(errs::PI_GATEWAY_DROP_REASON);
            }
            if final_error["reason"] == json!(errs::UNSUPPORTED_REQUEST_OPTION_REASON) {
                final_error["piCacheRecoverySafe"] = json!(!ctx.cache_recovery_has_output);
            }
            events.push(json!({ "type": "error", "data": final_error, "source": "pi" }));
            // 终止 error 之后、done 之前先排空旧的 idle status（监听者可能在 done 释放 admission 时同步发送）。
            push_status(&mut events, ctx, "Done", false, now);
            events.push(json!({
                "type": "done",
                "data": {
                    "type": "pi/auto_retry_end", "status": "failed", "result": "",
                    "requestUsage": usage_snapshot_of(ctx, now)["requestUsage"],
                    "usage": turn_usage_snapshot(ctx),
                },
                "source": "pi",
            }));
        }
        "compaction_start" => {
            // 空闲的手动压缩是后台工作；阈值 / 溢出压缩发生在 Pi 活跃的 agent run 里。
            // 开始时锁存 scope：end 到达前可能已开始新回合。
            let scope = if ctx.is_streaming { None } else { Some("background") };
            ctx.compact_turn_scope = Some(if scope.is_some() { "background" } else { "turn" });
            ctx.compact_turn_generation = Some(ctx.turn_generation);
            push_status_with(&mut events, ctx, "Compacting context…", true, now, scope, false);
        }
        "compaction_end" => {
            let belongs = ctx.compact_turn_generation.is_none_or(|g| g == ctx.turn_generation);
            ctx.compact_turn_generation = None;
            let latched = ctx.compact_turn_scope.take();
            let compact_scope: Option<&str> = match latched {
                Some("background") => Some("background"),
                Some(_) => None,
                None => if ctx.is_streaming { None } else { Some("background") },
            };
            let manual = event["reason"].as_str() == Some("manual");
            // 失败 / 取消不是压缩边界：手动压缩仍要收口 Compacting 状态，避免圆环卡 running。
            if event["aborted"] == json!(true) || event["result"].is_null() {
                if belongs && manual && !ctx.is_streaming {
                    push_status_with(&mut events, ctx, "Done", false, now, compact_scope, false);
                }
                return events;
            }
            let result = &event["result"];
            let post = result["estimatedTokensAfter"].as_f64().filter(|v| v.is_finite() && *v >= 0.0);
            let has_snapshot = belongs && post.is_some();
            // 先更新再发布，让后面的 status 带上这份快照；无效值 / 旧的空闲压缩不覆盖更新的用量。
            if has_snapshot {
                ctx.context_tokens = post.unwrap_or(0.0) as i64;
            }
            let mut data = json!({ "trigger": if manual { "manual" } else { "auto" } });
            if let Some(pre) = result.get("tokensBefore").filter(|v| !v.is_null()) {
                data["preTokens"] = pre.clone();
            }
            if has_snapshot {
                data["postTokens"] = result["estimatedTokensAfter"].clone();
            }
            let mut ev = json!({ "type": "compact_boundary", "data": data, "source": "pi" });
            if let Some(sc) = compact_scope {
                ev["turnScope"] = json!(sc);
            }
            events.push(ev);
            // 手动压缩事件必须闭环；auto 在活跃回合内由 agent_settled 收口，压缩期间已开新回合时也不能收口（会误杀）。
            if belongs && manual && !ctx.is_streaming {
                push_status_with(&mut events, ctx, "Done", false, now, compact_scope, false);
            } else if has_snapshot && ctx.is_streaming && compact_scope != Some("background") {
                // 压缩不是回合终态：此刻就发布更低的水位线，并保持后续工具存活。
                push_status_with(&mut events, ctx, "Working…", true, now, None, true);
            }
        }
        // Pi 扩展遥测：即便漏到 RPC 流上也忽略（compaction_end 已带 aborted / errorMessage）。
        "queue_update" | "thinking_level_changed" | "summarization_retry_scheduled" | "summarization_retry_attempt_start"
        | "summarization_retry_finished" | "bash_execution_update" | "session_compact_failed" => {}
        "agent_settled" => {
            if ctx.turn_settled {
                return events;
            }
            ctx.turn_settled = true;
            ctx.is_streaming = false;
            ctx.pending_price_variants.clear();
            ctx.token_speed.stop(ctx.turn_main_output + ctx.unreported_estimated_output, now);
            stop_generation_heartbeat(ctx);
            // 宿主主动 Stop 优先于一切：不当作失败，也不算输出上限。
            let host_abort = ctx.host_abort_requested();
            let output_limited = !host_abort && !ctx.terminal_assistant_error_emitted && ctx.final_assistant_stop_reason.as_deref() == Some("length");
            if host_abort {
                ctx.pending_assistant_error = None;
            }
            // TS：terminalError = pendingAssistantError ?? (outputLimited ? {...} : null)；
            // failed 优先于 cancelled。
            let terminal_error: Option<Value> = if ctx.pending_assistant_error.is_some() {
                ctx.pending_assistant_error.clone()
            } else if output_limited {
                Some(json!({
                    "message": "Pi reached the model output limit. The response may be incomplete.",
                    "sdkError": "Pi response stopped at the model output limit",
                    "reason": "output-limit",
                }))
            } else {
                None
            };
            let outcome = if terminal_error.is_some() || ctx.terminal_assistant_error_emitted {
                "failed"
            } else if host_abort || ctx.final_assistant_stop_reason.as_deref() == Some("aborted") {
                "cancelled"
            } else {
                "completed"
            };
            let usage = turn_usage_of(ctx, now);
            if let Some(te) = &terminal_error {
                let mut data = te.clone();
                data["usage"] = usage.clone();
                data["piCacheRecoverySafe"] = json!(!ctx.cache_recovery_has_output);
                data["isTerminal"] = json!(true);
                events.push(json!({ "type": "error", "data": data, "source": "pi" }));
            }
            ctx.pending_assistant_error = None;
            ctx.clear_host_abort();
            // 静默停止：正常完成但没有任何面向用户的回复。宿主用它做有界的续跑保护（同一会话接着跑，不重放原请求）。
            let silent_stop = outcome == "completed" && !host_abort && terminal_error.is_none() && !ctx.terminal_assistant_error_emitted && ctx.final_assistant_text.trim().is_empty();
            if silent_stop {
                (ctx.logger)("pi turn settled without a user-facing assistant reply");
            }
            let mut done_data = json!({
                "type": "pi/agent_settled",
                "requestUsage": usage_snapshot_of(ctx, now)["requestUsage"],
                "result": if output_limited || outcome == "completed" { ctx.final_assistant_text.clone() } else { String::new() },
                "status": outcome,
                "usage": usage,
            });
            if silent_stop {
                done_data["silentStop"] = json!(true);
            }
            events.push(json!({ "type": "done", "data": done_data, "source": "pi" }));
            push_status(&mut events, ctx, "Done", false, now);
        }
        _ => {
            (ctx.logger)(&format!("maker: unhandled frame type {}", event["type"]));
        }
    }
    events
}

impl MakerCtx {
    fn turn_tokens_reset(&mut self) {
        // 对齐 TS agent_start：只清 turn 级计数。requestUsageTracker（跨 turn 累计）、
        // contextTokens / costUsd（任务级）一律保留。
        self.cache_recovery_has_output = false;
        self.turn_tokens = 0;
        self.turn_input = 0;
        self.turn_output = 0;
        self.turn_main_output = 0;
        self.turn_reasoning = None;
        self.turn_cache_read = 0;
        self.turn_cache_write = 0;
        self.unreported_estimated_output = 0;
        self.last_calibrated_tokens = 0;
        self.turn_usage_segments.clear();
        self.turn_usage_segment_seq = 0;
        self.usage_receipts.clear();
        self.usage_receipt_cursor = 0;
        self.token_speed.reset();
        self.last_running_status = "Working…".into();
    }
}

fn handle_text_delta(events: &mut Vec<Value>, ctx: &mut MakerCtx, delta: &Value) {
    let Some(d) = delta["delta"].as_str() else { return };
    if d.is_empty() {
        return;
    }
    // 每个 contentIndex 一份缓冲：stop token / 未闭合引用先扣住，确认后再放出。
    let content_index = delta["contentIndex"].as_i64().unwrap_or(0);
    let buffer = ctx.stream_stop_token_by_index.entry(content_index).or_default();
    if let Some(visible) = hold_standalone_stop_token_delta(buffer, d) {
        events.push(json!({ "type": "text", "data": { "text": visible, "isFinal": false }, "source": "pi" }));
    }
}

/// pi 把 redacted 标记放在 partial 的当前 block 上；兼容未附 partial 的旧版 thinking_end 帧（占位文本判定）。
fn is_redacted_thinking(delta: &Value) -> bool {
    let idx = delta["contentIndex"].as_u64().unwrap_or(0) as usize;
    if let Some(block) = delta["partial"]["content"].as_array().and_then(|c| c.get(idx)) {
        if block["type"] == "thinking" && block["redacted"] == json!(true) {
            return true;
        }
    }
    delta["content"].as_str() == Some("[Reasoning redacted]")
}

/// ensureThinkingBlock：返回 (blockId, redacted)；新块且非 redacted 时发 `start`。
fn ensure_thinking_block(events: &mut Vec<Value>, ctx: &mut MakerCtx, content_index: i64, redacted: bool, now: i64) -> (String, bool) {
    if let Some((id, red, _)) = ctx.thinking_blocks.get(&content_index) {
        return (id.clone(), *red);
    }
    ctx.thinking_seq += 1;
    let id = format!("{}-{}", ctx.thinking_id_prefix, ctx.thinking_seq);
    ctx.thinking_blocks.insert(content_index, (id.clone(), redacted, now));
    if !redacted {
        events.push(json!({ "type": "thinking", "data": { "stage": "start", "blockId": id, "startedAt": now }, "source": "pi" }));
    }
    (id, redacted)
}

fn handle_thinking_delta(events: &mut Vec<Value>, ctx: &mut MakerCtx, delta: &Value, now: i64) {
    let content_index = delta["contentIndex"].as_i64().unwrap_or(0);
    let redacted = is_redacted_thinking(delta);
    let (block_id, block_redacted) = ensure_thinking_block(events, ctx, content_index, redacted, now);
    if redacted && !block_redacted {
        if let Some(b) = ctx.thinking_blocks.get_mut(&content_index) {
            b.1 = true;
        }
        events.push(json!({ "type": "thinking", "data": { "stage": "redacted", "blockId": block_id }, "source": "pi" }));
        return;
    }
    if block_redacted {
        return;
    }
    if let Some(d) = delta["delta"].as_str() {
        if !d.is_empty() {
            events.push(json!({ "type": "thinking", "data": { "stage": "delta", "blockId": block_id, "text": d }, "source": "pi" }));
        }
    }
}

fn handle_thinking_end(events: &mut Vec<Value>, ctx: &mut MakerCtx, delta: &Value, now: i64) {
    let content_index = delta["contentIndex"].as_i64().unwrap_or(0);
    let redacted = is_redacted_thinking(delta);
    let (block_id, block_redacted) = ensure_thinking_block(events, ctx, content_index, redacted, now);
    let started_at = ctx.thinking_blocks.get(&content_index).map(|b| b.2).unwrap_or(now);
    ctx.thinking_blocks.remove(&content_index);
    if redacted || block_redacted {
        events.push(json!({ "type": "thinking", "data": { "stage": "redacted", "blockId": block_id }, "source": "pi" }));
        return;
    }
    events.push(json!({
        "type": "thinking",
        "data": {
            "stage": "final", "blockId": block_id,
            "text": delta["content"].as_str().unwrap_or(""),
            "durationMs": now - started_at,
        },
        "source": "pi",
    }));
}

/// toolExecutionResult 的 fullText：tool_result 块的 text 拼接（对齐 toolResultFullText）。
fn tool_result_full_text(result: &Value) -> String {
    let mut parts: Vec<String> = Vec::new();
    for block in result["content"].as_array().unwrap_or(&Vec::new()) {
        if block["type"] == "text" {
            if let Some(t) = block["text"].as_str() {
                parts.push(t.to_string());
            }
        }
    }
    parts.join("\n\n")
}

/// `--replay-maker` 入口：读帧 JSONL，输出 maker 事件 JSONL。
pub fn replay(frames_path: &str) -> Result<(), String> {
    let text = std::fs::read_to_string(frames_path).map_err(|e| e.to_string())?;
    let mut ctx = MakerCtx::new(Box::new(|msg| eprintln!("[maker] {msg}")));
    let mut out = String::new();
    for line in text.lines().filter(|l| !l.trim().is_empty()) {
        let frame: Value = serde_json::from_str(line).map_err(|e| format!("bad frame: {e}"))?;
        if frame["type"] == "response" {
            continue;
        }
        // 伪帧：宿主在发 abort RPC 之前登记主动停止（与 TS 重放器的约定一致）。
        if frame["type"] == "__host_abort" {
            ctx.mark_host_abort_requested();
            continue;
        }
        for ev in translate_event(&frame, &mut ctx) {
            out.push_str(&serde_json::to_string(&ev).map_err(|e| e.to_string())?);
            out.push('\n');
        }
    }
    print!("{out}");
    Ok(())
}
