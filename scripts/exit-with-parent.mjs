// 测试用 mock 服务的看门狗：父进程（测试 / 冒烟脚本 / cargo test）没了就自己退出，避免被超时或中断留下的孤儿进程一直占着端口。
// 父进程一旦退出，本进程会被 init 收养（ppid 变成 1）或 ppid 改变；每 2 秒查一次，定时器 unref 不阻止正常退出。
import process from 'node:process';

export function exitWithParent(intervalMs = 2000) {
  const parent = process.ppid;
  if (!parent || parent === 1) return; // 本来就没有父进程（手动启动）：不管
  const t = setInterval(() => {
    let alive = process.ppid === parent;
    if (alive) {
      try { process.kill(parent, 0); } catch (e) { alive = e.code === 'EPERM'; }
    }
    if (!alive) process.exit(0);
  }, intervalMs);
  t.unref();
}
