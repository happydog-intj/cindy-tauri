//! 历史视图投影（上游 maker-shared `projectHistoryView`）：把分页后的消息行折成「工作过程」分组。
//! 输入输出都是 `serde_json::Value`，键序与上游一致，可与上游输出逐字节对照。
//! 只移植投影实际用到的可达函数（分组算法、工具描述中的 MCP 产出路径、结果里的媒体 / 文件 / 卡片提取、交付正文判定）。
pub mod payload;
pub mod project;
pub mod text;

#[cfg(test)]
mod golden_tests;
