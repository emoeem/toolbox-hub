//! Manifest Provider：从 TOML 里读「动作 + 参数」，把已装的 CLI 包装成带表单的工具。
//!
//! 两个来源：
//!
//! 1. **随程序发布的默认动作**（`manifests/*.toml`，用 `include_str!` 编译进二进制）——
//!    装好就有，跟着代码一起更新；
//! 2. **用户自己的**（`~/.config/toolbox-hub/tools.d/*.toml`，或被
//!    `TOOLBOX_HUB_MANIFEST_PATH` 覆盖）——**同 id 以用户那份为准**。
//!
//! 加一个动作 = 写一个 TOML 文件，不用改 Rust 代码、不用重编译。
//! 写法见 `manifests/yt-dlp.toml` 顶部的字段说明。
//!
//! 写错了不会炸：解析/校验失败只记一条 [`Discovery::warnings`]，其它动作照常出现。

use std::{
    collections::BTreeSet,
    fs, io,
    path::{Path, PathBuf},
};

use serde::Deserialize;

use crate::{
    model::{
        Action, ArgKind, ArgPlacement, Argument, Choice, Danger, Domain, RunMode, ToolDefinition,
    },
    providers::{Discovery, Provider, metadata},
};

/// 覆盖用户 manifest 目录的环境变量（冒号分隔，可以给多个目录）。
pub const DIRS_ENV: &str = "TOOLBOX_HUB_MANIFEST_PATH";

/// 用户放 manifest 的地方（相对 `$HOME`）。
pub const USER_DIR: &str = ".config/toolbox-hub/tools.d";

const PROVIDER_ID: &str = "manifest";
const PROVIDER_LABEL: &str = "Manifest";

/// 随程序发布的默认动作。加内置动作 = 往 `manifests/` 放一个 TOML 并在这里挂上。
const BUNDLED: &[(&str, &str)] = &[
    ("yt-dlp.toml", include_str!("../../manifests/yt-dlp.toml")),
    (
        "imagemagick.toml",
        include_str!("../../manifests/imagemagick.toml"),
    ),
    (
        "exiftool.toml",
        include_str!("../../manifests/exiftool.toml"),
    ),
    ("pandoc.toml", include_str!("../../manifests/pandoc.toml")),
    ("jq.toml", include_str!("../../manifests/jq.toml")),
    ("7zip.toml", include_str!("../../manifests/7zip.toml")),
    ("aria2c.toml", include_str!("../../manifests/aria2c.toml")),
    ("ffmpeg.toml", include_str!("../../manifests/ffmpeg.toml")),
    (
        "packages.toml",
        include_str!("../../manifests/packages.toml"),
    ),
];

/// 内置动作总数。
///
/// **加/删 `manifests/*.toml` 里的动作后要更新这里**：测试拿它对账，
/// 某个 manifest 悄悄解析失败时（例如拼错一个键），工具数会立刻对不上。
#[cfg(test)]
const BUNDLED_ACTION_COUNT: usize = 32;

pub struct ManifestProvider {
    /// 用户 manifest 目录，按顺序读；先读到的占住 id。
    dirs: Vec<PathBuf>,
}

impl ManifestProvider {
    pub fn new(dirs: Vec<PathBuf>) -> Self {
        Self { dirs }
    }

    /// 默认目录：`~/.config/toolbox-hub/tools.d`，或被 [`DIRS_ENV`] 覆盖。
    pub fn with_defaults() -> Self {
        if let Some(raw) = std::env::var_os(DIRS_ENV) {
            let dirs = std::env::split_paths(&raw).collect::<Vec<_>>();
            if !dirs.is_empty() {
                return Self::new(dirs);
            }
        }
        Self::new(
            std::env::var_os("HOME")
                .map(|home| vec![PathBuf::from(home).join(USER_DIR)])
                .unwrap_or_default(),
        )
    }

    fn load_dir(&self, dir: &Path, discovery: &mut Discovery, claimed: &mut BTreeSet<String>) {
        let Ok(read_dir) = fs::read_dir(dir) else {
            // 用户还没建这个目录是常态，不是问题。
            return;
        };
        let mut entries = match read_dir.collect::<Result<Vec<_>, _>>() {
            Ok(entries) => entries,
            Err(error) => {
                discovery
                    .warnings
                    .push(format!("{}: {error}", dir.display()));
                return;
            }
        };
        entries.sort_by_key(|entry| entry.file_name());

        for entry in entries {
            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("toml") {
                continue;
            }
            match fs::read_to_string(&path) {
                Ok(text) => {
                    self.load_text(&path.display().to_string(), &text, discovery, claimed, true)
                }
                Err(error) => discovery
                    .warnings
                    .push(format!("{}: {error}", path.display())),
            }
        }
    }

    /// 解析一段 manifest 文本，把里面的动作加进去。
    ///
    /// `on_duplicate`：用户目录里的 id 重复值得提醒（多半是复制粘贴忘了改），
    /// 而内置默认被用户覆盖是**预期行为**，就不吵。
    fn load_text(
        &self,
        source: &str,
        text: &str,
        discovery: &mut Discovery,
        claimed: &mut BTreeSet<String>,
        on_duplicate: bool,
    ) {
        let parsed: ManifestFile = match toml::from_str(text) {
            Ok(parsed) => parsed,
            Err(error) => {
                discovery.warnings.push(format!("{source}: {error}"));
                return;
            }
        };

        for spec in &parsed.action {
            match spec.build() {
                Ok(tool) => {
                    let key = spec.id.trim().to_string();
                    if !claimed.insert(key.clone()) {
                        if on_duplicate {
                            discovery
                                .warnings
                                .push(format!("{source}: 动作 id 重复「{key}」，跳过这一条"));
                        }
                        continue;
                    }
                    discovery.tools.push(tool);
                }
                Err(problem) => discovery.warnings.push(format!("{source}: {problem}")),
            }
        }
    }
}

impl Provider for ManifestProvider {
    fn id(&self) -> &'static str {
        PROVIDER_ID
    }

    fn label(&self) -> &'static str {
        PROVIDER_LABEL
    }

    fn discover(&self) -> io::Result<Discovery> {
        let mut discovery = Discovery::default();
        let mut claimed: BTreeSet<String> = BTreeSet::new();

        // 用户目录优先：先占住的 id 会让内置默认里同名的那条让位。
        for dir in &self.dirs {
            self.load_dir(dir, &mut discovery, &mut claimed);
        }
        for (source, text) in BUNDLED {
            self.load_text(source, text, &mut discovery, &mut claimed, false);
        }
        Ok(discovery)
    }
}

// ── TOML 结构 ────────────────────────────────────────────────────────────────
//
// 一律 `deny_unknown_fields`：手写的配置里拼错一个键，应该当场报出来，
// 而不是被静默忽略、然后让人对着「怎么没生效」发呆。

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ManifestFile {
    #[serde(default)]
    action: Vec<ManifestAction>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ManifestAction {
    id: String,
    name: String,
    summary: String,
    domain: String,
    #[serde(default)]
    tags: Vec<String>,
    program: String,
    #[serde(default)]
    base_argv: Vec<String>,
    #[serde(default)]
    install: Option<String>,
    #[serde(default)]
    input: Option<String>,
    #[serde(default)]
    output: Option<String>,
    #[serde(default)]
    features: Option<String>,
    #[serde(default)]
    foreach: Option<String>,
    /// 允许不带参数运行（见 [`crate::model::Action::allow_empty`]）。
    #[serde(default)]
    allow_empty: bool,
    /// 哪些退出码算成功（见 [`crate::model::Action::ok_exit_codes`]）。
    #[serde(default)]
    ok_exit_codes: Option<Vec<i32>>,
    #[serde(default)]
    duration_from: Option<String>,
    #[serde(default)]
    limit_from: Option<String>,
    #[serde(default)]
    mode: Option<String>,
    #[serde(default)]
    danger: Option<String>,
    #[serde(default)]
    argument: Vec<ManifestArgument>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ManifestArgument {
    key: String,
    label: String,
    kind: String,
    #[serde(default)]
    default: Option<String>,
    #[serde(default)]
    choices: Vec<ManifestChoice>,
    #[serde(default)]
    required: bool,
    #[serde(default)]
    flag: Option<String>,
    #[serde(default)]
    flag_join: bool,
    #[serde(default)]
    placement: Option<String>,
    #[serde(default)]
    sensitive: bool,
    #[serde(default)]
    repeatable: bool,
    /// 带 flag 的多值：`true` = 每个值配一个 flag（`-i a -i b`）；
    /// 缺省 = flag 一次、值平铺（`-S a b c`）。
    #[serde(default)]
    repeat_flag: bool,
    #[serde(default)]
    dir_only: bool,
    #[serde(default)]
    separator: Option<String>,
    #[serde(default)]
    help: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ManifestChoice {
    label: String,
    value: String,
}

impl ManifestAction {
    /// 翻译成工具定义；`Err` 里是给用户看的毛病说明。
    fn build(&self) -> Result<ToolDefinition, String> {
        let id = self.id.trim();
        if id.is_empty() {
            return Err(String::from("有个动作缺少 id"));
        }
        let complain = |what: &str| format!("{id}: {what}");

        if self.name.trim().is_empty() {
            return Err(complain("缺少 name"));
        }
        if self.summary.trim().is_empty() {
            return Err(complain("缺少 summary"));
        }
        if self.program.trim().is_empty() {
            return Err(complain("缺少 program"));
        }
        let domain = Domain::parse(&self.domain)
            .ok_or_else(|| complain(&format!("不认识的域「{}」", self.domain)))?;

        let mut arguments = Vec::with_capacity(self.argument.len());
        let mut keys: BTreeSet<String> = BTreeSet::new();
        for raw in &self.argument {
            let argument = raw.build(id)?;
            if !keys.insert(argument.key.clone()) {
                return Err(complain(&format!("参数 key 重复「{}」", argument.key)));
            }
            arguments.push(argument);
        }

        // `foreach` 指向的字段必须真能装下多个输入 —— 漏配在**发现阶段**就报出来，
        // 而不是等你选了两个文件、被选择器拒绝时才发现（实拍踩过一次）。
        // 注意：带 flag 是允许的（每次跑只有一条值）。
        if let Some(key) = self
            .foreach
            .as_deref()
            .map(str::trim)
            .filter(|key| !key.is_empty())
        {
            let Some(argument) = arguments.iter().find(|argument| argument.key == key) else {
                return Err(complain(&format!("foreach 指向的参数「{key}」不存在")));
            };
            if !argument.repeatable {
                return Err(complain(&format!(
                    "foreach 的字段「{key}」必须写 repeatable = true（每个输入各跑一次，它要装得下多个）"
                )));
            }
        }

        let program = self.program.trim().to_string();
        let deps = metadata::dependencies_of(std::slice::from_ref(&program));
        let ready = deps.is_ready();

        Ok(ToolDefinition {
            id: format!("{PROVIDER_ID}:{id}"),
            name: self.name.trim().to_string(),
            provider: PROVIDER_LABEL.to_string(),
            domain,
            tags: self
                .tags
                .iter()
                .map(|tag| tag.trim().to_string())
                .filter(|tag| !tag.is_empty())
                .collect(),
            summary: self.summary.trim().to_string(),
            input: self.input.clone(),
            output: self.output.clone(),
            features: self.features.clone(),
            requires: deps.required,
            missing_deps: deps.missing,
            install_hint: self.install.clone(),
            mode: match self.mode.as_deref().map(str::trim) {
                // 默认捕获输出：manifest 包的是普通 CLI，不是 fzf 那种 TUI。
                None | Some("") => RunMode::Capture,
                Some(raw) => RunMode::parse(raw).ok_or_else(|| {
                    complain(&format!(
                        "不认识的 mode「{raw}」，可用 interactive / capture"
                    ))
                })?,
            },
            danger: match self.danger.as_deref().map(str::trim) {
                None | Some("") => Danger::Safe,
                Some(raw) => Danger::parse(raw).ok_or_else(|| {
                    complain(&format!("不认识的 danger「{raw}」，可用 safe / caution"))
                })?,
            },
            action: Some(Action {
                program: program.clone(),
                base_argv: self.base_argv.clone(),
                arguments,
                duration_from: self.duration_from.clone(),
                limit_from: self.limit_from.clone(),
                foreach: self.foreach.clone(),
                allow_empty: self.allow_empty,
                ok_exit_codes: self.ok_exit_codes.clone().unwrap_or_else(|| vec![0]),
            }),
            // 解析成绝对路径，详情区就能说清「到底会跑哪个二进制」。
            path: metadata::resolve_program(&program).unwrap_or_else(|| PathBuf::from(&program)),
            ready,
        })
    }
}

impl ManifestArgument {
    fn build(&self, action_id: &str) -> Result<Argument, String> {
        let key = self.key.trim();
        if key.is_empty() {
            return Err(format!("{action_id}: 有个参数缺少 key"));
        }
        if self.label.trim().is_empty() {
            return Err(format!("{action_id}/{key}: 缺少 label"));
        }

        let kind = match self.kind.trim() {
            "text" => ArgKind::Text,
            "path" => ArgKind::Path,
            "choice" => ArgKind::Choice,
            "toggle" => ArgKind::Toggle,
            other => {
                return Err(format!(
                    "{action_id}/{key}: 不认识的参数类型「{other}」，可用 text / path / choice / toggle"
                ));
            }
        };

        if kind == ArgKind::Choice && self.choices.is_empty() {
            return Err(format!("{action_id}/{key}: kind = choice 必须写 choices"));
        }
        if kind != ArgKind::Choice && !self.choices.is_empty() {
            return Err(format!("{action_id}/{key}: 只有 choice 能写 choices"));
        }
        if kind == ArgKind::Toggle && self.flag.is_none() {
            return Err(format!(
                "{action_id}/{key}: toggle 必须给 flag，否则打开了也没有效果"
            ));
        }
        // 多值 + flag 是允许的：默认「flag 一次、值平铺」（`-S a b c`），
        // 想「每个值一个 flag」（`-i a -i b`）就写 repeat_flag = true。
        if self.repeatable && matches!(kind, ArgKind::Choice | ArgKind::Toggle) {
            return Err(format!(
                "{action_id}/{key}: 只有 text / path 能多值，choice 与 toggle 不行"
            ));
        }
        if self.separator.is_some() && !self.repeatable {
            return Err(format!(
                "{action_id}/{key}: separator 只在 repeatable = true 时有意义"
            ));
        }
        if self.flag_join && self.flag.is_none() {
            return Err(format!(
                "{action_id}/{key}: flag_join 只在同时写了 flag 时才有意义"
            ));
        }

        // 不写 placement 就按默认：带 flag 的落中间，位置参数落最后。
        let placement = match self.placement.as_deref().map(str::trim) {
            None | Some("") => match self.flag {
                Some(_) => ArgPlacement::Middle,
                None => ArgPlacement::Trailing,
            },
            Some("leading") => ArgPlacement::Leading,
            Some("middle") => ArgPlacement::Middle,
            Some("trailing") => ArgPlacement::Trailing,
            Some(other) => {
                return Err(format!(
                    "{action_id}/{key}: 不认识的 placement「{other}」，可用 leading / middle / trailing"
                ));
            }
        };

        Ok(Argument {
            key: key.to_string(),
            label: self.label.trim().to_string(),
            kind,
            default: self.default.clone(),
            choices: self
                .choices
                .iter()
                .map(|choice| Choice::new(choice.label.trim(), &choice.value))
                .collect(),
            required: self.required,
            flag: self.flag.clone(),
            flag_join: self.flag_join,
            placement,
            sensitive: self.sensitive,
            repeatable: self.repeatable,
            repeat_flag: self.repeat_flag,
            dir_only: self.dir_only,
            separator: self.separator.clone().unwrap_or_else(|| String::from(",")),
            help: self.help.clone(),
        })
    }
}

/// 内置动作的成品定义。
///
/// app / ui 层的测试用它拿到**真实**的带参数工具，而不是另造仿制品。
#[cfg(test)]
pub fn bundled_tools() -> Vec<ToolDefinition> {
    let discovery = ManifestProvider::new(Vec::new())
        .discover()
        .expect("内置 manifest 的发现不该失败");
    discovery.tools
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        path::PathBuf,
        time::{SystemTime, UNIX_EPOCH},
    };

    use super::{BUNDLED_ACTION_COUNT, ManifestProvider};
    use crate::{
        model::Domain,
        providers::{Discovery, Provider},
    };

    fn temp_dir(tag: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock before unix epoch")
            .as_nanos();
        let dir =
            std::env::temp_dir().join(format!("toolbox-hub-{tag}-{}-{nanos}", std::process::id()));
        fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    fn bundled_discovery() -> Discovery {
        ManifestProvider::new(Vec::new())
            .discover()
            .expect("内置 manifest 必须能解析")
    }

    fn by_id<'a>(discovery: &'a Discovery, id: &str) -> &'a crate::model::ToolDefinition {
        discovery
            .tools
            .iter()
            .find(|tool| tool.id == id)
            .unwrap_or_else(|| panic!("没有这个内置动作: {id}"))
    }

    /// 把必填项随便填上，构建出「默认 + 必填」的命令。
    fn argv_with_defaults(tool: &crate::model::ToolDefinition) -> Vec<String> {
        let action = tool.action.as_ref().expect("manifest 工具必须带动作");
        let mut values = action.default_values();
        for argument in &action.arguments {
            if argument.required && values.get(&argument.key).unwrap_or("").is_empty() {
                values.set(&argument.key, format!("/tmp/{}", argument.key));
            }
        }
        action.build_argv(&values).expect("填好必填项就该能构建")
    }

    #[test]
    fn bundled_manifests_all_parse_without_warnings() {
        let discovery = bundled_discovery();
        assert!(
            discovery.warnings.is_empty(),
            "内置 manifest 不该有毛病: {:#?}",
            discovery.warnings
        );
        assert_eq!(
            discovery.tools.len(),
            BUNDLED_ACTION_COUNT,
            "内置动作数量变了？"
        );
        assert!(discovery.tools.iter().all(|tool| tool.action.is_some()));
        assert!(discovery.tools.iter().all(|tool| !tool.summary.is_empty()));

        // 覆盖面：图像 / 媒体 / 网络 / 开发 / 工具 都该有内置动作。
        for domain in [
            Domain::Image,
            Domain::Media,
            Domain::Network,
            Domain::Dev,
            Domain::Tools,
        ] {
            assert!(
                discovery.tools.iter().any(|tool| tool.domain == domain),
                "{} 域没有内置动作",
                domain.label()
            );
        }
    }

    #[test]
    fn every_bundled_action_keeps_its_program_and_base_argv() {
        let discovery = bundled_discovery();
        for tool in &discovery.tools {
            let argv = argv_with_defaults(tool);
            let action = tool.action.as_ref().expect("带动作");
            let mut expected_prefix = action.base_argv.clone();
            for argument in &action.arguments {
                if argument.required {
                    expected_prefix.push(format!("/tmp/{}", argument.key));
                }
            }
            // base_argv 必须原样打头，缺一不可。
            assert_eq!(
                &argv[..action.base_argv.len()],
                action.base_argv.as_slice(),
                "{} 的 base_argv 没排在前面: {argv:?}",
                tool.id
            );
            // 默认取值不该构建出空 argv —— 除非这个动作**本来就没有参数**
            // （`checkupdates` 这种无参命令合法）。
            //
            // 这条断言抓到过真漏配：`pacman -Qdt` 与 `paru -Syu` 忘了写进 base_argv，
            // 结果是光跑 `pacman`、`paru`（打印用法就退出）。
            assert!(
                !argv.is_empty() || action.allow_empty || action.arguments.is_empty(),
                "{} 构建出了空 argv —— 本体命令（base_argv）是不是忘了写？\n\
                 确实允许空 argv 的话，就在动作里写 allow_empty = true",
                tool.id
            );
        }
    }

    #[test]
    fn app_and_ui_tests_get_real_actions_from_here() {
        let tools = super::bundled_tools();
        assert_eq!(tools.len(), BUNDLED_ACTION_COUNT);
        assert!(tools.iter().all(|tool| tool.action.is_some()));
    }

    /// 实测：`magick in.png -resize … out.webp` 才行，
    /// 把开关全放前面会被拒绝（`no images found for operation '-resize'`）。
    #[test]
    fn imagemagick_takes_the_input_before_the_options() {
        let discovery = bundled_discovery();
        let tool = by_id(&discovery, "manifest:magick-convert");
        let action = tool.action.as_ref().expect("带动作");
        let mut values = action.default_values();
        values.set("input", "/tmp/in.png");
        values.set("output", "/tmp/out.webp");
        values.set("resize", "1920x");
        values.set("quality", "85");
        values.set("strip", "true");

        assert_eq!(
            action.build_argv(&values).expect("应能构建"),
            vec![
                "/tmp/in.png",
                "-resize",
                "1920x",
                "-quality",
                "85",
                "-strip",
                "/tmp/out.webp"
            ],
            "输入文件必须在操作之前（placement = leading）"
        );
    }

    /// 需要 root 的两个动作：`sudo` 必须打头，而且 `-Rns` 只能出现一次。
    ///
    /// 这条抓到过真问题：`program` 从 `pacman` 改成 `sudo` 之后，参数里那个
    /// `flag = "-Rns"` 会和 `base_argv` 里的撞车，变成 `sudo pacman -Rns -Rns bash`。
    #[test]
    fn root_actions_put_sudo_first_and_do_not_repeat_flags() {
        let discovery = bundled_discovery();

        let remove = by_id(&discovery, "manifest:pkg-remove");
        let action = remove.action.as_ref().expect("带动作");
        let mut values = action.default_values();
        values.set("package", "fzf bash");
        assert_eq!(
            action.build_argv(&values).expect("应能构建"),
            vec!["pacman", "-Rns", "fzf", "bash"],
            "sudo 是程序，pacman -Rns 在后面，包名最后"
        );
        assert_eq!(action.program, "sudo");

        let cache = by_id(&discovery, "manifest:pkg-clean-cache");
        let action = cache.action.as_ref().expect("带动作");
        assert_eq!(action.program, "sudo");
        assert_eq!(
            action
                .build_argv(&action.default_values())
                .expect("应能构建"),
            vec!["paccache", "-rk1"],
            "paccache 认 -rk1 这种连写"
        );
    }

    #[test]
    fn jq_keeps_the_expression_before_the_file() {
        let discovery = bundled_discovery();
        let tool = by_id(&discovery, "manifest:jq-query");
        let action = tool.action.as_ref().expect("带动作");
        let mut values = action.default_values();
        values.set("filter", ".items[].name");
        values.set("file", "/tmp/data.json");
        values.set("compact", "true");

        assert_eq!(
            action.build_argv(&values).expect("应能构建"),
            vec!["-c", ".items[].name", "/tmp/data.json"],
            "jq 的表达式必须排在文件前面"
        );
    }

    #[test]
    fn sevenzip_joins_switch_and_value_into_one_element() {
        let discovery = bundled_discovery();
        let tool = by_id(&discovery, "manifest:7z-create");
        let action = tool.action.as_ref().expect("带动作");
        let mut values = action.default_values();
        values.set("archive", "/tmp/backup.7z");
        values.set("source", "/tmp/src");
        values.set("level", "9");

        assert_eq!(
            action.build_argv(&values).expect("应能构建"),
            vec!["a", "-mx9", "-r", "/tmp/backup.7z", "/tmp/src"],
            "7z 只认贴在一起的开关（实测 -mx9 可以、-mx 9 不行）"
        );
    }

    #[test]
    fn pandoc_and_aria2c_and_exiftool_build_the_expected_commands() {
        let discovery = bundled_discovery();

        let pandoc = by_id(&discovery, "manifest:pandoc-convert");
        let action = pandoc.action.as_ref().expect("带动作");
        let mut values = action.default_values();
        values.set("input", "/tmp/a.md");
        values.set("to", "html");
        values.set("output", "/tmp/b.html");
        assert_eq!(
            action.build_argv(&values).expect("应能构建"),
            vec![
                "--to",
                "html",
                "--standalone",
                "-o",
                "/tmp/b.html",
                "/tmp/a.md"
            ]
        );

        let aria = by_id(&discovery, "manifest:aria2c-download");
        let action = aria.action.as_ref().expect("带动作");
        let mut values = action.default_values();
        values.set("url", "https://example.com/x.zip");
        values.set("dir", "/tmp/dl");
        values.set("connections", "16");
        assert_eq!(
            action.build_argv(&values).expect("应能构建"),
            vec![
                "-d",
                "/tmp/dl",
                "-x",
                "16",
                "-c",
                "https://example.com/x.zip"
            ]
        );

        let exif = by_id(&discovery, "manifest:exif-strip");
        let action = exif.action.as_ref().expect("带动作");
        let mut values = action.default_values();
        values.set("input", "/tmp/p.jpg");
        values.set("output", "/tmp/clean.jpg");
        assert_eq!(
            action.build_argv(&values).expect("应能构建"),
            vec!["-all=", "-o", "/tmp/clean.jpg", "/tmp/p.jpg"],
            "清除元数据要写成新文件，不覆盖原图"
        );
    }

    #[test]
    fn every_argument_has_a_label_and_choices_carry_both_label_and_value() {
        let discovery = bundled_discovery();
        for tool in &discovery.tools {
            let action = tool.action.as_ref().expect("带动作");
            assert!(!action.program.is_empty());
            for argument in &action.arguments {
                assert!(!argument.label.is_empty(), "{}: {}", tool.id, argument.key);
                assert!(
                    argument.help.is_some(),
                    "{}: {} 缺少 help，表单里会显示空说明",
                    tool.id,
                    argument.key
                );
                if argument.kind == crate::model::ArgKind::Choice {
                    assert!(!argument.choices.is_empty());
                    for choice in &argument.choices {
                        assert!(!choice.label.is_empty());
                    }
                }
            }
        }
    }

    /// ffmpeg 的每个输入都必须写成 `-i <文件>`。
    ///
    /// 漏了 `-i`，ffmpeg 会把**输入当成输出**，报出来的却是
    /// 「Output file does not contain any stream」—— 很难一眼看懂。
    /// （这个坑是 ffmpeg 冒烟测试真跑出来的。）
    #[test]
    fn ffmpeg_inputs_are_always_passed_with_dash_i() {
        let discovery = bundled_discovery();
        for id in [
            "manifest:ffmpeg-trim-fast",
            "manifest:ffmpeg-trim-exact",
            "manifest:ffmpeg-compress",
            "manifest:ffmpeg-resize",
            "manifest:ffmpeg-extract-audio",
            "manifest:ffmpeg-to-gif",
            "manifest:ffmpeg-mute",
            "manifest:ffmpeg-merge-av",
        ] {
            let tool = by_id(&discovery, id);
            let argv = argv_with_defaults(tool);

            let last = argv
                .iter()
                .rposition(|item| item == "-i")
                .unwrap_or_else(|| panic!("{id} 的输入没有 -i: {argv:?}"));
            assert!(last + 1 < argv.len(), "{id} 的 -i 后面没有跟文件: {argv:?}");
        }
    }

    /// `foreach` 配错要在发现阶段报出来（实拍踩过一次：字段不是 repeatable，
    /// 选择器多选被拒，而报错信息完全指不到 manifest）。
    #[test]
    fn a_misconfigured_foreach_is_reported() {
        let dir = temp_dir("manifest-foreach");
        let write = |name: &str, body: &str| {
            fs::write(dir.join(name), body).expect("write");
        };

        // 指向不存在的参数
        write(
            "missing.toml",
            "[[action]]\nid = \"m1\"\nname = \"M\"\nsummary = \"s\"\ndomain = \"媒体\"\nprogram = \"ffmpeg\"\nforeach = \"input\"\n",
        );
        // 字段存在但不是 repeatable
        write(
            "notrepeat.toml",
            "[[action]]\nid = \"m2\"\nname = \"M\"\nsummary = \"s\"\ndomain = \"媒体\"\nprogram = \"ffmpeg\"\nforeach = \"input\"\n\n[[action.argument]]\nkey = \"input\"\nlabel = \"输入\"\nkind = \"path\"\n",
        );
        // 带 flag 的批量字段是**合法的**（每次跑只有一条值，`-i <一个>` 无歧义）
        write(
            "flagged.toml",
            "[[action]]\nid = \"m3\"\nname = \"M\"\nsummary = \"s\"\ndomain = \"媒体\"\nprogram = \"ffmpeg\"\nforeach = \"input\"\n\n[[action.argument]]\nkey = \"input\"\nlabel = \"输入\"\nkind = \"path\"\nrepeatable = true\nflag = \"-i\"\n",
        );

        let discovery = ManifestProvider::new(vec![dir.clone()])
            .discover()
            .expect("discover");

        assert_eq!(discovery.warnings.len(), 2, "{:#?}", discovery.warnings);
        assert!(
            discovery.tools.iter().any(|tool| tool.id == "manifest:m3"),
            "带 flag 的批量字段应该被接受: {:#?}",
            discovery.warnings
        );
        assert!(
            discovery
                .warnings
                .iter()
                .any(|warning| warning.contains("不存在")),
            "{:#?}",
            discovery.warnings
        );
        assert!(
            discovery
                .warnings
                .iter()
                .any(|warning| warning.contains("repeatable")),
            "{:#?}",
            discovery.warnings
        );

        fs::remove_dir_all(&dir).expect("cleanup");
    }

    /// 会改系统的包管理动作必须标 `danger = "caution"`。
    ///
    /// 这是一条**策略**测试：以后再加安装/卸载/更新类的动作，忘了标危险度就会红。
    #[test]
    fn destructive_package_actions_are_marked_caution() {
        let discovery = bundled_discovery();
        let destructive = [
            "manifest:pkg-install",
            "manifest:pkg-remove",
            "manifest:pkg-upgrade",
            "manifest:pkg-clean-cache",
        ];

        for id in destructive {
            let tool = by_id(&discovery, id);
            assert_eq!(
                tool.danger,
                crate::model::Danger::Caution,
                "{id} 会改系统，必须标 caution"
            );
            // 而且要接管终端：sudo 密码、Y/n 都得你自己回答
            assert_eq!(
                tool.mode,
                crate::model::RunMode::Interactive,
                "{id} 要 sudo / 要确认，必须 interactive"
            );
        }

        // 只读的那些反过来：不该打扰用户
        for id in [
            "manifest:pkg-search",
            "manifest:pkg-updates",
            "manifest:pkg-orphans",
        ] {
            let tool = by_id(&discovery, id);
            assert_eq!(tool.danger, crate::model::Danger::Safe, "{id} 是只读的");
            assert_eq!(
                tool.mode,
                crate::model::RunMode::Capture,
                "{id} 该留在界面里"
            );
        }
    }

    /// 内置的 foreach 动作必须配得对（真跑时才知道痛，所以这里先钉住）。
    #[test]
    fn bundled_foreach_actions_are_configured_correctly() {
        let discovery = bundled_discovery();
        let mut found = 0;
        for tool in &discovery.tools {
            let Some(action) = tool.action.as_ref() else {
                continue;
            };
            let Some(key) = action.foreach.as_deref() else {
                continue;
            };
            found += 1;
            let argument = action
                .arguments
                .iter()
                .find(|argument| argument.key == key)
                .unwrap_or_else(|| panic!("{} 的 foreach 指向了不存在的 {key}", tool.id));
            assert!(argument.repeatable, "{} 的 {key} 要 repeatable", tool.id);
            // 带不带 flag 都合法（ffmpeg 的输入就是 `-i`；每次跑只有一条值）。
        }
        assert!(found >= 5, "至少应有几个批量动作，实际 {found}");
    }

    /// 该多值的字段必须真的标了多值 —— 否则「一次处理多个文件」会静默失效。
    #[test]
    fn multi_value_actions_are_marked_repeatable() {
        let discovery = bundled_discovery();
        for (id, key) in [
            ("manifest:jq-query", "file"),
            ("manifest:7z-create", "source"),
            ("manifest:magick-convert", "input"),
            ("manifest:aria2c-download", "url"),
            ("manifest:yt-dlp-video", "url"),
            ("manifest:yt-dlp-audio", "url"),
            ("manifest:pandoc-convert", "input"),
            ("manifest:exif-read", "file"),
        ] {
            let tool = by_id(&discovery, id);
            let action = tool.action.as_ref().expect("带动作");
            let argument = action
                .arguments
                .iter()
                .find(|argument| argument.key == key)
                .unwrap_or_else(|| panic!("{id} 里没有参数 {key}"));
            assert!(argument.repeatable, "{id} 的 {key} 应该是多值");
            // 带不带 flag 都合法：ffmpeg 的输入就是 `-i`（每次跑只有一条值）。
        }
    }

    /// 多值一次跑多个输入：命令形状要正确。
    #[test]
    fn repeatable_arguments_produce_one_element_per_value() {
        let discovery = bundled_discovery();
        let tool = by_id(&discovery, "manifest:jq-query");
        let action = tool.action.as_ref().expect("带动作");
        let mut values = action.default_values();
        values.set("filter", ".items[].name");
        values.set("file", "/tmp/a.json, /tmp/b.json");
        values.set("compact", "true");

        assert_eq!(
            action.build_argv(&values).expect("应能构建"),
            vec!["-c", ".items[].name", "/tmp/a.json", "/tmp/b.json"]
        );
    }

    #[test]
    fn user_manifest_overrides_the_bundled_action_with_the_same_id() {
        let dir = temp_dir("manifest-override");
        fs::write(
            dir.join("mine.toml"),
            r#"
[[action]]
id = "yt-dlp-video"
name = "我的下载"
summary = "自定义的"
domain = "媒体"
program = "yt-dlp"

[[action.argument]]
key = "url"
label = "链接"
kind = "text"
required = true
help = "随便"
"#,
        )
        .expect("write");

        let discovery = ManifestProvider::new(vec![dir.clone()])
            .discover()
            .expect("discover");

        let mine: Vec<_> = discovery
            .tools
            .iter()
            .filter(|tool| tool.id == "manifest:yt-dlp-video")
            .collect();
        assert_eq!(mine.len(), 1, "同 id 只能留一条");
        assert_eq!(mine[0].name, "我的下载", "用户那份应覆盖内置默认");
        // 覆盖只影响同 id 的动作，别的内置动作照常在。
        assert!(discovery.tools.iter().any(|t| t.id == "manifest:jq-query"));
        assert!(discovery.warnings.is_empty(), "覆盖是预期行为，不该报警");

        fs::remove_dir_all(&dir).expect("cleanup");
    }

    #[test]
    fn broken_manifests_become_warnings_instead_of_killing_everything() {
        let dir = temp_dir("manifest-broken");

        // 缺字段：能被 TOML 解析，但校验不过
        fs::write(
            dir.join("incomplete.toml"),
            "[[action]]\nid = \"x\"\nname = \"X\"\n",
        )
        .expect("write");
        // 语法错误
        fs::write(dir.join("syntax.toml"), "[[action]\n").expect("write");
        // 拼错的键：deny_unknown_fields 应当场报出来
        fs::write(
            dir.join("typo.toml"),
            "[[action]]\nid = \"jq-typo\"\nname = \"J\"\nsummary = \"s\"\ndomain = \"开发\"\nprogrm = \"jq\"\n",
        )
        .expect("write");
        // 不认识的参数类型
        fs::write(
            dir.join("kind.toml"),
            "[[action]]\nid = \"k\"\nname = \"K\"\nsummary = \"s\"\ndomain = \"开发\"\nprogram = \"jq\"\n\n[[action.argument]]\nkey = \"a\"\nlabel = \"A\"\nkind = \"wizard\"\n",
        )
        .expect("write");

        let discovery = ManifestProvider::new(vec![dir.clone()])
            .discover()
            .expect("discover");

        assert_eq!(
            discovery.tools.len(),
            BUNDLED_ACTION_COUNT,
            "坏文件不该影响内置动作: {:#?}",
            discovery.warnings
        );
        assert_eq!(discovery.warnings.len(), 4, "{:#?}", discovery.warnings);
        for name in ["incomplete.toml", "syntax.toml", "typo.toml", "kind.toml"] {
            assert!(
                discovery.warnings.iter().any(|w| w.contains(name)),
                "{name} 的问题没被报出来: {:#?}",
                discovery.warnings
            );
        }

        fs::remove_dir_all(&dir).expect("cleanup");
    }

    #[test]
    fn missing_manifest_directories_are_ignored() {
        let discovery = ManifestProvider::new(vec![PathBuf::from("/nonexistent/toolbox-hub")])
            .discover()
            .expect("discover");
        assert_eq!(
            discovery.tools.len(),
            BUNDLED_ACTION_COUNT,
            "内置动作照常加载"
        );
        assert!(discovery.warnings.is_empty(), "没建目录不是问题");
    }

    #[test]
    fn non_toml_files_in_the_directory_are_skipped() {
        let dir = temp_dir("manifest-others");
        fs::write(dir.join("README.md"), "# 说明").expect("write");
        fs::write(dir.join("notes.txt"), "x").expect("write");

        let discovery = ManifestProvider::new(vec![dir.clone()])
            .discover()
            .expect("discover");
        assert_eq!(discovery.tools.len(), BUNDLED_ACTION_COUNT);
        assert!(discovery.warnings.is_empty());

        fs::remove_dir_all(&dir).expect("cleanup");
    }

    /// 真跑一遍：填上真实路径、真的执行外部程序，看它们认不认我们拼出来的参数。
    ///
    /// 形状测试只能说明「argv 长这样」，这个测试说明「命令真的能跑」。
    /// 默认不跑（会写临时文件、真的调用外部程序）：
    /// 包管理那批**只读**动作真跑一遍（安装/卸载/更新绝不实跑）。
    ///
    /// 抓的是「本体命令忘写进 base_argv」这类漏配：`pacman -Qdt` 少写就变成光跑
    /// `pacman`（打印用法、退出码非 0）—— 只有真跑才看得见。
    #[test]
    #[ignore = "真的执行 pacman/paru/checkupdates（只读），默认跳过"]
    fn smoke_run_package_queries() {
        let discovery = bundled_discovery();

        // (动作 id, 要填的值, 输出是否必须非空)
        type Case<'a> = (&'a str, &'a [(&'a str, &'a str)], bool);
        let cases: &[Case] = &[
            ("manifest:pkg-search", &[("query", "fzf")], true),
            ("manifest:pkg-info", &[("package", "fzf")], true),
            ("manifest:aur-search", &[("query", "pacsea")], true),
            ("manifest:aur-info", &[("package", "pacsea-bin")], true),
            ("manifest:pkg-owner", &[("file", "/usr/bin/pac")], true),
            ("manifest:pkg-files", &[("package", "pacman")], true),
            // 孤儿包完全可能是 0 个，所以只要求跑通
            ("manifest:pkg-orphans", &[], false),
            ("manifest:pkg-cache-size", &[], true),
        ];

        for (id, values, needs_output) in cases {
            let captured = run_built(&discovery, id, values, std::path::Path::new("/tmp"));
            // 用动作自己声明的退出码白名单判定：`pacman -Qdt` 没孤儿包时退 1，
            // 那是「没匹配到」而不是失败（这条是真跑之后才加上的）。
            let action = by_id(&discovery, id).action.as_ref().expect("带动作");
            assert!(
                action
                    .ok_exit_codes
                    .contains(&captured.status.code().unwrap_or(i32::MIN)),
                "{id} 真跑失败（本体命令写对了吗？）: {}",
                captured.status
            );
            if *needs_output {
                assert!(
                    !String::from_utf8_lossy(&captured.stdout).trim().is_empty(),
                    "{id} 没有输出 —— base_argv 是不是漏了？"
                );
            }
        }

        // checkupdates 要读数据库，慢一点，单独跑；有没有更新都算成功。
        let updates = run_built(
            &discovery,
            "manifest:pkg-updates",
            &[],
            std::path::Path::new("/tmp"),
        );
        assert!(
            updates.status.success() || updates.status.code() == Some(2),
            "checkupdates 退出码 {}（2 = 没有更新，也算正常）",
            updates.status
        );
    }

    /// `cargo test -- --ignored --nocapture smoke_run_bundled_actions`
    /// 按表单的值构建 argv 并**真的执行**；打印实际跑的命令。
    ///
    /// 「形状对不对」只有真跑才知道 —— 这个项目里已经栽过两次
    /// （7z 的 `-mx9`、ImageMagick 的输入必须在前）。
    fn run_built(
        discovery: &Discovery,
        id: &str,
        pairs: &[(&str, &str)],
        cwd: &std::path::Path,
    ) -> std::process::Output {
        use std::process::Command as Proc;

        let tool = by_id(discovery, id);
        let action = tool.action.as_ref().expect("带动作");
        let mut values = action.default_values();
        for (key, value) in pairs {
            values.set(key, *value);
        }
        let argv = action.build_argv(&values).expect("应能构建");
        let output = Proc::new(&tool.path)
            .args(&argv)
            .current_dir(cwd)
            .output()
            .unwrap_or_else(|error| panic!("{id} 跑不起来: {error}"));

        println!(
            "$ {} {}\n  → {} {}",
            tool.path.display(),
            argv.join(" "),
            output.status,
            String::from_utf8_lossy(&output.stderr)
                .lines()
                .next()
                .unwrap_or("(无 stderr)")
        );
        output
    }

    /// ffmpeg 那批动作真跑一遍。
    ///
    /// 这几个的坑最多：编码选项必须在 `-i` 之后、`-ss` 要在输入前、
    /// `-map` 的顺序、调色板滤镜的语法 —— 形状对不对只有真跑才知道。
    #[test]
    #[ignore = "真的执行 ffmpeg、写临时文件，默认跳过"]
    fn smoke_run_ffmpeg_actions_on_real_files() {
        use std::process::Command as Proc;

        let dir = temp_dir("ffmpeg-run");
        let video = dir.join("in.mp4");
        let audio = dir.join("music.m4a");

        // 造素材必须显式指定 libx264：这台机器的 ffmpeg 默认走硬件编码器（rkmpp），
        // 实测初始化失败。
        let made = Proc::new("ffmpeg")
            .args([
                "-v",
                "error",
                "-f",
                "lavfi",
                "-i",
                "testsrc=duration=1:size=320x240:rate=10",
                "-f",
                "lavfi",
                "-i",
                "sine=frequency=440:duration=1",
                "-c:v",
                "libx264",
                "-preset",
                "ultrafast",
                "-pix_fmt",
                "yuv420p",
                "-c:a",
                "aac",
                "-shortest",
                "-y",
            ])
            .arg(&video)
            .status()
            .expect("ffmpeg 应可用");
        assert!(made.success(), "造测试视频失败");

        let made = Proc::new("ffmpeg")
            .args([
                "-v",
                "error",
                "-f",
                "lavfi",
                "-i",
                "sine=frequency=440:duration=1",
                "-c:a",
                "aac",
                "-y",
            ])
            .arg(&audio)
            .status()
            .expect("ffmpeg 应可用");
        assert!(made.success(), "造测试音频失败");

        let discovery = bundled_discovery();
        let run = |id: &str, pairs: &[(&str, &str)]| run_built(&discovery, id, pairs, &dir);
        let path = |name: &str| dir.join(name).display().to_string();
        let exists = |name: &str| {
            let path = dir.join(name);
            let size = fs::metadata(&path).map(|meta| meta.len()).unwrap_or(0);
            assert!(size > 0, "{} 没生成或大小为 0", path.display());
        };

        // 1) 看信息：唯一一个 capture 模式的动作
        let out = run("manifest:ffmpeg-probe", &[("input", &path("in.mp4"))]);
        assert!(out.status.success(), "ffprobe 失败");
        let info = String::from_utf8_lossy(&out.stdout);
        assert!(info.contains("h264"), "应报出视频编码: {info}");

        // 2) 无损裁剪：-ss 必须排在输入之前
        let out = run(
            "manifest:ffmpeg-trim-fast",
            &[
                ("input", &path("in.mp4")),
                ("start", "0.3"),
                ("duration", "0.4"),
                ("output", &path("cut.mp4")),
            ],
        );
        assert!(out.status.success(), "无损裁剪失败");
        exists("cut.mp4");

        // 3) 精确裁剪：-ss 排在输入之后
        let out = run(
            "manifest:ffmpeg-trim-exact",
            &[
                ("input", &path("in.mp4")),
                ("start", "0.3"),
                ("duration", "0.4"),
                ("preset", "ultrafast"),
                ("output", &path("cut2.mp4")),
            ],
        );
        assert!(out.status.success(), "精确裁剪失败");
        exists("cut2.mp4");

        // 4) 压缩
        let out = run(
            "manifest:ffmpeg-compress",
            &[
                ("input", &path("in.mp4")),
                ("crf", "28"),
                ("preset", "ultrafast"),
                ("output", &path("small.mp4")),
            ],
        );
        assert!(out.status.success(), "压缩失败");
        exists("small.mp4");

        // 5) 改分辨率
        let out = run(
            "manifest:ffmpeg-resize",
            &[
                ("input", &path("in.mp4")),
                ("scale", "scale=-2:240"),
                ("output", &path("small-dim.mp4")),
            ],
        );
        assert!(out.status.success(), "改分辨率失败");
        exists("small-dim.mp4");

        // 6) 提取音频
        let out = run(
            "manifest:ffmpeg-extract-audio",
            &[
                ("input", &path("in.mp4")),
                ("audio_codec", "libmp3lame"),
                ("bitrate", "128k"),
                ("output", &path("audio.mp3")),
            ],
        );
        assert!(out.status.success(), "提取音频失败");
        exists("audio.mp3");

        // 7) 转 GIF：调色板滤镜的语法最容易写错
        let out = run(
            "manifest:ffmpeg-to-gif",
            &[
                ("input", &path("in.mp4")),
                ("duration", "0.5"),
                (
                    "preset",
                    "[0:v] fps=12,scale=160:-1:flags=lanczos,split [a][b];[a] palettegen [p];[b][p] paletteuse",
                ),
                ("output", &path("out.gif")),
            ],
        );
        assert!(out.status.success(), "转 GIF 失败");
        exists("out.gif");

        // 8) 去声音
        let out = run(
            "manifest:ffmpeg-mute",
            &[("input", &path("in.mp4")), ("output", &path("mute.mp4"))],
        );
        assert!(out.status.success(), "去声音失败");
        exists("mute.mp4");

        // 9) 合并画面与声音：-i / -map / -c copy 的顺序
        let out = run(
            "manifest:ffmpeg-merge-av",
            &[
                ("video", &path("mute.mp4")),
                ("audio", &path("music.m4a")),
                ("output", &path("merged.mp4")),
            ],
        );
        assert!(out.status.success(), "合并失败");
        exists("merged.mp4");

        fs::remove_dir_all(&dir).expect("cleanup");
    }

    /// 真跑 ffmpeg（通过后台任务），确认 `-progress` 的结构化进度真的能到调用方手里。
    ///
    /// 这条是拿来定位问题的：命令行里能看到 `-progress pipe:1`，但进度面板可能
    /// 一条都没收到 —— 那就说明是链路的问题，不是 ffmpeg 的问题。
    #[test]
    #[ignore = "真的执行 ffmpeg、写临时文件，默认跳过"]
    fn ffmpeg_progress_events_reach_the_caller() {
        use std::{process::Command as Proc, sync::mpsc::RecvTimeoutError, time::Instant};

        let dir = temp_dir("ffmpeg-progress");
        let video = dir.join("long.mp4");
        let made = Proc::new("ffmpeg")
            .args([
                "-v",
                "error",
                "-f",
                "lavfi",
                "-i",
                "testsrc=duration=30:size=1280x720:rate=30",
                "-c:v",
                "libx264",
                "-preset",
                "ultrafast",
                "-pix_fmt",
                "yuv420p",
                "-y",
            ])
            .arg(&video)
            .status()
            .expect("ffmpeg 应可用");
        assert!(made.success(), "造测试视频失败");

        let discovery = bundled_discovery();
        let tool = by_id(&discovery, "manifest:ffmpeg-compress");
        let action = tool.action.as_ref().expect("带动作");
        let mut values = action.default_values();
        values.set("input", video.to_str().expect("路径"));
        values.set("preset", "slow");
        values.set("output", dir.join("out.mp4").to_str().expect("路径"));
        let argv = action.build_argv(&values).expect("应能构建");

        let job = crate::runtime::spawn_captured(&tool.path, &argv, &dir, "ffmpeg").expect("spawn");

        // 只等第一条进度：能到就说明链路是通的。
        let mut first_progress = None;
        let deadline = Instant::now() + std::time::Duration::from_secs(30);
        while Instant::now() < deadline && first_progress.is_none() {
            match job
                .events
                .recv_timeout(std::time::Duration::from_millis(500))
            {
                Ok(crate::runtime::JobEvent::Progress { key, value }) => {
                    first_progress = Some((key, value));
                }
                Ok(crate::runtime::JobEvent::Done(_)) => break,
                Ok(_) => {}
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => break,
            }
        }

        job.cancel();
        println!("第一条进度: {first_progress:?}");
        assert!(
            first_progress.is_some(),
            "ffmpeg 的 -progress 进度应该能到调用方手里"
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    #[ignore = "真的执行外部程序、写临时文件，默认跳过"]
    fn smoke_run_bundled_actions_on_real_files() {
        use std::process::Command as Proc;

        let dir = temp_dir("manifest-run");
        let png = dir.join("in.png");
        let webp = dir.join("out.webp");
        let clean = dir.join("clean.png");
        let json = dir.join("data.json");
        let json2 = dir.join("data2.json");
        let doc = dir.join("doc.md");
        let html = dir.join("doc.html");
        let archive = dir.join("pack.7z");
        let source = dir.join("src");
        let unpacked = dir.join("unpacked");

        fs::create_dir_all(&source).expect("mkdir");
        fs::create_dir_all(&unpacked).expect("mkdir");
        let source2 = dir.join("src2");
        fs::create_dir_all(&source2).expect("mkdir");
        fs::write(source.join("a.txt"), "hello").expect("write");
        fs::write(source2.join("b.txt"), "second").expect("write");
        fs::write(&json, r#"{"items":[{"name":"a"},{"name":"b"}]}"#).expect("write");
        fs::write(&json2, r#"{"items":[{"name":"c"}]}"#).expect("write");
        fs::write(&doc, "# 标题\n\n正文\n").expect("write");

        // 先造一张真图当输入。
        let made = Proc::new("magick")
            .args(["-size", "64x64", "xc:red"])
            .arg(&png)
            .status()
            .expect("magick 应可用");
        assert!(made.success(), "造测试图失败");

        let discovery = bundled_discovery();
        let run = |id: &str, pairs: &[(&str, &str)], cwd: &std::path::Path| {
            run_built(&discovery, id, pairs, cwd)
        };

        // 图像：转格式 + 缩放 + 去元数据
        let out = run(
            "manifest:magick-convert",
            &[
                ("input", png.to_str().unwrap()),
                ("output", webp.to_str().unwrap()),
                ("resize", "50%"),
                ("quality", "85"),
                ("strip", "true"),
            ],
            &dir,
        );
        assert!(out.status.success(), "magick 转换失败");
        assert!(webp.exists(), "magick 没生成输出文件");

        // 元数据：读得出内容；清除时写新文件、原图仍在
        let out = run(
            "manifest:exif-read",
            &[("file", png.to_str().unwrap()), ("grouped", "true")],
            &dir,
        );
        assert!(out.status.success(), "exiftool 读取失败");
        assert!(
            String::from_utf8_lossy(&out.stdout).contains("PNG"),
            "exiftool 应该报出 PNG 相关标签"
        );

        let out = run(
            "manifest:exif-strip",
            &[
                ("input", png.to_str().unwrap()),
                ("output", clean.to_str().unwrap()),
            ],
            &dir,
        );
        assert!(out.status.success(), "exiftool 清除元数据失败");
        assert!(clean.exists() && png.exists(), "应写新文件且原图仍在");

        // 数据：jq 的表达式必须排在文件前，而且一次喂两个文件（多值参数）
        let both_json = format!("{},{}", json.display(), json2.display());
        let out = run(
            "manifest:jq-query",
            &[
                ("filter", ".items[].name"),
                ("file", both_json.as_str()),
                ("compact", "true"),
                ("raw", "true"),
            ],
            &dir,
        );
        assert!(out.status.success(), "jq 查询失败");
        assert_eq!(
            String::from_utf8_lossy(&out.stdout),
            "a\nb\nc\n",
            "两个文件的内容都要出来（多值参数真的拆开了）"
        );

        // 文档：pandoc 转 HTML
        let out = run(
            "manifest:pandoc-convert",
            &[
                ("input", doc.to_str().unwrap()),
                ("to", "html"),
                ("output", html.to_str().unwrap()),
            ],
            &dir,
        );
        assert!(out.status.success(), "pandoc 转换失败");
        assert!(
            fs::read_to_string(&html)
                .unwrap_or_default()
                .contains("标题"),
            "HTML 里应有正文"
        );

        // 压缩：创建（贴在一起的 -mx9）+ 解压
        let both_sources = format!("{},{}", source.display(), source2.display());
        let out = run(
            "manifest:7z-create",
            &[
                ("archive", archive.to_str().unwrap()),
                ("source", both_sources.as_str()),
                ("level", "9"),
            ],
            &dir,
        );
        assert!(out.status.success(), "7z 创建压缩包失败");
        assert!(archive.exists(), "压缩包没生成");

        let out = run(
            "manifest:7z-extract",
            &[("archive", archive.to_str().unwrap())],
            &unpacked,
        );
        assert!(out.status.success(), "7z 解压失败");
        assert!(
            unpacked.join("src/a.txt").exists() && unpacked.join("src2/b.txt").exists(),
            "两个来源的文件都该在包里"
        );

        fs::remove_dir_all(&dir).expect("cleanup");
    }
}
