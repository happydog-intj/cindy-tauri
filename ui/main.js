const { invoke } = window.__TAURI__.core;
const out = document.getElementById('out');

function show(value) {
  out.textContent = typeof value === 'string' ? value : JSON.stringify(value, null, 2);
}

const call = (method, params = {}) => invoke('sidecar_call', { method, params });

// 基础检查：sidecar 通信 + cindy-headless 可用。
async function basicChecks() {
  const result = {};
  result.ping = await call('ping');
  result.info = await call('info');
  result.echo = await call('echo', { hello: 'cindy' });
  result.headless = await call('headless.version');
  const caps = await call('headless.capabilities', { profile: 'cindy-production-pi/profile.example.json' });
  result.headlessCapabilities = { agentBackend: caps.agentBackend, profileId: caps.profileId };
  return result;
}

// 冒烟任务：让 agent 在临时工作目录里写一个文件，并回复固定文本。
const SMOKE_TASK = 'Create a file named hello.txt containing exactly the text: hello from cindy-tauri. Then reply with the single word: done';

async function smoke(info) {
  const result = await basicChecks();
  show(result);
  const parts = [
    `sidecar ok: ${JSON.stringify(result.info)}`,
    `headless ${result.headless.version} (${result.headlessCapabilities.agentBackend})`,
  ];

  const install = await call('agent.install', { kind: 'pi' });
  result.agent = install;
  show(result);
  parts.push(`pi ${install.version} ${install.downloaded ? 'downloaded' : 'cached'}`);

  if (info.modelBaseUrl && info.modelId) {
    const run = await call('headless.run', {
      task: SMOKE_TASK,
      model: { baseUrl: info.modelBaseUrl, id: info.modelId },
      timeoutMs: 240000,
    });
    result.run = run;
    show(result);
    if (run.status !== 'valid-completed') throw new Error(`run ${run.status}: ${run.error}`);
    parts.push(`run ${run.status} in ${run.durationMs}ms, reply=${JSON.stringify(run.finalText)}`);
  } else {
    parts.push('run skipped (no model configured)');
  }
  return parts.join('; ');
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
    const detail = await smoke(info);
    await invoke('smoke_finish', { ok: true, detail });
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
