//! 执行层：把工具交给终端运行。
//!
//! 这一层只依赖 [`crate::model`]，不认识 UI 状态，方便将来换成
//! 「后台任务队列」之类实现而不用改 app/ui。

mod exec;

pub use exec::{
    BrowsedBack, Captured, ExecReport, JobEvent, RunningJob, browse_directories, execute_action,
    execute_tools, parse_duration, probe_duration, run_captured, spawn_captured,
};
