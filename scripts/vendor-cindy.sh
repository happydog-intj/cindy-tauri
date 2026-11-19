#!/bin/sh
# 把上游 cindy 构建渲染层所需的源码同步到 vendor/cindy（用法：scripts/vendor-cindy.sh <上游目录>）。
# 只带 git 跟踪的文件；排除测试、移动端、大部分 docs。其余 workspace 只留 package.json 以保持锁文件一致。
set -e
SRC="${1:?用法: vendor-cindy.sh <cindy 仓库目录>}"
DST="$(cd "$(dirname "$0")/.." && pwd)/vendor/cindy"
mkdir -p "$DST"
LIST="$(mktemp)"
( cd "$SRC" && git ls-files ) | awk '
  /(^|\/)__tests__\// {next}
  /\.(test|spec)\.(ts|tsx|mjs|js)$/ {next}
  /^apps\/(mobile|android-platform-tools-bin|cindy-headless)\// { if ($0 ~ /package\.json$/) print; next }
  /^docs\// { if ($0 ~ /^docs\/(design-rules\/DESIGN\.md|examples\/model-catalog\.md|fixtures\/plugin-usage\.json)$/) print; next }
  /^prototypes\// {next}
  {print}' > "$LIST"
rsync -a --files-from="$LIST" "$SRC/" "$DST/"
rm -f "$LIST"
( cd "$SRC" && git rev-parse HEAD ) > "$DST/.upstream-commit"
echo "vendored $(find "$DST" -type f | wc -l) files -> $DST"
