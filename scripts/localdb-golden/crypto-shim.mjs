// 黄金数据生成时替换 node:crypto：固定序列的 randomUUID / randomBytes，让 id 可复现。
// Rust 侧 Env（local_db.rs 的 TestEnv）按同一规则产生。
let u = 0;
let b = 0;
export function randomUUID() {
  return `uuid-${String(++u).padStart(4, '0')}`;
}
export function randomBytes(n) {
  b += 1;
  return Buffer.from(Array.from({ length: n }, (_, i) => (b * 7 + i * 13) & 255));
}
export default { randomUUID, randomBytes };
