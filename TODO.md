# TODO

- [ ] 验证 `/` 命令补全已恢复（提交 a42a316）：打开 Cindy Tauri.app，输入 `/`，确认能看到 Pi 内置命令和 `~/.agents/skills` 里的 skill，不只是 `/goal`。
- [ ] 验证切换模型（提交 9e7b5a0）：切到 DeepSeek / Qwen 模型发一条消息，确认没有 "Engine switch failed" 和 404。
- [ ] pi-rs 未实现 `openai-responses`、`google-generative-ai`：切到 GPT / Gemini 类模型会报 `Unsupported api`（见 `pi-rs/src/agent.rs`）。
- [ ] 本地打包用 CI 固定的 cindy 版本（628f3217，worktree 在 `.cache/cindy`），不要用 `../cindy` 的最新 main，否则渲染层差异大，可能出现功能缺失。
