# pi-rs 接管 maker-core 能力面：差距清单

> 目标：让随包的 pi-rs（Rust）吃掉 sidecar 里 `@cindy/maker-core` 的能力面，使 Node sidecar（maker 桥 + persist + goal）可以整体退役。
> 这份清单按「迁移一块就删一块 sidecar 代码」组织；每块的验收标准是现有 e2e 套件保持全绿。
> 生成时点：pi-rs 0.1.0（rpc 模式）；maker-core 以 cindy 上游 main 为基准（非测试代码 ~10.6 万行）。

## 1. 现状盘点

### pi-rs 已有（rpc 模式，src/rpc.rs 386 行 + session.rs 283 行）
- 会话文件：JSONL 追加 / 分支（branch / clone / fork / navigate / tree / stats）
- 回合：`prompt` / `steer` / `follow_up` / `abort`；压缩（compact）
- 模型 / thinking 级别切换；provider 块（anthropic/openai 协议实现）
- 工具面（tools.rs）与权限（perm.rs）；扩展 UI 事件（extension_ui_request）

### maker-core 需要接管的面（按依赖层次）

| 层 | 模块 | 非测试行数 | 职责 |
|---|---|---|---|
| 注册中心 | `maker.ts` | 2,012 | 会话注册表、agent 路由、生命周期 hooks、close facts、shutdown report |
| 会话编排 | `session.ts` | 3,990 | 回合状态机、权限模式、交互请求（审批）、优雅停止、turn stall 检测、vision bridge、async user questions |
| 发送语义 | `session-send-outcome.ts` / `session-send-terminal.ts` | ~200 | dispatch outcome 断言 / 终端内发送 |
| agent 适配器 | `agents/pi`（含 translator、rpc-client、subagent）、`agents/codex`、`agents/claude-code` | 16,308 + 10,229 + 8,592 + … | 三家运行时的协议适配；SSH-bridged 远程 transport |
| 共享策略 | `agents/shared/*`（fast-mode、auto-review、review-read-scope、sensitive-credential-paths、capability-route-support） | ~9,000 | fast-mode 策略、自动评审、凭据路径保护 |
| Pi 专项 | `agents/pi/cache-*`、`skill-*`、`project-trust`、`customization-scanner`、`session-tree`、`jsonl-scan`、`windows-git-path`、`context-*`、`cindy-bridge-source`、`subagent-*` | ~18,000 | 缓存兼容/恢复、技能激活、项目信任、上下文估算与诊断、扩展源码注入 |
| 记忆 | `memory/*`（manager / storage / fts / scope-resolver / flush-controller / digest / review） | ~8,000 | 记忆摘要、FTS、作用域解析、落盘 |
| 事件模型 | `types/events.ts` 等 | ~2,000 | maker:event 形状（persist.mjs 与 renderer 都依赖） |
| 接口契约 | `interfaces/*`（session-storage / auth-adapter / mcp-provider / runtime-config / logger） | ~1,000 | host 注入点定义 |

### sidecar 胶水（退役前必须找到新宿主）
- `maker-host.mjs`（205 行）：装配 `Maker + PiAgent`，实现 AgentDeps（网关 baseUrl 指向本地 forwarder、密钥注入、秘密环境名清单）
- `maker-ipc.mjs`（874 行）：35 个 `maker:*` 通道 + `maker:event` 推送 + 会话落库挂钩
- `persist.mjs`（515 行）：maker:event → messages 表翻译（**只懂 maker-core 的事件形状**）
- `goal.mjs`（560 行）：/goal 自主续跑（拿 db.sql 查目标状态、按库里模型重建会话）
- `local-db/*`（1,423 行）：sessions/messages 持久化 + renderer 通道（事件翻译部分与 maker-core 绑定）

## 2. 差距清单（按迁移批次排序，P0 先行）

| # | 能力块 | pi-rs 现状 | 差距 | 优先级 | 验收 |
|---|---|---|---|---|---|
| 1 | **事件模型对齐** | **🟢 Rust 翻译层已落地并冻结**：pi-rs `src/maker.rs`（`--replay-maker`）实现了 translator 的回合子集（agent_start/turn_start/message_start/update/end/tool_execution_*/turn_end/agent_end/agent_settled → text/thinking/status/tool_use/tool_result_full/tool_result/done/account_usage），含 token 速度计（calibrated-token-speed 的时钟/backlog/publishDue 节流）与 usage 结算。验收 = `scripts/maker-freeze.mjs`：同一帧流 TS translator（esbuild 重放 cindy 仓库源码）与 Rust 产出逐字段全等（differing=0）。未移植块（auto_retry/compaction/host-abort/redacted thinking/subagent）在帧到达时逐块补，与 translator.ts case 一一对应 | **P0** | **金标准已就绪**：`scripts/event-freeze.mjs`（端到端 28 事件）+ `scripts/maker-freeze.mjs`（双产出 24 事件全等）。新增事件类型时用 `--update` 重新生成并 review | session-e2e / model-switch-e2e 不改断言通过 |
| 2 | **审批交互（permission mode / interaction request）** | **线协议已冻结**：pi-rs perm.rs 的 ask 门控走扩展 UI 子协议，已在 `pi-rs/baseline/permission-test.mjs` 锁定（24 断言）：ask+allow 写入 / user-deny 拒绝语进工具结果 / cancelled 兜底拒绝语 / bypass 直通（但 `cindy:turn-change-capture` 快照同步点仍需宿主应答）/ 控制面写入绕过 bypass 也要问（controlPlaneWrite + resolvedWritePath）/ 无权限文件 fail-closed | 宿主侧 Rust 化（pending 交互表 / dismissal / async 提问）待阶段 B；maker-ipc 的 `maker:resolve-interaction` / `get-pending-interactions` 由 Session 语义承接 | **P0** | session-e2e 审批步骤 |
<!-- #2 宿主侧：src-tauri/src/interactions.rs 已移植待处理交互表（register/resolve/cancel_session/expire/snapshot，6 个测试，语义对齐 maker-ipc 的 settleInteraction）。尚未接线：pi-rs 的 extension_ui_request ↔ maker:interaction-request/-dismissed、maker:resolve-interaction、maker:get-pending-interactions 仍由 sidecar 应答。 -->
| 3 | **会话落库契约（session-storage）** | 无 | Rust 直接写 SQLite（rusqlite），实现 `interfaces/session-storage` 语义：sessions/messages/right-sidebar-tabs/goal 状态；persist.mjs 的翻译随事件模型一并入 Rust | **P0** 🟡 存储层已落地：Rust 持有 SQLite（`localdb.rs`，rusqlite），sidecar 经 127.0.0.1 行协议同步代理（`local-db/engine.mjs`，无宿主时回退 node:sqlite）。业务 SQL 未动；`desktop-import` 仍直连 node:sqlite（它读的是桌面版外部库的一次性临时快照，不碰本应用的库；等去掉 Node 时再迁，需给引擎加只读打开和 ATTACH）；persist.mjs 的事件→落库翻译仍在 sidecar | local-db:* 通道行为不变（settings/model-switch e2e）；`scripts/db-engine-test.mjs` 15/15；release smoke 日志可见 Rust 引擎打开两个库 |
| 4 | **turn 生命周期观测 + stall 检测** | 无 | `SessionTurnLifecycleObserver`、turn-stall（超时判定）；中断恢复（interrupted-pending / resume-candidates 通道） | P1 | session-e2e 中断恢复 |
| 5 | **Maker 注册中心 + 生命周期 hooks** | 无（pi-rs 单会话进程） | 多会话注册表、close facts、shutdown report、before-start context；`maker:create-session/close/list-active/switch-session-agent` | P1 | session/model-switch e2e |
| 6 | **快速模式 / 能力路由** | 无 | `fast-mode-policy`、`capability-routing`（模型能力→路由决策） | P1 | model-switch e2e（alt-a 热切） |
| 7 | **网关凭据注入** | pi-rs 已支持 `apiKeyFile`（本次新增）：provider 块可指向 key 文件，每次请求现读（anthropic.rs/openai.rs 的 `api_key()` 本就是逐请求调用），父进程改写文件即轮换，无需重启会话；模型级可覆盖 provider 级，`to_json` 不泄 key | 缺上游配合：cindy 网关块由 maker-core 写入（占位 key + baseUrl 指向 JS 转发器），要直连需上游改写为 `apiKeyFile`；或等阶段 B 由 Rust 宿主直接引导会话（自行写 models.json，绕开上游）。关键事实：pi-rs 的 tools.rs 已把 `CINDY_PI_KEY_*` / `CINDY_PI_API_KEY` 屏蔽在 bash 子进程外，Rust 侧具备直持凭据的安全基础——**终态下转发器被删除而非移植**（pi-rs 进程内直持真实 key，凭据不落 env 也不进子进程） | **P0**（#1 之前就需要，maker-host 现在做的事） | auth-e2e / model-switch e2e |
| 8 | **Goal（自主续跑）** | 无 | 目标状态查询（Rust 侧读库）、按库里模型/工作目录重建会话 | P1 | goal-e2e |
| 9 | **记忆（memory）** | 无 | manager/storage/FTS/scope；renderer 有 `maker:memory:*` 通道 | P2 | settings-e2e（memory 通道） |
| 10 | **subagent** | 无 | `pi-subagent-runs`、cindy-subagent-source 注入、进度事件 | P2 | 冒烟无覆盖，需新增 e2e |
| 11 | **多 agent（codex / claude-code）** | 无（仅 pi） | 另两家适配器 + translator；`switch-session-agent` / `list-available-agents` | P3（Tauri 版只装配 Pi，可降级为「明确拒绝」） | maker-ipc 返回 NOT_IMPLEMENTED 语义保持 |
| 12 | **SSH-bridged 远程会话 / device-hosted-guest** | 无 | 远程主机 / 设备会话（Tauri 版已在 create 时显式拒绝） | P4 | 维持 NOT_IMPLEMENTED |
| 13 | **vision bridge / async user questions / voice-call** | 无 | 附件视觉桥、异步提问、语音通话 | P2 | 新增针对性 e2e |
| 14 | **Pi 专项杂项** | 部分（session tree/jsonl 已有） | cache 兼容/恢复、skill 激活、project trust、customization scanner、windows git-path、context doctor | P2 | 逐块单测迁移 |

## 3. 依赖关系与建议路线

```
阶段 A（P0，解锁 Node 侧 persist/goal 停止增长）
  #7 网关凭据注入 → #1 事件模型 → #2 审批协议 → #3 会话落库契约
  验收：session-e2e / model-switch-e2e / goal-e2e 全绿，maker-ipc 的落库与审批挂钩切到 Rust

阶段 B（P1，maker-ipc 退役）
  #4 turn 观测 + #5 注册中心 + #6 能力路由 + #8 goal
  验收：sidecar 的 maker-host/maker-ipc/goal 删除，35 个 maker:* 通道由 Rust 直答

阶段 C（P2，外围）
  #9 记忆、#10 subagent、#13 vision/voice、#14 杂项
  验收：对应通道逐个迁移 + 新增 e2e

阶段 D（收尾）
  #11/#12 明确降级语义；local-db 剩余查询（sessions 列表/历史渲染）全量入 Rust；
  删除 cindy-node 与 sidecar.cjs；externalBin / 资源打包 / fetch-node 清理
```

## 4. 风险与约定

- **事件形状是兼容性锚点**：persist.mjs 与 renderer 都按 `types/events.ts` 消费；Rust 侧先写「形状冻结测试」（对同一输入比对 Rust/TS 产出），再动 renderer。
- **审批协议无 e2e 之外的金标准**：以 desktop 版行为为基准，`maker:resolve-interaction` 的 payload/decision 枚举必须逐字对齐。
- **迁移期双写不引入**：每块要么 Rust 接管、要么留在 sidecar，禁止两处同时写库/发事件。
- **凭据供给路径已打通半条**：pi-rs `apiKeyFile`（请求时现读 + 轮换）已落地并有单测；sidecar 侧仍走 JS 转发器（注册/轮换语义一致），切换时机在阶段 B（Rust 宿主直接写 models.json）。切由上游改写 cindy 块则需 cindy 仓库配合。
- **SSH-bridged transport**：`agents/pi/transport.ts` 是接口抽象，Rust 侧用 trait 预留，远程会话维持显式拒绝（与今天一致）。
- maker-core 跟随 cindy 上游演进，本清单需在上游重大变更后刷新（重点盯 `session.ts` 与 `types/events.ts` 的 diff）。

## 附：Rust 会话宿主进度（宿主侧集成）
已落地且有测试（`src-tauri`，29 个 cargo 测试）：
- `interactions.rs`：待处理交互表（语义对齐 maker-ipc 的 settleInteraction）。
- `pi_session_core.rs`：pi-rs 帧分流（turn-change-capture 即答 / permission 登记 / 其余走翻译器），决策 → `extension_ui_response`，中止/超时/退出一律拒绝收尾，无法解析的权限请求 fail-closed。
- `pi_proc.rs`：`pi --mode rpc` 子进程包装（读线程、带锁 stdin、退出收尾）；PI_RS_BIN + MOCK_OPENAI 存在时对真实 pi-rs 跑「事件流」与「权限门允许/拒绝」两个端到端。
- 翻译器与 pi-rs 共用 `pi-rs/src/maker.rs`（`#[path]`），maker-freeze 仍 differing=0。
- `input_queue.rs`：输入队列与 turn 生命周期（clientId 去重、token 递增、仅当前 token 的前台终态事件收尾、stop/pause/resume/remove、派发失败只清自己的回合）。`SessionCore` 在翻译后的事件上盖 `turnAttemptToken`（session.ts 同规则），收尾后自动派发下一条。
- `maker_registry.rs`：多会话注册表与 `maker:*` 通道分发。`owns()` 列出接管的通道：`input:*`、`get-pending-interactions`、`resolve-interaction`、`abort-session`、`close-session`、`session-in-turn`、`list-active`、`any-session-in-turn`。会话创建经 `SessionFactory` 注入，6 个单测用假工厂覆盖懒创建、会话隔离、关闭后重建、错误码透传。**尚未接入 main.rs 的 ipc_invoke。**

切换风险（接线前必须先解决，否则双写）：renderer 先调 `maker:create-session`（仍在 sidecar）再调 `maker:input:enqueue`（Rust），会得到两个 pi-rs 进程。要切换必须同一批迁走：create-session / send / set-model / set-permission-mode 等会话通道、`persistMakerEvent` 落库、goal 的 userTurnSettled 钩子、`SessionFactory` 的 models.json 写入与 key 获取（经 sidecar.call 取 model-access）。
- **#7 网关直连已落地**：pi-rs 新增 `apiKeyHeaders`（`models.rs::auth_headers`，openai/anthropic 两个协议都走它），与 `apiKeyFile` 配合即可替代 `gateway-forwarder.mjs`：两个头同时带真实 key、轮换无需重启、空 key 不发空凭据。`pi-rs/baseline/gateway-key-test.mjs` 8 项通过。转发器暂未删除（现网关会话仍经它）；删除要等会话主链路切到 Rust。
- `pi_launch.rs`：写 `models.json`（provider 块只引用 `apiKeyFile`）、0600 key 文件、权限档文件，拼 `--mode rpc --extension cindy-bridge.ts`（不带 `--no-approve`）参数与环境；key 不进 models.json / 命令行 / 环境变量，关闭会话删运行目录。5 个单测。
- `host.resolve-launch`（Rust 专用，webview 调不了；已由 `src-tauri/src/model_config/manager.rs` 的 `build_gateway_launch` / `build_custom_launch` 回答）：网关 baseUrl 规则（anthropic 用根、openai 系补 /v1）、模型描述符、未登录 / 无 key / 网关不认该模型一律 fail-closed。`cargo test model_config::view_tests::launch` 5 组断言覆盖。
- sidecar 新增 Rust 专用回调 `host.persist-event / persist-user-message / persist-interaction-request / persist-interaction-resolved / upsert-session / session-resume-info / turn-settled / sync-sdk-session-id`：落库与 goal 钩子留在 sidecar（db 与 goal 绑定 maker-core 事件模型），Rust 经它们同步回调。
- `maker_host.rs`：`HostSink`（先落库再推 renderer，落库失败不丢事件；回合收尾通知 goal；退出清运行目录并通知注册表）与 `HostFactory`（续接信息 → `host.resolve-launch` → `pi_launch::prepare` → `PiSession::spawn` → 行记录；任何一步失败都不留运行目录 / key 文件）。8 个单测。
- `maker_registry.rs` 现接管 14 条通道，新增 `create-session`、`send`（走同一队列）、`set-permission-mode`（改写权限档文件，pi-rs 现读）。

**接线状态：Rust 侧已齐备，但 main.rs 尚未把它们接进 `ipc_invoke`，运行中的应用仍走 sidecar。** 全部 57 个 cargo 测试 + session / model-switch / goal e2e 当前都绿。
接线前仍要一并解决（否则 renderer 会看到半迁移状态）：
1. `maker:set-model` / `set-effort` / `set-fast-mode` / `set-thinking-enabled`：热切、回合中延期（pendingSelection）、重启后按新模型重建。当前仍在 sidecar，会话在 Rust 后这些调用找不到 maker-core 会话。
2. `maker:get-capabilities`、`maker:steer`、`maker:input:compact` 等周边通道对 Rust 会话的支持。
3. 卡死检测（零事件看门狗）。
4. 删除 `gateway-forwarder.mjs`（转发器在无 Node 会话后才能删）。
5. model-switch e2e 与 goal e2e 针对 Rust 会话重跑（现在它们验证的是 sidecar 路径）。、`maker:*` 通道切换、写 models.json（`apiKeyFile`）与网关鉴权取 key、goal 续跑（#8）。

### 接线状态（无开关、无回退；会话只在 Rust）
- `maker_wiring.rs` 把 `MakerRegistry` 接进 `ipc_invoke`；pi-rs 找不到时回退 sidecar。退出时 `shutdown_all`。
- Rust 已接管 18 条通道：create-session / send / set-model / set-effort / set-fast-mode / set-thinking-enabled / set-permission-mode / input:* / get-pending-interactions / resolve-interaction / abort / close / list-active / session-in-turn / any-session-in-turn。
- 换模型 = 重建 pi-rs 进程（模型表启动时加载），历史靠 `switch_session` 续接；pi-rs 没有 `--session` 参数。
- goal 与更新忙闲判断经 sidecar 桥：`host.activate`、`host.turn-state`、`rust.listeners`，goal 发送 / 中止走 `native maker.enqueue / maker.abort`。
- 验证：会话冒烟在开关开 / 关下都通过；65 个 cargo 测试通过。
- `rust_session_e2e.rs`：真实 sidecar + 真实 `HostFactory` / `MakerRegistry` + pi-rs + mock 模型，覆盖普通回合与落库、换模型重建并续接历史、goal 三轮自主续跑直到 complete（不多跑、不写 user 行）、重启后同一数据目录续接。它先暴露了一个真实缺陷：Rust 回合事件没有 `turnOrigin`，goal 控制器无法识别自己发起的回合，永远停在 active。已修（`InputQueue.active_origin`，只在回合进行中盖章）。
- 仍未用 Rust 会话覆盖的 e2e 场景：goal 的 blocked / 轮数上限 / 暂停恢复 / `/goal` 命令（这些逻辑在 sidecar 的 goal 控制器里，与会话来源无关，sidecar 路径 e2e 已覆盖）；model-switch 的回合中延期与 effort 切换。
- 未做：get-capabilities / steer / compact / plan-mode / session-tree 等周边通道、卡死检测、删除 gateway-forwarder、maker.rs 未翻译事件。

### 看门狗与错误 / 重试翻译（本轮）
- **卡死检测**（`pi_session_core::check_stall`，`PiSession` 每 5 秒检查）：回合进行中、无待审批、无在跑工具、连续 180 秒无任何帧 → 向 pi-rs 发 `abort`，以终止性 `error`（`code: AGENT_STALLED`）收尾回合，队列里的下一条继续派发。在跑的工具和待审批都不算卡死。
- **`pi-rs/src/maker_errors.rs`**：错误分类与脱敏的移植（上下文溢出、网络类、过载 / capacity、stream-interrupted、`unsupported-request-option`、状态码 / 用量上限信号、`redactSensitiveText`）。regex crate 没有先行断言，脱敏的幂等改成手动匹配。6 个单测。
- **`maker.rs` 新增翻译**：`auto_retry_start`（过载第 2 次起透出 `(auto-retry N/M)`，网络类透出 `Reconnecting... N/M`，其余静默）、`auto_retry_end`（成功清错误；失败以终止 error + done(failed) 收尾，重试预算用尽且是网络类时带 `pi-gateway-drop`）、`message_end` 的瞬时失败登记 `pendingAssistantError`（此前模型报错的回合会被当成正常完成）、`agent_settled` 的 `terminalAssistantErrorEmitted`、`usage` 的 `durationMs` / `reasoningTokens` 条件。
- **`maker-freeze` 手写场景**：`scripts/fixtures/maker-freeze/scenarios/*.jsonl`（request-error / network-retry-then-ok / overload-retry / retry-exhausted / context-overflow），TS 与 Rust 对同一帧流各出一份比对，全部 differing=0。
- **`compaction_start` / `compaction_end`**：开始时锁存 turnScope（空闲 = `background`，回合内 = 无），end 复用锁存值；发 `compact_boundary`（`trigger`、`preTokens`、`postTokens`）；手动压缩无论成功失败都要收口 `Done`，否则圆环卡 running；回合内压缩成功后发带 `contextSnapshotAuthoritative` 的 `Working…` 发布新水位。场景：`compaction-auto` / `compaction-auto-failed` / `compaction-manual-idle` / `compaction-manual-failed`，differing=0。
- **redacted thinking**：`partial` 标记或旧版占位文本 `[Reasoning redacted]` 都认；redacted 块不发 `start`，只发 `redacted`。场景 `thinking-*`（4 个）。
- **网页引用 / stop-token 剥离**（`maker_text.rs`，5 个单测）：流式增量按 contentIndex 各自缓冲，stop token 与未闭合引用先扣住，确认是正文后再原样放出；终稿同样剥离。场景 `text-*`（6 个）。
- **宿主主动 abort 登记**：`MakerCtx::mark_host_abort_requested`，在发 abort RPC 之前登记，因为 Pi 的 `aborted` 终态可能先于回执到达。用户 Stop 与看门狗的 abort 都走它；`agent_settled` 与 `auto_retry_end` 据此收成 `cancelled`，且压过输出上限。`pending_host_turn_start` 让 prompt 发出到 agent_start 之间的 Stop 指向下一代回合。场景 `host-abort*`（3 个）、`pi-aborted-request-no-host`；核心层测试覆盖「用户 Stop → cancelled，无 Stop 的同样帧 → failed」。
- **silentStop**：回合正常完成但没有可见回复时 `done.data.silentStop=true`，宿主用它做有界续跑保护。
- **不移植**：subagent（`agent_task_update`）与 cindy cache recovery。pi-rs 没有 `subagent` 工具，也不产生 `cindyCacheRecovery*` 字段，这两条路径在 pi-rs 上不会触发。
- **更正**：此前把 `get-capabilities` / `steer` / `compact` / `plan-mode` / `session-tree` 列为「Rust 会话缺失的周边通道」不准确。sidecar 自己对 `steer` / `compact` / `plan-mode` / `session-tree` 也返回 `[NOT_IMPLEMENTED]`，开关打开不会让它们退化；`get-capabilities`、`agent:status`、`list-*`、`goal:*` 等不属于会话进程，继续由 sidecar 处理，Rust 会话不影响它们。

### Rust 会话 e2e 新增覆盖（`rust_sessions_cover_goal_terminal_states_pause_resume_and_effort`）
- goal `blocked`：只跑一轮后停住。
- goal 轮数上限：`maxTurns=2` 时第 2 轮后进入 `budgetLimited`，`lastReason` 含 `max turns`。
- goal 暂停 / 恢复：暂停期间 `turnsUsed` 不增长，恢复后接着数轮并到 `complete`。
- effort：`set-effort` 落库（`effort=high`），之后会话照常回复。
- 回合进行中的 `set-model`：返回 `deferred:true`，在途回合不被打断；回合收尾时由钩子关掉旧进程、按新选择重建，并把队列里待派发的消息迁到新会话（排队消息用新模型，与 sidecar 同口径）。e2e `rust_sessions_defer_model_switch_until_the_running_turn_settles` 用 mock 的 `MOCK_SLOW`（约 4 秒的慢回复）制造回合窗口，断言：deferred、回合中同一进程、慢回复完整落历史、第二轮跑在新进程（`list-active` 带 `pid`）、两条用户消息都落库。
- 修了一个真实缺陷：此前延期的换模型只打了 `stale` 标记，要等「下一次取会话」才重建；若回合收尾时队列里还有消息，旧进程会先把它派发出去，新模型要晚一轮才生效。

## Phase 2 进展（sidecar 去 Node 的第一批）

已迁到 Rust（cargo 单测覆盖，不再保留 Node 对照）：
- `override_file.rs` + `maker_settings.rs`：maker 侧覆盖项设置 35 个通道。
- `misc_settings.rs`：Pi 扩展包 / Pi 内核 / 用量历史 / Bot 模型链 / 辅助模型。上游的 modelRoute、botModelChain、辅助模型引用、catalogModelPin 规则在此重写，之后需手动对齐。
- `agent_catalog.rs`：能力表、可用 agent、agent 状态、斜杠命令、skills 扫描（`~/.agents/skills` 与项目 `.pi/skills`、`.agents/skills`）。skill 名排序用 `locale_compare` 近似 JS `localeCompare`，由 Node 实测的顺序校准。
- 因此 sidecar 不再加载 `@cindy/maker-core`：`sidecar.cjs` 7.4 MB → 4.1 MB，空闲 RSS 165 MB → 132 MB（同一台机器、同样的启动与探测流程）。

仍在 sidecar（约 6000 行）：auth、model-access、model-config、secret-store、device-link、local-db 落库、browser-panel、owner / boot-stubs。goal、desktop-import、desktop-shell、app-settings、voice-input-data 已迁 Rust（`goal_*.rs`、`import_*.rs`、`desktop_shell.rs`、`app_settings.rs` + `appearance_core.rs` / `shortcuts_core.rs`、`voice_input.rs` + `voice_dict.rs` / `voice_settings.rs`；这些纯函数用 `scripts/gen-app-settings-data.mjs` 从上游代码导出的黄金样例核对（含 `voice_input_golden.json`，固定时钟与 id 下跑上游 voiceInputData.ts），上游改动后需重跑）。
