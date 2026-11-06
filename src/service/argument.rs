//! 服务层的参数校验。
//!
//! [`crate::model::Action::build_argv_typed`] 只关心「怎么拼」，对**多出来的键**
//! 保持沉默（TUI 的表单里不存在这种情况）。对外部调用方不行：拼错一个键就静默
//! 丢掉一个参数，命令照跑，结果全错。所以在进入执行器之前先过一遍这里。

use crate::model::{Action, ArgKind, ArgumentValue, TypedValues};

/// 校验一组取值与动作声明是否对得上。
///
/// 拦三类问题：
/// 1. 未声明的键（拼错了）；
/// 2. 类型不对（开关给了文本，或文本给了布尔）；
/// 3. 没声明 `repeatable` 却给了多个值。
///
/// 必填 / 取值内容是否为空由 [`Action::build_argv_typed`] 负责 —— 那里才知道
/// 分隔符与 `repeatable` 的关系，重复校验会分叉。
pub fn validate(action: &Action, values: &TypedValues) -> Result<(), String> {
    for (key, value) in values.iter() {
        let Some(argument) = action.arguments.iter().find(|argument| argument.key == key) else {
            return Err(format!("未声明的参数「{key}」"));
        };

        match (argument.kind, value) {
            (ArgKind::Toggle, ArgumentValue::Bool(_)) => {}
            (ArgKind::Toggle, _) => {
                return Err(format!("「{}」是开关，只接受 true / false", argument.label));
            }
            (_, ArgumentValue::Bool(_)) => {
                return Err(format!("「{}」需要文本值，不是开关", argument.label));
            }
            (_, ArgumentValue::List(items)) if !argument.repeatable && items.len() > 1 => {
                return Err(format!(
                    "「{}」只接受一个值（多值要在 manifest 里声明 repeatable = true）",
                    argument.label
                ));
            }
            _ => {}
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::validate;
    use crate::model::{Action, ArgKind, ArgPlacement, Argument, ArgumentValue, TypedValues};

    fn argument(key: &str, kind: ArgKind, repeatable: bool) -> Argument {
        Argument {
            key: key.to_string(),
            label: format!("{key} 的标签"),
            kind,
            default: None,
            choices: Vec::new(),
            required: false,
            flag: Some(format!("--{key}")),
            flag_join: false,
            placement: ArgPlacement::Middle,
            sensitive: false,
            repeatable,
            repeat_flag: false,
            separator: String::from(","),
            source: None,
            dir_only: false,
            help: None,
        }
    }

    fn action() -> Action {
        Action {
            program: String::from("demo"),
            base_argv: Vec::new(),
            arguments: vec![
                argument("verbose", ArgKind::Toggle, false),
                argument("name", ArgKind::Text, false),
                argument("files", ArgKind::Path, true),
            ],
            duration_from: None,
            limit_from: None,
            foreach: None,
            ok_exit_codes: vec![0],
            allow_empty: false,
        }
    }

    #[test]
    fn a_well_typed_input_passes() {
        let mut values = TypedValues::new();
        values.set_bool("verbose", true);
        values.set_text("name", "demo");
        values.set_list("files", vec![String::from("a"), String::from("b")]);
        assert_eq!(validate(&action(), &values), Ok(()));
    }

    #[test]
    fn unknown_keys_are_rejected_instead_of_silently_dropped() {
        let mut values = TypedValues::new();
        values.set_text("nmae", "typo");
        assert_eq!(
            validate(&action(), &values),
            Err(String::from("未声明的参数「nmae」"))
        );
    }

    #[test]
    fn a_toggle_only_accepts_a_boolean() {
        let mut values = TypedValues::new();
        values.set_text("verbose", "true");
        assert!(validate(&action(), &values).is_err());

        let mut wrong = TypedValues::new();
        wrong.set_bool("name", true);
        assert!(validate(&action(), &wrong).is_err());
    }

    #[test]
    fn several_values_need_a_repeatable_declaration() {
        let mut values = TypedValues::new();
        values.set(
            "name",
            ArgumentValue::List(vec![String::from("a"), String::from("b")]),
        );
        assert!(validate(&action(), &values).is_err());

        // 一个值的列表是允许的：调用方不必关心 manifest 有没有写 repeatable。
        let mut single = TypedValues::new();
        single.set_list("name", vec![String::from("a")]);
        assert_eq!(validate(&action(), &single), Ok(()));
    }
}
