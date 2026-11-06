//! 第二层：调用确认。
//!
//! 第一层是服务端策略（[`crate::security::policy`]），**无论客户端支不支持确认都要生效**。
//! 这一层只回答：某件工具需要人工确认时，发起调用的客户端有没有能力把确认真正
//! 呈现给人。客户端不支持（例如纯 stdio、无 Elicitation 能力）就必须拒绝，
//! 绝不能「先跑再说」或自动放行 —— 客户端弹窗不能代替本地策略。
//!
//! P2 只做能力探测与拒绝，不实现 MCP Elicitation 交互流程（那属于 P3/P4）。

use crate::model::{ToolDefinition, ToolEffects};

/// 调用方声明的能力。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ClientCapabilities {
    /// 能否向用户展示确认请求并回传结果。
    pub confirmation: bool,
}

impl ClientCapabilities {
    /// 没有确认能力的客户端（首期 stdio 的实际情形）。
    pub fn headless() -> Self {
        Self::default()
    }
}

/// 这件工具在调用前需要什么。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Requirement {
    /// 服务端策略已足够，直接放行。
    None,
    /// 需要客户端把确认呈现给人。
    Confirmation,
}

/// 判定确认需求：TUI 的 `Caution`，或任何非只读副作用。
pub fn requirement(tool: &ToolDefinition, effects: ToolEffects) -> Requirement {
    if tool.danger.needs_confirm() || !effects.read_only() {
        Requirement::Confirmation
    } else {
        Requirement::None
    }
}

/// 客户端没有确认能力时，需要确认的动作一律拒绝。
pub fn check(requirement: Requirement, client: ClientCapabilities) -> bool {
    match requirement {
        Requirement::None => true,
        Requirement::Confirmation => client.confirmation,
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::{ClientCapabilities, Requirement, check, requirement};
    use crate::model::{Danger, Domain, RunMode, ToolDefinition, ToolEffects};

    fn tool(danger: Danger) -> ToolDefinition {
        ToolDefinition {
            mcp: None,
            id: String::from("test:demo"),
            name: String::from("演示"),
            provider: String::from("Test"),
            domain: Domain::System,
            tags: Vec::new(),
            summary: String::new(),
            input: None,
            output: None,
            features: None,
            requires: Vec::new(),
            missing_deps: Vec::new(),
            install_hint: None,
            pin: None,
            action: None,
            mode: RunMode::Capture,
            danger,
            path: PathBuf::from("/bin/echo"),
            ready: true,
        }
    }

    #[test]
    fn a_read_only_safe_tool_needs_no_client_capability() {
        let effect = ToolEffects {
            filesystem_read: true,
            ..ToolEffects::default()
        };
        assert_eq!(requirement(&tool(Danger::Safe), effect), Requirement::None);
        assert!(check(Requirement::None, ClientCapabilities::headless()));
    }

    #[test]
    fn caution_and_writes_need_confirmation() {
        let safe = tool(Danger::Safe);
        assert_eq!(
            requirement(
                &safe,
                ToolEffects {
                    filesystem_write: true,
                    ..ToolEffects::default()
                }
            ),
            Requirement::Confirmation
        );
        assert_eq!(
            requirement(&tool(Danger::Caution), ToolEffects::default()),
            Requirement::Confirmation
        );
    }

    /// 客户端不支持确认 → 拒绝，而不是自动放行。
    #[test]
    fn a_headless_client_cannot_skip_confirmation() {
        assert!(!check(
            Requirement::Confirmation,
            ClientCapabilities::headless()
        ));
        assert!(check(
            Requirement::Confirmation,
            ClientCapabilities { confirmation: true }
        ));
    }
}
