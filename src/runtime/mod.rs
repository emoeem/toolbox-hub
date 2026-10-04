//! 执行层：把工具交给终端运行。
//!
//! 这一层只依赖 [`crate::model`]，不认识 UI 状态，方便将来换成
//! 「后台任务队列」之类实现而不用改 app/ui。

mod exec;

pub use exec::{
    BrowsedBack, Captured, ExecReport, JobEvent, RunningJob, browse_directories, execute_action,
    execute_tools, parse_duration, probe_duration, run_captured, run_in_terminal, spawn_captured,
};
// `ui pick` 也要用这几个：找外部文件管理器、解析它写回来的临时文件。
// 复用同一份而不是在 components.rs 里另写一份 —— 解析规则只能有一个出处。
pub(crate) use exec::{default_argv, find_on_path, parse_chooser_file, parse_cwd_file};
