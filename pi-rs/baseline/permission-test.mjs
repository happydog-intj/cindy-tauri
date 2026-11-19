// 权限门线协议冻结（差距清单 #2）：pi-rs 的 ask 门控走扩展 UI 子协议——
//   发 {type:"extension_ui_request", id, method:"input", title:"cindy:permission", placeholder:"<payload JSON>"}
//   宿主回 {type:"extension_ui_response", id, value:"allow"|"user-deny:<...>"|..., cancelled?}
// 这份测试锁定线协议：宿主侧（今天的 maker-core cindy-bridge、未来的 Rust 宿主）必须说同一协议。
// 场景：ask+allow 写入 / user-deny / cancelled / bypass 直通 / 控制面写入绕过 bypass 也要问 / 无权限文件 fail-closed。
// 用法：node baseline/permission-test.mjs（需要先 cargo build --release；mock 服务由本脚本自起）。
import { spawn } from 'node:child_process';
import { mkdtempSync, mkdirSync, existsSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import readline from 'node:readline';

const root = path.resolve(import.meta.dirname, '../..');
const PI = process.env.PI_BIN ?? path.join(root, 'pi-rs', 'target', 'release', 'pi');
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

const steps = [];
function check(name, cond, detail) {
  steps.push({ name, ok: !!cond });
  console.log(`${cond ? 'PASS' : 'FAIL'} ${name}${detail !== undefined ? ` — ${typeof detail === 'string' ? detail : JSON.stringify(detail).slice(0, 240)}` : ''}`);
  if (!cond) throw new Error(`step failed: ${name}`);
}

// 起一个 pi-rs 实例。perm：写到 CINDY_PI_PERMISSION_FILE 的 JSON（缺省不写 = fail-closed）。
async function startPi({ tmp, perm, cwd, agentDirAsCwd }) {
  const agentDir = path.join(tmp, `agent-${Math.random().toString(36).slice(2, 8)}`);
  const sessions = path.join(tmp, `sessions-${Math.random().toString(36).slice(2, 8)}`);
  for (const d of [agentDir, sessions]) mkdirSync(d, { recursive: true });
  // 控制面场景：cwd 必须就是 PI_CODING_AGENT_DIR（运行时目录），写路径才落在控制面内。
  if (agentDirAsCwd) cwd = agentDir;
  const mock = spawn(process.execPath, [path.join(root, 'scripts', 'mock-openai.mjs')], { stdio: ['ignore', 'pipe', 'ignore'], env: process.env });
  const port = await new Promise((resolve) => {
    readline.createInterface({ input: mock.stdout }).once('line', (l) => resolve(JSON.parse(l).port));
  });
  writeFileSync(path.join(agentDir, 'models.json'), JSON.stringify({ providers: { smoke: { baseUrl: `http://127.0.0.1:${port}`, api: 'openai-completions', apiKey: 'sk-none', models: [{ id: 'mock-model', name: 'Mock' }] } } }));
  // 注意：不带 --no-approve —— 那个旗标会整段绕过权限门，本测试测的就是门。
  // 权限门只在 bridge 模式生效（maker-core 经 cindy-bridge 扩展拉起 pi-rs）；
  // pi-rs 不加载扩展内容，只用 --extension 名字判断 bridge 身份，占位路径即可。
  const child = spawn(PI, ['--mode', 'rpc', '--extension', path.join(tmp, 'cindy-bridge.ts'), '--no-skills', '--no-context-files', '--no-prompt-templates', '--session-dir', sessions, '--provider', 'smoke', '--model', 'mock-model'],
    { cwd, env: { ...process.env, PI_CODING_AGENT_DIR: agentDir, PI_OFFLINE: '1', ...(perm ? { CINDY_PI_PERMISSION_FILE: perm } : {}) }, stdio: ['pipe', 'pipe', 'inherit'] });
  const out = [];
  let buf = '';
  child.stdout.on('data', (d) => { buf += d; let i; while ((i = buf.indexOf('\n')) >= 0) { out.push(buf.slice(0, i)); buf = buf.slice(i + 1); } });
  const send = (o) => child.stdin.write(JSON.stringify(o) + '\n');
  const waitFor = async (pred, from = 0, ms = 30_000) => {
    const t = Date.now();
    while (Date.now() - t < ms) {
      const hit = out.slice(from).map((l) => JSON.parse(l)).find(pred);
      if (hit) return hit;
      await sleep(20);
    }
    return null;
  };
  return { mock, child, out, send, waitFor, agentDir };
}

async function shutdown(pi) {
  pi.child.stdin.end();
  await sleep(200);
  pi.child.kill();
  pi.mock.kill();
}

async function scenario({ name, perm, actions, cwdOverride }) {
  const tmp = mkdtempSync(path.join(os.tmpdir(), 'cindy-perm-'));
  const cwd = cwdOverride ?? path.join(tmp, 'work');
  mkdirSync(cwd, { recursive: true });
  let permFile;
  if (perm) {
    permFile = path.join(tmp, 'permission.json');
    writeFileSync(permFile, JSON.stringify(perm));
  }
  const pi = await startPi({ tmp, perm: permFile, cwd });
  try {
    await actions({ pi, cwd, tmp });
  } finally {
    await shutdown(pi);
  }
  rmSync(tmp, { recursive: true, force: true });
}

// 等 cindy:permission 请求；途中出现的 cindy:turn-change-capture（写前快照同步点）
// 必须先应答，否则门控会一直挂在那里 —— 这也是线协议的一部分（宿主要答所有 UI 请求）。
async function waitPermissionRequest(pi, from, timeoutMs = 30_000) {
  const t = Date.now();
  let cursor = from;
  while (Date.now() - t < timeoutMs) {
    const lines = pi.out.slice(cursor);
    for (let i = 0; i < lines.length; i++) {
      const r = JSON.parse(lines[i]);
      if (r.type !== 'extension_ui_request') continue;
      if (r.title === 'cindy:permission') return r;
      // 控制面快照 confirm（cindy:turn-change-capture）：先应答放行（宿主行为），否则门控挂起。
      if (r.title === 'cindy:turn-change-capture') {
        pi.send({ type: 'extension_ui_response', id: r.id, value: 'ok' });
      }
    }
    cursor = pi.out.length;
    if (process.env.PERM_DEBUG) console.error('  [perm-wait] out:', pi.out.slice(-3).map((l) => l.slice(0, 120)).join(' | '));
    await sleep(20);
  }
  return null;
}

const main = async () => {
  // ── 1. ask + allow：门控生效（先问后写），allow 后文件落地 ──
  await scenario({
    name: 'ask-allow',
    perm: { mode: 'ask' },
    actions: async ({ pi, cwd }) => {
      await pi.waitFor((r) => r.type === 'response');
      const k = pi.out.length;
      pi.send({ id: 'p1', type: 'prompt', message: 'MOCK_WRITE_FILE please' });
      const req = await waitPermissionRequest(pi, k);
      check('ask: permission request arrives', !!req, req?.id);
      check('ask: request carries uuid id', typeof req?.id === 'string' && req.id.length >= 32, req?.id);
      const payload = JSON.parse(req.placeholder);
      check('ask: payload has toolName/input', payload.toolName === 'write' && payload.input?.path !== undefined, payload);
      check('ask: payload has resolvedWritePath', typeof payload.resolvedWritePath === 'string' && payload.resolvedWritePath.includes('work'), payload.resolvedWritePath);
      check('ask: writable roots include cwd', Array.isArray(payload.resolvedWritableRoots) && payload.resolvedWritableRoots.some((r) => r.includes('work')), payload.resolvedWritableRoots);
      check('ask: gate holds before approval (no file)', !existsSync(path.join(cwd, 'mock-ok.txt')));
      // 门控在工具执行内部：start 事件先发，gate 阻塞在 run 之前 —— 执行**结束**事件不会出现。
      check('ask: no tool execution end before approval', !pi.out.slice(k).some((l) => JSON.parse(l).type === 'tool_execution_end'));
      pi.send({ type: 'extension_ui_response', id: req.id, value: 'allow' });
      const settled = await pi.waitFor((r) => r.type === 'agent_settled', k);
      check('ask: turn settles after allow', !!settled, settled?.status ?? settled?.result);
      check('ask: file written after allow', existsSync(path.join(cwd, 'mock-ok.txt')) && readFileSync(path.join(cwd, 'mock-ok.txt'), 'utf8').trim() === 'ok');
      rmSync(path.join(cwd, 'mock-ok.txt')); // 给 deny 场景一个干净的“文件不存在”断言。

      // ── 2. user-deny：value 前缀映射拒绝语，文件不被写 ──
      const k2 = pi.out.length;
      pi.send({ id: 'p2', type: 'prompt', message: 'MOCK_WRITE_FILE please' });
      const req2 = await waitPermissionRequest(pi, k2);
      check('deny: second request arrives', !!req2, req2?.id);
      pi.send({ type: 'extension_ui_response', id: req2.id, value: 'user-deny:testing deny path' });
      const settled2 = await pi.waitFor((r) => r.type === 'agent_settled', k2);
      check('deny: turn settles after deny', !!settled2);
      // 拒绝语进的是工具结果（isError），不是 assistant 的 errorMessage。
      const deniedTool = pi.out.slice(k2).map((l) => JSON.parse(l)).find((r) => r.type === 'tool_execution_end' && r.isError === true);
      check('deny: tool result flagged error', !!deniedTool, deniedTool?.result?.content?.[0]?.text?.slice(0, 120));
      check('deny: denial label surfaces', (deniedTool?.result?.content?.[0]?.text ?? '').includes('User denied this tool call via Cindy'), deniedTool?.result?.content?.[0]?.text?.slice(0, 120));
      check('deny: file untouched', !existsSync(path.join(cwd, 'mock-ok.txt')));

      // ── 3. cancelled：宿主关闭弹窗 = 拒绝 ──
      const k3 = pi.out.length;
      pi.send({ id: 'p3', type: 'prompt', message: 'MOCK_WRITE_FILE please' });
      const req3 = await waitPermissionRequest(pi, k3);
      check('cancelled: request arrives', !!req3);
      pi.send({ type: 'extension_ui_response', id: req3.id, cancelled: true });
      const settled3 = await pi.waitFor((r) => r.type === 'agent_settled', k3);
      check('cancelled: turn settles', !!settled3);
      const cancelledTool = pi.out.slice(k3).map((l) => JSON.parse(l)).find((r) => r.type === 'tool_execution_end' && r.isError === true);
      check('cancelled: fallback denial label', (cancelledTool?.result?.content?.[0]?.text ?? '').includes('Cindy could not approve this tool call'), cancelledTool?.result?.content?.[0]?.text?.slice(0, 120));
    },
  });

  // ── 4. bypass：不问直写 ──
  await scenario({
    name: 'bypass',
    perm: { mode: 'bypassPermissions' },
    actions: async ({ pi, cwd }) => {
      await pi.waitFor((r) => r.type === 'response');
      const k = pi.out.length;
      pi.send({ id: 'p1', type: 'prompt', message: 'MOCK_WRITE_FILE please' });
      // bypass 下仍要应答 turn-change-capture（快照同步点对任何模式都发）；其它 UI 请求不该出现。
      const deadline = Date.now() + 30_000;
      let settled = null;
      let cursor = k;
      while (Date.now() < deadline) {
        for (const l of pi.out.slice(cursor)) {
          const j = JSON.parse(l);
          if (j.type === 'extension_ui_request' && j.title === 'cindy:turn-change-capture') {
            pi.send({ type: 'extension_ui_response', id: j.id, value: 'ok' });
          }
        }
        cursor = pi.out.length;
        const done = pi.out.slice(k).map((l) => JSON.parse(l)).find((r) => r.type === 'agent_settled');
        if (done) { settled = done; break; }
        await sleep(30);
      }
      // bypass 下不该出现 cindy:permission 请求；turn-change-capture（快照同步点）任何模式都发，不算。
      check('bypass: no permission request', !pi.out.slice(k).some((l) => { const j = JSON.parse(l); return j.type === 'extension_ui_request' && j.title === 'cindy:permission'; }));
      check('bypass: file written directly', !!settled && existsSync(path.join(cwd, 'mock-ok.txt')), `settled=${JSON.stringify(settled)?.slice(0, 120)} exists=${existsSync(path.join(cwd, 'mock-ok.txt'))}`);
    },
  });

  // ── 5. 控制面：bypass 下写运行时目录（agentDir）仍要问。mock 固定写相对路径 mock-ok.txt，
  //      所以让 cwd = agentDir，写路径即落在运行时目录内（control_plane 判据）。──
  {
    const tmp = mkdtempSync(path.join(os.tmpdir(), 'cindy-perm-'));
    const agentDir = path.join(tmp, 'agent');
    mkdirSync(agentDir, { recursive: true });
    const permFile = path.join(tmp, 'permission.json');
    writeFileSync(permFile, JSON.stringify({ mode: 'bypassPermissions' }));
    const pi = await startPi({ tmp, perm: permFile, agentDirAsCwd: true });
    try {
      await pi.waitFor((r) => r.type === 'response');
      const k = pi.out.length;
      pi.send({ id: 'p1', type: 'prompt', message: 'MOCK_WRITE_FILE please' });
      const req = await waitPermissionRequest(pi, k, 20_000);
      check('control-plane: asked even under bypass', !!req, req?.placeholder?.slice(0, 160));
      if (req) {
        const payload = JSON.parse(req.placeholder);
        check('control-plane: payload flags controlPlaneWrite', payload.controlPlaneWrite === true, payload.controlPlaneWrite);
        check('control-plane: resolved path inside runtime dir', typeof payload.resolvedWritePath === 'string' && payload.resolvedWritePath.includes('agent'), payload.resolvedWritePath);
      }
    } finally {
      await shutdown(pi);
    }
    rmSync(tmp, { recursive: true, force: true });
  }

  // ── 6. 无权限文件：fail-closed 按 ask ──
  await scenario({
    name: 'fail-closed',
    actions: async ({ pi, cwd }) => {
      await pi.waitFor((r) => r.type === 'response');
      const k = pi.out.length;
      pi.send({ id: 'p1', type: 'prompt', message: 'MOCK_WRITE_FILE please' });
      const req = await waitPermissionRequest(pi, k, 20_000);
      check('fail-closed: missing permission file still asks', !!req);
      if (req) pi.send({ type: 'extension_ui_response', id: req.id, value: 'allow' });
      const settled = await pi.waitFor((r) => r.type === 'agent_settled', k);
      check('fail-closed: allow still works', !!settled && existsSync(path.join(cwd, 'mock-ok.txt')));
    },
  });
};

let failed = false;
try {
  await main();
} catch (err) {
  failed = true;
  console.error(err.message);
}
const passed = steps.filter((s) => s.ok).length;
console.log(`\npermission wire: ${passed}/${steps.length} passed${failed ? ' (FAILED)' : ''}`);
process.exit(failed ? 1 : 0);
