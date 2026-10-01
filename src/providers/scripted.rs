//! 通用脚本 Provider —— 扫描若干目录，凡是头部带 `# <脚本名>:summary=` 注解的
//! 可执行脚本，都会成为一个工具条目，域与二级分类由注解决定。
//!
//! 与 FFTools Provider 的分工：
//!
//! * FFTools 只认 `fzf-*`，分类写死在 `category_for` 里，整批挂 `媒体` 域；
//! * 这里完全由注解驱动，**新增一个域不需要改 Rust 代码**，给脚本加一行
//!   `# <名>:domain=系统` 就够了。
//!
//! 元数据契约见 [`crate::providers::metadata`]。与 FFTools 一样，本 Provider
//! 只读文件，不改任何脚本。

use std::{
    collections::BTreeSet,
    fs, io,
    path::{Path, PathBuf},
};

use crate::{
    model::{Danger, Domain, RunMode, ToolDefinition},
    providers::{Discovery, Provider, metadata},
};

/// 覆盖扫描根的环境变量，冒号分隔（与 `PATH` 同构）。
pub const ROOTS_ENV: &str = "TOOLBOX_HUB_PATH";

/// 用户自己放注解脚本的地方（相对**配置目录**）。
pub const USER_TOOLS_DIR: &str = "tools";

/// 扫描若干目录，收录带注解的脚本。
pub struct ScriptedProvider {
    roots: Vec<PathBuf>,
}

impl ScriptedProvider {
    pub fn new(roots: Vec<PathBuf>) -> Self {
        Self { roots }
    }

    /// 默认扫描根。
    ///
    /// `TOOLBOX_HUB_PATH` 存在时完全以它为准；否则用
    /// `<bin_dir>`（Toolbox 的启动参数）、`~/bin`、`~/.config/toolbox-hub/tools`、
    /// `/usr/local/bin`。顺序即优先级：同名脚本以先出现的根为准。
    /// 不存在的根目录会被安静跳过 —— 用户还没建 `~/bin` 是常态，不该变成一个报错。
    pub fn with_defaults(bin_dir: &Path) -> Self {
        if let Some(raw) = std::env::var_os(ROOTS_ENV) {
            let roots = std::env::split_paths(&raw).collect::<Vec<_>>();
            if !roots.is_empty() {
                return Self::new(roots);
            }
        }

        let mut roots = vec![bin_dir.to_path_buf()];
        if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
            roots.push(home.join("bin"));
        }
        // 用户脚本目录跟着配置目录走（`--config-dir` 能把它一起挪走）
        roots.push(crate::config::config_dir().join(USER_TOOLS_DIR));
        roots.push(PathBuf::from("/usr/local/bin"));
        Self::new(roots)
    }

    /// 被扫描的根目录。冒烟测试用它打印实际扫描范围。
    #[cfg(test)]
    pub fn roots(&self) -> &[PathBuf] {
        &self.roots
    }

    /// 由脚本名 + 头部元数据构造工具定义。
    fn definition(&self, name: &str, path: PathBuf, head: &str) -> ToolDefinition {
        let summary = metadata::meta(head, name, "summary").unwrap_or_else(|| "-".to_string());
        let deps = metadata::dependencies(head, name);
        let ready = deps.is_ready();
        let domain = metadata::meta(head, name, "domain")
            .and_then(|raw| Domain::parse(&raw))
            .unwrap_or_else(|| guess_domain(name, &summary));

        ToolDefinition {
            id: format!("{}:{name}", self.id()),
            name: name.to_string(),
            provider: self.label().to_string(),
            domain,
            tags: metadata::meta(head, name, "tags")
                .map(|raw| metadata::split_list(&raw))
                .unwrap_or_default(),
            summary,
            input: metadata::meta(head, name, "input"),
            output: metadata::meta(head, name, "output"),
            features: metadata::meta(head, name, "features"),
            requires: deps.required,
            missing_deps: deps.missing,
            install_hint: metadata::meta(head, name, "install"),
            // 注解脚本自己负责交互，不经过参数表单。
            action: None,
            mode: mode_of(head, name),
            danger: danger_of(head, name),
            path,
            ready,
        }
    }
}

impl Provider for ScriptedProvider {
    fn id(&self) -> &'static str {
        "scripted"
    }

    fn label(&self) -> &'static str {
        "本地脚本"
    }

    fn discover(&self) -> io::Result<Discovery> {
        let mut tools = Vec::new();
        // 同名脚本按 PATH 语义处理：多个根目录里存在同名命令时，实际被执行的只有
        // 最先命中的那一个，所以这里也只收录它（顺带保证 tool id 不重复）。
        // 跨 Provider 的同一份文件由 Registry 按路径去重。
        let mut seen_names: BTreeSet<String> = BTreeSet::new();

        for root in &self.roots {
            let Ok(read_dir) = fs::read_dir(root) else {
                continue;
            };
            let mut entries = read_dir.collect::<Result<Vec<_>, _>>()?;
            entries.sort_by_key(|entry| entry.file_name());

            for entry in entries {
                let path = entry.path();
                let name = entry.file_name().to_string_lossy().to_string();
                if !metadata::is_executable(&path) {
                    continue;
                }
                let head = metadata::read_head(&path);
                // 注解即「请把我放进工具箱」：没有 summary 的脚本一律不收录。
                if metadata::meta(&head, &name, "summary").is_none() {
                    continue;
                }
                if metadata::is_internal(&head, &name) {
                    continue;
                }
                if !seen_names.insert(name.clone()) {
                    continue;
                }
                tools.push(self.definition(&name, path, &head));
            }
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

/// 没写 `domain` 注解时，按脚本名与说明猜一个域。
///
/// 猜错的代价很低：工具照样出现，只是落在别的域。所以这里只匹配明显的技术关键词，
/// 猜不出来就归 `工具`；需要精确归属时请显式写 `domain=`。
fn guess_domain(name: &str, summary: &str) -> Domain {
    let haystack = format!("{} {}", name.to_lowercase(), summary);

    const RULES: [(Domain, &[&str]); 5] = [
        (
            Domain::Media,
            &[
                "video", "audio", "ffmpeg", "media", "subtitle", "gif", "视频", "音频", "媒体",
                "字幕",
            ],
        ),
        (
            Domain::Image,
            &[
                "image", "photo", "picture", "png", "jpeg", "jpg", "svg", "graphics", "图像",
                "图片", "壁纸",
            ],
        ),
        (
            Domain::Network,
            &[
                "network", "wget", "curl", "ssh", "download", "proxy", "dns", "firewall", "http",
                "网络", "下载",
            ],
        ),
        (
            Domain::System,
            &[
                "system",
                "systemd",
                "power",
                "battery",
                "backlight",
                "disk",
                "mount",
                "kernel",
                "service",
                "cpu",
                "memory",
                "系统",
                "电源",
                "硬件",
                "磁盘",
            ],
        ),
        (
            Domain::Dev,
            &[
                "git", "build", "cargo", "npm", "lint", "repo", "debug", "compile", "开发", "构建",
                "编译",
            ],
        ),
    ];

    for (domain, keywords) in RULES {
        if keywords.iter().any(|keyword| haystack.contains(keyword)) {
            return domain;
        }
    }
    Domain::Tools
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        path::{Path, PathBuf},
        time::{SystemTime, UNIX_EPOCH},
    };

    use super::{ScriptedProvider, guess_domain};
    use crate::{model::Domain, providers::Provider};

    fn provider(roots: Vec<PathBuf>) -> ScriptedProvider {
        ScriptedProvider::new(roots)
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
    fn definition_reads_domain_tags_and_requirements_from_annotations() {
        let head = "\
# my-tool:summary=把图片批量转成 WebP
# my-tool:domain=图像
# my-tool:tags=转换,批处理
# my-tool:input=图片目录
# my-tool:output=WebP 文件
# my-tool:features=多线程
# my-tool:requires=sh,toolbox-hub-definitely-missing
";
        let tool = provider(Vec::new()).definition("my-tool", PathBuf::from("/tmp/my-tool"), head);

        assert_eq!(tool.id, "scripted:my-tool");
        assert_eq!(tool.provider, "本地脚本");
        assert_eq!(tool.domain, Domain::Image);
        assert_eq!(tool.tags, vec!["转换".to_string(), "批处理".to_string()]);
        assert_eq!(tool.summary, "把图片批量转成 WebP");
        assert_eq!(tool.input.as_deref(), Some("图片目录"));
        assert_eq!(tool.output.as_deref(), Some("WebP 文件"));
        assert_eq!(tool.features.as_deref(), Some("多线程"));
        assert_eq!(tool.requires.len(), 2);
        assert!(!tool.ready, "缺一个依赖就不该算就绪");
    }

    #[test]
    fn definition_without_domain_annotation_falls_back_to_keyword_guess() {
        let tool = provider(Vec::new()).definition(
            "disk-report",
            PathBuf::from("/tmp/disk-report"),
            "# disk-report:summary=磁盘占用报告\n",
        );
        assert_eq!(tool.domain, Domain::System);
        assert!(tool.tags.is_empty(), "没写 tags 就没有二级筛选");
    }

    #[test]
    fn unknown_domain_annotation_falls_back_instead_of_dropping_the_tool() {
        let tool = provider(Vec::new()).definition(
            "mystery",
            PathBuf::from("/tmp/mystery"),
            "# mystery:summary=说不清干什么\n# mystery:domain=星际\n",
        );
        assert_eq!(tool.domain, Domain::Tools);
    }

    #[test]
    fn guess_domain_matches_keywords_and_defaults_to_tools() {
        assert_eq!(guess_domain("gif-maker", "做个动图"), Domain::Media);
        assert_eq!(guess_domain("photo-resize", "缩图"), Domain::Image);
        assert_eq!(guess_domain("dns-check", "查 DNS"), Domain::Network);
        assert_eq!(guess_domain("battery-monitor", "看电量"), Domain::System);
        assert_eq!(guess_domain("repo-lint", "跑 lint"), Domain::Dev);
        assert_eq!(guess_domain("shorin", "修正帮助信息语言"), Domain::Tools);
    }

    #[test]
    fn discover_requires_summary_annotation_and_skips_internal_and_non_executable() {
        let dir = temp_dir("scripted-discover");
        write_script(
            &dir,
            "annotated",
            "#!/usr/bin/env bash\n# annotated:summary=有注解\n# annotated:domain=系统\n",
        );
        write_script(
            &dir,
            "internal",
            "#!/usr/bin/env bash\n# internal:summary=内部件\n# internal:internal=true\n",
        );
        write_script(&dir, "unannotated", "#!/usr/bin/env bash\necho hi\n");
        write_plain(
            &dir,
            "noexec",
            "#!/usr/bin/env bash\n# noexec:summary=没有执行位\n",
        );

        let tools = provider(vec![dir.clone()])
            .discover()
            .expect("discover")
            .tools;
        assert_eq!(tools.len(), 1, "只应留下一个工具: {tools:#?}");
        assert_eq!(tools[0].name, "annotated");
        assert_eq!(tools[0].domain, Domain::System);

        fs::remove_dir_all(&dir).expect("cleanup temp dir");
    }

    #[test]
    fn discover_tolerates_missing_roots_and_dedupes_across_roots() {
        let first = temp_dir("scripted-root-a");
        let second = temp_dir("scripted-root-b");
        write_script(
            &first,
            "shared",
            "#!/usr/bin/env bash\n# shared:summary=来自第一个根\n# shared:domain=开发\n",
        );
        write_script(
            &second,
            "shared",
            "#!/usr/bin/env bash\n# shared:summary=来自第二个根\n# shared:domain=工具\n",
        );
        write_script(
            &second,
            "only-second",
            "#!/usr/bin/env bash\n# only-second:summary=只属于第二个根\n",
        );

        let missing = first.join("does-not-exist");
        let tools = provider(vec![missing, first.clone(), second.clone()])
            .discover()
            .expect("discover")
            .tools;

        assert_eq!(tools.len(), 2, "同名脚本只能出现一次: {tools:#?}");
        let shared = tools
            .iter()
            .find(|t| t.name == "shared")
            .expect("有 shared");
        assert_eq!(shared.summary, "来自第一个根", "先扫到的根优先");
        assert_eq!(shared.domain, Domain::Dev);

        // 同名去重顺带保证了 id 唯一。
        let mut ids: Vec<_> = tools.iter().map(|tool| tool.id.clone()).collect();
        ids.sort();
        let total = ids.len();
        ids.dedup();
        assert_eq!(total, ids.len(), "tool id 必须唯一: {tools:#?}");

        assert!(tools.iter().any(|t| t.name == "only-second"));

        fs::remove_dir_all(&first).expect("cleanup");
        fs::remove_dir_all(&second).expect("cleanup");
    }

    /// 对真实默认扫描根的冒烟测试。默认不跑：
    /// `cargo test -- --ignored --nocapture`
    #[test]
    #[ignore = "读取真实 HOME / /usr/local/bin，默认跳过"]
    fn smoke_scan_real_roots() {
        let Some(home) = std::env::var_os("HOME") else {
            return;
        };
        let bin_dir = PathBuf::from(home).join(".local/bin");
        let provider = ScriptedProvider::with_defaults(&bin_dir);

        let mut surveyed = 0;
        println!("扫描根: {:?}", provider.roots());
        for root in provider.roots() {
            let exists = root.is_dir();
            let count = if exists {
                fs::read_dir(root).map(|it| it.count()).unwrap_or(0)
            } else {
                0
            };
            surveyed += 1;
            println!(
                "  [{surveyed}] {} 存在={exists} 条目={count}",
                root.display()
            );
        }

        let tools = provider.discover().expect("扫描真实根目录").tools;
        println!("通用脚本 Provider 发现 {} 个工具", tools.len());
        for tool in &tools {
            println!(
                "  {} → {} (tags={:?})",
                tool.name,
                tool.domain.label(),
                tool.tags
            );
        }

        // 注解脚本目前全部属于 FFTools，通用 Provider 会扫到它们，
        // 但 Registry 会按路径去重 —— 这里只断言 Provider 自己不含内部件。
        assert!(
            tools.iter().all(|tool| tool.name != "shorin"),
            "shorin 标了 internal=true，不该出现在通用 Provider 的结果里"
        );
        assert!(
            tools
                .iter()
                .all(|tool| !tool.name.starts_with("fzf-preview") && tool.name != "fzf-fftools"),
            "内部件不该被收录"
        );
    }
}
