#!/bin/sh
# 把上游 cindy 构建渲染层所需的源码同步到 frontend（用法：scripts/sync-frontend.sh <上游目录>）。
# 只带 git 跟踪的文件；排除测试、移动端、大部分 docs。其余 workspace 只留 package.json 以保持锁文件一致。
set -e
SRC="${1:?用法: sync-frontend.sh <cindy 仓库目录>}"
DST="$(cd "$(dirname "$0")/.." && pwd)/frontend"
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
# 本地改动：只装当前平台的可选依赖。上游 package.json 的 pnpm.supportedArchitectures 列了全部 os/cpu，
# 会把 claude-agent-sdk 等的 8 个平台二进制（含 claude.exe，每个约 230 MB）全装下来。
node -e '
const fs=require("fs"),p=process.argv[1],j=JSON.parse(fs.readFileSync(p,"utf8"));
j.pnpm=j.pnpm||{};j.pnpm.supportedArchitectures={os:["current"],cpu:["current"],libc:["current"]};
fs.writeFileSync(p,JSON.stringify(j,null,2)+"\n");' "$DST/package.json"
( cd "$SRC" && git rev-parse HEAD ) > "$DST/.upstream-commit"
echo "synced $(find "$DST" -type f | wc -l) files -> $DST"
