//! MCP stdio 服务端（计划书 P3）。
//!
//! 用法：
//!
//! ```text
//! toolbox-hub mcp stdio [--config-dir X] [--data-dir Y]
//! ```
//!
//! AI 客户端把它当普通 MCP Server 启动（stdout = 协议、stderr = 诊断）：
//!
//! ```json
//! {"mcpServers": {"toolbox-hub": {"command": "/usr/bin/toolbox-hub", "args": ["mcp", "stdio"]}}}
//! ```
//!
//! 纪律（计划书 §4.4 / P3-01）：
//!
//! 1. **stdout 只承载协议**。任何诊断都走 stderr；审计单独落文件。
//! 2. 不进 TUI、不碰终端初始化 —— 这里连 crossterm 都不 import。
//! 3. 所有拒绝发生在启动子进程之前（`security::policy` + `service::gateway`）。
//! 4. 退出时无孤儿进程：执行是同步的，行处理完才收下一行。
//!
//! 模块划分：
//!
//! | 文件 | 职责 |
//! | --- | --- |
//! | [`protocol`] | JSON-RPC 行协议（解析、响应、错误码） |
//! | [`names`] | 内部 id → MCP 工具名（规范化 + 撞名消解） |
//! | [`schema`] | 参数 Schema 生成与 JSON 取值转换 |
//! | [`server`] | 消息分派、确认门槛、结果组装 |

pub mod names;
pub mod protocol;
pub mod schema;
pub mod server;

use std::io::{self, BufRead, Write};

use serde_json::{Value, json};

use crate::{
    cli::Dirs,
    config,
    registry::Registry,
    security::{audit::Auditor, policy::SafetyPolicy},
    service::catalog::ToolCatalog,
};

use self::server::McpServer;

/// `mcp` 子命令入口（不进 TUI）。
pub fn run_cli(args: &[String]) -> Result<(), String> {
    // args[0] == "mcp"（main.rs 已保证）。
    let mut subcommand: Option<&str> = None;
    let mut dirs = Dirs::default();
    let mut rest = args[1..].iter();
    while let Some(arg) = rest.next() {
        match arg.as_str() {
            "stdio" => subcommand = Some("stdio"),
            "--config-dir" => dirs.config = Some(path_value(&mut rest)?),
            "--data-dir" => dirs.data = Some(path_value(&mut rest)?),
            other => {
                return Err(format!(
                    "mcp 不认识「{other}」；用法：toolbox-hub mcp stdio [--config-dir 目录] [--data-dir 目录]"
                ));
            }
        }
    }
    if subcommand != Some("stdio") {
        return Err(String::from("用法：toolbox-hub mcp stdio"));
    }

    config::configure(dirs.config, dirs.data);
    run_stdio().map_err(|error| error.to_string())
}

/// 从参数迭代器里取一个路径值。
fn path_value<'a, I>(rest: &mut I) -> Result<std::path::PathBuf, String>
where
    I: Iterator<Item = &'a String>,
{
    rest.next()
        .map(std::path::PathBuf::from)
        .ok_or_else(|| String::from("缺少路径值（--config-dir / --data-dir 后面要跟目录）"))
}

/// stdio 主循环：stdin 一行一条消息，stdout 一行一条响应。
fn run_stdio() -> io::Result<()> {
    // 第一次跑 MCP 时把带注释的模板写出来，让人知道有这个文件、能去哪里改授权。
    // 已存在则一个字节都不动。
    let created = crate::security::policy::McpConfig::ensure_template();
    if created {
        eprintln!("toolbox-hub mcp: 已生成 mcp.toml 模板（默认只放行 4 个只读巡检动作）");
    }

    let (policy, problem) = SafetyPolicy::from_file();
    if let Some(problem) = &problem {
        eprintln!("toolbox-hub mcp: {problem}");
    }
    if !policy.config.mcp.enabled {
        eprintln!("toolbox-hub mcp: mcp.toml 未启用 MCP（[mcp] enabled），工具列表将为空");
    }

    // 工具发现与 TUI 同源：bin 目录决定本地脚本扫哪儿。
    let bin_dir = crate::resolve_bin_dir(None);
    let (registry, _report) = Registry::discover(&bin_dir);
    let catalog = ToolCatalog::from_registry(&registry);
    let auditor = Auditor::new(policy.config.audit_path(), policy.config.audit.enabled);
    eprintln!(
        "toolbox-hub mcp: 就绪（目录 {} 件工具，授权由 mcp.toml 决定）",
        catalog.len()
    );

    let mut server = McpServer::new(catalog, policy, auditor);
    let stdin = io::stdin();
    let mut reader = stdin.lock();
    let stdout = io::stdout();
    let mut out = stdout.lock();

    loop {
        let mut line = String::new();
        let read = reader.read_line(&mut line)?;
        if read == 0 {
            break; // 客户端关了 stdin：正常退出。
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        // elicitation 回程：写请求 → 按行读，直到那条 id 对上的响应。
        // 期间来的其它消息（客户端不该在这个时机发）安静跳过。
        let mut elicit = |request_id: u64, method: &str, params: Value| -> Option<Value> {
            let request =
                json!({"jsonrpc": "2.0", "id": request_id, "method": method, "params": params});
            writeln!(out, "{request}").ok()?;
            out.flush().ok()?;
            loop {
                let mut answer = String::new();
                let n = reader.read_line(&mut answer).ok()?;
                if n == 0 {
                    return None; // 客户端断开。
                }
                let Ok(value) = serde_json::from_str::<Value>(answer.trim()) else {
                    continue;
                };
                let is_response = value.get("method").is_none();
                if is_response && value.get("id").and_then(Value::as_u64) == Some(request_id) {
                    return value.get("result").cloned();
                }
            }
        };

        for response in server.handle(trimmed, &mut elicit) {
            writeln!(out, "{response}")?;
        }
        out.flush()?;
    }
    eprintln!("toolbox-hub mcp: 客户端断开，退出");
    Ok(())
}
