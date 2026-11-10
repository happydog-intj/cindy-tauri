// 回退测试：pi-rs 遇到自己不支持的会话配置时，应 exec 原版 Pi（这里用一个记录参数的假脚本代替），否则自己处理。
import { mkdtempSync, writeFileSync, chmodSync, readFileSync, existsSync } from 'node:fs';
import { spawnSync } from 'node:child_process';
import os from 'node:os'; import path from 'node:path'; import assert from 'node:assert/strict';
const tmp = mkdtempSync(path.join(os.tmpdir(), 'pifb-')); const fake = tmp + '/orig-pi'; const log = tmp + '/log';
writeFileSync(fake, `#!/bin/bash\necho "ORIG $@" > ${log}\necho "ENV $CINDY_PI_MCP_BRIDGE" >> ${log}\n`); chmodSync(fake, 0o755);
const run = (args, env = {}, withFallback = true) => { try { writeFileSync(log, ''); } catch {} return spawnSync(process.env.PI_BIN, args, { env: { PATH: process.env.PATH, HOME: tmp, PI_CODING_AGENT_DIR: tmp, ...(withFallback ? { CINDY_PI_FALLBACK_BINARY: fake } : {}), ...env }, input: '', encoding: 'utf8', timeout: 8000 }); };
const base = ['--mode', 'rpc', '--no-approve', '--no-extensions'];
const bridge = ['--extension', '/x/cindy-bridge.ts'];
const cases = [
  ['bridge only → pi-rs', [...base, ...bridge], {}, false],
  ['plain → pi-rs', base, {}, false],
  ['subagent ext → fallback', [...base, ...bridge, '--extension', '/x/subagent.ts'], {}, true],
  ['plan-mode ext → fallback', [...base, ...bridge, '--extension', '/a/plan-mode/index.ts'], {}, true],
  ['--skill → fallback', [...base, ...bridge, '--skill', '/s'], {}, true],
  ['MCP bridge → fallback', [...base, ...bridge], { CINDY_PI_MCP_BRIDGE: '{"servers":[]}' }, true],
  ['vision bridge → fallback', [...base, ...bridge], { CINDY_PI_VISION_BRIDGE: '{}' }, true],
  ['hosted guest → fallback', [...base, ...bridge], { CINDY_PI_HOSTED: '1' }, true],
  ['native adapters [] → pi-rs', [...base, ...bridge], { CINDY_PI_NATIVE_PROVIDER_ADAPTERS: '[]' }, false],
  ['native adapters set → fallback', [...base, ...bridge], { CINDY_PI_NATIVE_PROVIDER_ADAPTERS: '[{"id":"x"}]' }, true],
];
for (const [name, args, env, expectFb] of cases) {
  const r = run(args, env); const used = readFileSync(log, 'utf8').startsWith('ORIG');
  assert.equal(used, expectFb, `${name}: fallback=${used}`);
  if (used) { assert.ok(readFileSync(log, 'utf8').includes(args.join(' ')), name + ': argv forwarded unchanged'); assert.match(r.stderr, /runtime=original reason=/); }
  console.log('ok  ', name);
  if (!used) assert.match(r.stderr, /\[pi-rs\] session=.* runtime=pi-rs/);
}
// 没配置回退二进制 → 即使需要也由 pi-rs 自己跑（不崩）
const r = run([...base, ...bridge, '--extension', '/x/subagent.ts'], { CINDY_PI_SESSION_ID: 'sid-1' }, false);
assert.match(r.stderr, /session=sid-1 runtime=pi-rs unsupported=extension/);
assert.ok(!readFileSync(log, 'utf8').startsWith('ORIG')); console.log('ok   no fallback configured → pi-rs keeps running');
// 回退二进制不存在 → 同上
const r2 = run([...base, ...bridge, '--skill', '/s'], { CINDY_PI_FALLBACK_BINARY: tmp + '/missing' }, false);
assert.ok(!readFileSync(log, 'utf8').startsWith('ORIG')); console.log('ok   missing fallback binary → pi-rs keeps running');
console.log('fallback-test ok');
