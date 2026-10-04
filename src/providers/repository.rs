//! Repository Provider：把**已经装到本机**的仓库包里的动作定义变成工具。
//!
//! 为什么单独一个 Provider，而不是让 Manifest Provider 顺手扫一下包目录：
//!
//! * Manifest Provider 管的是「用户自己写的 tools.d」与「随程序发布的内置动作」；
//! * 这里管的是「仓库装来的包」，它的**事实来源是账本**
//!   （installed.toml），而不是某个目录里恰好躺着什么文件。
//!
//! 两者产出的都是 [ToolDefinition]，所以 Registry 照常去重、排序、搜索 —— UI 与
//! 执行层不需要知道这个工具「是装来的」还是「本来就有的」。
//!
//! # 动作定义格式完全复用 manifest
//!
//! 包里的 `manifests/*.toml` 和用户手写的 manifest 是**同一套 schema**，
//! 解析走 [crate::providers::manifest::load_actions] 这唯一一份实现。
//! 仓库里能表达的东西因此不会比手写的少，也不会多出另一套语义。

use std::{
    collections::BTreeSet,
    fs, io,
    path::{Path, PathBuf},
};

use crate::{
    model::ToolDefinition,
    providers::{Discovery, Provider, manifest, metadata},
    repository::installed,
};

const PROVIDER_ID: &str = "repository";
const PROVIDER_LABEL: &str = "仓库";

/// 包载荷里放动作定义的目录名。
const MANIFEST_DIR: &str = "manifests";

pub struct RepositoryProvider {
    /// 数据目录：账本与包载荷都在它下面。
    data_dir: PathBuf,
    /// 可执行脚本装到的目录（`~/.local/bin`）。用来把 `program` 解析成绝对路径。
    bin_dir: PathBuf,
}

impl RepositoryProvider {
    pub fn new(data_dir: PathBuf, bin_dir: PathBuf) -> Self {
        Self { data_dir, bin_dir }
    }

    /// 用当前配置的目录。
    ///
    /// 锚定用的是 `install::bin_dir()`（安装**真正**落到哪儿），不是扫描目录 ——
    /// 两者可能是不同的目录，用错了就会出现「装着却找不到」。
    pub fn with_defaults(_bin_dir: &Path) -> Self {
        Self::new(
            crate::config::data_dir(),
            crate::repository::install::bin_dir(),
        )
    }

    /// 一个包里所有 `manifests/*.toml` 的路径（已排序，顺序稳定）。
    fn manifest_files(&self, package_id: &str) -> Vec<PathBuf> {
        let dir = installed::files_dir(&self.data_dir, package_id).join(MANIFEST_DIR);
        let Ok(read_dir) = fs::read_dir(&dir) else {
            return Vec::new();
        };
        let mut paths: Vec<PathBuf> = read_dir
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.path())
            .filter(|path| path.is_file())
            .filter(|path| path.extension().and_then(|ext| ext.to_str()) == Some("toml"))
            .collect();
        paths.sort();
        paths
    }

    /// 把仓库工具调成「一定能跑起来」的样子。
    ///
    /// 关键一步：如果 `program` 是裸命令名、而它就在我们的 bin 目录里（用户可能
    /// 没把 `~/.local/bin` 放进 PATH），就把 action.program 换成绝对路径。
    /// 这不是优化，是正确性 —— 否则「装好了却跑不起来」。
    fn anchor(&self, tool: &mut ToolDefinition, package_id: &str) {
        let Some(action) = tool.action.as_mut() else {
            return;
        };
        let program = action.program.clone();
        if program.contains('/') {
            return;
        }
        let candidate = self.bin_dir.join(&program);
        if !candidate.is_file() {
            return;
        }
        // program 变了，依赖探测要按**新的**绝对路径重来一遍：
        // 之前按裸名字查的是 PATH，而用户很可能没把 ~/.local/bin 放进 PATH。
        let anchored = candidate.to_string_lossy().to_string();
        action.program = anchored.clone();
        tool.path = candidate.clone();
        let deps = metadata::dependencies_of(std::slice::from_ref(&anchored));
        tool.ready = deps.missing.is_empty();
        tool.missing_deps = deps.missing;
        // 包 **id** 放进标签，有两个用处：
        //   1. 页内筛选条上能按包过滤；
        //   2. 搜索时能用 id 命中 —— 用户装的时候敲的是包名（disk-report），
        //      跑/搜的时候自然还敲包名，而动作名往往是中文的，否则搜不到。
        if !package_id.is_empty() && !tool.tags.contains(&package_id.to_string()) {
            tool.tags.push(package_id.to_string());
        }
    }
}

impl Provider for RepositoryProvider {
    fn id(&self) -> &'static str {
        PROVIDER_ID
    }

    fn label(&self) -> &'static str {
        PROVIDER_LABEL
    }

    fn discover(&self) -> io::Result<Discovery> {
        let mut discovery = Discovery::default();
        let (packages, ledger_warnings) = installed::load_all(&self.data_dir);
        discovery.warnings.extend(ledger_warnings);

        let mut seen: BTreeSet<String> = BTreeSet::new();
        for package in packages {
            for path in self.manifest_files(&package.id) {
                let source = path
                    .strip_prefix(&self.data_dir)
                    .unwrap_or(&path)
                    .display()
                    .to_string();
                let Ok(text) = fs::read_to_string(&path) else {
                    discovery.warnings.push(format!("{source}: 读不了"));
                    continue;
                };

                let (tools, warnings) = manifest::load_actions(
                    &format!("{} 里的 {source}", package.name),
                    &text,
                    PROVIDER_ID,
                    PROVIDER_LABEL,
                );
                discovery.warnings.extend(warnings);

                for mut tool in tools {
                    // 动作 id 在包内唯一，但跨包可能撞：加上包 id 做命名空间。
                    let action_id = tool
                        .id
                        .strip_prefix(&format!("{PROVIDER_ID}:"))
                        .unwrap_or(&tool.id)
                        .to_string();
                    tool.id = format!("{PROVIDER_ID}:{}:{action_id}", package.id);
                    if !seen.insert(tool.id.clone()) {
                        discovery
                            .warnings
                            .push(format!("动作 id 重复「{}」，跳过这一条", tool.id));
                        continue;
                    }
                    self.anchor(&mut tool, &package.id);
                    discovery.tools.push(tool);
                }
            }
        }
        Ok(discovery)
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;
    use crate::{
        model::{Action, Domain, RunMode},
        repository::installed::{self, InstalledFile, InstalledPackage, SCHEMA_VERSION},
    };

    fn temp(tag: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("toolbox-hub-repoprovider-{tag}-{nanos}"));
        fs::create_dir_all(&dir).expect("mkdir");
        dir
    }

    const MANIFEST: &str = r#"
[[action]]
id = "demo-echo"
name = "Demo 回显"
summary = "把参数原样回显"
domain = "工具"
program = "demo-echo"
mode = "capture"

[[action.argument]]
key = "text"
label = "文字"
kind = "text"
required = true
"#;

    /// 装一个假的包：账本 + 载荷里的 manifest（+ 可选的可执行文件）。
    fn install_fake(base: &Path, id: &str, name: &str, manifest: Option<&str>, bin: Option<&str>) {
        let data = base.join("data");
        let bin_dir = base.join("bin");
        fs::create_dir_all(&bin_dir).expect("mkdir bin");

        let mut files = Vec::new();
        if let Some(text) = manifest {
            let path = installed::files_dir(&data, id).join("manifests/demo.toml");
            fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
            fs::write(&path, text).expect("write");
            files.push(InstalledFile {
                path,
                sha256: crate::repository::install::sha256_text(text),
                kind: String::from("manifest"),
                source: String::from("manifests/demo.toml"),
            });
        }
        if let Some(program) = bin {
            let path = bin_dir.join(program);
            fs::write(&path, "#!/bin/sh\nexit 0\n").expect("write bin");
            // 依赖探测看的是执行位，不是「文件存在」—— 夹具也要像真的一样。
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).expect("chmod");
            }
            files.push(InstalledFile {
                path,
                sha256: crate::repository::install::sha256_text("#!/bin/sh\nexit 0\n"),
                kind: String::from("bin"),
                source: format!("scripts/{program}"),
            });
        }

        let package = InstalledPackage {
            schema_version: SCHEMA_VERSION,
            id: id.to_string(),
            name: name.to_string(),
            version: String::from("1.0.0"),
            repository: String::from("official"),
            repository_name: String::from("ToolHub Official"),
            trust: String::from("trusted"),
            installed_at: 0,
            source: None,
            license: None,
            requires_root: false,
            danger: String::from("safe"),
            artifact_sha256: None,
            dependencies: Vec::new(),
            files,
            allow_unverified: false,
        };
        installed::save(&data, &package).expect("save");
    }

    fn provider(base: &Path) -> RepositoryProvider {
        RepositoryProvider::new(base.join("data"), base.join("bin"))
    }

    #[test]
    fn an_installed_package_contributes_its_actions() {
        let base = temp("basic");
        install_fake(
            &base,
            "demo-actions",
            "Demo 动作集",
            Some(MANIFEST),
            Some("demo-echo"),
        );

        let discovery = provider(&base).discover().expect("discover");
        assert!(discovery.warnings.is_empty(), "{:?}", discovery.warnings);
        assert_eq!(discovery.tools.len(), 1);

        let tool = &discovery.tools[0];
        assert_eq!(tool.id, "repository:demo-actions:demo-echo");
        assert_eq!(tool.provider, "仓库");
        assert_eq!(tool.name, "Demo 回显");
        assert_eq!(tool.domain, Domain::Tools);
        assert_eq!(tool.mode, RunMode::Capture);
        assert!(
            tool.tags.contains(&String::from("demo-actions")),
            "包 id 要进标签，这样 search demo-actions 能找到装来的动作：{:?}",
            tool.tags
        );
        std::fs::remove_dir_all(&base).expect("cleanup");
    }

    /// 装到 `~/.local/bin` 的脚本要被解析成**绝对路径** —— 否则 PATH 里没有它就
    /// 会出现「装好了却跑不起来」。
    #[test]
    fn a_program_in_the_bin_dir_is_anchored_to_an_absolute_path() {
        let base = temp("anchor");
        install_fake(
            &base,
            "demo-actions",
            "Demo 动作集",
            Some(MANIFEST),
            Some("demo-echo"),
        );

        let discovery = provider(&base).discover().expect("discover");
        let tool = &discovery.tools[0];
        let expected = base.join("bin/demo-echo");
        assert_eq!(tool.path, expected);
        let action: &Action = tool.action.as_ref().expect("有动作");
        assert_eq!(action.program, expected.to_string_lossy());
        assert!(tool.ready, "锚定到绝对路径之后依赖就该是齐的");
        std::fs::remove_dir_all(&base).expect("cleanup");
    }

    /// 没装出来的程序不该被锚定（保持裸名字，依赖缺失如实报出来）。
    #[test]
    fn a_program_that_is_not_installed_keeps_its_declared_name() {
        let base = temp("noanchor");
        install_fake(&base, "demo-actions", "Demo", Some(MANIFEST), None);

        let discovery = provider(&base).discover().expect("discover");
        let tool = &discovery.tools[0];
        let action = tool.action.as_ref().expect("有动作");
        assert_eq!(action.program, "demo-echo");
        std::fs::remove_dir_all(&base).expect("cleanup");
    }

    #[test]
    fn a_package_without_manifests_contributes_nothing_and_no_noise() {
        let base = temp("nomanifest");
        install_fake(&base, "hello-tool", "Hello", None, Some("hello-tool"));

        let discovery = provider(&base).discover().expect("discover");
        assert!(discovery.tools.is_empty());
        assert!(discovery.warnings.is_empty(), "{:?}", discovery.warnings);
        std::fs::remove_dir_all(&base).expect("cleanup");
    }

    /// 包里的 toolbox.toml 不是动作定义，不该被当成坏 manifest 报错。
    #[test]
    fn the_package_manifest_at_the_payload_root_is_ignored() {
        let base = temp("rootmanifest");
        install_fake(&base, "demo-actions", "Demo", Some(MANIFEST), None);
        let root = installed::files_dir(&base.join("data"), "demo-actions").join("toolbox.toml");
        fs::write(
            &root,
            "[package]\nid = \"demo-actions\"\nversion = \"1.0.0\"\n",
        )
        .expect("write");

        let discovery = provider(&base).discover().expect("discover");
        assert_eq!(discovery.tools.len(), 1);
        assert!(discovery.warnings.is_empty(), "{:?}", discovery.warnings);
        std::fs::remove_dir_all(&base).expect("cleanup");
    }

    /// 一个包里的动作写坏了：只有那一条没了，别的包照常。
    #[test]
    fn a_broken_manifest_in_one_package_does_not_affect_another() {
        let base = temp("broken");
        install_fake(
            &base,
            "broken",
            "坏包",
            Some("[[action]]\nid = \"x\"\nname = \"X\"\n"),
            None,
        );
        install_fake(&base, "good", "好包", Some(MANIFEST), None);

        let discovery = provider(&base).discover().expect("discover");
        assert_eq!(discovery.tools.len(), 1, "{:?}", discovery.tools);
        assert_eq!(discovery.tools[0].name, "Demo 回显");
        assert!(
            discovery
                .warnings
                .iter()
                .any(|warning| warning.contains("summary")),
            "{:?}",
            discovery.warnings
        );
        std::fs::remove_dir_all(&base).expect("cleanup");
    }

    /// 两个包装了同名动作：id 要带上包名，不能互相覆盖。
    #[test]
    fn the_same_action_id_in_two_packages_stays_distinct() {
        let base = temp("collide");
        install_fake(&base, "one", "包一", Some(MANIFEST), Some("demo-echo"));
        install_fake(&base, "two", "包二", Some(MANIFEST), Some("demo-echo"));

        let discovery = provider(&base).discover().expect("discover");
        let ids: Vec<&str> = discovery
            .tools
            .iter()
            .map(|tool| tool.id.as_str())
            .collect();
        assert_eq!(discovery.tools.len(), 2, "{ids:?}");
        assert!(ids.contains(&"repository:one:demo-echo"), "{ids:?}");
        assert!(ids.contains(&"repository:two:demo-echo"), "{ids:?}");
        std::fs::remove_dir_all(&base).expect("cleanup");
    }

    #[test]
    fn an_empty_data_dir_is_not_an_error() {
        let base = temp("empty");
        let discovery = provider(&base).discover().expect("discover");
        assert!(discovery.tools.is_empty());
        assert!(discovery.warnings.is_empty());
        std::fs::remove_dir_all(&base).expect("cleanup");
    }

    #[test]
    fn a_broken_ledger_is_reported_instead_of_hidden() {
        let base = temp("ledger");
        let path = installed::ledger_path(&base.join("data"), "broken");
        fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
        fs::write(&path, "{{{").expect("write");

        let discovery = provider(&base).discover().expect("discover");
        assert!(discovery.tools.is_empty());
        assert_eq!(discovery.warnings.len(), 1);
        assert!(discovery.warnings[0].contains("broken"));
        std::fs::remove_dir_all(&base).expect("cleanup");
    }

    #[test]
    fn non_toml_files_in_the_manifest_dir_are_skipped() {
        let base = temp("nonstd");
        install_fake(&base, "demo", "Demo", Some(MANIFEST), None);
        let dir = installed::files_dir(&base.join("data"), "demo").join("manifests");
        fs::write(dir.join("README.md"), "不是动作定义").expect("write");

        let discovery = provider(&base).discover().expect("discover");
        assert_eq!(discovery.tools.len(), 1);
        assert!(discovery.warnings.is_empty(), "{:?}", discovery.warnings);
        std::fs::remove_dir_all(&base).expect("cleanup");
    }
}
