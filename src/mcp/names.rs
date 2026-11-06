//! MCP 工具名：把内部 id（`manifest:disk-space`）映射成 MCP 工具名
//! （`toolbox_manifest_disk_space`）。
//!
//! 规则（计划书 P3-02）：
//!
//! 1. 前缀 `toolbox_`，避免和其它 MCP Server 的工具撞名；
//! 2. id 的来源段（`:` 之前）与动作段逐段转成 `[a-z0-9_]`，其余字符折叠成 `_`；
//! 3. **同名冲突**（不同 id 规范化后撞车，比如中文名全部被折叠成空）不报错、
//!    也不悄悄覆盖：后到的加 `_` + id 的 FNV-1a 哈希后缀，谁先谁后由目录的
//!    稳定排序决定 —— 相同内容永远产生相同的名字（P4-04 的要求在这里就立住）。

use std::collections::{BTreeMap, BTreeSet};

use crate::model::ToolDefinition;

/// 内部 id ↔ MCP 工具名 的稳定映射。
#[derive(Clone, Debug, Default)]
pub struct NameRegistry {
    /// MCP 工具名 → 内部 id。
    by_name: BTreeMap<String, String>,
    /// 内部 id → MCP 工具名。
    by_id: BTreeMap<String, String>,
}

impl NameRegistry {
    /// 按给定顺序建表；顺序必须来自目录的稳定排序。
    pub fn build<'a>(tools: impl IntoIterator<Item = &'a ToolDefinition>) -> Self {
        let mut by_name = BTreeMap::new();
        let mut by_id = BTreeMap::new();
        let mut used: BTreeSet<String> = BTreeSet::new();
        for tool in tools {
            let mut name = mcp_name(&tool.id);
            if !used.insert(name.clone()) {
                // 撞名：加哈希后缀。哈希只用于**区分**，不参与命名主逻辑，
                // 所以正常情况下（id 本来就不重）永远走不到这里。
                name = format!("{}_{}{:08x}", name, "x", fnv1a(&tool.id) & 0xffff_ffff);
                // 哈希后缀还撞（同一 id 出现两次）就只能跳过 —— id 唯一性
                // 本来就由目录的去重保证，这里是最后一道保险。
                if !used.insert(name.clone()) {
                    continue;
                }
            }
            by_name.insert(name.clone(), tool.id.clone());
            by_id.insert(tool.id.clone(), name);
        }
        Self { by_name, by_id }
    }

    /// MCP 工具名 → 内部 id。
    pub fn resolve(&self, name: &str) -> Option<&str> {
        self.by_name.get(name).map(String::as_str)
    }

    /// 内部 id → MCP 工具名。
    pub fn name_of(&self, id: &str) -> Option<&str> {
        self.by_id.get(id).map(String::as_str)
    }
}

/// `manifest:disk-space` → `toolbox_manifest_disk_space`。
///
/// 没有来源段的 id（不太可能，但模型不禁止）按 `tool` 处理；
/// 来源段或动作段被折叠成空（比如中文名）时，该段兜底为 `tool`，
/// 保证名字至少有 `toolbox_<a>_<b>` 三段、不会出现悬空下划线。
pub fn mcp_name(id: &str) -> String {
    let (source, rest) = match id.split_once(':') {
        Some((source, rest)) => (source, rest),
        None => ("tool", id),
    };
    let head = slug(source);
    let head = if head.is_empty() {
        String::from("tool")
    } else {
        head
    };
    let tail = slug(rest);
    if tail.is_empty() {
        format!("toolbox_{head}_tool")
    } else {
        format!("toolbox_{head}_{tail}")
    }
}

/// 折叠成 `[a-z0-9_]`：其它字符各变一个 `_`，连续下划线并成一个，掐头去尾。
fn slug(text: &str) -> String {
    let mut out = String::new();
    for character in text.chars() {
        if character.is_ascii_alphanumeric() {
            out.push(character.to_ascii_lowercase());
        } else if !out.is_empty() && !out.ends_with('_') {
            out.push('_');
        }
    }
    while out.ends_with('_') {
        out.pop();
    }
    out
}

/// FNV-1a（64 位）：无依赖、确定性，只用来给撞名的 id 做区分后缀。
fn fnv1a(text: &str) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in text.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::{NameRegistry, fnv1a, mcp_name, slug};
    use crate::model::{Danger, Domain, RunMode, ToolDefinition};
    use std::path::PathBuf;

    fn tool(id: &str) -> ToolDefinition {
        ToolDefinition {
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
            action: None,
            mode: RunMode::Capture,
            danger: Danger::Safe,
            path: PathBuf::from("/bin/echo"),
            ready: true,
        }
    }

    #[test]
    fn ids_become_prefixed_underscore_names() {
        assert_eq!(
            mcp_name("manifest:disk-space"),
            "toolbox_manifest_disk_space"
        );
        assert_eq!(
            mcp_name("fftools:fzf-trim-video"),
            "toolbox_fftools_fzf_trim_video"
        );
        assert_eq!(
            mcp_name("scripted:my.tool v2"),
            "toolbox_scripted_my_tool_v2"
        );
        assert_eq!(mcp_name("bare"), "toolbox_tool_bare");
        assert_eq!(mcp_name("manifest:磁盘"), "toolbox_manifest_tool");
    }

    #[test]
    fn names_only_contain_safe_characters() {
        for id in [
            "manifest:disk-space",
            "scripted:我的脚本",
            "repo:pkg/sub--cmd!!",
            ":::",
        ] {
            let name = mcp_name(id);
            assert!(
                name.chars()
                    .all(|c| c.is_ascii_lowercase() || c == '_' || c.is_ascii_digit()),
                "{id} → {name}"
            );
            assert!(name.starts_with("toolbox_"), "{id} → {name}");
        }
    }

    #[test]
    fn slug_folds_and_trims() {
        assert_eq!(slug("Disk-Space"), "disk_space");
        assert_eq!(slug("  a--b  "), "a_b");
        assert_eq!(slug("中文"), "");
    }

    #[test]
    fn colliding_ids_get_distinct_stable_names() {
        // 两个 id 规范化后撞车：先来的保住原名，后来的拿哈希后缀。
        let first = tool("manifest:disk");
        let second = tool("manifest:disk!");
        let registry = NameRegistry::build([first, second].iter());
        assert_eq!(
            registry.resolve("toolbox_manifest_disk"),
            Some("manifest:disk")
        );
        let other = registry.name_of("manifest:disk!").expect("撞名要有名字");
        assert_ne!(other, "toolbox_manifest_disk");
        assert!(other.starts_with("toolbox_manifest_disk_"), "{other}");

        // 顺序反过来，归属也反过来 —— 名字始终跟着「目录里谁排前面」走。
        let first = tool("manifest:disk");
        let second = tool("manifest:disk!");
        let flipped = NameRegistry::build([second, first].iter());
        assert_eq!(
            flipped.resolve("toolbox_manifest_disk"),
            Some("manifest:disk!")
        );
    }

    #[test]
    fn registry_resolves_in_both_directions() {
        let registry = NameRegistry::build([tool("manifest:disk-space")].iter());
        assert_eq!(
            registry.name_of("manifest:disk-space"),
            Some("toolbox_manifest_disk_space")
        );
        assert_eq!(
            registry.resolve("toolbox_manifest_disk_space"),
            Some("manifest:disk-space")
        );
        assert_eq!(registry.resolve("nope"), None);
        assert_eq!(registry.name_of("nope"), None);
    }

    #[test]
    fn hash_is_deterministic() {
        assert_eq!(fnv1a("manifest:disk"), fnv1a("manifest:disk"));
        assert_ne!(fnv1a("manifest:disk"), fnv1a("manifest:disk!"));
    }
}
