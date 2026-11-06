//! 工具服务层：**无终端**的目录、参数与执行能力。
//!
//! 存在的理由：同一件事（发现工具 → 填参数 → 生成 argv → 跑 → 收结果）现在有
//! 三套调用方，而且将来还会有第四套（MCP）：
//!
//! | 调用方 | 现在怎么跑 |
//! | --- | --- |
//! | TUI 捕获队列 | `App` 自己拼 `PendingJob`，主循环按帧收事件 |
//! | CLI / ecosystem | 自己的 runner（Recipe、队列） |
//! | 无终端调用（本模块） | 一次调用拿到结构化结果 |
//!
//! 本层不碰 Ratatui、不写父进程 stdout、不假设有 TTY。它只依赖
//! [`crate::model`]、[`crate::registry`] 与 [`crate::runtime`] 里那条已经验证过的
//! 捕获执行路径，所以 TUI 与将来的 MCP 服务端看到的是**同一条命令**。

pub mod argument;
pub mod catalog;
pub mod error;
pub mod execution;
pub mod gateway;
pub mod result;
