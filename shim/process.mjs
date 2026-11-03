// preload 读 process.platform / arch / argv / env。webview 里没有 process，
// 由 esbuild inject 替换成这个对象，值来自 Rust 注入的 window.__CINDY_BOOT__。
const boot = typeof window !== 'undefined' ? window.__CINDY_BOOT__ ?? {} : {};

export const process = {
  platform: boot.platform ?? 'darwin',
  arch: boot.arch ?? 'arm64',
  argv: Array.isArray(boot.argv) ? boot.argv : [],
  env: {},
  versions: {},
};
