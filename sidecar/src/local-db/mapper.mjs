// 消息/会话行的 camelCase 投影与预览文本。
// 对齐 cindy apps/desktop/src/main/localDb/mapper.ts 的 messageToCamel / sessionToCamel /
// extractMessagePreview（简化版：不 import src/main 与 @cindy/maker-shared，见各函数注释）。

const PREVIEW_MAX_CHARS = 140; // 同 mapper.ts PREVIEW_MAX_CHARS
export const PREVIEW_EXTRACT_CHARS = 4096; // 同 mapper.ts PREVIEW_CONTENT_BOUND_CHARS

const msToIso = (ms) => (typeof ms === 'number' && Number.isFinite(ms) ? new Date(ms).toISOString() : null);

function parseJsonOr(raw, fallback) {
  if (raw == null) return fallback;
  try {
    return JSON.parse(raw);
  } catch {
    return fallback;
  }
}

/** 抄自 mapper.ts messageToCamel：content 先 JSON.parse，失败保留原串；agentMeta 解析失败为 null。
 *  省略了 stripInternalWebCitations / projectMcpAppHistory（依赖 maker-shared 与 src/main）。 */
export function messageToCamel(row) {
  let content = row.content;
  try {
    content = JSON.parse(row.content);
  } catch {
    content = row.content;
  }
  const out = {
    id: row.id,
    clientId: row.client_id,
    sessionId: row.session_id,
    role: row.role,
    content,
    toolUseId: row.tool_use_id ?? null,
    agentMeta: parseJsonOr(row.agent_meta, null),
    agentKind: row.agent_kind ?? null,
    createdAt: new Date(row.created_at).toISOString(),
  };
  if (typeof row.rowid === 'number') out.rowid = row.rowid;
  return out;
}

/** 简化版 computePlainPreview（mapper.ts）：desktop 用 remark 解析 markdown 后取纯文本，
 *  这里用正则去掉常见 markdown 记号，再折叠空白、截到 140 字符。 */
export function plainPreview(text) {
  if (typeof text !== 'string' || !text) return null;
  const collapsed = text
    .replace(/```[^\n]*\n?/g, ' ')
    .replace(/!\[([^\]]*)\]\([^)]*\)/g, '$1')
    .replace(/\[([^\]]*)\]\([^)]*\)/g, '$1')
    .replace(/<[^>]+>/g, ' ')
    .replace(/^\s{0,3}(#{1,6}|>|[-*+]|\d+\.)\s+/gm, '')
    .replace(/[*_~`]+/g, '')
    .replace(/\s+/g, ' ')
    .trim();
  if (!collapsed) return null;
  return collapsed.length > PREVIEW_MAX_CHARS ? collapsed.slice(0, PREVIEW_MAX_CHARS) : collapsed;
}

/** 抄自 mapper.ts extractMessagePreview：assistant 取 JSON 字符串，user 取对象的 .text。 */
export function extractMessagePreview(raw, role) {
  if (!raw) return null;
  let text = null;
  try {
    const parsed = JSON.parse(raw);
    if (typeof parsed === 'string') text = parsed;
    else if (role === 'user' && parsed && typeof parsed === 'object' && typeof parsed.text === 'string') text = parsed.text;
  } catch {
    text = raw;
  }
  return plainPreview(text);
}

/**
 * 简化版 sessionToCamel（mapper.ts）。字段名与 desktop Session 一致；本原型没有的能力
 * （orca、fork、远程主机、worktree、tags/companion）一律给中性值。totalMoney 取
 * zeroUsageMoney()（shared/regionalMoney.ts，dev 构建 region=global → USD）。
 * 省略 projectSessionRuntimeFields（main 注入的运行时投影）。
 */
export function sessionToCamel(row, bootAt) {
  const base = {
    id: row.id,
    userId: '',
    title: row.title,
    workingDir: row.working_dir ?? null,
    projectDir: row.project_dir ?? null,
    workspaceKind: row.workspace_kind === 'dialogue' ? 'dialogue' : 'project',
    model: row.model,
    effort: row.effort,
    permissionMode: row.permission_mode,
    providerId: row.provider_id ?? null,
    status: row.status,
    sdkSessionId: row.sdk_session_id ?? null,
    totalTokenUsage: row.total_token_usage ?? 0,
    totalCostUsd: row.total_cost_usd ?? 0,
    totalMoney: { amount: 0, currency: 'USD', approximate: false, kind: 'actual-cost' },
    contextTokens: row.context_tokens ?? 0,
    contextWindow: row.context_window ?? 0,
    fastMode: !!row.fast_mode,
    planModeEnabled: !!row.plan_mode_enabled,
    clearedAt: msToIso(row.cleared_at),
    pinnedAt: msToIso(row.pinned_at),
    hiddenAt: msToIso(row.hidden_at),
    notificationsMuted: !!row.notifications_muted,
    userSendAt: msToIso(row.user_send_at),
    lastActivityAt: msToIso(row.last_activity_at),
    agentKind: row.agent_kind,
    source: row.source || 'desktop',
    orcaRole: null,
    parentSessionId: null,
    forkedAtMessageId: null,
    worktreePath: null,
    usedProjectContext: false,
    extraDirs: [],
    writableDirs: [],
    remoteHostId: null,
    agentDeviceId: null,
    orcaRemoteLead: null,
    activeTurnStartedAt: row.active_turn_started_at ?? null,
    lastTurnEndedAt: row.last_turn_ended_at ?? null,
    createdAt: new Date(row.created_at).toISOString(),
    updatedAt: new Date(row.updated_at).toISOString(),
    _count: { messages: row.message_count ?? 0 },
    preview: plainPreview(row.latest_extract ?? null),
    summary: row.summary ?? null,
  };
  // 对齐 mapper.ts 末尾的 interruptedTurnStartedAt 判定（hasPendingSessionInterruption，
  // maker-shared/sessionActivity.ts）：只有本进程启动前开始、且没收尾的 turn 才算中断。
  base.interruptedTurnStartedAt = isInterrupted(row, bootAt) ? row.active_turn_started_at : null;
  return base;
}

export function isInterrupted(row, bootAt) {
  const started = row.active_turn_started_at;
  if (typeof started !== 'number' || started <= 0) return false;
  return (
    row.status === 'active' &&
    started < bootAt &&
    started > (row.last_turn_ended_at ?? 0) &&
    started > (row.cleared_at ?? 0)
  );
}
