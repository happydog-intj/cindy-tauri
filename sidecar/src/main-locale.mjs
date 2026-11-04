// 主进程侧文案：托盘菜单、系统通知、原生对话框用的少量 key。
// 对齐 desktop main/i18n.ts：renderer 通过 app-menu:set-locale 同步语言，之前用系统语言；
// 每个语言按 FALLBACK_CHAIN 回落，最后回落到 key 本身。
// 文案在构建时从 cindy renderer 的 locale JSON 里按 main-strings-keys.json 抽取（见 sidecar/build.mjs），
// 不把整份 common.json（单个近 1MB）打进 sidecar。
import strings from 'virtual:cindy-main-strings';
import { preferredLocale } from './ipc.mjs';

const BRAND_NAME = 'Cindy';

const FALLBACK_CHAIN = {
  es: ['es', 'en'],
  fr: ['fr', 'en'],
  de: ['de', 'en'],
  'pt-PT': ['pt-PT', 'en'],
  'pt-BR': ['pt-BR', 'en'],
  'zh-Hant-x-lzh': ['zh-Hant-x-lzh', 'zh-TW', 'en'],
  'zh-Hans-x-lzh': ['zh-Hans-x-lzh', 'zh-CN', 'en'],
  'zh-CN': ['zh-CN', 'en'],
  'zh-TW': ['zh-TW', 'zh-CN', 'en'],
  en: ['en'],
  ja: ['ja', 'en'],
  ko: ['ko', 'en'],
};

let current = null;
const listeners = new Set();

export function isSupportedMainLocale(locale) {
  return typeof locale === 'string' && Object.hasOwn(FALLBACK_CHAIN, locale);
}

// 不支持的值忽略（返回 false），与 desktop resolveApplicationMenuLocale 的“保持当前”一致。
export function setMainLocale(locale) {
  if (!isSupportedMainLocale(locale)) return false;
  if (current === locale) return true;
  current = locale;
  for (const fn of listeners) {
    try { fn(locale); } catch (err) { console.error('[main-locale] listener failed:', err); }
  }
  return true;
}

export function getMainLocale() {
  return current ?? preferredLocale();
}

export function onMainLocaleChange(fn) {
  listeners.add(fn);
  return () => listeners.delete(fn);
}

export function t(key, locale = getMainLocale()) {
  const chain = FALLBACK_CHAIN[locale] ?? FALLBACK_CHAIN.en;
  for (const loc of chain) {
    const v = strings[loc]?.[key];
    if (typeof v === 'string') return v.replaceAll('{{appName}}', BRAND_NAME);
  }
  return key;
}
