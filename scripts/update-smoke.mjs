// 打包后的自动更新冒烟：旧版本经本机 latest.json 检查 → 下载 → 校验签名 → 安装 → 以新版本启动。
// 用法：
//   macOS：node scripts/update-smoke.mjs --old "<旧版 Cindy Tauri.app>" --new "<新版 Cindy Tauri.app.tar.gz>" --version 0.0.2
//   Windows：node scripts/update-smoke.mjs --old "<旧版 *-setup.exe>" --new "<新版 *-setup.exe>" --version 0.0.2
// --tamper 1：改掉新包里的一个字节再提供下载，期望签名校验拒绝、旧版本保持不动（只支持 macOS）。
// 新包旁边必须有 tauri build 生成的 .sig（同一把更新签名私钥，和 tauri.conf.json 的 pubkey 配对）。
// macOS 会把旧 .app 复制到临时目录再运行；Windows 会把旧版本静默安装到当前用户（%LOCALAPPDATA%\Cindy Tauri）。
import { execFileSync, spawn, spawnSync } from 'node:child_process';
import { cpSync, createReadStream, existsSync, mkdirSync, mkdtempSync, readFileSync, realpathSync, rmSync, statSync, writeFileSync } from 'node:fs';
import http from 'node:http';
import os from 'node:os';
import path from 'node:path';
import { buildLatestJson } from './latest-json.mjs';

const args = process.argv.slice(2);
const opt = {};
for (let i = 0; i < args.length; i += 2) opt[args[i].replace(/^--/, '')] = args[i + 1];
if (!opt.old || !opt.new || !opt.version) {
  console.error('usage: update-smoke.mjs --old <old app|setup.exe> --new <new .app.tar.gz|setup.exe> --version <new version>');
  process.exit(2);
}
const isWin = process.platform === 'win32';
const arch = process.arch === 'arm64' ? 'aarch64' : 'x86_64';
// macOS 的 /var → /private/var 是符号链接，Tauri 拒绝路径里带符号链接的可执行文件（取不到 resource_dir），先解析成真实路径。
const work = realpathSync(mkdtempSync(path.join(os.tmpdir(), 'cindy-update-smoke-')));
const dataDir = path.join(work, 'data');
mkdirSync(dataDir, { recursive: true });
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

// 本机更新服务器：/latest.json 和新包本身，记录请求方便排错。
const newFile = path.resolve(opt.new);
const tamper = opt.tamper === '1';
// 篡改：中间位置翻转一个字节（签名仍是原包的）。
const tampered = tamper ? (() => { const b = readFileSync(newFile); b[b.length >> 1] ^= 0xff; return b; })() : null;
const requests = [];
const server = http.createServer((req, res) => {
  requests.push(req.url);
  const url = new URL(req.url, 'http://127.0.0.1');
  if (url.pathname === '/latest.json') {
    res.writeHead(200, { 'Content-Type': 'application/json' });
    res.end(latestJson);
    return;
  }
  if (decodeURIComponent(url.pathname) === `/${path.basename(newFile)}`) {
    if (tampered) {
      res.writeHead(200, { 'Content-Type': 'application/octet-stream', 'Content-Length': tampered.length });
      res.end(tampered);
      return;
    }
    res.writeHead(200, { 'Content-Type': 'application/octet-stream', 'Content-Length': statSync(newFile).size });
    createReadStream(newFile).pipe(res);
    return;
  }
  res.writeHead(404);
  res.end();
});
await new Promise((r) => server.listen(0, '127.0.0.1', r));
const base = `http://127.0.0.1:${server.address().port}`;
const os_ = isWin ? 'windows' : 'darwin';
const installer = isWin ? 'nsis' : 'app';
// 带安装类型和不带的两个键都给：直接运行未打包的 exe 时插件按 {os}-{arch} 查找。
const latestJson = JSON.stringify(buildLatestJson({
  version: opt.version,
  notes: `update smoke ${opt.version}`,
  baseUrl: base,
  targets: { [`${os_}-${arch}-${installer}`]: newFile, [`${os_}-${arch}`]: newFile },
}));
console.log(`update smoke: serving ${path.basename(newFile)} as ${opt.version} on ${base}`);

function smokeEnv(out) {
  return {
    ...process.env,
    CINDY_TAURI_SMOKE: '1',
    CINDY_TAURI_SMOKE_OUT: out,
    CINDY_TAURI_DATA_DIR: dataDir,
    CINDY_TAURI_SMOKE_UPDATE_URL: `${base}/latest.json`,
    CINDY_TAURI_SMOKE_UPDATE_EXPECT: opt.version,
    // 新旧版本签名不同，读钥匙串会弹授权框，冒烟跳过。
    CINDY_TAURI_SMOKE_NO_KEYCHAIN: '1',
  };
}

async function waitForFile(file, ms, alsoDone = () => false) {
  const end = Date.now() + ms;
  while (Date.now() < end) {
    if (existsSync(file)) {
      for (let i = 0; i < 5; i++) {
        try { return JSON.parse(readFileSync(file, 'utf8')); } catch { await sleep(300); }
      }
      return { ok: false, detail: 'result file is not valid JSON' };
    }
    if (alsoDone()) return null;
    await sleep(500);
  }
  return undefined;
}

function report(result, label) {
  let detail = {};
  try { detail = JSON.parse(result.detail); } catch { /* 非 JSON */ }
  console.log(`${label} result: ok=${result.ok} updateSmoke=${JSON.stringify(detail.updateSmoke ?? null)}`);
  if (!result.ok) console.log(`${label} detail: ${String(result.detail).slice(0, 2000)}`);
  return detail.updateSmoke ?? null;
}

let ok = false;
try {
  if (!isWin) {
    // ── macOS：旧 .app 复制到临时目录（和系统临时目录同盘，插件可以原地替换）──
    const appDir = path.join(work, 'apps');
    mkdirSync(appDir);
    const app = path.join(appDir, path.basename(opt.old));
    cpSync(path.resolve(opt.old), app, { recursive: true, verbatimSymlinks: true });
    const exe = path.join(app, 'Contents', 'MacOS', 'cindy-tauri');
    const plistVersion = () => execFileSync('/usr/bin/defaults', ['read', path.join(app, 'Contents', 'Info.plist'), 'CFBundleShortVersionString']).toString().trim();
    const oldVersion = plistVersion();
    console.log(`update smoke: old app ${oldVersion} at ${app}`);
    const out = path.join(work, 'result.json');
    // 旧版本安装成功后自己拉起新版本（同一路径、继承环境），结果由新版本写出。
    const child = spawn(exe, [], { env: smokeEnv(out), stdio: 'inherit' });
    child.once('exit', (code) => console.log(`update smoke: old process exited (${code})`));
    const result = await waitForFile(out, 600_000);
    if (!result) throw new Error('no result file in 600s');
    const smoke = report(result, tamper ? 'old app' : 'updated app');
    const newVersion = plistVersion();
    console.log(`update smoke: bundle version ${oldVersion} → ${newVersion}`);
    if (tamper) {
      // 期望：下载阶段签名校验失败，check 返回 download_failed，.app 没被替换。
      const failedStep = smoke?.steps?.find((s) => !s.ok);
      ok = !result.ok && smoke?.appVersion === oldVersion && newVersion === oldVersion
        && /download_failed/.test(failedStep?.error ?? '');
      console.log(`update smoke: tampered package rejected=${ok} (${failedStep?.error ?? 'no failed step'})`);
    } else {
      ok = result.ok && smoke?.appVersion === opt.version && newVersion === opt.version && oldVersion !== opt.version;
    }
    await sleep(3000);
    spawnSync('/usr/bin/pkill', ['-f', exe]);
  } else {
    // ── Windows：旧版本静默安装到当前用户，再运行已安装的 exe ──
    const r = spawnSync(path.resolve(opt.old), ['/S'], { stdio: 'inherit' });
    if (r.status !== 0) throw new Error(`old installer exited ${r.status}`);
    // NSIS 安装程序（currentUser）把安装位置和主程序名写在卸载键里。
    const key = 'HKCU\\Software\\Microsoft\\Windows\\CurrentVersion\\Uninstall\\Cindy Tauri';
    const regValue = (name) => {
      const q = spawnSync('reg', ['query', key, '/v', name], { encoding: 'utf8' });
      return q.stdout?.match(new RegExp(`${name}\\s+REG_SZ\\s+(.+)`))?.[1]?.trim().replace(/^"|"$/g, '') ?? null;
    };
    const displayVersion = () => regValue('DisplayVersion');
    const installDir = regValue('InstallLocation') ?? path.join(process.env.LOCALAPPDATA, 'Cindy Tauri');
    // MainBinaryName 已带 .exe（例如 cindy-tauri.exe）。
    const bin = regValue('MainBinaryName') ?? 'cindy-tauri.exe';
    const exe = path.join(installDir, /\.exe$/i.test(bin) ? bin : `${bin}.exe`);
    if (!existsSync(exe)) throw new Error(`installed exe not found: ${exe}`);
    const oldVersion = displayVersion();
    console.log(`update smoke: installed old version ${oldVersion} at ${exe}`);
    const out1 = path.join(work, 'result-old.json');
    const child = spawn(exe, [], { env: smokeEnv(out1), stdio: 'inherit' });
    child.once('exit', (code) => console.log(`update smoke: old process exited (${code})`));
    // 旧版本成功时不写结果：交给 NSIS（/P /UPDATE）后退出；失败时会写 ok=false。
    const failed = await waitForFile(out1, 600_000, () => displayVersion() === opt.version);
    if (failed) {
      report(failed, 'old app');
      throw new Error('old app reported failure before installing');
    }
    if (failed === undefined) throw new Error(`installer did not update DisplayVersion in 600s (requests ${JSON.stringify(requests)})`);
    console.log(`update smoke: DisplayVersion ${oldVersion} → ${displayVersion()}`);
    // 注册表在安装末尾写入，再等安装程序把收尾做完。
    await sleep(8000);
    const out2 = path.join(work, 'result-new.json');
    const child2 = spawn(exe, [], { env: smokeEnv(out2), stdio: 'inherit' });
    let exited2 = false;
    child2.once('exit', () => { exited2 = true; });
    const result = await waitForFile(out2, 300_000, () => exited2 && !existsSync(out2));
    if (!result) throw new Error('updated app produced no result');
    const smoke = report(result, 'updated app');
    ok = result.ok && smoke?.appVersion === opt.version && oldVersion !== opt.version;
  }
} catch (err) {
  console.error(`update smoke failed: ${err.message}`);
} finally {
  server.close();
}
console.log(`update smoke: requests ${JSON.stringify(requests)}`);
writeFileSync(path.join(work, 'requests.json'), JSON.stringify(requests));
if (ok) {
  console.log('update smoke ok');
  rmSync(work, { recursive: true, force: true });
} else {
  console.error(`update smoke failed (work dir kept: ${work})`);
}
process.exit(ok ? 0 : 1);
