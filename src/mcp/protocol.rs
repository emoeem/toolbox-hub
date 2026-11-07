//! JSON-RPC 2.0（MCP 传输编码）的最小实现。
//!
//! stdio 传输是**按行分隔的 JSON**：一行一条消息。这里只实现服务端要面对的
//! 那一小块：
//!
//! * 解析请求 / 通知（`id` 缺失即通知）；
//! * 组装 result / error 响应；
//! * 拒绝批量请求（MCP 2025-06-18 起已移除批量支持，直接回 `INVALID_REQUEST`）。
//!
//! 为什么不用官方 `rmcp` SDK：计划书 §3.2 推荐 rmcp，但它会把 Tokio 全家
//! 拖进一个**纯同步**程序，MSRV / `--no-default-features` / 依赖面三头都要重审
//! （计划书 §9 也要求新增依赖同步维护 PKGBUILD 与锁文件）。MCP 服务端真正用到的
//! 协议面很小（initialize / tools/list / tools/call / elicitation/create），
//! 一份带协议测试的最小实现反而更贴近本项目的「少依赖」纪律。偏差已记入 Work Log。

use serde_json::{Value, json};

/// 本服务端实现的协议版本（2025-06-18 稳定版）。
pub const PROTOCOL_VERSION: &str = "2025-06-18";

/// `tools/listChanged` 变更通知（2025-06-18 与 2025-11-25 同名同义）。
pub const LIST_CHANGED: &str = "notifications/tools/list_changed";

pub const PARSE_ERROR: i64 = -32700;
pub const INVALID_REQUEST: i64 = -32600;
pub const METHOD_NOT_FOUND: i64 = -32601;
pub const INVALID_PARAMS: i64 = -32602;

/// 一条进来的请求（或通知，`id = None`）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Incoming {
    pub id: Option<Value>,
    pub method: String,
    pub params: Value,
}

/// 解析失败的原因与错误码。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProtocolError {
    pub code: i64,
    pub message: String,
}

impl ProtocolError {
    fn new(code: i64, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

/// 解析一行。失败时给出**协议级**错误（带正确的 JSON-RPC 错误码）。
pub fn parse(line: &str) -> Result<Incoming, ProtocolError> {
    let value: Value = serde_json::from_str(line)
        .map_err(|error| ProtocolError::new(PARSE_ERROR, format!("JSON 解析失败：{error}")))?;

    if value.is_array() {
        // JSON-RPC 批量请求：MCP 已不支持，明确拒绝而不是假装处理第一条。
        return Err(ProtocolError::new(
            INVALID_REQUEST,
            "不支持批量请求（MCP 已移除该特性）",
        ));
    }
    let Some(object) = value.as_object() else {
        return Err(ProtocolError::new(INVALID_REQUEST, "消息必须是 JSON 对象"));
    };

    let Some(method) = object.get("method").and_then(Value::as_str) else {
        return Err(ProtocolError::new(INVALID_REQUEST, "缺少 method 字段"));
    };

    let id = match object.get("id") {
        None | Some(Value::Null) => None,
        Some(id @ (Value::Number(_) | Value::String(_))) => Some(id.clone()),
        Some(_) => {
            return Err(ProtocolError::new(INVALID_REQUEST, "id 只能是数字或字符串"));
        }
    };

    Ok(Incoming {
        id,
        method: method.to_string(),
        params: object.get("params").cloned().unwrap_or(Value::Null),
    })
}

/// 成功响应（一行 JSON）。
pub fn success(id: &Value, result: Value) -> String {
    json!({"jsonrpc": "2.0", "id": id, "result": result}).to_string()
}

/// 失败响应（一行 JSON）；`id` 缺失时用 `null`（JSON-RPC 规范）。
pub fn failure(id: Option<&Value>, code: i64, message: &str) -> String {
    let id = id.cloned().unwrap_or(Value::Null);
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}}).to_string()
}

/// 无参数的通知（如 `notifications/tools/list_changed`）。
pub fn notification(method: &str) -> String {
    json!({"jsonrpc": "2.0", "method": method}).to_string()
}

#[cfg(test)]
mod tests {
    use super::{failure, parse, success};
    use crate::mcp::protocol::{INVALID_PARAMS, INVALID_REQUEST, PARSE_ERROR};
    use serde_json::{Value, json};

    #[test]
    fn requests_and_notifications_are_told_apart() {
        let request = parse(r#"{"jsonrpc":"2.0","id":7,"method":"ping"}"#).expect("请求");
        assert_eq!(request.id, Some(json!(7)));
        assert_eq!(request.method, "ping");
        assert_eq!(request.params, Value::Null);

        let notification =
            parse(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#).expect("通知");
        assert_eq!(notification.id, None);
    }

    #[test]
    fn malformed_lines_carry_the_right_error_codes() {
        let broken = parse("{nope").expect_err("坏 JSON");
        assert_eq!(broken.code, PARSE_ERROR);

        let batch = parse(r#"[{"jsonrpc":"2.0","id":1,"method":"ping"}]"#).expect_err("批量");
        assert_eq!(batch.code, INVALID_REQUEST);

        let no_method = parse(r#"{"jsonrpc":"2.0","id":1}"#).expect_err("没方法");
        assert_eq!(no_method.code, INVALID_REQUEST);

        let bad_id =
            parse(r#"{"jsonrpc":"2.0","id":{"x":1},"method":"ping"}"#).expect_err("id 类型不对");
        assert_eq!(bad_id.code, INVALID_REQUEST);
    }

    #[test]
    fn id_may_be_a_string() {
        let request =
            parse(r#"{"jsonrpc":"2.0","id":"abc","method":"x","params":{"a":1}}"#).expect("请求");
        assert_eq!(request.id, Some(json!("abc")));
        assert_eq!(request.params, json!({"a":1}));
    }

    #[test]
    fn responses_are_single_lines_with_matching_id() {
        let line = success(&json!(3), json!({"ok": true}));
        assert!(!line.contains('\n'));
        let parsed: Value = serde_json::from_str(&line).expect("合法 JSON");
        assert_eq!(parsed["id"], 3);
        assert_eq!(parsed["result"]["ok"], true);
        assert!(parsed.get("error").is_none());

        let line = failure(Some(&json!("abc")), INVALID_PARAMS, "缺参数");
        let parsed: Value = serde_json::from_str(&line).expect("合法 JSON");
        assert_eq!(parsed["id"], "abc");
        assert_eq!(parsed["error"]["code"], INVALID_PARAMS);

        let line = failure(None, INVALID_PARAMS, "解析失败");
        let parsed: Value = serde_json::from_str(&line).expect("合法 JSON");
        assert_eq!(parsed["id"], Value::Null, "没有 id 时用 null");
    }

    #[test]
    fn notifications_have_no_id_and_no_params() {
        let bare = super::notification(super::LIST_CHANGED);
        assert!(!bare.contains('\n'));
        let parsed: Value = serde_json::from_str(&bare).expect("合法 JSON");
        assert_eq!(parsed["method"], super::LIST_CHANGED);
        assert!(parsed.get("id").is_none(), "通知不能有 id");
        assert!(parsed.get("params").is_none(), "无参数通知不带 params");
    }
}
