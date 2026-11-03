const { invoke } = window.__TAURI__.core;
const out = document.getElementById('out');

function show(value) {
  out.textContent = typeof value === 'string' ? value : JSON.stringify(value, null, 2);
}

async function callSidecar() {
  const result = {};
  try {
    result.ping = await invoke('sidecar_call', { method: 'ping', params: {} });
    result.info = await invoke('sidecar_call', { method: 'info', params: {} });
    result.echo = await invoke('sidecar_call', { method: 'echo', params: { hello: 'cindy' } });
    result.headless = await invoke('sidecar_call', { method: 'headless.version', params: {} });
    const caps = await invoke('sidecar_call', {
      method: 'headless.capabilities',
      params: { profile: 'cindy-production-pi/profile.example.json' },
    });
    result.headlessCapabilities = { agentBackend: caps.agentBackend, profileId: caps.profileId };
    return { ok: true, result };
  } catch (err) {
    return { ok: false, result: { error: String(err) } };
  }
}

async function main() {
  const { ok, result } = await callSidecar();
  show(result);

  const smokeOut = await invoke('smoke_info');
  if (smokeOut) {
    const detail = ok
      ? `sidecar ok: ${JSON.stringify(result.info)}; headless ${result.headless.version} (${result.headlessCapabilities.agentBackend})`
      : result.error;
    await invoke('smoke_finish', { ok, detail });
  }
}

document.getElementById('btn').addEventListener('click', async () => {
  const { result } = await callSidecar();
  show(result);
});

main();
