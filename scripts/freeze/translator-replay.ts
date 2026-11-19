// translator 重放器（esbuild 打包后由 scripts/maker-freeze.mjs 调用）：
// 读 pi RPC 帧 JSONL（baseline/record.mjs 录制），逐帧喂 translatePiEvent，
// 把 AgentEvent 流写到 stdout JSONL。这是 maker 事件形状的 TS 金标准产出端。
import { readFileSync } from 'node:fs';
// 打包期由 alias 指向 cindy 仓库的 maker-core。
// 打包期由 alias 指向 cindy 仓库的 maker-core。
import { createPiTranslateContext, translatePiEvent, disposePiTranslateContext, markPiHostAbortRequested } from '@cindy/maker-core/agents/pi/translator';
export { createPiTranslateContext, translatePiEvent, disposePiTranslateContext };

const [, , framesPath] = process.argv;
const frames = readFileSync(framesPath, 'utf8')
  .trim()
  .split('\n')
  .map((l) => JSON.parse(l))
  // rpc-client 只把事件帧喂给 translator；response（RPC 回包）不过翻译层。
  .filter((f) => f.type !== 'response');

const events = [];
const queue = {
  push(e) { events.push(e); return true; },
  end() {},
  clear() {},
  get pending() { return 0; },
  [Symbol.asyncIterator]() { return { next: async () => ({ done: true }) }; },
};
const logger = {
  trace() {}, debug() {}, info() {}, warn() {}, error() {}, fatal() {},
  child() { return logger; },
};

const ctx = createPiTranslateContext(logger, { model: 'mock-model', api: 'openai-completions' });
const GROUP = process.env.TRANSLATOR_GROUP === '1';
for (const f of frames) {
  // 伪帧：宿主在发 abort RPC 之前登记主动停止（真实链路里由宿主调用，不在 pi 帧流里）。
  if (f.type === '__host_abort') { markPiHostAbortRequested(ctx); continue; }
  const before = events.length;
  try { translatePiEvent(f, queue, ctx); } catch (err) {
    console.error(`[replay] translate failed on frame ${f.type}: ${err?.message ?? err}`);
    process.exit(3);
  }
  if (GROUP) {
    const produced = events.slice(before).map((e) => e.type);
    console.error(`FRAME#${frames.indexOf(f)} ${f.type} ${JSON.stringify(f.assistantMessageEvent?.type ?? '')} -> ${produced.join(',') || '-'}`);
  }
}
// 心跳 / 定时推送落盘后再收一次尾巴。
setTimeout(() => {
  for (const e of events) console.log(JSON.stringify(e));
  disposePiTranslateContext(ctx);
}, 300);
