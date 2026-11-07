//! `mcp.toml`：默认拒绝的白名单策略与执行限制。
//!
//! 权限判断顺序严格按计划书 P2：
//!
//! ```text
//! 工具是否存在 → 是否显式声明 MCP → 是否被本地配置拒绝 → 是否在白名单
//!   → 依赖是否齐全 → 执行模式是否允许 → 风险/副作用是否符合策略
//!   → 参数是否合法 → 路径是否越界 → 是否具备确认能力 → 授权
//! ```
//!
//! **fail-closed**：文件不存在、写错、字段名拼错都退回全拒绝的默认配置，
//! 并在 `Loaded::problem` 里说明原因。绝不会因为「配置读不动」而把整个工具箱放开。

use std::{
    fs,
    path::{Component, Path, PathBuf},
    time::Duration,
};

use serde::Deserialize;

use crate::{
    config::{self, Loaded},
    model::{ArgKind, ArgumentValue, Danger, RunMode, ToolDefinition, ToolEffects, TypedValues},
    security::{approval, effects},
};

/// P5-01：Recipe Loader 产出的工具在目录里的 provider 标签。
///
/// 策略层用它识别「参数是纯字符串替换进 argv 的 Recipe 工具」，对这类工具的
/// **所有**参数值做选项注入拒绝（S-02）：值以 `-` 开头就可能被目标程序当成
/// 选项解析，改变命令语义。误把自己标成这个标签只会让自己更受限（fail-safe），
/// 所以拿 provider 当判别式是安全的。
pub const RECIPE_PROVIDER: &str = "Recipe";

/// 配置模板：第一次跑 MCP 时写出来，让人知道有这个文件。
const MCP_TEMPLATE: &str = r#"# Toolbox Hub 的 MCP 权限配置。
# 缺省全拒绝：这里点名的工具 + Manifest/脚本自己声明了 mcp，才会出现在 AI 的工具列表里。

[mcp]
enabled = true

[exposure]
# deny = 只有 allow 里的 id 会被暴露（推荐）；allow = 所有「已声明 mcp」的工具。
default = "deny"
allow = [
    "manifest:disk-space",
    "manifest:boot-errors",
    "manifest:block-devices",
    "manifest:pci-devices",
    # 包管理域的只读查询（P5-03）；安装/卸载/更新等写动作没有 mcp 声明，不在暴露面里。
    "manifest:pkg-search",
    "manifest:pkg-status",
    "manifest:pkg-info",
    "manifest:pkg-updates",
    "manifest:pkg-health",
]
deny = []

[execution]
# 改动下面任何一项，都等于主动放宽 AI 能做的事，请逐条确认。
allow_safe = true
allow_caution = false
allow_destructive = false
allow_interactive = false
allow_privileged = false
allow_system_mutation = false
allow_network = false
allow_filesystem_write = false
allow_shell = false

[limits]
default_timeout_secs = 60
max_timeout_secs = 300
max_output_bytes = 262144
max_concurrent = 2

[paths]
# 空 = 不限制工作目录和路径参数。填上以后，路径参数必须落在这些目录里。
allowed_dirs = []

# P5-01：精选 Recipe 目录（可选）。目录里要有 index.json（build-featured.py 产出的
# 那份），每条 Recipe 按 SHA-256 校验、经 validate_recipe 后注册成 id 形如
# recipe:<名字> 的工具——仍要在上面 [exposure] allow 里逐个点名才会真正暴露。
# 填相对路径时相对服务端进程的启动目录；建议写绝对路径。
# dirs = ["/home/me/code/toolbox-hub/registry/featured"]

[audit]
enabled = true
# 默认写到数据目录的 mcp-audit.jsonl；要换位置就写绝对路径。
# path = "/home/me/.local/share/toolbox-hub/mcp-audit.jsonl"
"#;

/// `[mcp]`。缺配置时 `enabled = false`（bool 的默认值就是 false）：
/// 不是「放开」，是「什么都没授权」。
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct McpSection {
    pub enabled: bool,
}

/// `[exposure] default` 的取值。
#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ExposureDefault {
    /// 只有 `allow` 里的 id 可暴露（推荐，也是缺省）。
    #[default]
    Deny,
    /// 所有「已声明 mcp」的工具都可暴露（仍受 `deny` 与执行策略约束）。
    Allow,
}

/// `[exposure]`
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ExposureSection {
    pub default: ExposureDefault,
    pub allow: Vec<String>,
    pub deny: Vec<String>,
}

/// `[execution]`
#[derive(Clone, Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ExecutionSection {
    pub allow_safe: bool,
    pub allow_caution: bool,
    pub allow_destructive: bool,
    pub allow_interactive: bool,
    pub allow_privileged: bool,
    pub allow_system_mutation: bool,
    pub allow_network: bool,
    pub allow_filesystem_write: bool,
    pub allow_shell: bool,
}

impl Default for ExecutionSection {
    fn default() -> Self {
        Self {
            allow_safe: true,
            allow_caution: false,
            allow_destructive: false,
            allow_interactive: false,
            allow_privileged: false,
            allow_system_mutation: false,
            allow_network: false,
            allow_filesystem_write: false,
            allow_shell: false,
        }
    }
}

/// `[limits]`
#[derive(Clone, Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LimitsSection {
    pub default_timeout_secs: u64,
    pub max_timeout_secs: u64,
    pub max_output_bytes: usize,
    pub max_concurrent: usize,
}

impl Default for LimitsSection {
    fn default() -> Self {
        Self {
            default_timeout_secs: 60,
            max_timeout_secs: 300,
            max_output_bytes: 262_144,
            max_concurrent: 2,
        }
    }
}

/// `[paths]`
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PathsSection {
    /// 空 = 不限制；非空 = 工作目录与所有路径参数都必须落在其中。
    pub allowed_dirs: Vec<PathBuf>,
}

/// `[recipes]`（P5-01）：要加载精选 Recipe 的目录。
///
/// 空（缺省）＝ 一个 Recipe 都不加载。这里只是「来源」，真正的授权仍在
/// `[exposure] allow` 里逐条点名 —— 和 Manifest 的 `mcp` 声明同一套语义，
/// 不为 Recipe 造新机制。
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RecipesSection {
    pub dirs: Vec<PathBuf>,
}

/// `[audit]`
#[derive(Clone, Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AuditSection {
    pub enabled: bool,
    pub path: Option<PathBuf>,
}

impl Default for AuditSection {
    fn default() -> Self {
        Self {
            enabled: true,
            path: None,
        }
    }
}

/// 整份 `mcp.toml`。
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct McpConfig {
    pub mcp: McpSection,
    pub exposure: ExposureSection,
    pub execution: ExecutionSection,
    pub limits: LimitsSection,
    pub paths: PathsSection,
    pub recipes: RecipesSection,
    pub audit: AuditSection,
}

impl McpConfig {
    /// 默认位置：`~/.config/toolbox-hub/mcp.toml`。
    pub fn path() -> PathBuf {
        config::config_dir().join("mcp.toml")
    }

    /// 从默认位置读；读不出来就全拒绝 + 一句提示。
    pub fn load() -> Loaded<Self> {
        Self::load_from(&Self::path())
    }

    /// 从指定文件读（测试与自定义配置目录用）。
    pub fn load_from(path: &Path) -> Loaded<Self> {
        let Ok(text) = fs::read_to_string(path) else {
            return Loaded {
                value: Self::default(),
                problem: None,
            };
        };
        match toml::from_str::<Self>(&text) {
            Ok(config) => Loaded {
                value: config,
                problem: None,
            },
            Err(error) => Loaded {
                value: Self::default(),
                problem: Some(format!(
                    "{} 读不动（已回退到全拒绝）：{}",
                    path.display(),
                    error.message()
                )),
            },
        }
    }

    /// 第一次跑写一份带注释的模板；已存在则一个字节都不动。
    pub fn ensure_template() -> bool {
        let path = Self::path();
        if path.exists() {
            return false;
        }
        let Some(parent) = path.parent() else {
            return false;
        };
        if fs::create_dir_all(parent).is_err() {
            return false;
        }
        fs::write(&path, MCP_TEMPLATE).is_ok()
    }

    /// 默认超时：夹在 1 秒与 `max_timeout_secs` 之间。
    pub fn timeout(&self) -> Option<Duration> {
        let max = self.limits.max_timeout_secs.max(1);
        let secs = self.limits.default_timeout_secs.clamp(1, max);
        Some(Duration::from_secs(secs))
    }

    /// 单次调用最多回传多少字节输出。
    pub fn max_output_bytes(&self) -> usize {
        self.limits.max_output_bytes.max(1)
    }

    /// 并发上限（至少 1）。
    pub fn max_concurrent(&self) -> usize {
        self.limits.max_concurrent.max(1)
    }

    /// 审计日志位置。
    pub fn audit_path(&self) -> PathBuf {
        self.audit
            .path
            .clone()
            .unwrap_or_else(|| config::data_dir().join("mcp-audit.jsonl"))
    }

    /// P5-01：要加载精选 Recipe 的目录（空 = 不加载任何 Recipe 工具）。
    pub fn recipe_dirs(&self) -> &[PathBuf] {
        &self.recipes.dirs
    }
}

/// 一次授权通过后交给执行器的约束。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Authorization {
    pub tool_id: String,
    pub timeout: Option<Duration>,
    pub max_output_bytes: usize,
    pub effects: ToolEffects,
}

/// 为什么拒绝。每个分支都有稳定的 `code()`，供 CLI / MCP 映射错误。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DenyReason {
    McpDisabled,
    NotExposed,
    Denylisted,
    NotAllowed,
    DependenciesMissing(Vec<String>),
    UnsupportedMode(&'static str),
    DangerSafeDisabled,
    DangerCaution,
    Destructive,
    Privileged,
    SystemMutation,
    Network,
    FilesystemWrite,
    ShellRunner,
    ConfirmationUnsupported,
    InvalidArguments(String),
    PathOutsideAllowedDirs(String),
    OptionLikeValue(String),
}

impl DenyReason {
    pub fn code(&self) -> &'static str {
        match self {
            DenyReason::McpDisabled => "mcp_disabled",
            DenyReason::NotExposed => "not_exposed",
            DenyReason::Denylisted => "denylisted",
            DenyReason::NotAllowed => "not_allowed",
            DenyReason::DependenciesMissing(_) => "dependencies_missing",
            DenyReason::UnsupportedMode(_) => "unsupported_mode",
            DenyReason::DangerSafeDisabled => "safe_disabled",
            DenyReason::DangerCaution => "caution_denied",
            DenyReason::Destructive => "destructive_denied",
            DenyReason::Privileged => "privileged_denied",
            DenyReason::SystemMutation => "system_mutation_denied",
            DenyReason::Network => "network_denied",
            DenyReason::FilesystemWrite => "write_denied",
            DenyReason::ShellRunner => "shell_denied",
            DenyReason::ConfirmationUnsupported => "confirmation_unsupported",
            DenyReason::InvalidArguments(_) => "invalid_arguments",
            DenyReason::PathOutsideAllowedDirs(_) => "path_denied",
            DenyReason::OptionLikeValue(_) => "option_like_value",
        }
    }

    pub fn message(&self, tool_id: &str) -> String {
        match self {
            DenyReason::McpDisabled => String::from("mcp.toml 未启用 MCP"),
            DenyReason::NotExposed => format!("{tool_id} 没有声明 mcp 暴露"),
            DenyReason::Denylisted => format!("{tool_id} 在本地 deny 名单里"),
            DenyReason::NotAllowed => format!("{tool_id} 不在本地 allow 名单里"),
            DenyReason::DependenciesMissing(missing) => {
                format!("{tool_id} 缺依赖：{}", missing.join("、"))
            }
            DenyReason::UnsupportedMode(mode) => {
                format!("{tool_id} 的执行方式是 {mode}，无终端环境只能跑 capture")
            }
            DenyReason::DangerSafeDisabled => format!("{tool_id}：策略未允许 safe 动作"),
            DenyReason::DangerCaution => format!("{tool_id} 是 caution，默认禁止"),
            DenyReason::Destructive => format!("{tool_id} 被判定为破坏性操作，默认禁止"),
            DenyReason::Privileged => format!("{tool_id} 需要提权，默认禁止"),
            DenyReason::SystemMutation => format!("{tool_id} 会修改系统状态，默认禁止"),
            DenyReason::Network => format!("{tool_id} 会访问网络，默认禁止"),
            DenyReason::FilesystemWrite => format!("{tool_id} 会写文件，默认禁止"),
            DenyReason::ShellRunner => format!("{tool_id} 是通用命令执行器，默认禁止"),
            DenyReason::ConfirmationUnsupported => {
                format!("{tool_id} 需要人工确认，但客户端不支持确认能力")
            }
            DenyReason::InvalidArguments(reason) => {
                format!("{tool_id} 的参数不合法：{reason}")
            }
            DenyReason::PathOutsideAllowedDirs(path) => {
                format!("{tool_id} 的路径不在授权范围内：{path}")
            }
            DenyReason::OptionLikeValue(value) => {
                format!("{tool_id} 的参数值不能以 - 开头（疑似选项注入）：{value}")
            }
        }
    }
}

/// 服务端策略。**所有拒绝都在启动子进程之前发生。**
#[derive(Clone, Debug)]
pub struct SafetyPolicy {
    pub config: McpConfig,
}

impl SafetyPolicy {
    pub fn new(config: McpConfig) -> Self {
        Self { config }
    }

    /// 从默认位置加载（读不动时就是全拒绝，附带的问题交给调用方记日志）。
    pub fn from_file() -> (Self, Option<String>) {
        let loaded = McpConfig::load();
        (Self::new(loaded.value), loaded.problem)
    }

    /// `tools/list` 用：只判断「能不能出现在列表里」，不涉及具体参数。
    pub fn authorize_for_list(
        &self,
        tool: &ToolDefinition,
        client: approval::ClientCapabilities,
    ) -> Result<ToolEffects, DenyReason> {
        self.common(tool, client)
    }

    /// 一次调用前的完整授权。
    pub fn authorize(
        &self,
        tool: &ToolDefinition,
        values: &TypedValues,
        cwd: &Path,
        client: approval::ClientCapabilities,
    ) -> Result<Authorization, DenyReason> {
        let effects = self.common(tool, client)?;
        self.check_arguments(tool, values)?;
        self.check_paths(tool, values, cwd)?;
        // S-02（P5-01 一并处理）：Recipe 工具的参数是纯字符串替换进 argv，
        // 值以 `-` 开头就会被目标程序当成选项解析（参数注入，能改变命令语义）。
        // 这类工具的所有参数值一律拒绝「长得像选项」的取值 —— 与路径参数的
        // 选项注入检查同一套语义，只是范围扩大到 Recipe 的全部参数。
        if tool.provider == RECIPE_PROVIDER {
            self.check_recipe_values(values)?;
        }

        Ok(Authorization {
            tool_id: tool.id.clone(),
            timeout: self.config.timeout(),
            max_output_bytes: self.config.max_output_bytes(),
            effects,
        })
    }

    /// 与具体参数无关的那几层。
    fn common(
        &self,
        tool: &ToolDefinition,
        client: approval::ClientCapabilities,
    ) -> Result<ToolEffects, DenyReason> {
        if !self.config.mcp.enabled {
            return Err(DenyReason::McpDisabled);
        }
        if tool.mcp.is_none() {
            return Err(DenyReason::NotExposed);
        }
        if self.config.exposure.deny.contains(&tool.id) {
            return Err(DenyReason::Denylisted);
        }
        if self.config.exposure.default == ExposureDefault::Deny
            && !self.config.exposure.allow.contains(&tool.id)
        {
            return Err(DenyReason::NotAllowed);
        }
        if !tool.ready {
            return Err(DenyReason::DependenciesMissing(tool.missing_deps.clone()));
        }
        if tool.mode != RunMode::Capture {
            return Err(DenyReason::UnsupportedMode(mode_name(tool.mode)));
        }

        let effects = effects::infer(tool);
        let rules = &self.config.execution;
        let denied = if tool.danger.needs_confirm() && !rules.allow_caution {
            // 先给「caution 默认禁止」一个明确的原因，别被它附带的「写文件」
            // 推断结果抢先报成 write_denied —— 两者都是拒绝，但诊断要说人话。
            Some(DenyReason::DangerCaution)
        } else if effects.arbitrary_command && !rules.allow_shell {
            Some(DenyReason::ShellRunner)
        } else if effects.requires_privilege && !rules.allow_privileged {
            Some(DenyReason::Privileged)
        } else if effects.destructive && !rules.allow_destructive {
            Some(DenyReason::Destructive)
        } else if effects.system_mutation && !rules.allow_system_mutation {
            Some(DenyReason::SystemMutation)
        } else if effects.network_access && !rules.allow_network {
            Some(DenyReason::Network)
        } else if effects.filesystem_write && !rules.allow_filesystem_write {
            Some(DenyReason::FilesystemWrite)
        } else if effects.interactive && !rules.allow_interactive {
            Some(DenyReason::UnsupportedMode(mode_name(tool.mode)))
        } else if tool.danger == Danger::Safe && !rules.allow_safe {
            Some(DenyReason::DangerSafeDisabled)
        } else {
            None
        };
        if let Some(reason) = denied {
            return Err(reason);
        }

        let requirement = approval::requirement(tool, effects);
        if !approval::check(requirement, client) {
            return Err(DenyReason::ConfirmationUnsupported);
        }
        Ok(effects)
    }

    /// 参数契约：未声明的键、类型错误、多值误用都在这里拦掉。
    /// 必填项由 `Action::build_argv_typed` 在构造 argv 时检查（同样早于启动进程）。
    fn check_arguments(
        &self,
        tool: &ToolDefinition,
        values: &TypedValues,
    ) -> Result<(), DenyReason> {
        if let Some(action) = &tool.action {
            crate::service::argument::validate(action, values)
                .map_err(DenyReason::InvalidArguments)?;
        } else if !values.is_empty() {
            return Err(DenyReason::InvalidArguments(String::from(
                "这件工具没有参数契约，不接受附加参数",
            )));
        }
        Ok(())
    }

    /// P2-09：`[paths] allowed_dirs` 非空时，工作目录与所有路径参数都必须在范围内。
    /// 同时无条件拒绝「看起来像选项」的路径值 —— 值虽是单个 argv 元素，
    /// 以 `-` 开头仍可能被目标程序当成选项解析（S-02 那类语义注入）。
    fn check_paths(
        &self,
        tool: &ToolDefinition,
        values: &TypedValues,
        cwd: &Path,
    ) -> Result<(), DenyReason> {
        let Some(action) = &tool.action else {
            return Ok(());
        };
        let allowed: Vec<PathBuf> = self
            .config
            .paths
            .allowed_dirs
            .iter()
            .map(|dir| lexical_normalize(dir))
            .collect();
        let cwd_abs = lexical_normalize(cwd);
        if !allowed.is_empty() && !within_any(&cwd_abs, &allowed) {
            return Err(DenyReason::PathOutsideAllowedDirs(
                cwd.display().to_string(),
            ));
        }

        for argument in action
            .arguments
            .iter()
            .filter(|argument| argument.kind == ArgKind::Path || argument.dir_only)
        {
            let Some(value) = values.get(&argument.key) else {
                continue;
            };
            for raw in value_strings(value) {
                let raw = raw.trim();
                if raw.is_empty() {
                    continue;
                }
                if raw.starts_with('-') {
                    return Err(DenyReason::OptionLikeValue(raw.to_string()));
                }
                if allowed.is_empty() {
                    continue;
                }
                let path = Path::new(raw);
                let resolved = if path.is_absolute() {
                    lexical_normalize(path)
                } else {
                    lexical_normalize(&cwd_abs.join(path))
                };
                if !within_any(&resolved, &allowed) {
                    return Err(DenyReason::PathOutsideAllowedDirs(raw.to_string()));
                }
            }
        }
        Ok(())
    }

    /// S-02：Recipe 工具的全部参数值都不允许以 `-` 开头。
    ///
    /// Recipe 的参数声明由 Loader 生成（位置替换、没有 flag），一个值变成
    /// 独立的 argv 元素后，目标程序分不清它是值还是选项。首期精选 Recipe
    /// 的参数（路径、包名、数量、URL）没有一个需要以 `-` 开头，所以宁可
    /// 全拒：这一类「改命令语义」的注入面直接清零。
    fn check_recipe_values(&self, values: &TypedValues) -> Result<(), DenyReason> {
        for (_, value) in values.iter() {
            // 未声明的键由 check_arguments 拦截；这里只看取值本身。
            for raw in value_strings(value) {
                let raw = raw.trim();
                if raw.starts_with('-') {
                    return Err(DenyReason::OptionLikeValue(raw.to_string()));
                }
            }
        }
        Ok(())
    }
}

/// 一个参数取值里的全部字符串项（文本 → 单项；列表 → 逐项；开关 → 空）。
fn value_strings(value: &ArgumentValue) -> Vec<String> {
    match value {
        ArgumentValue::Text(text) => vec![text.clone()],
        ArgumentValue::List(items) => items.clone(),
        ArgumentValue::Bool(_) => Vec::new(),
    }
}

fn mode_name(mode: RunMode) -> &'static str {
    match mode {
        RunMode::Interactive => "interactive",
        RunMode::Capture => "capture",
        RunMode::Native => "native",
    }
}

/// 纯词法归一化（不碰文件系统：待执行的输出文件往往还不存在）。
fn lexical_normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() {
                    out.push("..");
                }
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

fn within_any(path: &Path, allowed: &[PathBuf]) -> bool {
    allowed.iter().any(|dir| path.starts_with(dir))
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use super::{DenyReason, ExposureDefault, McpConfig, SafetyPolicy, lexical_normalize};
    use crate::{
        model::{
            Action, ArgKind, ArgPlacement, Argument, Danger, Domain, McpExposure, RunMode,
            ToolDefinition, ToolEffects, TypedValues,
        },
        security::approval::ClientCapabilities,
    };

    fn config(extra: &str) -> McpConfig {
        // 传进来的 extra 可以整段替换 [exposure]；没传就用「只放行 test:echo」的缺省。
        let exposure = if extra.contains("[exposure]") {
            ""
        } else {
            "[exposure]\ndefault = \"deny\"\nallow = [\"test:echo\"]\ndeny = []\n"
        };
        let text = format!("[mcp]\nenabled = true\n\n{exposure}\n{extra}");
        toml::from_str(&text).unwrap_or_else(|error| panic!("配置该能解析: {error}"))
    }

    fn tool(id: &str) -> ToolDefinition {
        let mut tool = ToolDefinition {
            mcp: None,
            id: id.to_string(),
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
            action: Some(Action {
                program: String::from("echo"),
                base_argv: Vec::new(),
                arguments: vec![Argument {
                    key: String::from("path"),
                    label: String::from("路径"),
                    kind: ArgKind::Path,
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
            path: PathBuf::from("/bin/echo"),
            ready: true,
        };
        tool.mcp = Some(McpExposure::default());
        tool
    }

    fn authorize(policy: &SafetyPolicy, tool: &ToolDefinition) -> Result<(), DenyReason> {
        policy
            .authorize(
                tool,
                &TypedValues::new(),
                Path::new("/tmp"),
                ClientCapabilities::headless(),
            )
            .map(|_| ())
    }

    #[test]
    fn a_missing_or_broken_config_fails_closed() {
        let missing = McpConfig::load_from(Path::new("/definitely/not/here/mcp.toml"));
        assert!(missing.problem.is_none());
        assert!(!missing.value.mcp.enabled);
        assert!(missing.value.exposure.allow.is_empty());

        let dir = std::env::temp_dir().join(format!(
            "toolbox-hub-mcp-broken-{}-{}",
            std::process::id(),
            "cfg"
        ));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("mcp.toml");
        std::fs::write(&path, "[exposure]\ndefualt = \"allow\"\n").expect("write");
        let broken = McpConfig::load_from(&path);
        assert!(broken.problem.is_some(), "拼错的字段要报出来");
        assert!(!broken.value.mcp.enabled, "读不动就全拒绝");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_template_parses_and_exposes_only_nine_read_only_tools() {
        let config: McpConfig = toml::from_str(super::MCP_TEMPLATE).expect("模板要能解析");
        assert!(config.mcp.enabled);
        assert_eq!(config.exposure.default, ExposureDefault::Deny);
        // 四个系统巡检 + P5-03 的五个包管理只读查询；写动作（pkg-center 等）没有
        // mcp 声明，不在暴露面里。
        assert_eq!(config.exposure.allow.len(), 9);
        for id in [
            "manifest:pkg-search",
            "manifest:pkg-status",
            "manifest:pkg-info",
            "manifest:pkg-updates",
            "manifest:pkg-health",
        ] {
            assert!(
                config.exposure.allow.contains(&id.to_string()),
                "模板缺 {id}: {:?}",
                config.exposure.allow
            );
        }
        assert!(config.recipes.dirs.is_empty(), "Recipe 目录缺省为空");
        assert!(!config.execution.allow_caution);
        assert!(!config.execution.allow_destructive);
        assert_eq!(config.timeout(), Some(std::time::Duration::from_secs(60)));
    }

    #[test]
    fn default_deny_requires_an_explicit_allowlist_entry() {
        let policy = SafetyPolicy::new(config(""));
        assert_eq!(authorize(&policy, &tool("test:echo")), Ok(()));
        assert_eq!(
            authorize(&policy, &tool("test:other")),
            Err(DenyReason::NotAllowed)
        );
    }

    #[test]
    fn a_denylist_entry_wins_even_when_allowlisted() {
        let policy = SafetyPolicy::new(config(
            "[exposure]\ndefault = \"deny\"\nallow = [\"test:echo\"]\ndeny = [\"test:echo\"]\n",
        ));
        assert_eq!(
            authorize(&policy, &tool("test:echo")),
            Err(DenyReason::Denylisted)
        );
    }

    #[test]
    fn a_tool_without_an_mcp_declaration_is_never_authorized() {
        let policy = SafetyPolicy::new(config("[exposure]\ndefault = \"allow\"\n"));
        let mut bare = tool("test:echo");
        bare.mcp = None;
        assert_eq!(authorize(&policy, &bare), Err(DenyReason::NotExposed));
    }

    #[test]
    fn interactive_native_caution_and_missing_deps_are_all_denied() {
        let policy = SafetyPolicy::new(config("[exposure]\ndefault = \"allow\"\n"));
        let headless = ClientCapabilities::headless();

        let mut interactive = tool("test:echo");
        interactive.mode = RunMode::Interactive;
        assert_eq!(
            policy.authorize_for_list(&interactive, headless).err(),
            Some(DenyReason::UnsupportedMode("interactive"))
        );

        let mut native = tool("test:echo");
        native.mode = RunMode::Native;
        assert_eq!(
            policy.authorize_for_list(&native, headless).err(),
            Some(DenyReason::UnsupportedMode("native"))
        );

        let mut caution = tool("test:echo");
        caution.danger = Danger::Caution;
        assert_eq!(authorize(&policy, &caution), Err(DenyReason::DangerCaution));

        let mut unready = tool("test:echo");
        unready.ready = false;
        unready.missing_deps = vec![String::from("ffmpeg")];
        assert_eq!(
            authorize(&policy, &unready),
            Err(DenyReason::DependenciesMissing(vec![String::from(
                "ffmpeg"
            )]))
        );
    }

    /// 伪造 `danger = safe` + 声明「无副作用」也绕不过独立推断。
    #[test]
    fn forged_safe_metadata_is_still_denied() {
        let policy = SafetyPolicy::new(config("[exposure]\ndefault = \"allow\"\n"));
        let mut forged = tool("test:echo");
        let action = forged.action.as_mut().expect("action");
        action.program = String::from("sudo");
        action.base_argv = vec![
            String::from("rm"),
            String::from("-rf"),
            String::from("/tmp/x"),
        ];
        forged.danger = Danger::Safe;
        forged.mcp = Some(McpExposure {
            effects: Some(ToolEffects::default()),
            ..McpExposure::default()
        });

        assert_eq!(authorize(&policy, &forged), Err(DenyReason::Privileged));
    }

    #[test]
    fn a_shell_runner_is_denied_by_default() {
        let policy = SafetyPolicy::new(config("[exposure]\ndefault = \"allow\"\n"));
        let mut shell = tool("test:echo");
        let action = shell.action.as_mut().expect("action");
        action.program = String::from("sh");
        action.base_argv = vec![String::from("-c"), String::from("echo hi")];
        assert_eq!(authorize(&policy, &shell), Err(DenyReason::ShellRunner));
    }

    #[test]
    fn unknown_arguments_are_rejected_before_execution() {
        let policy = SafetyPolicy::new(config(""));
        let mut values = TypedValues::new();
        values.set_text("nope", "x");
        let error = policy
            .authorize(
                &tool("test:echo"),
                &values,
                Path::new("/tmp"),
                ClientCapabilities::headless(),
            )
            .expect_err("未声明的键");
        assert_eq!(error.code(), "invalid_arguments");
    }

    #[test]
    fn path_whitelist_blocks_escape_and_option_injection() {
        let policy = SafetyPolicy::new(config(
            "[paths]\nallowed_dirs = [\"/home/emo/Downloads\"]\n",
        ));
        let tool = tool("test:echo");
        let cwd = Path::new("/home/emo/Downloads");

        let mut inside = TypedValues::new();
        inside.set_text("path", "a.txt");
        assert!(
            policy
                .authorize(&tool, &inside, cwd, ClientCapabilities::headless())
                .is_ok()
        );

        let mut outside = TypedValues::new();
        outside.set_text("path", "/etc/passwd");
        assert_eq!(
            policy
                .authorize(&tool, &outside, cwd, ClientCapabilities::headless())
                .err()
                .map(|reason| reason.code()),
            Some("path_denied")
        );

        let mut traversal = TypedValues::new();
        traversal.set_text("path", "../.ssh/id_rsa");
        assert_eq!(
            policy
                .authorize(&tool, &traversal, cwd, ClientCapabilities::headless())
                .err()
                .map(|reason| reason.code()),
            Some("path_denied")
        );

        let mut option = TypedValues::new();
        option.set_text("path", "--help");
        assert_eq!(
            policy
                .authorize(&tool, &option, cwd, ClientCapabilities::headless())
                .err()
                .map(|reason| reason.code()),
            Some("option_like_value")
        );

        // 工作目录本身也必须在范围内。
        assert_eq!(
            policy
                .authorize(
                    &tool,
                    &TypedValues::new(),
                    Path::new("/tmp"),
                    ClientCapabilities::headless()
                )
                .err()
                .map(|reason| reason.code()),
            Some("path_denied")
        );
    }

    #[test]
    fn limits_are_clamped_and_lexical_normalization_resolves_dot_dot() {
        let config = config(
            "[limits]\ndefault_timeout_secs = 9999\nmax_timeout_secs = 30\nmax_output_bytes = 0\nmax_concurrent = 0\n",
        );
        assert_eq!(config.timeout(), Some(std::time::Duration::from_secs(30)));
        assert_eq!(config.max_output_bytes(), 1);
        assert_eq!(config.max_concurrent(), 1);
        assert_eq!(
            lexical_normalize(Path::new("/a/b/../c/./d")),
            PathBuf::from("/a/c/d")
        );
    }

    /// S-02（P5-01）：Recipe 工具（provider = Recipe）的参数值不允许以 `-` 开头
    /// —— 那是纯字符串替换进 argv 的注入面。Manifest 工具的普通文本参数不受影响。
    #[test]
    fn recipe_parameter_values_reject_option_like_strings() {
        let policy = SafetyPolicy::new(config("[exposure]\ndefault = \"allow\"\n"));
        let mut recipe = tool("recipe:dns-lookup");
        recipe.provider = String::from(super::RECIPE_PROVIDER);
        let action = recipe.action.as_mut().expect("action");
        action.arguments[0].kind = ArgKind::Text;

        let mut values = TypedValues::new();
        values.set_text("path", "example.org");
        assert!(
            policy
                .authorize(
                    &recipe,
                    &values,
                    Path::new("/tmp"),
                    ClientCapabilities::headless()
                )
                .is_ok(),
            "正常值放行"
        );

        for nasty in ["--help", "-n", "-rf"] {
            let mut values = TypedValues::new();
            values.set_text("path", nasty);
            let reason = policy
                .authorize(
                    &recipe,
                    &values,
                    Path::new("/tmp"),
                    ClientCapabilities::headless(),
                )
                .expect_err("选项样式的值要被拒");
            assert_eq!(reason.code(), "option_like_value", "{nasty}");
        }

        // 同样的值在 Manifest 工具（provider 不是 Recipe）的文本参数上不拦：
        // 那条路上选项注入只在路径参数上拒绝，是既有语义，本批不改。
        let mut manifest_tool = tool("manifest:demo");
        let action = manifest_tool.action.as_mut().expect("action");
        action.arguments[0].kind = ArgKind::Text;
        let mut values = TypedValues::new();
        values.set_text("path", "-n");
        assert!(
            policy
                .authorize(
                    &manifest_tool,
                    &values,
                    Path::new("/tmp"),
                    ClientCapabilities::headless()
                )
                .is_ok(),
            "Manifest 工具的文本参数维持既有语义"
        );
    }
}
