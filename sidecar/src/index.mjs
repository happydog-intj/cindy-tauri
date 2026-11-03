// Sidecar 入口：通过 stdin/stdout 按行收发 JSON-RPC 风格的消息。
// 请求：{"id": 1, "method": "ping", "params": {}}
// 响应：{"id": 1, "result": {...}} 或 {"id": 1, "error": "..."}
import readline from 'node:readline';

const handlers = {
  ping: () => ({ pong: true, at: new Date().toISOString() }),
  info: () => ({ node: process.version, platform: process.platform, arch: process.arch, pid: process.pid }),
  echo: (params) => params,
};

const rl = readline.createInterface({ input: process.stdin });

rl.on('line', (line) => {
  let req;
  try {
    req = JSON.parse(line);
  } catch {
    process.stdout.write(JSON.stringify({ id: null, error: 'invalid json' }) + '\n');
    return;
  }
  const handler = handlers[req.method];
  if (!handler) {
    process.stdout.write(JSON.stringify({ id: req.id, error: `unknown method: ${req.method}` }) + '\n');
    return;
  }
  try {
    const result = handler(req.params ?? {});
    process.stdout.write(JSON.stringify({ id: req.id, result }) + '\n');
  } catch (err) {
    process.stdout.write(JSON.stringify({ id: req.id, error: String(err) }) + '\n');
  }
});

rl.on('close', () => process.exit(0));
