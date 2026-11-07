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

use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};

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
        catalog::ToolCatalog,
        error::ServiceError,
        gateway::{ConcurrencyLimiter, ToolGateway},
        result::ExecutionResult,
    },
};

use super::{
    names::NameRegistry,
    protocol::{self, INVALID_PARAMS, INVALID_REQUEST, METHOD_NOT_FOUND, ProtocolVersion},
    reload::HotReload,
    schema::{input_schema, values_from_json},
    tasks::{GateRejection, RELATED_TASK, TaskManager, TaskPayload},
};

/// 退出清理（P4-01）等待任务终态的上限：执行器取消的最坏路径是
/// SIGTERM → 2s 宽限 → SIGKILL → 1s 收尾，8 秒绰绰有余。
const SHUTDOWN_WAIT: Duration = Duration::from_secs(8);

/// 一台 MCP 服务端。状态只有连接本身的那点：协商出来的能力、名称表、请求计数。
///
/// `catalog` / `auditor` / `limiter` 在 `Arc` 后面：热更新（P4-04）时整体换入
/// 新快照，已在处理的请求拿的仍是旧 `Arc`，不会引用失效对象。
pub struct McpServer {
    catalog: Arc<ToolCatalog>,
    policy: SafetyPolicy,
    auditor: Arc<Auditor>,
    /// 同步调用与后台任务共享的并发预算（P4-02）；上限可热更新。
    limiter: Arc<ConcurrencyLimiter>,
    /// MCP Tasks 任务表（P4-01）：后台执行、进度通知、取消、退出清理。
    tasks: Arc<TaskManager>,
    /// 热更新信箱（与 watcher 线程共享）。
    reload: Arc<HotReload>,
    /// 服务端要主动推给客户端的通知行（list_changed、进度等），主循环在
    /// 写完当前请求的响应后统一排空。
    outgoing: Arc<Mutex<Vec<String>>>,
    names: NameRegistry,
    initialized: bool,
    /// 本次连接协商出的协议版本（ADR-001：双版本，Tasks 只在 2025-11-25 生效）。
    negotiated: ProtocolVersion,
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
        let limiter = Arc::new(ConcurrencyLimiter::new(policy.config.max_concurrent()));
        let outgoing = Arc::new(Mutex::new(Vec::new()));
        let tasks = Arc::new(TaskManager::new(
            Arc::clone(&outgoing),
            Arc::clone(&limiter),
        ));
        Self {
            catalog: Arc::new(catalog),
            policy: policy.clone(),
            auditor: Arc::new(auditor),
            limiter,
            tasks,
            reload: Arc::new(HotReload::new()),
            outgoing,
            names: NameRegistry::default(),
            initialized: false,
            negotiated: ProtocolVersion::V2025_06_18,
            client_name: String::from("unknown"),
            client_elicitation: false,
            next_request_id: 1_000_000,
        }
    }

    /// 热更新信箱的把手（交给 watcher 线程）。
    pub fn reload_handle(&self) -> Arc<HotReload> {
        Arc::clone(&self.reload)
    }

    /// 待推送通知队列的把手（主循环排空后写 stdout）。
    pub fn outgoing_handle(&self) -> Arc<Mutex<Vec<String>>> {
        Arc::clone(&self.outgoing)
    }

    /// 任务表把手（测试用；运行期退出清理走 [`McpServer::shutdown_tasks`]）。
    #[allow(dead_code)]
    pub fn tasks_handle(&self) -> Arc<TaskManager> {
        Arc::clone(&self.tasks)
    }

    /// 退出清理：stdio 断开后把全部运行中任务置取消旗标（进程组 SIGTERM →
    /// 2s 宽限 → SIGKILL），等它们到终态，上限 8 秒。
    pub fn shutdown_tasks(&self) {
        self.tasks.shutdown(SHUTDOWN_WAIT);
    }

    /// 换入 watcher 产出的新快照（P4-04）。快照是整体构建好的，换入即生效；
    /// 授权工具清单的内容真的变了才发 `notifications/tools/list_changed`。
    pub fn apply_reload(&mut self) {
        let Some(snapshot) = self.reload.take() else {
            return;
        };
        let before = self.authorized_signature();
        self.catalog = snapshot.catalog;
        self.policy = snapshot.policy;
        if let Some(problem) = snapshot.problem {
            eprintln!("toolbox-hub mcp: {problem}");
        }
        // 审计位置与并发上限跟着新策略走；限制器原地改上限，预算不清零。
        self.auditor = Arc::new(Auditor::new(
            self.policy.config.audit_path(),
            self.policy.config.audit.enabled,
        ));
        self.limiter.set_max(self.policy.config.max_concurrent());
        self.rebuild_names();
        if before != self.authorized_signature() {
            self.push_outgoing(protocol::notification(protocol::LIST_CHANGED));
        }
    }

    /// 授权工具清单的内容签名（id + 摘要 + Schema），用于判断「列表真的变了吗」。
    fn authorized_signature(&self) -> Vec<String> {
        let caps = ClientCapabilities {
            confirmation: self.client_elicitation,
        };
        let gateway = ToolGateway::new(
            &self.catalog,
            &self.policy,
            &self.auditor,
            caps,
            self.client_name.clone(),
            Arc::clone(&self.limiter),
        );
        gateway
            .list()
            .iter()
            .map(|tool| {
                format!(
                    "{}\u{1}{}\u{1}{}",
                    tool.id,
                    tool.summary,
                    input_schema(tool)
                )
            })
            .collect()
    }

    fn push_outgoing(&self, line: String) {
        self.outgoing
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(line);
    }

    /// 名称表按「这次连接实际可授权的工具」重建（initialize 与热更新共用）。
    fn rebuild_names(&mut self) {
        let caps = ClientCapabilities {
            confirmation: self.client_elicitation,
        };
        let gateway = ToolGateway::new(
            &self.catalog,
            &self.policy,
            &self.auditor,
            caps,
            self.client_name.clone(),
            Arc::clone(&self.limiter),
        );
        self.names = NameRegistry::build(gateway.list());
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
            // P5-02：只读 Resources（2025-06-18 语义）。只有白名单里的三类
            // URI，不提供任意本地文件读取。
            "resources/list" if self.initialized => self.resources_list(),
            "resources/read" if self.initialized => self.resources_read(&incoming.params),
            // Tasks 面只在 2025-11-25 协商生效；旧版本客户端按未知方法拒绝
            //（规范：能力未声明时请求方不得使用，接收端回 -32601）。
            "tasks/get" if self.initialized && self.negotiated == ProtocolVersion::V2025_11_25 => {
                self.tasks_get(&incoming.params)
            }
            "tasks/result"
                if self.initialized && self.negotiated == ProtocolVersion::V2025_11_25 =>
            {
                self.tasks_result(&incoming.params)
            }
            "tasks/cancel"
                if self.initialized && self.negotiated == ProtocolVersion::V2025_11_25 =>
            {
                self.tasks_cancel(&incoming.params)
            }
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
        self.rebuild_names();
        self.initialized = true;

        // 协议版本协商（ADR-001）：请求 2025-11-25 / 2026-07-28 → 回
        // 2025-11-25 并开放 Tasks；请求 2025-06-18（或更老/未知版本）→ 维持
        // 2025-06-18。规范要求不支持客户端请求的版本时必须回自己支持的版本，
        // 由客户端决定是否继续；盲目回显等于宣称实现了未实现的语义。
        self.negotiated =
            ProtocolVersion::negotiate(params.get("protocolVersion").and_then(Value::as_str));
        // P4-04：热更新已实现，声明 listChanged 并在清单变化时发通知。
        // P4-01：Tasks 能力只在 2025-11-25 协商下声明；tasks.list 按规范的
        // 安全建议不声明（stdio 无法识别请求方，列表会把任务暴露给任何人）。
        // P5-02：Resources 面只在白名单 URI 上实现，不声明 subscribe/listChanged。
        let capabilities = match self.negotiated {
            ProtocolVersion::V2025_11_25 => json!({
                "tools": {"listChanged": true},
                "resources": {},
                "tasks": {
                    "cancel": {},
                    "requests": {"tools": {"call": {}}},
                },
            }),
            ProtocolVersion::V2025_06_18 => json!({
                "tools": {"listChanged": true},
                "resources": {},
            }),
        };
        Ok(json!({
            "protocolVersion": self.negotiated.as_str(),
            "capabilities": capabilities,
            "serverInfo": {
                "name": "toolbox-hub",
                "version": env!("CARGO_PKG_VERSION"),
            },
        }))
    }

    fn tools_list(&mut self) -> Result<Value, (i64, String)> {
        let tools: Vec<Value> = self
            .authorized_tools()
            .iter()
            .map(|tool| {
                let mut entry = tool_entry(&self.names, tool);
                // P4-01 工具级协商：2025-11-25 下声明每件工具可按任务执行。
                if self.negotiated == ProtocolVersion::V2025_11_25 {
                    entry["execution"] = json!({"taskSupport": "optional"});
                }
                entry
            })
            .collect();
        Ok(json!({"tools": tools}))
    }

    /// 本次连接可调用的工具（策略判定 + 当前能力），tools/list 与 resources 共用。
    fn authorized_tools(&self) -> Vec<&ToolDefinition> {
        let caps = ClientCapabilities {
            confirmation: self.client_elicitation,
        };
        let gateway = ToolGateway::new(
            &self.catalog,
            &self.policy,
            &self.auditor,
            caps,
            self.client_name.clone(),
            Arc::clone(&self.limiter),
        );
        gateway.list()
    }

    /// P5-02 `resources/list`：固定的两个信息资源 + 每件授权工具一个资源。
    fn resources_list(&self) -> Result<Value, (i64, String)> {
        let mut resources = vec![
            json!({
                "uri": CATALOG_URI,
                "name": "工具目录",
                "description": "全部可调用工具的清单：用途、参数定义、依赖要求、安全限制与执行方式",
                "mimeType": "application/json",
            }),
            json!({
                "uri": DOMAINS_URI,
                "name": "域列表",
                "description": "工具箱的域与每个域里的可调用工具数",
                "mimeType": "application/json",
            }),
        ];
        for tool in self.authorized_tools() {
            resources.push(json!({
                "uri": format!("{TOOLS_URI_PREFIX}{}", tool.id),
                "name": tool.name,
                "description": tool_description(tool),
                "mimeType": "application/json",
            }));
        }
        Ok(json!({ "resources": resources }))
    }

    /// P5-02 `resources/read`：只认三类白名单 URI；别的一律 -32602。
    fn resources_read(&self, params: &Value) -> Result<Value, (i64, String)> {
        let uri = params
            .get("uri")
            .and_then(Value::as_str)
            .ok_or((INVALID_PARAMS, String::from("缺少 uri")))?;
        let body = if uri == CATALOG_URI {
            json!({
                "tools": self
                    .authorized_tools()
                    .iter()
                    .map(|tool| tool_resource_detail(
                        tool,
                        self.names.name_of(&tool.id).unwrap_or(&tool.id),
                    ))
                    .collect::<Vec<_>>(),
            })
        } else if uri == DOMAINS_URI {
            json!({ "domains": domain_summaries(&self.authorized_tools()) })
        } else if let Some(id) = uri.strip_prefix(TOOLS_URI_PREFIX) {
            match self
                .authorized_tools()
                .into_iter()
                .find(|tool| tool.id == id)
            {
                Some(tool) => tool_resource_detail(tool, self.names.name_of(id).unwrap_or(id)),
                None => {
                    return Err((INVALID_PARAMS, format!("未知资源：{uri}")));
                }
            }
        } else {
            return Err((INVALID_PARAMS, format!("未知资源：{uri}")));
        };
        let text = serde_json::to_string_pretty(&body)
            .map_err(|error| (INVALID_PARAMS, format!("资源内容序列化失败：{error}")))?;
        Ok(json!({
            "contents": [{
                "uri": uri,
                "mimeType": "application/json",
                "text": text,
            }],
        }))
    }

    /// tools/call：名称 → 取值 →（task 字段 → 任务路径）→ 确认 → 网关。
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

        // P4-01：2025-11-25 协商下 params 带 task 字段 → 按任务执行。
        // 2025-06-18 客户端的 task 字段按规范忽略（未声明能力时按普通请求处理）。
        if self.negotiated == ProtocolVersion::V2025_11_25 && params.get("task").is_some() {
            return self.call_tool_as_task(params, &tool, values, elicit);
        }

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
            Arc::clone(&self.limiter),
        );
        // 工作目录 = 服务端进程的启动目录（MCP 客户端没有「当前目录」概念）。
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        match gateway.call(&id, &values, &cwd) {
            Ok(result) => Ok(execution_result(&result)),
            Err(ServiceError::UnknownTool(_)) => Err((INVALID_PARAMS, format!("未知工具：{name}"))),
            Err(error) => Ok(tool_error(error.to_string(), error.kind())),
        }
    }

    /// 任务路径（P4-01）：建任务 → 确认门槛（同步，拒绝即 failed）→ 后台
    /// 执行 → 立即回 CreateTaskResult。协议级错误（未知工具、参数形状）在
    /// 任务创建之前就已被拒绝，不会产生任务。
    fn call_tool_as_task(
        &mut self,
        params: &Value,
        tool: &ToolDefinition,
        values: TypedValues,
        elicit: Elicit<'_>,
    ) -> Result<Value, (i64, String)> {
        let id = tool.id.clone();
        let task_params = params.get("task").cloned().unwrap_or(Value::Null);
        let ttl = task_params.get("ttl").and_then(Value::as_u64);
        // progressToken：客户端在 _meta 里索要了才发 notifications/progress
        //（字符串或数字，规范允许两种）。
        let progress_token = params
            .get("_meta")
            .and_then(|meta| meta.get("progressToken"))
            .filter(|token| token.is_string() || token.is_number())
            .cloned();

        let tasks = Arc::clone(&self.tasks);
        let audit_arguments = redacted_arguments(tool.action.as_ref(), &values);
        let payload = TaskPayload {
            tool: tool.clone(),
            values,
            cwd: std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
            client_name: self.client_name.clone(),
            policy: self.policy.clone(),
            auditor: (*self.auditor).clone(),
            audit_arguments: audit_arguments.clone(),
        };

        // 确认门槛与同步路径同一套判定，差别只有两处：审计带 task_id 标记、
        // elicitation 请求带 related-task 元数据（规范要求任务相关消息必须可关联）。
        let mut gate = |task_id: &str| -> Result<(), GateRejection> {
            let effect = effects::infer(tool);
            if approval::requirement(tool, effect) != approval::Requirement::Confirmation {
                return Ok(());
            }
            if !self.client_elicitation {
                (*self.auditor).with_task_tag(task_id).denied(
                    &id,
                    &self.client_name,
                    "confirmation_unsupported",
                    audit_arguments.clone(),
                );
                return Err(GateRejection {
                    kind: "confirmation_unsupported",
                    message: format!("{id} 需要人工确认，但客户端不支持确认能力（elicitation）"),
                });
            }
            self.next_request_id += 1;
            let request_id = self.next_request_id;
            let request = json!({
                "mode": "form",
                "message": confirmation_message(tool, &effect),
                "requestedSchema": {"type": "object", "properties": {}, "required": []},
                "_meta": { RELATED_TASK: {"taskId": task_id} },
            });
            match elicit(request_id, "elicitation/create", request) {
                Some(result) if result.get("action").and_then(Value::as_str) == Some("accept") => {
                    Ok(())
                }
                _ => {
                    (*self.auditor).with_task_tag(task_id).denied(
                        &id,
                        &self.client_name,
                        "declined_by_user",
                        audit_arguments.clone(),
                    );
                    Err(GateRejection {
                        kind: "declined_by_user",
                        message: format!("{id} 未获用户确认，没有执行"),
                    })
                }
            }
        };

        Ok(tasks.begin(ttl, progress_token, payload, &mut gate))
    }

    /// `tasks/get`：当前任务状态（轮询模型，不阻塞）；未知/过期 → -32602。
    fn tasks_get(&self, params: &Value) -> Result<Value, (i64, String)> {
        let task_id = params
            .get("taskId")
            .and_then(Value::as_str)
            .ok_or((INVALID_PARAMS, String::from("缺少 taskId")))?;
        self.tasks.get(task_id)
    }

    /// `tasks/result`：阻塞到终态，回底层请求本该返回的结果 + related-task。
    fn tasks_result(&self, params: &Value) -> Result<Value, (i64, String)> {
        let task_id = params
            .get("taskId")
            .and_then(Value::as_str)
            .ok_or((INVALID_PARAMS, String::from("缺少 taskId")))?;
        // 等待上限 = 策略最大超时 + 收尾宽限：任务线程最坏情况是执行超时
        // 再走两段终止宽限，不会超过它。
        let cap = Duration::from_secs(self.policy.config.limits.max_timeout_secs.max(1) + 15);
        self.tasks.wait_result(task_id, cap)
    }

    /// `tasks/cancel`：响应前转 cancelled 终态并置取消旗标；终态再取消 → -32602。
    fn tasks_cancel(&self, params: &Value) -> Result<Value, (i64, String)> {
        let task_id = params
            .get("taskId")
            .and_then(Value::as_str)
            .ok_or((INVALID_PARAMS, String::from("缺少 taskId")))?;
        self.tasks.cancel(task_id)
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
/// `pub(crate)`：任务线程（mcp::tasks）落终态结果用同一形状。
pub(crate) fn execution_result(result: &ExecutionResult) -> Value {
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
/// `pub(crate)`：任务路径的门槛拒绝与执行失败用同一形状落终态；
/// CLI JSON（P5-04）也用它保持形状一致。
pub(crate) fn tool_error(message: String, kind: &str) -> Value {
    json!({
        "content": [{"type": "text", "text": message}],
        "structuredContent": {"error_kind": kind, "error": message},
        "isError": true,
    })
}

/// P5-02 资源 URI（白名单三类，别的一律不读）。
pub(crate) const CATALOG_URI: &str = "toolbox://catalog";
pub(crate) const DOMAINS_URI: &str = "toolbox://domains";
pub(crate) const TOOLS_URI_PREFIX: &str = "toolbox://tools/";

/// 一件授权工具的 MCP 描述（`tools/list` 条目）。
///
/// `pub(crate)`：CLI JSON（P5-04）复用同一形状 —— 两个入口的 JSON 不会各改各的。
pub(crate) fn tool_entry(names: &NameRegistry, tool: &ToolDefinition) -> Value {
    let effect = effects::infer(tool);
    json!({
        "name": names.name_of(&tool.id).unwrap_or(&tool.id),
        "description": tool_description(tool),
        "inputSchema": input_schema(tool),
        "annotations": {
            // readOnlyHint 只是**提示**（计划书 §5.1）：真正的授权在 mcp.toml。
            "readOnlyHint": effect.read_only() && !tool.danger.needs_confirm(),
        },
    })
}

/// 给 AI 看的一句话说明：作者声明优先，退化为 summary / 名称。
fn tool_description(tool: &ToolDefinition) -> String {
    tool.mcp
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
        })
}

/// 一件工具的完整详情（`toolbox://tools/{id}` 资源与 CLI `tools show` 共用）。
///
/// 只含定义层元数据：用途、参数 Schema、依赖、执行方式、安全限制。
/// 不含执行历史，也不含任何敏感值 —— Resources 不做个人执行历史的公开面。
pub(crate) fn tool_resource_detail(tool: &ToolDefinition, mcp_name: &str) -> Value {
    let effect = effects::infer(tool);
    json!({
        "id": tool.id,
        "mcp_name": mcp_name,
        "name": tool.name,
        "provider": tool.provider,
        "domain": tool.domain.label(),
        "tags": tool.tags,
        "description": tool_description(tool),
        "input": tool.input,
        "output": tool.output,
        // 依赖要求：全量声明 + 当前缺失 + 安装提示。
        "requires": tool.requires,
        "missing_dependencies": tool.missing_deps,
        "install_hint": tool.install_label(),
        // 执行方式：capture 才会出现在可调用列表里（与策略一致）。
        "execution_mode": crate::service::error::mode_name(tool.mode),
        "danger": tool.danger.label(),
        // 安全限制：独立推断的副作用 + 只读提示（真正的授权在 mcp.toml）。
        "effects": {
            "filesystem_read": effect.filesystem_read,
            "filesystem_write": effect.filesystem_write,
            "network_access": effect.network_access,
            "system_mutation": effect.system_mutation,
            "requires_privilege": effect.requires_privilege,
            "interactive": effect.interactive,
            "destructive": effect.destructive,
            "arbitrary_command": effect.arbitrary_command,
        },
        "annotations": {
            "readOnlyHint": effect.read_only() && !tool.danger.needs_confirm(),
        },
        "input_schema": input_schema(tool),
    })
}

/// 域汇总：全部域 + 每个域里的可调用工具数（`toolbox://domains`）。
fn domain_summaries(tools: &[&ToolDefinition]) -> Vec<Value> {
    crate::model::Domain::ALL
        .iter()
        .map(|domain| {
            json!({
                "id": domain.id(),
                "label": domain.label(),
                "description": domain.description(),
                "tool_count": tools.iter().filter(|tool| tool.domain == *domain).count(),
            })
        })
        .collect()
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
            response["result"]["capabilities"]["tools"]["listChanged"], true,
            "P4-04 热更新已实现，要声明 listChanged"
        );
        assert_eq!(
            response["result"]["capabilities"]["resources"],
            json!({}),
            "P5-02：声明 resources（subscribe/listChanged 未实现，不声明）"
        );
        assert!(
            response["result"]["capabilities"].get("tasks").is_none(),
            "2025-06-18 协商不声明 Tasks：{response}"
        );
        cleanup(&fixture);
    }

    /// P4-01：请求 2025-11-25（或 2026-07-28）→ 服务端回 2025-11-25 并开放
    /// Tasks 能力（cancel + requests.tools.call）；tasks.list 按规范安全建议
    /// 不声明；tools/list 每件工具带 execution.taskSupport = "optional"。
    #[test]
    fn a_2025_11_25_client_gets_the_tasks_capability() {
        let mut fixture = Fixture::new(vec![tool("test:echo", false)], config(""));
        let line = json!({
            "jsonrpc": "2.0", "id": 1, "method": "initialize",
            "params": {
                "protocolVersion": "2025-11-25",
                "clientInfo": {"name": "tester", "version": "0"},
                "capabilities": {},
            }
        });
        let responses = fixture.server.handle(&line.to_string(), &mut no_elicit());
        let response: Value = serde_json::from_str(&responses[0]).expect("合法 JSON");
        assert_eq!(response["result"]["protocolVersion"], "2025-11-25");
        let capabilities = &response["result"]["capabilities"];
        assert_eq!(capabilities["tools"]["listChanged"], true);
        assert_eq!(capabilities["tasks"]["cancel"], json!({}));
        assert_eq!(
            capabilities["tasks"]["requests"]["tools"]["call"],
            json!({})
        );
        assert!(
            capabilities["tasks"].get("list").is_none(),
            "stdio 无法识别请求方，不声明 tasks.list：{capabilities}"
        );

        let responses = fixture.server.handle(
            r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#,
            &mut no_elicit(),
        );
        let listed: Value = serde_json::from_str(&responses[0]).expect("合法 JSON");
        assert_eq!(
            listed["result"]["tools"][0]["execution"]["taskSupport"], "optional",
            "{listed}"
        );
        cleanup(&fixture);
    }

    /// P4-01 全链路（单元级）：task 字段 → CreateTaskResult(working) →
    /// tasks/get 到 completed → tasks/result 带 related-task 且 isError=false；
    /// 审计的 created/completed 都带 task_id。
    #[test]
    fn a_task_augmented_call_round_trips_through_begin_get_and_result() {
        let mut fixture = Fixture::new(vec![tool("test:echo", false)], config(""));
        // Tasks 只在 2025-11-25 协商下生效：先以该版本 initialize。
        let line = json!({
            "jsonrpc": "2.0", "id": 1, "method": "initialize",
            "params": {
                "protocolVersion": "2025-11-25",
                "clientInfo": {"name": "tester", "version": "0"},
                "capabilities": {},
            }
        });
        fixture.server.handle(&line.to_string(), &mut no_elicit());
        let line = json!({
            "jsonrpc": "2.0", "id": 3, "method": "tools/call",
            "params": {
                "name": "toolbox_test_echo",
                "arguments": {"text": "任务回显"},
                "task": {},
            }
        });
        let responses = fixture.server.handle(&line.to_string(), &mut no_elicit());
        assert_eq!(responses.len(), 1);
        let created: Value = serde_json::from_str(&responses[0]).expect("合法 JSON");
        let task = &created["result"]["task"];
        assert_eq!(task["status"], "working", "{created}");
        let task_id = task["taskId"].as_str().expect("taskId").to_string();
        assert_eq!(task["createdAt"].as_str().map(str::len), Some(20));
        assert_eq!(task["pollInterval"], 1000);

        // tasks/get：轮询到 completed（echo 任务很快，个别慢机器上多试几次）。
        let mut status = String::new();
        for _ in 0..50 {
            let responses = fixture.server.handle(
                &json!({"jsonrpc": "2.0", "id": 4, "method": "tasks/get",
                        "params": {"taskId": task_id}})
                .to_string(),
                &mut no_elicit(),
            );
            let got: Value = serde_json::from_str(&responses[0]).expect("合法 JSON");
            status = got["result"]["status"]
                .as_str()
                .expect("status")
                .to_string();
            if status == "completed" {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert_eq!(status, "completed");

        // tasks/result：底层请求的结果 + related-task 元数据。
        let responses = fixture.server.handle(
            &json!({"jsonrpc": "2.0", "id": 5, "method": "tasks/result",
                    "params": {"taskId": task_id}})
            .to_string(),
            &mut no_elicit(),
        );
        let result: Value = serde_json::from_str(&responses[0]).expect("合法 JSON");
        assert_eq!(result["result"]["isError"], false, "{result}");
        assert_eq!(
            result["result"]["structuredContent"]["success"], true,
            "{result}"
        );
        assert_eq!(
            result["result"]["_meta"]["io.modelcontextprotocol/related-task"]["taskId"],
            task_id.as_str(),
            "{result}"
        );
        let audit = fixture.audit_text();
        assert!(audit.contains("\"decision\":\"created\""));
        assert!(audit.contains("\"decision\":\"completed\""));
        assert!(audit.contains(&format!("\"task_id\":\"{task_id}\"")));
        cleanup(&fixture);
    }

    /// 2025-06-18 客户端：task 字段按普通请求处理（规范：未声明能力时忽略
    /// 任务增强元数据），响应是普通 CallToolResult；tasks/get 是未知方法。
    #[test]
    fn a_2025_06_18_client_gets_task_fields_ignored() {
        let mut fixture = Fixture::new(vec![tool("test:echo", false)], config(""));
        initialize(&mut fixture.server, false);
        let line = json!({
            "jsonrpc": "2.0", "id": 6, "method": "tools/call",
            "params": {
                "name": "toolbox_test_echo",
                "arguments": {"text": "同步"},
                "task": {"ttl": 60000},
            }
        });
        let responses = fixture.server.handle(&line.to_string(), &mut no_elicit());
        let called: Value = serde_json::from_str(&responses[0]).expect("合法 JSON");
        assert_eq!(called["result"]["isError"], false, "{called}");
        assert!(
            called["result"].get("task").is_none(),
            "同步路径不该有 task 字段：{called}"
        );
        assert_eq!(called["result"]["structuredContent"]["success"], true);

        let responses = fixture.server.handle(
            r#"{"jsonrpc":"2.0","id":7,"method":"tasks/get","params":{"taskId":"x"}}"#,
            &mut no_elicit(),
        );
        let unknown: Value = serde_json::from_str(&responses[0]).expect("合法 JSON");
        assert_eq!(unknown["error"]["code"], -32601);
        cleanup(&fixture);
    }

    /// P4-04：换入热更新快照后，目录、名称表整体更新；授权清单内容真的变了
    /// 才发 `notifications/tools/list_changed`；内容没变就不发。
    #[test]
    fn a_reload_snapshot_swaps_atomically_and_notifies_only_on_real_changes() {
        use crate::security::policy::SafetyPolicy;

        // 快照里的策略把审计指回测试目录，避免热更新把 Auditor 换到真实数据目录。
        let policy_for = |allow: &str, dir: &PathBuf| -> SafetyPolicy {
            let text = format!(
                "[mcp]\nenabled = true\n[exposure]\ndefault = \"deny\"\nallow = [{allow}]\ndeny = []\n[audit]\npath = \"{}\"\n",
                dir.join("audit.jsonl").display()
            );
            SafetyPolicy::new(toml::from_str(&text).expect("配置"))
        };

        let mut fixture = Fixture::new(vec![tool("test:echo", false)], config(""));
        initialize(&mut fixture.server, false);

        // 相同内容（同一件工具）的新快照：换入但不发通知。
        let same = crate::mcp::reload::ReloadSnapshot {
            catalog: std::sync::Arc::new(ToolCatalog::new(vec![tool("test:echo", false)])),
            policy: policy_for("\"test:echo\"", &fixture.dir),
            problem: None,
        };
        fixture.server.reload_handle().store(same);
        fixture.server.apply_reload();
        assert!(
            fixture
                .server
                .outgoing_handle()
                .lock()
                .expect("锁")
                .is_empty()
        );

        // 新增一件授权工具：换入并发通知；tools/list 立即看到新工具。
        let mut extra = tool("test:extra", false);
        extra.summary = String::from("新来的");
        let grown = crate::mcp::reload::ReloadSnapshot {
            catalog: std::sync::Arc::new(ToolCatalog::new(vec![tool("test:echo", false), extra])),
            policy: policy_for("\"test:echo\", \"test:extra\"", &fixture.dir),
            problem: None,
        };
        fixture.server.reload_handle().store(grown);
        fixture.server.apply_reload();
        let outgoing = fixture
            .server
            .outgoing_handle()
            .lock()
            .expect("锁")
            .drain(..)
            .collect::<Vec<_>>();
        assert_eq!(outgoing.len(), 1, "清单变了才发一次通知：{outgoing:?}");
        assert!(
            outgoing[0].contains("notifications/tools/list_changed"),
            "{outgoing:?}"
        );

        let responses = fixture.server.handle(
            r#"{"jsonrpc":"2.0","id":20,"method":"tools/list"}"#,
            &mut no_elicit(),
        );
        let parsed: Value = serde_json::from_str(&responses[0]).expect("合法");
        assert_eq!(
            parsed["result"]["tools"].as_array().expect("数组").len(),
            2,
            "新快照立即生效"
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
            r#"{"jsonrpc":"2.0","id":11,"method":"prompts/list"}"#,
            &mut no_elicit(),
        );
        let parsed: Value = serde_json::from_str(&responses[0]).expect("合法");
        assert_eq!(parsed["error"]["code"], -32601);
        cleanup(&fixture);
    }

    /// P5-02：resources/list 列出两个固定资源 + 每件授权工具一个；
    /// resources/read 三类白名单 URI 各自形状正确，未知 URI -32602。
    #[test]
    fn resources_cover_catalog_domains_and_tool_details() {
        let mut fixture = Fixture::new(vec![tool("test:echo", false)], config(""));
        initialize(&mut fixture.server, false);

        let responses = fixture.server.handle(
            r#"{"jsonrpc":"2.0","id":2,"method":"resources/list"}"#,
            &mut no_elicit(),
        );
        let listed: Value = serde_json::from_str(&responses[0]).expect("合法 JSON");
        let resources = listed["result"]["resources"].as_array().expect("数组");
        assert_eq!(resources.len(), 3, "catalog + domains + 1 件工具：{listed}");
        assert_eq!(resources[0]["uri"], "toolbox://catalog");
        assert_eq!(resources[1]["uri"], "toolbox://domains");
        assert_eq!(resources[2]["uri"], "toolbox://tools/test:echo");
        assert_eq!(resources[2]["mimeType"], "application/json");

        // read catalog：内容是 pretty JSON，含那件工具的完整详情。
        let responses = fixture.server.handle(
            r#"{"jsonrpc":"2.0","id":3,"method":"resources/read","params":{"uri":"toolbox://catalog"}}"#,
            &mut no_elicit(),
        );
        let read: Value = serde_json::from_str(&responses[0]).expect("合法 JSON");
        let contents = &read["result"]["contents"];
        assert_eq!(contents[0]["uri"], "toolbox://catalog");
        assert_eq!(contents[0]["mimeType"], "application/json");
        let body: Value = serde_json::from_str(contents[0]["text"].as_str().expect("text"))
            .expect("catalog 内容是 JSON 文本");
        assert_eq!(body["tools"][0]["id"], "test:echo");
        assert_eq!(body["tools"][0]["execution_mode"], "capture");
        assert_eq!(
            body["tools"][0]["input_schema"]["properties"]["text"]["type"], "string",
            "参数定义在场"
        );
        assert!(
            body["tools"][0]["requires"].is_array(),
            "依赖要求在场：{body}"
        );
        assert!(
            body["tools"][0]["effects"]["filesystem_read"].is_boolean(),
            "安全限制（副作用）在场"
        );

        // read 单件工具 + domains + 未知 URI。
        let read_tool = fixture.server.handle(
            r#"{"jsonrpc":"2.0","id":4,"method":"resources/read","params":{"uri":"toolbox://tools/test:echo"}}"#,
            &mut no_elicit(),
        );
        let parsed: Value = serde_json::from_str(&read_tool[0]).expect("合法 JSON");
        let detail: Value = serde_json::from_str(
            parsed["result"]["contents"][0]["text"]
                .as_str()
                .expect("text"),
        )
        .expect("详情是 JSON");
        assert_eq!(detail["id"], "test:echo");
        assert_eq!(detail["mcp_name"], "toolbox_test_echo");

        let read_domains = fixture.server.handle(
            r#"{"jsonrpc":"2.0","id":5,"method":"resources/read","params":{"uri":"toolbox://domains"}}"#,
            &mut no_elicit(),
        );
        let parsed: Value = serde_json::from_str(&read_domains[0]).expect("合法 JSON");
        let body: Value = serde_json::from_str(
            parsed["result"]["contents"][0]["text"]
                .as_str()
                .expect("text"),
        )
        .expect("domains 是 JSON");
        let domains = body["domains"].as_array().expect("数组");
        assert_eq!(domains.len(), crate::model::Domain::ALL.len());
        assert!(
            domains
                .iter()
                .any(|domain| domain["id"] == "system" && domain["tool_count"] == 1),
            "{body}"
        );

        let unknown = fixture.server.handle(
            r#"{"jsonrpc":"2.0","id":6,"method":"resources/read","params":{"uri":"file:///etc/passwd"}}"#,
            &mut no_elicit(),
        );
        let parsed: Value = serde_json::from_str(&unknown[0]).expect("合法 JSON");
        assert_eq!(parsed["error"]["code"], -32602, "任意文件读取不在白名单里");

        let unknown_tool = fixture.server.handle(
            r#"{"jsonrpc":"2.0","id":7,"method":"resources/read","params":{"uri":"toolbox://tools/test:nope"}}"#,
            &mut no_elicit(),
        );
        let parsed: Value = serde_json::from_str(&unknown_tool[0]).expect("合法 JSON");
        assert_eq!(parsed["error"]["code"], -32602);

        cleanup(&fixture);
    }

    /// P5-02：未授权 / 未声明的工具不产生资源；resources 只反映授权可见面。
    #[test]
    fn resources_only_expose_authorized_tools() {
        let mut fixture = Fixture::new(
            vec![tool("test:echo", false), tool("test:hidden", false)],
            config(""),
        );
        initialize(&mut fixture.server, false);
        let responses = fixture.server.handle(
            r#"{"jsonrpc":"2.0","id":2,"method":"resources/list"}"#,
            &mut no_elicit(),
        );
        let listed: Value = serde_json::from_str(&responses[0]).expect("合法 JSON");
        let text = listed["result"]["resources"].to_string();
        assert!(text.contains("test:echo"));
        assert!(
            !text.contains("test:hidden"),
            "未授权工具不该出现在资源里：{text}"
        );
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
