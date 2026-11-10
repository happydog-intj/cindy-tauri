#!/bin/bash
# 原版 vs pi-rs 的场景对照（mock 服务）：压缩、重试（5 种失败方式）、思考档位。每项输出 differing=N，N 必须为 0，
# 但压缩对照里 estimatedTokensAfter 有 16 的固定偏差（系统提示里文档路径长度不同），在 compact 项里按"已知差异"处理。
# 用法：baseline/compare.sh <原版pi> <pi-rs>
ORIG="$1"; PI="$2"; cd "$(dirname "$0")"; T=$(mktemp -d /tmp/pirs-cmp.XXXX); FAIL=0
cmpd() { n=$1; sc=$2; shift 2
  env "$@" PI_BIN="$ORIG" OUT=$T/$n-o.jsonl perl -e 'alarm 120; exec @ARGV' node $sc >/dev/null 2>&1
  env "$@" PI_BIN="$PI" OUT=$T/$n-r.jsonl perl -e 'alarm 120; exec @ARGV' node $sc >/dev/null 2>&1
  out=$(node diff.mjs $T/$n-o.jsonl $T/$n-r.jsonl); d=$(echo "$out" | tail -1 | sed -E 's/.*differing=//')
  printf '%-18s' $n
  if [ "$d" = 0 ]; then echo ok; else echo "FAIL differing=$d ($T/$n-*.jsonl)"; FAIL=1; fi; }
cmpd retry-503 retry-test.mjs MOCK_FAIL_FIRST=2
cmpd retry-exhaust retry-test.mjs MOCK_FAIL_FIRST=9
cmpd retry-cut retry-test.mjs MOCK_CUT_FIRST=1
# OpenAI 流被切断：原版的 message_start 事件里 content 是 JS 对象引用（事后才被填充），pi-rs 发的是发送时刻的快照。
# 只有 message_start 的 content 字段不同，失败回复（message_end / agent_end）一致，所以这里按"已知差异"把 message_start 归一化。
cmpd_oa() { n=retry-cut-openai
  API=openai-completions MOCK_CUT_FIRST=1 PI_BIN="$ORIG" OUT=$T/$n-o.jsonl perl -e 'alarm 120; exec @ARGV' node retry-test.mjs >/dev/null 2>&1
  API=openai-completions MOCK_CUT_FIRST=1 PI_BIN="$PI" OUT=$T/$n-r.jsonl perl -e 'alarm 120; exec @ARGV' node retry-test.mjs >/dev/null 2>&1
  for w in o r; do python3 - $T/$n-$w.jsonl <<'PY'
import json,sys
p=sys.argv[1]; out=[]
for l in open(p):
    j=json.loads(l)
    if j.get('type')=='message_start' and j['message']['role']=='assistant': j['message']['content']=[]
    out.append(json.dumps(j))
open(p,'w').write('\n'.join(out)+'\n')
PY
  done
  d=$(node diff.mjs $T/$n-o.jsonl $T/$n-r.jsonl | tail -1 | sed -E 's/.*differing=//'); printf '%-18s' $n; if [ "$d" = 0 ]; then echo ok; else echo "FAIL differing=$d ($T/$n-*.jsonl)"; FAIL=1; fi; }
cmpd_oa
cmpd retry-hang retry-test.mjs MOCK_HANG_FIRST=1 IDLE=600 WAIT=8000
cmpd retry-abort retry-test.mjs MOCK_FAIL_FIRST=9 ABORT_AFTER=1
# 压缩：估算值 estimatedTokensAfter / contextUsage.tokens 因系统提示里的路径长度不同有固定偏差，这里把这两个数归一化后比较。
env MOCK_REAL_USAGE=1 CW=6000 PI_BIN="$ORIG" OUT=$T/compact-o.jsonl perl -e 'alarm 120; exec @ARGV' node compact-test.mjs >/dev/null 2>&1
env MOCK_REAL_USAGE=1 CW=6000 PI_BIN="$PI" OUT=$T/compact-r.jsonl perl -e 'alarm 120; exec @ARGV' node compact-test.mjs >/dev/null 2>&1
for w in o r; do sed -E 's/"estimatedTokensAfter":[0-9]+/"estimatedTokensAfter":0/g; s/"contextUsage":\{"tokens":[0-9]+,"contextWindow":([0-9]+),"percent":[0-9.e-]+\}/"contextUsage":{"tokens":0,"contextWindow":\1,"percent":0}/g' $T/compact-$w.jsonl > $T/compact-$w.n.jsonl; done
d=$(node diff.mjs $T/compact-o.n.jsonl $T/compact-r.n.jsonl | tail -1 | sed -E 's/.*differing=//'); printf '%-18s' compact; if [ "$d" = 0 ]; then echo ok; else echo "FAIL differing=$d ($T)"; FAIL=1; fi
# 思考档位：请求体逐档位比较
for api in anthropic-messages openai-completions; do
  API=$api PI_BIN="$ORIG" OUT=$T/th-$api-o.json perl -e 'alarm 120; exec @ARGV' node think-test.mjs >/dev/null 2>&1
  API=$api PI_BIN="$PI" OUT=$T/th-$api-r.json perl -e 'alarm 120; exec @ARGV' node think-test.mjs >/dev/null 2>&1
  printf '%-18s' "think-$api"; if python3 json-same.py $T/th-$api-o.json $T/th-$api-r.json; then echo ok; else echo "FAIL ($T/th-$api-*.json)"; FAIL=1; fi
done
exit $FAIL
