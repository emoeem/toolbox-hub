//! `toolbox-hub tools …`：与 MCP 同一套 Service 的 JSON 命令行入口（计划书 P5-04）。
//!
//! ```text
//! toolbox-hub tools list --json
//! toolbox-hub tools show <tool-id> --json
//! toolbox-hub tools run <tool-id> --input-json '{"input":"a.mp4"}' --output json
//! ```
//!
//! 定位是「MCP 的独立测试入口 + 给 Shell / Python / 其它 Agent 的结构化界面」，
//! 所以纪律与 MCP 完全一致：
//!
//! * **同一套 Service**：目录（含 P5-01 的 Recipe 工具）、`mcp.toml` 策略、网关、
//!   审计全复用 —— 列表里出现的、能跑的，与 MCP 客户端看到的严格相同；
//! * **JSON 形状与 MCP 一致**：`list` 的条目就是 `tools/list` 的条目，`show`
//!   的条目就是 `toolbox://tools/{id}` 资源的内容，`run` 的结果就是
//!   `tools/call` 的 CallToolResult（`content` + `structuredContent` + `isError`）；
//! * **拒绝发生在子进程之前**：interactive / native / 未授权 / 参数不合法一律
//!   当场拒绝，JSON 模式绝不挂起等终端；
//! * stdout 只承载 JSON；诊断走 stderr。调用在审计里记 `cli` 客户端。
//!
//! 退出码：结果 `isError` 为假（或查询成功）= 0；被拒 / 执行失败 = 1；
//! 用法错误（不认识的参数、缺参数、JSON 拼错）走 stderr 的普通错误，退出码 1
//! （`main.rs` 统一打印）。

use std::{path::PathBuf, sync::Arc};

use serde_json::{Value, json};

use crate::{
    cli::Dirs,
    config,
    mcp::{
        names::NameRegistry,
        schema::values_from_json,
        server::{execution_result, tool_entry, tool_error, tool_resource_detail},
    },
    model::ToolDefinition,
    registry::Registry,
    security::{approval::ClientCapabilities, audit::Auditor, policy::SafetyPolicy},
    service::{
        catalog::ToolCatalog,
        error::ServiceError,
        gateway::{ConcurrencyLimiter, ToolGateway},
        recipe,
    },
};

/// `tools` 子命令入口（main.rs 的动词分发进来；`args[0] == "tools"`）。
pub fn run_cli(args: &[String]) -> Result<(), String> {
    let (dirs, command) = parse(args)?;
    let context = setup(&dirs);
    match command {
        Command::List => {
            print_json(&json!({
                "tools": context
                    .authorized()
                    .iter()
                    .map(|tool| tool_entry(&context.names, tool))
                    .collect::<Vec<_>>(),
            }));
            Ok(())
        }
        Command::Show(id) => match context.authorized_tool(&id) {
            Some(tool) => {
                print_json(&tool_resource_detail(
                    tool,
                    context.names.name_of(&id).unwrap_or(&id),
                ));
                Ok(())
            }
            None => {
                print_json(&tool_error(
                    format!("没有这个可调用工具：{id}（tools list 看得到的就是全部）"),
                    "unknown_tool",
                ));
                std::process::exit(1);
            }
        },
        Command::Run { id, arguments } => {
            let outcome = context.run(&id, arguments.as_ref());
            let is_error = outcome["isError"].as_bool().unwrap_or(true);
            print_json(&outcome);
            // isError 的结果也照样打印完整 JSON（调用方能读到 error_kind），
            // 但退出码非零，脚本不用解析 JSON 就知道失败了。
            if is_error {
                std::process::exit(1);
            }
            Ok(())
        }
    }
}

const USAGE: &str = "用法：toolbox-hub tools list --json | tools show <id> --json | tools run <id> [--input-json '{…}'] [--output json]";

/// 一次 `tools` 调用。
#[derive(Clone, Debug, PartialEq, Eq)]
enum Command {
    List,
    Show(String),
    Run {
        id: String,
        arguments: Option<Value>,
    },
}

/// 解析 `tools` 之后的参数。纯函数，方便直接测试。
///
/// `--config-dir` / `--data-dir` 与主 CLI 的全局选项同一语义：**放哪儿都认**
/// （先整体摘掉，剩下的才是子命令自己的参数）。
fn parse(args: &[String]) -> Result<(Dirs, Command), String> {
    // args[0] == "tools"（main.rs 已保证）。
    let mut dirs = Dirs::default();
    let mut words: Vec<String> = Vec::new();
    let mut iter = args[1..].iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--config-dir" => dirs.config = Some(path_value(&mut iter)?),
            "--data-dir" => dirs.data = Some(path_value(&mut iter)?),
            _ => words.push(arg.clone()),
        }
    }

    let Some(sub) = words.first().map(String::as_str) else {
        return Err(String::from(USAGE));
    };
    let rest = &mut words[1..].iter();
    let command = match sub {
        "list" => {
            expect_only(rest, "tools list")?;
            Command::List
        }
        "show" => {
            let id = rest.next().ok_or("tools show 后面要跟工具 id")?;
            if id.starts_with('-') && id != "-" {
                return Err(format!("tools show 的 id 不该以 - 开头：{id}"));
            }
            expect_only(rest, "tools show")?;
            Command::Show(id.clone())
        }
        "run" => {
            let id = rest.next().ok_or("tools run 后面要跟工具 id")?;
            if id.starts_with('-') && id != "-" {
                return Err(format!("tools run 的 id 不该以 - 开头：{id}"));
            }
            let mut input: Option<String> = None;
            let mut output_json = false;
            while let Some(arg) = rest.next() {
                match arg.as_str() {
                    "--input-json" => {
                        let value = rest.next().ok_or("--input-json 后面要跟 JSON 文本")?;
                        input = Some(value.clone());
                    }
                    "--output" => {
                        let value = rest
                            .next()
                            .ok_or("--output 后面要跟取值（目前只有 json）")?;
                        if value != "json" {
                            return Err(format!("--output 只支持 json，收到：{value}"));
                        }
                        output_json = true;
                    }
                    other => return Err(format!("tools run 不认识的选项：{other}")),
                }
            }
            let arguments = input
                .map(|text| {
                    serde_json::from_str::<Value>(&text)
                        .map_err(|error| format!("--input-json 不是合法 JSON：{error}"))
                })
                .transpose()?;
            // JSON 是唯一输出格式；显式写 `--output json` 只为自文档。
            let _ = output_json;
            Command::Run {
                id: id.clone(),
                arguments,
            }
        }
        other => return Err(format!("不认识的子命令「{other}」；{USAGE}")),
    };
    Ok((dirs, command))
}

fn path_value<'a, I>(rest: &mut I) -> Result<PathBuf, String>
where
    I: Iterator<Item = &'a String>,
{
    rest.next()
        .map(PathBuf::from)
        .ok_or_else(|| String::from("缺少路径值（--config-dir / --data-dir 后面要跟目录）"))
}

/// 动词后面只允许出现无参选项（目前只有 `--json`，写出来是自文档）。
fn expect_only<'a, I>(rest: &mut I, verb: &str) -> Result<(), String>
where
    I: Iterator<Item = &'a String>,
{
    for arg in rest {
        if arg != "--json" {
            return Err(format!("{verb} 不认识的选项：{arg}"));
        }
    }
    Ok(())
}

/// 一次 `tools` 调用的环境：目录、策略、审计、名称表 —— 与 MCP 服务端同源同构。
struct Context {
    catalog: ToolCatalog,
    policy: SafetyPolicy,
    auditor: Auditor,
    names: NameRegistry,
    limiter: Arc<ConcurrencyLimiter>,
}

impl Context {
    fn gateway(&self) -> ToolGateway<'_> {
        ToolGateway::new(
            &self.catalog,
            &self.policy,
            &self.auditor,
            // CLI 没有确认能力（无终端交互）：需要确认的工具与 MCP 无 elicitation
            // 客户端看到的一样，直接被拒，而不是挂起等人。
            ClientCapabilities::headless(),
            "cli",
            Arc::clone(&self.limiter),
        )
    }

    /// 本次能调用的工具（与 MCP `tools/list` 同一判定）。
    fn authorized(&self) -> Vec<&ToolDefinition> {
        self.gateway().list()
    }

    fn authorized_tool(&self, id: &str) -> Option<&ToolDefinition> {
        self.authorized().into_iter().find(|tool| tool.id == id)
    }

    /// `tools run`：与 MCP `tools/call` 同一条网关路径，结果同形状。
    fn run(&self, id: &str, arguments: Option<&Value>) -> Value {
        let Some(tool) = self.catalog.get(id).cloned() else {
            return tool_error(format!("没有这个工具：{id}"), "unknown_tool");
        };
        let values = match values_from_json(&tool, arguments) {
            Ok(values) => values,
            Err(message) => return tool_error(message, "invalid_arguments"),
        };
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        match self.gateway().call(id, &values, &cwd) {
            Ok(result) => execution_result(&result),
            Err(ServiceError::UnknownTool(_)) => {
                tool_error(format!("没有这个工具：{id}"), "unknown_tool")
            }
            Err(error) => tool_error(error.to_string(), error.kind()),
        }
    }
}

fn setup(dirs: &Dirs) -> Context {
    config::configure(dirs.config.clone(), dirs.data.clone());
    let (policy, problem) = SafetyPolicy::from_file();
    if let Some(problem) = &problem {
        eprintln!("toolbox-hub tools: {problem}");
    }
    if !policy.config.mcp.enabled {
        eprintln!("toolbox-hub tools: mcp.toml 未启用 MCP（[mcp] enabled），工具列表为空");
    }

    // 工具发现与 MCP / TUI 同源：bin 目录决定本地脚本扫哪儿。
    let bin_dir = crate::resolve_bin_dir(None);
    let (registry, _report) = Registry::discover(&bin_dir);
    // P5-01：Recipe 工具与 Manifest 工具合并进同一份目录。
    let (catalog, warnings) = recipe::catalog_with_recipes(&registry, policy.config.recipe_dirs());
    for warning in &warnings {
        eprintln!("toolbox-hub tools: {warning}");
    }
    let limiter = Arc::new(ConcurrencyLimiter::new(policy.config.max_concurrent()));
    let auditor = Auditor::new(policy.config.audit_path(), policy.config.audit.enabled);

    let context = Context {
        catalog,
        policy: policy.clone(),
        auditor,
        names: NameRegistry::default(),
        limiter,
    };
    // 名称表按「这次实际可授权的工具」重建（与 MCP 的 rebuild_names 同一规则），
    // 让 `list` / `show` 输出里的 name 与 MCP 完全一致。
    let names = NameRegistry::build(context.gateway().list());
    Context { names, ..context }
}

fn print_json(value: &Value) {
    println!(
        "{}",
        serde_json::to_string_pretty(value).unwrap_or_else(|_| String::from("{}"))
    );
}

#[cfg(test)]
mod tests {
    use super::{Command, parse};
    use crate::cli::Dirs;

    fn parse_args(args: &[&str]) -> (Dirs, Command) {
        parse(&args.iter().map(|arg| arg.to_string()).collect::<Vec<_>>()).expect("应能解析")
    }

    fn parse_err(args: &[&str]) -> String {
        parse(&args.iter().map(|arg| arg.to_string()).collect::<Vec<_>>()).expect_err("应报错")
    }

    #[test]
    fn subcommands_parse() {
        let (_, command) = parse_args(&["tools", "list", "--json"]);
        assert_eq!(command, Command::List);

        let (_, command) = parse_args(&["tools", "show", "manifest:disk-space"]);
        assert_eq!(command, Command::Show(String::from("manifest:disk-space")));

        let (_, command) = parse_args(&[
            "tools",
            "run",
            "manifest:disk-space",
            "--input-json",
            "{\"path\":\"/tmp\"}",
            "--output",
            "json",
        ]);
        assert_eq!(
            command,
            Command::Run {
                id: String::from("manifest:disk-space"),
                arguments: Some(serde_json::json!({"path": "/tmp"})),
            }
        );
        // --output json 可省（JSON 是唯一格式）。
        let (_, command) = parse_args(&["tools", "run", "recipe:x", "--input-json", "{}"]);
        assert_eq!(
            command,
            Command::Run {
                id: String::from("recipe:x"),
                arguments: Some(serde_json::json!({})),
            }
        );
        // 完全省略 --input-json = 无参数调用。
        let (_, command) = parse_args(&["tools", "run", "recipe:x"]);
        assert_eq!(
            command,
            Command::Run {
                id: String::from("recipe:x"),
                arguments: None,
            }
        );
    }

    #[test]
    fn directory_options_come_before_the_verb() {
        let (dirs, command) = parse_args(&[
            "tools",
            "--config-dir",
            "/tmp/cfg",
            "--data-dir",
            "/tmp/data",
            "list",
        ]);
        assert_eq!(dirs.config, Some(std::path::PathBuf::from("/tmp/cfg")));
        assert_eq!(dirs.data, Some(std::path::PathBuf::from("/tmp/data")));
        assert_eq!(command, Command::List);
    }

    #[test]
    fn mistakes_are_reported() {
        assert!(parse_err(&["tools"]).contains("用法"));
        assert!(parse_err(&["tools", "frobnicate"]).contains("不认识的子命令"));
        assert!(parse_err(&["tools", "show"]).contains("要跟工具 id"));
        // id 位置上是选项 → 按「id 不该以 - 开头」拒绝，而不是静默当成参数。
        assert!(parse_err(&["tools", "run", "--input-json", "{}"]).contains("不该以 - 开头"));
        assert!(parse_err(&["tools", "run", "x", "--input-json", "{bad"]).contains("合法 JSON"));
        assert!(parse_err(&["tools", "run", "x", "--output", "text"]).contains("只支持 json"));
        assert!(parse_err(&["tools", "list", "--wat"]).contains("不认识的选项"));
        // id 打成选项的样子要被拦下来，而不是静默当成参数。
        assert!(parse_err(&["tools", "run", "--wat"]).contains("不该以 - 开头"));
    }
}
