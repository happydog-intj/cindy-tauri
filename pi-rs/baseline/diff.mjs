// 对拍：把两份录制归一化后逐条比较。易变字段（时间、路径、端口、随机 id、耗时）替换成占位符。
import { readFileSync } from 'node:fs';
const norm = (file) => readFileSync(file, 'utf8').trim().split('\n').map((l) => {
  const j = JSON.parse(l);
  return JSON.stringify(j, function (k, v) {
    if (['timestamp', 'durationMs'].includes(k)) return '<n>';
    if (k === 'usage' && this.type === 'message_update' && this.assistantMessageEvent?.type !== 'text_end' && this.assistantMessageEvent?.type !== 'toolcall_end') return '<u>';
    if (['sessionFile', 'sessionId', 'id', 'responseId', 'toolCallId'].includes(k) && typeof v === 'string' && k !== 'id') return '<s>';
    if (k === 'id' && typeof v === 'string' && v.length > 8) return '<s>';
    if (typeof v === 'string') return v.replace(/http:\/\/127\.0\.0\.1:\d+/g, 'http://H').replace(/\/(private\/)?var\/folders\/[^"\s]*?(work|sessions)/g, '<tmp>/$2').replace(/call_mock_[\w-]+/g, 'call_X');
    return v;
  }).replace(/- (Main documentation|Additional docs|Examples): [^\\]*?(?=(\\n|\\"| \())/g, '- $1: <PKG>').replace(/(Stream was interrupted: [^"]*|The socket connection was closed unexpectedly\. For more information, pass `verbose: true` in the second argument to fetch\(\))/g, '<NETERR>').replace(/"contextUsage":\{"tokens":\d+,"contextWindow":(\d+),"percent":[0-9.e-]+\}/g, '"contextUsage":{"tokens":0,"contextWindow":$1,"percent":0}').replace(/"(id|parentId|targetId|firstKeptEntryId)":"[0-9a-f]{8}"/g, '"$1":"<id8>"');

});
const [a, b] = [norm(process.argv[2]), norm(process.argv[3])];
let bad = 0;
for (let i = 0; i < Math.max(a.length, b.length); i++) {
  if (a[i] === b[i]) continue;
  bad++;
  if (bad > 6) continue;
  console.log(`--- #${i}`);
  const x = a[i] ?? '(none)', y = b[i] ?? '(none)';
  let p = 0; while (x[p] === y[p] && p < x.length) p++;
  console.log('orig:', x.slice(Math.max(0, p - 60), p + 140)); console.log('rs  :', y.slice(Math.max(0, p - 60), p + 140));
}
console.log(`records orig=${a.length} rs=${b.length} differing=${bad}`);
