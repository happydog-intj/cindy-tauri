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
    const detail = ok ? `sidecar ok: ${JSON.stringify(result.info)}` : result.error;
    await invoke('smoke_finish', { ok, detail });
  }
}

document.getElementById('btn').addEventListener('click', async () => {
  const { result } = await callSidecar();
  show(result);
});

main();
