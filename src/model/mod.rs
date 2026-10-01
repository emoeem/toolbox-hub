//! Provider 无关的数据模型。
//!
//! 这一层刻意不依赖 ratatui：域和工具定义都是纯数据，配色、布局一律留给 [`crate::ui`]。
//! FFTools 的「转码 / 字幕 / 分析」之类概念在这里只作为通用 `tags` 存在，
//! 不允许反向污染成 UI 的硬编码分支。

mod argument;
mod domain;
mod tool;

pub use argument::{Action, ArgKind, ArgPlacement, Argument, ArgumentValues, Choice};
pub use domain::Domain;
pub use tool::{Danger, RunMode, ToolDefinition};
