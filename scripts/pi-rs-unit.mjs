import assert from 'node:assert/strict';
import { mkdtempSync, mkdirSync, readFileSync, writeFileSync } from 'node:fs';
import os from 'node:os'; import path from 'node:path';
import { PI_RS_VERSION, piRsPath } from '../sidecar/src/pi-rs.mjs';
import { agentStatus, installAgent } from '../sidecar/src/agents.mjs';

const root = path.resolve(import.meta.dirname, '..');
const cargo = readFileSync(path.join(root, 'pi-rs', 'Cargo.toml'), 'utf8').match(/^version = "([^"]+)"/m)?.[1];
assert.equal(PI_RS_VERSION, cargo, 'PI_RS_VERSION must match pi-rs/Cargo.toml');

const tmp = mkdtempSync(path.join(os.tmpdir(), 'pirs-unit-')); const bin = path.join(tmp, 'pi'); writeFileSync(bin, '#!/bin/sh\n');
assert.equal(piRsPath({}), null);
assert.equal(piRsPath({ CINDY_PI_RS: '/nonexistent/pi' }), null);
assert.equal(piRsPath({ CINDY_PI_RS: tmp }), null, 'a directory is not a binary');
assert.equal(piRsPath({ CINDY_PI_RS: bin }), bin);

// 解析随包二进制：覆盖路径优先；覆盖路径无效时落到随包位置（开发模式 sidecar/dist/pi-rs/）。
process.env.CINDY_PI_RS = bin;
const st = agentStatus('pi');
assert.deepEqual({ kind: st.kind, version: st.version, installed: st.installed, binary: st.binary, downloadBytes: st.downloadBytes },
  { kind: 'pi', version: PI_RS_VERSION, installed: true, binary: bin, downloadBytes: 0 });
assert.deepEqual(await installAgent('pi'), { ...st, downloaded: false });
assert.throws(() => agentStatus('claude'), /unsupported agent/);
delete process.env.CINDY_PI_RS;
const bundled = agentStatus('pi');
assert.ok(/pi(\.exe)?$/.test(bundled.binary), bundled.binary);
if (!bundled.installed) await assert.rejects(installAgent('pi'), /bundled pi-rs is missing/);
console.log('pi-rs-unit ok');
