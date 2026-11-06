// Cindy 云端地址（global 区，cindy/config/endpoint.global.json 的子集）。
// 测试可用 CINDY_TAURI_AUTH_BASE_URL / CINDY_TAURI_MODEL_ACCESS_BASE_URL 指到本机 mock，
// 只接受 127.0.0.1 的 http 地址，避免环境变量把登录凭据导向外部主机。
export const AUTH_REGION = 'global';

const GLOBAL = {
  authApiBaseUrl: 'https://auth.cindy.app',
  modelAccessApiBaseUrl: 'https://model-access.cindy.app',
  authDesktopCallbackUrl: 'https://auth.cindy.app/api/auth/desktop/callback',
  websiteUrl: 'https://cindy.app',
};

function loopbackOverride(name) {
  const raw = process.env[name];
  if (!raw) return null;
  try {
    const u = new URL(raw);
    if (u.protocol === 'http:' && u.hostname === '127.0.0.1' && !u.username && !u.password && !u.search && !u.hash) {
      return u.toString().replace(/\/+$/, '');
    }
  } catch { /* 下面报错 */ }
  process.stderr.write(`[cloud-endpoints] ignoring ${name}: only http://127.0.0.1 is allowed\n`);
  return null;
}

const authOverride = loopbackOverride('CINDY_TAURI_AUTH_BASE_URL');
const modelAccessOverride = loopbackOverride('CINDY_TAURI_MODEL_ACCESS_BASE_URL');

export function authBaseUrl() {
  return authOverride ?? GLOBAL.authApiBaseUrl;
}

export function modelAccessBaseUrl() {
  return modelAccessOverride ?? GLOBAL.modelAccessApiBaseUrl;
}

export function authDesktopCallbackUrl() {
  return authOverride ? `${authOverride}/api/auth/desktop/callback` : GLOBAL.authDesktopCallbackUrl;
}

export function websiteUrl() {
  return GLOBAL.websiteUrl;
}
