// 把 sidecar 打成单个 CJS 文件，供 Tauri 资源和 dev 模式使用。
import { build } from 'esbuild';
import { fileURLToPath } from 'node:url';
import path from 'node:path';

const here = path.dirname(fileURLToPath(import.meta.url));
// 直接复用 cindy desktop 的纯函数模块（shared/ 下无 electron 依赖的部分）。
const cindyDir = path.resolve(process.env.CINDY_DIR ?? path.join(here, '..', '..', 'cindy'));
await build({
  alias: { '@cindy-desktop-shared': path.join(cindyDir, 'apps', 'desktop', 'src', 'shared') },
  entryPoints: [path.join(here, 'src/index.mjs')],
  bundle: true,
  platform: 'node',
  format: 'cjs',
  target: 'node22',
  outfile: path.join(here, 'dist/sidecar.cjs'),
  logLevel: 'info',
});
