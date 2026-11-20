// projectHistoryView 的黄金样例：运行上游 maker-shared 的 projectHistoryView / historySubagentScopes。
// 输入是一批「会话脚本」（行序列 × streaming × lazyDetails），输出是投影结果。
import { projectHistoryView, hasVisibleHistoryResult, completesHistoryTurn, isHistoryDetailTool } from '../../frontend/packages/maker-shared/src/historyViewProjection.ts';
import { isDeliveryProseText } from '../../frontend/packages/maker-shared/src/messageRender.ts';
import { isContinuationMessage } from '../../frontend/packages/maker-shared/src/syntheticTrigger.ts';

const norm = (v: any) => JSON.parse(JSON.stringify(v === undefined ? null : v));
const T0 = Date.parse('2026-05-01T10:00:00.000Z');
const iso = (sec: number) => new Date(T0 + sec * 1000).toISOString();

let n = 0;
type R = any;
const row = (role: string, content: any, sec: number, extra: R = {}): R => {
  n++;
  return { id: `m${n}`, clientId: `c${n}`, sessionId: 's1', role, content, toolUseId: null, agentMeta: null, agentKind: 'pi', createdAt: iso(sec), rowid: n, ...extra };
};
const user = (t: string, sec: number, extra: R = {}) => row('user', t, sec, extra);
const asst = (t: string, sec: number, meta: R | null = null, extra: R = {}) => row('assistant', t, sec, { agentMeta: meta, ...extra });
const think = (t: string, sec: number, extra: R = {}) => row('thinking', { text: t, durationMs: 1000, ...extra }, sec);
const use = (id: string, name: string, input: any, sec: number, meta: R | null = null) =>
  row('tool_use', { toolName: name, input }, sec, { toolUseId: id, agentMeta: meta });
const res = (id: string, content: any, sec: number, meta: R | null = null) => row('tool_result', content, sec, { toolUseId: id, agentMeta: meta });
const sealed = { turnCompleted: true };

const long = (k: number) => 'x'.repeat(k);
const cases: any[] = [];
const add = (name: string, rows: R[], streaming = false, lazy = false) => {
  cases.push({ name, streaming, lazy, rows: norm(rows), out: norm(projectHistoryView(rows, streaming, lazy)) });
};
const reset = () => { n = 0; };

// 1. 基础：用户→回答
reset(); add('plain', [user('hi', 0), asst('hello', 5, sealed)]);
reset(); add('empty', []);
reset(); add('only-user', [user('hi', 0)]);
reset(); add('user-streaming', [user('hi', 0)], true);
// 2. 工具活动折叠
reset(); add('tools-folded', [user('q', 0), think('hmm', 1), use('t1', 'Read', { file_path: '/a' }, 2), res('t1', 'ok', 3), use('t2', 'Grep', { pattern: 'x' }, 4), res('t2', 'found', 5), asst('done', 6, sealed)]);
reset(); add('tools-streaming', [user('q', 0), think('hmm', 1), use('t1', 'Read', { file_path: '/a' }, 2), res('t1', 'ok', 3), use('t2', 'Bash', { command: 'ls' }, 4)], true);
reset(); add('tools-streaming-with-answer', [user('q', 0), use('t1', 'Read', {}, 1), res('t1', 'ok', 2), asst('partial', 3), use('t2', 'Bash', { command: 'ls' }, 4)], true);
reset(); add('no-seal-legacy', [user('q', 0), use('t1', 'Read', {}, 1), res('t1', 'ok', 2), asst('final', 3)]);
reset(); add('no-seal-trailing-activity', [user('q', 0), asst('a', 1), use('t1', 'Read', {}, 2), res('t1', 'ok', 3)]);
// 3. 特殊工具
reset(); add('edit-tools', [user('q', 0), use('e1', 'Edit', { file_path: '/a', old_string: 'a', new_string: 'b' }, 1), res('e1', 'ok', 2), use('w1', 'write', { path: '/b', content: 'x' }, 3), res('w1', 'ok', 4), asst('d', 5, sealed)]);
reset(); add('plan-tools', [user('q', 0), use('p1', 'TodoWrite', { todos: [] }, 1), res('p1', 'ok', 2), use('p2', 'update_plan', { text: 'a' }, 3), res('p2', 'ok', 4), use('p3', 'TaskCreate', {}, 5), res('p3', 'ok', 6), asst('d', 7, sealed)]);
reset(); add('agent-tools', [user('q', 0), use('a1', 'Agent', { prompt: 'x' }, 1), res('a1', 'sub done', 2), use('a2', 'collab:spawn', {}, 3), res('a2', 'ok', 4), use('a3', 'subagent', {}, 5), asst('d', 6, sealed)]);
reset(); add('orca-and-ghost', [user('q', 0), use('o1', 'mcp__orca_worker_bridge__send_to_lead', {}, 1), res('o1', '{"ok":true}', 2), use('g1', 'mcp__x__ghost_call', {}, 3), res('g1', 'ok', 4), use('g2', 'create_worker', {}, 5), res('g2', 'ok', 6), asst('d', 7, sealed)]);
reset(); add('interactive-tools', [user('q', 0), use('i1', 'AskUserQuestion', {}, 1), res('i1', 'ok', 2), use('i2', 'ExitPlanMode', {}, 3), res('i2', 'ok', 4), use('i3', 'Workflow', {}, 5), asst('d', 6, sealed)]);
// 4. 可见结果（媒体/文件/卡片）
reset(); add('visible-media', [user('q', 0), use('m1', 'mcp__art__gen', { prompt: 'x' }, 1), res('m1', JSON.stringify({ xdt_image_url: 'cindy-media://blobs/' + 'a'.repeat(64) + '.png' }), 2), asst('d', 3, sealed)]);
reset(); add('visible-media-legacy', [user('q', 0), use('m1', 'mcp:x:gen', {}, 1), res('m1', JSON.stringify({ xdt_image_urls: ['xdt-image://a', 'http://x/y.png'], xdt_video_url: 'xdt-video://v' }), 2), asst('d', 3, sealed)]);
reset(); add('visible-audio', [user('q', 0), use('m1', 'mcp:x:tts', {}, 1), res('m1', JSON.stringify({ xdt_audio_tracks: [{ audioUrl: 'xdt-audio://a', title: 'T' }] }), 2), asst('d', 3, sealed)]);
reset(); add('visible-card', [user('q', 0), use('c1', 'mcp:x:card', {}, 1), res('c1', JSON.stringify({ xdt_card_id: 'card-1' }), 2), asst('d', 3, sealed)]);
reset(); add('visible-anchor-card', [user('q', 0), use('c1', 'mcp:x:card', {}, 1), res('c1', JSON.stringify({ xdt_anchor_card_id: 'card-1', xdt_card_id: 'x'.repeat(200) }), 2), asst('d', 3, sealed)]);
reset(); add('visible-receipt', [user('q', 0), use('c1', 'mcp:x:send_to_session', {}, 1), res('c1', JSON.stringify({ ok: true, target_session_id: 's2', wake_kind: 'now' }), 2), asst('d', 3, sealed)]);
reset(); add('visible-file-url', [user('q', 0), use('f1', 'mcp:x:export', {}, 1), res('f1', JSON.stringify({ xdt_media_produced: ['xdt-file:///tmp/a.png'] }), 2), asst('d', 3, sealed)]);
reset(); add('visible-file-in-text', [user('q', 0), use('f1', 'Bash', { command: 'x' }, 1), res('f1', 'saved to xdt-file:///tmp/out.pdf done', 2), asst('d', 3, sealed)]);
reset(); add('file-url-in-source', [user('q', 0), use('f1', 'Read', {}, 1), res('f1', 'const a = "xdt-file:///tmp/out.pdf";\nreturn "xdt-file:///x"', 2), asst('d', 3, sealed)]);
reset(); add('file-url-in-comment', [user('q', 0), use('f1', 'Read', {}, 1), res('f1', '// see xdt-file:///tmp/out.pdf\n/* xdt-file:///b */ nothing', 2), asst('d', 3, sealed)]);
reset(); add('file-url-list', [user('q', 0), use('f1', 'Read', {}, 1), res('f1', "['xdt-file:///a', 'xdt-file:///b']", 2), asst('d', 3, sealed)]);
reset(); add('file-url-glb', [user('q', 0), use('f1', 'mcp:x:model', {}, 1), res('f1', JSON.stringify({ _xdt_model_files: [{ url: 'cindy-media://blobs/' + 'b'.repeat(64) + '.glb', name: 'm' }] }), 2), asst('d', 3, sealed)]);
reset(); add('render-image-false', [user('q', 0), use('m1', 'mcp:x:gen', {}, 1), res('m1', JSON.stringify({ _xdt_render_image: false, xdt_image_url: 'xdt-image://a', xdt_card_id: 'c' }), 2), asst('d', 3, sealed)]);
reset(); add('ghost-envelope', [user('q', 0), use('g1', 'mcp:x:ghost_call', {}, 1), res('g1', JSON.stringify({ ok: true, result: { xdt_image_url: 'xdt-image://a' } }), 2), asst('d', 3, sealed)]);
reset(); add('mcp-app', [user('q', 0), use('g1', 'mcp:x:app', {}, 1, { mcpApp: { a: 1 } }), res('g1', 'ok', 2), asst('d', 3, sealed)]);
reset(); add('mcp-out-path', [user('q', 0), use('g1', 'mcp:x:render', { outPath: '/tmp/o.png' }, 1), res('g1', 'ok', 2), asst('d', 3, sealed)]);
reset(); add('mcp-out-path-lazy', [user('q', 0), use('g1', 'mcp:x:render', { out_path: '/tmp/o.png' }, 1), res('g1', 'ok', 2), asst('d', 3, sealed)], false, true);
reset(); add('pi-mcp-wrapper', [user('q', 0), use('g1', 'cindy_mcp_call_tool', { server: 'x', tool: 'render', args: { output_path: '/o' } }, 1), res('g1', 'ok', 2), asst('d', 3, sealed)]);
// 5. 回答类型
reset(); add('long-answer-prose', [user('q', 0), use('t', 'Read', {}, 1), res('t', 'ok', 2), asst(long(700), 3), use('t2', 'Read', {}, 4), res('t2', 'ok', 5), asst('final', 6, sealed)]);
reset(); add('heading-answer', [user('q', 0), use('t', 'Read', {}, 1), res('t', 'ok', 2), asst('# Title\nbody', 3), use('t2', 'Read', {}, 4), res('t2', 'ok', 5), asst('final', 6, sealed)]);
reset(); add('list-answer', [user('q', 0), use('t', 'Read', {}, 1), res('t', 'ok', 2), asst('- a\n- b\n- c', 3), use('t2', 'Read', {}, 4), res('t2', 'ok', 5), asst('final', 6, sealed)]);
reset(); add('table-answer', [user('q', 0), use('t', 'Read', {}, 1), res('t', 'ok', 2), asst('| a | b |\n|---|---|\n| 1 | 2 |', 3), use('t2', 'Read', {}, 4), res('t2', 'ok', 5), asst('final', 6, sealed)]);
reset(); add('image-answer', [user('q', 0), use('t', 'Read', {}, 1), res('t', 'ok', 2), asst('![x](http://a/b.png)', 3), use('t2', 'Read', {}, 4), res('t2', 'ok', 5), asst('final', 6, sealed)]);
reset(); add('html-image-answer', [user('q', 0), use('t', 'Read', {}, 1), res('t', 'ok', 2), asst('<img src="http://a/b.png">', 3), use('t2', 'Read', {}, 4), res('t2', 'ok', 5), asst('final', 6, sealed)]);
reset(); add('rich-answer', [user('q', 0), use('t', 'Read', {}, 1), res('t', 'ok', 2), asst('hi\n```cindy-ui\n{"id":"a"}\n```\n', 3), use('t2', 'Read', {}, 4), res('t2', 'ok', 5), asst('final', 6, sealed)]);
reset(); add('rich-in-fence', [user('q', 0), use('t', 'Read', {}, 1), res('t', 'ok', 2), asst('````md\n```cindy-ui\nx\n```\n````', 3), use('t2', 'Read', {}, 4), res('t2', 'ok', 5), asst('final', 6, sealed)]);
reset(); add('commentary', [user('q', 0), asst('step 1', 1, { assistantPhase: 'commentary' }), use('t', 'Read', {}, 2), res('t', 'ok', 3), asst('final', 4, sealed)]);
reset(); add('special-assistant', [user('q', 0), use('t', 'Read', {}, 1), res('t', 'ok', 2), asst('a', 3, { systemCardType: 'x' }), asst('b', 4, { goalNotice: 1 }), asst('c', 5, { botCollaboration: 1 }), asst('d', 6, sealed)]);
reset(); add('assistant-object-content', [user('q', 0), asst({ text: 'x' } as any, 1), use('t', 'Read', {}, 2), res('t', 'ok', 3), asst('f', 4, sealed)]);
// 6. 多回合 / 封印 / 恢复 / 间隙
reset(); add('two-turns', [user('q1', 0), use('t', 'Read', {}, 1), res('t', 'ok', 2), asst('a1', 3, sealed), user('q2', 10), use('t2', 'Read', {}, 11), res('t2', 'ok', 12), asst('a2', 13, sealed)]);
reset(); add('wakeups', [user('q', 0), use('t', 'Read', {}, 1), res('t', 'ok', 2), asst('a1', 3, sealed), use('t2', 'Read', {}, 4), res('t2', 'ok', 5), asst('a2', 6, sealed)]);
reset(); add('wakeups-delivery', [user('q', 0), asst(long(700), 1, sealed), use('t2', 'Read', {}, 4), res('t2', 'ok', 5), asst('a2', 6, sealed)]);
reset(); add('cost-seal', [user('q', 0), use('t', 'Read', {}, 1), res('t', 'ok', 2), asst('a', 3, { turnCost: { amount: 0.1 } })]);
reset(); add('usage-seal', [user('q', 0), use('t', 'Read', {}, 1), res('t', 'ok', 2), asst('a', 3, { turnUsageDetails: {} })]);
reset(); add('recovery', [user('q', 0), use('t', 'Read', {}, 1), res('t', 'ok', 2), asst('half', 3), user('[UI_ACTION_TRIGGER] continue', 4, { agentMeta: { autoResume: true } }), use('t2', 'Read', {}, 5), res('t2', 'ok', 6), asst('done', 7, sealed)]);
reset(); add('recovery-text', [user('q', 0), asst('half', 3), user(JSON.stringify({ text: '[UI_ACTION_TRIGGER] The previous turn errored partway through. Review the conversation above to determine how far the task progressed, then continue from where it left off. Do not repeat steps that already completed and had external effects (messages or comments already sent, commits already pushed, files already written).' }), 4), asst('done', 7, sealed)]);
reset(); add('steer', [user('q', 0), use('t', 'Read', {}, 1), user('steer it', 2, { delivery: 'steer' }), res('t', 'ok', 3), user('steer2', 4, { agentMeta: { delivery: 'steer' } }), asst('d', 5, sealed)]);
reset(); add('gap-30min', [user('q', 0), use('t', 'Read', {}, 1), res('t', 'ok', 2), use('t2', 'Read', {}, 2 + 31 * 60), res('t2', 'ok', 3 + 31 * 60), asst('d', 4 + 31 * 60, sealed)]);
reset(); add('gap-exact', [user('q', 0), use('t', 'Read', {}, 1), res('t', 'ok', 2), use('t2', 'Read', {}, 2 + 30 * 60), res('t2', 'ok', 3 + 30 * 60), asst('d', 4 + 30 * 60, sealed)]);
reset(); add('compact-boundary', [user('q', 0), use('t', 'Read', {}, 1), res('t', 'ok', 2), row('system', 'compacted', 3), use('t2', 'Read', {}, 4), res('t2', 'ok', 5)], true);
reset(); add('streaming-sealed-before', [user('q', 0), use('t', 'Read', {}, 1), res('t', 'ok', 2), asst('a1', 3, sealed), use('t2', 'Read', {}, 4), res('t2', 'ok', 5), use('t3', 'Bash', {}, 6)], true);
reset(); add('streaming-preview-many', [user('q', 0), ...Array.from({ length: 6 }, (_, i) => [think('t' + i, 1 + i * 3), use('u' + i, 'Read', {}, 2 + i * 3), res('u' + i, 'ok', 3 + i * 3)]).flat()], true);
reset(); add('thinking-forms', [user('q', 0), row('thinking', '', 1), row('thinking', 'text', 2), row('thinking', { text: '' }, 3), row('thinking', { durationMs: 5 }, 4), row('thinking', { isRedacted: true }, 5), row('thinking', null, 6), asst('d', 7, sealed)]);
reset(); add('thinking-finished-at', [user('q', 0), row('thinking', { text: 'a', finishedAt: T0 + 20_000 }, 1), asst('d', 3, sealed)]);
reset(); add('thinking-after-answer', [user('q', 0), use('t', 'Read', {}, 1), res('t', 'ok', 2), asst('answer', 5, sealed), think('late', 4)]);
reset(); add('thinking-fold-tail', [user('q', 0), use('t', 'Read', {}, 1), res('t', 'ok', 2), asst('answer', 10, sealed), row('thinking', { text: 'x', durationMs: 5 }, 6)]);
reset(); add('unpaired-results', [user('q', 0), res('orphan1', 'x', 1), use('t', 'Read', {}, 2), res('t', 'ok', 3), res('orphan2', 'y', 4), asst('d', 5, sealed)]);
reset(); add('unpaired-streaming', [user('q', 0), res('orphan1', 'x', 1), use('t', 'Read', {}, 2)], true);
reset(); add('nested-parent', [user('q', 0), use('a1', 'Agent', {}, 1), use('c1', 'Read', {}, 2, { parentUuid: 'a1' }), res('c1', 'ok', 3, { parentUuid: 'a1' }), res('a1', 'sub', 4), asst('d', 5, sealed)]);
reset(); add('nested-lazy', [user('q', 0), use('a1', 'Agent', {}, 1), use('c1', 'Read', {}, 2, { parentUuid: 'a1' }), res('c1', 'ok', 3, { parentUuid: 'a1' }), asst('m', 3.5, { parentUuid: 'a1', model: 'claude-x' }), res('a1', 'sub', 4), asst('d', 5, sealed)], false, true);
reset(); add('nested-lazy-chain', [user('q', 0), use('a1', 'Agent', {}, 1), use('a2', 'Agent', {}, 2, { parentUuid: 'a1' }), use('c1', 'Read', {}, 3, { parentUuid: 'a2' }), res('c1', 'ok', 4, { parentUuid: 'a2' }), res('a2', 's', 5, { parentUuid: 'a1' }), res('a1', 'sub', 6), asst('d', 7, sealed)], false, true);
reset(); add('nested-lazy-visible', [user('q', 0), use('a1', 'Agent', {}, 1), use('c1', 'mcp:x:gen', {}, 2, { parentUuid: 'a1' }), res('c1', JSON.stringify({ xdt_card_id: 'k' }), 3, { parentUuid: 'a1' }), res('a1', 'sub', 4), asst('d', 5, sealed)], false, true);
reset(); add('nested-lazy-live', [user('q', 0), use('a1', 'Agent', {}, 1), row('assistant', 'x', 2, { id: 'history-live:1', agentMeta: { parentUuid: 'a1' } }), row('assistant', 'y', 3, { id: 'real-1', agentMeta: { parentUuid: 'a1' } }), res('a1', 'sub', 4), asst('d', 5, sealed)], false, true);
reset(); add('lazy-artifacts', [user('q', 0), use('e1', 'Edit', {}, 1), res('e1', '<tool_use_error>x', 2), asst('d', 3, sealed, { historyArtifacts: [{ path: '/a', source: 'tool', createdAt: iso(1), toolUseId: 'e1' }, { path: '/b', source: 'command', createdAt: iso(1) }] })], false, true);
reset(); add('lazy-artifacts-ok', [user('q', 0), use('e1', 'Edit', {}, 1), res('e1', 'fine', 2), asst('d', 3, sealed, { historyArtifacts: [{ path: '/a', source: 'tool', createdAt: iso(1), toolUseId: 'e1' }, { path: '/c', source: 'tool', createdAt: iso(1), toolUseId: 'nope' }] })], false, true);
reset(); add('artifacts-in-group', [user('q', 0), use('t', 'Read', {}, 1, null), res('t', 'ok', 2), { ...use('t2', 'Read', {}, 3), historyArtifacts: [{ path: '/a', source: 'tool', createdAt: iso(3) }] }, res('t2', 'ok', 4), asst('d', 5, sealed)]);
reset(); add('live-ids', [user('q', 0), row('tool_use', { toolName: 'Read', input: {} }, 1, { id: 'history-live:1', toolUseId: 't' }), row('tool_result', 'ok', 2, { id: 'history-live:2', toolUseId: 't' }), asst('d', 3, sealed)]);
reset(); add('content-stamp', [user('q', 0), { ...use('t', 'Read', {}, 1), historyContentStamp: 'abc' }, res('t', 'ok', 2), asst('d', 3, sealed)]);
reset(); add('unicode-hash', [user('问', 0), use('t', 'Read', { p: '😀中文' }, 1), res('t', '结果😀', 2), asst('完成', 3, sealed)]);
reset(); add('bad-dates', [user('q', 0, { createdAt: 'nope' }), use('t', 'Read', {}, 1, null), res('t', 'ok', 2), asst('d', 3, sealed)]);
reset(); add('tool-without-id', [user('q', 0), row('tool_use', { toolName: 'Read', input: {} }, 1), row('tool_result', 'ok', 2), asst('d', 3, sealed)]);
reset(); add('tool-use-bad-content', [user('q', 0), row('tool_use', 'str', 1, { toolUseId: 'x' }), row('tool_use', null, 2, { toolUseId: 'y' }), row('tool_use', { toolName: 5 }, 3), asst('d', 4, sealed)]);
reset(); add('error-rows', [user('q', 0), row('error', { reason: 'x' }, 1), asst('d', 2, sealed)]);
reset(); add('system-rows-mid', [user('q', 0), use('t', 'Read', {}, 1), res('t', 'ok', 2), row('system', 's', 3), use('t2', 'Read', {}, 4), res('t2', 'ok', 5), asst('d', 6, sealed)]);

// 小函数表
const fnCases: any = { visible: [], completes: [], detail: [], delivery: [], continuation: [] };
const resultSamples: any[] = [
  '', 'plain', '{}', '[]', 'null', '{"xdt_card_id":"a"}', '{"xdt_card_id":""}', '{"xdt_card_id":5}', '{"ok":true,"target_session_id":"s","wake_kind":"w"}',
  '{"ok":true,"target_session_id":"s"}', 'cindy-media://blobs/x', ' cindy-media://x ', 'xdt-file://a b', '{"xdt_media_produced":["cindy-media://blobs/' + 'a'.repeat(64) + '.mp4"]}',
  '{"xdt_audio_urls":["xdt-audio://z"]}', '{"xdt_image_url":"xdt-image://a"}', '{"xdt_image_url":"nope://a"}', { type: 'text', text: '{"xdt_card_id":"a"}' }, [{ type: 'text', text: 'xdt-file:///a' }],
  'xdt-file:///a/b.png', 'xdt-file://relative', "'xdt-file:///q'", 'x = "xdt-file:///q"', 'x = `xdt-file:///q`', 'a, "xdt-file:///a", "xdt-file:///b"', '# xdt-file:///a', 'url(xdt-file:///a)', 'xdt-file:///a%20b',
  'xdt-file:///a%zz', '{"a":{"b":"xdt-file:///deep"}}', '{"_xdt_model_files":[{"url":"xdt-file:///m.glb"}]}', 'x = """xdt-file:///py"""', "x = r'xdt-file:///py'", 'x = $"xdt-file:///cs"',
  'xdt-file:///a "quoted" and \'single\'', '/* c */ xdt-file:///a', 'x = "a" + "xdt-file:///b"',
];
for (const r of resultSamples) fnCases.visible.push({ in: norm(r), out: hasVisibleHistoryResult(r) });
const metaSamples: any[] = [null, undefined, {}, { turnCompleted: true }, { turnCompleted: false }, { turnCost: { amount: 1 } }, { turnCost: { amount: 0 } }, { turnCost: { amount: -1 } }, { userTurnCost: { amount: 2 } }, { turnCostUsd: 0.5 }, { userTurnCostUsd: 0 }, { turnUsageDetails: {} }, { turnUsageDetails: null }, { turnCost: 5 }, { turnCostUsd: Infinity }, { turnCostUsd: '1' }];
for (const m of metaSamples) fnCases.completes.push({ in: norm(m), out: completesHistoryTurn({ agentMeta: m } as any) });
for (const nm of ['', 'Read', 'Agent', 'Task', 'subagent', 'collab:x', 'TodoWrite', 'update_plan', 'TaskCreate', 'TaskList', 'AskUserQuestion', 'ExitPlanMode', 'Workflow', 'Edit', 'Write', 'MultiEdit', 'edit', 'write', 'mcp__x__create_worker', 'mcp__x__create_workers', 'mcp__x__send_to_worker', 'x:ghost_call', 'ghost_call', 'mcp__orca_worker_bridge__read_lead', 'send_to_lead', 'lead_status', 'mcp:orca_worker_bridge:lead_status', 'bash']) fnCases.detail.push({ in: nm, out: isHistoryDetailTool(nm) });
const prose = ['', '   ', 'short', 'x'.repeat(599), 'x'.repeat(600), '# h', '  ### h', '####### h', 'a\n# h', '|a|b|\n|---|---|', '| --- | --- |', '![a](b)', '![a](b "t")', '<img src="a">', '<IMG alt=x src=\'a\'>', '- a\n- b', '- a\n- b\n- c', '1. a\n2) b\n3. c', '* a\n+ b\n- c', '    - a\n    - b\n    - c', '```cindy-ui\nx\n```', '~~~cindy-ui\nx\n~~~', '```cindy-ui\nunclosed', '````\n```cindy-ui\nx\n```\n````', '中'.repeat(599), '😀'.repeat(300)];
for (const t of prose) fnCases.delivery.push({ in: t, out: isDeliveryProseText(t) });
const cont: any[] = [{ role: 'assistant' }, { role: 'user', isContinuationTrigger: true }, { role: 'user', agentMeta: { autoResume: true } }, { role: 'user', systemCardType: 'auto-resume' }, { role: 'user', content: 'hi' }, { role: 'user', content: '{bad' }, { role: 'user', content: '{"text":"[UI_ACTION_TRIGGER] x"}' }, { role: 'user', content: { text: '[UI_ACTION_TRIGGER] x' } }, { role: 'user', content: '  {"text":"hello"}' }, { role: 'user', content: 5 }, { role: 'user', content: null }];
for (const m of cont) fnCases.continuation.push({ in: norm(m), out: isContinuationMessage(m) });

async function main() {
  console.log(JSON.stringify({ data: {}, golden: { cases: norm(cases), fn: norm(fnCases) } }));
}
main();
