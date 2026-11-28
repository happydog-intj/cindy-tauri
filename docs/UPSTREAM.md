# 上游：Cindy

本项目的渲染层从 Cindy 项目（<https://github.com/makecindy/cindy>）复制而来，后续要持续跟进它的新提交。这份文档记录从哪个提交开始、本地改了什么，以及怎么同步。

## 基线

| 目录 | 上游提交 | 记录文件 | 导入时间 |
| --- | --- | --- | --- |
| `frontend/`（渲染层、共享包） | `628f32170e046cdb5869ac58ccac7b201c748495` | `frontend/.upstream-commit` | 2026-11-19（本仓库提交 `d465f58`） |
| `scripts/freeze/upstream-maker/`（maker-freeze 用的 translator 依赖闭包） | `d3a73394dbbd2adad4bebb2d82e7489d5b026f49` | `scripts/freeze/upstream-maker/.upstream-commit` | 2026-11-19 |

这两个提交是上游 `main` 上的提交，本仓库从它们开始分叉。同步脚本会把新的 `HEAD` 写回 `.upstream-commit`，所以升级后这张表要一起更新。

`frontend/` 只带上游 git 跟踪的文件，并排除了测试、移动端、`prototypes/` 和大部分 `docs/`（规则见 `scripts/sync-frontend.sh`）。

## 本地对上游文件的改动

同步时这些改动会被覆盖，需要手动带回来。下面七处是导入之后对上游文件的全部改动，行数很少。

| 文件 | 改动 | 本仓库提交 |
| --- | --- | --- |
| `frontend/package.json` | `pnpm.supportedArchitectures` 改成只装当前平台（上游列了全部 os/cpu，会装 8 份 claude 二进制）。`scripts/sync-frontend.sh` 已自动重做这一步，不用手动处理。 | `0c9130a` |
| `frontend/apps/desktop/src/renderer/features/bots/TeammatesSection.tsx` | 去掉侧边栏"添加伙伴"：删除 `BotCreateMenu` 的导入、标题右侧的 `<BotCreateMenu variant="header" />`，以及空状态分支（只剩列表分支）。 | `41ccdc0` |
| `frontend/apps/desktop/src/renderer/features/bots/BotCreateMenu.tsx` | 整个文件删除。 | `41ccdc0` |
| `frontend/apps/desktop/src/renderer/features/cc-agent/CCAgentSidebarUpper.tsx` | 侧边栏不再渲染"伙伴"段：删除 `TeammatesSection` 的导入和 `<TeammatesSection />`。`TeammatesSection.tsx` 本身保留，`/bots` 路由不动。 | `54ec05c` |
| `frontend/apps/desktop/src/renderer/components/sidebar/SidebarTopNav.tsx` | 去掉侧边栏的"更多"：展开态的"更多"行（`customizeRow` 的定义和三处使用）和收起态 rail 里的 `<SidebarNavigationMoreMenu>`。`SidebarNavigationMoreMenu`、`customizing` 状态等现在没有入口，故意没删，以减小和上游的差异。另外 `useSidebarNavigationEntries` 里的 `isVisible` 对内置的 `search`（搜索）和 `plugins`（插件）恒返回 false，侧边栏不再显示这两项（定义和偏好存储不动；有查询在跑或锁定项目搜索时，搜索行仍会按原逻辑出现）。 | `092982a`、本次提交 |
| `frontend/apps/desktop/src/renderer/features/cc-agent/sidebar/MainListScopeHeader.tsx` | 去掉任务列表段头的"全部任务 ▾"下拉：删除 `MachineSwitcherMenu` 的导入和渲染（左侧留一个空的 flex 容器，右侧的折叠 / 显示设置按钮不动）。`MachineSwitcherMenu.tsx` 文件保留，只是没人引用；去掉后侧边栏无法再切换"本机 / 远程设备"范围，保持默认的全部范围。 | 本次提交 |
| `frontend/apps/desktop/src/renderer/i18n/index.ts`、`main-entry.tsx`、`recording-preview-entry.tsx`、`region-screenshot-entry.tsx`、`region-screenshot-pin-entry.tsx`、`remote-desktop-viewer-entry.tsx`、`resource-usage-entry.tsx`、`sidebar-window-entry.tsx`、`lib/deviceName.ts` | 翻译按需加载：上游把 12 种语言的 JSON（共约 13 MB）静态导入，首屏要先解析约 11 MB 的 JS。改成每种语言一个动态 `import()` 加载器（`localeLoaders`），`init` 时不带资源，新增导出 `ensureLocaleLoaded` 和 `i18nReady`（当前语言及其回退语言加载完成），并包装 `i18n.changeLanguage` 使其先加载再切换。各入口在首次渲染前 await `i18nReady`（`main-entry.tsx` 在 async 入口开头，其余入口把 `render` 包进 `i18nReady.then`，没有重排缩进）。`deviceName.ts` 不再导入整份 `en/common.json`，改用内联常量 `UNKNOWN_DEVICE_EN`（值与 `bots.cloud.unknownDevice` 一致）。同步上游后要重做；上游若新增入口文件或新增静态导入 locale JSON 的地方，也要同样处理。 | 本次提交 |

同步后检查：上游如果又改了 `TeammatesSection.tsx`，用 `git diff` 看冲突，再重做上表的删除。`BotCreateMenu.tsx` 会被同步重新带回来，需要再删一次。

`useCCSessions.ts` 的诊断日志已经还原（`613324c`），现在和上游一致。

## 本地新增的目录

下面这些不来自上游，同步不会碰：`src-tauri/`、`pi-rs/`、`src/`（shim、ui、shared）、`scripts/`（除 `freeze/upstream-maker`）、`resources/`、`docs/`。

## 跟进上游的步骤

```sh
# 1. 准备上游仓库（本机没有，需要自己克隆）
git clone https://github.com/makecindy/cindy.git ../cindy
cd ../cindy && git log --oneline 628f32170e..HEAD    # 看有哪些新提交

# 2. 同步渲染层，再同步 maker-freeze 的依赖闭包
cd ../cindy-tauri
scripts/sync-frontend.sh ../cindy
scripts/sync-upstream-maker.sh ../cindy

# 3. 看哪些文件变了，重做上面"本地对上游文件的改动"
git status --short frontend | head
git diff frontend/apps/desktop/src/renderer/features/bots/TeammatesSection.tsx

# 4. 验证
node scripts/build-frontend.mjs
cd src-tauri && cargo test --bin cindy-tauri
```

同步脚本是整目录覆盖，不会合并。所以每次同步前先确认工作区干净，同步后用 `git diff` 区分"上游新增的变化"和"本地改动被覆盖"。

## 需要重点盯的上游变化

Tauri 版用 Rust 重写了 Electron 主进程，渲染层通过 IPC 调用它。上游改这些地方时，Rust 侧要跟着改：

- IPC 通道名和参数：`frontend/apps/desktop/src/preload/preload.ts`、`frontend/apps/desktop/src/main/maker-ipc/channels.ts`。新通道在 Tauri 版里会返回 `[NOT_IMPLEMENTED]`，看 `cindy.log` 里的 `[ipc] invoke ... failed` 就能发现。
- 本地数据库结构和 `local-db:*` 行为：对应 `src-tauri/src/local_db/`，有黄金数据测试 `local_db_golden.json`，由 `scripts/gen-localdb-golden.mjs` 用上游 JS 实现生成。
- maker-core（会话、事件、交互）：对应 `pi-rs/` 和 `src-tauri/src/pi_session_core.rs`，差距清单见 `docs/pi-rs-maker-core-gap.md`，上游有重大变更后要刷新。
