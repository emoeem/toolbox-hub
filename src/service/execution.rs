//! 无终端执行器。
//!
//! 把「动作 + 类型化取值」变成一条真正跑起来的命令，并返回**结构化**结果。
//! 复用 [`crate::runtime::spawn_captured`] 的进程组、取消与输出上限 —— 那一套
//! 已经被 TUI 的队列验证过，不再写第二份。
//!
//! 与 TUI 执行路径的关系：
//!
//! * TUI 的捕获队列追求「实时输出 + 进度条」，事件由主循环按帧取；
//! * 这里追求「一次调用拿到最终结果」，所以自己把事件收干净，并额外提供超时。
//!
//! 两条路都只传 argv、都不经 shell、都不占用终端。

use std::{
    path::{Path, PathBuf},
    sync::mpsc::RecvTimeoutError,
    time::{Duration, Instant},
};

use crate::{
    model::{RunMode, ToolDefinition, TypedValues},
    runtime::{Captured, JobEvent, RunningJob, spawn_captured},
    service::{
        argument,
        error::{ServiceError, mode_name},
        result::ExecutionResult,
    },
};

/// 默认超时：60 秒。
///
/// 无终端调用没有「用户看着进度条、随时按 q」这层兜底，所以必须自己定一个上限。
/// 真需要更久的调用方用 [`ExecutionRequest::with_timeout`] 显式放宽。
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(60);

/// 超时后先礼后兵：SIGTERM 之后的宽限期。
const TERM_GRACE: Duration = Duration::from_secs(2);
/// SIGKILL 之后等收尾结果的上限。
const KILL_GRACE: Duration = Duration::from_secs(1);
/// 轮询事件通道的间隔：够短（取消/超时反应快），又不烧 CPU。
const POLL_INTERVAL: Duration = Duration::from_millis(50);

/// 一次执行请求。
///
/// 字段是公开的：测试与调用方可以手工拼一个请求（比如跑 `/bin/sh -c …`），
/// 不必先造一个完整的 [`ToolDefinition`]。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExecutionRequest {
    pub tool_id: String,
    pub tool_name: String,
    /// 可执行文件路径（来自 `ToolDefinition::path`，Provider 已尽量解析成绝对路径）。
    pub program: PathBuf,
    /// 参数（**不含程序名**）。
    pub argv: Vec<String>,
    pub cwd: PathBuf,
    /// `None` 表示不设超时（由调用方自己控制生命周期）。
    pub timeout: Option<Duration>,
    /// 哪些退出码算成功。
    pub ok_exit_codes: Vec<i32>,
    /// 敏感参数的真值：生成命令摘要时把它们抹成 `***`。
    ///
    /// 执行当然要用真值；这里只影响**回传给调用方**的那一行命令。
    pub secrets: Vec<String>,
}

impl ExecutionRequest {
    /// 从目录里的工具 + 类型化取值构造请求。
    ///
    /// 只接受 `capture` 且依赖齐全的工具：`interactive` / `native` 需要终端或界面，
    /// 无终端环境跑不了 —— 与其跑到一半失败，不如在构造请求时就拒绝。
    ///
    /// 取值只覆盖调用方**显式给出**的字段，其余字段用动作声明的缺省值（与 TUI
    /// 打开表单时的初值一致），所以调用方不必把每个开关都重复一遍。
    pub fn for_tool(
        tool: &ToolDefinition,
        values: &TypedValues,
        cwd: &Path,
    ) -> Result<Self, ServiceError> {
        if tool.mode != RunMode::Capture {
            return Err(ServiceError::UnsupportedMode {
                tool: tool.name.clone(),
                mode: mode_name(tool.mode),
            });
        }
        if !tool.ready {
            return Err(ServiceError::NotReady {
                tool: tool.name.clone(),
                missing: tool.missing_deps.clone(),
            });
        }

        let (argv, ok_exit_codes, secrets) = match &tool.action {
            Some(action) => {
                argument::validate(action, values).map_err(ServiceError::InvalidArguments)?;
                let merged = merge_defaults(action, values);
                let argv = action
                    .build_argv_typed(&merged)
                    .map_err(ServiceError::InvalidArguments)?;
                let secrets = merged.secrets(&action.arguments);
                (argv, action.ok_exit_codes.clone(), secrets)
            }
            None => {
                if !values.is_empty() {
                    return Err(ServiceError::UnexpectedArguments(tool.name.clone()));
                }
                (Vec::new(), vec![0], Vec::new())
            }
        };

        Ok(Self {
            tool_id: tool.id.clone(),
            tool_name: tool.name.clone(),
            program: tool.path.clone(),
            argv,
            cwd: cwd.to_path_buf(),
            timeout: Some(DEFAULT_TIMEOUT),
            ok_exit_codes,
            secrets,
        })
    }

    /// 覆盖超时设置；`None` 表示不限时。
    pub fn with_timeout(mut self, timeout: Option<Duration>) -> Self {
        self.timeout = timeout;
        self
    }

    fn accepts(&self, code: i32) -> bool {
        if self.ok_exit_codes.is_empty() {
            code == 0
        } else {
            self.ok_exit_codes.contains(&code)
        }
    }
}

/// 把调用方给的值叠加到动作声明的缺省值上。
fn merge_defaults(action: &crate::model::Action, values: &TypedValues) -> TypedValues {
    let mut merged = TypedValues::from_form(&action.default_values(), &action.arguments);
    for (key, value) in values.iter() {
        merged.set(key, value.clone());
    }
    merged
}

/// 执行一次请求。阻塞到进程结束、超时或通道断开。
pub fn execute(request: &ExecutionRequest) -> Result<ExecutionResult, ServiceError> {
    let job = spawn_captured(
        &request.program,
        &request.argv,
        &request.cwd,
        &request.tool_name,
    )
    .map_err(|error| ServiceError::Execution(format!("无法启动 {}: {error}", request.tool_name)))?;

    let started = Instant::now();
    let deadline = request.timeout.map(|timeout| started + timeout);

    if let Some(captured) = pump(&job, deadline)? {
        return Ok(result_from(captured, request, false));
    }

    // 超时：先 SIGTERM（脚本/ffmpeg 有机会把输出文件收尾），再 SIGKILL。
    job.terminate();
    if let Some(captured) = pump(&job, Some(Instant::now() + TERM_GRACE))? {
        return Ok(result_from(captured, request, true));
    }

    job.cancel();
    if let Some(captured) = pump(&job, Some(Instant::now() + KILL_GRACE))? {
        return Ok(result_from(captured, request, true));
    }

    Err(ServiceError::Execution(format!(
        "{} 被终止后仍未退出（收尾结果丢失）",
        request.tool_name
    )))
}

/// 收事件，直到 `Done` 或 `deadline` 到点。
///
/// `Ok(None)` 表示超时；`Ok(Some(captured))` 表示正常收尾。
/// 到点时先做一次零等待的 `recv_timeout`：刚好压在截止时间上的 `Done` 不该被丢掉。
fn pump(job: &RunningJob, deadline: Option<Instant>) -> Result<Option<Captured>, ServiceError> {
    loop {
        let wait = match deadline {
            Some(deadline) => {
                let now = Instant::now();
                if now >= deadline {
                    Duration::ZERO
                } else {
                    (deadline - now).min(POLL_INTERVAL)
                }
            }
            None => POLL_INTERVAL,
        };

        match job.events.recv_timeout(wait) {
            Ok(JobEvent::Done(captured)) => return Ok(Some(*captured)),
            Ok(_) => {}
            Err(RecvTimeoutError::Timeout) => {
                if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
                    return Ok(None);
                }
            }
            Err(RecvTimeoutError::Disconnected) => {
                return Err(ServiceError::Execution(String::from(
                    "输出通道在任务结束前断开",
                )));
            }
        }
    }
}

/// 生成「给人/给 AI 看」的命令摘要：敏感值换成 `***`。
///
/// 用 `replace` 而不是整元素相等：`flag_join` 会把 `-o` 和值贴成一个元素
/// （`-o/tmp/secret`），按整元素比对会漏掉。
fn command_line(program: &Path, argv: &[String], secrets: &[String]) -> String {
    let masked: Vec<String> = argv
        .iter()
        .map(|item| {
            let mut text = item.clone();
            for secret in secrets {
                if secret.is_empty() {
                    continue;
                }
                let applies = item == secret || (secret.len() >= 2 && item.contains(secret));
                if applies {
                    text = text.replace(secret, "***");
                }
            }
            text
        })
        .collect();
    format!("{} {}", program.display(), masked.join(" "))
        .trim_end()
        .to_string()
}

/// 把 runtime 的捕获结果翻译成服务层结果。
///
/// 成功与否按 `ok_exit_codes` 判断，不看 `status.success()`：`pacman -Qdt` 在没有
/// 孤儿包时退出 1，那是正常结果（这条规则已经在 manifest 里声明过了）。
fn result_from(captured: Captured, request: &ExecutionRequest, timed_out: bool) -> ExecutionResult {
    let success = !captured.cancelled
        && !timed_out
        && captured.status.is_some_and(|code| request.accepts(code));

    ExecutionResult {
        tool_id: request.tool_id.clone(),
        tool_name: request.tool_name.clone(),
        command: command_line(&request.program, &request.argv, &request.secrets),
        stdout: captured.stdout,
        stderr: captured.stderr,
        exit_code: captured.status,
        success,
        cancelled: captured.cancelled,
        timed_out,
        truncated: captured.truncated,
        elapsed: captured.elapsed,
    }
}

#[cfg(test)]
mod tests {
    use std::{
        path::{Path, PathBuf},
        time::Duration,
    };

    use super::{ExecutionRequest, execute};
    use crate::model::{
        Action, ArgKind, ArgPlacement, Argument, Danger, Domain, RunMode, ToolDefinition,
        TypedValues,
    };
    use crate::service::error::ServiceError;

    fn shell(
        argv: &[&str],
        ok_exit_codes: Vec<i32>,
        timeout: Option<Duration>,
    ) -> ExecutionRequest {
        ExecutionRequest {
            tool_id: String::from("test:shell"),
            tool_name: String::from("测试命令"),
            program: PathBuf::from("/bin/sh"),
            argv: argv.iter().map(|item| item.to_string()).collect(),
            cwd: std::env::temp_dir(),
            timeout,
            ok_exit_codes,
            secrets: Vec::new(),
        }
    }

    fn tool(action: Option<Action>, mode: RunMode, ready: bool) -> ToolDefinition {
        ToolDefinition {
            mcp: None,
            id: String::from("test:demo"),
            name: String::from("演示"),
            provider: String::from("Test"),
            domain: Domain::Tools,
            tags: Vec::new(),
            summary: String::new(),
            input: None,
            output: None,
            features: None,
            requires: Vec::new(),
            missing_deps: if ready {
                Vec::new()
            } else {
                vec![String::from("jq")]
            },
            install_hint: None,
            action,
            pin: None,
            mode,
            danger: Danger::Safe,
            path: PathBuf::from("/bin/sh"),
            ready,
        }
    }

    fn demo_action() -> Action {
        Action {
            program: String::from("sh"),
            base_argv: vec![String::from("-c"), String::from("printf '%s' \"$1\"")],
            arguments: vec![Argument {
                key: String::from("text"),
                label: String::from("文本"),
                kind: ArgKind::Text,
                default: None,
                choices: Vec::new(),
                required: true,
                flag: None,
                flag_join: false,
                placement: ArgPlacement::Trailing,
                sensitive: false,
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
            allow_empty: false,
        }
    }

    /// 命令摘要是**回传给调用方**的，敏感值不能原样出现在里面（含 flag_join 形状）。
    #[test]
    fn command_summary_masks_secrets_even_when_joined_to_a_flag() {
        let line = super::command_line(
            Path::new("/bin/echo"),
            &[String::from("--token=s3cr3t"), String::from("plain")],
            &[String::from("s3cr3t")],
        );
        assert_eq!(line, "/bin/echo --token=*** plain");
    }

    #[test]
    fn captures_stdout_and_exit_code() {
        let result = execute(&shell(&["-c", "printf 'hi'"], vec![0], None)).expect("执行");
        assert_eq!(result.stdout, "hi\n");
        assert_eq!(result.exit_code, Some(0));
        assert!(result.success);
        assert!(!result.timed_out && !result.cancelled && !result.truncated);
        assert!(result.command.contains("/bin/sh"));
    }

    #[test]
    fn success_follows_the_declared_exit_codes() {
        let rejected = execute(&shell(&["-c", "exit 3"], vec![0], None)).expect("执行");
        assert_eq!(rejected.exit_code, Some(3));
        assert!(!rejected.success, "3 不在白名单里");

        let accepted = execute(&shell(&["-c", "exit 3"], vec![3], None)).expect("执行");
        assert!(accepted.success, "manifest 说 3 算成功就得算成功");
    }

    #[test]
    fn stderr_is_kept_separate() {
        let result =
            execute(&shell(&["-c", "echo out; echo err >&2"], vec![0], None)).expect("执行");
        assert_eq!(result.stdout.trim(), "out");
        assert_eq!(result.stderr.trim(), "err");
    }

    /// 超时必须真的把进程杀掉，而不是「不再等它」—— 否则后台会留下孤儿在写文件。
    #[test]
    fn a_hanging_command_is_killed_at_the_deadline() {
        let request = shell(
            &["-c", "sleep 5"],
            vec![0],
            Some(Duration::from_millis(300)),
        );
        let result = execute(&request).expect("执行");
        assert!(result.timed_out, "{}", result.summary());
        assert!(!result.success);
        assert!(
            result.elapsed < Duration::from_secs(4),
            "超时后不该还等满 5 秒：{:?}",
            result.elapsed
        );
    }

    #[test]
    fn a_missing_program_is_an_execution_error() {
        let request = ExecutionRequest {
            program: PathBuf::from("/definitely/not/here"),
            ..shell(&[], vec![0], None)
        };
        assert!(matches!(execute(&request), Err(ServiceError::Execution(_))));
    }

    #[test]
    fn only_capture_tools_can_be_requested() {
        for mode in [RunMode::Interactive, RunMode::Native] {
            let error = ExecutionRequest::for_tool(
                &tool(None, mode, true),
                &TypedValues::new(),
                Path::new("/tmp"),
            )
            .expect_err("需要终端的工具不该被接受");
            assert_eq!(error.kind(), "unsupported_mode");
        }
    }

    #[test]
    fn a_tool_with_missing_dependencies_is_rejected_before_spawning() {
        let error = ExecutionRequest::for_tool(
            &tool(Some(demo_action()), RunMode::Capture, false),
            &TypedValues::new(),
            Path::new("/tmp"),
        )
        .expect_err("缺依赖不该硬跑");
        assert_eq!(error.kind(), "not_ready");
    }

    #[test]
    fn argv_is_built_from_typed_values_with_defaults_filled_in() {
        let mut values = TypedValues::new();
        values.set_text("text", "hello world");

        let request = ExecutionRequest::for_tool(
            &tool(Some(demo_action()), RunMode::Capture, true),
            &values,
            Path::new("/tmp"),
        )
        .expect("构造请求");

        assert_eq!(
            request.argv,
            vec![
                String::from("-c"),
                String::from("printf '%s' \"$1\""),
                String::from("hello world"),
            ]
        );
        assert_eq!(request.timeout, Some(super::DEFAULT_TIMEOUT));
    }

    #[test]
    fn a_missing_required_argument_is_rejected_with_a_readable_message() {
        let error = ExecutionRequest::for_tool(
            &tool(Some(demo_action()), RunMode::Capture, true),
            &TypedValues::new(),
            Path::new("/tmp"),
        )
        .expect_err("必填项没给");
        assert_eq!(error.kind(), "invalid_arguments");
        assert!(error.to_string().contains("必填项"), "{error}");
    }
}
