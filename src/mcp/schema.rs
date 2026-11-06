//! Manifest 参数 → MCP JSON Schema 的映射（计划书 P3-02 / §4.2）。
//!
//! | Toolbox Hub 参数 | JSON Schema |
//! | --- | --- |
//! | `Text` / `Path` / `Dynamic` | `string` |
//! | `Choice` | 带 `enum` 的 `string` |
//! | `Toggle` | `boolean` |
//! | `repeatable` | `array`（每项一个 argv 元素，**不经分隔符**） |
//!
//! `additionalProperties: false`：AI 多传一个字段就在这里被拒，
//! 不会静默丢弃 —— 静默丢参数是 P0 审计里明确的反模式。

use serde_json::{Value, json};

use crate::model::{ArgKind, ToolDefinition, TypedValues};

/// 工具的输入 Schema。没有参数契约的工具得到空对象（不接受任何字段）。
pub fn input_schema(tool: &ToolDefinition) -> Value {
    let mut properties = serde_json::Map::new();
    let mut required: Vec<Value> = Vec::new();

    if let Some(action) = &tool.action {
        for argument in &action.arguments {
            let (property_type, extra) = property_schema(argument);
            let mut property = serde_json::Map::new();
            property.insert(String::from("type"), json!(property_type));
            if let Some(items) = extra {
                property.insert(String::from("items"), items);
            }
            if let Some(enum_values) = choice_values(argument) {
                property.insert(String::from("enum"), json!(enum_values));
            }
            let mut description = argument.label.clone();
            if let Some(help) = &argument.help {
                description.push('（');
                description.push_str(help);
                description.push('）');
            }
            property.insert(String::from("description"), json!(description));
            if let Some(default) = default_value(argument) {
                property.insert(String::from("default"), default);
            }
            properties.insert(argument.key.clone(), Value::Object(property));

            if argument.required {
                required.push(json!(argument.key));
            }
        }
    }

    json!({
        "type": "object",
        "properties": properties,
        "required": required,
        "additionalProperties": false,
    })
}

/// 单个参数的类型：repeatable 是数组，元素类型跟基础类型走。
fn property_schema(argument: &crate::model::Argument) -> (&'static str, Option<Value>) {
    let base = match argument.kind {
        ArgKind::Toggle => "boolean",
        _ => "string",
    };
    if argument.repeatable {
        ("array", Some(json!({"type": base})))
    } else {
        (base, None)
    }
}

fn choice_values(argument: &crate::model::Argument) -> Option<Vec<String>> {
    if argument.kind == ArgKind::Choice && !argument.choices.is_empty() {
        Some(
            argument
                .choices
                .iter()
                .map(|choice| choice.value.clone())
                .collect(),
        )
    } else {
        None
    }
}

fn default_value(argument: &crate::model::Argument) -> Option<Value> {
    let default = argument.default.as_deref()?;
    if default.is_empty() {
        return None;
    }
    if argument.kind == ArgKind::Toggle {
        return Some(json!(default == "true"));
    }
    if argument.repeatable {
        // 缺省值按分隔符理解（与 TUI 表单同一语义）；声明成 array 是给 AI 的提示。
        return Some(json!(argument.split_values(default)));
    }
    Some(json!(default))
}

/// 把 `tools/call` 的 `arguments` 对象转成类型化取值。
///
/// 这一层只做**形状**校验（键、JSON 类型）；必填、Choice 合法性等**语义**校验
/// 仍由服务层完成（见 `crate::service::argument`）—— 语义只有一套，MCP 不另写。
pub fn values_from_json(
    tool: &ToolDefinition,
    arguments: Option<&Value>,
) -> Result<TypedValues, String> {
    // arguments 可以整体省略（或为 null）＝空参数。
    let empty = serde_json::Map::new();
    let object = match arguments {
        None | Some(Value::Null) => &empty,
        Some(Value::Object(map)) => map,
        Some(_) => return Err(String::from("arguments 必须是对象（可以省略）")),
    };
    let Some(action) = &tool.action else {
        return if object.is_empty() {
            Ok(TypedValues::new())
        } else {
            Err(String::from("这件工具没有参数契约，不接受 arguments"))
        };
    };

    let mut values = TypedValues::new();
    for (key, value) in object {
        let Some(argument) = action.arguments.iter().find(|a| a.key == *key) else {
            return Err(format!("未声明的参数：{key}"));
        };
        if value.is_null() {
            // null 当成「没给」，让缺省值/必填逻辑按原路走。
            continue;
        }
        match (argument.kind, argument.repeatable, value) {
            (ArgKind::Toggle, _, Value::Bool(on)) => values.set_bool(key, *on),
            (ArgKind::Toggle, _, Value::String(text)) if text == "true" || text == "false" => {
                // 宽容：字符串形式的布尔也收（有些客户端序列化习惯不同）。
                values.set_bool(key, text == "true");
            }
            (ArgKind::Toggle, _, _) => {
                return Err(format!("参数 {key} 是开关，需要 boolean"));
            }
            (_, true, Value::Array(items)) => {
                let mut list = Vec::with_capacity(items.len());
                for item in items {
                    list.push(scalar_string(key, item)?);
                }
                values.set_list(key, list);
            }
            (_, true, scalar) => {
                // 单值也接受：一个元素的数组语义等价，不做分隔符拆分。
                values.set_list(key, vec![scalar_string(key, scalar)?]);
            }
            (_, false, scalar) => values.set_text(key, scalar_string(key, scalar)?),
        }
    }
    Ok(values)
}

fn scalar_string(key: &str, value: &Value) -> Result<String, String> {
    match value {
        Value::String(text) => Ok(text.clone()),
        Value::Number(number) => Ok(number.to_string()),
        Value::Bool(_) => Err(format!("参数 {key} 需要字符串，不是布尔")),
        Value::Array(_) => Err(format!("参数 {key} 不能嵌套数组")),
        Value::Object(_) => Err(format!("参数 {key} 需要字符串，不是对象")),
        Value::Null => Ok(String::new()),
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::{input_schema, values_from_json};
    use crate::model::{
        Action, ArgKind, ArgPlacement, Argument, Choice, Danger, Domain, McpExposure, RunMode,
        ToolDefinition, TypedValues,
    };
    use serde_json::{Value, json};

    fn make_tool(action: Option<Action>) -> ToolDefinition {
        let mut tool = ToolDefinition {
            mcp: Some(McpExposure::default()),
            id: String::from("manifest:demo"),
            name: String::from("演示"),
            provider: String::from("Test"),
            domain: Domain::System,
            tags: Vec::new(),
            summary: String::new(),
            input: None,
            output: None,
            features: None,
            requires: Vec::new(),
            missing_deps: Vec::new(),
            install_hint: None,
            pin: None,
            action,
            mode: RunMode::Capture,
            danger: Danger::Safe,
            path: PathBuf::from("/bin/echo"),
            ready: true,
        };
        tool.mcp = Some(McpExposure::default());
        tool
    }

    fn argument(key: &str, kind: ArgKind) -> Argument {
        Argument {
            key: key.to_string(),
            label: format!("{key} 标签"),
            kind,
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
        }
    }

    fn action(arguments: Vec<Argument>) -> Action {
        Action {
            program: String::from("echo"),
            base_argv: Vec::new(),
            arguments,
            duration_from: None,
            limit_from: None,
            foreach: None,
            ok_exit_codes: vec![0],
            allow_empty: true,
        }
    }

    #[test]
    fn schema_maps_kinds_choices_and_repeatable() {
        let mut quality = argument("quality", ArgKind::Choice);
        quality.choices = vec![Choice::new("高", "18"), Choice::new("平衡", "23")];
        quality.default = Some(String::from("23"));
        quality.required = true;

        let mut urls = argument("url", ArgKind::Text);
        urls.repeatable = true;

        let tool = make_tool(Some(action(vec![
            argument("path", ArgKind::Path),
            argument("subtitles", ArgKind::Toggle),
            quality,
            urls,
        ])));

        let schema = input_schema(&tool);
        assert_eq!(schema["type"], "object");
        assert_eq!(schema["additionalProperties"], false);
        assert_eq!(schema["properties"]["path"]["type"], "string");
        assert_eq!(schema["properties"]["subtitles"]["type"], "boolean");
        assert_eq!(schema["properties"]["quality"]["enum"], json!(["18", "23"]));
        assert_eq!(schema["properties"]["quality"]["default"], "23");
        assert_eq!(schema["properties"]["url"]["type"], "array");
        assert_eq!(schema["properties"]["url"]["items"]["type"], "string");
        assert_eq!(schema["required"], json!(["quality"]));
    }

    #[test]
    fn a_tool_without_arguments_gets_a_closed_empty_schema() {
        let schema = input_schema(&make_tool(Some(action(Vec::new()))));
        assert_eq!(schema["properties"], json!({}));
        assert_eq!(schema["required"], json!([]));
    }

    #[test]
    fn json_arguments_map_to_typed_values() {
        let mut urls = argument("url", ArgKind::Text);
        urls.repeatable = true;
        let tool = make_tool(Some(action(vec![
            argument("path", ArgKind::Path),
            argument("subtitles", ArgKind::Toggle),
            urls,
        ])));

        let values = values_from_json(
            &tool,
            Some(&json!({
                "path": "视频/a,b.mp4",
                "subtitles": true,
                "url": ["a,b.mp4", "c.mp4"],
            })),
        )
        .expect("应能转换");

        let mut expected = TypedValues::new();
        expected.set_text("path", "视频/a,b.mp4");
        expected.set_bool("subtitles", true);
        // 列表不按分隔符拆：带逗号的路径原样保留。
        expected.set_list("url", vec![String::from("a,b.mp4"), String::from("c.mp4")]);
        assert_eq!(values, expected);
    }

    #[test]
    fn a_single_string_for_repeatable_is_one_item() {
        let mut urls = argument("url", ArgKind::Text);
        urls.repeatable = true;
        let tool = make_tool(Some(action(vec![urls])));

        let values = values_from_json(&tool, Some(&json!({"url": "a,b.mp4"}))).expect("应能转换");
        assert_eq!(
            values.get("url"),
            Some(&crate::model::ArgumentValue::List(vec![String::from(
                "a,b.mp4"
            )]))
        );
    }

    #[test]
    fn unknown_keys_wrong_types_and_missing_object_are_rejected() {
        let tool = make_tool(Some(action(vec![argument("path", ArgKind::Path)])));

        assert!(
            values_from_json(&tool, Some(&json!({"nope": "x"})))
                .expect_err("未声明的键")
                .contains("未声明")
        );
        assert!(
            values_from_json(&tool, Some(&json!(["a"])))
                .expect_err("不是对象")
                .contains("必须是对象")
        );
        let mut toggle = argument("subtitles", ArgKind::Toggle);
        toggle.repeatable = false;
        let tool_with_toggle = make_tool(Some(action(vec![toggle])));
        assert!(
            values_from_json(&tool_with_toggle, Some(&json!({"subtitles": "yes"})))
                .expect_err("开关要布尔")
                .contains("boolean")
        );

        let bare = make_tool(None);
        assert!(
            values_from_json(&bare, None).is_ok(),
            "没参数契约 + 没给参数 = 通过"
        );
        assert!(
            values_from_json(&bare, Some(&json!({"x": "1"}))).is_err(),
            "没参数契约却给了参数 = 拒绝"
        );
    }

    #[test]
    fn null_values_are_treated_as_absent() {
        let tool = make_tool(Some(action(vec![argument("path", ArgKind::Path)])));
        let values = values_from_json(&tool, Some(&json!({"path": null}))).expect("null 当缺席");
        assert!(values.is_empty());
    }

    #[test]
    fn numbers_are_accepted_as_strings() {
        let mut number = argument("level", ArgKind::Text);
        number.kind = ArgKind::Text;
        let tool = make_tool(Some(action(vec![number])));
        let values = values_from_json(&tool, Some(&json!({"level": 9}))).expect("数字转字符串");
        assert_eq!(
            values.get("level"),
            Some(&crate::model::ArgumentValue::Text(String::from("9")))
        );
    }

    /// 序列化一遍再解析：schema 本身必须是合法 JSON（golden 冒烟）。
    #[test]
    fn schema_survives_a_roundtrip() {
        let tool = make_tool(Some(action(vec![argument("path", ArgKind::Path)])));
        let text = input_schema(&tool).to_string();
        let parsed: Value = serde_json::from_str(&text).expect("合法 JSON");
        assert_eq!(parsed["properties"]["path"]["type"], "string");
    }
}
