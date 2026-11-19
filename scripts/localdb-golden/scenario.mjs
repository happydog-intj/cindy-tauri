// 本地库黄金样例的操作脚本（数据）。每个操作带 now（固定时钟）；生成器用原 JS 实现执行并记录结果，Rust 回放对比。
export const BASE = 1_699_963_200_000; // 2023-11-14T12:00:00Z：-11..+11 时区下本地日期都是 11-14 或 11-15 的同一天
const t = (n) => BASE + n * 1000;
const meta = { model: 'm1', provider: 'p1' };
const ev = (type, data, extra = {}) => ({ type, data, ...extra });
const text = (s, extra = {}) => ev('text', { text: s, ...extra });

export function buildOps() {
  const ops = [];
  let n = 0;
  const at = () => t(++n);
  const inv = (channel, ...args) => ops.push({ k: 'invoke', now: at(), channel, args });
  const ev1 = (sid, event) => ops.push({ k: 'persistEvent', now: at(), sid, event });
  const same = (kind, o) => ops.push({ k: kind, now: at(), ...o });

  // ── 会话：默认值、校验、dialogue 工作目录 ──
  inv('local-db:sessions:create', { id: 's1', agentKind: 'pi', title: 'First', model: 'm1', providerId: 'p1', workspaceKind: 'dialogue' });
  inv('local-db:sessions:create', { id: 's2', agentKind: 'pi', workspaceKind: 'project', workingDir: '/w/proj', projectDir: '/w/proj' });
  inv('local-db:sessions:create', { agentKind: 'pi', title: 'Auto id', workspaceKind: 'dialogue', provisional: true });
  inv('local-db:sessions:create', { id: 's1', agentKind: 'pi' }); // 已存在：原样返回
  inv('local-db:sessions:create', { id: 'bad id!' });
  inv('local-db:sessions:create', { id: 's9', agentKind: 'zzz' });
  inv('local-db:sessions:create', { id: 's9', title: 'x'.repeat(201) });
  inv('local-db:sessions:create', { id: 's9', workspaceKind: 'weird' });
  inv('local-db:sessions:create', { id: 's9', remoteHostId: 'h' });
  inv('local-db:sessions:create', null);
  inv('local-db:sessions:create', { id: 's3', agentKind: 'pi', title: 'Third', effort: 'low', permissionMode: 'auto', planModeEnabled: true, fastMode: true, workspaceKind: 'project', workingDir: '/w/other' });
  inv('local-db:sessions:get', 's1');
  inv('local-db:sessions:get', 'nope');
  inv('local-db:sessions:get', '');
  inv('local-db:sessions:get-many', ['s1', 's3', 'x']);
  inv('local-db:sessions:get-many', 'no');
  inv('local-db:sessions:list', 20);
  inv('local-db:sessions:list', 2, undefined, {});
  inv('local-db:sessions:list', 20, 'archived');
  inv('local-db:sessions:list', 0);
  inv('local-db:sessions:list', 20, undefined, { ids: ['s2'] });
  inv('local-db:sessions:list', 20, undefined, { ids: [] });
  inv('local-db:sessions:list', 20, undefined, { before: { updatedAt: t(11), id: 's9' } });
  inv('local-db:sessions:restore-if-archived', 's2');

  // ── 会话更新 ──
  inv('local-db:sessions:update', 's1', { title: 'Renamed', pinnedAt: new Date(t(20)).toISOString(), notificationsMuted: true });
  inv('local-db:sessions:update', 's1', { hiddenAt: null, fastMode: false, effort: 'max', contextTokens: 10, contextWindow: 100, totalTokenUsage: 5, totalCostUsd: 0.25 });
  inv('local-db:sessions:update', 's1', { status: 'archived' });
  inv('local-db:sessions:update', 's1', { status: 'active', summary: 'sum' });
  inv('local-db:sessions:update', 'nope', { title: 'x' });
  inv('local-db:sessions:update', 's1', { workingDir: '/x' });
  inv('local-db:sessions:update', 's1', { agentKind: 'cc' });
  inv('local-db:sessions:update', 's1', { title: 5 });
  inv('local-db:sessions:update', 's1', { title: 'x'.repeat(201) });
  inv('local-db:sessions:update', 's1', { notificationsMuted: 'yes' });
  inv('local-db:sessions:update', 's1', { model: { a: 1 } });
  inv('local-db:sessions:update', 's1', { model: 'm2', providerId: null, pinnedAt: 12345 });
  inv('local-db:sessions:update', 's1', 5);
  inv('local-db:sessions:touchUserSend', 's2', t(30));
  inv('local-db:sessions:touchUserSend', 's2');
  inv('local-db:sessions:set-pinned-card-summaries', true);
  inv('local-db:sessions:set-pinned-card-summaries', 'x');

  // ── upsert（maker-ipc 的 host.upsert-session）──
  same('upsert', { row: { id: 's4', agentKind: 'pi', title: 'Via upsert', workDir: '/d/s4', model: 'mx', providerId: 'px', permissionMode: 'ask', status: 'active' } });
  same('upsert', { row: { id: 's4', title: 'Renamed upsert', model: 'my', status: 'active', updatedAt: t(40) } });
  same('upsert', { row: { id: 's4', title: '', model: null } });
  same('upsert', { row: { notId: 1 } });
  same('upsert', null);

  // ── 消息 ──
  inv('local-db:messages:create', 's1', { clientId: 'c1', role: 'user', content: { text: 'hello **world**' }, createdAt: new Date(t(50)).toISOString() });
  inv('local-db:messages:create', 's1', { clientId: 'c1', role: 'user', content: { text: 'dup' } });
  inv('local-db:messages:create', 's1', { clientId: 'c2', role: 'assistant', content: 'Reply with [link](http://x) and `code`\n\n# Head', agentMeta: { model: 'm1' } });
  inv('local-db:messages:create', 's1', { clientId: 'c3', role: 'tool_use', content: { toolUseId: 'tu1', toolName: 'bash', input: { command: 'ls' } }, toolUseId: 'tu1' });
  inv('local-db:messages:create', 'nope', { clientId: 'c9', role: 'user', content: 'x' });
  inv('local-db:messages:create', 's1', { role: 'user' });
  inv('local-db:messages:create', 's1', null);
  inv('local-db:messages:updateContent', 's1', 'c2', 'Updated reply');
  inv('local-db:messages:updateContent', 's1', 'zz', 'x');
  inv('local-db:messages:list', 's1', {});
  inv('local-db:messages:list', 's1', { limit: 2 });
  inv('local-db:messages:list', 's1', { limit: 1, before: 'uuid-0006' });
  inv('local-db:messages:list', 's1', { before: 'missing' });
  inv('local-db:messages:list', 's1', { beforeTs: t(50) + 500 });
  inv('local-db:messages:list', 'nope');
  inv('local-db:messages:around', 's1', 'uuid-0006', { radius: 1 });
  inv('local-db:messages:around', 's1', 'missing', {});
  inv('local-db:messages:around-client-id', 's1', 'c2', { radius: 1 });
  inv('local-db:messages:estimatedSessionValue');
  inv('local-db:sessions:get', 's1');

  // ── maker:auto-title ──
  inv('maker:auto-title', { sessionId: 's3', text: '  Fix the   build please  ' });
  inv('maker:auto-title', { sessionId: 's3', text: 'again' });
  inv('maker:auto-title', { sessionId: 'nope', text: 'x' });
  inv('maker:auto-title', { sessionId: 's2', text: '   ' });
  inv('maker:auto-title', { sessionId: 's2', text: 'Attachment desc', isUserText: false });
  inv('maker:auto-title', { sessionId: 's2', text: 'x'.repeat(100) + ' 你好🙂' });
  inv('maker:auto-title', { sessionId: 's2' });
  inv('maker:auto-title', 5);

  // ── 事件落库：一个完整回合 ──
  same('persistUser', { sid: 's2', clientId: 'u1', content: { text: 'run it' } });
  same('persistUser', { sid: 's2', clientId: '', content: 'x' });
  same('persistUser', { sid: 'nope', clientId: 'u2', content: 'x' });
  ev1('s2', text('Let me ', { agentMessageId: 'am1' }));
  ev1('s2', text('check.', { agentMessageId: 'am1' }));
  ev1('s2', ev('thinking', { blockId: 'th1', stage: 'final', text: 'hmm', durationMs: 1500 }, { agentMeta: meta }));
  ev1('s2', ev('thinking', { blockId: 'th2', stage: 'redacted' }));
  ev1('s2', ev('thinking', { blockId: 'th3', stage: 'delta', text: 'x' }));
  ev1('s2', ev('tool_use', { toolUseId: 'tu-a', toolName: 'bash', input: { command: 'ls' } }, { agentMeta: meta }));
  ev1('s2', ev('tool_use', { toolUseId: 'tu-a', toolName: 'bash', input: { command: 'ls -la' } }));
  ev1('s2', ev('tool_use', { toolUseId: 'tu-b', toolName: 'cindy_browser', input: { proxyServer: 'http://secret', url: 'x' } }));
  ev1('s2', ev('tool_use', { toolUseId: 'tu-c', toolName: 'cindy_mcp_call_tool', input: 'has proxyServer inside' }));
  ev1('s2', ev('tool_result_full', { toolUseId: 'tu-a', fullText: 'F'.repeat(9000), isError: false }));
  ev1('s2', ev('tool_result', { toolUseIds: ['tu-a'], summary: 'short summary' }));
  ev1('s2', ev('tool_result_full', { toolUseId: 'tu-a', fullText: 'F'.repeat(9500) }));
  ev1('s2', ev('tool_result_full', { toolUseId: 'tu-a', fullText: 'F'.repeat(9500) }));
  ev1('s2', ev('tool_result', { toolUseIds: ['tu-b'], summary: 'b done' }));
  ev1('s2', ev('tool_result_full', { toolUseId: 'tu-c', fullText: 'boom', isError: true }));
  ev1('s2', ev('tool_result_full', { toolUseId: 'tu-orphan', fullText: 'buffered only' }));
  ev1('s2', ev('tool_result_full', { toolUseId: '', fullText: 'x' }));
  ev1('s2', ev('tool_result_full', { toolUseId: 'tu-c' }));
  ev1('s2', text('Done: ', { agentMessageId: 'am2' }));
  ev1('s2', text('all fine.', { agentMessageId: 'am2', isFinal: true }));
  ev1('s2', text('all fine.', { agentMessageId: 'am2', isFinal: true, isFullText: true }));
  ev1('s2', ev('done', {}, { agentMeta: meta }));
  // 新回合：final 无在飞块、DUP-SKIP、standalone、Pi message_end
  ev1('s2', text('standalone note', {}, {}));
  ops[ops.length - 1].event.standaloneText = true;
  ev1('s2', text('Final only', { isFinal: true, agentMessageId: 'am3' }));
  ev1('s2', text('Final only', { isFinal: true, agentMessageId: 'am3' }));
  ev1('s2', { type: 'text', source: 'pi', data: { text: 'pi full', isFinal: true, isFullText: true, agentMessageId: 'am4' } });
  ev1('s2', text('phase', { phase: 'commentary', agentMessageId: 'am5' }));
  ev1('s2', text('', { runtimeRecovery: true }));
  ev1('s2', ev('tool_use', { toolUseId: 'tu-d', toolName: 'read', input: {} }));
  ev1('s2', ev('error', { message: 'retrying', willRetry: true }));
  ev1('s2', ev('error', { message: 'bad thing', reason: 'rate_limit', sdkError: 'E1', toolLoop: { n: 3 } }));
  ev1('s2', ev('error', { message: '' }));
  ev1('s2', { type: 'text', turnScope: 'background', data: { text: 'late' } });
  ev1('s2', { type: 'nonsense', data: {} });
  ev1('s2', null);
  ev1('s2', ev('error', { message: 'terminal2', isTerminal: true }));

  // ── 交互行 ──
  same('interactionReq', { sid: 's2', req: { requestId: 'r1', kind: 'ask_user_question', questions: [{ q: 'ok?' }] } });
  same('interactionReq', { sid: 's2', req: { requestId: 'r1', kind: 'ask_user_question', questions: [] } });
  same('interactionReq', { sid: 's2', req: { requestId: 'r2', kind: 'plan_review', plan: 'Do X', planFilePath: '/p.md' } });
  same('interactionReq', { sid: 's2', req: { requestId: 'r3', kind: 'plan_review' } });
  same('interactionReq', { sid: 's2', req: { requestId: 'r4', kind: 'permission' } });
  same('interactionReq', { sid: 's2', req: { kind: 'ask_user_question' } });
  same('interactionRes', { sid: 's2', requestId: 'r1', decision: { answers: { a: 'yes' } } });
  same('interactionRes', { sid: 's2', requestId: 'r1', decision: { dismissed: true } });
  same('interactionRes', { sid: 's2', requestId: 'r2', decision: { behavior: 'deny', reason: 'rework' } });
  same('interactionRes', { sid: 's2', requestId: 'nope', decision: {} });
  same('interactionReq', { sid: 's2', req: { requestId: 'r5', kind: 'plan_review', plan: 'P5' } });
  same('interactionRes', { sid: 's2', requestId: 'r5', decision: { behavior: 'allow', editedPlan: 'P5 edited' } });
  same('interactionReq', { sid: 's2', req: { requestId: 'r6', kind: 'plan_review', plan: 'P6' } });
  same('interactionRes', { sid: 's2', requestId: 'r6', decision: { behavior: 'deny', dismissed: true } });

  // ── 中断 / 错误尾 / 确认 ──（s3 开了回合没收尾，重开库后算被中断）
  same('persistUser', { sid: 's3', clientId: 'u3', content: { text: 'left hanging' } });
  same('reopen', {});
  inv('local-db:sessions:interrupted-pending');
  inv('local-db:sessions:interrupted-resume-candidates');
  inv('local-db:sessions:error-tail-pending');
  inv('local-db:messages:dismiss-error', 's2', 'zzz');
  inv('local-db:messages:dismiss-error', 's2', 'c1');
  inv('local-db:sessions:ack-interrupted', 's2');
  inv('local-db:sessions:dismiss-pending-alerts', ['s1', 's3']);
  inv('local-db:sessions:dismiss-pending-alerts', 'no');

  // ── 最近目录 / 项目别名 ──
  inv('local-db:recent-workdirs:list');
  inv('local-db:recent-workdirs:remove', '/w/proj');
  inv('local-db:recent-workdirs:remove', { path: '/w/other' });
  inv('local-db:recent-workdirs:remove', {});
  inv('local-db:recent-workdirs:list');
  inv('local-db:project-aliases:set', { projectKey: 'k1', alias: 'Alias One' });
  inv('local-db:project-aliases:set', { projectKey: 'k1', alias: 'Alias Uno' });
  inv('local-db:project-aliases:set', { projectKey: 'k2', alias: 'x'.repeat(201) });
  inv('local-db:project-aliases:set', { projectKey: '', alias: 'x' });
  inv('local-db:project-aliases:list');
  inv('local-db:project-aliases:delete', 'k1');
  inv('local-db:project-aliases:delete', '');
  inv('local-db:project-aliases:list');

  // ── 右侧栏页签 ──
  const T = 'local-db:right-sidebar-tabs:';
  inv(`${T}list`, { sessionId: 'draft' });
  inv(`${T}list`, { sessionId: 's1' });
  inv(`${T}list`, null);
  inv(`${T}upsert`, { id: 'tab-a', sessionId: 's1', kind: 'web-browser', position: 0, state: { url: 'https://a.test/' } });
  inv(`${T}upsert`, { id: 'tab-b', sessionId: 's1', kind: 'web-browser', position: 1, state: { url: 'https://b.test/' } });
  inv(`${T}upsert`, { id: 'tab-a', sessionId: 's1', kind: 'web-browser', position: 0, state: { url: 'https://a2.test/' } });
  inv(`${T}upsert`, { id: 'tab-c', sessionId: 's1', kind: 'web-browser', position: 2 });
  inv(`${T}upsert`, { id: 'big', sessionId: 's1', kind: 'web-browser', position: 3, state: { s: 'x'.repeat(17000) } });
  inv(`${T}upsert`, { id: 'o', sessionId: 'missing', kind: 'web-browser', position: 0, state: {} });
  inv(`${T}upsert`, { id: 'neg', sessionId: 's1', kind: 'k', position: -1 });
  inv(`${T}upsert`, { id: 'frac', sessionId: 's1', kind: 'k', position: 1.5 });
  inv(`${T}upsert`, { id: '', sessionId: 's1', kind: 'k', position: 1 });
  inv(`${T}setActive`, { sessionId: 's1', id: 'tab-b' });
  inv(`${T}setActive`, { sessionId: 's1', id: 'nope' });
  inv(`${T}setActive`, { sessionId: 's1', id: null });
  inv(`${T}setActive`, { sessionId: 's1', id: 'tab-a' });
  inv(`${T}reorder`, { sessionId: 's1', orderedIds: ['tab-c', 'tab-b', 'tab-a'] });
  inv(`${T}reorder`, { sessionId: 's1', orderedIds: 'x' });
  inv(`${T}reorder`, { sessionId: 's1', orderedIds: [''] });
  inv(`${T}ensure-singleton`, { sessionId: 's1', kind: 'subagents', state: {} });
  inv(`${T}ensure-singleton`, { sessionId: 's1', kind: 'subagents', state: { a: 1 } });
  inv(`${T}ensure-singleton`, { sessionId: 's1', kind: 'web-browser' });
  inv(`${T}ensure-singleton`, { sessionId: 'draft', kind: 'subagents' });
  inv(`${T}list`, { sessionId: 's1' });
  inv(`${T}close`, { id: 'tab-c' });
  inv(`${T}close`, { id: 'tab-c' });
  for (let i = 0; i < 20; i++) inv(`${T}upsert`, { id: `fill-${i}`, sessionId: 's3', kind: 'web-browser', position: i, state: {} });
  inv(`${T}upsert`, { id: 'one-more', sessionId: 's3', kind: 'web-browser', position: 30, state: {} });
  inv(`${T}upsert`, { id: 'fill-0', sessionId: 's3', kind: 'web-browser', position: 3, state: { url: 'https://c.test/' } });
  inv(`${T}ensure-singleton`, { sessionId: 's3', kind: 'subagents', state: {} });
  inv(`${T}ensure-singleton`, { sessionId: 's3', kind: 'subagents', state: {} });
  inv(`${T}list`, { sessionId: 's3' });

  same('dump', {});
  return ops;
}
