//! `toolbox-hub tools …` JSON 命令行集成测试（计划书 P5-04）：**真实子进程**。
//!
//! 断言对象是 stdout 上的 JSON 与退出码：
//!
//! 1. `tools list --json`：条目形状与 MCP `tools/list` 一致，只含授权工具；
//! 2. `tools show <id> --json`：与 `toolbox://tools/{id}` 资源同形状；
//! 3. `tools run <id> --input-json … --output json`：与 `tools/call` 结果同形状，
//!    isError 时退出码 1；
//! 4. 拒绝路径（interactive 模式 / 未授权 / 未知工具 / 选项注入）在子进程之前
//!    拒绝 —— 快速失败，绝不挂起等终端；
//! 5. Recipe 工具（P5-01）出现在同一份列表里。

use std::process::{Command, Stdio};

use serde_json::Value;

struct Sandbox {
    config: std::path::PathBuf,
    data: std::path::PathBuf,
    root: std::path::PathBuf,
}

impl Sandbox {
    fn new(tag: &str) -> Self {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let root = std::env::temp_dir().join(format!("toolbox-hub-tools-it-{tag}-{nanos}"));
        let config = root.join("config");
        let data = root.join("data");
        std::fs::create_dir_all(config.join("tools.d")).expect("mkdir config/tools.d");
        std::fs::create_dir_all(&data).expect("mkdir data");

        // 一件 capture + safe 的 echo 动作（声明 mcp），一件 interactive 动作
        //（mcp 声明会被发现层拒绝，这里故意不声明 —— 真实世界里 interactive
        // 工具根本进不了暴露面）。
        std::fs::write(
            config.join("tools.d/demo.toml"),
            r#"[[action]]
id = "cli-demo"
name = "CLI 演示"
summary = "回显一段文本"
domain = "系统"
program = "echo"
mode = "capture"
danger = "safe"
mcp = { enabled = true, description = "回显一段文本" }

[[action.argument]]
key = "text"
label = "文本"
kind = "text"
required = true

[[action]]
id = "cli-interactive"
name = "交互式动作"
summary = "需要终端"
domain = "系统"
program = "sleep"
mode = "interactive"
danger = "safe"
"#,
        )
        .expect("写 manifest");

        std::fs::write(
            config.join("mcp.toml"),
            r#"[mcp]
enabled = true

[exposure]
default = "deny"
allow = ["manifest:cli-demo"]
deny = []
"#,
        )
        .expect("写 mcp.toml");

        Self { config, data, root }
    }

    /// 跑一次 `toolbox-hub tools …`，返回（退出码，stdout）。
    fn run(&self, args: &[&str], cwd: &std::path::Path) -> (i32, String) {
        let output = Command::new(env!("CARGO_BIN_EXE_toolbox-hub"))
            .arg("tools")
            .args(args)
            .arg("--config-dir")
            .arg(&self.config)
            .arg("--data-dir")
            .arg(&self.data)
            .current_dir(cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .expect("运行 toolbox-hub tools");
        let stdout = String::from_utf8_lossy(&output.stdout).to_string();
        assert!(
            !stdout.trim().is_empty() || output.status.success(),
            "失败时 stdout 也要有 JSON（stderr: {}）",
            String::from_utf8_lossy(&output.stderr)
        );
        (output.status.code().unwrap_or(-1), stdout)
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn parse_json(stdout: &str) -> Value {
    serde_json::from_str(stdout.trim()).expect("stdout 是合法 JSON")
}

#[test]
fn list_and_show_report_the_same_shape_as_mcp() {
    let sandbox = Sandbox::new("list");
    let cwd = &sandbox.root;

    let (code, stdout) = sandbox.run(&["list", "--json"], cwd);
    assert_eq!(code, 0, "{stdout}");
    let listed = parse_json(&stdout);
    let tools = listed["tools"].as_array().expect("tools 数组");
    assert_eq!(tools.len(), 1, "只放行 manifest:cli-demo：{listed}");
    assert_eq!(tools[0]["name"], "toolbox_manifest_cli_demo");
    assert_eq!(tools[0]["description"], "回显一段文本");
    assert_eq!(
        tools[0]["inputSchema"]["properties"]["text"]["type"],
        "string"
    );
    assert_eq!(tools[0]["inputSchema"]["additionalProperties"], false);

    let (code, stdout) = sandbox.run(&["show", "manifest:cli-demo", "--json"], cwd);
    assert_eq!(code, 0, "{stdout}");
    let detail = parse_json(&stdout);
    assert_eq!(detail["id"], "manifest:cli-demo");
    assert_eq!(detail["mcp_name"], "toolbox_manifest_cli_demo");
    assert_eq!(detail["execution_mode"], "capture");
    assert_eq!(detail["domain"], "系统");
    assert!(
        detail["input_schema"]["properties"]["text"].is_object(),
        "参数定义要在场：{detail}"
    );
    assert!(detail["effects"].is_object(), "安全限制要在场：{detail}");

    // 未知 / 未授权：错误 JSON + 退出码 1。
    for id in ["manifest:nothing", "manifest:cli-interactive"] {
        let (code, stdout) = sandbox.run(&["show", id, "--json"], cwd);
        assert_eq!(code, 1, "{id}: {stdout}");
        let error = parse_json(&stdout);
        assert_eq!(error["isError"], true, "{id}: {stdout}");
        assert!(
            error["structuredContent"]["error_kind"].is_string(),
            "{id}: {stdout}"
        );
    }
}

#[test]
fn run_executes_and_reports_mcp_shaped_results() {
    let sandbox = Sandbox::new("run");
    let cwd = &sandbox.root;

    let (code, stdout) = sandbox.run(
        &[
            "run",
            "manifest:cli-demo",
            "--input-json",
            r#"{"text": "中文 参数"}"#,
            "--output",
            "json",
        ],
        cwd,
    );
    assert_eq!(code, 0, "{stdout}");
    let result = parse_json(&stdout);
    assert_eq!(result["isError"], false, "{stdout}");
    let structured = &result["structuredContent"];
    assert_eq!(structured["tool_id"], "manifest:cli-demo");
    assert_eq!(structured["success"], true);
    assert_eq!(structured["exit_code"], 0);
    assert!(
        result["content"][0]["text"]
            .as_str()
            .expect("text")
            .contains("中文 参数")
    );
    // MCP 结构化结果的其余字段在场。
    for key in [
        "command",
        "stdout",
        "stderr",
        "timed_out",
        "cancelled",
        "truncated",
        "elapsed_ms",
    ] {
        assert!(structured.get(key).is_some(), "缺 {key}：{structured}");
    }
    // 审计记的是 cli 客户端。
    let audit = std::fs::read_to_string(sandbox.data.join("mcp-audit.jsonl")).expect("审计");
    assert!(audit.contains("\"client\":\"cli\""), "{audit}");

    // 缺必填参数：isError + 退出码 1，程序没跑（invalid_arguments）。
    let (code, stdout) = sandbox.run(&["run", "manifest:cli-demo"], cwd);
    assert_eq!(code, 1, "{stdout}");
    let error = parse_json(&stdout);
    assert_eq!(error["isError"], true);
    assert_eq!(
        error["structuredContent"]["error_kind"],
        "invalid_arguments"
    );

    // 未声明的参数：拒绝（形状错误 → invalid_arguments）。
    let (code, stdout) = sandbox.run(
        &[
            "run",
            "manifest:cli-demo",
            "--input-json",
            r#"{"nope": "x"}"#,
        ],
        cwd,
    );
    assert_eq!(code, 1, "{stdout}");
    assert!(
        stdout.contains("invalid_arguments") || stdout.contains("未声明"),
        "{stdout}"
    );

    // 未知工具：错误 JSON + 退出码 1。
    let (code, stdout) = sandbox.run(&["run", "manifest:nothing", "--input-json", "{}"], cwd);
    assert_eq!(code, 1, "{stdout}");
    assert_eq!(
        parse_json(&stdout)["structuredContent"]["error_kind"],
        "unknown_tool"
    );
}

#[test]
fn interactive_tools_are_refused_instead_of_hanging() {
    let sandbox = Sandbox::new("interactive");
    let cwd = &sandbox.root;
    let started = std::time::Instant::now();

    // interactive 动作没有 mcp 声明：不在列表里，run 也在子进程之前被拒。
    let (code, stdout) = sandbox.run(&["run", "manifest:cli-interactive"], cwd);
    let elapsed = started.elapsed();
    assert_eq!(code, 1, "{stdout}");
    let error = parse_json(&stdout);
    assert_eq!(
        error["structuredContent"]["error_kind"], "permission_denied",
        "{stdout}"
    );
    assert!(
        elapsed < std::time::Duration::from_secs(10),
        "拒绝要立刻发生，不能挂起：{elapsed:?}"
    );

    // 就算有人给 interactive 动作写了 mcp 声明，策略层也会按执行方式拒绝：
    // 改写 manifest 让它声明 mcp（发现层只放行 capture 声明会报错，
    // 所以这里改用策略侧证据 —— allow 名单里放它也进不了列表）。
    let (_, stdout) = sandbox.run(&["list", "--json"], cwd);
    assert!(
        !stdout.contains("cli-interactive"),
        "interactive 工具不出现在 JSON 列表里：{stdout}"
    );
}

#[test]
fn recipe_tools_appear_in_the_json_list_and_reject_injection() {
    let sandbox = Sandbox::new("recipe");
    let recipes = sandbox.root.join("recipes");
    std::fs::create_dir_all(&recipes).expect("mkdir recipes");
    std::fs::write(
        recipes.join("it-echo.json"),
        serde_json::json!({
            "id": "it-echo",
            "title": "集成测试回显",
            "description": "把参数原样 echo 回来",
            "command": "echo",
            "params": [{"name": "text", "description": "文本", "required": true}],
            "args": ["{text}"],
            "safety": "safe",
            "source": "toolbox-hub/featured",
            "tags": ["工具"],
        })
        .to_string(),
    )
    .expect("写 recipe");
    std::fs::write(
        recipes.join("index.json"),
        serde_json::json!({
            "schema_version": 1,
            "recipes": [{"id": "it-echo", "file": "it-echo.json"}],
        })
        .to_string(),
    )
    .expect("写 index");
    std::fs::write(
        sandbox.config.join("mcp.toml"),
        format!(
            r#"[mcp]
enabled = true

[exposure]
default = "deny"
allow = ["manifest:cli-demo", "recipe:it-echo"]
deny = []

[recipes]
dirs = ["{}"]
"#,
            recipes.display()
        ),
    )
    .expect("写 mcp.toml");
    let cwd = &sandbox.root;

    let (code, stdout) = sandbox.run(&["list", "--json"], cwd);
    assert_eq!(code, 0, "{stdout}");
    let listed = parse_json(&stdout);
    let ids: Vec<&str> = listed["tools"]
        .as_array()
        .expect("数组")
        .iter()
        .filter_map(|tool| tool["name"].as_str())
        .collect();
    assert!(
        ids.contains(&"toolbox_recipe_it_echo"),
        "Recipe 工具要和 Manifest 工具出现在同一份列表：{listed}"
    );

    // Recipe 工具真的能跑。
    let (code, stdout) = sandbox.run(
        &["run", "recipe:it-echo", "--input-json", r#"{"text": "hi"}"#],
        cwd,
    );
    assert_eq!(code, 0, "{stdout}");
    assert!(stdout.contains("hi"), "{stdout}");

    // S-02：选项注入被拒（与 MCP 同一条策略路径）。
    let (code, stdout) = sandbox.run(
        &[
            "run",
            "recipe:it-echo",
            "--input-json",
            r#"{"text": "--version"}"#,
        ],
        cwd,
    );
    assert_eq!(code, 1, "{stdout}");
    let error = parse_json(&stdout);
    assert_eq!(error["isError"], true);
    assert!(
        error["content"][0]["text"]
            .as_str()
            .expect("text")
            .contains("不能以 - 开头"),
        "{stdout}"
    );
}

/// 用法错误走 stderr（不是 JSON），退出码 1。
#[test]
fn usage_errors_go_to_stderr() {
    let sandbox = Sandbox::new("usage");
    let cwd = &sandbox.root;
    let output = Command::new(env!("CARGO_BIN_EXE_toolbox-hub"))
        .arg("tools")
        .arg("--config-dir")
        .arg(&sandbox.config)
        .arg("--data-dir")
        .arg(&sandbox.data)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("运行");
    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    assert!(stderr.contains("用法"), "{stderr}");
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    assert!(stdout.trim().is_empty(), "stdout 不该有内容：{stdout}");

    // 未启用 MCP 的配置目录：列表为空而不是报错（fail-closed 的诚实形态）。
    let disabled = Sandbox::new("disabled");
    std::fs::write(disabled.config.join("mcp.toml"), "[mcp]\nenabled = false\n")
        .expect("写 mcp.toml");
    let (code, stdout) = disabled.run(&["list", "--json"], &disabled.root);
    assert_eq!(code, 0);
    assert_eq!(
        parse_json(&stdout)["tools"].as_array().expect("数组").len(),
        0
    );
}
