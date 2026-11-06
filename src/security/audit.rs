//! MCP 调用审计（JSONL，一行一条）。
//!
//! 与 TUI 的 `history.log` 分开：历史是给人「回填再改」用的，审计是给安全复盘用的。
//! 这里**只记元数据**：时间、工具 id、客户端、授权结果、退出码、耗时、脱敏后的
//! 参数摘要。原始 JSON 请求、stdout / stderr、敏感值一律不落盘。
//!
//! 写不进去只是返回 `io::Error`，调用方记一句即可 —— 绝不能往 stdout 写，
//! 那会污染 MCP 协议流（stdout 只归协议）。

use std::{
    collections::BTreeMap,
    fs::{self, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};

use crate::model::{Action, TypedValues};

/// 一条审计记录。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuditRecord {
    /// Unix 秒。
    pub epoch: u64,
    pub tool_id: String,
    /// 客户端标识（可用时；stdio 首期固定 `"stdio"`）。
    pub client: String,
    /// `denied` / `completed`。
    pub decision: String,
    /// 是否通过了服务端授权。
    pub authorized: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    pub elapsed_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub success: Option<bool>,
    /// 脱敏后的参数摘要（键 → 一行文字）。
    pub arguments: BTreeMap<String, String>,
}

impl AuditRecord {
    fn now(tool_id: &str, client: &str, decision: &'static str) -> Self {
        Self {
            epoch: now_epoch(),
            tool_id: tool_id.to_string(),
            client: client.to_string(),
            decision: decision.to_string(),
            authorized: decision != "denied",
            reason: None,
            exit_code: None,
            elapsed_ms: 0,
            success: None,
            arguments: BTreeMap::new(),
        }
    }
}

/// 把调用参数变成「可以落盘」的摘要：敏感字段替换为 `***`。
///
/// 没有参数契约的工具（本地脚本）不接受任何参数，返回空表。
pub fn redacted_arguments(
    action: Option<&Action>,
    values: &TypedValues,
) -> BTreeMap<String, String> {
    match action {
        Some(action) => values.summary(&action.arguments),
        None => BTreeMap::new(),
    }
}

/// 审计写入器。
#[derive(Clone, Debug)]
pub struct Auditor {
    path: PathBuf,
    enabled: bool,
}

impl Auditor {
    pub fn new(path: impl Into<PathBuf>, enabled: bool) -> Self {
        Self {
            path: path.into(),
            enabled,
        }
    }

    /// 测试/临时用：不落盘。
    pub fn disabled() -> Self {
        Self::new(PathBuf::from("/dev/null"), false)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// 授权被拒：进程根本没启动，也要记一条。
    pub fn denied(
        &self,
        tool_id: &str,
        client: &str,
        reason: &str,
        arguments: BTreeMap<String, String>,
    ) {
        let mut record = AuditRecord::now(tool_id, client, "denied");
        record.reason = Some(reason.to_string());
        record.arguments = arguments;
        let _ = self.record(&record);
    }

    /// 执行完成（成功、失败、超时都算完成）。
    pub fn completed(
        &self,
        tool_id: &str,
        client: &str,
        exit_code: Option<i32>,
        elapsed_ms: u64,
        success: bool,
        arguments: BTreeMap<String, String>,
    ) {
        let mut record = AuditRecord::now(tool_id, client, "completed");
        record.exit_code = exit_code;
        record.elapsed_ms = elapsed_ms;
        record.success = Some(success);
        record.arguments = arguments;
        let _ = self.record(&record);
    }

    /// 追加一行 JSON。
    pub fn record(&self, record: &AuditRecord) -> io::Result<()> {
        if !self.enabled {
            return Ok(());
        }
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)?;
        }
        let line = serde_json::to_string(record)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        let mut handle = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;
        writeln!(handle, "{line}")
    }

    /// 读出全部记录（测试用；正式功能不需要回读）。
    pub fn read_all(path: &Path) -> Vec<AuditRecord> {
        let Ok(text) = fs::read_to_string(path) else {
            return Vec::new();
        };
        text.lines()
            .filter_map(|line| serde_json::from_str(line).ok())
            .collect()
    }
}

fn now_epoch() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use std::{
        collections::BTreeMap,
        fs,
        path::PathBuf,
        time::{SystemTime, UNIX_EPOCH},
    };

    use super::{Auditor, redacted_arguments};
    use crate::model::{Action, Argument, TypedValues};

    fn temp_file(tag: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("toolbox-hub-audit-{tag}-{nanos}"));
        fs::create_dir_all(&dir).expect("mkdir");
        dir.join("mcp-audit.jsonl")
    }

    fn sensitive_action() -> Action {
        Action {
            program: String::from("echo"),
            base_argv: Vec::new(),
            arguments: vec![Argument {
                key: String::from("token"),
                label: String::from("令牌"),
                kind: crate::model::ArgKind::Text,
                default: None,
                choices: Vec::new(),
                required: true,
                flag: Some(String::from("--token")),
                flag_join: false,
                placement: crate::model::ArgPlacement::Middle,
                sensitive: true,
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

    #[test]
    fn sensitive_values_never_reach_the_audit_file() {
        let file = temp_file("secret");
        let auditor = Auditor::new(&file, true);
        let action = sensitive_action();

        let mut values = TypedValues::new();
        values.set_text("token", "s3cr3t-token-value");
        let summary = redacted_arguments(Some(&action), &values);

        auditor.denied("test:echo", "stdio", "not_allowed", summary.clone());
        auditor.completed("test:echo", "stdio", Some(0), 12, true, summary);

        let text = fs::read_to_string(&file).expect("审计文件");
        assert!(
            !text.contains("s3cr3t-token-value"),
            "敏感值不能落盘: {text}"
        );
        assert!(text.contains("***"), "要有脱敏占位符: {text}");
        assert!(!text.contains("stdout"), "输出不进审计: {text}");
        assert!(!text.contains("stderr"), "输出不进审计: {text}");

        let records = Auditor::read_all(&file);
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].decision, "denied");
        assert!(!records[0].authorized);
        assert_eq!(records[0].reason.as_deref(), Some("not_allowed"));
        assert_eq!(records[1].exit_code, Some(0));
        assert_eq!(records[1].elapsed_ms, 12);
        assert_eq!(records[1].success, Some(true));

        fs::remove_dir_all(file.parent().expect("parent")).expect("cleanup");
    }

    #[test]
    fn a_disabled_auditor_writes_nothing() {
        let file = temp_file("disabled");
        let auditor = Auditor::disabled();
        auditor.completed("test:echo", "stdio", Some(0), 1, true, BTreeMap::new());
        assert!(Auditor::read_all(&file).is_empty());
        let _ = fs::remove_dir_all(file.parent().expect("parent"));
    }

    #[test]
    fn a_tool_without_an_argument_contract_has_no_arguments_to_record() {
        assert!(redacted_arguments(None, &TypedValues::new()).is_empty());
    }
}
