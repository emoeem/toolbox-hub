//! FFTools Provider —— 把 `~/.local/bin/fzf-*` 脚本头部的元数据翻译成
//! Provider 无关的 [`ToolDefinition`]，统一挂在 `媒体` 域下。
//!
//! 元数据格式与依赖检测复用 [`crate::providers::metadata`]；
//! 「转码 / 编辑 / 媒体 / 字幕 / 分析 / 工具」这套二级分类是 FFTools 自己的口味，
//! 写在本文件里，不上升为 UI 概念。
//!
//! 它只读文件，不改任何脚本，也不依赖 FFTools 仓库。

use std::{fs, io, path::PathBuf};

use crate::{
    model::{Danger, Domain, RunMode, ToolDefinition},
    providers::{Discovery, Provider, metadata},
};

/// FFTools 的二级分类顺序：页内筛选条按此排列。
const TAG_ORDER: &[&str] = &["转码", "编辑", "媒体", "字幕", "分析", "工具"];

/// 扫描 `fzf-*` 脚本目录，产出 `媒体` 域下的工具。
pub struct FftoolsProvider {
    bin_dir: PathBuf,
}

impl FftoolsProvider {
    pub fn new(bin_dir: PathBuf) -> Self {
        Self { bin_dir }
    }

    /// 由脚本名 + 头部元数据构造工具定义。
    ///
    /// id 前缀与 Provider 名都取自 trait 方法，避免和 `id()` / `label()` 各写一份。
    fn definition(&self, name: &str, path: PathBuf, head: &str) -> ToolDefinition {
        let deps = metadata::dependencies(head, name);
        let ready = deps.is_ready();

        ToolDefinition {
            id: format!("{}:{name}", self.id()),
            name: name.to_string(),
            provider: self.label().to_string(),
            // FFTools 的工具全部落在媒体域；域属于工具，不属于 Provider。
            domain: Domain::Media,
            tags: vec![category_for(name)],
            summary: metadata::meta(head, name, "summary").unwrap_or_else(|| "-".to_string()),
            input: metadata::meta(head, name, "input"),
            output: metadata::meta(head, name, "output"),
            features: metadata::meta(head, name, "features"),
            requires: deps.required,
            missing_deps: deps.missing,
            install_hint: metadata::meta(head, name, "install"),
            // FFTools 的脚本自己会问参数，不需要工具箱代填。
            pin: None,
            action: None,
            mode: mode_of(head, name),
            danger: danger_of(head, name),
            path,
            ready,
        }
    }
}

impl Provider for FftoolsProvider {
    fn id(&self) -> &'static str {
        "fftools"
    }

    fn label(&self) -> &'static str {
        "FFTools"
    }

    fn tag_order(&self, domain: Domain) -> &'static [&'static str] {
        // FFTools 全是媒体工具，只为媒体域声明筛选条顺序。
        if domain == Domain::Media {
            TAG_ORDER
        } else {
            &[]
        }
    }

    fn discover(&self) -> io::Result<Discovery> {
        let mut entries = fs::read_dir(&self.bin_dir)?.collect::<Result<Vec<_>, _>>()?;
        entries.sort_by_key(|entry| entry.file_name());

        let mut tools = Vec::new();
        for entry in entries {
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().to_string();
            if !is_candidate(&name) || !metadata::is_executable(&path) {
                continue;
            }
            let head = metadata::read_head(&path);
            // internal=true 的是内部件（主菜单、共享库、预览器），不是可执行工具。
            if metadata::is_internal(&head, &name) {
                continue;
            }
            tools.push(self.definition(&name, path, &head));
        }
        Ok(Discovery::clean(tools))
    }
}

/// 脚本注解里声明的执行方式；不写就是「接管终端」（FFTools 那批全靠 fzf 交互）。
fn mode_of(head: &str, name: &str) -> RunMode {
    metadata::meta(head, name, "mode")
        .and_then(|raw| RunMode::parse(&raw))
        .unwrap_or(RunMode::Interactive)
}

/// 脚本注解里声明的危险度；不写就是安全。
fn danger_of(head: &str, name: &str) -> Danger {
    metadata::meta(head, name, "danger")
        .and_then(|raw| Danger::parse(&raw))
        .unwrap_or(Danger::Safe)
}

/// 只接受 `fzf-*` 脚本，并排除 FFTools 主菜单本身。
fn is_candidate(name: &str) -> bool {
    name.starts_with("fzf-") && name != "fzf-fftools"
}

/// 脚本名 → 二级分类。
///
/// 这是 FFTools 自己的口味，属于 Provider 内部实现，不上升为 UI 概念。
fn category_for(name: &str) -> String {
    let category = if ["subtitle", "subs"].iter().any(|key| name.contains(key)) {
        "字幕"
    } else if ["ffmpeg", "convert", "target-size"]
        .iter()
        .any(|key| name.contains(key))
    {
        "转码"
    } else if ["trim", "speed", "gif", "watermark", "screenshot"]
        .iter()
        .any(|key| name.contains(key))
    {
        "编辑"
    } else if ["quality", "validate", "loudness", "metadata", "report"]
        .iter()
        .any(|key| name.contains(key))
    {
        "分析"
    } else if [
        "audio",
        "mute",
        "swap-audio",
        "merge-av",
        "mux",
        "extract-streams",
    ]
    .iter()
    .any(|key| name.contains(key))
    {
        "媒体"
    } else {
        "工具"
    };
    category.to_string()
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        path::{Path, PathBuf},
        time::{SystemTime, UNIX_EPOCH},
    };

    use super::{FftoolsProvider, category_for};
    use crate::{model::Domain, providers::Provider};

    /// 只用来跑 `definition()` 的 Provider，目录本身无所谓。
    fn provider() -> FftoolsProvider {
        FftoolsProvider::new(PathBuf::from("/tmp/toolbox-hub-unused"))
    }

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

    fn write_script(dir: &Path, name: &str, body: &str) {
        use std::os::unix::fs::PermissionsExt;
        let path = dir.join(name);
        fs::write(&path, body).expect("write script");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).expect("chmod script");
    }

    fn write_plain(dir: &Path, name: &str, body: &str) {
        fs::write(dir.join(name), body).expect("write plain file");
    }

    #[test]
    fn category_for_maps_known_families_and_falls_back() {
        assert_eq!(category_for("fzf-burn-subs"), "字幕");
        assert_eq!(category_for("fzf-convert-media"), "转码");
        assert_eq!(category_for("fzf-trim-video"), "编辑");
        assert_eq!(category_for("fzf-quality-eval"), "分析");
        assert_eq!(category_for("fzf-mux-media"), "媒体");
        assert_eq!(category_for("fzf-media-rename"), "工具");
    }

    #[test]
    fn definition_falls_back_when_metadata_is_missing() {
        let tool = provider().definition("fzf-mystery", PathBuf::from("/tmp/fzf-mystery"), "");
        assert_eq!(tool.summary, "-");
        assert_eq!(tool.input, None);
        assert!(tool.requires.is_empty());
        assert!(tool.ready);
        assert_eq!(tool.tags, vec!["工具".to_string()]);
        assert_eq!(tool.domain, Domain::Media);
        assert_eq!(tool.provider, "FFTools");
        // id 前缀来自 Provider::id()，不是硬编码字符串。
        assert_eq!(tool.id, format!("{}:fzf-mystery", provider().id()));
    }

    #[test]
    fn ready_tracks_declared_dependencies() {
        let ok = provider().definition("fzf-a", PathBuf::from("/tmp/fzf-a"), "# fzf-a:requires=sh");
        assert!(ok.ready);

        let bad = provider().definition(
            "fzf-b",
            PathBuf::from("/tmp/fzf-b"),
            "# fzf-b:requires=sh,toolbox-hub-definitely-missing-cmd",
        );
        assert_eq!(bad.requires.len(), 2);
        assert!(!bad.ready);
    }

    #[test]
    fn discover_reads_metadata_and_skips_internal_and_non_tools() {
        let dir = temp_dir("discover");
        write_script(
            &dir,
            "fzf-test-convert",
            "#!/usr/bin/env bash\n# fzf-test-convert:summary=测试转换\n# fzf-test-convert:input=视频文件\n# fzf-test-convert:output=转码结果\n# fzf-test-convert:features=快而稳\n",
        );
        write_script(
            &dir,
            "fzf-test-internal",
            "#!/usr/bin/env bash\n# fzf-test-internal:summary=内部件\n# fzf-test-internal:internal=true\n",
        );
        write_script(
            &dir,
            "fzf-fftools",
            "#!/usr/bin/env bash\n# fzf-fftools:summary=主菜单\n",
        );
        write_script(
            &dir,
            "not-a-tool",
            "#!/usr/bin/env bash\n# not-a-tool:summary=命名不符合\n",
        );
        write_plain(
            &dir,
            "fzf-test-noexec",
            "#!/usr/bin/env bash\n# fzf-test-noexec:summary=没有执行位\n",
        );

        let tools = FftoolsProvider::new(dir.clone())
            .discover()
            .expect("discover")
            .tools;
        assert_eq!(tools.len(), 1, "只应留下一个真实工具: {tools:#?}");

        let tool = &tools[0];
        assert_eq!(tool.id, "fftools:fzf-test-convert");
        assert_eq!(tool.name, "fzf-test-convert");
        assert_eq!(tool.provider, "FFTools");
        assert_eq!(tool.domain, Domain::Media);
        assert_eq!(tool.tags, vec!["转码".to_string()]);
        assert_eq!(tool.summary, "测试转换");
        assert_eq!(tool.input.as_deref(), Some("视频文件"));
        assert_eq!(tool.output.as_deref(), Some("转码结果"));
        assert_eq!(tool.features.as_deref(), Some("快而稳"));
        assert_eq!(tool.path, dir.join("fzf-test-convert"));
        assert!(tool.ready);

        fs::remove_dir_all(&dir).expect("cleanup temp dir");
    }

    #[test]
    fn discover_reports_missing_directory_as_error() {
        let dir = temp_dir("missing");
        let missing = dir.join("nope");
        assert!(FftoolsProvider::new(missing).discover().is_err());
        fs::remove_dir_all(&dir).expect("cleanup temp dir");
    }

    /// 对真实 `~/.local/bin` 的冒烟测试：不进交互界面，只验证发现结果。
    ///
    /// 默认不跑（会读真实 HOME）。手动执行：
    /// `cargo test -- --ignored --nocapture`
    #[test]
    #[ignore = "读取真实 HOME 目录，默认跳过"]
    fn smoke_scan_real_home_bin_dir() {
        use std::collections::BTreeMap;

        let Some(home) = std::env::var_os("HOME") else {
            return;
        };
        let dir = PathBuf::from(home).join(".local/bin");
        if !dir.is_dir() {
            return;
        }

        let tools = FftoolsProvider::new(dir)
            .discover()
            .expect("扫描真实目录")
            .tools;
        println!("发现 {} 个工具", tools.len());

        let mut per_tag: BTreeMap<&str, usize> = BTreeMap::new();
        for tool in &tools {
            for tag in &tool.tags {
                *per_tag.entry(tag.as_str()).or_default() += 1;
            }
        }
        for (tag, count) in &per_tag {
            println!("  {tag}: {count}");
        }

        let missing: Vec<&str> = tools
            .iter()
            .filter(|tool| !tool.ready)
            .map(|tool| tool.name.as_str())
            .collect();
        println!("依赖缺失的工具: {missing:?}");

        assert!(!tools.is_empty(), "真实目录里应该有 fzf-* 工具");
        assert!(tools.iter().all(|tool| tool.domain == Domain::Media));
        assert!(tools.iter().all(|tool| tool.provider == "FFTools"));
        assert!(tools.iter().all(|tool| !tool.summary.is_empty()));
        assert!(
            tools.iter().all(|tool| tool.tags.len() == 1),
            "每个 FFTools 工具应恰好有一个二级分类"
        );
        // 第一版是按名字硬编码排除的，这里确认元数据驱动后行为一致。
        assert!(
            tools
                .iter()
                .all(|tool| !tool.name.starts_with("fzf-fftools") && tool.name != "fzf-preview"),
            "内部件（主菜单/共享库/预览器）不该出现在工具列表里"
        );
    }
}
