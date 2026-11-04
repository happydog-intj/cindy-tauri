// 确定性的 OpenAI Chat Completions 兼容 mock 服务（零依赖），给 CI 冒烟用。
// 启动后在 stdout 打印一行 {"port":N}，请求日志写 stderr（不记录 header，避免泄露 key）。
//
// 接口：GET /models、/v1/models；POST /chat/completions、/v1/chat/completions（支持 stream=true 的 SSE）。
// 回复规则：
// - 最近一条 user 消息之后已有 tool 结果：回复 done；
// - 最近一条 user 消息含 MOCK_WRITE_FILE：返回一个工具调用，写 mock-ok.txt，内容 ok；
// - 其它情况：分几个 chunk 回复 "Hello from mock."。
// 环境变量：MOCK_PORT（默认 0 = 随机端口）；MOCK_DUMP=1 时把每个请求体写到 MOCK_DUMP_DIR（默认系统临时目录）。
import { createServer } from 'node:http';
import { mkdirSync, writeFileSync } from 'node:fs';
import os from 'node:os';
import path from 'node:path';

const TRIGGER = 'MOCK_WRITE_FILE';
const FILE_NAME = 'mock-ok.txt';
const FILE_CONTENT = 'ok';
const MODEL_ID = process.env.MOCK_MODEL_ID || 'mock-model';
const dumpDir = process.env.MOCK_DUMP === '1' ? path.resolve(process.env.MOCK_DUMP_DIR || os.tmpdir()) : null;
if (dumpDir) mkdirSync(dumpDir, { recursive: true });

let seq = 0;
const log = (obj) => process.stderr.write(`[mock-openai] ${JSON.stringify({ at: new Date().toISOString(), ...obj })}\n`);

// 消息 content 可能是字符串或 [{type:'text', text}] 数组。
function textOf(content) {
  if (typeof content === 'string') return content;
  if (Array.isArray(content)) return content.map((p) => (typeof p === 'string' ? p : p?.text ?? '')).join('');
  return '';
}

// 按请求里的 tools 列表挑一个能写文件的工具，并按其参数 schema 组装参数。
function pickWriteTool(tools) {
  const fns = (Array.isArray(tools) ? tools : []).map((t) => t?.function ?? t).filter((f) => f && typeof f.name === 'string');
  const byName = (re) => fns.find((f) => re.test(f.name));
  const propsOf = (f) => Object.keys(f.parameters?.properties ?? {});
  const write = byName(/^(write|write_file|writefile|create_file)$/i) ?? byName(/write/i);
  if (write) {
    const props = propsOf(write);
    const pathKey = props.find((k) => /^(path|file_path|filepath|filename|file)$/i.test(k)) ?? 'path';
    const contentKey = props.find((k) => /^(content|contents|text|data)$/i.test(k)) ?? 'content';
    return { name: write.name, args: { [pathKey]: FILE_NAME, [contentKey]: FILE_CONTENT } };
  }
  const shell = byName(/^(bash|shell|exec|run_command|terminal)$/i) ?? byName(/bash|shell/i);
  if (shell) {
    const props = propsOf(shell);
    const cmdKey = props.find((k) => /^(command|cmd|script)$/i.test(k)) ?? 'command';
    return { name: shell.name, args: { [cmdKey]: `printf ${FILE_CONTENT} > ${FILE_NAME}` } };
  }
  return null;
}

// 决定这一轮的回复：{ kind: 'text', text } 或 { kind: 'tool', name, args }。
function decide(body) {
  const messages = Array.isArray(body.messages) ? body.messages : [];
  let lastUser = -1;
  for (let i = messages.length - 1; i >= 0; i--) {
    if (messages[i]?.role === 'user') { lastUser = i; break; }
  }
  const userText = lastUser >= 0 ? textOf(messages[lastUser].content) : '';
  const hasToolResult = messages.slice(lastUser + 1).some((m) => m?.role === 'tool');
  if (hasToolResult) return { kind: 'text', text: 'done', reason: 'tool-result' };
  if (userText.includes(TRIGGER)) {
    const tool = pickWriteTool(body.tools);
    if (tool) return { kind: 'tool', ...tool, reason: 'trigger' };
    return { kind: 'text', text: 'no write tool available', reason: 'trigger-no-tool' };
  }
  return { kind: 'text', text: 'Hello from mock.', reason: 'default' };
}

const usage = { prompt_tokens: 10, completion_tokens: 5, total_tokens: 15 };

// 文本按 6 个字符切段，模拟流式输出。
function splitText(text) {
  return text.match(/.{1,6}/gs) ?? [text];
}

function sendJson(res, status, obj) {
  const data = JSON.stringify(obj);
  res.writeHead(status, { 'content-type': 'application/json', 'content-length': Buffer.byteLength(data) });
  res.end(data);
}

async function streamReply(res, id, created, model, reply, includeUsageChunk) {
  res.writeHead(200, { 'content-type': 'text/event-stream', 'cache-control': 'no-cache', connection: 'keep-alive' });
  const send = (choices, extra = {}) => {
    res.write(`data: ${JSON.stringify({ id, object: 'chat.completion.chunk', created, model, choices, ...extra })}\n\n`);
  };
  const tick = () => new Promise((r) => setTimeout(r, 20));
  send([{ index: 0, delta: { role: 'assistant', content: '' }, finish_reason: null }]);
  let finish;
  if (reply.kind === 'tool') {
    const args = JSON.stringify(reply.args);
    const half = Math.ceil(args.length / 2);
    send([{ index: 0, delta: { tool_calls: [{ index: 0, id: `call_mock_${id}`, type: 'function', function: { name: reply.name, arguments: '' } }] }, finish_reason: null }]);
    for (const piece of [args.slice(0, half), args.slice(half)]) {
      await tick();
      send([{ index: 0, delta: { tool_calls: [{ index: 0, function: { arguments: piece } }] }, finish_reason: null }]);
    }
    finish = 'tool_calls';
  } else {
    for (const piece of splitText(reply.text)) {
      await tick();
      send([{ index: 0, delta: { content: piece }, finish_reason: null }]);
    }
    finish = 'stop';
  }
  // 客户端要求 include_usage 时按 OpenAI 规范单独发一个 choices 为空的 usage chunk，否则把 usage 附在结束 chunk 上。
  if (includeUsageChunk) {
    send([{ index: 0, delta: {}, finish_reason: finish }]);
    send([], { usage });
  } else {
    send([{ index: 0, delta: {}, finish_reason: finish }], { usage });
  }
  res.write('data: [DONE]\n\n');
  res.end();
}

function plainReply(res, id, created, model, reply) {
  const message = reply.kind === 'tool'
    ? { role: 'assistant', content: null, tool_calls: [{ id: `call_mock_${id}`, type: 'function', function: { name: reply.name, arguments: JSON.stringify(reply.args) } }] }
    : { role: 'assistant', content: reply.text };
  sendJson(res, 200, {
    id, object: 'chat.completion', created, model,
    choices: [{ index: 0, message, finish_reason: reply.kind === 'tool' ? 'tool_calls' : 'stop' }],
    usage,
  });
}

function readBody(req) {
  return new Promise((resolve, reject) => {
    const chunks = [];
    let size = 0;
    req.on('data', (c) => {
      size += c.length;
      if (size > 32 * 1024 * 1024) { reject(new Error('body too large')); req.destroy(); return; }
      chunks.push(c);
    });
    req.on('end', () => resolve(Buffer.concat(chunks).toString('utf8')));
    req.on('error', reject);
  });
}

const server = createServer(async (req, res) => {
  const n = ++seq;
  const url = new URL(req.url ?? '/', 'http://127.0.0.1');
  const route = url.pathname.replace(/\/+$/, '').replace(/^\/v1(?=\/)/, '');
  try {
    if (req.method === 'GET' && route === '/models') {
      log({ n, method: req.method, path: url.pathname });
      sendJson(res, 200, { object: 'list', data: [{ id: MODEL_ID, object: 'model', created: 0, owned_by: 'mock' }] });
      return;
    }
    if (req.method === 'POST' && route === '/chat/completions') {
      const raw = await readBody(req);
      if (dumpDir) writeFileSync(path.join(dumpDir, `mock-openai-req-${process.pid}-${n}.json`), raw);
      let body;
      try { body = JSON.parse(raw); } catch {
        log({ n, method: req.method, path: url.pathname, error: 'invalid json' });
        sendJson(res, 400, { error: { message: 'invalid json', type: 'invalid_request_error' } });
        return;
      }
      const reply = decide(body);
      log({
        n, method: req.method, path: url.pathname, model: body.model, stream: body.stream === true,
        messages: Array.isArray(body.messages) ? body.messages.map((m) => m?.role).join(',') : null,
        tools: Array.isArray(body.tools) ? body.tools.map((t) => t?.function?.name ?? t?.name) : [],
        reply: reply.kind === 'tool' ? { tool: reply.name, args: reply.args } : { text: reply.text }, reason: reply.reason,
      });
      const id = `mock-${n}`;
      const created = Math.floor(Date.now() / 1000);
      const model = typeof body.model === 'string' ? body.model : MODEL_ID;
      if (body.stream === true) await streamReply(res, id, created, model, reply, body.stream_options?.include_usage === true);
      else plainReply(res, id, created, model, reply);
      return;
    }
    log({ n, method: req.method, path: url.pathname, status: 404 });
    sendJson(res, 404, { error: { message: `not found: ${req.method} ${url.pathname}`, type: 'invalid_request_error' } });
  } catch (err) {
    log({ n, error: String(err?.message ?? err) });
    if (!res.headersSent) sendJson(res, 500, { error: { message: 'mock internal error', type: 'server_error' } });
    else res.end();
  }
});

server.listen(Number(process.env.MOCK_PORT) || 0, '127.0.0.1', () => {
  process.stdout.write(JSON.stringify({ port: server.address().port }) + '\n');
});

for (const sig of ['SIGINT', 'SIGTERM']) process.on(sig, () => server.close(() => process.exit(0)));
