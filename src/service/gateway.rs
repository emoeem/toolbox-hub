//! 唯一的「AI 可调用入口」。
//!
//! P3 的 MCP Server、将来的 CLI JSON 都只跟 [`ToolGateway`] 打交道：
//!
//! ```text
//! tools/list → list()   只返回通过策略的工具
//! tools/call → call()   授权 → 构造 argv → 执行 → 输出限额 → 审计
//! ```
//!
//! 这里保证两件事：
//!
//! 1. **未授权工具绝对不会启动子进程**：策略判定完才 `execute`；
//! 2. stdout 永远不写诊断信息，审计单独落文件（MCP 的 stdout 只归协议）。

use std::{
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
};

use crate::{
    model::{ToolDefinition, TypedValues},
    security::{
        approval::ClientCapabilities,
        audit::{Auditor, redacted_arguments},
        policy::SafetyPolicy,
    },
    service::{
        catalog::ToolCatalog,
        error::ServiceError,
        execution::{ExecutionRequest, ProgressHook, execute_with_hooks},
        result::ExecutionResult,
    },
};

/// 工具网关。借用目录、策略、审计器，本身不持有工具数据的副本。
///
/// 并发名额来自调用方传入的**共享** [`ConcurrencyLimiter`]（P4-02）：同一个
/// 服务端里的同步调用与后台任务共用预算，超上限一律直接拒绝。
pub struct ToolGateway<'a> {
    catalog: &'a ToolCatalog,
    policy: &'a SafetyPolicy,
    auditor: &'a Auditor,
    client: ClientCapabilities,
    client_name: String,
    limiter: std::sync::Arc<ConcurrencyLimiter>,
}

impl<'a> ToolGateway<'a> {
    pub fn new(
        catalog: &'a ToolCatalog,
        policy: &'a SafetyPolicy,
        auditor: &'a Auditor,
        client: ClientCapabilities,
        client_name: impl Into<String>,
        limiter: std::sync::Arc<ConcurrencyLimiter>,
    ) -> Self {
        Self {
            catalog,
            policy,
            auditor,
            client,
            client_name: client_name.into(),
            limiter,
        }
    }

    /// 可调用工具列表：`Interactive` / `Native` / 未声明 / 未授权的一律不出现。
    pub fn list(&self) -> Vec<&'a ToolDefinition> {
        self.catalog
            .list()
            .iter()
            .filter(|tool| self.policy.authorize_for_list(tool, self.client).is_ok())
            .collect()
    }

    /// 调用一件工具。**所有拒绝路径都在启动进程之前。**
    pub fn call(
        &self,
        id: &str,
        values: &TypedValues,
        cwd: &Path,
    ) -> Result<ExecutionResult, ServiceError> {
        self.call_with_hooks(id, values, cwd, None, &mut |_| {})
    }

    /// 带取消旗标与进度钩子的调用（P4-01 / P4-03）：授权、名额、输出限额、
    /// 审计与 [`Self::call`] 完全一致，只是把两个钩子穿透到执行器。
    pub fn call_with_hooks(
        &self,
        id: &str,
        values: &TypedValues,
        cwd: &Path,
        cancel: Option<&Arc<AtomicBool>>,
        on_progress: ProgressHook<'_>,
    ) -> Result<ExecutionResult, ServiceError> {
        let Some(tool) = self.catalog.get(id) else {
            return Err(ServiceError::UnknownTool(id.to_string()));
        };
        let arguments = redacted_arguments(tool.action.as_ref(), values);

        let authorization = self
            .policy
            .authorize(tool, values, cwd, self.client)
            .map_err(|reason| {
                let code = reason.code();
                let message = reason.message(id);
                self.denied(id, code, message, arguments.clone())
            })?;

        let mut request = match ExecutionRequest::for_tool(tool, values, cwd) {
            Ok(request) => request,
            Err(error) => {
                // 构造 argv 失败（必填缺失、Choice 不合法）是**参数错误**，不是
                // 权限错误：审计记一条拒绝，但错误类型原样上抛 —— 调用方（MCP /
                // CLI）要能把 invalid_arguments 和 permission_denied 区分开。
                self.auditor
                    .denied(id, &self.client_name, error.kind(), arguments);
                return Err(error);
            }
        };
        // 策略统一决定超时，调用方不能直接放宽到 max_timeout_secs 之上。
        request = request.with_timeout(authorization.timeout);

        let _permit = self.limiter.acquire().ok_or_else(|| {
            self.denied(
                id,
                "busy",
                format!("并发已达上限 {}", self.limiter.max()),
                arguments.clone(),
            )
        })?;

        let mut result = execute_with_hooks(&request, cancel, on_progress)?;
        limit_output(&mut result, authorization.max_output_bytes);
        self.auditor.completed(
            id,
            &self.client_name,
            result.exit_code,
            result.elapsed.as_millis().min(u128::from(u64::MAX)) as u64,
            result.success,
            arguments,
        );
        Ok(result)
    }

    /// 记一条拒绝并返回稳定的错误。
    fn denied(
        &self,
        id: &str,
        reason: &'static str,
        message: String,
        arguments: std::collections::BTreeMap<String, String>,
    ) -> ServiceError {
        self.auditor
            .denied(id, &self.client_name, &message, arguments);
        ServiceError::PermissionDenied { reason, message }
    }
}

/// 把输出夹到策略允许的字节数，并明确标记 `truncated`。
fn limit_output(result: &mut ExecutionResult, max_bytes: usize) {
    if result.stdout.len() > max_bytes {
        result.stdout = truncate_utf8(&result.stdout, max_bytes);
        result.truncated = true;
    }
    if result.stderr.len() > max_bytes {
        result.stderr = truncate_utf8(&result.stderr, max_bytes);
        result.truncated = true;
    }
}

/// 按 UTF-8 边界截断：`String::truncate` 切在中文字符中间会 panic。
fn truncate_utf8(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_string();
    }
    let mut end = max.min(text.len());
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_string()
}

/// P2-07 / P4-02 并发控制：超上限直接拒绝（不排队），避免多个 Agent 一起把机器压满。
///
/// 整个服务端共享**一个**实例（MCP 层用 `Arc` 持有）：同步调用与后台任务共用同一份
/// 名额预算，上限读取走 `AtomicUsize`，`mcp.toml` 热更新（P4-04）时可以原地改上限。
pub struct ConcurrencyLimiter {
    active: AtomicUsize,
    max: AtomicUsize,
}

impl ConcurrencyLimiter {
    pub fn new(max: usize) -> Self {
        Self {
            active: AtomicUsize::new(0),
            max: AtomicUsize::new(max.max(1)),
        }
    }

    /// 当前上限（至少 1）。
    pub fn max(&self) -> usize {
        self.max.load(Ordering::SeqCst)
    }

    /// 热更新上限；至少 1。
    pub fn set_max(&self, max: usize) {
        self.max.store(max.max(1), Ordering::SeqCst);
    }

    /// 当前占用的名额数（诊断用）。
    pub fn active(&self) -> usize {
        self.active.load(Ordering::SeqCst)
    }

    pub fn acquire(&self) -> Option<Permit<'_>> {
        loop {
            let current = self.active.load(Ordering::SeqCst);
            if current >= self.max() {
                return None;
            }
            if self
                .active
                .compare_exchange(current, current + 1, Ordering::SeqCst, Ordering::SeqCst)
                .is_ok()
            {
                return Some(Permit(self));
            }
        }
    }
}

/// RAII：调用结束（哪怕提前 return / panic）都要把名额还回去。
pub struct Permit<'a>(&'a ConcurrencyLimiter);

impl Drop for Permit<'_> {
    fn drop(&mut self) {
        self.0.active.fetch_sub(1, Ordering::SeqCst);
    }
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        path::{Path, PathBuf},
        time::{SystemTime, UNIX_EPOCH},
    };

    use super::{ConcurrencyLimiter, ToolGateway, limit_output, truncate_utf8};
    use crate::{
        model::{
            Action, ArgKind, ArgPlacement, Argument, Danger, Domain, McpExposure, RunMode,
            ToolDefinition, TypedValues,
        },
        security::{
            approval::ClientCapabilities,
            audit::Auditor,
            policy::{McpConfig, SafetyPolicy},
        },
        service::{catalog::ToolCatalog, error::ServiceError},
    };

    fn temp_dir(tag: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("toolbox-hub-gateway-{tag}-{nanos}"));
        fs::create_dir_all(&dir).expect("mkdir");
        dir
    }

    fn tool(id: &str, sensitive: bool) -> ToolDefinition {
        ToolDefinition {
            mcp: Some(McpExposure::default()),
            id: id.to_string(),
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
            action: Some(Action {
                program: String::from("echo"),
                base_argv: Vec::new(),
                arguments: vec![Argument {
                    key: String::from("text"),
                    label: String::from("文本"),
                    kind: ArgKind::Text,
                    default: None,
                    choices: Vec::new(),
                    required: false,
                    flag: None,
                    flag_join: false,
                    placement: ArgPlacement::Trailing,
                    sensitive,
                    repeatable: false,
                    repeat_flag: false,
                    separator: String::from(","),
                    source: None,
                    dir_only: false,
                    help: None,
                }],
                duration_from: None,
                limit_from: None,
                foreach: None,
                ok_exit_codes: vec![0],
                allow_empty: true,
            }),
            mode: RunMode::Capture,
            danger: Danger::Safe,
            path: PathBuf::from("/bin/echo"),
            ready: true,
        }
    }

    fn config(extra: &str) -> McpConfig {
        let text = format!(
            "[mcp]\nenabled = true\n[exposure]\ndefault = \"deny\"\nallow = [\"test:echo\"]\ndeny = []\n{extra}"
        );
        toml::from_str(&text).expect("配置")
    }

    /// 把网关需要的几个借用对象绑在一起，避免测试里到处传生命周期。
    struct Fixture {
        catalog: ToolCatalog,
        policy: SafetyPolicy,
        auditor: Auditor,
        dir: PathBuf,
    }

    impl Fixture {
        fn new(tools: Vec<ToolDefinition>, config: McpConfig) -> Self {
            let dir = temp_dir("fixture");
            Self {
                catalog: ToolCatalog::new(tools),
                policy: SafetyPolicy::new(config),
                auditor: Auditor::new(dir.join("audit.jsonl"), true),
                dir,
            }
        }

        fn gateway(&self) -> ToolGateway<'_> {
            ToolGateway::new(
                &self.catalog,
                &self.policy,
                &self.auditor,
                ClientCapabilities::headless(),
                "test",
                std::sync::Arc::new(ConcurrencyLimiter::new(2)),
            )
        }

        fn audit_text(&self) -> String {
            fs::read_to_string(self.dir.join("audit.jsonl")).unwrap_or_default()
        }
    }

    fn cleanup(fixture: &Fixture) {
        let _ = fs::remove_dir_all(&fixture.dir);
    }

    /// 未授权工具绝对不会执行：用一个「会创建文件」的工具验证连进程都没起来。
    #[test]
    fn an_unauthorized_tool_never_starts_a_process() {
        let target = temp_dir("unauthorized").join("should-not-exist");
        let mut writer = tool("test:writer", false);
        let action = writer.action.as_mut().expect("action");
        action.program = String::from("touch");
        let fixture = Fixture::new(
            vec![writer],
            config(""), // allow 里只有 test:echo
        );

        let mut values = TypedValues::new();
        values.set_text("text", target.to_string_lossy().as_ref());
        let error = fixture
            .gateway()
            .call("test:writer", &values, Path::new("/tmp"))
            .expect_err("未授权");

        assert!(
            matches!(error, ServiceError::PermissionDenied { .. }),
            "{error}"
        );
        assert!(!target.exists(), "拒绝之后不该有文件被创建");
        assert!(fixture.audit_text().contains("\"authorized\":false"));
        cleanup(&fixture);
        let _ = fs::remove_dir_all(target.parent().expect("parent"));
    }

    /// 参数里的 shell 元字符只是普通字符，不会变成 shell 代码。
    #[test]
    fn shell_metacharacters_stay_plain_argv_elements() {
        let fixture = Fixture::new(vec![tool("test:echo", false)], config(""));
        let target = fixture.dir.join("pwned");
        let mut values = TypedValues::new();
        let nasty = format!("; touch {} && echo $(id)", target.display());
        values.set_text("text", &nasty);

        let result = fixture
            .gateway()
            .call("test:echo", &values, Path::new("/tmp"))
            .expect("执行");
        assert!(
            result.stdout.contains(&nasty),
            "原样回显: {:?}",
            result.stdout
        );
        assert!(!target.exists(), "元字符绝不能被当成命令执行");
        assert!(fixture.audit_text().contains("\"decision\":\"completed\""));
        cleanup(&fixture);
    }

    #[test]
    fn interactive_and_native_tools_never_appear_in_the_callable_list() {
        let mut interactive = tool("test:interactive", false);
        interactive.mode = RunMode::Interactive;
        let mut native = tool("test:native", false);
        native.mode = RunMode::Native;
        let mut bare = tool("test:bare", false);
        bare.mcp = None;

        let fixture = Fixture::new(
            vec![
                tool("test:echo", false),
                interactive,
                native,
                bare,
                tool("test:not-allowed", false),
            ],
            config(""),
        );
        let ids: Vec<&str> = fixture
            .gateway()
            .list()
            .into_iter()
            .map(|tool| tool.id.as_str())
            .collect();
        assert_eq!(ids, ["test:echo"]);
        cleanup(&fixture);
    }

    /// 敏感值：执行用真值，命令摘要和审计里都只能是 `***`。
    #[test]
    fn sensitive_values_are_masked_in_command_and_audit() {
        let fixture = Fixture::new(vec![tool("test:echo", true)], config(""));
        let mut values = TypedValues::new();
        values.set_text("text", "s3cr3t-token");

        let result = fixture
            .gateway()
            .call("test:echo", &values, Path::new("/tmp"))
            .expect("执行");

        assert!(result.stdout.contains("s3cr3t-token"), "执行必须用真值");
        assert!(
            !result.command.contains("s3cr3t-token"),
            "{}",
            result.command
        );
        assert!(result.command.contains("***"));
        assert!(
            !fixture.audit_text().contains("s3cr3t-token"),
            "审计不能存原值"
        );
        cleanup(&fixture);
    }

    #[test]
    fn output_is_limited_to_the_configured_byte_budget() {
        let fixture = Fixture::new(
            vec![tool("test:echo", false)],
            config("[limits]\nmax_output_bytes = 8\n"),
        );
        let mut values = TypedValues::new();
        values.set_text("text", "0123456789-abcdef");

        let result = fixture
            .gateway()
            .call("test:echo", &values, Path::new("/tmp"))
            .expect("执行");
        assert!(result.truncated);
        assert!(result.stdout.len() <= 8, "{:?}", result.stdout);
        cleanup(&fixture);
    }

    #[test]
    fn concurrency_is_bounded_and_permits_are_released() {
        let limiter = ConcurrencyLimiter::new(1);
        let first = limiter.acquire().expect("第一个名额");
        assert!(limiter.acquire().is_none(), "超过上限要拒绝而不是排队");
        drop(first);
        assert!(limiter.acquire().is_some(), "名额要还回来");
    }

    /// P4-02：名额是**跨网关实例共享**的 —— 服务端同步调用与后台任务共用一份预算，
    /// 名额被占住时，另一个网关的调用同样直接拒绝（不排队）。
    #[test]
    fn the_shared_limiter_bounds_calls_across_gateway_instances() {
        let limiter = std::sync::Arc::new(ConcurrencyLimiter::new(1));
        let fixture = Fixture::new(vec![tool("test:echo", false)], config(""));
        let _held = limiter.acquire().expect("占住唯一名额");

        let mut values = TypedValues::new();
        values.set_text("text", "hi");
        let gateway = ToolGateway::new(
            &fixture.catalog,
            &fixture.policy,
            &fixture.auditor,
            ClientCapabilities::headless(),
            "test",
            std::sync::Arc::clone(&limiter),
        );
        let error = gateway
            .call("test:echo", &values, Path::new("/tmp"))
            .expect_err("名额被占住");
        assert!(
            matches!(error, ServiceError::PermissionDenied { reason: "busy", .. }),
            "{error}"
        );
        assert!(
            fixture.audit_text().contains("并发已达上限"),
            "busy 拒绝要有审计记录"
        );

        // 热更新上限（P4-04 会改 mcp.toml 的 max_concurrent）：原地生效。
        limiter.set_max(2);
        assert_eq!(limiter.max(), 2);
        let _second = limiter.acquire().expect("上限放宽后有名额");
        assert_eq!(limiter.active(), 2);
    }

    /// 一件真正跑 `/bin/sleep <text>` 的工具（穿透测试用）。
    fn sleeper() -> ToolDefinition {
        let mut tool = tool("test:echo", false);
        let action = tool.action.as_mut().expect("action");
        action.program = String::from("sleep");
        tool.path = PathBuf::from("/bin/sleep");
        tool
    }

    /// P4-01：取消旗标从网关穿透到执行器 —— 名额、审计等流水线行为不变。
    #[test]
    fn call_with_hooks_passes_the_cancel_flag_through_the_pipeline() {
        let fixture = Fixture::new(vec![sleeper()], config(""));
        let flag = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));

        let mut values = TypedValues::new();
        values.set_text("text", "30");
        let result = fixture
            .gateway()
            .call_with_hooks(
                "test:echo",
                &values,
                Path::new("/tmp"),
                Some(&flag),
                &mut |_| {},
            )
            .expect("旗标已立：执行立刻被取消");

        assert!(result.cancelled, "{:?}", result.summary());
        assert!(!result.success);
        assert!(
            fixture.audit_text().contains("\"decision\":\"completed\""),
            "取消也是一次完成记录（被取消的结果）"
        );
        cleanup(&fixture);
    }

    /// P4-03：进度钩子从网关穿透到执行器（1 秒节流下，慢任务至少一条）。
    #[test]
    fn call_with_hooks_passes_progress_through_the_pipeline() {
        let fixture = Fixture::new(vec![sleeper()], config(""));
        let mut ticks = Vec::new();

        let mut values = TypedValues::new();
        values.set_text("text", "1.4");
        let result = fixture
            .gateway()
            .call_with_hooks(
                "test:echo",
                &values,
                Path::new("/tmp"),
                None,
                &mut |progress: crate::service::execution::ExecutionProgress| {
                    ticks.push(progress);
                },
            )
            .expect("执行");

        assert!(result.success, "{:?}", result.summary());
        assert!(!ticks.is_empty(), "进度钩子要被调用过：{ticks:?}");
        for tick in &ticks {
            assert!(
                !tick.message.contains('%'),
                "不伪造百分比：{}",
                tick.message
            );
        }
        cleanup(&fixture);
    }

    #[test]
    fn truncation_never_splits_a_utf8_character() {
        assert_eq!(truncate_utf8("中文abc", 4), "中");
        assert_eq!(truncate_utf8("abc", 10), "abc");

        let mut result = crate::service::result::ExecutionResult {
            tool_id: String::from("test"),
            tool_name: String::from("test"),
            command: String::new(),
            stdout: String::from("中文中文"),
            stderr: String::new(),
            exit_code: Some(0),
            success: true,
            cancelled: false,
            timed_out: false,
            truncated: false,
            elapsed: std::time::Duration::ZERO,
        };
        limit_output(&mut result, 3);
        assert!(result.truncated);
        assert_eq!(result.stdout, "中");
    }
}
