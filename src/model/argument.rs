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
    /// **动态候选**：取值来自命令输出，而不是 manifest 里写死的列表。
    ///
    /// 这是从 navi 的 cheatsheet 学来的一条：让用户记住「有哪些编码器 / 哪些分支 /
    /// 哪些容器」是没道理的，机器自己知道。`source` 说明候选从哪来
    /// （见 `crate::dynamic`），填法与 `Text` 相同，只是表单里会列出候选。
    Dynamic,
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
    /// 多值：这一项可以填多条（用 `separator` 分隔）。
    ///
    /// 带 flag 时有**两种真实存在的写法**，所以由 [`Argument::repeat_flag`] 显式选：
    /// 位置参数每条各占一个元素；带 flag 时默认「flag 一次、值平铺」。
    pub repeatable: bool,
    /// 带 flag 的多值怎么展开：
    ///
    /// * `false`（默认）：**flag 只出现一次，值平铺在后面** —— `-S a b c`
    ///   （`paru -S`、`pacman -Rns`、`apt install` 都是这个形状）。
    /// * `true`：**每个值配一个 flag** —— `-i a -i b`（ffmpeg 那种）。
    ///
    /// 两种写法真的都存在，猜错代价是命令跑不起来，所以做成显式选择；
    /// 选错了在表单的命令预览里一眼就能看出来。
    pub repeat_flag: bool,
    /// 多值之间的分隔符，`repeatable` 为真时才有意义。默认逗号。
    pub separator: String,
    /// `Dynamic` 的候选从哪来。两种写法：
    ///
    /// * 内置名字：`git-branches` / `docker-containers` / `ffmpeg-video-codecs` …
    /// * `command:<命令行>`，或者任何不像名字的字符串 —— 会被当成一条命令跑。
    ///
    /// **不经过 shell**：命令串只在空白与引号处切开，然后作为 argv 交给进程。
    /// 所以管道、重定向、`$VAR` 展开都不会发生 —— 这是刻意的，
    /// 见 `crate::dynamic::split_command`。
    pub source: Option<String>,
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
    /// 导出全部取值（键有序，便于落盘与比较）。
    pub fn pairs(&self) -> Vec<(String, String)> {
        self.entries
            .iter()
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect()
    }

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

/// 类型化参数取值。
///
/// 表单（[`ArgumentValues`]）把所有东西都存成字符串，多值靠分隔符拼起来 ——
/// 这对键盘输入够用，但对外部调用方（服务层 / MCP）有两个问题：
///
/// 1. 值里本来就有分隔符时无法表达（路径里带逗号很常见）；
/// 2. 开关和文本分不开，调用方得猜 `"true"` 到底是不是「打开」。
///
/// 所以再引入这一层：**只描述值本身，不描述怎么拼 argv**。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ArgumentValue {
    /// 单值文本。
    Text(String),
    /// 开关。
    Bool(bool),
    /// 多值。每一项就是一个 argv 元素，**不再拆分**。
    List(Vec<String>),
}

/// 一组类型化取值（键有序）。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TypedValues {
    entries: BTreeMap<String, ArgumentValue>,
}

impl TypedValues {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set(&mut self, key: &str, value: ArgumentValue) {
        self.entries.insert(key.to_string(), value);
    }

    pub fn set_text(&mut self, key: &str, value: impl Into<String>) {
        self.set(key, ArgumentValue::Text(value.into()));
    }

    pub fn set_bool(&mut self, key: &str, value: bool) {
        self.set(key, ArgumentValue::Bool(value));
    }

    pub fn set_list(&mut self, key: &str, values: Vec<String>) {
        self.set(key, ArgumentValue::List(values));
    }

    pub fn get(&self, key: &str) -> Option<&ArgumentValue> {
        self.entries.get(key)
    }

    /// 按声明顺序遍历（未知键排在后面 —— 校验会先把它们拦住）。
    pub fn iter(&self) -> impl Iterator<Item = (&str, &ArgumentValue)> {
        self.entries
            .iter()
            .map(|(key, value)| (key.as_str(), value))
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// 把表单取值归一成类型化取值：多值按分隔符拆、开关按 `"true"` 判。
    ///
    /// 这是**兼容层**：TUI 仍然填字符串，但构建 argv 走的是同一条
    /// [`Action::build_argv_typed`]，所以界面预览和外部调用不会各算一套。
    pub fn from_form(values: &ArgumentValues, arguments: &[Argument]) -> Self {
        let mut typed = Self::new();
        for argument in arguments {
            let Some(raw) = values.get(&argument.key) else {
                continue;
            };
            match argument.kind {
                ArgKind::Toggle => typed.set_bool(&argument.key, raw == "true"),
                _ if argument.repeatable => {
                    typed.set_list(&argument.key, argument.split_values(raw));
                }
                _ => typed.set_text(&argument.key, raw),
            }
        }
        typed
    }

    /// 把 `sensitive` 字段的值换成 `***`（其余原样）。
    ///
    /// 服务层的审计、命令摘要、执行历史都从这里取「可以落盘/可以回传」的那一份，
    /// 不再各自写一遍脱敏规则。
    pub fn redacted(&self, arguments: &[Argument]) -> Self {
        let mut out = Self::new();
        for (key, value) in self.entries.iter() {
            let sensitive = arguments
                .iter()
                .any(|argument| argument.key == *key && argument.sensitive);
            out.set(
                key,
                if sensitive {
                    // 连「有几个值」都不留：敏感字段整个替换成一个占位符。
                    ArgumentValue::Text(String::from("***"))
                } else {
                    value.clone()
                },
            );
        }
        out
    }

    /// `sensitive` 字段当前真正用到的值（用于把命令摘要里出现的地方抹掉）。
    ///
    /// 列表要逐项收集：`flag_join` 会把值和 flag 贴成一个 argv 元素，
    /// 只按「整元素相等」比对会漏掉那种形状。
    pub fn secrets(&self, arguments: &[Argument]) -> Vec<String> {
        let mut secrets = Vec::new();
        for (key, value) in &self.entries {
            let sensitive = arguments
                .iter()
                .any(|argument| argument.key == *key && argument.sensitive);
            if !sensitive {
                continue;
            }
            match value {
                ArgumentValue::Text(text) if !text.is_empty() => secrets.push(text.clone()),
                ArgumentValue::List(items) => {
                    secrets.extend(items.iter().filter(|item| !item.is_empty()).cloned())
                }
                _ => {}
            }
        }
        secrets
    }

    /// 审计摘要：键 → 一行文字（敏感字段已是 `***`，长值截断）。
    ///
    /// 未声明的键一律记成 `<unknown>`：宁可丢信息，也不能把来路不明的值写进日志。
    pub fn summary(&self, arguments: &[Argument]) -> BTreeMap<String, String> {
        let redacted = self.redacted(arguments);
        redacted
            .entries
            .iter()
            .map(|(key, value)| {
                let declared = arguments.iter().any(|argument| argument.key == *key);
                let text = if !declared {
                    String::from("<unknown>")
                } else {
                    match value {
                        ArgumentValue::Text(text) => text.clone(),
                        ArgumentValue::Bool(on) => on.to_string(),
                        ArgumentValue::List(items) => items.join(", "),
                    }
                };
                (key.clone(), truncate_utf8(&text, 200))
            })
            .collect()
    }
}

/// 按 UTF-8 字符边界截断（`String::truncate` 遇到多字节字符会 panic）。
fn truncate_utf8(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_string();
    }
    let mut end = max.min(text.len());
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_string()
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
    /// 哪些退出码算「成功」。默认只有 `0`。
    ///
    /// 真跑抓到过：`pacman -Qdt` 在**没有孤儿包**时退出码是 1 ——
    /// 「没匹配到」是正常结果，不该在界面上报成失败。同理 `checkupdates`
    /// 没有更新时退 2、`grep` 没匹配时退 1。
    pub ok_exit_codes: Vec<i32>,
    /// 允许**不带任何参数**运行（`allow_empty = true`）。
    ///
    /// 绝大多数动作都要带点东西（子命令、查询词），所以「默认 argv 是空的」
    /// 通常意味着忘了写 `base_argv`（`pacman -Qdt` 那种本体命令漏了会变成光跑
    /// `pacman`）。真要允许空 argv 就在这里明说，例如 `pac` 不带参数＝列出全部。
    pub allow_empty: bool,
    /// 批量：声明的那个参数**每个取值各跑一次**（`foreach = "input"`）。
    ///
    /// 和 `repeatable` 的区别很重要：`repeatable` 是「一条命令塞多个参数」
    /// （jq 读多个文件就是这样），`foreach` 是「N 条命令、各一个输出」
    /// （转码一个目录就是这样）。其余字段里的 `{name}`/`{stem}`/`{ext}`/`{dir}`
    /// 会按当前那个文件替换，所以输出能写成 `{stem}_small.mp4`。
    pub foreach: Option<String>,
}

/// 一个参数归一化之后的形态。
enum Resolved {
    /// 不参与 argv（空值，或者关闭的开关）。
    Skip,
    /// 开关打开：追加它的 flag。
    Flag,
    /// 单值。
    Value(String),
    /// 多值，每一项各占一个 argv 元素。
    Items(Vec<String>),
}

/// 把类型化取值按参数声明归一成 [`Resolved`]。
///
/// 必填校验放在这里：`Skip` 只对可选项成立，必填项为空一律报错，不替用户猜。
fn resolve(argument: &Argument, values: &TypedValues) -> Result<Resolved, String> {
    if argument.kind == ArgKind::Toggle {
        return match values.get(&argument.key) {
            None | Some(ArgumentValue::Bool(false)) => Ok(Resolved::Skip),
            Some(ArgumentValue::Bool(true)) => Ok(Resolved::Flag),
            Some(_) => Err(format!("「{}」是开关，只接受 true / false", argument.label)),
        };
    }

    let items: Vec<String> = match values.get(&argument.key) {
        None => Vec::new(),
        Some(ArgumentValue::Text(raw)) => {
            let text = raw.trim();
            if text.is_empty() {
                Vec::new()
            } else if argument.repeatable {
                // 文本仍按分隔符理解：TUI 的表单值就是从这条路进来的。
                argument.split_values(text)
            } else {
                vec![text.to_string()]
            }
        }
        Some(ArgumentValue::List(values)) => values
            .iter()
            .map(|item| item.trim())
            .filter(|item| !item.is_empty())
            .map(str::to_string)
            .collect(),
        Some(ArgumentValue::Bool(_)) => {
            return Err(format!("「{}」需要文本值，不是开关", argument.label));
        }
    };

    if items.is_empty() {
        if argument.required {
            return Err(format!("「{}」是必填项", argument.label));
        }
        return Ok(Resolved::Skip);
    }

    if !argument.repeatable {
        let mut iter = items.into_iter();
        let value = iter.next().unwrap_or_default();
        if iter.next().is_some() {
            return Err(format!(
                "「{}」只接受一个值（多值要在 manifest 里声明 repeatable = true）",
                argument.label
            ));
        }
        return Ok(Resolved::Value(value));
    }

    Ok(Resolved::Items(items))
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
        // 表单值先归一成类型化取值，再走同一条构建路径 —— 界面预览、TUI 执行和
        // 服务层 / MCP 调用因此永远得到同一条命令，不会各写一份「差不多」的逻辑。
        self.build_argv_typed(&TypedValues::from_form(values, &self.arguments))
    }

    /// 用类型化取值构造 argv（**不含程序名**）。
    ///
    /// 与 [`Action::build_argv`] 的区别只有输入形态：多值用 [`ArgumentValue::List`]
    /// 直接给，**不经过分隔符字符串**，所以单个值里带逗号（路径常见）也不会被拆开。
    ///
    /// 规则与 [`Action::build_argv`] 完全一致：
    ///
    /// 1. `base_argv` 原样打头；
    /// 2. 再按 [`ArgPlacement`] 分三段（Leading → Middle → Trailing），段内按声明顺序；
    /// 3. 值为空的参数整个跳过；必填项为空直接报错；
    /// 4. 多值带 flag 时按 `flag_join` / `repeat_flag` 展开。
    pub fn build_argv_typed(&self, values: &TypedValues) -> Result<Vec<String>, String> {
        let mut phases: [Vec<String>; 3] = std::array::from_fn(|_| Vec::new());

        for argument in &self.arguments {
            let slot = match argument.placement {
                ArgPlacement::Leading => 0,
                ArgPlacement::Middle => 1,
                ArgPlacement::Trailing => 2,
            };

            match resolve(argument, values)? {
                Resolved::Skip => {}
                Resolved::Flag => {
                    if let Some(flag) = &argument.flag {
                        phases[slot].push(flag.clone());
                    }
                }
                Resolved::Value(value) => match &argument.flag {
                    Some(flag) if argument.flag_join => {
                        // 贴成一个元素，例如 7z 的 `-mx9` / `-o/tmp/dir`
                        phases[slot].push(format!("{flag}{value}"));
                    }
                    Some(flag) => {
                        phases[slot].push(flag.clone());
                        phases[slot].push(value);
                    }
                    None => phases[slot].push(value),
                },
                Resolved::Items(items) => match &argument.flag {
                    Some(flag) if argument.flag_join => {
                        for item in items {
                            phases[slot].push(format!("{flag}{item}"));
                        }
                    }
                    Some(flag) if argument.repeat_flag => {
                        for item in items {
                            phases[slot].push(flag.clone());
                            phases[slot].push(item);
                        }
                    }
                    Some(flag) => {
                        phases[slot].push(flag.clone());
                        phases[slot].extend(items);
                    }
                    None => phases[slot].extend(items),
                },
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
        let mut secrets: Vec<String> = Vec::new();
        for argument in self.arguments.iter().filter(|item| item.sensitive) {
            let Some(raw) = values.get(&argument.key) else {
                continue;
            };
            if argument.repeatable {
                secrets.extend(argument.split_values(raw));
            } else if !raw.is_empty() {
                secrets.push(raw.to_string());
            }
        }

        if secrets.is_empty() {
            return argv.to_vec();
        }
        argv.iter()
            .map(|item| {
                let mut masked = item.clone();
                for secret in &secrets {
                    // 整元素相等时一个字符也照抹；只在别的元素里做「包含」替换时
                    // 才要求至少两个字符，免得密码是 "a" 时把 "--all" 搅成 "--***ll"。
                    let applies = item == secret || (secret.len() >= 2 && item.contains(secret));
                    if applies {
                        masked = masked.replace(secret, "***");
                    }
                }
                masked
            })
            .collect()
    }

    /// 把表单取值里 `sensitive` 字段的值换成 `***`（P2-08 / 审计 S-01）。
    ///
    /// 历史文件除了 argv 还存一份「当时的表单取值」用来回填。以前这份是**原样**
    /// 落盘的，`sensitive = true` 的密码 / token 会明文留在磁盘上。这里补上同一套
    /// 规则：执行用真值，落盘用脱敏后的值。
    pub fn redacted_values(&self, values: &ArgumentValues) -> ArgumentValues {
        let mut out = ArgumentValues::new();
        for (key, value) in values.pairs() {
            let sensitive = self
                .arguments
                .iter()
                .any(|argument| argument.key == key && argument.sensitive);
            out.set(
                &key,
                if sensitive && !value.is_empty() {
                    "***".to_string()
                } else {
                    value
                },
            );
        }
        out
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
    use super::{Action, ArgKind, ArgPlacement, Argument, ArgumentValues, Choice, TypedValues};

    /// 一个模仿 yt-dlp 的动作：`base` + 带 flag 的选项 + 末尾的位置参数。
    fn action() -> Action {
        Action {
            program: "yt-dlp".to_string(),
            base_argv: vec!["--no-mtime".to_string()],
            duration_from: None,
            limit_from: None,
            foreach: None,
            ok_exit_codes: vec![0],
            allow_empty: false,
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
                    repeat_flag: false,
                    source: None,
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
                    repeat_flag: false,
                    source: None,
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
                    repeat_flag: false,
                    source: None,
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

    /// 多值字段带 flag 时，每个值都要有自己的 flag（`-i a -i b`）。
    ///
    /// 这个分支以前直接把 flag 丢了 —— 因为那时「多值 + flag」是禁止的，
    /// 直到 `foreach` 放开了它（批量转码的 `-i`）才暴露出来。
    /// 默认写法：**flag 只出现一次，值平铺后面** —— `paru -S a b c` 的形状。
    #[test]
    fn a_repeatable_flagged_argument_defaults_to_one_flag() {
        let mut action = action();
        action.arguments[1].repeatable = true; // quality：带 -f
        action.arguments[1].separator = String::from("|");
        let mut values = action.default_values();
        values.set("url", "a.mp4");
        values.set("quality", "best|worst");
        values.set("subtitles", "false");

        assert_eq!(
            action.build_argv(&values).expect("应能构建"),
            vec!["--no-mtime", "-f", "best", "worst", "a.mp4"],
            "flag 一次、值平铺"
        );
    }

    #[test]
    fn a_repeatable_flagged_argument_repeats_its_flag() {
        let mut action = action();
        action.arguments[1].repeatable = true; // quality：带 -f
        action.arguments[1].repeat_flag = true; // 每个值配一个 flag
        action.arguments[1].separator = String::from("|");
        let mut values = action.default_values();
        values.set("url", "a.mp4");
        values.set("quality", "best|worst");
        values.set("subtitles", "false");

        assert_eq!(
            action.build_argv(&values).expect("应能构建"),
            vec!["--no-mtime", "-f", "best", "-f", "worst", "a.mp4"]
        );
    }

    /// 表单取值与类型化取值必须生成**逐元素相同**的 argv。
    ///
    /// 这条是兼容层的契约：TUI 的预览、TUI 的执行、服务层 / MCP 的调用都依赖
    /// 「同一组参数只有一种命令」。
    #[test]
    fn typed_and_form_values_build_the_same_argv() {
        let mut action = action();
        action.arguments[0].repeatable = true; // 链接改成多值
        let mut values = action.default_values();
        values.set("url", "a.jpg, b.jpg");
        values.set("quality", "best");
        values.set("subtitles", "true");

        let typed = TypedValues::from_form(&values, &action.arguments);
        assert_eq!(
            action.build_argv(&values),
            action.build_argv_typed(&typed),
            "两条路只能差在输入形态，不能差在结果"
        );
        assert_eq!(
            action.build_argv(&values).expect("应能构建"),
            vec!["--no-mtime", "-f", "best", "--write-subs", "a.jpg", "b.jpg"]
        );
    }

    /// 类型化列表**不再拆分**：值里带逗号（路径常见）也原样保留。
    #[test]
    fn a_typed_list_keeps_separators_inside_a_value() {
        let mut action = action();
        action.arguments[0].repeatable = true;

        let mut typed = TypedValues::new();
        typed.set_list("url", vec![String::from("a,b.jpg"), String::from("c.jpg")]);
        typed.set_text("quality", "");
        typed.set_bool("subtitles", false);

        assert_eq!(
            action.build_argv_typed(&typed).expect("应能构建"),
            vec!["--no-mtime", "a,b.jpg", "c.jpg"],
            "列表的每一项就是一个 argv 元素"
        );
    }

    #[test]
    fn a_non_repeatable_field_rejects_several_typed_values() {
        let mut typed = TypedValues::new();
        typed.set_list("url", vec![String::from("a.jpg"), String::from("b.jpg")]);

        assert!(
            action()
                .build_argv_typed(&typed)
                .expect_err("没声明 repeatable 就不该接受多个值")
                .contains("只接受一个值")
        );
    }

    #[test]
    fn a_switch_rejects_a_text_value() {
        let mut typed = TypedValues::new();
        typed.set_text("subtitles", "yes");
        typed.set_text("url", "u");

        assert!(
            action()
                .build_argv_typed(&typed)
                .expect_err("开关只认布尔")
                .contains("只接受 true / false")
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
            foreach: None,
            ok_exit_codes: vec![0],
            allow_empty: true, // 「不带参数也合法」的声明位
        };
        assert_eq!(action.build_argv(&ArgumentValues::new()), Ok(Vec::new()));
    }
}
