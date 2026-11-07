//! MCP Tasks（2025-11-25，实验性；计划书 P4-01）。
//!
//! 客户端给 `tools/call` 带 `task` 字段时，服务端不再同步等待执行结束，而是：
//!
//! ```text
//! tools/call {task} → begin()：建 working 记录 → 确认门槛（elicitation，请求带
//!   related-task 元数据）→ 拒绝即转 failed（子进程从未启动）→ 通过则起后台线程
//!   走完整网关流水线 → 立即回 CreateTaskResult
//! tasks/get    → 当前任务状态（非阻塞，规范是轮询模型）
//! tasks/result → 阻塞到终态，回底层请求本该返回的结果（必带 related-task 元数据）
//! tasks/cancel → 置共享取消旗标 → 执行器 pump 见旗标 → terminate()（SIGTERM →
//!   2s 宽限 → SIGKILL）；终态在响应前落定，之后不可再变；终态再取消回 -32602
//! ```
//!
//! 纪律（来源：modelcontextprotocol.io/specification/2025-11-25/basic/utilities/tasks）：
//!
//! * 任务 id 必须不可猜测：无授权上下文的 stdio 服务端用 /dev/urandom 生成
//!   128 位熵的 id，不引入新依赖；
//! * `tasks.list` 能力**不声明**（规范安全建议：无法识别请求方的接收端不应声明），
//!   所以任务表上限（256）只在这里防资源耗尽，不提供列表面；
//! * `statusMessage` / `notifications/progress` 只承载真实耗时与阶段信息，
//!   没有总量依据就不给 total、不编百分比；
//! * 拒绝（策略、确认、并发）全部发生在启动子进程之前，与同步路径同一套网关。

use std::{
    collections::BTreeMap,
    fs,
    path::PathBuf,
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use serde::Serialize;
use serde_json::{Value, json};

use crate::{
    model::{ToolDefinition, TypedValues},
    security::{approval::ClientCapabilities, audit::Auditor, policy::SafetyPolicy},
    service::{
        catalog::ToolCatalog,
        execution::ExecutionProgress,
        gateway::{ConcurrencyLimiter, ToolGateway},
    },
};

use super::{
    protocol,
    server::{execution_result, tool_error},
};

/// related-task 元数据键：所有与任务相关的请求/响应必须带它。
pub const RELATED_TASK: &str = "io.modelcontextprotocol/related-task";

/// 任务表上限：防客户端无限创建任务把内存耗尽。超限时先清过期终态任务，
/// 再逐出最老的终态任务。
const MAX_TABLE: usize = 256;

/// 接受的 ttl 上限（毫秒）：规范要求接收端限制 ttl 时长，防止无限期保留。
const MAX_TTL_MS: u64 = 3_600_000;

/// 建议给请求方的轮询间隔（毫秒）；与 P4-03 的 1 秒进度节流对齐。
const POLL_INTERVAL_MS: u64 = 1000;

/// 任务终态后，等后台线程把结果写进记录的上限（取消后线程还在收尾）。
const RESULT_SETTLE_WAIT: Duration = Duration::from_secs(10);

/// 任务状态。只实现 working 与三个终态：确认门槛在创建时同步完成，
/// 不存在中途要输入的 `input_required`。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TaskStatus {
    Working,
    Completed,
    Failed,
    Cancelled,
}

impl TaskStatus {
    fn as_str(&self) -> &'static str {
        match self {
            Self::Working => "working",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }

    /// completed / failed / cancelled 是终态，永不再变（规范的状态机要求）。
    fn is_terminal(&self) -> bool {
        !matches!(self, Self::Working)
    }
}

/// 一条任务记录。除 result 外的字段都是创建/更新时落定的元数据；
/// `auditor` 是带 task_id 标记的副本，任务全生命周期（created / progress /
/// cancelled 与网关的 completed / denied）写出的审计都自动串在同一条链上。
struct TaskRecord {
    /// 插入序号：表满时逐出「最老的终态任务」用它比较。
    seq: u64,
    task_id: String,
    tool_id: String,
    client: String,
    /// Unix 毫秒（响应里转 ISO 8601）。
    created_ms: u64,
    updated_ms: u64,
    /// 保留时长（毫秒，自创建起算）；`None` = 未限时。
    ttl: Option<u64>,
    status: TaskStatus,
    status_message: Option<String>,
    /// 客户端在请求 `_meta.progressToken` 里给的令牌；给了才发进度通知。
    progress_token: Option<Value>,
    /// 已发出的进度条数（规范：progress 必须单调递增）。
    progress_count: u64,
    /// 共享取消旗标：`tasks/cancel` 与退出清理置位，执行器 pump 循环消费。
    cancel: Arc<AtomicBool>,
    /// 脱敏后的参数摘要（审计生命周期条目用）。
    arguments: BTreeMap<String, String>,
    auditor: Auditor,
    /// 终态后存放底层请求本该返回的结果（CallToolResult 形状）。
    result: Option<Value>,
    /// 后台线程是否已收尾（exit 清理只等未收尾的）。
    settled: bool,
}

impl TaskRecord {
    /// ttl 自创建起算，只对终态任务生效：运行中的任务必须留住取消把手，
    /// 不能因 ttl 悄悄消失（规范允许删，但删了就没人能取消它的子进程了）。
    fn is_expired(&self, now_ms: u64) -> bool {
        self.status.is_terminal()
            && self
                .ttl
                .is_some_and(|ttl| now_ms >= self.created_ms.saturating_add(ttl))
    }
}

/// 后台线程执行一次任务所需的全部输入（都是副本/快照：热更新换入新快照后，
/// 已在跑的任务拿的仍是创建时刻的目录与策略，与同步调用同一条纪律）。
pub struct TaskPayload {
    pub tool: ToolDefinition,
    pub values: TypedValues,
    pub cwd: PathBuf,
    pub client_name: String,
    pub policy: SafetyPolicy,
    /// 基础审计器（任务线程会再打上 task_id 标记）。
    pub auditor: Auditor,
    /// 脱敏后的参数摘要（task_created 审计条目用）。
    pub audit_arguments: BTreeMap<String, String>,
}

/// 确认门槛的拒绝（发生在起子进程之前）。`kind` 与同步路径的
/// `structuredContent.error_kind` 一致。
pub struct GateRejection {
    pub kind: &'static str,
    pub message: String,
}

/// 审计 `progress` 条目 reason 里的统一进度数据（P4-05 预留的形状）。
/// `current` / `total` 只在有真实总量依据时给值；本服务端没有依据，恒缺省。
#[derive(Serialize)]
struct TaskProgress<'a> {
    job_id: &'a str,
    status: &'static str,
    progress: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    current: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    total: Option<u64>,
    message: &'a str,
}

/// 任务表 + 出站通知队列的句柄 + 共享并发预算。整体可 Clone（字段全是 Arc），
/// 每个后台任务线程持有一份克隆，用来回报进度与终态。
#[derive(Clone)]
pub struct TaskManager {
    state: Arc<Mutex<State>>,
    /// 终态/进度变化的唤醒源：`tasks/result` 阻塞等待与退出清理都挂在它上。
    gate: Arc<Condvar>,
    /// 服务端的待推送通知队列（主循环在写完当前响应后排空到 stdout）。
    outgoing: Arc<Mutex<Vec<String>>>,
    /// 同步调用与后台任务共享的并发预算（P4-02）。
    limiter: Arc<ConcurrencyLimiter>,
}

struct State {
    tasks: BTreeMap<String, TaskRecord>,
    next_seq: u64,
}

impl TaskManager {
    pub fn new(outgoing: Arc<Mutex<Vec<String>>>, limiter: Arc<ConcurrencyLimiter>) -> Self {
        Self {
            state: Arc::new(Mutex::new(State {
                tasks: BTreeMap::new(),
                next_seq: 0,
            })),
            gate: Arc::new(Condvar::new()),
            outgoing,
            limiter,
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// 接受一个任务增强的 `tools/call`：
    ///
    /// 1. 清理过期终态任务，给新任务腾位（表上限 [`MAX_TABLE`]）；
    /// 2. /dev/urandom 生成不可猜测的 taskId，建 `working` 记录，审计 `created`；
    /// 3. **同步**跑确认门槛（elicitation，请求带 related-task 元数据）——
    ///    拒绝 / 无确认能力 / 用户拒绝 → 任务直接转 `failed`，子进程从未启动；
    /// 4. 通过则起后台线程走完整网关流水线（授权、名额、执行、限额、审计全复用）；
    /// 5. 返回 `CreateTaskResult`（`{"task": {...}}`）。
    pub fn begin(
        &self,
        ttl: Option<u64>,
        progress_token: Option<Value>,
        payload: TaskPayload,
        gate: &mut dyn FnMut(&str) -> Result<(), GateRejection>,
    ) -> Value {
        let now = now_ms();
        let task_id = {
            let mut state = self.lock();
            state.purge_expired(now);
            state.make_room();
            state.fresh_task_id()
        };
        let tagged = payload.auditor.with_task_tag(&task_id);
        let cancel = Arc::new(AtomicBool::new(false));
        {
            let mut state = self.lock();
            state.next_seq += 1;
            let seq = state.next_seq;
            state.tasks.insert(
                task_id.clone(),
                TaskRecord {
                    seq,
                    task_id: task_id.clone(),
                    tool_id: payload.tool.id.clone(),
                    client: payload.client_name.clone(),
                    created_ms: now,
                    updated_ms: now,
                    ttl: ttl.map(|value| value.min(MAX_TTL_MS)),
                    status: TaskStatus::Working,
                    status_message: None,
                    progress_token,
                    progress_count: 0,
                    cancel: Arc::clone(&cancel),
                    arguments: payload.audit_arguments.clone(),
                    auditor: tagged.clone(),
                    result: None,
                    settled: false,
                },
            );
        }
        tagged.task_created(
            &payload.tool.id,
            &payload.client_name,
            payload.audit_arguments.clone(),
        );

        match gate(&task_id) {
            Ok(()) => self.spawn_task(task_id.clone(), cancel, tagged, payload),
            Err(rejection) => {
                // 拒绝路径全部在起子进程之前：任务转 failed，结果就是同步路径
                // 本该回的那条 isError CallToolResult。
                let result = tool_error(rejection.message.clone(), rejection.kind);
                let mut state = self.lock();
                if let Some(record) = state.tasks.get_mut(&task_id) {
                    record.status = TaskStatus::Failed;
                    record.status_message = Some(rejection.message.clone());
                    record.updated_ms = now_ms();
                    record.result = Some(result);
                    record.settled = true;
                }
                drop(state);
                self.gate.notify_all();
            }
        }

        let state = self.lock();
        let task = state
            .tasks
            .get(&task_id)
            .map(task_json)
            .unwrap_or_else(|| json!({}));
        json!({ "task": task })
    }

    /// `tasks/get`：当前任务对象；不知道 / 已过期清理 → `-32602`。
    /// 规范的轮询模型：这里**不阻塞**，非终态就如实回 `working`。
    pub fn get(&self, task_id: &str) -> Result<Value, (i64, String)> {
        let state = self.lock();
        match state.tasks.get(task_id) {
            Some(record) if record.is_expired(now_ms()) => {
                Err((protocol::INVALID_PARAMS, format!("任务已过期：{task_id}")))
            }
            Some(record) => Ok(task_json(record)),
            None => Err((protocol::INVALID_PARAMS, format!("找不到任务：{task_id}"))),
        }
    }

    /// `tasks/result`：阻塞到终态，然后返回底层请求本该返回的结果
    /// （本服务端的底层响应永远是 CallToolResult；协议级错误在任务创建之前
    /// 就已拒绝），并按规范补上 related-task 元数据。
    ///
    /// 刚取消的任务可能状态已是终态、结果还没落进记录（线程在收尾）：
    /// 再给它一小段等待，仍没有就如实回「已取消、无结果」。
    pub fn wait_result(&self, task_id: &str, cap: Duration) -> Result<Value, (i64, String)> {
        let deadline = Instant::now() + cap;
        let mut state = self.lock();
        loop {
            match state.tasks.get(task_id) {
                None => {
                    return Err((protocol::INVALID_PARAMS, format!("找不到任务：{task_id}")));
                }
                Some(record) if record.status.is_terminal() => break,
                Some(_) => {}
            }
            if Instant::now() >= deadline {
                return Err((
                    -32603,
                    format!("等待任务 {task_id} 到达终态超时（任务仍在运行）"),
                ));
            }
            let wait = deadline.saturating_duration_since(Instant::now());
            let (guard, _) = self
                .gate
                .wait_timeout(state, wait)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            state = guard;
        }

        // 终态已到：等线程把结果写进记录。
        let settle_deadline = Instant::now() + RESULT_SETTLE_WAIT;
        let result = loop {
            if let Some(record) = state.tasks.get(task_id)
                && let Some(value) = &record.result
            {
                break value.clone();
            }
            if Instant::now() >= settle_deadline {
                break json!({
                    "content": [{"type": "text", "text": "任务已被取消，未取得执行结果"}],
                    "isError": true,
                });
            }
            let (guard, _) = self
                .gate
                .wait_timeout(state, Duration::from_millis(50))
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            state = guard;
        };

        let mut result = result;
        result["_meta"] = json!({ RELATED_TASK: {"taskId": task_id} });
        Ok(result)
    }

    /// `tasks/cancel`：响应前先把任务转成 `cancelled` 终态（规范要求），再置
    /// 共享取消旗标让执行器收掉进程组。终态再取消 → `-32602`。
    pub fn cancel(&self, task_id: &str) -> Result<Value, (i64, String)> {
        let (cancel, auditor, tool_id, client, arguments) = {
            let mut state = self.lock();
            let Some(record) = state.tasks.get_mut(task_id) else {
                return Err((protocol::INVALID_PARAMS, format!("找不到任务：{task_id}")));
            };
            if record.status.is_terminal() {
                return Err((
                    protocol::INVALID_PARAMS,
                    format!("不能取消任务：已处于终态「{}」", record.status.as_str()),
                ));
            }
            record.status = TaskStatus::Cancelled;
            record.status_message = Some(String::from("任务已被请求取消，进程组正在终止"));
            record.updated_ms = now_ms();
            (
                Arc::clone(&record.cancel),
                record.auditor.clone(),
                record.tool_id.clone(),
                record.client.clone(),
                record.arguments.clone(),
            )
        };
        cancel.store(true, Ordering::SeqCst);
        auditor.task_cancelled(&tool_id, &client, arguments);
        self.gate.notify_all();

        let state = self.lock();
        state
            .tasks
            .get(task_id)
            .map(task_json)
            .ok_or_else(|| (protocol::INVALID_PARAMS, format!("找不到任务：{task_id}")))
    }

    /// 退出清理（P4-01）：stdio 断开后，把全部未终态任务置取消旗标并转
    /// `cancelled`（逐条记审计 cancelled），等它们收尾（进程组 SIGTERM →
    /// 2s 宽限 → SIGKILL，最多 3 秒出头）到上限 `max_wait` 再放行退出。
    pub fn shutdown(&self, max_wait: Duration) {
        let cancelled: Vec<(Auditor, String, String, BTreeMap<String, String>)> = {
            let mut state = self.lock();
            let mut cancelled = Vec::new();
            for record in state.tasks.values_mut() {
                if record.status == TaskStatus::Working {
                    record.status = TaskStatus::Cancelled;
                    record.status_message = Some(String::from("连接断开，任务被服务端取消"));
                    record.updated_ms = now_ms();
                    record.cancel.store(true, Ordering::SeqCst);
                    cancelled.push((
                        record.auditor.clone(),
                        record.tool_id.clone(),
                        record.client.clone(),
                        record.arguments.clone(),
                    ));
                }
            }
            cancelled
        };
        for (auditor, tool_id, client, arguments) in &cancelled {
            auditor.task_cancelled(tool_id, client, arguments.clone());
        }
        self.gate.notify_all();

        let deadline = Instant::now() + max_wait;
        let mut state = self.lock();
        while state.tasks.values().any(|record| !record.settled) {
            if Instant::now() >= deadline {
                return;
            }
            let wait = deadline.saturating_duration_since(Instant::now());
            let (guard, _) = self
                .gate
                .wait_timeout(state, wait)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            state = guard;
        }
    }

    /// 起后台执行线程：完整 `ToolGateway` 流水线（授权、名额、执行、限额、
    /// 审计全复用），目录是「只装这一件工具」的快照，审计器带 task_id 标记，
    /// 并发名额来自共享 limiter。
    fn spawn_task(
        &self,
        task_id: String,
        cancel: Arc<AtomicBool>,
        tagged: Auditor,
        payload: TaskPayload,
    ) {
        let manager = self.clone();
        let thread_name = format!("mcp-task-{}", &task_id[..task_id.len().min(8)]);
        thread::Builder::new()
            .name(thread_name)
            .spawn(move || {
                let catalog = ToolCatalog::new(vec![payload.tool.clone()]);
                let gateway = ToolGateway::new(
                    &catalog,
                    &payload.policy,
                    &tagged,
                    // 门槛已经过：确认能力在网关里视为具备。
                    ClientCapabilities { confirmation: true },
                    payload.client_name.clone(),
                    Arc::clone(&manager.limiter),
                );
                let tool_id = payload.tool.id.clone();
                let progress_task_id = task_id.clone();
                let mut hook = |progress: ExecutionProgress| {
                    manager.record_progress(&progress_task_id, &progress);
                };
                let outcome = gateway.call_with_hooks(
                    &tool_id,
                    &payload.values,
                    &payload.cwd,
                    Some(&cancel),
                    &mut hook,
                );
                let (status, result, message) = match outcome {
                    Ok(result) => {
                        let value = execution_result(&result);
                        if result.success {
                            (TaskStatus::Completed, value, None)
                        } else {
                            // 规范：isError=true 的工具结果 → failed。
                            let summary = result.summary();
                            (TaskStatus::Failed, value, Some(summary))
                        }
                    }
                    Err(error) => {
                        let message = error.to_string();
                        let kind = error.kind();
                        (
                            TaskStatus::Failed,
                            tool_error(message.clone(), kind),
                            Some(message),
                        )
                    }
                };
                manager.finish(&task_id, status, result, message);
            })
            .expect("起 MCP 任务线程");
    }

    /// 后台线程回报进度（P4-03）：更新 statusMessage 与单调递增的计数，
    /// 审计一条 progress（统一进度数据 JSON），客户端索要过 progressToken
    /// 才排一条 `notifications/progress`（无 total，不编百分比）。
    /// 终态后迟到的进度一律忽略。
    ///
    /// 计数递增与通知入队都在同一把状态锁里完成：出站队列里的通知顺序
    /// 因此与 progress 计数严格一致（规范要求 progress 单调递增）。
    fn record_progress(&self, task_id: &str, progress: &ExecutionProgress) {
        let (auditor, tool_id, client, progress_json) = {
            let mut state = self.lock();
            let Some(record) = state.tasks.get_mut(task_id) else {
                return;
            };
            if record.status != TaskStatus::Working {
                return;
            }
            record.progress_count += 1;
            record.updated_ms = now_ms();
            record.status_message = Some(progress.message.clone());
            let data = TaskProgress {
                job_id: task_id,
                status: "working",
                progress: record.progress_count,
                current: None,
                total: None,
                message: &progress.message,
            };
            let progress_json = serde_json::to_string(&data).unwrap_or_default();
            if let Some(token) = &record.progress_token {
                let line = protocol::progress_notification(
                    token,
                    record.progress_count,
                    &progress.message,
                );
                self.outgoing
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .push(line);
            }
            (
                record.auditor.clone(),
                record.tool_id.clone(),
                record.client.clone(),
                progress_json,
            )
        };
        auditor.task_progress(&tool_id, &client, &progress_json);
    }

    /// 落终态：结果总是写进记录（取消的任务也留有底层响应）；状态只允许
    /// `working → 终态` 一次性迁移，已终态的（取消先行）保持不变。
    fn finish(&self, task_id: &str, status: TaskStatus, result: Value, message: Option<String>) {
        {
            let mut state = self.lock();
            if let Some(record) = state.tasks.get_mut(task_id) {
                record.result = Some(result);
                record.updated_ms = now_ms();
                record.settled = true;
                if record.status == TaskStatus::Working {
                    record.status = status;
                    record.status_message = message;
                }
            }
        }
        self.gate.notify_all();
    }
}

/// 清理已过期的终态任务（ttl 自创建起算；规范允许过期即删）。
impl State {
    fn purge_expired(&mut self, now: u64) {
        let expired: Vec<String> = self
            .tasks
            .iter()
            .filter(|(_, record)| record.is_expired(now))
            .map(|(id, _)| id.clone())
            .collect();
        for id in expired {
            self.tasks.remove(&id);
        }
    }

    /// 表满先逐出最老的终态任务，给新任务腾位。全表都是运行中任务时
    /// 不强逐（运行中任务数受并发上限约束，实际到不了 256）。
    fn make_room(&mut self) {
        while self.tasks.len() >= MAX_TABLE {
            let victim = self
                .tasks
                .iter()
                .filter(|(_, record)| record.status.is_terminal())
                .min_by_key(|(_, record)| record.seq)
                .map(|(id, _)| id.clone());
            let Some(victim) = victim else {
                break;
            };
            self.tasks.remove(&victim);
        }
    }

    /// 任务 id：/dev/urandom 128 位熵（规范：无授权上下文时必须加密安全随机）。
    fn fresh_task_id(&self) -> String {
        for _ in 0..3 {
            if let Some(id) = random_task_id()
                && !self.tasks.contains_key(&id)
            {
                return id;
            }
        }
        // 兜底（Linux 上 /dev/urandom 恒在，到不了这里）：时间 + pid + 序号。
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos())
            .unwrap_or(0);
        format!("t{nanos:x}-{:x}-{}", std::process::id(), self.next_seq)
    }
}

/// 32 个十六进制字符的不可猜测任务 id。
fn random_task_id() -> Option<String> {
    use std::io::Read;
    let mut bytes = [0u8; 16];
    let mut file = fs::File::open("/dev/urandom").ok()?;
    file.read_exact(&mut bytes).ok()?;
    Some(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

/// 任务对象（CreateTaskResult / tasks/get / tasks/cancel 响应里的形状）。
/// createdAt / lastUpdatedAt 是规范要求的 ISO 8601；ttl 按规范给实际值或 null。
fn task_json(record: &TaskRecord) -> Value {
    let mut task = json!({
        "taskId": record.task_id,
        "status": record.status.as_str(),
        "createdAt": iso8601(record.created_ms),
        "lastUpdatedAt": iso8601(record.updated_ms),
        "ttl": record.ttl,
        "pollInterval": POLL_INTERVAL_MS,
    });
    if let Some(message) = &record.status_message {
        task["statusMessage"] = json!(message);
    }
    task
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis().min(u128::from(u64::MAX)) as u64)
        .unwrap_or(0)
}

/// Unix 毫秒 → `YYYY-MM-DDTHH:MM:SSZ`（UTC）。无 chrono 依赖，
/// 日期换算用 Howard Hinnant 的 civil_from_days 算法。
fn iso8601(epoch_ms: u64) -> String {
    let secs = epoch_ms / 1000;
    let (year, month, day) = civil_from_days((secs / 86_400) as i64);
    let rest = secs % 86_400;
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        rest / 3_600,
        (rest % 3_600) / 60,
        rest % 60
    )
}

/// 天数（自 1970-01-01）→（年，月，日）。
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = (if mp < 10 { mp + 3 } else { mp - 9 }) as u32;
    (if month <= 2 { year + 1 } else { year }, month, day)
}

#[cfg(test)]
mod tests {
    use std::{
        collections::BTreeMap,
        fs,
        path::{Path, PathBuf},
        sync::{Arc, Mutex},
        time::{Duration, Instant, SystemTime, UNIX_EPOCH},
    };

    use serde_json::{Value, json};

    use super::{
        GateRejection, MAX_TABLE, RELATED_TASK, TaskManager, TaskPayload, iso8601, random_task_id,
    };
    use crate::{
        model::{
            Action, ArgKind, ArgPlacement, Argument, Danger, Domain, McpExposure, RunMode,
            ToolDefinition, TypedValues,
        },
        security::{
            audit::Auditor,
            policy::{McpConfig, SafetyPolicy},
        },
    };

    fn temp_dir(tag: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("toolbox-hub-mcp-tasks-{tag}-{nanos}"));
        fs::create_dir_all(&dir).expect("mkdir");
        dir
    }

    fn tool(id: &str, program: &str, base_argv: Vec<String>) -> ToolDefinition {
        ToolDefinition {
            mcp: Some(McpExposure::default()),
            id: id.to_string(),
            name: String::from("演示"),
            provider: String::from("Test"),
            domain: Domain::System,
            tags: Vec::new(),
            summary: String::from("测试工具"),
            input: None,
            output: None,
            features: None,
            requires: Vec::new(),
            missing_deps: Vec::new(),
            install_hint: None,
            pin: None,
            action: Some(Action {
                program: program.to_string(),
                base_argv,
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
                allow_empty: true,
            }),
            mode: RunMode::Capture,
            danger: Danger::Safe,
            path: PathBuf::from(program),
            ready: true,
        }
    }

    fn config() -> McpConfig {
        let text = "[mcp]\nenabled = true\n[exposure]\ndefault = \"deny\"\nallow = [\"test:echo\"]\ndeny = []\n";
        toml::from_str(text).expect("配置")
    }

    /// 一个 echo 工具的任务载荷：跑 `/bin/echo <text>`，即刻完成。
    fn echo_payload(dir: &Path, text: &str) -> TaskPayload {
        let mut values = TypedValues::new();
        values.set_text("text", text);
        TaskPayload {
            tool: tool("test:echo", "/bin/echo", Vec::new()),
            values,
            cwd: PathBuf::from("/tmp"),
            client_name: String::from("test"),
            policy: SafetyPolicy::new(config()),
            auditor: Auditor::new(dir.join("audit.jsonl"), true),
            audit_arguments: BTreeMap::new(),
        }
    }

    /// 一个 sleep 工具的任务载荷：跑 `/bin/sleep <seconds>`，用来测阻塞与取消。
    fn sleep_payload(dir: &Path, seconds: &str) -> TaskPayload {
        let mut values = TypedValues::new();
        values.set_text("text", seconds);
        TaskPayload {
            tool: tool("test:echo", "/bin/sleep", Vec::new()),
            values,
            cwd: PathBuf::from("/tmp"),
            client_name: String::from("test"),
            policy: SafetyPolicy::new(config()),
            auditor: Auditor::new(dir.join("audit.jsonl"), true),
            audit_arguments: BTreeMap::new(),
        }
    }

    fn manager() -> (TaskManager, PathBuf) {
        let dir = temp_dir("mgr");
        let limiter = Arc::new(crate::service::gateway::ConcurrencyLimiter::new(2));
        (
            TaskManager::new(Arc::new(Mutex::new(Vec::new())), limiter),
            dir,
        )
    }

    fn approve(_: &str) -> Result<(), GateRejection> {
        Ok(())
    }

    /// 拒绝门槛：与服务端真实行为一致 —— 先写一条带 task_id 标记的 denied
    /// 审计，再回 GateRejection。
    fn reject(
        dir: &Path,
        kind: &'static str,
        message: &str,
    ) -> impl FnMut(&str) -> Result<(), GateRejection> {
        let message = message.to_string();
        let auditor = Auditor::new(dir.join("audit.jsonl"), true);
        move |task_id: &str| {
            auditor
                .with_task_tag(task_id)
                .denied("test:echo", "test", kind, BTreeMap::new());
            Err(GateRejection {
                kind,
                message: message.clone(),
            })
        }
    }

    fn audit_text(dir: &Path) -> String {
        fs::read_to_string(dir.join("audit.jsonl")).unwrap_or_default()
    }

    #[test]
    fn task_ids_are_unguessable_and_unique() {
        let mut seen = std::collections::BTreeSet::new();
        for _ in 0..64 {
            let id = random_task_id().expect("Linux 恒有 /dev/urandom");
            assert_eq!(id.len(), 32, "128 位熵 = 32 个十六进制字符：{id}");
            assert!(id.chars().all(|c| c.is_ascii_hexdigit()), "{id}");
            assert!(seen.insert(id), "任务 id 不得重复");
        }
    }

    #[test]
    fn timestamps_are_iso8601_utc() {
        assert_eq!(iso8601(0), "1970-01-01T00:00:00Z");
        assert_eq!(iso8601(1_767_225_600_000), "2026-01-01T00:00:00Z");
        assert_eq!(iso8601(1_794_009_600_000), "2026-11-07T00:00:00Z");
        assert_eq!(iso8601(1_709_164_800_000), "2024-02-29T00:00:00Z", "闰日");
        assert_eq!(iso8601(1_767_225_599_000), "2025-12-31T23:59:59Z");
    }

    /// 确认门槛拒绝 → 任务转 failed，结果就是同步路径那条 isError 结果，
    /// 全程没有子进程（审计里只有 created + denied，没有 completed）。
    #[test]
    fn a_rejected_gate_fails_the_task_before_any_process() {
        let (manager, dir) = manager();
        let created = manager.begin(
            None,
            None,
            echo_payload(&dir, "hi"),
            &mut reject(&dir, "declined_by_user", "test:echo 未获用户确认，没有执行"),
        );
        let task = &created["task"];
        assert_eq!(task["status"], "failed");
        assert_eq!(task["taskId"].as_str().map(str::len), Some(32));
        assert!(task["createdAt"].is_string());
        assert_eq!(
            task["statusMessage"], "test:echo 未获用户确认，没有执行",
            "{created}"
        );

        let audit = audit_text(&dir);
        assert!(audit.contains("\"decision\":\"created\""));
        assert!(audit.contains("declined_by_user"));
        assert!(!audit.contains("\"decision\":\"completed\""), "{audit}");

        // tasks/get 看得到终态；tasks/result 拿到底层响应（isError）+ related-task。
        let status = manager
            .get(task["taskId"].as_str().expect("id"))
            .expect("get");
        assert_eq!(status["status"], "failed");
        let result = manager
            .wait_result(task["taskId"].as_str().expect("id"), Duration::from_secs(5))
            .expect("终态结果");
        assert_eq!(result["isError"], true);
        assert_eq!(
            result["_meta"][RELATED_TASK]["taskId"], task["taskId"],
            "{result}"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    /// 通过门槛 → 后台执行 → completed；tasks/result 阻塞到终态并带
    /// related-task 元数据；审计里 created / completed 都带 task_id。
    #[test]
    fn a_task_runs_to_completion_and_result_carries_related_task() {
        let (manager, dir) = manager();
        let created = manager.begin(None, None, echo_payload(&dir, "任务回显"), &mut approve);
        let task_id = created["task"]["taskId"].as_str().expect("id").to_string();
        assert_eq!(created["task"]["status"], "working");

        let started = Instant::now();
        let result = manager
            .wait_result(&task_id, Duration::from_secs(30))
            .expect("结果");
        assert_eq!(result["isError"], false, "{result}");
        assert!(
            result["content"][0]["text"]
                .as_str()
                .expect("text")
                .contains("任务回显")
        );
        assert_eq!(result["_meta"][RELATED_TASK]["taskId"], task_id.as_str());
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "echo 任务不该等很久"
        );

        let status = manager.get(&task_id).expect("get");
        assert_eq!(status["status"], "completed");

        let audit = audit_text(&dir);
        assert!(audit.contains("\"decision\":\"created\""));
        assert!(audit.contains("\"decision\":\"completed\""));
        assert!(
            audit.contains(&format!("\"task_id\":\"{task_id}\"")),
            "生命周期条目要带 task_id：{audit}"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    /// tasks/result 对运行中的任务要**阻塞**到终态（规范要求）。
    #[test]
    fn wait_result_blocks_until_the_task_reaches_a_terminal_state() {
        let (manager, dir) = manager();
        let created = manager.begin(None, None, sleep_payload(&dir, "1"), &mut approve);
        let task_id = created["task"]["taskId"].as_str().expect("id").to_string();

        // 先确认还在跑（working），再阻塞等结果。
        let working = manager.get(&task_id).expect("get");
        assert_eq!(working["status"], "working", "{working}");

        let started = Instant::now();
        let result = manager
            .wait_result(&task_id, Duration::from_secs(30))
            .expect("结果");
        assert!(
            started.elapsed() >= Duration::from_millis(900),
            "必须真的阻塞到进程结束：{:?}",
            started.elapsed()
        );
        assert_eq!(result["isError"], false);
        let _ = fs::remove_dir_all(&dir);
    }

    /// 取消：响应前转 cancelled 终态；终态再取消回 -32602；之后状态不再变；
    /// 结果照常可取（isError 的底层响应）；审计带 cancelled 条目。
    #[test]
    fn cancel_transitions_before_the_response_and_is_irreversible() {
        let (manager, dir) = manager();
        let created = manager.begin(None, None, sleep_payload(&dir, "30"), &mut approve);
        let task_id = created["task"]["taskId"].as_str().expect("id").to_string();

        let cancelled = manager.cancel(&task_id).expect("取消");
        assert_eq!(cancelled["status"], "cancelled");
        assert_eq!(cancelled["taskId"], task_id.as_str());

        let again = manager.cancel(&task_id).expect_err("终态不能再取消");
        assert_eq!(again.0, -32602, "{again:?}");

        let result = manager
            .wait_result(&task_id, Duration::from_secs(15))
            .expect("取消后的底层结果");
        assert_eq!(result["isError"], true, "{result}");
        assert_eq!(result["_meta"][RELATED_TASK]["taskId"], task_id.as_str());

        let status = manager.get(&task_id).expect("get");
        assert_eq!(status["status"], "cancelled", "取消不可再变：{status}");

        let audit = audit_text(&dir);
        assert!(audit.contains("\"decision\":\"cancelled\""));
        assert!(audit.contains(&format!("\"task_id\":\"{task_id}\"")));
        let _ = fs::remove_dir_all(&dir);
    }

    /// ttl 过期的终态任务在下一次 begin 时被清理（规范：过期即可删）。
    #[test]
    fn expired_terminal_tasks_are_purged_on_the_next_begin() {
        let (manager, dir) = manager();
        let created = manager.begin(
            Some(1),
            None,
            echo_payload(&dir, "hi"),
            &mut reject(&dir, "declined_by_user", "拒绝"),
        );
        let first = created["task"]["taskId"].as_str().expect("id").to_string();

        std::thread::sleep(Duration::from_millis(30));
        manager.begin(
            None,
            None,
            echo_payload(&dir, "hi"),
            &mut reject(&dir, "declined_by_user", "拒绝"),
        );

        let gone = manager.get(&first).expect_err("过期任务应已清理");
        assert_eq!(gone.0, -32602);
        let _ = fs::remove_dir_all(&dir);
    }

    /// 表上限 256：满后逐出最老的终态任务，不无限增长。
    #[test]
    fn the_table_cap_evicts_the_oldest_terminal_task() {
        let (manager, dir) = manager();
        let mut first_id = String::new();
        for index in 0..(MAX_TABLE + 1) {
            let created = manager.begin(
                None,
                None,
                echo_payload(&dir, "hi"),
                &mut reject(&dir, "declined_by_user", "拒绝"),
            );
            if index == 0 {
                first_id = created["task"]["taskId"].as_str().expect("id").to_string();
            }
        }
        let gone = manager.get(&first_id).expect_err("最老的应被逐出");
        assert_eq!(gone.0, -32602);

        let state = manager.lock();
        assert!(
            state.tasks.len() <= MAX_TABLE,
            "表不得超过上限：{}",
            state.tasks.len()
        );
        drop(state);
        let _ = fs::remove_dir_all(&dir);
    }

    /// 退出清理：置取消旗标、转 cancelled、等收尾 —— 30 秒的 sleep 任务
    /// 远在上限 8 秒之前就该被进程组信号收掉。
    #[test]
    fn shutdown_cancels_running_tasks_and_does_not_wait_forever() {
        let (manager, dir) = manager();
        let created = manager.begin(None, None, sleep_payload(&dir, "30"), &mut approve);
        let task_id = created["task"]["taskId"].as_str().expect("id").to_string();

        let started = Instant::now();
        manager.shutdown(Duration::from_secs(8));
        let waited = started.elapsed();
        assert!(
            waited < Duration::from_secs(8),
            "SIGTERM 就该收掉 sleep：等了 {waited:?}"
        );
        let status = manager.get(&task_id).expect("get");
        assert_eq!(status["status"], "cancelled", "{status}");
        let _ = fs::remove_dir_all(&dir);
    }

    /// 进度（P4-03）：计数单调递增、TaskProgress 与通知都不带 total、
    /// statusMessage 承载同一信息、审计 progress 条目带统一进度 JSON。
    #[test]
    fn progress_updates_are_monotonic_carry_no_total_and_reach_every_surface() {
        let dir = temp_dir("progress");
        let outgoing = Arc::new(Mutex::new(Vec::new()));
        let limiter = Arc::new(crate::service::gateway::ConcurrencyLimiter::new(2));
        let manager = TaskManager::new(Arc::clone(&outgoing), limiter);

        let created = manager.begin(
            None,
            Some(json!("tok-progress")),
            sleep_payload(&dir, "30"),
            &mut approve,
        );
        let task_id = created["task"]["taskId"].as_str().expect("id").to_string();

        // 直接驱动两次进度（后台线程自己的 1 秒节流会再叠加，单调性不受影响）。
        manager.record_progress(
            &task_id,
            &super::ExecutionProgress {
                elapsed_ms: 10,
                message: String::from("运行中"),
            },
        );
        manager.record_progress(
            &task_id,
            &super::ExecutionProgress {
                elapsed_ms: 1_010,
                message: String::from("运行中 · out_time=00:00:01"),
            },
        );

        let status = manager.get(&task_id).expect("get");
        assert_eq!(
            status["statusMessage"], "运行中 · out_time=00:00:01",
            "statusMessage 承载同一信息：{status}"
        );

        // 通知面：带 progressToken、progress 递增、无 total。
        let lines: Vec<Value> = outgoing
            .lock()
            .expect("锁")
            .iter()
            .map(|line| serde_json::from_str(line).expect("合法 JSON"))
            .collect();
        assert!(!lines.is_empty(), "索要过 progressToken 就该有通知");
        let mut last = 0;
        for line in &lines {
            assert_eq!(line["method"], "notifications/progress");
            assert_eq!(line["params"]["progressToken"], "tok-progress");
            let progress = line["params"]["progress"].as_u64().expect("progress");
            assert!(progress > last, "progress 必须单调递增：{lines:?}");
            last = progress;
            assert!(line["params"]["message"].is_string());
            assert!(
                line["params"].get("total").is_none(),
                "无总量依据不给 total：{}",
                line["params"]
            );
        }

        // 审计面：progress 条目带统一进度数据 JSON（reason 是转义存储的
        // 字符串，同样无 total）。
        let audit = audit_text(&dir);
        assert!(audit.contains(r#""decision":"progress""#), "{audit}");
        assert!(audit.contains(r#"\"job_id\""#), "{audit}");
        assert!(!audit.contains("total"), "审计里也不编总量：{audit}");

        manager.shutdown(Duration::from_secs(8));
        let _ = fs::remove_dir_all(&dir);
    }
}
