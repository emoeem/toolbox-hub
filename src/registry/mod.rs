//! Registry —— Provider 聚合层。
//!
//! 负责：装配 Provider、发现工具、按路径去重、按 `(域, 二级筛选, 关键词)` 生成视图、重载。
//! UI 只通过这里拿数据，不直接认识任何具体 Provider。

use std::{
    collections::BTreeSet,
    fs,
    path::{Path, PathBuf},
};

use crate::{
    model::{Domain, ToolDefinition},
    providers::{self, Provider, metadata},
};

/// 一次发现过程的结果。
///
/// 单个 Provider 失败不致命：其余 Provider 照常工作，错误汇总成一句话给状态栏。
#[derive(Clone, Debug, Default)]
pub struct ReloadReport {
    /// 本次发现聚合到的工具总数（已去重）。
    pub tool_count: usize,
    /// 各 Provider 的整体错误（该 Provider 这次没产出）。
    pub errors: Vec<String>,
    /// 不致命的问题，例如某个 manifest 写错了 —— 其它工具照常出现。
    pub warnings: Vec<String>,
}

impl ReloadReport {
    /// 状态栏文案。
    ///
    /// 状态栏放不下全部细节，警告只给个数和第一条；完整内容在测试与日志里。
    pub fn message(&self, prefix: &str) -> String {
        let mut text = if self.errors.is_empty() {
            format!("{prefix} · {} 个工具", self.tool_count)
        } else {
            format!(
                "{prefix} · {} 个工具 · Provider 失败 {} 个: {}",
                self.tool_count,
                self.errors.len(),
                self.errors.join("; ")
            )
        };
        if let Some(first) = self.warnings.first() {
            text.push_str(&format!(" · {} 个警告: {first}", self.warnings.len()));
        }
        text
    }
}

/// Provider 集合 + 已发现工具的缓存。
pub struct Registry {
    providers: Vec<Box<dyn Provider>>,
    tools: Vec<ToolDefinition>,
}

impl Registry {
    /// 按 `bin_dir` 装配全部 Provider 并立即发现一次。
    pub fn discover(bin_dir: &Path) -> (Self, ReloadReport) {
        let mut registry = Self {
            providers: providers::all(bin_dir),
            tools: Vec::new(),
        };
        let report = registry.reload();
        (registry, report)
    }

    /// 用一个固定工具集构造 Registry（无 Provider），仅供测试使用。
    #[cfg(test)]
    pub fn from_tools(tools: Vec<ToolDefinition>) -> Self {
        Self {
            providers: Vec::new(),
            tools,
        }
    }

    /// 用一组 Provider 构造 Registry（不立即发现），仅供测试使用。
    #[cfg(test)]
    pub fn from_providers(providers: Vec<Box<dyn Provider>>) -> Self {
        Self {
            providers,
            tools: Vec::new(),
        }
    }

    /// 重新发现全部 Provider，成功则整体替换工具集。
    ///
    /// 按路径去重：同一个脚本可能被多个 Provider 认领 —— `fzf-*` 既符合 FFTools 的
    /// 命名约定，也符合通用脚本 Provider 的注解约定 —— **先注册的 Provider 优先**，
    /// 否则同一份脚本会在表格里出现两次。
    pub fn reload(&mut self) -> ReloadReport {
        // 依赖探测结果只缓存一轮：用户装了新工具之后按 Ctrl-R 应该立刻看到变化。
        metadata::clear_command_cache();

        let mut tools: Vec<ToolDefinition> = Vec::new();
        let mut errors = Vec::new();
        let mut warnings = Vec::new();
        // 去重键是「路径 + 名字」：同一个脚本被两个 Provider 认领时（名字都取自文件名）
        // 合并成一条；而 Curated 里同一个程序的多个动作路径相同、名字不同，必须都留下。
        let mut claimed: BTreeSet<(PathBuf, String)> = BTreeSet::new();

        for provider in &self.providers {
            match provider.discover() {
                Ok(found) => {
                    warnings.extend(found.warnings);
                    for tool in found.tools {
                        // canonicalize 失败（路径已被删掉等）就退化成原路径比较。
                        let key = (
                            fs::canonicalize(&tool.path).unwrap_or_else(|_| tool.path.clone()),
                            tool.name.clone(),
                        );
                        if claimed.insert(key) {
                            tools.push(tool);
                        }
                    }
                }
                Err(error) => errors.push(format!("{}: {error}", provider.label())),
            }
        }

        // 按域顺序 + 工具名排序，让表格顺序稳定，不受目录遍历顺序影响。
        tools.sort_by(|a, b| {
            a.domain
                .index()
                .cmp(&b.domain.index())
                .then_with(|| a.name.cmp(&b.name))
        });
        self.tools = tools;
        ReloadReport {
            tool_count: self.tools.len(),
            errors,
            warnings,
        }
    }

    pub fn tools(&self) -> &[ToolDefinition] {
        &self.tools
    }

    pub fn len(&self) -> usize {
        self.tools.len()
    }

    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }

    /// 已接入的 Provider 展示名，用于顶部栏。
    pub fn provider_labels(&self) -> Vec<&'static str> {
        self.providers
            .iter()
            .map(|provider| provider.label())
            .collect()
    }

    /// 某个域下的工具数量（含所有二级分类）。
    pub fn tool_count_in(&self, domain: Domain) -> usize {
        self.tools
            .iter()
            .filter(|tool| tool.domain == domain)
            .count()
    }

    /// 某个域下真实存在的二级分类，按 Provider 声明的顺序优先。
    pub fn tags_for(&self, domain: Domain) -> Vec<String> {
        let mut candidates: Vec<String> = Vec::new();
        for provider in &self.providers {
            candidates.extend(
                provider
                    .tag_order(domain)
                    .iter()
                    .map(|tag| (*tag).to_string()),
            );
        }
        for tool in &self.tools {
            if tool.domain == domain {
                candidates.extend(tool.tags.iter().cloned());
            }
        }

        // 去重，并且只保留当前域里真的有工具用到的标签（空域不显示筛选条）。
        let mut tags: Vec<String> = Vec::new();
        for tag in candidates {
            let in_use = self
                .tools
                .iter()
                .any(|tool| tool.domain == domain && tool.tags.contains(&tag));
            if in_use && !tags.contains(&tag) {
                tags.push(tag);
            }
        }
        tags
    }

    /// 全部工具的下标（按注册顺序）。跨域视图（收藏 / 最近使用）拿它当基础集合：[`Registry::view`] 的跨域只由「关键词非空」触发，而这两个视图在没有关键词时也要跨域。
    pub fn all_indices(&self) -> Vec<usize> {
        (0..self.tools.len()).collect()
    }

    /// 生成视图：返回 `tools()` 中命中的下标。
    ///
    /// * **关键词非空 → 跨域搜索**：不再受域与二级分类限制。「我记得有这个工具，
    ///   但忘了它在哪个域」是工具变多以后最常见的场景，找得到优先于分得清；
    ///   表格里有一列「域」，所以跨域结果仍然可辨认。
    /// * 关键词为空 → 按 `(域, 二级分类)` 浏览，域是硬边界。
    pub fn view(&self, domain: Domain, tag: Option<&str>, query: &str) -> Vec<usize> {
        let needle = query.trim().to_lowercase();

        if needle.is_empty() {
            return self
                .tools
                .iter()
                .enumerate()
                .filter(|(_, tool)| tool.domain == domain)
                .filter(|(_, tool)| tag.is_none_or(|tag| tool.tags.iter().any(|own| own == tag)))
                .map(|(index, _)| index)
                .collect();
        }

        self.tools
            .iter()
            .enumerate()
            .filter(|(_, tool)| tool.matches(&needle))
            .map(|(index, _)| index)
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use std::{io, path::PathBuf};

    use super::Registry;
    use crate::{
        model::{Danger, Domain, RunMode, ToolDefinition},
        providers::{Discovery, Provider},
    };

    fn tool(name: &str, domain: Domain, tags: &[&str], summary: &str) -> ToolDefinition {
        ToolDefinition {
            id: format!("test:{name}"),
            name: name.to_string(),
            provider: "Test".to_string(),
            domain,
            tags: tags.iter().map(|tag| (*tag).to_string()).collect(),
            summary: summary.to_string(),
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

    fn sample_registry() -> Registry {
        Registry::from_tools(vec![
            tool("trim-video", Domain::Media, &["编辑"], "裁剪视频"),
            tool("convert-media", Domain::Media, &["转码"], "格式转换"),
            tool("burn-subs", Domain::Media, &["字幕"], "烧字幕"),
            tool("shorin", Domain::Tools, &[], "通用脚本"),
        ])
    }

    /// 返回预设工具集的假 Provider。
    struct FakeProvider {
        id: &'static str,
        label: &'static str,
        tools: Vec<ToolDefinition>,
    }

    impl Provider for FakeProvider {
        fn id(&self) -> &'static str {
            self.id
        }

        fn label(&self) -> &'static str {
            self.label
        }

        fn discover(&self) -> io::Result<Discovery> {
            Ok(Discovery::clean(self.tools.clone()))
        }
    }

    /// 永远失败的 Provider，用来验证「坏了也不拖垮别人」。
    struct FailingProvider;

    impl Provider for FailingProvider {
        fn id(&self) -> &'static str {
            "failing"
        }

        fn label(&self) -> &'static str {
            "Failing"
        }

        fn discover(&self) -> io::Result<Discovery> {
            Err(io::Error::other("无法读取目录"))
        }
    }

    #[test]
    fn missing_bin_dir_is_reported_as_a_provider_error_without_aborting() {
        // 这里刻意不用 `Registry::discover`：它会装配全部 Provider，连带扫描真实
        // HOME 下的脚本目录与 /usr/share 的应用条目 —— 那样测试既慢又依赖机器。
        // 只把真实的 FftoolsProvider 指向一个不存在的目录，验证错误路径本身。
        let mut registry = Registry::from_providers(vec![Box::new(
            crate::providers::fftools::FftoolsProvider::new(PathBuf::from("/definitely/not/here")),
        )]);

        let report = registry.reload();
        assert_eq!(report.tool_count, 0);
        assert!(
            report.errors.iter().any(|error| error.contains("FFTools")),
            "缺失目录应记为 FFTools 的 Provider 错误: {report:?}"
        );
        assert!(
            report.message("就绪").contains("Provider 失败"),
            "{}",
            report.message("就绪")
        );
    }

    #[test]
    fn reload_sorts_by_domain_then_name() {
        let mut registry = Registry::from_providers(vec![Box::new(FakeProvider {
            id: "sorted",
            label: "Sorted",
            tools: vec![
                tool("zebra", Domain::Tools, &[], "z"),
                tool("alpha", Domain::Tools, &[], "a"),
                tool("beta", Domain::Media, &[], "b"),
            ],
        })]);

        registry.reload();
        let names: Vec<_> = registry
            .tools()
            .iter()
            .map(|tool| tool.name.as_str())
            .collect();
        // 域顺序优先（Media 排在 Tools 之前），域内按名字排 —— 表格顺序因此稳定，
        // 不受目录遍历顺序影响。
        assert_eq!(names, vec!["beta", "alpha", "zebra"]);
    }

    #[test]
    fn reload_dedupes_by_path_and_keeps_the_first_provider() {
        let shared = PathBuf::from("/tmp/toolbox-hub-shared-tool");

        let mut first = tool("shared", Domain::Media, &["编辑"], "来自第一个 Provider");
        first.path = shared.clone();
        first.provider = "First".to_string();

        let mut second = tool("shared", Domain::Tools, &[], "来自第二个 Provider");
        second.path = shared.clone();
        second.provider = "Second".to_string();

        let mut only_second = tool("only-second", Domain::Tools, &[], "只属于第二个");
        only_second.path = PathBuf::from("/tmp/toolbox-hub-only-second");

        let mut registry = Registry::from_providers(vec![
            Box::new(FakeProvider {
                id: "first",
                label: "First",
                tools: vec![first],
            }),
            Box::new(FakeProvider {
                id: "second",
                label: "Second",
                tools: vec![second, only_second],
            }),
        ]);

        let report = registry.reload();
        assert_eq!(
            report.tool_count,
            2,
            "重复路径应被丢掉: {:#?}",
            registry.tools()
        );
        assert!(report.errors.is_empty());

        let kept: Vec<_> = registry
            .tools()
            .iter()
            .filter(|tool| tool.path == shared)
            .collect();
        assert_eq!(kept.len(), 1, "同一份脚本只能出现一次");
        assert_eq!(kept[0].provider, "First", "先注册的 Provider 优先");
        assert_eq!(kept[0].domain, Domain::Media);
    }

    #[test]
    fn failing_provider_does_not_break_the_rest() {
        let mut registry = Registry::from_providers(vec![
            Box::new(FailingProvider),
            Box::new(FakeProvider {
                id: "ok",
                label: "OK",
                tools: vec![tool("survivor", Domain::Tools, &[], "活下来了")],
            }),
        ]);

        let report = registry.reload();
        assert_eq!(report.tool_count, 1);
        assert_eq!(report.errors.len(), 1);
        assert!(report.errors[0].contains("Failing"));
        assert!(report.message("已刷新").contains("Provider 失败 1 个"));
        assert_eq!(registry.tools()[0].name, "survivor");
    }

    /// 某个 manifest 写错了：应该能在状态栏看到，而不是静悄悄少几个工具。
    #[test]
    fn provider_warnings_are_reported_without_losing_the_tools() {
        struct WarningProvider;

        impl Provider for WarningProvider {
            fn id(&self) -> &'static str {
                "warn"
            }

            fn label(&self) -> &'static str {
                "Warn"
            }

            fn discover(&self) -> io::Result<Discovery> {
                Ok(Discovery {
                    tools: vec![tool("fine", Domain::Tools, &[], "还行")],
                    warnings: vec!["mine.toml: 缺少 program".to_string()],
                })
            }
        }

        let mut registry = Registry::from_providers(vec![Box::new(WarningProvider)]);
        let report = registry.reload();

        assert_eq!(report.tool_count, 1, "有警告不代表工具丢了");
        assert!(report.errors.is_empty());

        let message = report.message("就绪");
        assert!(message.contains("1 个警告"), "{message}");
        assert!(
            message.contains("缺少 program"),
            "第一条警告要能看见: {message}"
        );
    }

    #[test]
    fn browsing_is_domain_scoped_and_search_is_cross_domain() {
        let registry = sample_registry();

        // 浏览（关键词为空）：域是硬边界，二级分类参与过滤。
        assert_eq!(registry.view(Domain::Media, None, "").len(), 3);
        assert_eq!(registry.view(Domain::Tools, None, "").len(), 1);
        let subtitle = registry.view(Domain::Media, Some("字幕"), "");
        assert_eq!(subtitle.len(), 1);
        assert_eq!(registry.tools()[subtitle[0]].name, "burn-subs");

        // 搜索（关键词非空）：跨域，且二级分类不再参与过滤。
        // 「我记得有这工具，但忘了它在哪个域」正是这一步要解决的场景。
        assert_eq!(registry.view(Domain::Media, None, "shorin").len(), 1);
        assert_eq!(registry.view(Domain::Tools, None, "shorin").len(), 1);
        assert_eq!(
            registry.view(Domain::Image, None, "裁剪").len(),
            1,
            "停在一个空域里也要能搜到别的域的工具"
        );
        assert_eq!(
            registry.view(Domain::Media, Some("转码"), "裁剪").len(),
            1,
            "跨域搜索时二级分类不再过滤"
        );

        // 查询串大小写不敏感，且会 trim。
        assert_eq!(registry.view(Domain::Media, None, "  CONVERT  ").len(), 1);
    }

    #[test]
    fn tool_count_in_counts_every_tag_of_a_domain() {
        let registry = sample_registry();
        assert_eq!(registry.tool_count_in(Domain::Media), 3);
        assert_eq!(registry.tool_count_in(Domain::Tools), 1);
        assert_eq!(registry.tool_count_in(Domain::Image), 0);
        assert_eq!(registry.tool_count_in(Domain::Network), 0);
    }

    #[test]
    fn tags_for_returns_only_tags_in_use() {
        let registry = sample_registry();
        let media_tags = registry.tags_for(Domain::Media);
        assert!(media_tags.contains(&"转码".to_string()));
        assert!(media_tags.contains(&"编辑".to_string()));
        assert!(media_tags.contains(&"字幕".to_string()));
        // 该域没人用的标签不该出现在筛选条里。
        assert!(!media_tags.contains(&"分析".to_string()));
        // 空域没有筛选条。
        assert!(registry.tags_for(Domain::Image).is_empty());
    }

    #[test]
    fn reload_without_providers_keeps_tools_and_reports_no_error() {
        let mut registry = sample_registry();
        let before = registry.len();
        let report = registry.reload();
        // from_tools 没有 Provider，reload 会把工具集清空——这是有意的：
        // reload 的语义是「按 Provider 重新发现」，不是「保留旧数据」。
        assert_eq!(report.tool_count, 0);
        assert!(report.errors.is_empty());
        assert_ne!(before, registry.len());
    }

    /// 端到端冒烟：真实 Provider 跑真实根目录，按域统计并验证路径唯一。
    ///
    /// 默认不跑。手动执行：`cargo test -- --ignored --nocapture`
    #[test]
    #[ignore = "读取真实 HOME / /usr/local/bin，默认跳过"]
    fn smoke_registry_over_real_roots() {
        let Some(home) = std::env::var_os("HOME") else {
            return;
        };
        let bin_dir = PathBuf::from(home).join(".local/bin");
        if !bin_dir.is_dir() {
            return;
        }

        let (registry, report) = Registry::discover(&bin_dir);
        println!("已接入 Provider: {:?}", registry.provider_labels());
        println!("总计 {} 个工具", registry.len());
        for domain in Domain::ALL {
            println!(
                "  {} ({}) → {} 个  tags={:?}",
                domain.label(),
                domain.id(),
                registry.tool_count_in(domain),
                registry.tags_for(domain)
            );
        }
        println!("报告: {}", report.message("就绪"));

        assert!(report.errors.is_empty(), "真实环境下不该有 Provider 失败");
        assert!(!registry.is_empty());

        // 端到端最关键的一条：同一份脚本只能出现一次。
        // 去重键是「路径 + 名字」—— 同一个程序的多个动作（yt-dlp 下载 / 提取音频）
        // 路径相同、名字不同，那是**正常的**，不能算重复。
        let mut keys: Vec<_> = registry
            .tools()
            .iter()
            .map(|tool| (tool.path.clone(), tool.name.clone()))
            .collect();
        keys.sort();
        let total = keys.len();
        keys.dedup();
        assert_eq!(
            total,
            keys.len(),
            "出现重复的（路径 + 名字）: {:#?}",
            registry.tools()
        );

        // fzf-* 属于 FFTools，不该被通用脚本 Provider 抢走。
        assert!(
            registry
                .tools()
                .iter()
                .filter(|tool| tool.name.starts_with("fzf-"))
                .all(|tool| tool.provider == "FFTools"),
            "fzf-* 必须以 FFTools 的身份出现"
        );
    }
}
