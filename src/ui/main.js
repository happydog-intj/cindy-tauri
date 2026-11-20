const { invoke } = window.__TAURI__.core;
const out = document.getElementById('out');

function show(value) {
  out.textContent = typeof value === 'string' ? value : JSON.stringify(value, null, 2);
}

// 基础检查：Rust 宿主可达 + 随包 pi-rs 就绪。
async function basicChecks() {
  return invoke('runtime_info');
}

// 冒烟阶段 1：agent 的完整往返（发消息、审批、重启续跑）由 smoke.mjs 的会话冒烟与 cargo e2e 覆盖，这里只确认宿主与二进制就绪。
async function smoke() {
  const result = await basicChecks();
  show(result);
  if (!result.agent.installed) throw new Error(`bundled pi-rs missing: ${result.agent.binary}`);
  return `host ok: ${JSON.stringify(result.info)}; pi-rs ${result.agent.version} ready`;
}

async function main() {
  const info = await invoke('smoke_info');
  if (!info) {
    try {
      show(await basicChecks());
    } catch (err) {
      show({ error: String(err) });
    }
    return;
  }
  try {
    const detail = await smoke();
    // 第一阶段通过后加载真实 renderer，由 preload 里的冒烟钩子写最终结果。
    sessionStorage.setItem('cindySmokePhase1', detail);
    location.replace('/index.html');
  } catch (err) {
    await invoke('smoke_finish', { ok: false, detail: String(err) });
  }
}

document.getElementById('btn').addEventListener('click', async () => {
  try {
    show(await basicChecks());
  } catch (err) {
    show({ error: String(err) });
  }
});

main();
