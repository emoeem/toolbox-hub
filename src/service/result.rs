//! 一次无终端执行的结构化结果。
//!
//! 与 [`crate::runtime::Captured`] 的区别：`Captured` 是**界面**要的东西
//! （中文标签、给人看的命令串），这里是**调用方**要的东西 —— 稳定的 id、
//! 退出码、是否超时 / 取消 / 截断，全部是字段，不靠解析提示文字。

use std::time::Duration;

/// 一次执行的结果。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExecutionResult {
    /// 目录里的工具 id。
    pub tool_id: String,
    /// 展示名。
    pub tool_name: String,
    /// 真正跑的命令（给人看的一行，**已含程序名**）。
    pub command: String,
    pub stdout: String,
    pub stderr: String,
    /// 退出码；被信号杀掉时为 `None`。
    pub exit_code: Option<i32>,
    /// 是否算成功（按动作声明的 `ok_exit_codes` 判断，而不是只看 0）。
    pub success: bool,
    /// 是否被取消（SIGTERM / SIGKILL）。
    pub cancelled: bool,
    /// 是否因为超时而终止。
    pub timed_out: bool,
    /// 输出是否被截断（任一条流超过上限）。
    pub truncated: bool,
    pub elapsed: Duration,
}

impl ExecutionResult {
    /// 一行摘要，适合状态栏 / 日志。
    pub fn summary(&self) -> String {
        let outcome = if self.timed_out {
            String::from("超时终止")
        } else if self.cancelled {
            String::from("已取消")
        } else {
            match self.exit_code {
                Some(0) => String::from("退出码 0"),
                Some(code) => format!("退出码 {code}"),
                None => String::from("被信号终止"),
            }
        };
        let truncated = if self.truncated {
            " · 输出已截断"
        } else {
            ""
        };
        format!(
            "{} · {:.2}s{truncated}",
            outcome,
            self.elapsed.as_secs_f64()
        )
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::ExecutionResult;

    fn result() -> ExecutionResult {
        ExecutionResult {
            tool_id: String::from("manifest:jq-query"),
            tool_name: String::from("jq 查询"),
            command: String::from("/usr/bin/jq . a.json"),
            stdout: String::from("{}\n"),
            stderr: String::new(),
            exit_code: Some(0),
            success: true,
            cancelled: false,
            timed_out: false,
            truncated: false,
            elapsed: Duration::from_millis(20),
        }
    }

    #[test]
    fn summary_tells_timeout_cancel_and_truncation_apart() {
        assert_eq!(result().summary(), "退出码 0 · 0.02s");

        let mut timed_out = result();
        timed_out.timed_out = true;
        timed_out.success = false;
        assert_eq!(timed_out.summary(), "超时终止 · 0.02s");

        let mut cancelled = result();
        cancelled.cancelled = true;
        cancelled.success = false;
        assert_eq!(cancelled.summary(), "已取消 · 0.02s");

        let mut truncated = result();
        truncated.truncated = true;
        assert_eq!(truncated.summary(), "退出码 0 · 0.02s · 输出已截断");
    }

    #[test]
    fn a_signal_death_has_no_exit_code_to_show() {
        let mut killed = result();
        killed.exit_code = None;
        killed.success = false;
        assert!(killed.summary().starts_with("被信号终止"));
    }
}
