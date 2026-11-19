//! Goal（会话内自主续跑 /goal）的纯函数部分：提示词、裁决解析、状态机、错误归类。
//! 与 desktop main/goal-host/{directive,verdict,controller}.ts 同口径（原 sidecar/src/goal.mjs 的移植）。
//! 验收：`goal_golden.json` 是用原 JS 实现在边界输入上跑出来的输出，下面的测试逐条比对（改措辞要两边一起改）。

use regex::Regex;
use serde_json::{json, Value};
use std::sync::OnceLock;

const SEPARATOR: &str = "---";
pub const OVERLOAD_RESUME_DELAY_MS: i64 = 60_000;
pub const MAX_CONSECUTIVE_OVERLOAD_TURNS: u32 = 3;
pub const USAGE_LIMIT_DEFAULT_WAIT_MS: i64 = 5 * 60_000;

pub fn is_terminal(status: &str) -> bool {
    status == "complete" || status == "budgetLimited"
}

// ── 提示词（模型提示词，措辞别改）──

fn verdict_contract(include_refined: bool) -> String {
    let block_line = if include_refined {
        r#"{"goal_status":"complete|continue|blocked","reason":"<one short sentence>","refined_objective":"<OPTIONAL — see rule below; omit unless you clarified a vague goal this turn>"}"#
    } else {
        r#"{"goal_status":"complete|continue|blocked","reason":"<one short sentence>"}"#
    };
    let mut lines: Vec<&str> = vec![
        "When you finish working this turn, end your reply with EXACTLY this fenced block (and nothing after it):",
        "",
        "```json",
        block_line,
        "```",
        "",
        "Rules for the verdict:",
        r#"- "complete": the goal is verifiably done. Before claiming this, actually run the check (tests/build/etc.) and show its output this turn."#,
        r#"- "continue": real work remains; you made progress and want another turn."#,
        r#"- "blocked": you cannot proceed without human input (a decision, a credential, or an action you are not permitted to take). Put what you need in "reason"."#,
    ];
    if include_refined {
        lines.push(r#"- "refined_objective": OPTIONAL. Include this field ONLY if you used AskUserQuestion this turn to clarify a vague goal and now have a concrete one. Set it to the sharpened goal in the user's own language (one clear sentence based on their answer) — the system will replace the stored goal with it. If you did not clarify the goal, or the user chose to keep it as-is, OMIT this field entirely."#);
    }
    lines.push("Do real work with tools each turn — do not just chat. Emit the verdict block every turn.");
    lines.join("\n")
}

fn clarify_contract(max_turns: Option<i64>) -> String {
    let mut lines: Vec<String> = vec![
        "Before doing substantial work, do a quick one-time sanity check of this goal:".into(),
        r#"- Is the goal clear and specific enough to pursue autonomously and to define what "complete" means?"#.into(),
    ];
    if let Some(n) = max_turns {
        lines.push(format!("- The current turn budget is {n} turns — is that plausibly enough for this goal's scope?"));
    }
    lines.push(String::new());
    lines.push("If the goal is already clear and specific, do NOT ask anything — just start working toward it. Asking when there is no genuine problem wastes the user's time.".into());
    lines.push(String::new());
    let mut vague = String::from(r#"BUT if the goal is too vague or open-ended to know when it would be "complete" — for example a single word or a bare one-liner like "think about it", "take a look", or "improve things" — then you DO have a real concern, so call the AskUserQuestion tool ONCE before working. Frame the options as concrete candidate goals: each option label should itself BE a specific, self-contained goal you could actually pursue and verify (not "what do you want me to do?") — the option label IS the goal. Also include one option whose label is the user's original goal verbatim (so picking it keeps the goal unchanged). The system uses the option the user picks as the goal directly, so write each label as the exact goal statement. When the goal is genuinely underspecified, prefer asking via AskUserQuestion over either guessing what the user meant or stalling."#);
    if max_turns.is_some() {
        vague.push_str(" Use AskUserQuestion the same way if the turn budget looks clearly too small for the goal.");
    }
    lines.push(vague);
    lines.push(String::new());
    lines.push(r#"After the user answers, pursue the concrete goal their answer points to. If that goal is sharper than what they originally typed, report it via the "refined_objective" field of your verdict block (described below) so the system updates the stored goal to match."#.into());
    lines.push(String::new());
    lines.push(r#"Reserve "goal_status":"blocked" for a different situation: only end the turn as blocked (instead of asking) if making progress would require a dangerous or irreversible action, a credential, or a permission you do not have — NOT merely because the goal is vague. A vague goal is a reason to ask via AskUserQuestion, never a reason to block."#.into());
    lines.join("\n")
}

pub fn first_turn_directive(objective: &str, max_turns: Option<i64>) -> String {
    ["[Goal] Work autonomously toward this goal across multiple turns until it is met:", "", objective.trim(), SEPARATOR, &clarify_contract(max_turns), SEPARATOR, &verdict_contract(true)].join("\n")
}

pub fn continuation_directive(objective: &str, last_reason: Option<&str>) -> String {
    let reason_line = match last_reason.map(str::trim).filter(|r| !r.is_empty()) {
        Some(r) => format!("Last status note: {r}"),
        None => String::new(),
    };
    // 与 JS 的 filter(line !== '') 一致：空串（含标题后的空行）全部丢掉。
    ["[Goal] Continue working toward this goal:", "", objective.trim(), &reason_line, SEPARATOR, &verdict_contract(false)]
        .iter()
        .filter(|l| !l.is_empty())
        .copied()
        .collect::<Vec<_>>()
        .join("\n")
}

// ── 裁决解析 ──

#[derive(Debug, Clone, PartialEq)]
pub struct Verdict {
    pub status: String,
    pub reason: String,
    pub refined: Option<String>,
}

impl Verdict {
    pub fn to_json(&self) -> Value {
        let mut v = json!({ "status": self.status, "reason": self.reason });
        if let Some(r) = &self.refined {
            v["refinedObjective"] = json!(r);
        }
        v
    }
}

fn try_parse_verdict(candidate: &str) -> Option<Verdict> {
    let parsed: Value = serde_json::from_str(candidate).ok()?;
    let obj = parsed.as_object()?;
    let status = obj.get("goal_status")?.as_str()?;
    if !["complete", "continue", "blocked"].contains(&status) {
        return None;
    }
    let text = |k: &str| obj.get(k).and_then(Value::as_str).map(|s| s.trim().to_string()).unwrap_or_default();
    let refined = text("refined_objective");
    Some(Verdict { status: status.to_string(), reason: text("reason"), refined: (!refined.is_empty()).then_some(refined) })
}

pub fn parse_verdict(final_text: &str) -> Option<Verdict> {
    static FENCE: OnceLock<Regex> = OnceLock::new();
    static BARE: OnceLock<Regex> = OnceLock::new();
    let fence = FENCE.get_or_init(|| Regex::new(r"(?is)```(?:json|jsonc)?\s*(.*?)```").unwrap());
    let bare = BARE.get_or_init(|| Regex::new(r#"(?i)\{[^{}]*"goal_status"[^{}]*\}"#).unwrap());
    let mut last = None;
    for m in fence.captures_iter(final_text) {
        if let Some(v) = try_parse_verdict(m[1].trim()) {
            last = Some(v);
        }
    }
    if last.is_some() {
        return last;
    }
    let mut last_bare = None;
    for m in bare.find_iter(final_text) {
        if let Some(v) = try_parse_verdict(m.as_str()) {
            last_bare = Some(v);
        }
    }
    last_bare
}

// ── 状态机 ──

#[derive(Debug, Clone)]
pub struct Prev {
    pub turns_used: i64,
    pub tokens_used: i64,
    pub no_progress_streak: i64,
    pub budget_tokens: Option<i64>,
    pub max_turns: Option<i64>,
    pub no_progress_limit: Option<i64>,
}

#[derive(Debug, Clone)]
pub struct Outcome {
    pub goal_turn: bool,
    pub saw_tool_use: bool,
    pub tokens_this_turn: i64,
    pub verdict: Option<Verdict>,
    pub errored: bool,
    pub error_message: Option<String>,
    /// "usage_limit" | "overload"
    pub error_kind: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Decision {
    pub status: String,
    pub last_reason: Option<String>,
    pub turns_used: i64,
    pub tokens_used: i64,
    pub no_progress_streak: i64,
    pub should_fire: bool,
}

impl Decision {
    pub fn to_json(&self) -> Value {
        json!({
            "turnsUsed": self.turns_used, "tokensUsed": self.tokens_used, "noProgressStreak": self.no_progress_streak,
            "shouldFire": self.should_fire, "status": self.status, "lastReason": self.last_reason,
        })
    }
}

fn non_empty(s: &str) -> Option<String> {
    (!s.is_empty()).then(|| s.to_string())
}

pub fn decide_next_goal_state(prev: &Prev, o: &Outcome) -> Decision {
    let turns_used = prev.turns_used + i64::from(o.goal_turn);
    let tokens_used = prev.tokens_used + if o.goal_turn { o.tokens_this_turn.max(0) } else { 0 };
    let mk = |status: &str, reason: Option<String>, streak: i64, fire: bool| Decision {
        status: status.into(), last_reason: reason, turns_used, tokens_used, no_progress_streak: streak, should_fire: fire,
    };
    let base_streak = prev.no_progress_streak;

    if o.errored {
        let msg = o.error_message.clone().unwrap_or_else(|| "unknown error".into());
        match o.error_kind.as_deref() {
            Some("usage_limit") => return mk("usageLimited", Some("usage limit reached".into()), base_streak, false),
            Some("overload") => return mk("usageLimited", Some("upstream model overloaded; retrying shortly".into()), base_streak, false),
            _ => {}
        }
        static STOPPED: OnceLock<Regex> = OnceLock::new();
        let stopped = STOPPED.get_or_init(|| Regex::new(r"(?i)abort|cancel|interrupt|stopped").unwrap()).is_match(&msg);
        return if stopped {
            mk("paused", Some("paused: stopped by user".into()), base_streak, false)
        } else {
            mk("blocked", Some(format!("turn failed: {msg}")), base_streak, false)
        };
    }
    if !o.goal_turn {
        return mk("active", None, base_streak, true);
    }
    if let Some(v) = &o.verdict {
        if v.status == "complete" {
            return mk("complete", non_empty(&v.reason).or(Some("goal achieved".into())), base_streak, false);
        }
        if v.status == "blocked" {
            return mk("blocked", non_empty(&v.reason).or(Some("agent reported blocked".into())), base_streak, false);
        }
    }
    let streak = if o.saw_tool_use { 0 } else { base_streak + 1 };
    if let Some(b) = prev.budget_tokens.filter(|b| tokens_used >= *b) {
        return mk("budgetLimited", Some(format!("token budget reached ({tokens_used}/{b})")), streak, false);
    }
    if let Some(m) = prev.max_turns.filter(|m| turns_used >= *m) {
        return mk("budgetLimited", Some(format!("max turns reached ({turns_used}/{m})")), streak, false);
    }
    if prev.no_progress_limit.is_some_and(|l| streak >= l) {
        return mk("paused", Some(format!("paused: {streak} turns with no tool use")), streak, false);
    }
    mk("active", o.verdict.as_ref().and_then(|v| non_empty(&v.reason)), streak, true)
}

/// 错误归类：过载（529 / overloaded / 没容量）与账号限流（429 / rate limit / quota）。
pub fn classify_turn_error(data: &Value) -> Option<&'static str> {
    let pick = |v: &Value| v.as_str().map(str::to_string);
    let text = [data.get("message"), data.get("code"), data.get("type"), data.get("error").and_then(|e| e.get("message")), data.get("error").and_then(|e| e.get("type"))]
        .into_iter()
        .flatten()
        .filter_map(pick)
        .collect::<Vec<_>>()
        .join(" ");
    // JS 的 \b 是 ASCII 口径（中文紧挨数字也算边界），所以用 (?-u:\b)。
    static OVERLOAD: OnceLock<Regex> = OnceLock::new();
    static USAGE: OnceLock<Regex> = OnceLock::new();
    let overload = OVERLOAD.get_or_init(|| Regex::new(r"(?i)(?-u:\b)529(?-u:\b)|overload|no capacity|capacity").unwrap());
    let usage = USAGE.get_or_init(|| Regex::new(r"(?i)(?-u:\b)429(?-u:\b)|rate.?limit|usage.?limit|quota|insufficient.?(credit|balance)|exceeded").unwrap());
    if overload.is_match(&text) {
        Some("overload")
    } else if usage.is_match(&text) {
        Some("usage_limit")
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn golden() -> Value {
        serde_json::from_str(include_str!("goal_golden.json")).unwrap()
    }

    #[test]
    fn directives_match_the_original_text_byte_for_byte() {
        let g = golden();
        let items = g["directives"].as_array().unwrap();
        assert!(items.len() >= 25);
        for d in items {
            let max_turns = d["maxTurns"].as_i64();
            let got = match d["kind"].as_str().unwrap() {
                "first" => first_turn_directive(d["objective"].as_str().unwrap(), max_turns),
                _ => continuation_directive(d["objective"].as_str().unwrap(), d["reason"].as_str()),
            };
            assert_eq!(got, d["text"].as_str().unwrap(), "{d}");
        }
    }

    #[test]
    fn verdict_parsing_matches_the_original_on_edge_inputs() {
        let g = golden();
        for c in g["verdicts"].as_array().unwrap() {
            let got = c["text"].as_str().and_then(parse_verdict).map(|v| v.to_json()).unwrap_or(Value::Null);
            assert_eq!(got, c["verdict"], "{}", c["text"]);
        }
    }

    #[test]
    fn state_machine_matches_the_original_for_every_combination() {
        let g = golden();
        let items = g["decide"].as_array().unwrap();
        assert!(items.len() >= 200);
        for c in items {
            let p = &c["prev"];
            let prev = Prev {
                turns_used: p["turnsUsed"].as_i64().unwrap(), tokens_used: p["tokensUsed"].as_i64().unwrap(), no_progress_streak: p["noProgressStreak"].as_i64().unwrap(),
                budget_tokens: p["budgetTokens"].as_i64(), max_turns: p["maxTurns"].as_i64(), no_progress_limit: p["noProgressLimit"].as_i64(),
            };
            let o = &c["outcome"];
            let verdict = o["verdict"].as_object().map(|v| Verdict { status: v["status"].as_str().unwrap().into(), reason: v["reason"].as_str().unwrap().into(), refined: None });
            let outcome = Outcome {
                goal_turn: o["origin"] == "goal", saw_tool_use: o["sawToolUse"].as_bool().unwrap(), tokens_this_turn: o["tokensThisTurn"].as_i64().unwrap(), verdict,
                errored: o["errored"].as_bool().unwrap(), error_message: o["errorMessage"].as_str().map(String::from), error_kind: o["errorKind"].as_str().map(String::from),
            };
            assert_eq!(decide_next_goal_state(&prev, &outcome).to_json(), c["result"], "{c}");
        }
    }

    #[test]
    fn error_classification_matches_the_original() {
        let g = golden();
        for c in g["classify"].as_array().unwrap() {
            let got = classify_turn_error(&c["data"]).map(|k| json!(k)).unwrap_or(Value::Null);
            assert_eq!(got, c["kind"], "{}", c["data"]);
        }
    }
}
