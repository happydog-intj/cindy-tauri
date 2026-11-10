import json,sys
o=json.load(open(sys.argv[1])); r=json.load(open(sys.argv[2])); bad=0
for i,(a,b) in enumerate(zip(o,r)):
    if a['text']==b['text'] and a['isError']==b['isError']: continue
    if a['call']['name']=='grep' and a['isError']==b['isError'] and sorted(a['text'].split('\n'))==sorted(b['text'].split('\n')): print(i,'order-only',json.dumps(a['call']['args'])); continue
    bad+=1; print(i,'DIFF',json.dumps(a['call']),a['isError'],b['isError']); print('  orig',json.dumps(a['text'])[:260]); print('  rs  ',json.dumps(b['text'])[:260])
print(len(o),'calls,',bad,'diffs')
