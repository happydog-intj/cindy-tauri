#!/bin/sh
# 同步 maker-freeze 用的 translator 依赖闭包到 scripts/freeze/upstream-maker（用法：scripts/sync-upstream-maker.sh <上游目录>）。
set -e
SRC="${1:?用法: sync-upstream-maker.sh <cindy 仓库目录>}"
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
DST="$ROOT/scripts/freeze/upstream-maker"
LIST="$(mktemp)"
CLOSURE_SRC="$ROOT/scripts/.closure.tmp.mjs"
cat > "$CLOSURE_SRC" <<'JS'
import { build } from 'esbuild';
import path from 'node:path';
const root = process.cwd(), c = path.resolve(process.argv[2]);
const r = await build({
  entryPoints: [path.join(root, 'scripts/freeze/translator-replay.ts')], bundle: true, platform: 'node', format: 'cjs', target: 'node22', write: false, metafile: true,
  nodePaths: [c + '/apps/cindy-headless/node_modules', c + '/apps/desktop/node_modules', c + '/node_modules'],
  alias: { '@cindy/maker-core/agents/pi/translator': c + '/packages/maker-core/src/agents/pi/translator.ts' },
  loader: { '.md': 'text' }, logLevel: 'silent',
  define: { 'import.meta.url': '"file:///x"', 'import.meta.env': '{"VITE_CINDY_AUTH_REGION":"global"}' },
});
for (const f of Object.keys(r.metafile.inputs).map((f) => path.resolve(f))) if (f.startsWith(c + '/') && !f.includes('node_modules')) console.log(path.relative(c, f));
JS
( cd "$ROOT" && node "$CLOSURE_SRC" "$SRC" > "$LIST" ) || { rm -f "$CLOSURE_SRC" "$LIST"; exit 1; }
rm -f "$CLOSURE_SRC"
rm -rf "$DST/packages"
mkdir -p "$DST"
rsync -a --files-from="$LIST" "$SRC/" "$DST/"
( cd "$SRC" && git rev-parse HEAD ) > "$DST/.upstream-commit"
rm -f "$LIST"
echo "synced $(find "$DST/packages" -type f | wc -l) files -> $DST"
