/**
 * i18n 入口 —— 同步 init i18next，导出常量与类型。
 *
 * 设计要点：
 * - 资源同步 import (零网络/零 IO)，i18n.init 同步完成 → React 首屏不闪。
 * - 默认 namespace 为 'common'；体积较小、边界清晰的功能文案可独立拆分。
 * - fallbackLng：缺 key 时不显示 key 本身,直接回退英文。
 * - 不接 LanguageDetector backend，初始化与 useLocale 共用现有语言偏好。
 *   'system' 的实际语言由 main 侧读取 OS 首选语言后传入 renderer。
 */

import i18n from 'i18next';
import { initReactI18next } from 'react-i18next';
import { BRAND_NAME } from '@cindy/maker-shared/branding';

import { GHOST_OFFICIAL_ID_PREFIX } from '../../shared/ghost';
import { DEFAULT_LOCALE } from '../../shared/locale';
import { getEffectiveLocale } from '../lib/localePreference';

export {
  DEFAULT_LOCALE,
  resolvePreferredSystemLocale,
  resolveSystemLocale,
  SUPPORTED_LOCALES,
} from '../../shared/locale';
export type { LocalePreference, SupportedLocale } from '../../shared/locale';

// 凭证写入逃生口必须在全部语言里共用同一条安全命令；集中覆盖防止某个 locale 又退回
// “手动设置环境变量”的危险指引。对应跨 locale 契约由 devOAuthWritePolicy.test 锁住。
// 每种语言一个异步加载器：只在用到时才取对应的 JSON（每种约 1 MB），首屏不再带上全部 12 种语言。
// 凭证写入逃生口必须在全部语言里共用同一条安全命令；集中覆盖防止某个 locale 又退回
// “手动设置环境变量”的危险指引。对应跨 locale 契约由 devOAuthWritePolicy.test 锁住。
const localeLoaders: Record<string, () => Promise<{ common: Record<string, any>; aiRename: Record<string, any> }>> = {
  'es': async () => {
    const [{ default: esCommon }, { default: esAiRename }] = await Promise.all([import('./locales/es/common.json'), import('./locales/es/aiRename.json')]);
    return { common: esCommon, aiRename: esAiRename };
  },
  'fr': async () => {
    const [{ default: frCommon }, { default: frAiRename }] = await Promise.all([import('./locales/fr/common.json'), import('./locales/fr/aiRename.json')]);
    return { common: frCommon, aiRename: frAiRename };
  },
  'de': async () => {
    const [{ default: deCommon }, { default: deAiRename }] = await Promise.all([import('./locales/de/common.json'), import('./locales/de/aiRename.json')]);
    return { common: deCommon, aiRename: deAiRename };
  },
  'pt-PT': async () => {
    const [{ default: ptPTCommon }, { default: ptPTAiRename }] = await Promise.all([import('./locales/pt-PT/common.json'), import('./locales/pt-PT/aiRename.json')]);
    return { common: ptPTCommon, aiRename: ptPTAiRename };
  },
  'pt-BR': async () => {
    const [{ default: ptBRCommon }, { default: ptBRAiRename }] = await Promise.all([import('./locales/pt-BR/common.json'), import('./locales/pt-BR/aiRename.json')]);
    return { common: ptBRCommon, aiRename: ptBRAiRename };
  },
  'zh-Hant-x-lzh': async () => {
    const [{ default: lzhCommon }, { default: lzhAiRename }] = await Promise.all([import('./locales/zh-Hant-x-lzh/common.json'), import('./locales/zh-Hant-x-lzh/aiRename.json')]);
    return {
    common: {
      ...lzhCommon,
      chatgptAuthRecovery: {
        ...lzhCommon.chatgptAuthRecovery,
        devWriteBlocked: '開發之境，例以唯讀共用此機之 ChatGPT 登入，不得發起登入，亦不得改寫憑證。欲試 OAuth，唯執行：pnpm restart:desktop:remote -- --isolated-auth --isolated。指令碼驗明獨立沙箱後，自會開放寫入，毋須手設環境變數。',
      },
    },
    aiRename: lzhAiRename,
  };
  },
  'zh-Hans-x-lzh': async () => {
    const [{ default: lzhHansCommon }, { default: lzhHansAiRename }] = await Promise.all([import('./locales/zh-Hans-x-lzh/common.json'), import('./locales/zh-Hans-x-lzh/aiRename.json')]);
    return {
    common: {
      ...lzhHansCommon,
      chatgptAuthRecovery: {
        ...lzhHansCommon.chatgptAuthRecovery,
        devWriteBlocked: '开发之境，例以只读共用此机之 ChatGPT 登录，不得发起登录，亦不得改写凭证。欲试 OAuth，唯执行：pnpm restart:desktop:remote -- --isolated-auth --isolated。脚本验明独立沙箱后，自会开放写入，毋须手设环境变量。',
      },
    },
    aiRename: lzhHansAiRename,
  };
  },
  en: async () => {
    const [{ default: enCommon }, { default: enAiRename }] = await Promise.all([import('./locales/en/common.json'), import('./locales/en/aiRename.json')]);
    return {
    common: {
      ...enCommon,
      chatgptAuthRecovery: {
        ...enCommon.chatgptAuthRecovery,
        devWriteBlocked: 'Development builds use this computer’s ChatGPT sign-in read-only by default and cannot start OAuth or change its credentials. To test OAuth, run: pnpm restart:desktop:remote -- --isolated-auth --isolated. The restart script enables writes only after verifying the sandbox.',
      },
    },
    aiRename: enAiRename,
  };
  },
  'zh-CN': async () => {
    const [{ default: zhCNCommon }, { default: zhCNAiRename }] = await Promise.all([import('./locales/zh-CN/common.json'), import('./locales/zh-CN/aiRename.json')]);
    return {
    common: {
      ...zhCNCommon,
      chatgptAuthRecovery: {
        ...zhCNCommon.chatgptAuthRecovery,
        devWriteBlocked: '开发环境默认只读共享本机 ChatGPT 登录，不能发起登录或改写凭证。测试 OAuth 只能运行：pnpm restart:desktop:remote -- --isolated-auth --isolated。脚本验证独立沙箱后会自动开放写入，无需手动设置环境变量。',
      },
    },
    aiRename: zhCNAiRename,
  };
  },
  'zh-TW': async () => {
    const [{ default: zhTWCommon }, { default: zhTWAIName }] = await Promise.all([import('./locales/zh-TW/common.json'), import('./locales/zh-TW/aiRename.json')]);
    return {
    common: {
      ...zhTWCommon,
      chatgptAuthRecovery: {
        ...zhTWCommon.chatgptAuthRecovery,
        devWriteBlocked: '開發環境預設以唯讀方式共用本機 ChatGPT 登入，不能發起登入或改寫憑證。測試 OAuth 只能執行：pnpm restart:desktop:remote -- --isolated-auth --isolated。指令碼驗證獨立沙箱後會自動開放寫入，無需手動設定環境變數。',
      },
    },
    aiRename: zhTWAIName,
  };
  },
  ja: async () => {
    const [{ default: jaCommon }, { default: jaAiRename }] = await Promise.all([import('./locales/ja/common.json'), import('./locales/ja/aiRename.json')]);
    return {
    common: {
      ...jaCommon,
      chatgptAuthRecovery: {
        ...jaCommon.chatgptAuthRecovery,
        devWriteBlocked: '開発環境では、このコンピューターの ChatGPT ログインを既定で読み取り専用として共有するため、OAuth の開始や認証情報の変更はできません。OAuth をテストする場合は、pnpm restart:desktop:remote -- --isolated-auth --isolated を実行してください。スクリプトはサンドボックスを検証した後にのみ書き込みを許可します。環境変数を手動で設定する必要はありません。',
      },
    },
    aiRename: jaAiRename,
  };
  },
  ko: async () => {
    const [{ default: koCommon }, { default: koAiRename }] = await Promise.all([import('./locales/ko/common.json'), import('./locales/ko/aiRename.json')]);
    return {
    common: {
      ...koCommon,
      chatgptAuthRecovery: {
        ...koCommon.chatgptAuthRecovery,
        devWriteBlocked: '개발 환경은 기본적으로 이 컴퓨터의 ChatGPT 로그인을 읽기 전용으로 공유하므로 OAuth를 시작하거나 자격 증명을 변경할 수 없습니다. OAuth를 테스트하려면 pnpm restart:desktop:remote -- --isolated-auth --isolated을 실행하세요. 스크립트가 샌드박스를 검증한 뒤에만 쓰기를 허용하므로 환경 변수를 직접 설정할 필요가 없습니다.',
      },
    },
    aiRename: koAiRename,
  };
  },
};

// 每种语言的加载状态：同一语言只加载一次，加载失败后允许重试。
const loading = new Map<string, Promise<void>>();

/** 把某种语言的资源加进 i18next（已加载则直接返回）。未知语言或加载失败都不抛错，由 fallbackLng 兜底。 */
export function ensureLocaleLoaded(lng: string): Promise<void> {
  const loader = localeLoaders[lng];
  if (!loader || i18n.hasResourceBundle(lng, 'common')) return Promise.resolve();
  let p = loading.get(lng);
  if (!p) {
    p = loader()
      .then((res) => {
        i18n.addResourceBundle(lng, 'common', res.common, true, true);
        i18n.addResourceBundle(lng, 'aiRename', res.aiRename, true, true);
      })
      .catch((e) => {
        console.warn(`[i18n] failed to load locale ${lng}`, e);
      })
      .finally(() => loading.delete(lng));
    loading.set(lng, p);
  }
  return p;
}

// 缺 key 时的回退链（与下面 init 的 fallbackLng 保持一致）：先加载这些，再切到目标语言。
function fallbackChain(lng: string): string[] {
  if (lng === 'zh-Hant-x-lzh') return ['zh-TW', DEFAULT_LOCALE];
  if (lng === 'zh-Hans-x-lzh') return ['zh-CN', DEFAULT_LOCALE];
  return [DEFAULT_LOCALE];
}

/** 加载某语言及其回退语言。 */
function loadWithFallbacks(lng: string): Promise<unknown> {
  return Promise.all([lng, ...fallbackChain(lng)].map(ensureLocaleLoaded));
}

// 切换语言前先把资源加载好，否则切换瞬间会短暂显示回退语言或 key。
const originalChangeLanguage = i18n.changeLanguage.bind(i18n);
i18n.changeLanguage = (async (lng?: string, ...rest: unknown[]) => {
  if (lng) await loadWithFallbacks(lng);
  return (originalChangeLanguage as (l?: string, ...r: unknown[]) => unknown)(lng, ...rest);
}) as typeof i18n.changeLanguage;

// 不带资源初始化（resources 为空），随后由 `i18nReady` 加载当前语言。
// 入口必须在首次渲染前 await `i18nReady`，否则首屏会先显示 key 再换成翻译。
void i18n.use(initReactI18next).init({
  resources: {},
  partialBundledLanguages: true,
  // 翻译资源热更新会重新执行本模块，不能把用户已选语言重置为默认英文。
  lng: getEffectiveLocale(),
  // 缺 key 回退英文。
  // 文言版缺 key 先落其现代底本（繁體 / 简体），再落英文。
  fallbackLng: {
    'zh-Hant-x-lzh': ['zh-TW', DEFAULT_LOCALE],
    'zh-Hans-x-lzh': ['zh-CN', DEFAULT_LOCALE],
    default: [DEFAULT_LOCALE],
  },
  defaultNS: 'common',
  ns: ['common', 'aiRename'],
  interpolation: {
    escapeValue: false, // React 已转义
    // 品牌名单一事实源:locale 文案里的 {{appName}} 全部由此注入,改名只改
    // @cindy/maker-shared/branding 的 BRAND_NAME(见该文件对"不跟随改名"标识符的说明)。
    //
    // pronoun:伙伴文案里指代该伙伴的第三人称。具体视图可按当前伙伴传入；这里给
    // 一个通用兜底，避免漏传时把 `{{pronoun}}` 原样显示给用户。
    // 用户装入通道只保留 cindy-。官方运行时前缀仍在 GHOST_OFFICIAL_ID_PREFIXES。
    defaultVariables: {
      appName: BRAND_NAME,
      pronoun: '这位伙伴',
      reservedGhostIdPrefixes: GHOST_OFFICIAL_ID_PREFIX,
    },
  },
  parseMissingKeyHandler(key: string, defaultValue?: string) {
    if (
      (key.startsWith('ipcError.') || key.startsWith('chat.remoteError.')) &&
      key !== 'ipcError.INTERNAL'
    ) {
      const lng = i18n.resolvedLanguage || i18n.language;
      const fallback =
        i18n.getResource(lng, 'common', 'ipcError.INTERNAL') ??
        i18n.getResource('en', 'common', 'ipcError.INTERNAL');
      if (typeof fallback === 'string') return fallback;
    }
    return defaultValue ?? key;
  },
  returnNull: false,
});

/** 当前语言（及回退语言）加载完成。入口在渲染前 await 它。 */
export const i18nReady: Promise<void> = loadWithFallbacks(getEffectiveLocale()).then(() =>
  originalChangeLanguage(getEffectiveLocale()).then(() => undefined),
);

export { i18n };
export default i18n;
