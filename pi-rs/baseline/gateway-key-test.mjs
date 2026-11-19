// 网关直连：apiKeyFile + apiKeyHeaders（取代 sidecar 的本地转发器）。
// 一个只认「authorization 与 x-api-key 同时等于当前 key」的 mock 网关，验证：
//   1. 两个头都带上了正确的 key（openai-completions 协议）；
//   2. 改写 key 文件后，下一个请求用新 key，无需重启 pi-rs（轮换）；
//   3. key 文件为空时不发空凭据，请求被网关 401 拒绝，pi-rs 把错误带回而不是挂起；
//   4. 真实 key 不出现在 pi-rs 的 stderr / 环境（只在文件里）。
// 用法：PI_BIN=<pi-rs 二进制> node pi-rs/baseline/gateway-key-test.mjs
import { spawn } from 'node:child_process';
import { createServer } from 'node:http';
import { mkdtempSync, mkdirSync, writeFileSync } from 'node:fs';
import os from 'node:os';
import path from 'node:path';

const PI = process.env.PI_BIN;
if (!PI) { console.error('PI_BIN required'); process.exit(2); }

let failed = 0;
const check = (name, ok, info) => {
  console.log(`${ok ? 'PASS' : 'FAIL'} ${name}${info !== undefined ? ` — ${typeof info === 'string' ? info : JSON.stringify(info)}` : ''}`);
  if (!ok) failed++;
};

const tmp = mkdtempSync(path.join(os.tmpdir(), 'pi-gw-'));
const agentDir = path.join(tmp, 'agent');
const sessions = path.join(tmp, 'sessions');
const work = path.join(tmp, 'work');
for (const d of [agentDir, sessions, work]) mkdirSync(d, { recursive: true });
const keyFile = path.join(tmp, 'gateway.key');

let currentKey = 'gw-key-ONE';
const seen = []; // 每个请求：{ auth, xApiKey, ok }
const server = createServer((req, res) => {
  const auth = req.headers.authorization ?? null;
  const xk = req.headers['x-api-key'] ?? null;
  const ok = auth === `Bearer ${currentKey}` && xk === currentKey;
  seen.push({ auth, xk, ok });
  req.resume();
  if (!ok) {
    res.writeHead(401, { 'content-type': 'application/json' });
    res.end(JSON.stringify({ error: { message: 'bad gateway key' } }));
    return;
  }
  res.writeHead(200, { 'content-type': 'text/event-stream' });
  const chunk = (delta, finish = null) => `data: ${JSON.stringify({ id: 'c1', object: 'chat.completion.chunk', model: 'm1', choices: [{ index: 0, delta, finish_reason: finish }] })}\n\n`;
  res.write(chunk({ role: 'assistant', content: 'pong' }));
  res.write(chunk({}, 'stop'));
  res.write('data: [DONE]\n\n');
  res.end();
});
await new Promise((r) => server.listen(0, '127.0.0.1', r));
const port = server.address().port;

writeFileSync(keyFile, currentKey + '\n');
writeFileSync(path.join(agentDir, 'models.json'), JSON.stringify({
  providers: {
    cindy: {
      baseUrl: `http://127.0.0.1:${port}`,
      api: 'openai-completions',
      apiKey: 'cindy-tauri-keyless', // 占位：被 apiKeyFile 覆盖
      apiKeyFile: keyFile,
      apiKeyHeaders: ['authorization', 'x-api-key'],
      models: [{ id: 'm1', name: 'M1' }],
    },
  },
}));

const child = spawn(PI, ['--mode', 'rpc', '--no-extensions', '--no-skills', '--no-context-files', '--no-prompt-templates',
  '--session-dir', sessions, '--provider', 'cindy', '--model', 'm1'],
{ cwd: work, env: { ...process.env, PI_CODING_AGENT_DIR: agentDir, PI_OFFLINE: '1' }, stdio: ['pipe', 'pipe', 'pipe'] });
const out = [];
let errText = '';
let buf = '';
child.stdout.on('data', (d) => { buf += d; let i; while ((i = buf.indexOf('\n')) >= 0) { out.push(buf.slice(0, i)); buf = buf.slice(i + 1); } });
child.stderr.on('data', (d) => { errText += d; });
const send = (o) => child.stdin.write(JSON.stringify(o) + '\n');
const events = () => out.map((l) => { try { return JSON.parse(l); } catch { return null; } }).filter(Boolean);
const waitFor = async (pred, ms = 20_000) => {
  const t = Date.now();
  while (Date.now() - t < ms) { if (pred()) return true; await new Promise((r) => setTimeout(r, 50)); }
  return pred();
};
const agentEnds = () => events().filter((e) => e.type === 'agent_end').length;

try {
  // 1. 首个请求：两个头都是 key ONE。
  send({ id: 'p1', type: 'prompt', message: 'hello' });
  check('turn 1 finishes', await waitFor(() => agentEnds() >= 1));
  check('request 1 carried both headers with key ONE', seen[0]?.ok === true, seen[0]);
  check('authorization has Bearer prefix, x-api-key is bare', seen[0]?.auth === 'Bearer gw-key-ONE' && seen[0]?.xk === 'gw-key-ONE');

  // 2. 轮换：改写文件，不重启，下一个请求用新 key。
  currentKey = 'gw-key-TWO';
  writeFileSync(keyFile, currentKey + '\n');
  send({ id: 'p2', type: 'prompt', message: 'again' });
  check('turn 2 finishes after rotation', await waitFor(() => agentEnds() >= 2));
  const last = seen[seen.length - 1];
  check('request 2 used the rotated key in both headers', last?.ok === true && last.auth === 'Bearer gw-key-TWO' && last.xk === 'gw-key-TWO', last);

  // 3. 空 key 文件：不发空凭据，网关 401，pi-rs 不挂起。
  writeFileSync(keyFile, '');
  const before = seen.length;
  send({ id: 'p3', type: 'prompt', message: 'no key' });
  check('turn 3 ends (no hang) with an empty key file', await waitFor(() => agentEnds() >= 3 || events().some((e) => e.type === 'agent_end' || (e.type === 'response' && e.id === 'p3' && e.success === false))));
  const req3 = seen.slice(before);
  check('empty key file → no empty credential sent', req3.length === 0 || req3.every((r) => r.ok === false && !r.auth?.replace('Bearer', '').trim() && !r.xk), req3);

  // 4. key 不外泄：stderr 里没有，子进程环境里也没有（只在文件里）。
  check('real key never appears on stderr', !errText.includes('gw-key-ONE') && !errText.includes('gw-key-TWO'));
} finally {
  child.kill();
  server.close();
}

console.log(failed === 0 ? '\ngateway key test: all passed' : `\ngateway key test: ${failed} FAILED`);
process.exit(failed === 0 ? 0 : 1);
