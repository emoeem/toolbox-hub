//! 动作参数模型：把「一个命令 + 一堆参数」变成可填写的表单和可执行的 argv。
//!
//! 这是整个工具箱的核心抽象。目标是让用户**不需要知道命令怎么写**：
//!
//! ```text
//! 表单字段            argv
//! ─────────────       ────────────────────────────────
//! 链接  [https://…]    yt-dlp -f bestvideo+bestaudio/best
//! 画质  [1080p ▼]            -P /home/emo/Downloads
//! 字幕  [✓]                  --write-subs https://…
//! 目录  [/home/emo/Downloads]
//! ```
//!
//! # 为什么坚持 argv 而不是拼 shell 字符串
//!
//! 用户填的值会原样作为**单个 argv 元素**传给程序（[`std::process::Command`]），
//! 中间没有 shell。所以 `; rm -rf /`、`$(whoami)`、反引号、空格、中文、引号
//! 都只是普通字符，不存在注入面。见 `values_with_shell_metacharacters_stay_single_elements`。

use std::collections::BTreeMap;

/// 参数的填写方式。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ArgKind {
    /// 单行文本。
    Text,
    /// 文件 / 目录路径。表单里只是提示，不做强制校验。
    Path,
    /// 从固定选项里选一个（`choices` 非空）。
    Choice,
    /// 开关：打开时追加 `flag`，关闭时什么都不加。
    Toggle,
}

/// `Choice` 的一个选项：**显示给用户的文字**和**真正进 argv 的值**是两回事。
///
/// 例如画质选「1080p」，实际传给 yt-dlp 的是
/// `bestvideo[height<=1080]+bestaudio/best[height<=1080]` —— 这正是表单的价值。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Choice {
    /// 表单里显示的文字，例如 `1080p`。
    pub label: String,
    /// 实际进 argv 的值。
    pub value: String,
}

impl Choice {
    pub fn new(label: &str, value: &str) -> Self {
        Self {
            label: label.to_string(),
            value: value.to_string(),
        }
    }
}

/// 参数在 argv 里的位置。
///
/// 大多数工具是「开关在前、操作对象在后」，但**不是所有**：实测 ImageMagick 要求
/// 输入文件排在操作之前（`magick in.png -resize 50% out.webp`），
/// 所以位置必须能显式指定，而不是让构建器替所有工具猜一套顺序。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ArgPlacement {
    /// 排在最前面（输入文件常在这里）。
    Leading,
    /// 中间区：带 flag 的参数默认落在这里。
    #[default]
    Middle,
    /// 排在最后：位置参数默认落在这里（操作对象常在这里）。
    Trailing,
}

/// 一个可填写的参数。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Argument {
    /// 模板里引用它的名字，也是取值表的键。
    pub key: String,
    /// 表单里显示的中文标签。
    pub label: String,
    pub kind: ArgKind,
    /// 缺省值。`Toggle` 用 `"true"` / `"false"`；`Choice` 用选项的 `value`。
    pub default: Option<String>,
    /// `Choice` 的可选项。
    pub choices: Vec<Choice>,
    /// 必填：为空时不允许执行（[`Action::build_argv`] 会报错，不猜）。
    pub required: bool,
    /// 怎么进 argv：`Some("--quality")` → `--quality <值>`；
    /// `None` → 位置参数（放在所有带 flag 的参数之后）。
    pub flag: Option<String>,
    /// 开关和值是否**贴成一个 argv 元素**。
    ///
    /// 大多数 GNU 风格工具用分开写（`--to html`），但有些工具只认贴一起的
    /// （实测 7z：`-mx9` 可以，`-mx 9` 不行）。两种都支持，由 manifest 决定。
    pub flag_join: bool,
    /// 在 argv 里排哪一段（见 [`ArgPlacement`]）。
    pub placement: ArgPlacement,
    /// 敏感参数（密码、带 token 的地址…）：**记录历史前**它的值会被换成 `***`。
    pub sensitive: bool,
    /// 多值：这一项可以填多条，**每条各占一个 argv 元素**。
    ///
    /// 只对位置参数开放（带 flag 的多值有两种写法 —— `-i a -i b` 与 `-i a b` ——
    /// 在真的遇到需要它的工具之前，不先造一个没人验证过的开关）。
    pub repeatable: bool,
    /// 多值之间的分隔符，`repeatable` 为真时才有意义。默认逗号。
    pub separator: String,
    /// 这个字段要填的是**目录**（例如 aria2c 的保存目录）：
    /// 选择器里会提示用 `Ctrl-D` 挑当前目录。
    pub dir_only: bool,
    /// 表单里的一句话说明。
    pub help: Option<String>,
}

impl Argument {
    /// `Choice` 字段：把当前值翻译成展示文字（找不到就原样返回）。
    pub fn choice_label(&self, value: &str) -> String {
        self.choices
            .iter()
            .find(|choice| choice.value == value)
            .map(|choice| choice.label.clone())
            .unwrap_or_else(|| value.to_string())
    }

    /// `Choice` 字段：循环取值。空选项表返回 `None`。
    pub fn cycle_choice(&self, current: &str, delta: isize) -> Option<String> {
        if self.choices.is_empty() {
            return None;
        }
        let index = self
            .choices
            .iter()
            .position(|choice| choice.value == current)
            .unwrap_or(0) as isize;
        let len = self.choices.len() as isize;
        let next = (index + delta).rem_euclid(len) as usize;
        Some(self.choices[next].value.clone())
    }

    /// 把一个多值字段的取值拆成若干项。
    ///
    /// 每项去掉首尾空白、丢掉空项 —— 所以 `a, b, , c` 就是三项。
    pub fn split_values(&self, raw: &str) -> Vec<String> {
        raw.split(&self.separator)
            .map(str::trim)
            .filter(|item| !item.is_empty())
            .map(str::to_string)
            .collect()
    }
}

/// 表单里填好的一组取值。`key → 值`，值一律是字符串。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ArgumentValues {
    entries: BTreeMap<String, String>,
}

impl ArgumentValues {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set(&mut self, key: &str, value: impl Into<String>) {
        self.entries.insert(key.to_string(), value.into());
    }

    pub fn get(&self, key: &str) -> Option<&str> {
        self.entries.get(key).map(String::as_str)
    }

    /// `Toggle` 字段是否打开。
    pub fn is_on(&self, key: &str) -> bool {
        self.get(key) == Some("true")
    }
}

/// 一个动作：要执行的程序 + 无参数部分 + 表单字段。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Action {
    /// 要执行的程序（命令名或绝对路径）。Provider 会尽量解析成绝对路径。
    pub program: String,
    /// 无条件追加的参数，例如 `["--extract-audio"]`。
    pub base_argv: Vec<String>,
    /// 表单字段，同时也决定 argv 顺序。
    pub arguments: Vec<Argument>,
    /// 进度条用的「总时长」从哪个参数取：声明了就去探那个文件的时长
    /// （`duration_from = "input"`）。不声明就没有百分比，只有已跑时间。
    pub duration_from: Option<String>,
    /// 「这次只会做 N 秒」从哪个参数取（`limit_from = "duration"`）：
    /// 裁剪类动作的输出时长不等于输入时长，有它就按它算百分比。
    pub limit_from: Option<String>,
}

impl Action {
    /// 构造 argv（**不含程序名**）。
    ///
    /// 规则（刻意做得可预测，这样预览和执行永远是同一条命令）：
    ///
    /// 1. `base_argv` 原样打头；
    /// 2. 然后按 [`ArgPlacement`] 分三段：先 `Leading`，再 `Middle`（带 flag 的默认
    ///    在这里），最后 `Trailing`（位置参数默认在这里）；每段内部按声明顺序；
    /// 3. 值为空的参数整个跳过，所以不会留下「有 flag 没值」的半截参数；
    /// 4. 必填项为空直接报错，不替用户猜。
    ///
    /// 全程只做字符串拼接，**不经过 shell**。
    pub fn build_argv(&self, values: &ArgumentValues) -> Result<Vec<String>, String> {
        let mut phases: [Vec<String>; 3] = std::array::from_fn(|_| Vec::new());

        for argument in &self.arguments {
            let slot = match argument.placement {
                ArgPlacement::Leading => 0,
                ArgPlacement::Middle => 1,
                ArgPlacement::Trailing => 2,
            };
            let value = values.get(&argument.key).unwrap_or("").trim().to_string();

            if argument.required && argument.kind != ArgKind::Toggle && value.is_empty() {
                return Err(format!("「{}」是必填项", argument.label));
            }

            if argument.kind == ArgKind::Toggle {
                if values.is_on(&argument.key)
                    && let Some(flag) = &argument.flag
                {
                    phases[slot].push(flag.clone());
                }
                continue;
            }

            if value.is_empty() {
                continue;
            }

            // 多值：拆成多个 argv 元素，各占一个位置。
            if argument.repeatable {
                let items = argument.split_values(&value);
                if argument.required && items.is_empty() {
                    return Err(format!("「{}」是必填项", argument.label));
                }
                phases[slot].extend(items);
                continue;
            }

            match &argument.flag {
                Some(flag) => {
                    if argument.flag_join {
                        // 贴成一个元素，例如 7z 的 `-mx9` / `-o/tmp/dir`
                        phases[slot].push(format!("{flag}{value}"));
                    } else {
                        phases[slot].push(flag.clone());
                        phases[slot].push(value);
                    }
                }
                None => phases[slot].push(value),
            }
        }

        let mut argv = self.base_argv.clone();
        for phase in phases {
            argv.extend(phase);
        }
        Ok(argv)
    }

    /// 把 argv 里敏感参数的值换成 `***`（记历史用）。
    ///
    /// 按**值**精确匹配：先取所有 sensitive 参数当前的取值，argv 里等于这些值的元素
    /// 一律替换。这样不依赖参数在 argv 的位置，也不会误伤别的参数。
    pub fn redacted(&self, values: &ArgumentValues, argv: &[String]) -> Vec<String> {
        let secrets: Vec<&str> = self
            .arguments
            .iter()
            .filter(|argument| argument.sensitive)
            .filter_map(|argument| values.get(&argument.key))
            .filter(|value| !value.is_empty())
            .collect();

        if secrets.is_empty() {
            return argv.to_vec();
        }
        argv.iter()
            .map(|item| {
                if secrets.contains(&item.as_str()) {
                    String::from("***")
                } else {
                    item.clone()
                }
            })
            .collect()
    }

    /// 按声明的缺省值初始化一份取值（打开表单时的初值）。
    pub fn default_values(&self) -> ArgumentValues {
        let mut values = ArgumentValues::new();
        for argument in &self.arguments {
            let default = argument
                .default
                .clone()
                .unwrap_or_else(|| match argument.kind {
                    ArgKind::Toggle => "false".to_string(),
                    ArgKind::Choice => argument
                        .choices
                        .first()
                        .map(|choice| choice.value.clone())
                        .unwrap_or_default(),
                    _ => String::new(),
                });
            values.set(&argument.key, default);
        }
        values
    }
}

#[cfg(test)]
mod tests {
    use super::{Action, ArgKind, ArgPlacement, Argument, ArgumentValues, Choice};

    /// 一个模仿 yt-dlp 的动作：`base` + 带 flag 的选项 + 末尾的位置参数。
    fn action() -> Action {
        Action {
            program: "yt-dlp".to_string(),
            base_argv: vec!["--no-mtime".to_string()],
            duration_from: None,
            limit_from: None,
            arguments: vec![
                Argument {
                    key: "url".to_string(),
                    label: "链接".to_string(),
                    kind: ArgKind::Text,
                    default: None,
                    choices: Vec::new(),
                    required: true,
                    flag: None,
                    flag_join: false,
                    repeatable: false,
                    separator: String::from(","),
                    dir_only: false,
                    placement: ArgPlacement::Trailing,
                    sensitive: false,
                    help: None,
                },
                Argument {
                    key: "quality".to_string(),
                    label: "画质".to_string(),
                    kind: ArgKind::Choice,
                    default: None,
                    choices: vec![
                        Choice::new("最好", "best"),
                        Choice::new("1080p", "bestvideo[height<=1080]+bestaudio/best"),
                    ],
                    required: false,
                    flag: Some("-f".to_string()),
                    flag_join: false,
                    repeatable: false,
                    separator: String::from(","),
                    dir_only: false,
                    placement: ArgPlacement::Middle,
                    sensitive: false,
                    help: None,
                },
                Argument {
                    key: "subtitles".to_string(),
                    label: "字幕".to_string(),
                    kind: ArgKind::Toggle,
                    default: None,
                    choices: Vec::new(),
                    required: false,
                    flag: Some("--write-subs".to_string()),
                    flag_join: false,
                    repeatable: false,
                    separator: String::from(","),
                    dir_only: false,
                    placement: ArgPlacement::Middle,
                    sensitive: false,
                    help: None,
                },
            ],
        }
    }

    #[test]
    fn flags_come_first_and_positional_values_last() {
        let mut values = action().default_values();
        values.set("url", "https://example.com/v");
        values.set("quality", "best");

        let argv = action().build_argv(&values).expect("应能构建");
        assert_eq!(
            argv,
            vec!["--no-mtime", "-f", "best", "https://example.com/v"],
            "声明顺序是「链接、画质、字幕」，但链接作为位置参数要落在最后"
        );
    }

    #[test]
    fn empty_optional_values_leave_no_dangling_flag() {
        let mut values = action().default_values();
        values.set("url", "https://example.com/v");
        values.set("quality", ""); // 用户清空了画质

        let argv = action().build_argv(&values).expect("应能构建");
        assert_eq!(argv, vec!["--no-mtime", "https://example.com/v"]);
        assert!(
            !argv.contains(&"-f".to_string()),
            "不能留下有 flag 没值的半截参数"
        );
    }

    #[test]
    fn toggles_append_only_the_flag() {
        let mut values = action().default_values();
        values.set("url", "u");
        values.set("quality", ""); // 隔离开关行为，不让画质参与
        values.set("subtitles", "true");
        let argv = action().build_argv(&values).expect("应能构建");
        assert_eq!(argv, vec!["--no-mtime", "--write-subs", "u"]);

        values.set("subtitles", "false");
        let argv = action().build_argv(&values).expect("应能构建");
        assert_eq!(argv, vec!["--no-mtime", "u"], "关闭时什么都不加");
    }

    #[test]
    fn missing_required_argument_is_an_error_not_a_guess() {
        let mut values = action().default_values();
        values.set("url", "   "); // 只有空白也算没填

        assert_eq!(
            action().build_argv(&values),
            Err("「链接」是必填项".to_string()),
            "必填项为空时给出一句能直接显示给用户的话"
        );
    }

    #[test]
    fn choice_labels_and_values_are_translated_both_ways() {
        let action = action();
        let quality = &action.arguments[1];

        assert_eq!(quality.choice_label("best"), "最好");
        assert_eq!(
            quality.choice_label("bestvideo[height<=1080]+bestaudio/best"),
            "1080p"
        );
        // 认不出来的值原样显示，不丢信息。
        assert_eq!(quality.choice_label("自定义"), "自定义");

        // 循环取值：从「最好」往后一格是 1080p，再往后绕回最好。
        assert_eq!(
            quality.cycle_choice("best", 1).as_deref(),
            Some("bestvideo[height<=1080]+bestaudio/best")
        );
        assert_eq!(
            quality.cycle_choice("best", -1).as_deref(),
            Some("bestvideo[height<=1080]+bestaudio/best")
        );
        assert_eq!(quality.cycle_choice("best", 2).as_deref(), Some("best"));
    }

    #[test]
    fn default_values_follow_declared_defaults_and_kind() {
        let values = action().default_values();
        assert_eq!(values.get("url"), Some(""));
        // Choice 没写 default → 取第一个选项的值
        assert_eq!(values.get("quality"), Some("best"));
        // Toggle 没写 default → 关闭
        assert!(!values.is_on("subtitles"));

        let mut explicit = action();
        explicit.arguments[2].default = Some("true".to_string());
        assert!(explicit.default_values().is_on("subtitles"));
    }

    /// 安全核心：用户填的东西原样成为**单个 argv 元素**，不经 shell，不存在注入面。
    #[test]
    fn values_with_shell_metacharacters_stay_single_elements() {
        let action = action();
        let nasty_values = [
            "两个 空格.mp4",
            "中文 文件名 带空格.mp4",
            "it's \"quoted\".mp4",
            "x; rm -rf /",
            "x | cat /etc/passwd",
            "x$(whoami)",
            "`id`",
            "x && y",
            "x > /tmp/out",
            "$HOME/$(id)",
        ];

        for nasty in nasty_values {
            let mut values = action.default_values();
            values.set("url", nasty);
            let argv = action.build_argv(&values).expect("应能构建");

            assert_eq!(
                argv.iter().filter(|item| *item == nasty).count(),
                1,
                "{nasty:?} 必须原样、且只出现一次: {argv:?}"
            );
            assert_eq!(
                argv.last().map(String::as_str),
                Some(nasty),
                "位置参数应完整保留在末尾: {argv:?}"
            );
        }
    }

    /// 实测出来的需求：ImageMagick 要求输入文件排在操作之前，
    /// 所以位置参数不能一律丢到最后。
    #[test]
    fn leading_placement_puts_a_value_before_the_options() {
        let mut action = action();
        action.arguments[0].placement = ArgPlacement::Leading; // 链接那一项挪到最前
        let mut values = action.default_values();
        values.set("url", "in.png");
        values.set("quality", "50%");

        assert_eq!(
            action.build_argv(&values).expect("应能构建"),
            vec!["--no-mtime", "in.png", "-f", "50%"],
            "Leading 的值要排在开关之前"
        );
    }

    /// 标了 sensitive 的参数值不会进历史。
    #[test]
    fn sensitive_values_are_redacted_from_the_recorded_command() {
        let mut action = action();
        action.arguments[1].key = "token".to_string();
        action.arguments[1].sensitive = true;

        let mut values = action.default_values();
        values.set("url", "https://example.com/v");
        values.set("token", "s3cr3t-value");

        let argv = action.build_argv(&values).expect("应能构建");
        assert!(
            argv.contains(&"s3cr3t-value".to_string()),
            "执行时当然要用真值"
        );

        let redacted = action.redacted(&values, &argv);
        assert!(
            !redacted.contains(&"s3cr3t-value".to_string()),
            "{redacted:?}"
        );
        assert!(redacted.contains(&"***".to_string()), "{redacted:?}");
        assert!(
            redacted.contains(&"https://example.com/v".to_string()),
            "非敏感参数照原样留着: {redacted:?}"
        );
    }

    /// 多值：一个字段拆成多个 argv 元素。
    #[test]
    fn a_repeatable_argument_becomes_several_argv_elements() {
        let mut action = action();
        action.arguments[0].repeatable = true; // 「链接」那一项改成多值
        let mut values = action.default_values();
        values.set("quality", "");
        values.set("subtitles", "false");
        values.set("url", "a.jpg, b.jpg ,,c.jpg");

        assert_eq!(
            action.build_argv(&values).expect("应能构建"),
            vec!["--no-mtime", "a.jpg", "b.jpg", "c.jpg"],
            "去掉首尾空白、丢掉空项"
        );
    }

    /// 路径里真的有逗号时，可以换个分隔符。
    #[test]
    fn a_repeatable_argument_can_use_another_separator() {
        let mut action = action();
        action.arguments[0].repeatable = true;
        action.arguments[0].separator = String::from("|");
        let mut values = action.default_values();
        values.set("quality", "");
        values.set("subtitles", "false");
        values.set("url", "a,b.jpg|c,d.jpg");

        assert_eq!(
            action.build_argv(&values).expect("应能构建"),
            vec!["--no-mtime", "a,b.jpg", "c,d.jpg"]
        );
        assert_eq!(action.arguments[0].split_values("a|b"), vec!["a", "b"]);
    }

    /// 只填了分隔符等于没填：必填项要拦住。
    #[test]
    fn a_repeatable_required_argument_rejects_separator_only_input() {
        let mut action = action();
        action.arguments[0].repeatable = true;
        action.arguments[0].required = true;
        let mut values = action.default_values();
        values.set("url", " , , ");

        assert!(
            action.build_argv(&values).is_err(),
            "只有分隔符不算填了内容"
        );
    }

    #[test]
    fn empty_action_yields_empty_argv() {
        let action = Action {
            program: "true".to_string(),
            base_argv: Vec::new(),
            arguments: Vec::new(),
            duration_from: None,
            limit_from: None,
        };
        assert_eq!(action.build_argv(&ArgumentValues::new()), Ok(Vec::new()));
    }
}
