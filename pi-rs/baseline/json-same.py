#!/usr/bin/env python3
# 两个 JSON 文件是否一致（忽略随机的 sessionId）。退出码 0 = 一致。
import json, sys
a, b = [json.load(open(f)) for f in sys.argv[1:3]]
for x in (a, b):
    if isinstance(x, dict): x.pop('sessionId', None)
sys.exit(0 if json.dumps(a, sort_keys=True) == json.dumps(b, sort_keys=True) else 1)
