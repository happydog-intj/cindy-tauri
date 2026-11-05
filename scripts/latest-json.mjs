// 生成 tauri-plugin-updater 的 latest.json（静态 JSON 格式）。发布工作流和更新冒烟共用。
// 用法：node scripts/latest-json.mjs --version 0.0.2 --base-url https://…/download/v0.0.2/ --out latest.json \
//         [--notes "…"] darwin-aarch64-app=path/Cindy-Tauri.app.tar.gz windows-x86_64-nsis=path/setup.exe …
// 每个文件旁边必须有 tauri build 生成的 <file>.sig；下载地址 = base-url + 文件名。
import { readFileSync, writeFileSync } from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

// targets：{ 'darwin-aarch64-app': '/path/file', … }
export function buildLatestJson({ version, notes = '', pubDate = new Date().toISOString(), baseUrl, targets }) {
  if (!/^\d+\.\d+\.\d+(-[0-9A-Za-z.-]+)?$/.test(version)) throw new Error(`bad version ${version}`);
  const base = baseUrl.endsWith('/') ? baseUrl : `${baseUrl}/`;
  const platforms = {};
  for (const [key, file] of Object.entries(targets)) {
    if (!/^(darwin|windows)-(aarch64|x86_64)(-(app|nsis|msi))?$/.test(key)) throw new Error(`bad target ${key}`);
    const signature = readFileSync(`${file}.sig`, 'utf8').trim();
    platforms[key] = { signature, url: base + encodeURIComponent(path.basename(file)) };
  }
  if (Object.keys(platforms).length === 0) throw new Error('no targets');
  return { version, notes, pub_date: pubDate, platforms };
}

if (process.argv[1] && path.resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  const args = process.argv.slice(2);
  const opt = {};
  const targets = {};
  for (let i = 0; i < args.length; i++) {
    const a = args[i];
    if (a.startsWith('--')) opt[a.slice(2)] = args[++i];
    else {
      const eq = a.indexOf('=');
      if (eq < 0) throw new Error(`expected key=file, got ${a}`);
      targets[a.slice(0, eq)] = a.slice(eq + 1);
    }
  }
  if (!opt.version || !opt['base-url'] || !opt.out) {
    console.error('usage: latest-json.mjs --version V --base-url URL --out FILE [--notes N] key=file …');
    process.exit(2);
  }
  const json = buildLatestJson({ version: opt.version, notes: opt.notes ?? '', baseUrl: opt['base-url'], targets });
  writeFileSync(opt.out, JSON.stringify(json, null, 2) + '\n');
  console.log(`wrote ${opt.out}: ${Object.keys(json.platforms).join(', ')}`);
}
