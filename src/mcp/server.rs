//! MCP 服务端的核心：把 JSON-RPC 消息翻译成「目录查询 + 网关调用」。
//!
//! 职责边界（计划书 P3-03）：
//!
//! ```text
//! tools/call → 名称映射 → 取值转换（schema.rs）→ 确认门槛（elicitation）
//!            → ToolGateway（安全策略 → 执行器）→ 结构化结果
//! ```
//!
//! 这里**绝不**做的事：
//!
//! * 不从客户端参数里读 `program` 并执行 —— 程序只来自目录里的工具定义；
//! * 不把 JSON 拼成 shell 命令 —— 参数永远是独立 argv 元素；
//! * 不往 stdout 写任何日志 —— 本模块只返回「要回给客户端的行」，落盘与诊断
//!   由调用方（`run_stdio`）负责。
//!
//! 消息按顺序处理（一次一条）：并发上限由网关策略兜底，协议层不排队不乱序。

use std::path::PathBuf;

use serde_json::{Value, json};

use crate::{
    model::{ToolDefinition, TypedValues},
    security::{
        approval::{self, ClientCapabilities},
        audit::{Auditor, redacted_arguments},
        effects,
        policy::SafetyPolicy,
    },
    service::{
        catalog::ToolCatalog, error::ServiceError, gateway::ToolGateway, result::ExecutionResult,
    },
};

use super::{
    names::NameRegistry,
    protocol::{self, INVALID_PARAMS, INVALID_REQUEST, METHOD_NOT_FOUND, PROTOCOL_VERSION},
    schema::{input_schema, values_from_json},
};

/// 一台 MCP 服务端。状态只有连接本身的那点：协商出来的能力、名称表、请求计数。
pub struct McpServer {
    catalog: ToolCatalog,
    policy: SafetyPolicy,
    auditor: Auditor,
    names: NameRegistry,
    initialized: bool,
    client_name: String,
    /// 客户端是否声明了 `elicitation` 能力（initialize 时读一次）。
    client_elicitation: bool,
    /// 服务端主动请求（elicitation）的 id，从一个大数起，避免和客户端 id 撞。
    next_request_id: u64,
}

/// 客户端到服务端的回程函数：发一条请求、等它的响应。
///
/// 抽成回调是为了让 [`McpServer::handle`] 保持可测 —— 测试里塞一个假客户端，
/// 真正的 stdio 循环里才是「写出去、按行读回来」。
pub type Elicit<'a> = &'a mut dyn FnMut(u64, &str, Value) -> Option<Value>;

impl McpServer {
    pub fn new(catalog: ToolCatalog, policy: SafetyPolicy, auditor: Auditor) -> Self {
        Self {
            catalog,
            policy,
            auditor,
            names: NameRegistry::default(),
            initialized: false,
            client_name: String::from("unknown"),
            client_elicitation: false,
            next_request_id: 1_000_000,
        }
    }

    /// 处理一行输入，返回**要写回 stdout 的行**（通知类消息返回空）。
    pub fn handle(&mut self, line: &str, elicit: Elicit<'_>) -> Vec<String> {
        let incoming = match protocol::parse(line) {
            Ok(incoming) => incoming,
            Err(error) => {
                return vec![protocol::failure(None, error.code, &error.message)];
            }
        };

        // 通知没有 id，不该有响应（即使是未知通知也安静吞掉）。
        let Some(id) = incoming.id else {
            // notifications/initialized 在这里被自然忽略。
            return Vec::new();
        };

        let outcome = match incoming.method.as_str() {
            "initialize" => self.initialize(&incoming.params),
            "ping" if self.initialized => Ok(json!({})),
            "tools/list" if self.initialized => self.tools_list(),
            "tools/call" if self.initialized => self.call_tool(&incoming.params, elicit),
            _ if !self.initialized => Err((
                INVALID_REQUEST,
                String::from("先发 initialize 再发其它请求"),
            )),
            other => Err((METHOD_NOT_FOUND, format!("不支持的方法：{other}"))),
        };

        match outcome {
            Ok(result) => vec![protocol::success(&id, result)],
            Err((code, message)) => vec![protocol::failure(Some(&id), code, &message)],
        }
    }

    /// initialize：记录客户端能力，按能力重建名称表，回服务端信息。
    fn initialize(&mut self, params: &Value) -> Result<Value, (i64, String)> {
        if let Some(name) = params
            .get("clientInfo")
            .and_then(|info| info.get("name"))
            .and_then(Value::as_str)
            .filter(|name| !name.trim().is_empty())
        {
            self.client_name = name.trim().to_string();
        }
        self.client_elicitation = params
            .get("capabilities")
            .is_some_and(|caps| caps.get("elicitation").is_some());

        // 名称表按「这次连接实际可授权的工具」建：支持 elicitation 的客户端
        // 能看到（策略放行的）需要确认的工具；无确认能力的看不到。
        let caps = ClientCapabilities {
            confirmation: self.client_elicitation,
        };
        let gateway = ToolGateway::new(
            &self.catalog,
            &self.policy,
            &self.auditor,
            caps,
            self.client_name.clone(),
        );
        self.names = NameRegistry::build(gateway.list());
        self.initialized = true;

        // 协议版本：只实现 2025-06-18。规范要求不支持客户端请求的版本时
        // 必须回自己支持的版本，由客户端决定是否继续；盲目回显更高版本
        // 等于宣称实现了未实现的语义（如 2025-11-25 起的 Tasks）。
        Ok(json!({
            "protocolVersion": PROTOCOL_VERSION,
            "capabilities": {"tools": {"listChanged": false}},
            "serverInfo": {
                "name": "toolbox-hub",
                "version": env!("CARGO_PKG_VERSION"),
            },
        }))
    }

    fn tools_list(&mut self) -> Result<Value, (i64, String)> {
        let caps = ClientCapabilities {
            confirmation: self.client_elicitation,
        };
        let gateway = ToolGateway::new(
            &self.catalog,
            &self.policy,
            &self.auditor,
            caps,
            self.client_name.clone(),
        );
        let tools: Vec<Value> = gateway
            .list()
            .iter()
            .map(|tool| self.tool_entry(tool))
            .collect();
        Ok(json!({"tools": tools}))
    }

    /// 一件授权工具的 MCP 描述。
    fn tool_entry(&self, tool: &ToolDefinition) -> Value {
        let effect = effects::infer(tool);
        let description = tool
            .mcp
            .as_ref()
            .and_then(|exposure| {
                exposure
                    .description
                    .clone()
                    .filter(|text| !text.trim().is_empty())
            })
            .unwrap_or_else(|| {
                if tool.summary == "-" {
                    tool.name.clone()
                } else {
                    tool.summary.clone()
                }
            });
        json!({
            "name": self.names.name_of(&tool.id).unwrap_or(&tool.id),
            "description": description,
            "inputSchema": input_schema(tool),
            "annotations": {
                // readOnlyHint 只是**提示**（计划书 §5.1）：真正的授权在 mcp.toml。
                "readOnlyHint": effect.read_only() && !tool.danger.needs_confirm(),
            },
        })
    }

    /// tools/call：名称 → 取值 → 确认 → 网关。
    fn call_tool(&mut self, params: &Value, elicit: Elicit<'_>) -> Result<Value, (i64, String)> {
        let name = params
            .get("name")
            .and_then(Value::as_str)
            .ok_or((INVALID_PARAMS, String::from("缺少工具名 name")))?;
        let Some(id) = self.names.resolve(name).map(str::to_string) else {
            // 名字表只含授权工具：不在表里 = 未知或未授权，都不给看区别。
            return Err((INVALID_PARAMS, format!("未知工具：{name}")));
        };
        let Some(tool) = self.catalog.get(&id).cloned() else {
            return Err((INVALID_PARAMS, format!("未知工具：{name}")));
        };

        let values = values_from_json(&tool, params.get("arguments"))
            .map_err(|message| (INVALID_PARAMS, message))?;
        let audit_arguments = redacted_arguments(tool.action.as_ref(), &values);

        // 第二层确认：需要人工确认时，先走 elicitation。客户端没这个能力就让
        // 网关去拒绝（confirmation_unsupported）—— 与 P2 的行为完全一致。
        let mut confirmed = false;
        let effect = effects::infer(&tool);
        if approval::requirement(&tool, effect) == approval::Requirement::Confirmation {
            if !self.client_elicitation {
                self.auditor.denied(
                    &id,
                    &self.client_name,
                    "confirmation_unsupported",
                    audit_arguments,
                );
                return Ok(tool_error(
                    format!("{id} 需要人工确认，但客户端不支持确认能力（elicitation）"),
                    "confirmation_unsupported",
                ));
            }
            self.next_request_id += 1;
            let request_id = self.next_request_id;
            let request = json!({
                "mode": "form",
                "message": confirmation_message(&tool, &effect),
                "requestedSchema": {"type": "object", "properties": {}, "required": []},
            });
            match elicit(request_id, "elicitation/create", request) {
                Some(result) if result.get("action").and_then(Value::as_str) == Some("accept") => {
                    confirmed = true;
                }
                _ => {
                    self.auditor.denied(
                        &id,
                        &self.client_name,
                        "declined_by_user",
                        audit_arguments,
                    );
                    return Ok(tool_error(
                        format!("{id} 未获用户确认，没有执行"),
                        "declined_by_user",
                    ));
                }
            }
        }

        let caps = ClientCapabilities {
            confirmation: confirmed,
        };
        let gateway = ToolGateway::new(
            &self.catalog,
            &self.policy,
            &self.auditor,
            caps,
            self.client_name.clone(),
        );
        // 工作目录 = 服务端进程的启动目录（MCP 客户端没有「当前目录」概念）。
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        match gateway.call(&id, &values, &cwd) {
            Ok(result) => Ok(execution_result(&result)),
            Err(ServiceError::UnknownTool(_)) => Err((INVALID_PARAMS, format!("未知工具：{name}"))),
            Err(error) => Ok(tool_error(error.to_string(), error.kind())),
        }
    }
}

/// 需要确认时的 elicitation 文案：把副作用说清楚，别只写「要执行一条命令」。
fn confirmation_message(tool: &ToolDefinition, effect: &crate::model::ToolEffects) -> String {
    let mut effects: Vec<&str> = Vec::new();
    if effect.filesystem_write {
        effects.push("写文件");
    }
    if effect.network_access {
        effects.push("访问网络");
    }
    if effect.system_mutation {
        effects.push("修改系统状态");
    }
    if effect.requires_privilege {
        effects.push("需要提权");
    }
    if effect.destructive {
        effects.push("破坏性操作");
    }
    if effect.arbitrary_command {
        effects.push("通用命令执行");
    }
    if effect.interactive {
        effects.push("需要终端交互");
    }
    if effects.is_empty() {
        effects.push("非只读操作");
    }
    format!(
        "MCP 客户端请求执行工具「{}」（{}）。\n副作用：{}。\n允许执行吗？",
        tool.name,
        tool.id,
        effects.join("、")
    )
}

/// 执行成功/失败（程序真的跑过了）→ MCP result。
fn execution_result(result: &ExecutionResult) -> Value {
    let mut text = format!("$ {}\n", result.command);
    if !result.stdout.is_empty() {
        text.push_str("\n--- stdout ---\n");
        text.push_str(&result.stdout);
    }
    if !result.stderr.is_empty() {
        text.push_str("\n--- stderr ---\n");
        text.push_str(&result.stderr);
    }
    text.push_str("\n\n[");
    text.push_str(&result.summary());
    text.push(']');

    let structured = json!({
        "tool_id": result.tool_id,
        "tool_name": result.tool_name,
        // command 在服务层生成时已做过敏感值脱敏（ExecutionRequest::secrets）。
        "command": result.command,
        "stdout": result.stdout,
        "stderr": result.stderr,
        "exit_code": result.exit_code,
        "success": result.success,
        "timed_out": result.timed_out,
        "cancelled": result.cancelled,
        "truncated": result.truncated,
        "elapsed_ms": result.elapsed.as_millis().min(u128::from(u64::MAX)) as u64,
    });
    json!({
        "content": [{"type": "text", "text": text}],
        "structuredContent": structured,
        "isError": !result.success,
    })
}

/// 程序**没有**运行的失败（权限、参数、确认）→ `isError: true` 的 result。
///
/// 按 MCP 规范，这类「工具自身的错误」用 result 报告，不用协议错误——
/// 协议错误只留给坏消息本身（未知方法、参数形状不对）。
fn tool_error(message: String, kind: &str) -> Value {
    json!({
        "content": [{"type": "text", "text": message}],
        "structuredContent": {"error_kind": kind, "error": message},
        "isError": true,
    })
}

/// 供测试用的取值转换再出口（schema 的语义校验在服务层，测试会用到）。
#[allow(dead_code)]
fn convert(tool: &ToolDefinition, arguments: Option<&Value>) -> Result<TypedValues, String> {
    values_from_json(tool, arguments)
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        path::PathBuf,
        time::{SystemTime, UNIX_EPOCH},
    };

    use serde_json::{Value, json};

    use super::{McpServer, tool_error};
    use crate::{
        model::{
            Action, ArgKind, ArgPlacement, Argument, Danger, Domain, McpExposure, RunMode,
            ToolDefinition, ToolEffects,
        },
        security::{
            audit::Auditor,
            policy::{McpConfig, SafetyPolicy},
        },
        service::catalog::ToolCatalog,
    };

    fn temp_dir(tag: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("toolbox-hub-mcp-server-{tag}-{nanos}"));
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
            summary: String::from("回显一段文本"),
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

    struct Fixture {
        server: McpServer,
        dir: PathBuf,
    }

    impl Fixture {
        fn new(tools: Vec<ToolDefinition>, config: McpConfig) -> Self {
            let dir = temp_dir("fx");
            let server = McpServer::new(
                ToolCatalog::new(tools),
                SafetyPolicy::new(config),
                Auditor::new(dir.join("audit.jsonl"), true),
            );
            Self { server, dir }
        }

        fn audit_text(&self) -> String {
            fs::read_to_string(self.dir.join("audit.jsonl")).unwrap_or_default()
        }
    }

    fn cleanup(fixture: &Fixture) {
        let _ = fs::remove_dir_all(&fixture.dir);
    }

    /// 假客户端：把 elicitation 的答案直接塞回去。
    fn elicit_with(answer: &'static str) -> impl FnMut(u64, &str, Value) -> Option<Value> {
        move |_id, _method, _params| Some(json!({"action": answer}))
    }

    fn no_elicit() -> impl FnMut(u64, &str, Value) -> Option<Value> {
        |_id, _method, _params| panic!("不该触发确认请求")
    }

    fn initialize(server: &mut McpServer, elicitation: bool) -> Value {
        let caps = if elicitation {
            json!({"elicitation": {}})
        } else {
            json!({})
        };
        let line = json!({
            "jsonrpc": "2.0", "id": 1, "method": "initialize",
            "params": {
                "protocolVersion": "2025-06-18",
                "clientInfo": {"name": "tester", "version": "0"},
                "capabilities": caps,
            }
        });
        let responses = server.handle(&line.to_string(), &mut no_elicit());
        assert_eq!(responses.len(), 1);
        serde_json::from_str(&responses[0]).expect("响应是合法 JSON")
    }

    #[test]
    fn initialize_reports_server_info_and_capabilities() {
        let mut fixture = Fixture::new(vec![tool("test:echo", false)], config(""));
        let response = initialize(&mut fixture.server, false);
        assert_eq!(response["result"]["serverInfo"]["name"], "toolbox-hub");
        assert_eq!(response["result"]["protocolVersion"], "2025-06-18");
        assert_eq!(
            response["result"]["capabilities"]["tools"]["listChanged"],
            false
        );
        cleanup(&fixture);
    }

    #[test]
    fn requests_before_initialize_are_refused() {
        let mut fixture = Fixture::new(vec![tool("test:echo", false)], config(""));
        let responses = fixture.server.handle(
            r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#,
            &mut no_elicit(),
        );
        let parsed: Value = serde_json::from_str(&responses[0]).expect("合法");
        assert_eq!(parsed["error"]["code"], -32600);
        cleanup(&fixture);
    }

    #[test]
    fn tools_list_only_contains_authorized_tools_with_schemas() {
        let mut fixture = Fixture::new(
            vec![
                tool("test:echo", false),
                tool("test:other", false),
                tool("test:bare", false),
            ],
            config(""),
        );
        initialize(&mut fixture.server, false);
        let responses = fixture.server.handle(
            r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#,
            &mut no_elicit(),
        );
        let parsed: Value = serde_json::from_str(&responses[0]).expect("合法");
        let tools = parsed["result"]["tools"].as_array().expect("数组");
        assert_eq!(tools.len(), 1, "只有 test:echo 在 allow 名单里");
        assert_eq!(tools[0]["name"], "toolbox_test_echo");
        assert_eq!(
            tools[0]["inputSchema"]["properties"]["text"]["type"],
            "string"
        );
        assert_eq!(tools[0]["annotations"]["readOnlyHint"], true);
        cleanup(&fixture);
    }

    #[test]
    fn a_call_round_trips_through_the_gateway() {
        let mut fixture = Fixture::new(vec![tool("test:echo", false)], config(""));
        initialize(&mut fixture.server, false);
        let line = json!({
            "jsonrpc": "2.0", "id": 3, "method": "tools/call",
            "params": {"name": "toolbox_test_echo", "arguments": {"text": "中文 参数"}}
        });
        let responses = fixture.server.handle(&line.to_string(), &mut no_elicit());
        assert_eq!(responses.len(), 1);
        let parsed: Value = serde_json::from_str(&responses[0]).expect("合法");
        assert_eq!(parsed["result"]["isError"], false);
        assert!(
            parsed["result"]["content"][0]["text"]
                .as_str()
                .expect("text")
                .contains("中文 参数")
        );
        let structured = &parsed["result"]["structuredContent"];
        assert_eq!(structured["tool_id"], "test:echo");
        assert_eq!(structured["success"], true);
        assert!(fixture.audit_text().contains("\"decision\":\"completed\""));
        cleanup(&fixture);
    }

    #[test]
    fn unknown_and_unauthorized_names_are_invalid_params() {
        let mut fixture = Fixture::new(vec![tool("test:echo", false)], config(""));
        initialize(&mut fixture.server, false);

        for name in ["toolbox_nothing", "test:echo"] {
            let line = json!({
                "jsonrpc": "2.0", "id": 4, "method": "tools/call",
                "params": {"name": name, "arguments": {}}
            });
            let responses = fixture.server.handle(&line.to_string(), &mut no_elicit());
            let parsed: Value = serde_json::from_str(&responses[0]).expect("合法");
            assert_eq!(parsed["error"]["code"], -32602, "{name}");
        }
        cleanup(&fixture);
    }

    #[test]
    fn bad_argument_shapes_are_protocol_errors_but_semantic_failures_are_tool_errors() {
        let mut required = tool("test:echo", false);
        let action = required.action.as_mut().expect("action");
        action.arguments[0].required = true;
        action.arguments[0].kind = ArgKind::Path;
        let mut fixture = Fixture::new(vec![required], config(""));
        initialize(&mut fixture.server, false);

        // 形状错（多传字段）→ 协议错误。
        let line = json!({
            "jsonrpc": "2.0", "id": 5, "method": "tools/call",
            "params": {"name": "toolbox_test_echo", "arguments": {"nope": "x"}}
        });
        let responses = fixture.server.handle(&line.to_string(), &mut no_elicit());
        let parsed: Value = serde_json::from_str(&responses[0]).expect("合法");
        assert_eq!(parsed["error"]["code"], -32602);

        // 语义错（必填缺失）→ isError result，程序没跑。
        let line = json!({
            "jsonrpc": "2.0", "id": 6, "method": "tools/call",
            "params": {"name": "toolbox_test_echo", "arguments": {}}
        });
        let responses = fixture.server.handle(&line.to_string(), &mut no_elicit());
        let parsed: Value = serde_json::from_str(&responses[0]).expect("合法");
        assert_eq!(parsed["result"]["isError"], true);
        assert_eq!(
            parsed["result"]["structuredContent"]["error_kind"],
            "invalid_arguments"
        );
        assert!(fixture.audit_text().contains("\"authorized\":false"));
        cleanup(&fixture);
    }

    /// 敏感值：执行用真值（echo 当然会把原值打印回来），但**命令摘要**与
    /// **审计**里只能是 `***` —— stdout 是工具自己的输出，不属于脱敏范围。
    #[test]
    fn sensitive_values_are_masked_in_command_and_audit() {
        let mut fixture = Fixture::new(vec![tool("test:echo", true)], config(""));
        initialize(&mut fixture.server, false);
        let line = json!({
            "jsonrpc": "2.0", "id": 7, "method": "tools/call",
            "params": {"name": "toolbox_test_echo", "arguments": {"text": "s3cr3t-token"}}
        });
        let responses = fixture.server.handle(&line.to_string(), &mut no_elicit());
        let parsed: Value = serde_json::from_str(&responses[0]).expect("合法");
        let structured = &parsed["result"]["structuredContent"];
        assert!(
            !structured["command"]
                .as_str()
                .expect("command")
                .contains("s3cr3t-token"),
            "{}",
            structured["command"]
        );
        assert!(
            structured["command"]
                .as_str()
                .expect("command")
                .contains("***")
        );
        assert!(!fixture.audit_text().contains("s3cr3t-token"));
        cleanup(&fixture);
    }

    #[test]
    fn confirmation_uses_elicitation_and_respects_the_answer() {
        // 需要 confirm 的工具：Caution + 策略允许 caution（默认 false，这里显式开）。
        let mut caution = tool("test:echo", false);
        caution.danger = Danger::Caution;
        let mut fixture = Fixture::new(
            vec![caution],
            config("[execution]\nallow_caution = true\nallow_filesystem_write = true\n"),
        );

        // 无 elicitation 能力的客户端：需要确认的工具根本进不了工具列表，
        // 所以按「未知工具」拒绝 —— 列表里都没有，AI 也无从调用。
        initialize(&mut fixture.server, false);
        let line = json!({
            "jsonrpc": "2.0", "id": 8, "method": "tools/call",
            "params": {"name": "toolbox_test_echo", "arguments": {"text": "hi"}}
        });
        let responses = fixture.server.handle(&line.to_string(), &mut no_elicit());
        let parsed: Value = serde_json::from_str(&responses[0]).expect("合法");
        assert_eq!(parsed["error"]["code"], -32602);

        // 支持 elicitation 且用户拒绝 → 不执行。
        let mut caution = tool("test:echo", false);
        caution.danger = Danger::Caution;
        let mut fixture = Fixture::new(
            vec![caution],
            config("[execution]\nallow_caution = true\nallow_filesystem_write = true\n"),
        );
        initialize(&mut fixture.server, true);
        let responses = fixture
            .server
            .handle(&line.to_string(), &mut elicit_with("decline"));
        let parsed: Value = serde_json::from_str(&responses[0]).expect("合法");
        assert_eq!(
            parsed["result"]["structuredContent"]["error_kind"],
            "declined_by_user"
        );
        assert!(fixture.audit_text().contains("declined_by_user"));
        cleanup(&fixture);

        // 用户接受 → 真执行。声明「无副作用」也抹不掉 Caution 的确认要求，
        // 所以这里同时验证 elicitation 文案里写明了副作用。
        let mut accepted = tool("test:echo", false);
        accepted.danger = Danger::Caution;
        accepted.mcp = Some(McpExposure {
            effects: Some(ToolEffects::default()),
            ..McpExposure::default()
        });
        let mut fixture = Fixture::new(
            vec![accepted],
            config("[execution]\nallow_caution = true\nallow_filesystem_write = true\n"),
        );
        initialize(&mut fixture.server, true);
        let mut asked = false;
        let responses = fixture
            .server
            .handle(&line.to_string(), &mut |_id, method, params| {
                asked = true;
                assert_eq!(method, "elicitation/create");
                let message = params["message"].as_str().expect("message");
                assert!(message.contains("副作用"), "{message}");
                Some(json!({"action": "accept"}))
            });
        let parsed: Value = serde_json::from_str(&responses[0]).expect("合法");
        assert_eq!(parsed["result"]["isError"], false);
        assert!(asked, "必须真的发过确认请求");
        cleanup(&fixture);
    }

    #[test]
    fn ping_and_unknown_methods() {
        let mut fixture = Fixture::new(vec![tool("test:echo", false)], config(""));
        initialize(&mut fixture.server, false);

        let responses = fixture.server.handle(
            r#"{"jsonrpc":"2.0","id":10,"method":"ping"}"#,
            &mut no_elicit(),
        );
        let parsed: Value = serde_json::from_str(&responses[0]).expect("合法");
        assert_eq!(parsed["result"], json!({}));

        let responses = fixture.server.handle(
            r#"{"jsonrpc":"2.0","id":11,"method":"resources/list"}"#,
            &mut no_elicit(),
        );
        let parsed: Value = serde_json::from_str(&responses[0]).expect("合法");
        assert_eq!(parsed["error"]["code"], -32601);
        cleanup(&fixture);
    }

    #[test]
    fn notifications_never_get_a_response() {
        let mut fixture = Fixture::new(vec![tool("test:echo", false)], config(""));
        assert!(
            fixture
                .server
                .handle(
                    r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
                    &mut no_elicit()
                )
                .is_empty()
        );
        // 未知通知同样安静。
        assert!(
            fixture
                .server
                .handle(
                    r#"{"jsonrpc":"2.0","method":"notifications/whatever"}"#,
                    &mut no_elicit()
                )
                .is_empty()
        );
        cleanup(&fixture);
    }

    #[test]
    fn tool_error_result_shape() {
        let value = tool_error(String::from("boom"), "execution");
        assert_eq!(value["isError"], true);
        assert_eq!(value["structuredContent"]["error_kind"], "execution");
    }

    /// 间接验证 `convert` 出口与 schema 一致（防以后重构把入口删了没人发现）。
    #[test]
    fn values_conversion_is_exposed_for_callers() {
        let tool = tool("test:echo", false);
        let values = super::convert(&tool, Some(&json!({"text": "hi"}))).expect("转换");
        assert_eq!(
            values.get("text"),
            Some(&crate::model::ArgumentValue::Text(String::from("hi")))
        );
    }
}
