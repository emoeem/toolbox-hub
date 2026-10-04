//! 插件（仓库包）的作者工具：脚手架 / 校验 / 打包。
//!
//! 这一层服务的不是「装包的人」，而是**写包的人**。它要解决三件让插件生态长不
//! 起来的事：
//!
//! 1. **起步难** —— 目录摆哪儿、TOML 写哪些字段。→ 脚手架
//! 2. **不敢发** —— 索引里的哈希要手算、tar 的坑要自己踩。→ 打包
//! 3. **发错了** —— 路径穿越、符号链接、忘记重打包、依赖没写。→ 校验
//!
//! # 唯一权威是 toolbox.toml
//!
//! 作者只维护包目录，**索引是构建产物**：build_registry 扫描
//! packages/*/toolbox.toml，逐个打包、算哈希、写出 index.json。
//! 手工维护一份 index.json 是这类生态最容易烂掉的地方（版本对不上、哈希忘了改）。
//!
//! # 产物必须可复现
//!
//! 两次构建必须逐字节相同，否则同一份源码会得到两个不同的哈希，「这个哈希是谁
//! 签的」就永远说不清了。所以 tar 条目按路径排序、mtime/uid/gid 全归零、
//! 路径不带 ./ 前缀，gzip 头也不带时间戳。

use std::{
    collections::BTreeSet,
    fs,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};

use crate::{
    model::Danger,
    repository::{
        index::{Artifact, Dependency, FileKind, Index, PackageFile, PackageMeta, SCHEMA_VERSION},
        install::{self, sha256_file},
        paths,
    },
    util::path::{command_available, is_executable},
};

/// 包元数据文件名。
pub const TOOLBOX_TOML: &str = "toolbox.toml";
/// 产物输出目录（相对包目录）。
pub const ARTIFACTS_DIR: &str = "artifacts";
/// 源码目录（相对仓库根）。
pub const PACKAGES_DIR: &str = "packages";

/// 打进归档但**不安装**的文件（客户端拿它核对 id / 版本）。
const ARCHIVE_ONLY: &[&str] = &[TOOLBOX_TOML];

// ── toolbox.toml ────────────────────────────────────────────────────────────

/// 一个包目录的 toolbox.toml。
///
/// 一律 deny_unknown_fields：作者拼错一个字段名，应该当场报出来，而不是被静默
/// 忽略、然后对着「怎么没生效」发呆。（索引是**远端**格式，所以那边正相反。）
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PackageManifest {
    pub package: PackageSection,
    /// 显式声明要安装哪些文件。不写就按目录约定推导。
    #[serde(default)]
    pub files: Vec<FileEntry>,
    /// 显式声明的外部命令依赖。动作里的 program 会自动补进来。
    #[serde(default)]
    pub dependencies: Vec<DependencyEntry>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PackageSection {
    pub id: String,
    pub name: String,
    pub version: String,
    pub summary: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub categories: Vec<String>,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub author: Option<String>,
    #[serde(default)]
    pub license: Option<String>,
    #[serde(default)]
    pub source: Option<String>,
    #[serde(default)]
    pub homepage: Option<String>,
    #[serde(default)]
    pub requires_root: bool,
    #[serde(default)]
    pub danger: Option<String>,
    /// 依赖缺失时给用户看的安装办法。
    #[serde(default)]
    pub install: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct FileEntry {
    pub path: String,
    #[serde(default)]
    pub kind: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DependencyEntry {
    pub command: String,
    #[serde(default)]
    pub hint: Option<String>,
}

impl PackageManifest {
    pub fn parse(text: &str) -> Result<Self, String> {
        toml::from_str(text).map_err(|error| format!("toolbox.toml 读不懂：{error}"))
    }

    /// 读一个包目录的 toolbox.toml。
    pub fn load(dir: &Path) -> Result<Self, String> {
        let file = dir.join(TOOLBOX_TOML);
        let text = fs::read_to_string(&file)
            .map_err(|error| format!("读不了 {}：{error}", file.display()))?;
        Self::parse(&text)
    }
}

// ── 载荷 ────────────────────────────────────────────────────────────────────

/// 一个要安装的文件。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PayloadFile {
    pub relative: PathBuf,
    pub kind: FileKind,
}

/// 一个包的载荷。
#[derive(Clone, Debug, Default)]
pub struct Payload {
    /// 打进归档的全部文件（相对路径，已排序），含 toolbox.toml。
    pub archive: Vec<PathBuf>,
    /// 要安装的文件（装到用户机器上的那些）。
    pub install: Vec<PayloadFile>,
    /// 打进归档但不会安装的文件（例如 toolbox.toml）。
    pub archive_only: Vec<PathBuf>,
}

/// 按目录约定给一个相对路径分类；None 表示「不认识，只进归档不安装」。
fn kind_for(relative: &Path) -> Option<FileKind> {
    let text = relative.to_string_lossy().replace('\\', "/");
    if let Some((top, _rest)) = text.split_once('/') {
        return match top {
            "scripts" | "bin" => Some(FileKind::Bin),
            "manifests" => Some(FileKind::Manifest),
            "docs" => Some(FileKind::Doc),
            "data" => Some(FileKind::Data),
            _ => None,
        };
    }
    match text.as_str() {
        "README.md" | "LICENSE" => Some(FileKind::Doc),
        _ => None,
    }
}

/// 走一遍包目录，收集载荷。
///
/// 规则：只收普通文件；**符号链接一律报错**（客户端会拒绝，早点说不比晚点说好）；
/// artifacts/、.git 与隐藏文件跳过；路径按字典序排序（可复现的前提）。
pub fn collect_payload(dir: &Path) -> Result<Payload, String> {
    let mut payload = Payload::default();
    walk(dir, dir, &mut payload, 0)?;
    payload.archive.sort();
    payload.install.sort_by(|a, b| a.relative.cmp(&b.relative));
    payload.archive_only.sort();
    Ok(payload)
}

fn walk(root: &Path, dir: &Path, payload: &mut Payload, depth: usize) -> Result<(), String> {
    if depth > 8 {
        return Err(format!("{} 目录嵌套太深（上限 8 层）", dir.display()));
    }
    let entries =
        fs::read_dir(dir).map_err(|error| format!("读不了 {}：{error}", dir.display()))?;
    let mut names: Vec<PathBuf> = entries
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .collect();
    names.sort();

    for path in names {
        let relative = path
            .strip_prefix(root)
            .map_err(|_| format!("{} 不在 {} 里", path.display(), root.display()))?
            .to_path_buf();
        let name = path
            .file_name()
            .map(|name| name.to_string_lossy().to_string())
            .unwrap_or_default();

        // 构建产物、版本控制、编辑器垃圾一律不进包。
        if name.starts_with('.') || name == ARTIFACTS_DIR || name == "index.json" {
            continue;
        }

        let metadata = fs::symlink_metadata(&path)
            .map_err(|error| format!("读不了 {}：{error}", path.display()))?;
        if metadata.file_type().is_symlink() {
            return Err(format!(
                "{} 是符号链接 —— 包里的符号链接会被客户端拒绝（也是最经典的越界写文件手法），请改成普通文件",
                relative.display()
            ));
        }
        if metadata.is_dir() {
            walk(root, &path, payload, depth + 1)?;
            continue;
        }
        if !metadata.is_file() {
            return Err(format!("{} 不是普通文件", relative.display()));
        }

        // 包内路径必须干净（.. 与绝对路径都不许）。
        let safe = paths::safe_relative(&relative.to_string_lossy())?;
        payload.archive.push(safe.clone());

        if ARCHIVE_ONLY.contains(&safe.to_string_lossy().as_ref()) {
            payload.archive_only.push(safe);
            continue;
        }
        match kind_for(&safe) {
            Some(kind) => payload.install.push(PayloadFile {
                relative: safe,
                kind,
            }),
            None => payload.archive_only.push(safe),
        }
    }
    Ok(())
}

/// 把显式声明与实际推导的依赖合并。
///
/// 自动那条规则很实用：动作里 program = "jq" 就说明这个包依赖 jq ——
/// 作者不用再手抄一遍，抄漏了界面就会显示「就绪」，然后一跑就找不到命令。
pub fn merge_dependencies(
    manifest: &PackageManifest,
    facts: &ManifestFacts,
    payload: &Payload,
) -> Vec<Dependency> {
    let provided: BTreeSet<String> = payload
        .install
        .iter()
        .filter(|file| file.kind == FileKind::Bin)
        .filter_map(|file| {
            file.relative
                .file_name()
                .map(|name| name.to_string_lossy().to_string())
        })
        .collect();

    let mut out: Vec<Dependency> = Vec::new();
    let mut push = |command: String, hint: Option<String>| {
        let command = command.trim().to_string();
        if command.is_empty() || provided.contains(&command) {
            return;
        }
        if out.iter().any(|existing| existing.command == command) {
            return;
        }
        out.push(Dependency { command, hint });
    };

    // toolbox.toml 里声明的 + 动作文件里声明的（两边都收，见 manifest.rs 的注释）
    for dependency in &manifest.dependencies {
        push(dependency.command.clone(), dependency.hint.clone());
    }
    for (command, hint) in &facts.dependencies {
        push(command.clone(), hint.clone());
    }
    for program in &facts.programs {
        if program.contains('/') {
            continue;
        }
        push(program.clone(), manifest.package.install.clone());
    }
    out.sort_by(|a, b| a.command.cmp(&b.command));
    out
}

// ── 打包 ────────────────────────────────────────────────────────────────────

/// 一次打包的结果。
#[derive(Clone, Debug)]
pub struct BuiltPackage {
    pub id: String,
    pub version: String,
    /// 产物文件。
    pub artifact: PathBuf,
    /// 产物在索引里该写的相对地址（相对 index.json）。
    pub artifact_url: String,
    pub artifact_sha256: String,
    pub artifact_size: u64,
    pub files: Vec<(PayloadFile, String)>,
    pub warnings: Vec<String>,
}

impl BuiltPackage {
    /// 编译成索引条目。
    pub fn index_entry(
        &self,
        manifest: &PackageManifest,
        dependencies: Vec<Dependency>,
    ) -> PackageMeta {
        let package = &manifest.package;
        PackageMeta {
            id: package.id.clone(),
            name: package.name.clone(),
            version: package.version.clone(),
            summary: Some(package.summary.clone()),
            description: package.description.clone(),
            categories: package.categories.clone(),
            tags: package.tags.clone(),
            author: package.author.clone(),
            license: package.license.clone(),
            source: package.source.clone(),
            homepage: package.homepage.clone(),
            dependencies,
            requires_root: package.requires_root,
            danger: package.danger.clone(),
            artifact: Some(Artifact {
                url: self.artifact_url.clone(),
                sha256: Some(self.artifact_sha256.clone()),
                kind: Some(String::from("tar.gz")),
                size: Some(self.artifact_size),
            }),
            files: self
                .files
                .iter()
                .map(|(file, digest)| PackageFile {
                    path: file.relative.to_string_lossy().to_string(),
                    kind: Some(kind_id(file.kind).to_string()),
                    sha256: Some(digest.clone()),
                    executable: None,
                    ..PackageFile::default()
                })
                .collect(),
            extra: Default::default(),
        }
    }
}

fn kind_id(kind: FileKind) -> &'static str {
    match kind {
        FileKind::Bin => "bin",
        FileKind::Manifest => "manifest",
        FileKind::Doc => "doc",
        FileKind::Data => "data",
    }
}

/// 打包一个包目录。
pub fn build_package(dir: &Path, artifacts_dir: &Path) -> Result<BuiltPackage, String> {
    let manifest = PackageManifest::load(dir)?;
    let payload = collect_payload(dir)?;
    if payload.install.is_empty() {
        return Err(format!(
            "「{}」没有任何可安装的文件 —— 把脚本放进 scripts/、动作定义放进 manifests/",
            manifest.package.id
        ));
    }

    fs::create_dir_all(artifacts_dir)
        .map_err(|error| format!("建不了 {}：{error}", artifacts_dir.display()))?;
    let name = format!(
        "{}-{}.tar.gz",
        manifest.package.id, manifest.package.version
    );
    let artifact = artifacts_dir.join(&name);

    write_archive(dir, &payload.archive, &artifact)?;

    let artifact_sha256 = sha256_file(&artifact)?;
    let artifact_size = fs::metadata(&artifact)
        .map_err(|error| format!("读不了 {}：{error}", artifact.display()))?
        .len();

    let mut files = Vec::new();
    for file in &payload.install {
        let digest = sha256_file(&dir.join(&file.relative))?;
        files.push((file.clone(), digest));
    }

    // toolbox.toml 是**故意**只进归档不安装的（客户端拿它核对 id / 版本），
    // 所以它不该出现在「不认识的目录」那条提醒里 —— 那会让人以为自己放错了。
    let unexpected: Vec<String> = payload
        .archive_only
        .iter()
        .map(|path| path.to_string_lossy().to_string())
        .filter(|name| !ARCHIVE_ONLY.contains(&name.as_str()))
        .collect();
    let mut warnings = Vec::new();
    if !unexpected.is_empty() {
        warnings.push(format!(
            "这些文件会进产物但不会安装（目录约定不识别）：{}",
            unexpected.join(", ")
        ));
    }

    Ok(BuiltPackage {
        id: manifest.package.id.clone(),
        version: manifest.package.version.clone(),
        artifact,
        artifact_url: format!("{ARTIFACTS_DIR}/{name}"),
        artifact_sha256,
        artifact_size,
        files,
        warnings,
    })
}

/// 写一个**可复现**的 tar.gz。
///
/// 可复现的定义就是「同样的输入 → 逐字节相同的输出」，所以每个会飘的字段都得摁住：
/// 条目顺序、mtime、uid/gid、路径写法、gzip 头时间戳。
fn write_archive(dir: &Path, entries: &[PathBuf], out: &Path) -> Result<(), String> {
    use flate2::{Compression, GzBuilder};

    let file =
        fs::File::create(out).map_err(|error| format!("写不了 {}：{error}", out.display()))?;
    let encoder = GzBuilder::new().mtime(0).write(file, Compression::best());
    let mut builder = tar::Builder::new(encoder);
    builder.mode(tar::HeaderMode::Deterministic);

    for relative in entries {
        let source = dir.join(relative);
        let bytes =
            fs::read(&source).map_err(|error| format!("读不了 {}：{error}", source.display()))?;
        let executable = kind_for(relative) == Some(FileKind::Bin);

        let mut header = tar::Header::new_gnu();
        header.set_entry_type(tar::EntryType::Regular);
        header.set_size(bytes.len() as u64);
        header.set_mode(if executable { 0o755 } else { 0o644 });
        header.set_uid(0);
        header.set_gid(0);
        header.set_mtime(0);
        // 路径**不带 ./ 前缀**：客户端会把 ./ 当非法路径拒绝（这是真踩过的坑）。
        builder
            .append_data(&mut header, relative, bytes.as_slice())
            .map_err(|error| format!("写不了归档条目 {}：{error}", relative.display()))?;
    }

    let encoder = builder
        .into_inner()
        .map_err(|error| format!("归档收尾失败：{error}"))?;
    encoder
        .finish()
        .map_err(|error| format!("压缩收尾失败：{error}"))?;
    Ok(())
}

// ── 构建整个仓库 ────────────────────────────────────────────────────────────

/// 一次仓库构建的结果。
#[derive(Clone, Debug, Default)]
pub struct RegistryBuild {
    pub index_path: PathBuf,
    pub packages: Vec<String>,
    pub warnings: Vec<String>,
}

/// 扫 <root>/packages/*/ 重建 <root>/index.json。
///
/// 默认不写 updated（那会让每次构建都产生一个不同的索引，破坏可复现）；
/// 要盖时间戳就传 stamp = true。
pub fn build_registry(root: &Path, stamp: bool) -> Result<RegistryBuild, String> {
    let packages_dir = root.join(PACKAGES_DIR);
    let artifacts_dir = root.join(ARTIFACTS_DIR);
    let mut dirs: Vec<PathBuf> = fs::read_dir(&packages_dir)
        .map_err(|error| format!("读不了 {}：{error}", packages_dir.display()))?
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| path.is_dir())
        .collect();
    dirs.sort();

    let mut result = RegistryBuild::default();
    let mut entries: Vec<PackageMeta> = Vec::new();

    for dir in dirs {
        let name = dir
            .file_name()
            .map(|name| name.to_string_lossy().to_string())
            .unwrap_or_default();
        if !dir.join(TOOLBOX_TOML).is_file() {
            return Err(format!(
                "packages/{name}/ 里没有 {TOOLBOX_TOML} —— 每个包目录都必须有它（用 toolbox-hub new {name} 生成一个）"
            ));
        }
        let manifest = PackageManifest::load(&dir)?;
        // 目录名就是包的 id：不然索引里的 id 和 packages/<id>/ 会对不上，
        // 别人 clone 下来就跑不了 check。
        if manifest.package.id != name {
            return Err(format!(
                "packages/{name}/toolbox.toml 里的 id 是「{}」—— 必须和目录名一致",
                manifest.package.id
            ));
        }
        let built = build_package(&dir, &artifacts_dir)?;
        result.packages.push(built.id.clone());
        result.warnings.extend(
            built
                .warnings
                .iter()
                .map(|warning| format!("{}：{warning}", built.id)),
        );

        let payload = collect_payload(&dir)?;
        let facts = manifest_facts(&dir)?;
        let dependencies = merge_dependencies(&manifest, &facts, &payload);
        entries.push(built.index_entry(&manifest, dependencies));
    }

    entries.sort_by(|a, b| a.id.cmp(&b.id));
    for pair in entries.windows(2) {
        if pair[0].id == pair[1].id {
            return Err(format!("有两个包用了同一个 id：{}", pair[0].id));
        }
    }

    let index_path = root.join("index.json");
    let name = previous_name(&index_path).or_else(|| {
        root.file_name()
            .map(|name| name.to_string_lossy().to_string())
    });
    let updated = stamp.then(|| {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|delta| delta.as_secs())
            .unwrap_or(0);
        format!("epoch:{now}")
    });

    let index = Index {
        schema_version: SCHEMA_VERSION,
        name,
        updated,
        packages: entries,
        extra: Default::default(),
    };
    let mut text =
        serde_json::to_string_pretty(&index).map_err(|error| format!("写不出 JSON：{error}"))?;
    text.push('\n');
    fs::write(&index_path, text)
        .map_err(|error| format!("写不了 {}：{error}", index_path.display()))?;
    result.index_path = index_path;
    Ok(result)
}

fn previous_name(index_path: &Path) -> Option<String> {
    let text = fs::read_to_string(index_path).ok()?;
    Index::parse(&text).ok()?.index.name
}

/// 一份包的动作定义里能推出来的事实。
#[derive(Clone, Debug, Default)]
pub struct ManifestFacts {
    /// 动作里声明要跑的 program（去重、保序）。
    pub programs: Vec<String>,
    /// 动作文件里额外声明的依赖 [[dependencies]]：(命令, 提示)。
    pub dependencies: Vec<(String, Option<String>)>,
}

/// 扫 manifests/*.toml，收齐 program 与依赖声明。
pub fn manifest_facts(dir: &Path) -> Result<ManifestFacts, String> {
    let manifests_dir = dir.join("manifests");
    let Ok(entries) = fs::read_dir(&manifests_dir) else {
        return Ok(ManifestFacts::default());
    };
    let mut paths: Vec<PathBuf> = entries
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| path.extension().and_then(|ext| ext.to_str()) == Some("toml"))
        .collect();
    paths.sort();

    let mut facts = ManifestFacts::default();
    for path in paths {
        let text = fs::read_to_string(&path)
            .map_err(|error| format!("读不了 {}：{error}", path.display()))?;
        // 复用工具箱自己那套 manifest 解析：作者写的动作和内置动作必须同一条路。
        let (tools, _warnings) = crate::providers::manifest::load_actions(
            &path.display().to_string(),
            &text,
            "author",
            "Authoring",
        );
        for tool in tools {
            if let Some(action) = tool.action
                && !facts.programs.contains(&action.program)
            {
                facts.programs.push(action.program);
            }
        }
        for (command, hint) in crate::providers::manifest::declared_dependencies(&text) {
            if !facts
                .dependencies
                .iter()
                .any(|existing: &(String, Option<String>)| existing.0 == command)
            {
                facts.dependencies.push((command, hint));
            }
        }
    }
    Ok(facts)
}

// ── 校验 ────────────────────────────────────────────────────────────────────

/// 一次校验的结果。
#[derive(Clone, Debug, Default)]
pub struct CheckReport {
    /// 必须修的。
    pub errors: Vec<String>,
    /// 该看看的。
    pub warnings: Vec<String>,
    /// 通过的项目（让人知道检查真的跑了）。
    pub passed: Vec<String>,
}

impl CheckReport {
    pub fn ok(&self) -> bool {
        self.errors.is_empty()
    }

    pub fn merge(&mut self, other: CheckReport) {
        self.errors.extend(other.errors);
        self.warnings.extend(other.warnings);
        self.passed.extend(other.passed);
    }

    pub fn summary(&self) -> String {
        format!(
            "{} 项通过 · {} 个警告 · {} 个错误",
            self.passed.len(),
            self.warnings.len(),
            self.errors.len()
        )
    }
}

/// 校验一个包目录。
pub fn check_package(dir: &Path) -> CheckReport {
    let mut report = CheckReport::default();

    let manifest = match PackageManifest::load(dir) {
        Ok(manifest) => {
            report.passed.push(format!("{TOOLBOX_TOML} 解析通过"));
            manifest
        }
        Err(problem) => {
            report.errors.push(problem);
            return report;
        }
    };

    match paths::safe_identifier(&manifest.package.id) {
        Ok(id) => {
            let dir_name = dir
                .file_name()
                .map(|name| name.to_string_lossy().to_string())
                .unwrap_or_default();
            if dir_name != id {
                report.errors.push(format!(
                    "目录名是「{dir_name}」，而 toolbox.toml 里的 id 是「{id}」—— 两者必须一致（build 按目录找包）"
                ));
            } else {
                report.passed.push(format!("id「{id}」与目录名一致"));
            }
        }
        Err(problem) => report.errors.push(format!("id 有问题：{problem}")),
    }
    if crate::repository::version::Version::parse(&manifest.package.version).is_none() {
        report.errors.push(format!(
            "版本号「{}」读不懂（要像 1.2.0）",
            manifest.package.version
        ));
    } else {
        report
            .passed
            .push(format!("版本 {}", manifest.package.version));
    }
    if manifest.package.summary.trim().is_empty() {
        report
            .errors
            .push(String::from("summary 是空的（搜索与列表都靠它）"));
    }
    if let Some(raw) = manifest.package.danger.as_deref()
        && Danger::parse(raw).is_none()
    {
        report
            .errors
            .push(format!("danger 只认 safe / caution，收到：{raw}"));
    }

    let payload = match collect_payload(dir) {
        Ok(payload) => {
            report.passed.push(format!(
                "载荷 {} 个文件（安装 {} 个）",
                payload.archive.len(),
                payload.install.len()
            ));
            payload
        }
        Err(problem) => {
            report.errors.push(problem);
            return report;
        }
    };
    if payload.install.is_empty() {
        report.errors.push(String::from(
            "没有任何可安装的文件：脚本放 scripts/、动作放 manifests/",
        ));
    }
    for path in &payload.archive_only {
        if path.to_string_lossy() == TOOLBOX_TOML {
            continue;
        }
        report.warnings.push(format!(
            "{} 进了产物但不会安装 —— 想让它装上的话，放进 scripts/ manifests/ docs/ data/ 之一",
            path.display()
        ));
    }
    for file in &payload.install {
        if file.kind == FileKind::Bin && !is_executable(&dir.join(&file.relative)) {
            report.warnings.push(format!(
                "{} 没有可执行位（装的时候会被设上，但源码里也该 chmod +x）",
                file.relative.display()
            ));
        }
    }

    // 动作定义
    let manifests_dir = dir.join("manifests");
    let mut action_count = 0usize;
    if manifests_dir.is_dir() {
        let mut paths: Vec<PathBuf> = fs::read_dir(&manifests_dir)
            .map(|entries| {
                entries
                    .filter_map(|entry| entry.ok())
                    .map(|entry| entry.path())
                    .collect()
            })
            .unwrap_or_default();
        paths.sort();
        for path in paths {
            if path.extension().and_then(|ext| ext.to_str()) != Some("toml") {
                continue;
            }
            match fs::read_to_string(&path) {
                Ok(text) => {
                    let (tools, warnings) = crate::providers::manifest::load_actions(
                        &path.display().to_string(),
                        &text,
                        "author",
                        "Authoring",
                    );
                    action_count += tools.len();
                    report.warnings.extend(
                        warnings
                            .into_iter()
                            .map(|warning| format!("动作定义：{warning}")),
                    );
                }
                Err(error) => report
                    .errors
                    .push(format!("读不了 {}：{error}", path.display())),
            }
        }
        if action_count == 0 {
            report.warnings.push(String::from(
                "manifests/ 里一个可用动作都没有 —— 这个包装上后不会在界面里出现任何入口",
            ));
        } else {
            report
                .passed
                .push(format!("{action_count} 个动作定义解析通过"));
        }
    } else if !payload
        .install
        .iter()
        .any(|file| file.kind == FileKind::Bin)
    {
        report.warnings.push(String::from(
            "既没有 manifests/ 也没有 scripts/ —— 这个包只会往磁盘上放几个文件",
        ));
    }

    // 依赖：动作里用到的命令是否真的在
    let facts = manifest_facts(dir).unwrap_or_default();
    let dependencies = merge_dependencies(&manifest, &facts, &payload);
    for dependency in &dependencies {
        if command_available(&dependency.command) {
            report
                .passed
                .push(format!("依赖 {} 在 PATH 上", dependency.command));
        } else {
            report.warnings.push(format!(
                "依赖「{}」本机没有 —— 用户那边缺了会显示「依赖缺失」{}",
                dependency.command,
                dependency
                    .hint
                    .as_deref()
                    .map(|hint| format!("（提示：{hint}）"))
                    .unwrap_or_default()
            ));
        }
    }
    report
        .passed
        .push(format!("自动推导出 {} 个依赖", dependencies.len()));

    report
}

/// 校验一个仓库：索引 ↔ 产物 ↔ 源码三方对得上吗。
pub fn check_registry(root: &Path) -> CheckReport {
    let mut report = CheckReport::default();
    let index_path = root.join("index.json");

    let text = match fs::read_to_string(&index_path) {
        Ok(text) => text,
        Err(error) => {
            report.errors.push(format!(
                "读不了 {}：{error}（先跑一次 build）",
                index_path.display()
            ));
            return report;
        }
    };
    let parsed = match Index::parse(&text) {
        Ok(parsed) => parsed,
        Err(problem) => {
            report.errors.push(problem);
            return report;
        }
    };
    report.passed.push(format!(
        "索引解析通过：{} 个包",
        parsed.index.packages.len()
    ));
    report.warnings.extend(parsed.warnings);

    let mut indexed: BTreeSet<String> = BTreeSet::new();
    for package in &parsed.index.packages {
        indexed.insert(package.id.clone());
        let dir = root.join(PACKAGES_DIR).join(&package.id);

        if !dir.is_dir() {
            report.errors.push(format!(
                "索引里的「{}」在 packages/ 下没有源码目录 —— 索引是按源码生成的",
                package.id
            ));
            continue;
        }
        report.merge(check_package(&dir));

        // 索引 <-> **源码**：改了源码忘记重新打包，是这类生态里最容易发生的
        // 「发出去的东西和源码对不上」。只比对索引与产物是查不出来的 ——
        // 那两个都还是旧的那一份，彼此当然一致。
        match collect_payload(&dir) {
            Ok(payload) => {
                let mut declared: BTreeSet<String> = BTreeSet::new();
                for file in &payload.install {
                    let name = file.relative.to_string_lossy().to_string();
                    declared.insert(name.clone());
                    let Some(entry) = package.files.iter().find(|entry| entry.path == name) else {
                        report.errors.push(format!(
                            "{}：源码里的 {name} 不在索引的 files[] 里 —— 跑一次 build",
                            package.id
                        ));
                        continue;
                    };
                    let Some(want) = entry.sha256() else {
                        continue;
                    };
                    match sha256_file(&dir.join(&file.relative)) {
                        Ok(now) if now == want => {}
                        Ok(now) => report.errors.push(format!(
                            "{}：源码里的 {} 与索引声明的不一致（索引 {want} / 现在 {now}）—— 改了没重新 build",
                            package.id, name
                        )),
                        Err(problem) => report.errors.push(problem),
                    }
                }
                for file in &package.files {
                    if !declared.contains(&file.path) {
                        report.errors.push(format!(
                            "{}：索引里的 {} 在源码里已经没有了 —— 跑一次 build",
                            package.id, file.path
                        ));
                    }
                }
            }
            Err(problem) => report.errors.push(problem),
        }

        let Some(artifact) = &package.artifact else {
            report
                .errors
                .push(format!("{}：索引里没有 artifact", package.id));
            continue;
        };
        let artifact_path = PathBuf::from(install::resolve_url(
            Some(&index_path.to_string_lossy()),
            &artifact.url,
        ));
        if !artifact_path.is_file() {
            report.errors.push(format!(
                "{}：产物 {} 不存在（跑一次 build）",
                package.id,
                artifact_path.display()
            ));
            continue;
        }
        match sha256_file(&artifact_path) {
            Ok(digest) => match artifact.sha256() {
                Some(declared) if declared == digest => {
                    report.passed.push(format!("{}：产物哈希一致", package.id));
                }
                Some(declared) => report.errors.push(format!(
                    "{}：产物哈希对不上（索引 {declared} / 实际 {digest}）—— 改了源码没重新 build",
                    package.id
                )),
                None => report.errors.push(format!(
                    "{}：索引里的 artifact.sha256 缺失或格式不对",
                    package.id
                )),
            },
            Err(problem) => report.errors.push(problem),
        }

        match archive_digests(&artifact_path) {
            Ok(entries) => {
                for file in &package.files {
                    let Some(digest) = entries.get(&file.path) else {
                        report.errors.push(format!(
                            "{}：产物里没有索引声明的文件「{}」",
                            package.id, file.path
                        ));
                        continue;
                    };
                    match file.sha256() {
                        Some(declared) if declared == *digest => {}
                        Some(declared) => report.errors.push(format!(
                            "{}：{} 的内容哈希对不上（索引 {declared} / 产物 {digest}）",
                            package.id, file.path
                        )),
                        None => report
                            .warnings
                            .push(format!("{}：{} 没有声明 sha256", package.id, file.path)),
                    }
                }
                for name in entries.keys() {
                    let installed = package.files.iter().any(|file| &file.path == name);
                    let archive_only = ARCHIVE_ONLY.contains(&name.as_str());
                    if !installed && !archive_only {
                        report.warnings.push(format!(
                            "{}：产物里的「{name}」既不在 files[] 里、也不是 toolbox.toml —— 它不会被安装",
                            package.id
                        ));
                    }
                }
                report
                    .passed
                    .push(format!("{}：产物条目与索引一致", package.id));
            }
            Err(problem) => report.errors.push(format!("{}：{problem}", package.id)),
        }
    }

    if let Ok(entries) = fs::read_dir(root.join(PACKAGES_DIR)) {
        for path in entries
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.path())
        {
            if !path.is_dir() {
                continue;
            }
            let id = path
                .file_name()
                .map(|name| name.to_string_lossy().to_string())
                .unwrap_or_default();
            if !indexed.contains(&id) {
                report
                    .errors
                    .push(format!("packages/{id}/ 存在但索引里没有它 —— 跑一次 build"));
            }
        }
    }

    report
}

/// 读一个 tar.gz 里每个文件的内容哈希。
fn archive_digests(path: &Path) -> Result<std::collections::BTreeMap<String, String>, String> {
    use std::io::Read;

    let file =
        fs::File::open(path).map_err(|error| format!("读不了 {}：{error}", path.display()))?;
    let decoder = flate2::read::GzDecoder::new(file);
    let mut archive = tar::Archive::new(decoder);
    let entries = archive
        .entries()
        .map_err(|error| format!("读不了归档：{error}"))?;

    let mut out = std::collections::BTreeMap::new();
    for entry in entries {
        let mut entry = entry.map_err(|error| format!("读不了归档条目：{error}"))?;
        let path = entry
            .path()
            .map_err(|error| format!("归档里有读不出来的路径：{error}"))?
            .to_path_buf();
        let name = path.to_string_lossy().replace('\\', "/");
        if name.starts_with("./") {
            return Err(format!(
                "归档条目「{name}」带 ./ 前缀 —— 客户端会把 ./ 当非法路径拒绝，重新 build 一次"
            ));
        }
        if !entry.header().entry_type().is_file() {
            return Err(format!("归档里有非普通文件条目：{name}"));
        }
        let mut bytes = Vec::new();
        entry
            .read_to_end(&mut bytes)
            .map_err(|error| format!("读不了 {name}：{error}"))?;
        out.insert(name, install::sha256_bytes(&bytes));
    }
    Ok(out)
}

// `is_executable` / `command_available` 搬去了 [`crate::util::path`]：全项目
// 只有那一份 PATH 查找（执行层、Provider 元数据层、这里共用同一套边界）。
// 以前这里判「是文件」、那边判「带执行位」，同一个依赖会出现两处结论不一致。

// ── 脚手架 ──────────────────────────────────────────────────────────────────

/// 新建包时选哪种形状。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScaffoldKind {
    /// 自带脚本：包里有 scripts/<名字>，动作调用它。
    Script,
    /// 只写配方：包装一个**已经装好的** CLI，包里没有脚本。
    Recipe,
}

impl ScaffoldKind {
    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_lowercase().as_str() {
            "script" | "脚本" => Some(ScaffoldKind::Script),
            "recipe" | "配方" => Some(ScaffoldKind::Recipe),
            _ => None,
        }
    }

    pub fn id(self) -> &'static str {
        match self {
            ScaffoldKind::Script => "script",
            ScaffoldKind::Recipe => "recipe",
        }
    }
}

/// 造一个新包骨架，返回写出的文件。
pub fn scaffold(
    root: &Path,
    name: &str,
    kind: ScaffoldKind,
    description: Option<&str>,
    domain: Option<&str>,
    program: Option<&str>,
) -> Result<Vec<PathBuf>, String> {
    let id = paths::safe_identifier(name)?;
    let dir = root.join(&id);
    if dir.exists() {
        return Err(format!("{} 已经存在了", dir.display()));
    }

    let summary = description
        .map(str::to_string)
        .unwrap_or_else(|| format!("{id} 做什么（改掉这句话）"));
    let domain = domain.unwrap_or("工具");
    if crate::model::Domain::parse(domain).is_none() {
        return Err(format!(
            "不认识的域「{domain}」；可用的是：媒体 / 图像 / 系统 / 网络 / 开发 / 工具 / 包管理 / 打包 / 发现"
        ));
    }

    let mut written = Vec::new();
    fs::create_dir_all(dir.join("manifests")).map_err(|error| error.to_string())?;
    fs::create_dir_all(dir.join("docs")).map_err(|error| error.to_string())?;

    let manifest_text = format!(
        r#"# 字段说明见 docs/plugin-authoring.md。改完跑：
#   toolbox-hub check .
#   toolbox-hub build .

[package]
id = "{id}"
name = "{id}"
version = "0.1.0"
summary = "{summary}"
description = ""
categories = []
tags = []
author = ""
license = "MIT"
danger = "safe"
requires_root = false
"#
    );
    write_file(&dir.join(TOOLBOX_TOML), &manifest_text, &mut written)?;

    let program = match (kind, program) {
        (_, Some(program)) => program.to_string(),
        (ScaffoldKind::Script, None) => id.clone(),
        (ScaffoldKind::Recipe, None) => String::from("jq"),
    };
    let action_id = format!("{id}-run");
    let manifest_body = match kind {
        ScaffoldKind::Script => format!(
            r#"# 一个动作 = 界面上一条可填表单 + 一条命令（argv，不经过 shell）。
# 完整字段见 docs/plugin-authoring.md。

[[action]]
id = "{action_id}"
name = "{id}"
summary = "{summary}"
domain = "{domain}"
program = "{program}"
mode = "capture"
input = "要处理的东西"
output = "处理结果"

[[action.argument]]
key = "input"
label = "输入"
kind = "path"
required = true
help = "要处理的文件"

[[action.argument]]
key = "verbose"
label = "多说几句"
kind = "toggle"
flag = "--verbose"
help = "打开后打印每一步"
"#
        ),
        ScaffoldKind::Recipe => format!(
            r##"# 只写配方：包装一个已经装好的命令行工具，包里不需要脚本。
#
# program 换成你要包装的命令；下面这段只是能跑通的例子（jq 过滤 JSON）。
# 想要动态候选就写：
#   kind = "dynamic"
#   source = "git-branches"     # 或 command:<命令行>

[[action]]
id = "{action_id}"
name = "{id}"
summary = "{summary}"
domain = "{domain}"
program = "{program}"
mode = "capture"
install = "# 缺依赖时给用户的提示，例如：sudo pacman -S jq"
input = "JSON 文件"
output = "筛选后的结果"

[[action.argument]]
key = "filter"
label = "过滤表达式"
kind = "text"
default = "."
required = true
help = "例如 .name / .items[]"

[[action.argument]]
key = "file"
label = "文件"
kind = "path"
required = true
placement = "trailing"
"##
        ),
    };
    write_file(
        &dir.join("manifests").join(format!("{id}.toml")),
        &manifest_body,
        &mut written,
    )?;

    if kind == ScaffoldKind::Script {
        let script = format!(
            r#"#!/bin/sh
# {id} —— {summary}
set -eu

input=""
verbose=0
while [ $# -gt 0 ]; do
    case "$1" in
        --verbose) verbose=1; shift ;;
        -h|--help)
            cat <<'USAGE'
用法: {id} [--verbose] <输入>

  把这里换成你真正的说明。参数会以 argv 传进来（不经过 shell），
  中文、空格、引号都不需要你自己转义。
USAGE
            exit 0
            ;;
        *) input="$1"; shift ;;
    esac
done

if [ -z "$input" ]; then
    echo "缺少输入：{id} -h 看用法" >&2
    exit 2
fi
if [ ! -e "$input" ]; then
    echo "找不到：$input" >&2
    exit 1
fi

[ "$verbose" = 1 ] && echo "正在处理 $input" >&2

# 在这里写你真正要做的事
echo "处理了 $input"
"#
        );
        let script_path = dir.join("scripts").join(&id);
        write_file(&script_path, &script, &mut written)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&script_path, fs::Permissions::from_mode(0o755))
                .map_err(|error| format!("设不了可执行位：{error}"))?;
        }
    }

    let readme = format!(
        r#"# {id}

{summary}

## 装

    toolbox-hub repo add <这个仓库的 index.json>
    toolbox-hub install {id}

## 用

装上之后它会出现在「{domain}」域里；也可以直接跑：

    toolbox-hub run {id} --input 某个文件

## 开发

    toolbox-hub check .
    toolbox-hub build .
"#
    );
    write_file(&dir.join("README.md"), &readme, &mut written)?;

    let usage = format!(
        r#"# {id} 用法

## 这是什么

{summary}

## 为什么用它

（写清楚它替你省掉了什么。）

## 例子

    toolbox-hub run {id} --input 例子文件

## 边界

（它不做什么，什么时候不该用。）
"#
    );
    write_file(&dir.join("docs").join("usage.md"), &usage, &mut written)?;

    Ok(written)
}

fn write_file(path: &Path, text: &str, written: &mut Vec<PathBuf>) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|error| format!("建不了 {}：{error}", parent.display()))?;
    }
    fs::write(path, text).map_err(|error| format!("写不了 {}：{error}", path.display()))?;
    written.push(path.to_path_buf());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(tag: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("toolbox-hub-author-{tag}-{nanos}"));
        fs::create_dir_all(&dir).expect("mkdir");
        dir
    }

    fn put(path: &Path, text: &str) {
        fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
        fs::write(path, text).expect("write");
    }

    #[test]
    fn scaffold_produces_a_package_that_passes_check() {
        let base = temp("scaffold");
        let written = scaffold(
            &base,
            "my-tool",
            ScaffoldKind::Script,
            Some("测试用"),
            Some("工具"),
            None,
        )
        .expect("scaffold");
        assert!(written.len() >= 5, "{written:?}");
        let dir = base.join("my-tool");
        let report = check_package(&dir);
        assert!(report.ok(), "{:?}", report.errors);
        assert!(report.warnings.is_empty(), "{:?}", report.warnings);
        assert!(is_executable(&dir.join("scripts/my-tool")), "脚本要可执行");
        std::fs::remove_dir_all(&base).expect("cleanup");
    }

    #[test]
    fn a_recipe_package_has_no_script() {
        let base = temp("recipe");
        scaffold(
            &base,
            "json-peek",
            ScaffoldKind::Recipe,
            None,
            Some("开发"),
            None,
        )
        .expect("scaffold");
        let dir = base.join("json-peek");
        assert!(!dir.join("scripts").exists(), "配方形状不该生成脚本");
        let report = check_package(&dir);
        assert!(report.ok(), "{:?}", report.errors);
        std::fs::remove_dir_all(&base).expect("cleanup");
    }

    #[test]
    fn scaffolding_over_an_existing_package_is_refused() {
        let base = temp("exists");
        scaffold(&base, "one", ScaffoldKind::Script, None, None, None).expect("第一次");
        let problem = scaffold(&base, "one", ScaffoldKind::Script, None, None, None).unwrap_err();
        assert!(problem.contains("已经存在"), "{problem}");
        std::fs::remove_dir_all(&base).expect("cleanup");
    }

    #[test]
    fn an_unknown_domain_is_refused() {
        let base = temp("domain");
        let problem =
            scaffold(&base, "one", ScaffoldKind::Script, None, Some("星际"), None).unwrap_err();
        assert!(problem.contains("不认识的域"), "{problem}");
        std::fs::remove_dir_all(&base).expect("cleanup");
    }

    /// 可复现：同样的源码两次构建必须逐字节相同，否则「这个哈希是谁签的」说不清。
    #[test]
    fn building_twice_gives_byte_identical_artifacts() {
        let base = temp("repro");
        scaffold(&base, "my-tool", ScaffoldKind::Script, None, None, None).expect("scaffold");
        let dir = base.join("my-tool");
        let artifacts = dir.join(ARTIFACTS_DIR);

        let first = build_package(&dir, &artifacts).expect("build");
        let second = build_package(&dir, &artifacts).expect("build");
        assert_eq!(
            first.artifact_sha256, second.artifact_sha256,
            "两次构建必须逐字节相同"
        );

        let entries = archive_digests(&first.artifact).expect("读归档");
        assert!(
            entries.keys().all(|name| !name.starts_with("./")),
            "归档条目不能带 ./ 前缀（客户端会拒绝）：{:?}",
            entries.keys()
        );
        assert!(entries.contains_key(TOOLBOX_TOML), "{:?}", entries.keys());
        assert!(
            entries.contains_key("scripts/my-tool"),
            "{:?}",
            entries.keys()
        );
        std::fs::remove_dir_all(&base).expect("cleanup");
    }

    #[test]
    fn the_index_is_generated_from_the_sources() {
        let base = temp("registry");
        let packages = base.join(PACKAGES_DIR);
        scaffold(&packages, "one", ScaffoldKind::Script, None, None, None).expect("scaffold");
        scaffold(
            &packages,
            "two",
            ScaffoldKind::Recipe,
            None,
            None,
            Some("jq"),
        )
        .expect("scaffold");

        let built = build_registry(&base, false).expect("build");
        assert_eq!(built.packages, vec!["one", "two"]);

        let text = fs::read_to_string(&built.index_path).expect("read");
        let index = Index::parse(&text).expect("parse").index;
        assert_eq!(index.schema_version, SCHEMA_VERSION);
        assert_eq!(index.packages.len(), 2);
        assert!(index.updated.is_none(), "默认不盖时间戳，不然不可复现");
        for package in &index.packages {
            assert!(
                package
                    .artifact
                    .as_ref()
                    .and_then(|artifact| artifact.sha256())
                    .is_some(),
                "{} 缺哈希",
                package.id
            );
            assert!(!package.files.is_empty(), "{} 没有文件清单", package.id);
        }

        // 刚建完就该是干净的 —— 这条同时验证了 check ↔ build 口径一致
        let report = check_registry(&base);
        assert!(report.ok(), "{:?}", report.errors);
        std::fs::remove_dir_all(&base).expect("cleanup");
    }

    #[test]
    fn the_index_is_reproducible_too() {
        let base = temp("indexrepro");
        let packages = base.join(PACKAGES_DIR);
        scaffold(&packages, "one", ScaffoldKind::Script, None, None, None).expect("scaffold");
        let first = fs::read_to_string(build_registry(&base, false).expect("build").index_path)
            .expect("read");
        let second = fs::read_to_string(build_registry(&base, false).expect("build").index_path)
            .expect("read");
        assert_eq!(first, second, "索引也必须是可复现的");
        std::fs::remove_dir_all(&base).expect("cleanup");
    }

    /// 改了源码没重新打包 —— 这是最容易发生的「发了但内容对不上」。
    #[test]
    fn editing_a_source_after_building_is_caught() {
        let base = temp("stale");
        let packages = base.join(PACKAGES_DIR);
        scaffold(&packages, "one", ScaffoldKind::Script, None, None, None).expect("scaffold");
        build_registry(&base, false).expect("build");
        assert!(check_registry(&base).ok(), "刚建完应当是干净的");

        put(
            &packages.join("one/scripts/one"),
            "#!/bin/sh\necho changed\n",
        );
        let report = check_registry(&base);
        assert!(!report.ok(), "改了源码没重打包必须被抓到");
        assert!(
            report
                .errors
                .iter()
                .any(|error| error.contains("哈希对不上") || error.contains("重新 build")),
            "{:?}",
            report.errors
        );
        std::fs::remove_dir_all(&base).expect("cleanup");
    }

    #[test]
    fn a_package_missing_from_the_index_is_caught() {
        let base = temp("missing");
        let packages = base.join(PACKAGES_DIR);
        scaffold(&packages, "one", ScaffoldKind::Script, None, None, None).expect("scaffold");
        build_registry(&base, false).expect("build");
        scaffold(&packages, "two", ScaffoldKind::Script, None, None, None).expect("scaffold");

        let report = check_registry(&base);
        assert!(!report.ok());
        assert!(
            report.errors.iter().any(|error| error.contains("two")),
            "{:?}",
            report.errors
        );
        std::fs::remove_dir_all(&base).expect("cleanup");
    }

    /// 包里的符号链接一律拒绝（也是最经典的越界写文件手法）。
    #[test]
    fn a_symlink_in_the_package_is_refused() {
        let base = temp("symlink");
        scaffold(&base, "one", ScaffoldKind::Script, None, None, None).expect("scaffold");
        let dir = base.join("one");
        #[cfg(unix)]
        std::os::unix::fs::symlink("/etc/passwd", dir.join("scripts/evil")).expect("symlink");
        let problem = collect_payload(&dir).unwrap_err();
        assert!(problem.contains("符号链接"), "{problem}");
        std::fs::remove_dir_all(&base).expect("cleanup");
    }

    #[test]
    fn toolbox_toml_is_archived_but_not_installed_and_not_warned_about() {
        let base = temp("archiveonly");
        scaffold(&base, "one", ScaffoldKind::Script, None, None, None).expect("scaffold");
        let dir = base.join("one");

        let payload = collect_payload(&dir).expect("payload");
        assert!(
            payload
                .archive_only
                .iter()
                .any(|path| path.to_string_lossy() == TOOLBOX_TOML)
        );
        assert!(
            !payload
                .install
                .iter()
                .any(|file| file.relative.to_string_lossy() == TOOLBOX_TOML)
        );

        let built = build_package(&dir, &dir.join(ARTIFACTS_DIR)).expect("build");
        assert!(
            built.warnings.is_empty(),
            "toolbox.toml 是故意只进归档的，不该被当成不认识的目录：{:?}",
            built.warnings
        );
        std::fs::remove_dir_all(&base).expect("cleanup");
    }

    #[test]
    fn an_unrecognised_top_level_file_warns_but_is_not_installed() {
        let base = temp("extra");
        scaffold(&base, "one", ScaffoldKind::Script, None, None, None).expect("scaffold");
        let dir = base.join("one");
        put(&dir.join("notes.txt"), "随手记");

        let report = check_package(&dir);
        assert!(report.ok(), "{:?}", report.errors);
        assert!(
            report
                .warnings
                .iter()
                .any(|warning| warning.contains("notes.txt")),
            "{:?}",
            report.warnings
        );

        let built = build_package(&dir, &dir.join(ARTIFACTS_DIR)).expect("build");
        assert!(
            built
                .warnings
                .iter()
                .any(|warning| warning.contains("notes.txt")),
            "{:?}",
            built.warnings
        );
        assert!(
            !built
                .files
                .iter()
                .any(|(file, _)| file.relative.to_string_lossy() == "notes.txt"),
            "不认识的文件不该被安装"
        );
        std::fs::remove_dir_all(&base).expect("cleanup");
    }

    /// 依赖是**推**出来的：动作里 program = "jq" 就说明依赖 jq。
    #[test]
    fn dependencies_come_from_both_declarations_and_actions() {
        let base = temp("deps");
        scaffold(&base, "one", ScaffoldKind::Script, None, None, None).expect("scaffold");
        let dir = base.join("one");
        put(
            &dir.join("manifests/extra.toml"),
            r#"[[action]]
id = "peek"
name = "看一眼"
summary = "看 JSON"
domain = "开发"
program = "jq"
mode = "capture"
base_argv = ["."]

[[action.argument]]
key = "file"
label = "文件"
kind = "path"
required = true
"#,
        );

        let manifest = PackageManifest::load(&dir).expect("manifest");
        let payload = collect_payload(&dir).expect("payload");
        let facts = manifest_facts(&dir).expect("facts");
        let dependencies = merge_dependencies(&manifest, &facts, &payload);
        let names: Vec<&str> = dependencies
            .iter()
            .map(|dependency| dependency.command.as_str())
            .collect();
        assert!(
            !names.contains(&"one"),
            "包自己的脚本是提供的，不是依赖：{names:?}"
        );
        assert!(names.contains(&"jq"), "{names:?}");

        let report = check_package(&dir);
        assert!(
            report.passed.iter().any(|line| line.contains("jq")),
            "{:?}",
            report.passed
        );
        std::fs::remove_dir_all(&base).expect("cleanup");
    }

    #[test]
    fn a_mismatched_id_is_an_error() {
        let base = temp("mismatch");
        scaffold(&base, "one", ScaffoldKind::Script, None, None, None).expect("scaffold");
        let dir = base.join("one");
        let text = fs::read_to_string(dir.join(TOOLBOX_TOML))
            .expect("read")
            .replace("id = \"one\"", "id = \"other\"");
        fs::write(dir.join(TOOLBOX_TOML), text).expect("write");

        let report = check_package(&dir);
        assert!(!report.ok());
        assert!(
            report.errors.iter().any(|error| error.contains("目录名")),
            "{:?}",
            report.errors
        );
        std::fs::remove_dir_all(&base).expect("cleanup");
    }

    #[test]
    fn a_typo_in_toolbox_toml_is_refused_loudly() {
        let problem = PackageManifest::parse(
            "[package]\nid = \"a\"\nname = \"A\"\nversion = \"1.0.0\"\nsummary = \"s\"\nnam = \"typo\"\n",
        )
        .unwrap_err();
        assert!(
            problem.contains("nam") || problem.contains("unknown field"),
            "{problem}"
        );
    }

    #[test]
    fn a_package_with_nothing_installable_is_refused_by_both_verbs() {
        let base = temp("empty");
        let dir = base.join("nothing");
        put(
            &dir.join(TOOLBOX_TOML),
            "[package]\nid = \"nothing\"\nname = \"空\"\nversion = \"1.0.0\"\nsummary = \"空包\"\n",
        );

        let report = check_package(&dir);
        assert!(!report.ok(), "空包不该通过校验");
        let problem = build_package(&dir, &dir.join(ARTIFACTS_DIR)).unwrap_err();
        assert!(problem.contains("没有任何可安装的文件"), "{problem}");
        std::fs::remove_dir_all(&base).expect("cleanup");
    }
}

#[cfg(test)]
mod shipped_registry_tests {
    use std::path::PathBuf;

    /// 仓库里那份官方 registry 必须自洽：索引 ↔ 产物 ↔ 源码三方一致。
    ///
    /// 这条守的是最容易发生的一种腐坏：有人改了某个插件的脚本、忘了跑 build，
    /// 于是仓库里发出去的东西和源码对不上。只读，不改任何文件。
    #[test]
    fn the_shipped_registry_is_consistent() {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("registry");
        if !root.join("index.json").is_file() {
            // 源码包里没带 registry/ 时不误报。
            return;
        }

        let report = super::check_registry(&root);
        assert!(
            report.ok(),
            "官方仓库不自洽（跑一次 toolbox-hub build registry/）：{:#?}",
            report.errors
        );
    }

    /// 官方仓库里的每个插件都必须**真的能装能跑**：至少校验通过、产物里有声明的文件。
    #[test]
    fn every_shipped_package_validates() {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("registry");
        let packages = root.join(super::PACKAGES_DIR);
        let Ok(entries) = std::fs::read_dir(&packages) else {
            return;
        };
        let mut checked = 0usize;
        for path in entries
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.path())
        {
            if !path.is_dir() || !path.join(super::TOOLBOX_TOML).is_file() {
                continue;
            }
            let report = super::check_package(&path);
            assert!(
                report.ok(),
                "{} 校验不过：{:#?}",
                path.display(),
                report.errors
            );
            checked += 1;
        }
        assert!(checked > 0, "官方仓库里至少要有一个插件");
    }
}
