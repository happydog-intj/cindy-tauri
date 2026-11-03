// Agent 按需下载：版本、URL、sha256 来自 cindy 仓库 tools/<kind>/latest.json（随包复制到 headless/agents/）。
// 下载后安装到 <dataDir>/agents/<kind>/<version>/<platform>/。
import { createHash } from 'node:crypto';
import { createWriteStream, existsSync, readFileSync } from 'node:fs';
import { chmod, mkdir, readdir, rename, rm, stat, writeFile } from 'node:fs/promises';
import { spawn } from 'node:child_process';
import os from 'node:os';
import path from 'node:path';
import { Readable } from 'node:stream';
import { pipeline } from 'node:stream/promises';

const AGENTS = {
  pi: {
    manifest: 'pi.json',
    binFile: process.platform === 'win32' ? 'pi.exe' : 'pi',
  },
};

function platformKey() {
  return `${process.platform}-${process.arch}`;
}

function agentSpec(kind, manifestDir) {
  const spec = AGENTS[kind];
  if (!spec) throw new Error(`unsupported agent: ${kind}`);
  const pinned = JSON.parse(readFileSync(path.join(manifestDir, spec.manifest), 'utf8'));
  const asset = pinned.runtimeAssets?.[platformKey()];
  if (!asset?.url || !/^[0-9a-f]{64}$/.test(asset.sha256 ?? '')) {
    throw new Error(`no pinned ${kind} asset for ${platformKey()}`);
  }
  return { kind, version: pinned.version, asset, binFile: spec.binFile };
}

function installDir(spec, dataDir) {
  return path.join(dataDir, 'agents', spec.kind, spec.version, platformKey());
}

export function agentStatus(kind, { manifestDir, dataDir }) {
  const spec = agentSpec(kind, manifestDir);
  const dir = installDir(spec, dataDir);
  const binary = path.join(dir, spec.binFile);
  const installed = existsSync(path.join(dir, '.installed')) && existsSync(binary);
  return { kind, version: spec.version, platform: platformKey(), installed, binary, downloadBytes: spec.asset.size };
}

function tarCommand() {
  if (process.platform !== 'win32') return 'tar';
  const systemTar = path.join(process.env.SystemRoot ?? 'C:\\Windows', 'System32', 'tar.exe');
  return existsSync(systemTar) ? systemTar : 'tar';
}

function run(cmd, args, cwd) {
  return new Promise((resolve, reject) => {
    const child = spawn(cmd, args, { cwd, stdio: ['ignore', 'ignore', 'pipe'], windowsHide: true });
    let stderr = '';
    child.stderr.on('data', (d) => { stderr += d; });
    child.once('error', reject);
    child.once('close', (code) => (code === 0 ? resolve() : reject(new Error(`${cmd} exited ${code}: ${stderr.slice(0, 500)}`))));
  });
}

// 下载到临时文件并边写边算 sha256；不匹配就删除并报错。
async function download(url, dest, expectedSha256) {
  const res = await fetch(url, { redirect: 'follow' });
  if (!res.ok || !res.body) throw new Error(`download failed: HTTP ${res.status} ${url}`);
  const hash = createHash('sha256');
  const body = Readable.fromWeb(res.body);
  body.on('data', (chunk) => hash.update(chunk));
  await pipeline(body, createWriteStream(dest));
  const actual = hash.digest('hex');
  if (actual !== expectedSha256) {
    await rm(dest, { force: true });
    throw new Error(`sha256 mismatch for ${url}: expected ${expectedSha256}, got ${actual}`);
  }
}

// macOS/Linux 的 tar.gz 包在 pi/ 目录下；Windows zip 直接平铺。统一成平铺布局。
async function flatten(dir, binFile) {
  if (existsSync(path.join(dir, binFile)) && (await stat(path.join(dir, binFile))).isFile()) return;
  const inner = path.join(dir, 'pi');
  if (!existsSync(inner)) throw new Error(`unexpected archive layout: ${(await readdir(dir)).join(', ')}`);
  const tmp = path.join(dir, '.extract-tmp');
  await rename(inner, tmp);
  for (const name of await readdir(tmp)) await rename(path.join(tmp, name), path.join(dir, name));
  await rm(tmp, { recursive: true, force: true });
}

const inflight = new Map();

export function installAgent(kind, ctx) {
  // 同一 agent 并发安装只跑一次。
  if (!inflight.has(kind)) {
    const task = doInstall(kind, ctx).finally(() => inflight.delete(kind));
    inflight.set(kind, task);
  }
  return inflight.get(kind);
}

async function doInstall(kind, { manifestDir, dataDir }) {
  const status = agentStatus(kind, { manifestDir, dataDir });
  if (status.installed) return { ...status, downloaded: false };
  const spec = agentSpec(kind, manifestDir);
  const dir = installDir(spec, dataDir);
  const staging = `${dir}.staging-${process.pid}`;
  const isZip = spec.asset.url.endsWith('.zip');
  const archive = path.join(os.tmpdir(), `cindy-${kind}-${spec.version}-${Date.now()}${isZip ? '.zip' : '.tar.gz'}`);
  await rm(staging, { recursive: true, force: true });
  await mkdir(staging, { recursive: true });
  try {
    await download(spec.asset.url, archive, spec.asset.sha256);
    // macOS 和 Win10 1803+ 自带 bsdtar，可解 tar.gz 和 zip。Windows 上显式用 System32 的 tar，
    // 避免 PATH 里 Git 自带的 GNU tar（不支持 zip）抢先。
    await run(tarCommand(), [isZip ? '-xf' : '-xzf', archive], staging);
    await flatten(staging, spec.binFile);
    if (process.platform !== 'win32') await chmod(path.join(staging, spec.binFile), 0o755);
    await rm(dir, { recursive: true, force: true });
    await mkdir(path.dirname(dir), { recursive: true });
    await rename(staging, dir);
    await writeFile(path.join(dir, '.installed'), `${spec.asset.sha256}\n`);
  } finally {
    await rm(archive, { force: true });
    await rm(staging, { recursive: true, force: true });
  }
  return { ...agentStatus(kind, { manifestDir, dataDir }), downloaded: true };
}
