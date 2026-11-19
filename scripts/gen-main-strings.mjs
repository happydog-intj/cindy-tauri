// 生成 src-tauri/src/main_strings.json：托盘 / 通知 / 原生对话框用的主进程文案。
// 从 cindy renderer 的 locale JSON 里按 scripts/main-strings/keys.json 抽取；上游改文案后重跑并提交。
// 用法：node scripts/gen-main-strings.mjs [cindy 仓库目录]，默认读 CINDY_DIR 或 ../cindy。
import { readFileSync, writeFileSync } from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const cindyDir = path.resolve(process.argv[2] ?? process.env.CINDY_DIR ?? path.join(root, '..', 'cindy'));
const localesDir = path.join(cindyDir, 'apps', 'desktop', 'src', 'renderer', 'i18n', 'locales');
const LOCALES = ['zh-CN', 'zh-TW', 'en', 'ja', 'ko', 'es', 'fr', 'de', 'pt-PT', 'pt-BR', 'zh-Hant-x-lzh', 'zh-Hans-x-lzh'];
const keys = JSON.parse(readFileSync(path.join(root, 'scripts', 'main-strings', 'keys.json'), 'utf8'));

const out = {};
for (const loc of LOCALES) {
  const bundle = JSON.parse(readFileSync(path.join(localesDir, loc, 'common.json'), 'utf8'));
  const picked = {};
  for (const key of keys) {
    const v = key.split('.').reduce((o, k) => (o && typeof o === 'object' ? o[k] : undefined), bundle);
    if (typeof v === 'string') picked[key] = v;
  }
  out[loc] = picked;
}
const missing = keys.filter((k) => !(k in out.en));
if (missing.length) throw new Error(`main strings missing in en: ${missing.join(', ')}`);
writeFileSync(path.join(root, 'src-tauri', 'src', 'main_strings.json'), JSON.stringify(out, null, 1) + '\n');
console.log(`wrote main_strings.json: ${LOCALES.length} locales × ${keys.length} keys`);
