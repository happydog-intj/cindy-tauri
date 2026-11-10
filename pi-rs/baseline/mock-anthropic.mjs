// 确定性的 Anthropic Messages 流式 mock：记录每个请求（头名和体），回复文本或一次工具调用。
// 规则与 mock-openai 对齐：最近一条 user 消息含 MOCK_WRITE_FILE 且其后没有 tool_result → 调 write；否则文本。
import { createServer } from 'node:http';
import { writeFileSync, appendFileSync } from 'node:fs';
const LOG = process.env.MOCK_LOG;
let n = 0;
createServer((req, res) => {
  let body = ''; req.on('data', d => body += d);
  req.on('end', async () => {
    const id = `msg_mock_${++n}`;
    let j = {}; try { j = JSON.parse(body); } catch {}
    if (LOG) appendFileSync(LOG, JSON.stringify({ url: req.url, headers: req.headers, body: j }) + '\n');
    if (new URL(req.url, 'http://x').pathname !== '/v1/messages') { res.writeHead(404); res.end('{}'); return; }
    globalThis.__n = (globalThis.__n ?? 0) + 1;
    if (process.env.MOCK_FAIL_FIRST && globalThis.__n <= Number(process.env.MOCK_FAIL_FIRST)) { res.writeHead(503, { 'content-type': 'application/json' }); res.end(JSON.stringify({ type: 'error', error: { type: 'overloaded_error', message: 'Overloaded' } })); return; }
    if (process.env.MOCK_FAIL_SUMMARY && JSON.stringify(j.system ?? '').includes('context summarization assistant') && (globalThis.__sf = (globalThis.__sf ?? 0) + 1) <= Number(process.env.MOCK_FAIL_SUMMARY)) { res.writeHead(503, { 'content-type': 'application/json' }); res.end(JSON.stringify({ type: 'error', error: { type: 'overloaded_error', message: 'Overloaded' } })); return; }
    if (process.env.MOCK_HANG_FIRST && globalThis.__n <= Number(process.env.MOCK_HANG_FIRST)) { return; }
    const msgs = j.messages ?? [];
    const lastUser = [...msgs].reverse().find(m => m.role === 'user' && (typeof m.content === 'string' || m.content.some(b => b.type === 'text')));
    const idxUser = msgs.lastIndexOf(lastUser);
    const hasResult = msgs.slice(idxUser + 1).some(m => Array.isArray(m.content) && m.content.some(b => b.type === 'tool_result'));
    const userText = typeof lastUser?.content === 'string' ? lastUser.content : (lastUser?.content ?? []).filter(b => b.type === 'text').map(b => b.text).join('');
    // MOCK_TOOLS=[{name,args},...]：用户消息 "MOCK_TOOLS_NEXT i" 时发出第 i 个工具调用。
    const tn = process.env.MOCK_TOOLS && !hasResult && userText.match(/MOCK_TOOLS_NEXT (\d+)/);
    const toolSpec = tn ? JSON.parse(process.env.MOCK_TOOLS)[Number(tn[1])] : null;
    const readMatch = !hasResult && userText.match(/MOCK_READ:(\S+)/);
    const sysText = JSON.stringify(j.system ?? '');
    const isSummary = sysText.includes('context summarization assistant');
    const tool = !isSummary && !hasResult && (userText.includes('MOCK_WRITE_FILE') || readMatch || toolSpec);
    res.writeHead(200, { 'content-type': 'text/event-stream' });
    const cut = process.env.MOCK_CUT_FIRST && globalThis.__n <= Number(process.env.MOCK_CUT_FIRST);
    const ev = async (t, d) => { res.write(`event: ${t}\ndata: ${JSON.stringify({ type: t, ...d })}\n\n`); await new Promise(r => setTimeout(r, 20)); };
    await ev('message_start', { message: { id, type: 'message', role: 'assistant', model: j.model, content: [], stop_reason: null, usage: { input_tokens: process.env.MOCK_REAL_USAGE ? Math.ceil(JSON.stringify(j.messages).length / 4) : 10, output_tokens: 1 } } });
    // MOCK_THINK=1 且请求开启了 thinking：先输出 thinking 块（含 signature），再输出后面的内容（块序号顺延）。
    const think = process.env.MOCK_THINK && j.thinking && j.thinking.type !== 'disabled';
    if (think) {
      await ev('content_block_start', { index: 0, content_block: { type: 'thinking', thinking: '', signature: '' } });
      for (const p of ['Let me ', 'think.']) await ev('content_block_delta', { index: 0, delta: { type: 'thinking_delta', thinking: p } });
      await ev('content_block_delta', { index: 0, delta: { type: 'signature_delta', signature: 'sig-abc' } });
      await ev('content_block_stop', { index: 0 });
    }
    const bi = think ? 1 : 0;
    if (tool) {
      await ev('content_block_start', { index: bi, content_block: { type: 'tool_use', id: `toolu_mock_${n}`, name: toolSpec ? toolSpec.name : readMatch ? 'read' : 'write', input: {} } });
      const a = JSON.stringify(toolSpec ? toolSpec.args : readMatch ? { path: readMatch[1] } : { path: 'mock-ok.txt', content: 'ok' }); const h = Math.ceil(a.length / 2);
      for (const p of [a.slice(0, h), a.slice(h)]) await ev('content_block_delta', { index: bi, delta: { type: 'input_json_delta', partial_json: p } });
      await ev('content_block_stop', { index: bi });
      await ev('message_delta', { delta: { stop_reason: 'tool_use', stop_sequence: null }, usage: { output_tokens: 5 } });
    } else {
      let text = hasResult ? 'done' : 'Hello from mock.';
      // MOCK_PLAN=1：计划模式下回复带 Plan: 的编号步骤；执行阶段回复带 [DONE:1] 标记。
      if (process.env.MOCK_PLAN && !hasResult) {
        if (userText.includes('PLAN MODE ACTIVE')) text = 'Plan:\n1. Read the configuration file\n2. Update the settings loader\n3. Run the full test suite';
        else if (userText.includes('Execute the plan')) text = 'Step one finished [DONE:1]';
      }
      await ev('content_block_start', { index: bi, content_block: { type: 'text', text: '' } });
      for (const p of [text.slice(0, 6), text.slice(6)]) await ev('content_block_delta', { index: bi, delta: { type: 'text_delta', text: p } });
      await ev('content_block_stop', { index: bi });
      await ev('message_delta', { delta: { stop_reason: 'end_turn', stop_sequence: null }, usage: { output_tokens: 5 } });
    }
    if (cut) { res.destroy(); return; }
    await ev('message_stop', {});
    res.end();
  });
}).listen(0, '127.0.0.1', function () { console.log(JSON.stringify({ port: this.address().port })); });
