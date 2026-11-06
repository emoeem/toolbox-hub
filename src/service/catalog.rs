//! 工具目录：给服务层 / MCP 用的只读索引。
//!
//! 为什么不在 [`crate::registry::Registry`] 上直接加方法：Registry 的生命周期
//! 和 Provider 绑在一起（重载会整体换掉工具集），而服务层需要一份**稳定快照** ——
//! 一次请求处理期间拿到的 id、顺序、内容都不能中途变。所以这里克隆一份并排好序。
//!
//! 排序比 Registry 多一个 `id` tie-breaker：目录内容应该产生**完全相同**的顺序，
//! 包括同名工具的极端情况（P4-04 的验收要求）。

use std::collections::{BTreeMap, BTreeSet};

use crate::{
    model::{Domain, RunMode, ToolDefinition},
    registry::Registry,
};

/// 工具目录。
#[derive(Clone, Debug, Default)]
pub struct ToolCatalog {
    tools: Vec<ToolDefinition>,
    /// id → 在 `tools` 里的下标（`BTreeMap` 而不是 `HashMap`：可预期、可调试）。
    index: BTreeMap<String, usize>,
    /// 重复的 id（先注册的留下，后来的记在这里）。
    duplicates: Vec<String>,
}

impl ToolCatalog {
    /// 用一批工具构造目录：先去重（保留输入优先级），再排序、建索引。
    ///
    /// 顺序很重要：**先按输入顺序去重**，与 [`crate::registry::Registry`] 的
    /// 「先注册的 Provider 胜出」一致；如果先排序再去重，同名工具的 Unicode
    /// 顺序会决定谁活下来 —— 那是显示顺序在偷偷决定身份，不能接受。
    pub fn new(tools: impl IntoIterator<Item = ToolDefinition>) -> Self {
        let mut index = BTreeMap::new();
        let mut duplicates = Vec::new();
        let mut unique: Vec<ToolDefinition> = Vec::new();
        let mut seen: BTreeSet<String> = BTreeSet::new();
        for tool in tools {
            if !seen.insert(tool.id.clone()) {
                duplicates.push(tool.id.clone());
                continue;
            }
            unique.push(tool);
        }

        unique.sort_by(|a, b| {
            a.domain
                .index()
                .cmp(&b.domain.index())
                .then_with(|| a.pin.is_none().cmp(&b.pin.is_none()))
                .then_with(|| a.pin.unwrap_or(0).cmp(&b.pin.unwrap_or(0)))
                .then_with(|| a.name.cmp(&b.name))
                .then_with(|| a.id.cmp(&b.id))
        });

        // 排序之后下标变了，重新建索引。
        index.clear();
        for (position, tool) in unique.iter().enumerate() {
            index.insert(tool.id.clone(), position);
        }

        Self {
            tools: unique,
            index,
            duplicates,
        }
    }

    /// 从 Registry 当前快照构造目录。
    pub fn from_registry(registry: &Registry) -> Self {
        Self::new(registry.tools().iter().cloned())
    }

    /// 全部工具（已排序）。
    pub fn list(&self) -> &[ToolDefinition] {
        &self.tools
    }

    pub fn get(&self, id: &str) -> Option<&ToolDefinition> {
        self.index.get(id).map(|index| &self.tools[*index])
    }

    pub fn len(&self) -> usize {
        self.tools.len()
    }

    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }

    /// 被发现过、但 id 与前面的工具重复的条目（先注册的胜出）。
    pub fn duplicate_ids(&self) -> &[String] {
        &self.duplicates
    }

    /// 按关键词搜索：名称 / 说明 / 标签 / 域 / Provider，大小写不敏感。
    ///
    /// 空查询返回全部（顺序与 [`ToolCatalog::list`] 一致）。
    pub fn search(&self, query: &str) -> Vec<&ToolDefinition> {
        let needle = query.trim().to_lowercase();
        self.tools
            .iter()
            .filter(|tool| tool.matches(&needle))
            .collect()
    }

    /// 某个域下的工具。
    pub fn in_domain(&self, domain: Domain) -> impl Iterator<Item = &ToolDefinition> {
        self.tools.iter().filter(move |tool| tool.domain == domain)
    }

    /// 可以在无终端环境里执行的工具（当前只有 `capture`）。
    pub fn headless(&self) -> impl Iterator<Item = &ToolDefinition> {
        self.tools
            .iter()
            .filter(|tool| tool.mode == RunMode::Capture)
    }

    /// 全部 id（已排序，适合做快照比较）。
    pub fn ids(&self) -> impl Iterator<Item = &str> {
        self.tools.iter().map(|tool| tool.id.as_str())
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::ToolCatalog;
    use crate::model::{Danger, Domain, RunMode, ToolDefinition};

    fn tool(id: &str, name: &str, domain: Domain, mode: RunMode) -> ToolDefinition {
        ToolDefinition {
            mcp: None,
            id: id.to_string(),
            name: name.to_string(),
            provider: String::from("Test"),
            domain,
            tags: vec![String::from("转换")],
            summary: String::from("把一种格式换成另一种"),
            input: None,
            output: None,
            features: None,
            requires: Vec::new(),
            missing_deps: Vec::new(),
            install_hint: None,
            action: None,
            pin: None,
            mode,
            danger: Danger::Safe,
            path: PathBuf::from(format!("/tmp/{name}")),
            ready: true,
        }
    }

    /// 目录顺序必须只由内容决定：输入顺序换了，输出顺序不能变。
    #[test]
    fn ordering_is_stable_and_independent_of_input_order() {
        let a = tool("test:b", "乙", Domain::Media, RunMode::Capture);
        let b = tool("test:a", "甲", Domain::System, RunMode::Capture);
        let c = tool("test:c", "丙", Domain::Media, RunMode::Capture);

        let first = ToolCatalog::new(vec![a.clone(), b.clone(), c.clone()]);
        let second = ToolCatalog::new(vec![c, a, b]);

        let ids: Vec<&str> = first.ids().collect();
        assert_eq!(ids, second.ids().collect::<Vec<_>>());
        assert_eq!(ids, vec!["test:c", "test:b", "test:a"], "域优先，同名按 id");
    }

    #[test]
    fn a_duplicate_id_keeps_the_first_and_is_reported() {
        let first = tool("test:same", "甲", Domain::Tools, RunMode::Capture);
        let mut second = tool("test:same", "乙", Domain::Tools, RunMode::Capture);
        second.path = PathBuf::from("/tmp/other");

        let catalog = ToolCatalog::new(vec![first, second]);
        assert_eq!(catalog.len(), 1);
        assert_eq!(
            catalog.get("test:same").map(|tool| tool.name.as_str()),
            Some("甲")
        );
        assert_eq!(catalog.duplicate_ids(), ["test:same"]);
    }

    #[test]
    fn search_is_case_insensitive_and_covers_tags() {
        let catalog = ToolCatalog::new(vec![
            tool("test:a", "Alpha", Domain::Media, RunMode::Capture),
            tool("test:b", "Beta", Domain::System, RunMode::Interactive),
        ]);

        assert_eq!(catalog.search("").len(), 2);
        assert_eq!(catalog.search("  alpha ").len(), 1);
        assert_eq!(catalog.search("转换").len(), 2, "标签也参与搜索");
        assert!(catalog.search("不存在").is_empty());
    }

    #[test]
    fn headless_only_lists_capture_tools() {
        let catalog = ToolCatalog::new(vec![
            tool("test:c", "捕获", Domain::Tools, RunMode::Capture),
            tool("test:i", "交互", Domain::Tools, RunMode::Interactive),
            tool("test:n", "界面", Domain::Tools, RunMode::Native),
        ]);

        let ids: Vec<&str> = catalog.headless().map(|tool| tool.id.as_str()).collect();
        assert_eq!(ids, ["test:c"]);
    }

    #[test]
    fn in_domain_filters_and_keeps_the_order() {
        let catalog = ToolCatalog::new(vec![
            tool("test:b", "乙", Domain::Media, RunMode::Capture),
            tool("test:a", "甲", Domain::Media, RunMode::Capture),
            tool("test:s", "系统", Domain::System, RunMode::Capture),
        ]);

        let names: Vec<&str> = catalog
            .in_domain(Domain::Media)
            .map(|tool| tool.name.as_str())
            .collect();
        assert_eq!(names, ["乙", "甲"]);
    }
}
