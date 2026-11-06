//! 服务层错误。
//!
//! 刻意不用 `anyhow` / `thiserror`：这一层的错误要**跨边界**（TUI、CLI、将来的
//! MCP）原样展示，所以既有稳定的分类，也有一句能直接给人看的话。

use std::fmt;

use crate::model::RunMode;

/// 服务层能给出的失败原因。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ServiceError {
    /// 工具 id 不在目录里。
    UnknownTool(String),
    /// 依赖缺失（`requires` 没满足）。
    NotReady { tool: String, missing: Vec<String> },
    /// 执行方式不适合无终端环境（`interactive` / `native`）。
    UnsupportedMode { tool: String, mode: &'static str },
    /// 这件工具没有动作定义，但调用方给了参数。
    UnexpectedArguments(String),
    /// 参数不合法（必填缺失、类型不对、未声明的键）。
    InvalidArguments(String),
    /// 安全策略拒绝：**子进程没有启动**。
    PermissionDenied {
        reason: &'static str,
        message: String,
    },
    /// 进程起不来、输出通道断开等执行期问题。
    Execution(String),
}

impl fmt::Display for ServiceError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ServiceError::UnknownTool(id) => write!(formatter, "没有这个工具：{id}"),
            ServiceError::NotReady { tool, missing } => write!(
                formatter,
                "{tool} 还缺依赖：{}",
                if missing.is_empty() {
                    String::from("未知")
                } else {
                    missing.join("、")
                }
            ),
            ServiceError::UnsupportedMode { tool, mode } => write!(
                formatter,
                "{tool} 的执行方式是 {mode}，需要一个终端；无终端环境只能跑 capture 动作"
            ),
            ServiceError::UnexpectedArguments(tool) => {
                write!(formatter, "{tool} 不接受参数")
            }
            ServiceError::InvalidArguments(message) => write!(formatter, "参数不合法：{message}"),
            ServiceError::PermissionDenied { reason, message } => {
                write!(formatter, "权限拒绝（{reason}）：{message}")
            }
            ServiceError::Execution(message) => write!(formatter, "执行失败：{message}"),
        }
    }
}

impl std::error::Error for ServiceError {}

/// 错误的稳定分类，方便调用方（尤其是 CLI / MCP）映射成状态码或协议错误。
impl ServiceError {
    pub fn kind(&self) -> &'static str {
        match self {
            ServiceError::UnknownTool(_) => "unknown_tool",
            ServiceError::NotReady { .. } => "not_ready",
            ServiceError::UnsupportedMode { .. } => "unsupported_mode",
            ServiceError::UnexpectedArguments(_) => "unexpected_arguments",
            ServiceError::InvalidArguments(_) => "invalid_arguments",
            ServiceError::PermissionDenied { .. } => "permission_denied",
            ServiceError::Execution(_) => "execution",
        }
    }
}

/// 执行方式的稳定名字（manifest 里那种写法）。
pub(crate) fn mode_name(mode: RunMode) -> &'static str {
    match mode {
        RunMode::Interactive => "interactive",
        RunMode::Capture => "capture",
        RunMode::Native => "native",
    }
}

#[cfg(test)]
mod tests {
    use super::ServiceError;

    #[test]
    fn errors_say_what_is_wrong_in_one_line() {
        let missing = ServiceError::NotReady {
            tool: String::from("jq 查询"),
            missing: vec![String::from("jq")],
        };
        assert_eq!(missing.to_string(), "jq 查询 还缺依赖：jq");
        assert_eq!(missing.kind(), "not_ready");

        let mode = ServiceError::UnsupportedMode {
            tool: String::from("btop"),
            mode: "interactive",
        };
        assert!(mode.to_string().contains("需要一个终端"));
        assert_eq!(mode.kind(), "unsupported_mode");
    }
}
