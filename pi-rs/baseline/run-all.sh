#!/bin/bash
# 回归入口：编译 pi-rs，然后跑 单元测试、系统提示对照、工具输出对照、export_html 对照。
# 需要原版 Pi（默认路径见 ORIG），对照 find 时需要 fd（PATH 里没有则 find 的原版结果是报错，会被标出来）。
# 加 --e2e：再用刚编译的 pi-rs（CINDY_PI_RS）跑 Cindy 的四套 e2e。加 --real：再对本机 brconnector 网关跑真实模型检查。
# 对照类测试需要原版 Pi 二进制（只用来对照，不随包；ORIG 可覆盖路径）。没有原版时跳过对照类，只跑 pi-rs 自身的测试。
set -u
cd "$(dirname "$0")/.."
ORIG="${ORIG:-$HOME/Library/Application Support/com.cindy.tauri/agents/pi/1.4.0/darwin-arm64/pi}"
PI="$PWD/target/release/pi"; B=baseline; T=$(mktemp -d /tmp/pirs-reg.XXXX); FAIL=0
HAVE_ORIG=1; [ -x "$ORIG" ] || HAVE_ORIG=0
step() { printf '%-28s' "$1"; }
skip_orig_step() { if [ $HAVE_ORIG = 0 ]; then printf '%-28s%s\n' "$1" "skipped (no original Pi at $ORIG)"; return 0; fi; return 1; }
skip_orig() { if [ $HAVE_ORIG = 0 ]; then echo "skipped (no original Pi at \$ORIG)"; return 0; fi; return 1; }
res() { if [ "$1" = 0 ]; then echo ok; else echo "FAIL ($2)"; FAIL=1; fi; }
run() { perl -e 'alarm shift; exec @ARGV' "$@"; }

step "cargo build + test"; (cargo build --release -q && cargo test --release -q) >$T/cargo.log 2>&1; res $? $T/cargo.log

if skip_orig_step "system prompt (12 cases)"; then :; else
step "system prompt (12 cases)"
printf "APPEND ONE" > /tmp/app1.txt; printf "APPEND TWO" > /tmp/app2.txt
export CASES="$(cat $B/sys-cases.json)"
(cd $B && PI_BIN="$ORIG" OUT=$T/sys-o.json run 120 node sys-test.mjs && PI_BIN="$PI" OUT=$T/sys-r.json run 120 node sys-test.mjs) >$T/sys.log 2>&1
python3 - $T/sys-o.json $T/sys-r.json >>$T/sys.log 2>&1 <<'PY'
import json,sys
o=json.load(open(sys.argv[1])); r=json.load(open(sys.argv[2])); bad=[k for k in o if o[k]!=r.get(k)]
print('diff cases:',bad); sys.exit(1 if bad else 0)
PY
res $? $T/sys.log
fi
if skip_orig_step "tools grep/find/ls (31)"; then :; else
step "tools grep/find/ls (31)"
(cd $B && PATH=/tmp/fdroot/bin:$PATH PI_BIN="$ORIG" OUT=$T/t-o.json run 200 node tools-test.mjs && PI_BIN="$PI" OUT=$T/t-r.json run 200 node tools-test.mjs) >$T/tools.log 2>&1
# 已知差异：命中数超过 limit 时，原版(fd/rg 并行遍历)取哪几个是随机的，pi-rs 按名字排序。这类用例只比"数量+提示语"。
python3 - $T/t-o.json $T/t-r.json >>$T/tools.log 2>&1 <<'PY'
import json,sys
o=json.load(open(sys.argv[1])); r=json.load(open(sys.argv[2])); bad=0
for i,(a,b) in enumerate(zip(o,r)):
    if a['isError']!=b['isError']: bad+=1; print(i,'error flag differs',a['call']); continue
    if a['text']==b['text']: continue
    args=a['call']['args']
    if a['call']['name']=='grep' and sorted(a['text'].split('\n'))==sorted(b['text'].split('\n')): continue
    if 'limit' in args and len(a['text'].split('\n'))==len(b['text'].split('\n')) and a['text'].split('\n')[-1]==b['text'].split('\n')[-1]: continue
    bad+=1; print(i,'DIFF',a['call']); print(' ',a['text'][:200]); print(' ',b['text'][:200])
print(len(o),'calls',bad,'diffs'); sys.exit(1 if bad else 0)
PY
res $? $T/tools.log
fi
if skip_orig_step "export_html"; then :; else
step "export_html"
(cd $B && PI_BIN="$ORIG" OUT=$T/e-o.html run 60 node export-test.mjs && PI_BIN="$PI" OUT=$T/e-r.html run 60 node export-test.mjs) >$T/exp.log 2>&1
python3 - $T/e-o.html $T/e-r.html >>$T/exp.log 2>&1 <<'PY'
import re,base64,json,sys
def load(p):
    h=open(p).read(); d=re.search(r'id="session-data"[^>]*>([^<]+)<',h)
    # 已知差异：模板里内嵌的 highlight.min.js 一处和 Cost 行在原版里被 String.replace 的 "$" 特殊序列改写过
    return h.replace(d.group(1),'DATA').split('\n'), json.loads(base64.b64decode(d.group(1)))
a,ja=load(sys.argv[1]); b,jb=load(sys.argv[2])
import difflib
n=[l for l in difflib.unified_diff(a,b,lineterm='',n=0) if l[:1] in '+-' and l[:3] not in ('+++','---')]
ok = list(ja)==list(jb) and list(ja['header'])==list(jb['header']) and [e['type'] for e in ja['entries']]==[e['type'] for e in jb['entries']] and ja['tools']==jb['tools']
print('html diff lines:',len(n),'data structure same:',ok); sys.exit(0 if ok and len(n)<=6 else 1)
PY
res $? $T/exp.log
fi
if skip_orig_step "scenarios vs original"; then :; else
step "scenarios vs original"; (cd $B && ./compare.sh "$ORIG" "$PI") >$T/cmp.log 2>&1; res $? $T/cmp.log
fi
step "bridge behaviour"; (cd $B && PI_BIN="$PI" run 60 node bridge-test.mjs) >$T/bridge.log 2>&1; res $? $T/bridge.log
step "stress (3x150 rounds)"; for sd in 1 2 3; do (cd $B && PI_BIN="$PI" SEED=$sd ROUNDS=150 run 250 node stress-test.mjs) >>$T/stress.log 2>&1 || { false; break; }; done; res $? $T/stress.log
step "per-session fallback"; PI_BIN="$PI" run 60 node $B/fallback-test.mjs >$T/fb.log 2>&1; res $? $T/fb.log

# 真实网关检查（需要本机 Cindy 的 brconnector 代理，没有就跳过）。加 --real 才跑，耗时几分钟、会消耗模型额度。
HAS_REAL=0; HAS_E2E=0; for a in "$@"; do [ "$a" = "--real" ] && HAS_REAL=1; [ "$a" = "--e2e" ] && HAS_E2E=1; done
if [ $HAS_REAL = 1 ]; then
  if ! curl -s -m 5 -o /dev/null http://127.0.0.1:30001/brconnector/models; then
    step "real gateway"; echo "skipped (no gateway at 127.0.0.1:30001)"
  else
    step "real: anthropic basics"; (cd $B && PI_BIN="$PI" LEVELS=off,low,high CW=2600 RESERVE=600 KEEP=300 EXTRA=5 run 400 node real-anthropic.mjs) >$T/real1.log 2>&1; res $? $T/real1.log
    step "real: thinking blocks"; rc=0; for cfg in "anthropic-messages claude-shein-glm-5.3-flashx" "openai-completions claude-shein-deepseek-v4.1-flash"; do set -- $cfg; (cd $B && PI_BIN="$PI" API=$1 MODEL=$2 run 300 node real-think.mjs) >>$T/real2.log 2>&1 || rc=1; done; res $rc $T/real2.log
    step "real: auto compaction"; (cd $B && PI_BIN="$PI" API=anthropic-messages MODEL=claude-shein-shannon-auto LINES=60 run 400 node real-autocompact.mjs) >$T/real3.log 2>&1; res $? $T/real3.log
    step "real: overflow recovery"; (cd $B && PI_BIN="$PI" MODEL=claude-shein-shannon-free CHUNK=20000 MSGS=5 run 1500 node real-overflow.mjs) >$T/real4.log 2>&1; res $? $T/real4.log
  fi
fi

if [ $HAS_E2E = 1 ]; then
  for n in session goal model-switch settings; do
    step "cindy e2e: $n"; D=$(mktemp -d /tmp/e2e-X.XXXX)
    (cd .. && CINDY_PI_RS="$PI" CINDY_E2E_DATA_DIR=$D run 170 node scripts/$n-e2e.mjs) >$T/e2e-$n.log 2>&1
    tail -1 $T/e2e-$n.log | grep -q '"ok":true'; res $? $T/e2e-$n.log
  done
fi
echo "logs: $T"; exit $FAIL
