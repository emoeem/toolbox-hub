//! MCP stdio 协议集成测试：**真实子进程**，不 mock。
//!
//! 计划书 P3-05 要求「使用真实 MCP 客户端和独立协议集成测试验证，不能只测试
//! 自写 Dispatcher」。这里把编译出来的二进制当黑盒启动（`mcp stdio`），按行
//! 说 JSON-RPC，覆盖：
//!
//! 1. 协议协商（initialize / serverInfo / 双版本降级与 Tasks 能力）；
//! 2. `tools/list`：只出现授权工具、Schema 正确、taskSupport 协商；
//! 3. `tools/call`：真执行、中文与特殊字符参数原样进 argv；
//! 4. 未知工具 → JSON-RPC 错误；
//! 5. 缺必填参数 → `isError` 结果，且程序没跑；
//! 6. MCP Tasks（P4-01）：创建 / 进度 / 完成 / 取消 / 终态不可再取消 /
//!    related-task 元数据 / 并发预算占用 / 2025-06-18 客户端忽略 task 字段；
//! 7. 无孤儿：取消与断开连接都把工具拉起的整个进程组收掉（marker 文件验证）。

use std::{
    io::{BufRead, BufReader, Write},
    process::{Child, ChildStdin, Command, Stdio},
    time::{Duration, Instant},
};

use serde_json::{Value, json};

/// related-task 元数据键（与实现一致，这里独立写死防两边一起漂）。
const RELATED_TASK: &str = "io.modelcontextprotocol/related-task";

/// 临时配置目录（manifest + mcp.toml）与数据目录。
struct Sandbox {
    config: std::path::PathBuf,
    data: std::path::PathBuf,
    root: std::path::PathBuf,
    /// P5-01：精选 Recipe 目录（`write_recipes` 填充内容）。
    recipes_dir: std::path::PathBuf,
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
        let recipes_dir = root.join("recipes");
        std::fs::create_dir_all(config.join("tools.d")).expect("mkdir config/tools.d");
        std::fs::create_dir_all(&data).expect("mkdir data");
        std::fs::create_dir_all(&recipes_dir).expect("mkdir recipes");

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

        // P4-01 慢任务脚本：起一个 sleep 孙子进程并把它的 pid 写进文件（marker），
        // 然后 wait —— 取消/断开时整个进程组要被收掉，用 /proc 验证 pid 消失。
        let script = root.join("slowtask.sh");
        let pid_file = root.join("slowtask.pid");
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\nsleep \"$1\" &\nprintf '%s\\n' $! > {}\necho started\nwait\n",
                pid_file.display()
            ),
        )
        .expect("写慢任务脚本");
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755))
            .expect("chmod 慢任务脚本");

        std::fs::write(
            config.join("tools.d/mcpslow.toml"),
            format!(
                r#"[[action]]
id = "mcp-slow"
name = "MCP 慢任务"
summary = "睡上给定的秒数"
domain = "系统"
program = "{}"
mode = "capture"
danger = "safe"
mcp = {{ enabled = true, description = "慢任务", effects = {{ filesystem_read = true }} }}

[[action.argument]]
key = "seconds"
label = "秒数"
kind = "text"
required = true
"#,
                script.display()
            ),
        )
        .expect("写慢任务 manifest");

        // 白名单先只放行 echo；Tasks 用例自己改写 mcp.toml 放行慢任务。
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

        Self {
            config,
            data,
            root,
            recipes_dir,
        }
    }

    /// P5-01：往 Recipe 目录里写两条 Recipe + index.json。
    ///
    /// * `it-echo`（safe）：echo 一段文本 —— Loader 应把它注册成可调用工具；
    /// * `it-caution`（caution，tar 写文件）：不进 allow，只用来验证默认拒绝。
    ///
    /// 索引行不带 sha256（Loader 对缺哈希的行跳过完整性比对；真 featured 目录
    /// 的哈希口径由 `recipe check` / `build-featured.py --check` 把守）。
    fn write_recipes(&self) {
        let echo = serde_json::json!({
            "id": "it-echo",
            "title": "集成测试回显",
            "description": "把参数原样 echo 回来",
            "command": "echo",
            "params": [{"name": "text", "description": "文本", "required": true}],
            "args": ["{text}"],
            "safety": "safe",
            "source": "toolbox-hub/featured",
            "tags": ["工具"],
        });
        let caution = serde_json::json!({
            "id": "it-caution",
            "title": "集成测试打包",
            "description": "tar 打包一个目录（写文件，caution）",
            "command": "tar",
            "params": [
                {"name": "output", "description": "输出", "required": true},
                {"name": "parent", "description": "父目录", "required": true},
                {"name": "name", "description": "名字", "required": true},
            ],
            "args": ["-czf", "{output}", "-C", "{parent}", "{name}"],
            "safety": "caution",
            "source": "toolbox-hub/featured",
            "tags": ["工具"],
        });
        for (file, recipe) in [("it-echo.json", &echo), ("it-caution.json", &caution)] {
            std::fs::write(
                self.recipes_dir.join(file),
                serde_json::to_string_pretty(recipe).expect("序列化 recipe"),
            )
            .expect("写 recipe");
        }
        std::fs::write(
            self.recipes_dir.join("index.json"),
            serde_json::json!({
                "schema_version": 1,
                "recipes": [
                    {"id": "it-echo", "file": "it-echo.json"},
                    {"id": "it-caution", "file": "it-caution.json"},
                ],
            })
            .to_string(),
        )
        .expect("写 index");
    }

    /// 给慢任务放行（Tasks 用例用）。
    fn allow_slow(&self) {
        std::fs::write(
            self.config.join("mcp.toml"),
            r#"[mcp]
enabled = true

[exposure]
default = "deny"
allow = ["manifest:mcp-demo", "manifest:mcp-slow"]
deny = []

[limits]
default_timeout_secs = 15
max_timeout_secs = 30
"#,
        )
        .expect("改写 mcp.toml");
    }

    fn audit_text(&self) -> String {
        std::fs::read_to_string(self.data.join("mcp-audit.jsonl")).unwrap_or_default()
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
    /// 启动服务端并按指定协议版本完成 initialize。
    fn spawn(sandbox: &Sandbox, version: &str, capabilities: Value) -> Self {
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
                "protocolVersion": version,
                "clientInfo": {"name": "it-client", "version": "0"},
                "capabilities": capabilities,
            }
        }));
        let response = server.read_until_response().0;
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

    /// 读到下一条**响应**为止；沿途的通知（进度等）原样收集返回。
    /// 服务端把通知排在响应之后写出，顺序无关紧要 —— 通知不会顶掉响应。
    fn read_until_response(&mut self) -> (Value, Vec<Value>) {
        let mut notifications = Vec::new();
        loop {
            let line = self.read_line();
            let value: Value = serde_json::from_str(line.trim()).expect("消息是合法 JSON");
            if value.get("method").is_some() {
                notifications.push(value);
                continue;
            }
            return (value, notifications);
        }
    }

    fn request(&mut self, method: &str, params: Value) -> Value {
        self.request_collecting(method, params).0
    }

    /// 发请求并返回（响应，沿途通知）。通知由服务端排在响应之后写出，
    /// 可能滞留到下一次读取 —— 这里一并收回来。
    fn request_collecting(&mut self, method: &str, params: Value) -> (Value, Vec<Value>) {
        self.next_id += 1;
        let id = self.next_id;
        self.send(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}));
        self.read_until_response()
    }

    /// 轮询 tasks/get 直到任务到达期望状态（Tasks 的 get 是非阻塞轮询模型）。
    fn poll_task_status(&mut self, task_id: &str, want: &str) -> Value {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let response = self.request("tasks/get", json!({"taskId": task_id}));
            if response["result"]["status"] == want {
                return response["result"].clone();
            }
            assert!(
                Instant::now() < deadline,
                "任务 {task_id} 没有在期限内到达 {want}：{response}"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
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

/// 这个 pid 还活着吗：/proc 不在即死；僵尸态（Z）视为已死（本测试进程
/// 不充当 PID 1，别把被杀后没人收尸的僵尸当活口）。
fn alive(pid: u32) -> bool {
    match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        Ok(stat) => stat
            .rsplit_once(')')
            .and_then(|(_, rest)| rest.split_whitespace().next())
            .is_some_and(|state| state != "Z"),
        Err(_) => false,
    }
}

/// 等慢任务把孙子进程 pid 写进 marker 文件（证明子进程真起来了）。
fn wait_for_grandchild_pid(sandbox: &Sandbox) -> u32 {
    let pid_file = sandbox.root.join("slowtask.pid");
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Ok(text) = std::fs::read_to_string(&pid_file)
            && let Ok(pid) = text.trim().parse::<u32>()
        {
            return pid;
        }
        assert!(Instant::now() < deadline, "慢任务没在期限内写出 pid marker");
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn server_negotiates_to_the_version_it_implements() {
    let sandbox = Sandbox::new("negotiate");
    // 2026-07-28 客户端 → 降到 2025-11-25（Tasks 版本），并声明 Tasks 能力。
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
        response["result"]["protocolVersion"], "2025-11-25",
        "请求 2026-07-28 时回自支持的 Tasks 版本：{response}"
    );
    let capabilities = &response["result"]["capabilities"];
    assert_eq!(capabilities["tasks"]["cancel"], json!({}), "{response}");
    assert_eq!(
        capabilities["tasks"]["requests"]["tools"]["call"],
        json!({}),
        "{response}"
    );
    assert!(
        capabilities["tasks"].get("list").is_none(),
        "stdio 无法识别请求方，不声明 tasks.list：{response}"
    );
    drop(stdin);
    let _ = child.wait();

    // 2025-06-18 客户端 → 维持现状，不声明 Tasks 能力。
    let mut server = Server::spawn(&sandbox, "2025-06-18", json!({}));
    let listed = server.request("tools/list", json!({}));
    for tool in listed["result"]["tools"].as_array().expect("数组") {
        assert!(
            tool.get("execution").is_none(),
            "旧版本客户端不认 execution.taskSupport：{tool}"
        );
    }
    assert_eq!(server.shutdown(), 0);
}

#[test]
fn full_stdio_conversation() {
    let sandbox = Sandbox::new("full");

    let mut server = Server::spawn(&sandbox, "2025-06-18", json!({}));

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

    let mut server = Server::spawn(&sandbox, "2025-06-18", json!({}));
    let listed = server.request("tools/list", json!({}));
    assert_eq!(
        listed["result"]["tools"].as_array().expect("数组").len(),
        0,
        "未启用 = 什么都没有（fail-closed）"
    );
    assert_eq!(server.shutdown(), 0);
}

/// P4-01 + P4-03 全链路：创建（CreateTaskResult）→ 完成后 tasks/result 带
/// related-task → 慢任务创建 → 1 秒节流的 notifications/progress（无 total、
/// 单调递增）→ tasks/cancel 立即转 cancelled → 终态再取消 -32602 → marker
/// 文件证明进程组被收掉 → 取消后的 tasks/result 是 isError 的底层响应。
#[test]
fn mcp_tasks_lifecycle_with_progress_and_cancel() {
    let sandbox = Sandbox::new("tasks");
    sandbox.allow_slow();
    let mut server = Server::spawn(&sandbox, "2025-11-25", json!({}));

    // ── 1. echo 任务：创建 → completed → 结果带 related-task ──────────────
    let created = server.request(
        "tools/call",
        json!({
            "name": "toolbox_manifest_mcp_demo",
            "arguments": {"text": "任务回显"},
            "task": {},
            "_meta": {"progressToken": "tok-echo"},
        }),
    );
    let task = &created["result"]["task"];
    assert_eq!(task["status"], "working", "{created}");
    let task_id = task["taskId"].as_str().expect("taskId").to_string();
    assert_eq!(task_id.len(), 32, "不可猜测的 id：{task_id}");
    assert_eq!(task["createdAt"].as_str().map(str::len), Some(20));
    assert_eq!(task["ttl"], Value::Null, "未限时 → null");
    assert_eq!(task["pollInterval"], 1000);
    assert!(
        created["result"].get("content").is_none(),
        "CreateTaskResult 不含工具结果：{created}"
    );

    let finished = server.poll_task_status(&task_id, "completed");
    assert_eq!(finished["taskId"], task_id.as_str());

    let result = server.request("tasks/result", json!({"taskId": task_id}));
    let payload = &result["result"];
    assert_eq!(payload["isError"], false, "{result}");
    assert!(
        payload["content"][0]["text"]
            .as_str()
            .expect("text")
            .contains("任务回显")
    );
    assert_eq!(
        payload["_meta"][RELATED_TASK]["taskId"],
        task_id.as_str(),
        "tasks/result 必须带 related-task：{result}"
    );

    // ── 2. 慢任务：创建 → 等它真跑起来（marker）→ 进度 → 取消 ─────────────
    let created = server.request(
        "tools/call",
        json!({
            "name": "toolbox_manifest_mcp_slow",
            "arguments": {"seconds": "30"},
            "task": {"ttl": 60000},
            "_meta": {"progressToken": "tok-slow"},
        }),
    );
    let slow_task = &created["result"]["task"];
    assert_eq!(slow_task["status"], "working", "{created}");
    let slow_id = slow_task["taskId"].as_str().expect("taskId").to_string();
    assert_eq!(slow_task["ttl"], 60000, "客户端要的 ttl 原样带回");

    // marker：脚本已起 sleep 孙子进程（并发名额也已被占住）。
    let grandchild = wait_for_grandchild_pid(&sandbox);
    assert!(
        alive(grandchild),
        "取消前孙子进程该活着（pid {grandchild}）"
    );

    // 等过 1 秒进度节流，再取消。进度通知由服务端排在响应之后写出，
    // 会滞留到下一次读取 —— 取消响应之后再用一个 ping 把它们收回来。
    std::thread::sleep(Duration::from_millis(1300));
    server.next_id += 1;
    let cancel_id = server.next_id;
    server.send(&json!({
        "jsonrpc": "2.0", "id": cancel_id, "method": "tasks/cancel",
        "params": {"taskId": slow_id},
    }));
    let (cancelled, _leftovers) = server.read_until_response();
    assert_eq!(cancelled["id"], cancel_id);
    assert_eq!(
        cancelled["result"]["status"], "cancelled",
        "取消必须在响应前落终态：{cancelled}"
    );
    assert_eq!(cancelled["result"]["taskId"], slow_id.as_str());

    let (_, pending) = server.request_collecting("ping", json!({}));
    let progress_lines: Vec<Value> = _leftovers
        .into_iter()
        .chain(pending)
        .filter(|line| line["method"] == "notifications/progress")
        .collect();
    assert!(!progress_lines.is_empty(), "1 秒节流下至少应有一条进度通知");
    let mut last = 0;
    for line in &progress_lines {
        assert_eq!(line["params"]["progressToken"], "tok-slow", "{line}");
        let progress = line["params"]["progress"].as_u64().expect("progress");
        assert!(progress > last, "progress 单调递增：{progress_lines:?}");
        last = progress;
        assert!(line["params"]["message"].is_string());
        assert!(
            line["params"].get("total").is_none(),
            "没有真实总量就不给 total：{line}"
        );
        assert!(
            !line["params"]["message"]
                .as_str()
                .expect("message")
                .contains('%'),
            "不伪造百分比：{line}"
        );
    }

    // 终态再取消 → -32602。
    let again = server.request("tasks/cancel", json!({"taskId": slow_id}));
    assert_eq!(again["error"]["code"], -32602, "{again}");

    // 状态停在 cancelled，不再变。
    let status = server.poll_task_status(&slow_id, "cancelled");
    assert_eq!(status["taskId"], slow_id.as_str());

    // marker 验证：孙子进程（sleep 30）已随进程组被收掉。
    let deadline = Instant::now() + Duration::from_secs(10);
    while alive(grandchild) {
        assert!(
            Instant::now() < deadline,
            "取消后孙子进程 {grandchild} 还活着 —— 进程组没被带走"
        );
        std::thread::sleep(Duration::from_millis(20));
    }

    // 取消后的 tasks/result：底层响应（isError）+ related-task。
    let result = server.request("tasks/result", json!({"taskId": slow_id}));
    assert_eq!(result["result"]["isError"], true, "{result}");
    assert_eq!(
        result["result"]["_meta"][RELATED_TASK]["taskId"],
        slow_id.as_str(),
        "{result}"
    );

    // 审计生命周期链：created + progress + cancelled + completed 都带 task_id。
    let audit = sandbox.audit_text();
    for decision in ["created", "progress", "cancelled", "completed"] {
        assert!(
            audit.contains(&format!("\"decision\":\"{decision}\"")),
            "审计缺 {decision}：{audit}"
        );
    }
    assert!(audit.contains(&format!("\"task_id\":\"{slow_id}\"")));

    assert_eq!(server.shutdown(), 0, "干净退出");
}

/// P4-01：并发预算共享 —— 后台任务占住名额（max_concurrent=1）后，同步调用
/// 直接被拒（busy），拒绝发生在子进程之前。
#[test]
fn mcp_tasks_rejected_when_concurrency_budget_is_exhausted() {
    let sandbox = Sandbox::new("busy");
    sandbox.allow_slow();
    std::fs::write(
        sandbox.config.join("mcp.toml"),
        r#"[mcp]
enabled = true

[exposure]
default = "deny"
allow = ["manifest:mcp-demo", "manifest:mcp-slow"]
deny = []

[limits]
default_timeout_secs = 15
max_timeout_secs = 30
max_concurrent = 1
"#,
    )
    .expect("写 mcp.toml");
    let mut server = Server::spawn(&sandbox, "2025-11-25", json!({}));

    let created = server.request(
        "tools/call",
        json!({
            "name": "toolbox_manifest_mcp_slow",
            "arguments": {"seconds": "30"},
            "task": {},
        }),
    );
    let slow_id = created["result"]["task"]["taskId"]
        .as_str()
        .expect("taskId")
        .to_string();

    // marker 出现 = 后台任务已拿到唯一名额并真的跑起来了。
    let _grandchild = wait_for_grandchild_pid(&sandbox);

    let busy = server.request(
        "tools/call",
        json!({"name": "toolbox_manifest_mcp_demo", "arguments": {"text": "hi"}}),
    );
    assert_eq!(busy["result"]["isError"], true, "{busy}");
    assert_eq!(
        busy["result"]["structuredContent"]["error_kind"], "permission_denied",
        "{busy}"
    );
    assert!(
        busy["result"]["content"][0]["text"]
            .as_str()
            .expect("text")
            .contains("并发已达上限"),
        "{busy}"
    );
    assert!(sandbox.audit_text().contains("\"decision\":\"denied\""));

    // 收尾：取消慢任务再退出。
    let cancelled = server.request("tasks/cancel", json!({"taskId": slow_id}));
    assert_eq!(cancelled["result"]["status"], "cancelled");
    assert_eq!(server.shutdown(), 0);
}

/// P4-01：2025-06-18 客户端的 task 字段按普通请求处理（规范：未声明能力时
/// 忽略任务增强元数据）；tasks/* 是未知方法（-32601）。
#[test]
fn mcp_tasks_ignored_for_2025_06_18_clients() {
    let sandbox = Sandbox::new("oldversion");
    let mut server = Server::spawn(&sandbox, "2025-06-18", json!({}));

    let called = server.request(
        "tools/call",
        json!({
            "name": "toolbox_manifest_mcp_demo",
            "arguments": {"text": "同步路径"},
            "task": {"ttl": 60000},
        }),
    );
    let result = &called["result"];
    assert_eq!(result["isError"], false, "{called}");
    assert!(
        result.get("task").is_none(),
        "同步响应不该有 task 字段：{called}"
    );
    assert!(
        result["content"][0]["text"]
            .as_str()
            .expect("text")
            .contains("同步路径")
    );

    let unknown = server.request("tasks/get", json!({"taskId": "x"}));
    assert_eq!(unknown["error"]["code"], -32601, "{unknown}");
    let unknown = server.request("tasks/cancel", json!({"taskId": "x"}));
    assert_eq!(unknown["error"]["code"], -32601);

    assert_eq!(server.shutdown(), 0);
}

/// P5-02：resources/list 与 resources/read 走真实 stdio。白名单三类 URI，
/// 任意文件读取之类的 URI 一律 -32602。
#[test]
fn resources_list_and_read_over_stdio() {
    let sandbox = Sandbox::new("resources");
    let mut server = Server::spawn(&sandbox, "2025-06-18", json!({}));

    let listed = server.request("resources/list", json!({}));
    let resources = listed["result"]["resources"].as_array().expect("数组");
    assert_eq!(resources[0]["uri"], "toolbox://catalog", "{listed}");
    assert_eq!(resources[1]["uri"], "toolbox://domains");
    assert!(
        resources
            .iter()
            .any(|r| r["uri"] == "toolbox://tools/manifest:mcp-demo"),
        "授权工具要有自己的资源：{listed}"
    );
    assert!(
        !resources
            .iter()
            .any(|r| r["uri"].as_str().unwrap_or("").contains("mcp-slow")),
        "未授权工具不该出现在资源里：{listed}"
    );

    let read = server.request("resources/read", json!({"uri": "toolbox://catalog"}));
    let contents = &read["result"]["contents"];
    assert_eq!(contents[0]["uri"], "toolbox://catalog");
    assert_eq!(contents[0]["mimeType"], "application/json");
    let body: Value =
        serde_json::from_str(contents[0]["text"].as_str().expect("text")).expect("JSON 文本");
    let tool = &body["tools"][0];
    assert_eq!(tool["id"], "manifest:mcp-demo");
    assert_eq!(tool["mcp_name"], "toolbox_manifest_mcp_demo");
    assert_eq!(tool["execution_mode"], "capture");
    assert_eq!(
        tool["input_schema"]["properties"]["text"]["type"], "string",
        "参数定义要在场：{tool}"
    );
    assert!(tool["requires"].is_array(), "依赖要求要在场：{tool}");
    assert!(
        tool["effects"]["filesystem_read"].is_boolean(),
        "安全限制要在场"
    );

    let read = server.request(
        "resources/read",
        json!({"uri": "toolbox://tools/manifest:mcp-demo"}),
    );
    let detail: Value = serde_json::from_str(
        read["result"]["contents"][0]["text"]
            .as_str()
            .expect("text"),
    )
    .expect("详情是 JSON");
    assert_eq!(detail["id"], "manifest:mcp-demo");

    let read = server.request("resources/read", json!({"uri": "toolbox://domains"}));
    let body: Value = serde_json::from_str(
        read["result"]["contents"][0]["text"]
            .as_str()
            .expect("text"),
    )
    .expect("domains 是 JSON");
    assert_eq!(
        body["domains"].as_array().expect("数组").len(),
        9,
        "九个域全列出来（计数可以为 0）：{body}"
    );

    // 白名单之外：任意文件读取与未知工具都 -32602。
    for uri in [
        "file:///etc/passwd",
        "toolbox://tools/manifest:mcp-slow",
        "toolbox://nope",
    ] {
        let denied = server.request("resources/read", json!({"uri": uri}));
        assert_eq!(denied["error"]["code"], -32602, "{uri}: {denied}");
    }

    assert_eq!(server.shutdown(), 0);
}

/// P5-01：精选 Recipe 经 Loader 注册成独立 MCP 工具，走与 Manifest 相同的
/// 网关路径；S-02 的选项注入（值以 `-` 开头）在子进程之前被拒。
#[test]
fn recipe_tools_run_through_the_gateway_and_reject_option_injection() {
    let sandbox = Sandbox::new("recipe");
    sandbox.write_recipes();
    std::fs::write(
        sandbox.config.join("mcp.toml"),
        format!(
            r#"[mcp]
enabled = true

[exposure]
default = "deny"
allow = ["recipe:it-echo"]
deny = []

[recipes]
dirs = ["{}"]

[limits]
default_timeout_secs = 15
max_timeout_secs = 30
"#,
            sandbox.recipes_dir.display()
        ),
    )
    .expect("写 mcp.toml");
    let mut server = Server::spawn(&sandbox, "2025-06-18", json!({}));

    // tools/list：只出现被点名的 Recipe 工具；参数成为声明的 Schema。
    let listed = server.request("tools/list", json!({}));
    let tools = listed["result"]["tools"].as_array().expect("数组");
    assert_eq!(tools.len(), 1, "caution 那条没被 allow：{listed}");
    assert_eq!(tools[0]["name"], "toolbox_recipe_it_echo");
    assert_eq!(
        tools[0]["inputSchema"]["properties"]["text"]["type"], "string",
        "Recipe 参数要有声明：{}",
        tools[0]
    );
    assert_eq!(
        tools[0]["inputSchema"]["additionalProperties"], false,
        "多传字段要被 schema 拒"
    );

    // 正常调用：真执行，参数原样进 argv（经 Loader 转成声明参数）。
    let called = server.request(
        "tools/call",
        json!({"name": "toolbox_recipe_it_echo", "arguments": {"text": "中文 参数"}}),
    );
    let result = &called["result"];
    assert_eq!(result["isError"], false, "{called}");
    assert!(
        result["content"][0]["text"]
            .as_str()
            .expect("text")
            .contains("中文 参数")
    );
    assert_eq!(result["structuredContent"]["tool_id"], "recipe:it-echo");

    // S-02：值以 `-` 开头 → 选项注入被拒，进程没跑。（策略拒绝统一映射成
    // permission_denied，稳定 reason 在消息里，与 busy 拒绝同一形状。）
    let injected = server.request(
        "tools/call",
        json!({"name": "toolbox_recipe_it_echo", "arguments": {"text": "--version"}}),
    );
    assert_eq!(injected["result"]["isError"], true, "{injected}");
    assert_eq!(
        injected["result"]["structuredContent"]["error_kind"],
        "permission_denied"
    );
    assert!(
        injected["result"]["content"][0]["text"]
            .as_str()
            .expect("text")
            .contains("不能以 - 开头"),
        "{injected}"
    );
    // 未声明的参数同样被拒（与 Manifest 工具同一条校验路径）。
    let unknown_key = server.request(
        "tools/call",
        json!({"name": "toolbox_recipe_it_echo", "arguments": {"nope": "x"}}),
    );
    assert_eq!(unknown_key["error"]["code"], -32602, "{unknown_key}");

    // 审计里两种决定都在场。
    let audit = sandbox.audit_text();
    assert!(audit.contains("\"decision\":\"completed\""));
    assert!(audit.contains("\"decision\":\"denied\""));
    assert!(audit.contains("不能以 - 开头"));

    assert_eq!(server.shutdown(), 0);
}

/// P4-01 退出清理：客户端断开（关 stdin）后，运行中的任务被置取消旗标，
/// 整个进程组被收掉（marker 验证），服务端在 8 秒上限内干净退出。
#[test]
fn client_disconnect_cancels_running_tasks_without_orphans() {
    let sandbox = Sandbox::new("disconnect");
    sandbox.allow_slow();
    let mut server = Server::spawn(&sandbox, "2025-11-25", json!({}));

    let created = server.request(
        "tools/call",
        json!({
            "name": "toolbox_manifest_mcp_slow",
            "arguments": {"seconds": "30"},
            "task": {},
        }),
    );
    let slow_id = created["result"]["task"]["taskId"]
        .as_str()
        .expect("taskId")
        .to_string();
    let grandchild = wait_for_grandchild_pid(&sandbox);
    assert!(alive(grandchild));

    // 不发 cancel，直接断开：退出清理要接手。
    let started = Instant::now();
    let code = server.shutdown();
    let waited = started.elapsed();
    assert_eq!(code, 0, "断开后干净退出");
    assert!(
        waited < Duration::from_secs(12),
        "退出清理上限 8 秒 + 收尾，不该等满 30 秒：{waited:?}"
    );

    // 孙子进程必须已经死了（没有孤儿在闷头睡 30 秒）。
    let deadline = Instant::now() + Duration::from_secs(10);
    while alive(grandchild) {
        assert!(
            Instant::now() < deadline,
            "断开后孙子进程 {grandchild} 还活着"
        );
        std::thread::sleep(Duration::from_millis(20));
    }

    // 断开前的任务被退出清理转成 cancelled 并记审计。
    let audit = sandbox.audit_text();
    assert!(
        audit.contains(&format!("\"task_id\":\"{slow_id}\"")),
        "{audit}"
    );
}
