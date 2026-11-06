//! MCP stdio 协议集成测试：**真实子进程**，不 mock。
//!
//! 计划书 P3-05 要求「使用真实 MCP 客户端和独立协议集成测试验证，不能只测试
//! 自写 Dispatcher」。这里把编译出来的二进制当黑盒启动（`mcp stdio`），按行
//! 说 JSON-RPC，覆盖：
//!
//! 1. 协议协商（initialize / serverInfo）；
//! 2. `tools/list`：只出现授权工具、Schema 正确；
//! 3. `tools/call`：真执行、中文与特殊字符参数原样进 argv；
//! 4. 未知工具 → JSON-RPC 错误；
//! 5. 缺必填参数 → `isError` 结果，且程序没跑；
//! 6. 客户端关 stdin 后服务端干净退出（无孤儿进程）。

use std::{
    io::{BufRead, BufReader, Write},
    process::{Child, ChildStdin, Command, Stdio},
};

use serde_json::{Value, json};

/// 临时配置目录（manifest + mcp.toml）与数据目录。
struct Sandbox {
    config: std::path::PathBuf,
    data: std::path::PathBuf,
}

impl Sandbox {
    fn new(tag: &str) -> Self {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let root = std::env::temp_dir().join(format!("toolbox-hub-mcp-it-{tag}-{nanos}"));
        let config = root.join("config");
        let data = root.join("data");
        std::fs::create_dir_all(config.join("tools.d")).expect("mkdir config/tools.d");
        std::fs::create_dir_all(&data).expect("mkdir data");

        // 一件 capture + safe 的 echo 动作，显式声明 mcp。
        std::fs::write(
            config.join("tools.d/mcpdemo.toml"),
            r#"[[action]]
id = "mcp-demo"
name = "MCP 演示"
summary = "回显一段文本"
domain = "系统"
program = "echo"
mode = "capture"
danger = "safe"
mcp = { enabled = true, description = "回显一段文本", effects = { filesystem_read = true } }

[[action.argument]]
key = "text"
label = "文本"
kind = "text"
required = true
"#,
        )
        .expect("写 manifest");

        // 白名单只放行这一件。
        std::fs::write(
            config.join("mcp.toml"),
            r#"[mcp]
enabled = true

[exposure]
default = "deny"
allow = ["manifest:mcp-demo"]
deny = []

[limits]
default_timeout_secs = 15
max_timeout_secs = 30
"#,
        )
        .expect("写 mcp.toml");

        Self { config, data }
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(self.config.parent().expect("root"));
    }
}

struct Server {
    child: Child,
    stdin: ChildStdin,
    reader: BufReader<std::process::ChildStdout>,
    next_id: u64,
}

impl Server {
    fn spawn(sandbox: &Sandbox, capabilities: Value) -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_toolbox-hub"))
            .arg("mcp")
            .arg("stdio")
            .arg("--config-dir")
            .arg(&sandbox.config)
            .arg("--data-dir")
            .arg(&sandbox.data)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("启动 toolbox-hub mcp stdio");
        let stdin = child.stdin.take().expect("stdin");
        let reader = BufReader::new(child.stdout.take().expect("stdout"));
        let mut server = Self {
            child,
            stdin,
            reader,
            next_id: 0,
        };
        server.next_id += 1;
        let id = server.next_id;
        server.send(&json!({
            "jsonrpc": "2.0", "id": id, "method": "initialize",
            "params": {
                "protocolVersion": "2025-06-18",
                "clientInfo": {"name": "it-client", "version": "0"},
                "capabilities": capabilities,
            }
        }));
        let response = server.read_response();
        assert_eq!(response["id"], id, "initialize 要先得到响应");
        server.send(&json!({"jsonrpc": "2.0", "method": "notifications/initialized"}));
        server
    }

    fn send(&mut self, message: &Value) {
        writeln!(self.stdin, "{message}").expect("写请求");
        self.stdin.flush().expect("flush");
    }

    fn read_line(&mut self) -> String {
        let mut line = String::new();
        let n = self.reader.read_line(&mut line).expect("读响应");
        assert!(n > 0, "服务端提前断开（stdout）");
        line
    }

    fn read_response(&mut self) -> Value {
        serde_json::from_str(self.read_line().trim()).expect("响应是合法 JSON")
    }

    fn request(&mut self, method: &str, params: Value) -> Value {
        self.next_id += 1;
        let id = self.next_id;
        self.send(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}));
        self.read_response()
    }

    /// 关 stdin 并等待退出；返回退出码。
    fn shutdown(mut self) -> i32 {
        drop(self.stdin);
        let status = self.child.wait().expect("等待服务端退出");
        // 收走 stderr，避免子进程在管道上阻塞（顺带断言诊断不进 stdout）。
        let mut stderr = String::new();
        if let Some(mut pipe) = self.child.stderr.take() {
            use std::io::Read;
            let _ = pipe.read_to_string(&mut stderr);
        }
        status.code().unwrap_or(-1)
    }
}

#[test]
fn server_negotiates_to_the_version_it_implements() {
    let sandbox = Sandbox::new("negotiate");
    let mut child = Command::new(env!("CARGO_BIN_EXE_toolbox-hub"))
        .arg("mcp")
        .arg("stdio")
        .arg("--config-dir")
        .arg(&sandbox.config)
        .arg("--data-dir")
        .arg(&sandbox.data)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("启动 toolbox-hub mcp stdio");
    let mut stdin = child.stdin.take().expect("stdin");
    let mut reader = BufReader::new(child.stdout.take().expect("stdout"));
    writeln!(
        stdin,
        "{}",
        json!({
            "jsonrpc": "2.0", "id": 1, "method": "initialize",
            "params": {
                "protocolVersion": "2026-07-28",
                "clientInfo": {"name": "it-client", "version": "0"},
                "capabilities": {},
            }
        })
    )
    .expect("写 initialize");
    let mut line = String::new();
    reader.read_line(&mut line).expect("读 initialize 响应");
    let response: Value = serde_json::from_str(&line).expect("合法 JSON");
    assert_eq!(response["id"], 1, "{response}");
    assert_eq!(
        response["result"]["protocolVersion"], "2025-06-18",
        "不支持客户端请求的更高版本时必须回自支持版本：{response}"
    );
    drop(stdin);
    let _ = child.wait();
}

#[test]
fn full_stdio_conversation() {
    let sandbox = Sandbox::new("full");

    let mut server = Server::spawn(&sandbox, json!({}));

    // tools/list：只有授权的那一件，名字是规范化后的 MCP 工具名。
    let listed = server.request("tools/list", json!({}));
    let tools = listed["result"]["tools"].as_array().expect("tools 数组");
    assert_eq!(tools.len(), 1, "只放行 manifest:mcp-demo：{listed}");
    assert_eq!(tools[0]["name"], "toolbox_manifest_mcp_demo");
    assert_eq!(tools[0]["description"], "回显一段文本");
    assert_eq!(
        tools[0]["inputSchema"]["properties"]["text"]["type"],
        "string"
    );
    assert_eq!(tools[0]["inputSchema"]["required"], json!(["text"]));
    assert_eq!(tools[0]["annotations"]["readOnlyHint"], true);

    // tools/call：中文 + 特殊字符 + 空格，原样进 argv（不经 shell、不按分隔符拆）。
    let nasty = "中文 参数 ; $(touch /tmp/mcp-it-pwned) a,b.mp4";
    let called = server.request(
        "tools/call",
        json!({"name": "toolbox_manifest_mcp_demo", "arguments": {"text": nasty}}),
    );
    let result = &called["result"];
    assert_eq!(result["isError"], false, "{called}");
    let text = result["content"][0]["text"].as_str().expect("text");
    assert!(text.contains(nasty), "输出要原样包含参数：{text}");
    let structured = &result["structuredContent"];
    assert_eq!(structured["tool_id"], "manifest:mcp-demo");
    assert_eq!(structured["success"], true);
    assert_eq!(structured["exit_code"], 0);
    assert!(
        !std::path::Path::new("/tmp/mcp-it-pwned").exists(),
        "元字符不能变成命令"
    );

    // 未知工具 → JSON-RPC 错误，不是 result。
    let unknown = server.request(
        "tools/call",
        json!({"name": "toolbox_manifest_nothing", "arguments": {}}),
    );
    assert_eq!(unknown["error"]["code"], -32602, "{unknown}");

    // 缺必填参数 → isError 结果，结构化错误说明原因（程序没跑）。
    let missing = server.request(
        "tools/call",
        json!({"name": "toolbox_manifest_mcp_demo", "arguments": {}}),
    );
    assert_eq!(missing["result"]["isError"], true, "{missing}");
    assert_eq!(
        missing["result"]["structuredContent"]["error_kind"],
        "invalid_arguments"
    );

    // ping 与未知方法。
    let ping = server.request("ping", json!({}));
    assert_eq!(ping["result"], json!({}));
    let unsupported = server.request("prompts/list", json!({}));
    assert_eq!(unsupported["error"]["code"], -32601);

    // 审计落盘：一次 completed、一次 denied（缺必填）。
    let audit = std::fs::read_to_string(sandbox.data.join("mcp-audit.jsonl")).expect("审计文件");
    assert!(audit.contains("\"decision\":\"completed\""));
    assert!(audit.contains("\"decision\":\"denied\""));

    // 干净退出。
    assert_eq!(server.shutdown(), 0, "正常退出码 0");
}

#[test]
fn disabled_mcp_lists_nothing() {
    let sandbox = Sandbox::new("disabled");
    std::fs::write(sandbox.config.join("mcp.toml"), "[mcp]\nenabled = false\n")
        .expect("写 mcp.toml");

    let mut server = Server::spawn(&sandbox, json!({}));
    let listed = server.request("tools/list", json!({}));
    assert_eq!(
        listed["result"]["tools"].as_array().expect("数组").len(),
        0,
        "未启用 = 什么都没有（fail-closed）"
    );
    assert_eq!(server.shutdown(), 0);
}
