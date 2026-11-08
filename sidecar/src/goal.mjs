// Goal：会话内自主续跑（/goal）。desktop main/goal-host/ 的精简移植。
//
// 做了：设 / 改 / 暂停 / 恢复 / 清除目标；首轮带质量自检与裁决约定，之后每轮结束解析模型末尾的
// goal_status 裁决块（complete / continue / blocked），再决定要不要自动发下一轮；
// 三道护栏（轮数、token 预算、连续空轮）；账号限流 / 上游过载 → usageLimited 到点自动续；
// 重启后把 active 目标接着跑；状态落 session_goals 表并推 maker:goal:status-changed；/goal 命令。
// 没做（用到了再补）：澄清问答答案改写目标、Codex 额度恢复、延迟的 agent 切换、远端会话。
// 纯函数部分（directive / verdict / decideNextGoalState）与 desktop 同口径，改动时两边对齐。

// ── 裁决约定（同 desktop goal-host/directive.ts，模型提示词，措辞别改）──────────────────────────
function buildVerdictContract(includeRefinedObjective) {
  const blockLine = includeRefinedObjective
    ? '{"goal_status":"complete|continue|blocked","reason":"<one short sentence>","refined_objective":"<OPTIONAL — see rule below; omit unless you clarified a vague goal this turn>"}'
    : '{"goal_status":"complete|continue|blocked","reason":"<one short sentence>"}';
  const lines = [
    'When you finish working this turn, end your reply with EXACTLY this fenced block (and nothing after it):',
    '',
    '```json',
    blockLine,
    '```',
    '',
    'Rules for the verdict:',
    '- "complete": the goal is verifiably done. Before claiming this, actually run the check (tests/build/etc.) and show its output this turn.',
    '- "continue": real work remains; you made progress and want another turn.',
    '- "blocked": you cannot proceed without human input (a decision, a credential, or an action you are not permitted to take). Put what you need in "reason".',
  ];
  if (includeRefinedObjective) {
    lines.push(
      '- "refined_objective": OPTIONAL. Include this field ONLY if you used AskUserQuestion this turn to clarify a vague goal and now have a concrete one. Set it to the sharpened goal in the user\'s own language (one clear sentence based on their answer) — the system will replace the stored goal with it. If you did not clarify the goal, or the user chose to keep it as-is, OMIT this field entirely.',
    );
  }
  lines.push('Do real work with tools each turn — do not just chat. Emit the verdict block every turn.');
  return lines.join('\n');
}

const SEPARATOR = '---';

function buildClarifyContract(maxTurns) {
  const lines = [
    'Before doing substantial work, do a quick one-time sanity check of this goal:',
    '- Is the goal clear and specific enough to pursue autonomously and to define what "complete" means?',
  ];
  if (maxTurns != null) {
    lines.push(`- The current turn budget is ${maxTurns} turns — is that plausibly enough for this goal's scope?`);
  }
  lines.push(
    '',
    'If the goal is already clear and specific, do NOT ask anything — just start working toward it. Asking when there is no genuine problem wastes the user\'s time.',
    '',
    'BUT if the goal is too vague or open-ended to know when it would be "complete" — for example a single word or a bare one-liner like "think about it", "take a look", or "improve things" — then you DO have a real concern, so call the AskUserQuestion tool ONCE before working. Frame the options as concrete candidate goals: each option label should itself BE a specific, self-contained goal you could actually pursue and verify (not "what do you want me to do?") — the option label IS the goal. Also include one option whose label is the user\'s original goal verbatim (so picking it keeps the goal unchanged). The system uses the option the user picks as the goal directly, so write each label as the exact goal statement. When the goal is genuinely underspecified, prefer asking via AskUserQuestion over either guessing what the user meant or stalling.'
      + (maxTurns != null ? ' Use AskUserQuestion the same way if the turn budget looks clearly too small for the goal.' : ''),
    '',
    'After the user answers, pursue the concrete goal their answer points to. If that goal is sharper than what they originally typed, report it via the "refined_objective" field of your verdict block (described below) so the system updates the stored goal to match.',
    '',
    'Reserve "goal_status":"blocked" for a different situation: only end the turn as blocked (instead of asking) if making progress would require a dangerous or irreversible action, a credential, or a permission you do not have — NOT merely because the goal is vague. A vague goal is a reason to ask via AskUserQuestion, never a reason to block.',
  );
  return lines.join('\n');
}

export function buildFirstTurnDirective(objective, opts = {}) {
  return [
    '[Goal] Work autonomously toward this goal across multiple turns until it is met:',
    '',
    objective.trim(),
    SEPARATOR,
    buildClarifyContract(opts.maxTurns ?? null),
    SEPARATOR,
    buildVerdictContract(true),
  ].join('\n');
}

export function buildContinuationDirective(objective, lastReason) {
  const reasonLine = lastReason && lastReason.trim() ? `Last status note: ${lastReason.trim()}` : '';
  return [
    '[Goal] Continue working toward this goal:',
    '',
    objective.trim(),
    reasonLine,
    SEPARATOR,
    buildVerdictContract(false),
  ].filter((line) => line !== '').join('\n');
}

// ── 裁决解析（同 desktop goal-host/verdict.ts）──────────────────────────────────────────────
const VALID_VERDICTS = new Set(['complete', 'continue', 'blocked']);

function tryParseVerdict(candidate) {
  let parsed;
  try { parsed = JSON.parse(candidate); } catch { return null; }
  if (typeof parsed !== 'object' || parsed === null) return null;
  const status = parsed.goal_status;
  if (typeof status !== 'string' || !VALID_VERDICTS.has(status)) return null;
  const reason = typeof parsed.reason === 'string' ? parsed.reason.trim() : '';
  const refined = typeof parsed.refined_objective === 'string' ? parsed.refined_objective.trim() : '';
  return { status, reason, ...(refined ? { refinedObjective: refined } : {}) };
}

export function parseVerdict(finalText) {
  if (!finalText || typeof finalText !== 'string') return null;
  let last = null;
  const fence = /```(?:json|jsonc)?\s*([\s\S]*?)```/gi;
  let m;
  while ((m = fence.exec(finalText)) !== null) {
    const v = m[1] != null ? tryParseVerdict(m[1].trim()) : null;
    if (v) last = v;
  }
  if (last) return last;
  const bare = /\{[^{}]*"goal_status"[^{}]*\}/gi;
  let lastBare = null;
  while ((m = bare.exec(finalText)) !== null) {
    const v = tryParseVerdict(m[0]);
    if (v) lastBare = v;
  }
  return lastBare;
}

// ── 状态机（同 desktop goal-host/controller.ts decideNextGoalState，纯函数）─────────────────────
export const TERMINAL_GOAL_STATUSES = new Set(['complete', 'budgetLimited']);
export const OVERLOAD_RESUME_DELAY_MS = 60_000;
export const MAX_CONSECUTIVE_OVERLOAD_TURNS = 3;
const USAGE_LIMIT_DEFAULT_WAIT_MS = 5 * 60_000;

export function decideNextGoalState(prev, outcome) {
  const goalTurn = outcome.origin === 'goal';
  const turnsUsed = prev.turnsUsed + (goalTurn ? 1 : 0);
  const tokensUsed = prev.tokensUsed + (goalTurn ? Math.max(0, outcome.tokensThisTurn) : 0);
  const base = { turnsUsed, tokensUsed, noProgressStreak: prev.noProgressStreak, shouldFire: false };

  if (outcome.errored) {
    const msg = outcome.errorMessage ?? 'unknown error';
    if (outcome.errorKind === 'usage_limit' || outcome.errorKind === 'overload') {
      return {
        ...base,
        status: 'usageLimited',
        lastReason: outcome.errorKind === 'overload' ? 'upstream model overloaded; retrying shortly' : 'usage limit reached',
      };
    }
    const stoppedByUser = /abort|cancel|interrupt|stopped/i.test(msg);
    return {
      ...base,
      status: stoppedByUser ? 'paused' : 'blocked',
      lastReason: stoppedByUser ? 'paused: stopped by user' : `turn failed: ${msg}`,
    };
  }
  if (!goalTurn) return { ...base, status: 'active', lastReason: null, shouldFire: true };
  if (outcome.verdict?.status === 'complete') {
    return { ...base, status: 'complete', lastReason: outcome.verdict.reason || 'goal achieved' };
  }
  if (outcome.verdict?.status === 'blocked') {
    return { ...base, status: 'blocked', lastReason: outcome.verdict.reason || 'agent reported blocked' };
  }
  const noProgressStreak = outcome.sawToolUse ? 0 : prev.noProgressStreak + 1;
  const next = { ...base, noProgressStreak };
  if (prev.budgetTokens != null && tokensUsed >= prev.budgetTokens) {
    return { ...next, status: 'budgetLimited', lastReason: `token budget reached (${tokensUsed}/${prev.budgetTokens})` };
  }
  if (prev.maxTurns != null && turnsUsed >= prev.maxTurns) {
    return { ...next, status: 'budgetLimited', lastReason: `max turns reached (${turnsUsed}/${prev.maxTurns})` };
  }
  if (prev.noProgressLimit != null && noProgressStreak >= prev.noProgressLimit) {
    return { ...next, status: 'paused', lastReason: `paused: ${noProgressStreak} turns with no tool use` };
  }
  return { ...next, status: 'active', lastReason: outcome.verdict?.reason || null, shouldFire: true };
}

// 错误归类：过载（529 / overloaded / 没容量）与账号限流（429 / rate limit / quota）。
export function classifyTurnError(data) {
  const text = [data?.message, data?.code, data?.type, data?.error?.message, data?.error?.type]
    .filter((v) => typeof v === 'string').join(' ');
  if (/\b529\b|overload|no capacity|capacity/i.test(text)) return 'overload';
  if (/\b429\b|rate.?limit|usage.?limit|quota|insufficient.?(credit|balance)|exceeded/i.test(text)) return 'usage_limit';
  return null;
}

// ── 控制器 ──────────────────────────────────────────────────────────────────────────────────
const DEFAULT_LIMITS = { maxTurns: null, budgetTokens: null, noProgressLimit: 3 };
const STATUS_CHANGED = 'maker:goal:status-changed';
const FIRE_DEBOUNCE_MS = 150;
const MAX_OBJECTIVE_CHARS = 4000;

const ipcError = (code, message) => new Error(`[${code}] ${message}`);

function normalizeLimit(value, name) {
  if (value === null) return null;
  if (typeof value === 'number' && Number.isFinite(value) && value > 0) return Math.floor(value);
  throw ipcError('INVALID_PARAMS', `${name} must be a positive number or null`);
}

function rowToState(row) {
  if (!row) return null;
  return {
    sessionId: row.session_id,
    objective: row.objective,
    status: row.status,
    budgetTokens: row.budget_tokens,
    maxTurns: row.max_turns,
    noProgressLimit: row.no_progress_limit,
    turnsUsed: row.turns_used,
    tokensUsed: row.tokens_used,
    noProgressStreak: row.no_progress_streak,
    usageResetAt: row.usage_reset_at,
    lastReason: row.last_reason,
    agentKind: row.agent_kind,
    startedAt: row.started_at,
    updatedAt: row.updated_at,
  };
}

// renderer 看到的扁平状态（preload getGoalStatus 的返回形状）。
function toPayload(state) {
  return {
    sessionId: state.sessionId,
    status: state.status,
    objective: state.objective,
    turnsUsed: state.turnsUsed,
    tokensUsed: state.tokensUsed,
    maxTurns: state.maxTurns,
    noProgressLimit: state.noProgressLimit,
    budgetTokens: state.budgetTokens,
    usageResetAt: state.usageResetAt,
    lastReason: state.lastReason,
  };
}

/**
 * createGoalController({ sql, getSession, ensureSession, isSessionInTurn, stopTurn, emit, defaults, now, log })
 * - sql：node:sqlite 句柄（session_goals 表）。
 * - getSession(id)：已活化的 maker-core Session（可无）；ensureSession(id)：没活就按存档拉起。
 * - isSessionInTurn(id)：本会话当前是否有回合在跑（用户输入或目标续跑）。
 * - stopTurn(id)：中断当前回合（清除 / 暂停时用）。
 * - emit(channel, payload)：推给 renderer。
 */
export function createGoalController({ sql, getSession, ensureSession, isSessionInTurn, stopTurn, emit, onTurnEnd = () => {}, defaults = () => DEFAULT_LIMITS, now = Date.now, log = () => {} }) {
  const turns = new Map(); // sessionId → { text, sawToolUse, tokens, origin, listenerFor }
  const listeners = new Map(); // sessionId → { session, off }
  const timers = new Map(); // sessionId → timeout（防抖续跑 / 限流到点续跑）
  const firing = new Set(); // 正在发下一轮
  const goalTurnsInFlight = new Set();
  const overloadStreak = new Map();
  let disposed = false;

  const select = sql.prepare('SELECT * FROM session_goals WHERE session_id = ?');
  const get = (sessionId) => rowToState(select.get(sessionId));
  const upsert = sql.prepare(`INSERT INTO session_goals (session_id, objective, status, budget_tokens, max_turns, no_progress_limit,
      turns_used, tokens_used, no_progress_streak, usage_reset_at, last_reason, agent_kind, started_at, updated_at)
    VALUES (@sessionId, @objective, @status, @budgetTokens, @maxTurns, @noProgressLimit, @turnsUsed, @tokensUsed, @noProgressStreak,
      @usageResetAt, @lastReason, @agentKind, @startedAt, @updatedAt)
    ON CONFLICT(session_id) DO UPDATE SET objective=excluded.objective, status=excluded.status, budget_tokens=excluded.budget_tokens,
      max_turns=excluded.max_turns, no_progress_limit=excluded.no_progress_limit, turns_used=excluded.turns_used,
      tokens_used=excluded.tokens_used, no_progress_streak=excluded.no_progress_streak, usage_reset_at=excluded.usage_reset_at,
      last_reason=excluded.last_reason, agent_kind=excluded.agent_kind, started_at=excluded.started_at, updated_at=excluded.updated_at`);
  const remove = sql.prepare('DELETE FROM session_goals WHERE session_id = ?');
  const sessionExists = sql.prepare('SELECT 1 AS ok FROM sessions WHERE id = ?');

  function save(state) {
    const next = { ...state, updatedAt: now() };
    upsert.run(next);
    emit(STATUS_CHANGED, { sessionId: next.sessionId, goal: toPayload(next) });
    return next;
  }

  function clearTimer(sessionId) {
    const t = timers.get(sessionId);
    if (t) clearTimeout(t);
    timers.delete(sessionId);
  }

  function detach(sessionId) {
    clearTimer(sessionId);
    listeners.get(sessionId)?.off?.();
    listeners.delete(sessionId);
    turns.delete(sessionId);
    goalTurnsInFlight.delete(sessionId);
  }

  function schedule(sessionId, delayMs) {
    clearTimer(sessionId);
    if (disposed) return;
    const timer = setTimeout(() => {
      timers.delete(sessionId);
      // 限流 / 过载等待结束：先回到 active 才会发下一轮。
      const waiting = get(sessionId);
      if (waiting?.status === 'usageLimited') save({ ...waiting, status: 'active', lastReason: null, usageResetAt: null });
      void fireTurn(sessionId).catch((err) => log('warn', `fire failed: ${err?.message ?? err}`));
    }, Math.max(0, delayMs));
    timer.unref?.();
    timers.set(sessionId, timer);
  }

  function freshTurn() {
    return { text: '', sawToolUse: false, tokens: 0, finalized: false };
  }

  // 订阅会话事件：只关心 text / tool_use / done / 终止 error。
  function attach(sessionId, session) {
    const bound = listeners.get(sessionId);
    if (bound?.session === session) return;
    bound?.off?.();
    if (!turns.has(sessionId)) turns.set(sessionId, freshTurn());
    const off = session.onEvent((event) => onEvent(sessionId, event));
    listeners.set(sessionId, { session, off });
  }

  function tokensOf(event) {
    const d = event.data ?? {};
    const u = d.usage;
    if (u && typeof u === 'object') {
      const fromParts = (u.inputTokens ?? u.promptTokens ?? 0) + (u.outputTokens ?? u.completionTokens ?? 0);
      if (fromParts > 0) return fromParts;
    }
    const models = d.requestUsage?.models;
    if (Array.isArray(models)) return models.reduce((n, m) => n + (m.inputTokens ?? 0) + (m.outputTokens ?? 0), 0);
    return typeof d.tokenUsage === 'number' ? d.tokenUsage : 0;
  }

  function onEvent(sessionId, event) {
    if (disposed || event.turnScope === 'background') return;
    const state = get(sessionId);
    if (!state || state.status !== 'active') return;
    let turn = turns.get(sessionId);
    if (!turn) { turn = freshTurn(); turns.set(sessionId, turn); }
    switch (event.type) {
      case 'text': {
        const d = event.data;
        if (d && typeof d.text === 'string') {
          if (d.isFinal) turn.text = d.text; else turn.text += d.text;
        }
        return;
      }
      case 'tool_use':
        turn.sawToolUse = true;
        return;
      case 'done': {
        if (event.turnOrigin?.kind === 'goal') goalTurnsInFlight.delete(sessionId);
        const t = tokensOf(event);
        if (t > 0) turn.tokens = t;
        // done.data.result 是整轮最终回复，比拼接的流式文本更可靠。
        if (typeof event.data?.result === 'string' && event.data.result) turn.text = event.data.result;
        void finalizeTurn(sessionId, event, false);
        return;
      }
      case 'error':
        if (event.turnOrigin?.kind === 'goal') goalTurnsInFlight.delete(sessionId);
        void finalizeTurn(sessionId, event, true);
        return;
      default:
    }
  }

  async function finalizeTurn(sessionId, event, errored) {
    const turn = turns.get(sessionId);
    if (!turn || turn.finalized) return;
    turn.finalized = true;
    const state = get(sessionId);
    if (!state || state.status !== 'active') { detach(sessionId); onTurnEnd(sessionId); return; }
    const origin = event.turnOrigin?.kind === 'goal' ? 'goal' : 'other';
    const errorKind = errored ? classifyTurnError(event.data) : null;
    const isOverload = errored && errorKind === 'overload';
    if (!isOverload) overloadStreak.delete(sessionId);
    const streak = isOverload ? (overloadStreak.get(sessionId) ?? 0) + 1 : 0;
    if (isOverload) overloadStreak.set(sessionId, streak);
    const exhausted = streak >= MAX_CONSECUTIVE_OVERLOAD_TURNS;
    const outcome = {
      origin,
      sawToolUse: turn.sawToolUse,
      tokensThisTurn: turn.tokens,
      verdict: origin === 'goal' ? parseVerdict(turn.text) : null,
      errored,
      errorMessage: errored ? String(event.data?.message ?? event.data?.error?.message ?? 'unknown error').slice(0, 300) : undefined,
      ...(errored && errorKind && !(isOverload && exhausted) ? { errorKind } : {}),
    };
    const decision = decideNextGoalState(state, outcome);
    turns.set(sessionId, freshTurn());
    // 模型澄清后回报的更具体的目标：替换存下的目标。
    const refined = outcome.verdict?.refinedObjective;
    let resetAt = null;
    if (decision.status === 'usageLimited') {
      resetAt = now() + (outcome.errorKind === 'overload' ? OVERLOAD_RESUME_DELAY_MS : USAGE_LIMIT_DEFAULT_WAIT_MS);
    }
    save({
      ...state,
      objective: refined && origin === 'goal' ? refined.slice(0, MAX_OBJECTIVE_CHARS) : state.objective,
      status: decision.status,
      lastReason: decision.lastReason,
      turnsUsed: decision.turnsUsed,
      tokensUsed: decision.tokensUsed,
      noProgressStreak: decision.noProgressStreak,
      usageResetAt: resetAt,
    });
    if (decision.shouldFire) {
      schedule(sessionId, FIRE_DEBOUNCE_MS);
    } else if (decision.status === 'usageLimited') {
      schedule(sessionId, Math.max(1_000, resetAt - now()));
    } else if (TERMINAL_GOAL_STATUSES.has(decision.status) || decision.status === 'paused' || decision.status === 'blocked') {
      clearTimer(sessionId);
    }
    // 目标轮结束：用户在这期间排的消息可以发了。
    if (origin === 'goal') onTurnEnd(sessionId);
  }

  // 发下一轮：目标 active、会话空闲时才发；用户正在输入的回合结束后会再被调起。
  async function fireTurn(sessionId) {
    if (disposed || firing.has(sessionId)) return;
    const state = get(sessionId);
    if (!state || state.status !== 'active') return;
    if (isSessionInTurn(sessionId)) return; // 回合结束后 finalizeTurn 会再次调度
    firing.add(sessionId);
    try {
      const session = getSession(sessionId) ?? await ensureSession(sessionId);
      if (!session) {
        save({ ...state, status: 'blocked', lastReason: 'unable to restore the agent session for Goal' });
        detach(sessionId);
        return;
      }
      attach(sessionId, session);
      const current = get(sessionId);
      if (!current || current.status !== 'active') return;
      const content = current.turnsUsed === 0
        ? buildFirstTurnDirective(current.objective, { maxTurns: current.maxTurns })
        : buildContinuationDirective(current.objective, current.lastReason);
      turns.set(sessionId, freshTurn());
      const result = await session.send({ type: 'user', content }, {
        origin: { kind: 'goal', goalSessionId: sessionId },
        planMode: false,
        onDispatching: () => { goalTurnsInFlight.add(sessionId); },
      });
      if (!result.accepted) {
        goalTurnsInFlight.delete(sessionId);
        const reason = `turn dispatch failed: ${result.reason ?? 'not accepted'}`;
        // 会话刚好有别的回合：不算失败，回合结束会再调起。
        if (/busy|running|in.?turn/i.test(String(result.reason))) return;
        save({ ...current, status: 'blocked', lastReason: reason });
        detach(sessionId);
      }
    } catch (err) {
      log('warn', `goal dispatch failed for ${sessionId.slice(0, 8)}: ${err?.message ?? err}`);
      const current = get(sessionId);
      if (current?.status === 'active') save({ ...current, status: 'blocked', lastReason: `turn dispatch failed: ${String(err?.message ?? err).slice(0, 200)}` });
      detach(sessionId);
    } finally {
      firing.delete(sessionId);
    }
  }

  return {
    get,
    getStatus: (sessionId) => { const s = get(sessionId); return s ? toPayload(s) : null; },

    async setGoal({ sessionId, objective, limits }) {
      if (disposed) throw ipcError('PRECONDITION_FAILED', 'goal controller has been disposed');
      const text = String(objective ?? '').trim();
      if (!text) throw ipcError('INVALID_PARAMS', 'objective must not be empty');
      if (text.length > MAX_OBJECTIVE_CHARS) throw ipcError('INVALID_PARAMS', `objective must be at most ${MAX_OBJECTIVE_CHARS} characters`);
      if (!sessionExists.get(sessionId)) throw ipcError('NOT_FOUND', 'session not found');
      const d = defaults();
      const lim = {
        maxTurns: limits ? normalizeLimit(limits.maxTurns, 'maxTurns') : d.maxTurns,
        budgetTokens: limits ? normalizeLimit(limits.budgetTokens, 'budgetTokens') : d.budgetTokens,
        noProgressLimit: limits ? normalizeLimit(limits.noProgressLimit, 'noProgressLimit') : d.noProgressLimit,
      };
      if (goalTurnsInFlight.has(sessionId)) stopTurn(sessionId);
      detach(sessionId);
      overloadStreak.delete(sessionId);
      const t = now();
      const saved = save({
        sessionId, objective: text, status: 'active', ...lim, turnsUsed: 0, tokensUsed: 0, noProgressStreak: 0,
        usageResetAt: null, lastReason: null, agentKind: 'pi', startedAt: t, updatedAt: t,
      });
      schedule(sessionId, FIRE_DEBOUNCE_MS);
      return saved;
    },

    async updateGoal(sessionId, patch) {
      const state = get(sessionId);
      if (!state) return null;
      const next = { ...state };
      if (patch.objective !== undefined) {
        const text = String(patch.objective).trim();
        if (!text) throw ipcError('INVALID_PARAMS', 'objective must not be empty');
        if (text.length > MAX_OBJECTIVE_CHARS) throw ipcError('INVALID_PARAMS', `objective must be at most ${MAX_OBJECTIVE_CHARS} characters`);
        next.objective = text;
      }
      for (const key of ['maxTurns', 'budgetTokens', 'noProgressLimit']) {
        if (patch[key] !== undefined) next[key] = normalizeLimit(patch[key], key);
      }
      return save(next);
    },

    async clearGoal(sessionId) {
      const state = get(sessionId);
      if (goalTurnsInFlight.has(sessionId)) stopTurn(sessionId);
      detach(sessionId);
      overloadStreak.delete(sessionId);
      if (!state) return;
      remove.run(sessionId);
      emit(STATUS_CHANGED, { sessionId, goal: null });
    },

    async pauseGoal(sessionId, reason) {
      const state = get(sessionId);
      if (!state || state.status !== 'active') return;
      if (goalTurnsInFlight.has(sessionId)) stopTurn(sessionId);
      clearTimer(sessionId);
      turns.set(sessionId, freshTurn());
      save({ ...state, status: 'paused', lastReason: reason ?? 'paused by user' });
    },

    async resumeGoal(sessionId) {
      const state = get(sessionId);
      if (!state) return;
      if (state.status === 'active') { schedule(sessionId, FIRE_DEBOUNCE_MS); return; }
      if (TERMINAL_GOAL_STATUSES.has(state.status)) return;
      overloadStreak.delete(sessionId);
      save({ ...state, status: 'active', lastReason: null, usageResetAt: null, noProgressStreak: 0 });
      schedule(sessionId, FIRE_DEBOUNCE_MS);
    },

    // 打开会话时：active 目标没有在跑就再踢一轮；限流到点的自动续。
    async resumeOnOpen(sessionId) {
      const state = get(sessionId);
      if (!state) return;
      if (state.status === 'usageLimited' && state.usageResetAt != null && state.usageResetAt <= now()) {
        save({ ...state, status: 'active', lastReason: null, usageResetAt: null });
        schedule(sessionId, FIRE_DEBOUNCE_MS);
        return;
      }
      if (state.status === 'active' && !timers.has(sessionId) && !firing.has(sessionId) && !isSessionInTurn(sessionId)) {
        schedule(sessionId, FIRE_DEBOUNCE_MS);
      }
    },

    // 启动：所有 active 目标接着跑，限流中的按重置时刻排队。
    resumeActiveGoals() {
      const rows = sql.prepare("SELECT * FROM session_goals WHERE status IN ('active', 'usageLimited')").all();
      for (const row of rows) {
        const state = rowToState(row);
        if (state.status === 'usageLimited') {
          const wait = Math.max(1_000, (state.usageResetAt ?? now()) - now());
          schedule(state.sessionId, wait);
          continue;
        }
        schedule(state.sessionId, 1_000);
      }
      return rows.length;
    },

    // 用户输入的回合结束后，active 目标要接着发下一轮。
    noteSessionIdle(sessionId) {
      const state = get(sessionId);
      if (state?.status === 'active' && !timers.has(sessionId) && !firing.has(sessionId)) schedule(sessionId, FIRE_DEBOUNCE_MS);
    },

    isGoalTurn: (sessionId) => goalTurnsInFlight.has(sessionId),

    dispose() {
      disposed = true;
      for (const id of [...listeners.keys(), ...timers.keys()]) detach(id);
    },
  };
}
