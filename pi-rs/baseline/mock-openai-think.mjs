// OpenAI chat 流式 mock：reasoning_content 先于正文；记录每个请求体到 MOCK_LOG。
import { createServer } from 'node:http'; import { appendFileSync } from 'node:fs';
const LOG = process.env.MOCK_LOG; let n = 0;
const srv = createServer((req, res) => {
  let body = ''; req.on('data', d => body += d); req.on('end', async () => {
    let j = {}; try { j = JSON.parse(body); } catch {}
    if (LOG) appendFileSync(LOG, JSON.stringify({ url: req.url, body: j }) + '\n');
    n++; res.writeHead(200, { 'content-type': 'text/event-stream' });
    const send = (choices, extra = {}) => res.write(`data: ${JSON.stringify({ id: `c${n}`, object: 'chat.completion.chunk', model: j.model, choices, ...extra })}\n\n`);
    const tick = () => new Promise(r => setTimeout(r, 15));
    const field = process.env.THINK_FIELD || 'reasoning_content';
    for (const p of ['Let me ', 'think.']) { await tick(); send([{ index: 0, delta: { [field]: p }, finish_reason: null }]); }
    for (const p of ['Hello ', 'there.']) { await tick(); send([{ index: 0, delta: { content: p }, finish_reason: null }]); }
    // MOCK_CUT_FIRST=N：前 N 个请求在正文发到一半时直接断开连接（不发 finish_reason / [DONE]）。
    if (process.env.MOCK_CUT_FIRST && n <= Number(process.env.MOCK_CUT_FIRST)) { await tick(); res.destroy(); return; }
    send([{ index: 0, delta: {}, finish_reason: 'stop' }]);
    send([], { usage: { prompt_tokens: 20, completion_tokens: 6, total_tokens: 26 } });
    res.write('data: [DONE]\n\n'); res.end();
  });
});
srv.listen(0, '127.0.0.1', () => console.log(JSON.stringify({ port: srv.address().port })));
