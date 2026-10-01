use std::path::PathBuf;

use crate::model::{Action, Domain};

/// 执行方式：决定了跑这个工具时 TUI 怎么办。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum RunMode {
    /// **接管终端**：挂起 TUI，把终端整个交给它。给交互式工具用 ——
    /// FFTools 那批 fzf 菜单、btop、以及下大文件时想看进度条的 yt-dlp/aria2c。
    #[default]
    Interactive,
    /// **捕获输出**：留在 TUI 里把 stdout/stderr 收下来，跑完进内置输出视图。
    /// 给「跑完吐一段文字」的工具用：jq、pandoc、ImageMagick、mediainfo…
    Capture,
}

impl RunMode {
    /// 解析注解里的取值。
    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_lowercase().as_str() {
            "interactive" | "接管" => Some(RunMode::Interactive),
            "capture" | "捕获" => Some(RunMode::Capture),
            _ => None,
        }
    }
}

/// 危险程度：决定执行前要不要再确认一次。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Danger {
    /// 只读，或者只写用户自己点名的输出文件。
    #[default]
    Safe,
    /// 会覆盖 / 改动用户没点名的文件（例如解压覆盖同名文件、往已有包里追加）。
    Caution,
}

impl Danger {
    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_lowercase().as_str() {
            "safe" | "安全" => Some(Danger::Safe),
            "caution" | "careful" | "注意" => Some(Danger::Caution),
            _ => None,
        }
    }

    /// 执行前是否需要用户再按一次确认。
    pub fn needs_confirm(self) -> bool {
        self != Danger::Safe
    }

    pub fn label(self) -> &'static str {
        match self {
            Danger::Safe => "安全",
            Danger::Caution => "注意",
        }
    }
}

/// 一个可执行的工具箱条目。
///
/// 由 [`crate::providers::Provider`] 产出、[`crate::registry::Registry`] 聚合，
/// UI 层只读消费。字段刻意做成 Provider 无关：
/// FFTools 的「转码 / 字幕 / 分析」等分类一律降级为 [`ToolDefinition::tags`]。
///
/// 工具箱里的一件「工具」通常是**一个可执行文件**：直接执行它，工作目录是 Toolbox
/// 启动时所在的那个目录。带 [`ToolDefinition::action`] 的工具则不同：它包装的是
/// 已装 CLI，要先在表单里填参数，再由 [`crate::model::Action::build_argv`] 生成 argv。
#[derive(Clone, Debug)]
pub struct ToolDefinition {
    /// 全局唯一 id，形如 `fftools:fzf-trim-video`。
    pub id: String,
    /// 展示名，通常等于可执行文件名。
    pub name: String,
    /// 来源 Provider 的展示名，例如 `FFTools`。
    pub provider: String,
    /// 所属一级域。
    pub domain: Domain,
    /// Provider 自定义的二级分类（页内筛选条用）。可以为空。
    pub tags: Vec<String>,
    /// 一句话说明，缺省为 `-`。
    pub summary: String,
    pub input: Option<String>,
    pub output: Option<String>,
    pub features: Option<String>,
    /// 依赖的外部命令。
    pub requires: Vec<String>,
    /// [`ToolDefinition::requires`] 里当前找不到的那些。
    ///
    /// 详情区靠它说出「到底缺什么」，而不是只标一个「依赖缺失」。
    pub missing_deps: Vec<String>,
    /// 依赖缺失时给用户看的安装办法（脚本里的 `install=` 注解）。没写时为 `None`。
    pub install_hint: Option<String>,
    /// 需要先填参数才能跑的动作（`Curated` Provider 的产物）。
    ///
    /// `None` 表示这件工具本身就是个可执行文件、直接跑就行；`Some` 表示要先进表单，
    /// 由 [`Action::build_argv`] 生成 argv。
    pub action: Option<Action>,
    /// 跑它的时候 TUI 怎么办（见 [`RunMode`]）。
    pub mode: RunMode,
    /// 危险程度（见 [`Danger`]）。
    pub danger: Danger,
    /// 可执行文件路径。详情区展示它，[`crate::registry::Registry::reload`]
    /// 也用它做跨 Provider 的同一份脚本去重。
    pub path: PathBuf,
    /// [`ToolDefinition::requires`] 是否全部满足。
    pub ready: bool,
}

impl ToolDefinition {
    /// 详情区展示用的输入描述，元数据缺失时按域兜底。
    pub fn input_or_default(&self) -> &str {
        self.input.as_deref().unwrap_or(match self.domain {
            Domain::Media => "视频 / 音频 / 图片文件",
            Domain::Image => "图片文件",
            Domain::System => "无需输入",
            Domain::Network => "URL / 目标地址",
            Domain::Dev => "项目目录 / 源码文件",
            Domain::Tools => "按工具提示选择输入",
            Domain::Packages => "包名 / 文件路径 / 关键词",
        })
    }

    /// 详情区展示用的输出描述，元数据缺失时按域兜底。
    pub fn output_or_default(&self) -> &str {
        self.output.as_deref().unwrap_or(match self.domain {
            Domain::Media => "处理后的媒体文件",
            Domain::Image => "处理后的图片文件",
            Domain::System => "终端输出 / 系统状态",
            Domain::Network => "下载或传输结果",
            Domain::Dev => "构建产物 / 命令输出",
            Domain::Tools => "按工具操作流程生成",
            Domain::Packages => "包信息 / 安装结果",
        })
    }

    /// 特性描述，缺失时退化为 summary。
    pub fn features_or_summary(&self) -> &str {
        self.features.as_deref().unwrap_or(&self.summary)
    }

    /// 依赖展示文本：满足的画 `✓`，缺的画 `✗`。
    pub fn deps_label(&self) -> String {
        if self.requires.is_empty() {
            return "无额外依赖".to_string();
        }
        self.requires
            .iter()
            .map(|dependency| {
                let mark = if self.missing_deps.contains(dependency) {
                    '✗'
                } else {
                    '✓'
                };
                format!("{mark} {dependency}")
            })
            .collect::<Vec<_>>()
            .join("  ")
    }

    /// 缺依赖时该告诉用户怎么装；依赖齐全时返回 `None`，详情区据此决定要不要多画一行。
    pub fn install_label(&self) -> Option<String> {
        if self.ready {
            return None;
        }
        Some(match &self.install_hint {
            Some(hint) => hint.clone(),
            None => format!("缺少命令：{}", self.missing_deps.join("  ")),
        })
    }

    /// 这件工具是不是「要先填参数」的动作。
    /// 是不是**有字段要填**（表单有意义）。
    ///
    /// 和 [`ToolDefinition::has_action`] 的区别很实在：`checkupdates` 有动作
    /// 但一个参数也没有 —— 给它弹一个空表单是纯粹的困惑（实拍踩到过），
    /// 它应该直接跑。
    pub fn needs_form(&self) -> bool {
        self.action
            .as_ref()
            .is_some_and(|action| !action.arguments.is_empty())
    }

    /// 表格「状态」列与详情区的状态文本。
    pub fn status_label(&self) -> &'static str {
        if self.ready {
            "● 就绪"
        } else {
            "! 依赖缺失"
        }
    }

    /// 行内搜索：名称 / 说明 / 标签 / 域 / Provider，大小写不敏感。
    ///
    /// `needle` 必须是已经小写化的查询串；空串视为全部命中。
    pub fn matches(&self, needle: &str) -> bool {
        if needle.is_empty() {
            return true;
        }
        self.name.to_lowercase().contains(needle)
            || self.summary.to_lowercase().contains(needle)
            || self.provider.to_lowercase().contains(needle)
            || self.domain.label().contains(needle)
            || self
                .tags
                .iter()
                .any(|tag| tag.to_lowercase().contains(needle))
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::ToolDefinition;
    use crate::model::{Danger, Domain, RunMode};

    fn tool(name: &str, domain: Domain) -> ToolDefinition {
        ToolDefinition {
            id: format!("test:{name}"),
            name: name.to_string(),
            provider: "Test".to_string(),
            domain,
            tags: vec!["转码".to_string()],
            summary: "把视频转成别的格式".to_string(),
            input: None,
            output: None,
            features: None,
            requires: Vec::new(),
            missing_deps: Vec::new(),
            install_hint: None,
            action: None,
            mode: RunMode::Interactive,
            danger: Danger::Safe,
            path: PathBuf::from(format!("/tmp/{name}")),
            ready: true,
        }
    }

    #[test]
    fn empty_needle_matches_everything() {
        assert!(tool("fzf-trim-video", Domain::Media).matches(""));
    }

    #[test]
    fn search_covers_name_summary_tag_provider_and_domain() {
        let item = tool("fzf-trim-video", Domain::Media);
        assert!(item.matches("trim"));
        assert!(item.matches("转成别的格式"));
        // 调用方负责把查询串小写化；这里不做大小写折叠。
        assert!(!item.matches("FZF-TRIM"));
        // id 不参与搜索，避免 `test:` 之类前缀造成误命中。
        assert!(!item.matches("test:"));
        assert!(item.matches("转码"));
        assert!(item.matches("test")); // provider
        assert!(item.matches("媒体")); // domain
        assert!(!item.matches("不存在的关键词"));
    }

    #[test]
    fn defaults_follow_domain_when_metadata_is_missing() {
        let media = tool("a", Domain::Media);
        let image = tool("b", Domain::Image);
        assert_ne!(media.input_or_default(), image.input_or_default());
        assert_ne!(media.output_or_default(), image.output_or_default());
        assert_eq!(media.features_or_summary(), media.summary);
    }

    #[test]
    fn deps_label_marks_each_dependency_and_install_label_explains_the_gap() {
        let mut item = tool("a", Domain::Media);
        assert_eq!(item.deps_label(), "无额外依赖");
        assert_eq!(item.status_label(), "● 就绪");
        assert_eq!(item.install_label(), None, "依赖齐全时不该多画一行");

        item.requires = vec!["ffmpeg".to_string(), "ffprobe".to_string()];
        item.missing_deps = vec!["ffprobe".to_string()];
        item.ready = false;
        assert_eq!(item.deps_label(), "✓ ffmpeg  ✗ ffprobe");
        assert_eq!(item.status_label(), "! 依赖缺失");
        // 脚本没写 `install=` 时，至少要说出缺的是什么。
        assert_eq!(item.install_label().as_deref(), Some("缺少命令：ffprobe"));

        item.install_hint = Some("sudo pacman -S ffmpeg".to_string());
        assert_eq!(
            item.install_label().as_deref(),
            Some("sudo pacman -S ffmpeg"),
            "写了 install= 就用脚本给的办法"
        );
    }
}
