//! 安装 / 卸载 / 更新 —— 以及它们共用的一整套安全检查。
//!
//! # 顺序是刻意的
//!
//! ```text
//! 从索引拿元数据
//!   ↓  不下载任何东西：先让用户看清楚要装什么
//! 计划（InstallPlan）
//!   ↓  路径安全、冲突、依赖、requires_root、哈希有没有声明
//! 用户确认
//!   ↓
//! 下载产物
//!   ↓  SHA-256 必须与索引一致，不一致**拒绝安装**
//! 解包到暂存目录
//!   ↓  拒绝符号链接 / 设备文件 / 绝对路径 / ..
//! 校验包内 toolbox.toml 与索引一致（元数据验证）
//!   ↓
//! 只装索引声明的那几个文件（包里多出来的不装）
//!   ↓
//! 逐个记进账本（路径 + sha256 + 来源）
//! ```
//!
//! # 一次也不 sudo
//!
//! 装的东西全部落在用户自己的目录里（`~/.local/bin`、数据目录）。
//! 声明了 `requires_root = true` 的包**不会被自动提权安装** —— 计划里会明确
//! 标出来，执行时拒绝并告诉你为什么。系统目录的写入留给用户自己决定。

use std::{
    collections::BTreeSet,
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
};

use sha2::{Digest, Sha256};

use crate::{
    model::Danger,
    repository::{
        cache,
        config::{RepositoryConfig, Trust},
        index::{Artifact, ArtifactKind, FileKind, PackageMeta},
        installed::{self, FileState, InstalledFile, InstalledPackage, SCHEMA_VERSION},
        paths, version,
    },
};

/// 产物大小上限（防止一个坏 URL 把磁盘写满）。
const MAX_ARTIFACT_BYTES: u64 = 256 * 1024 * 1024;

/// 安装要落到的几个根目录。
#[derive(Clone, Debug)]
pub struct Roots {
    /// 可执行脚本：`~/.local/bin`（或 `TOOLBOX_HUB_BIN_DIR`）。
    pub bin: PathBuf,
    /// 包载荷与账本：数据目录。
    pub data: PathBuf,
    /// 配置目录（留给未来把 manifest 直接投到 tools.d；当前走包载荷目录）。
    pub config: PathBuf,
}

impl Roots {
    /// 按当前配置解析（`TOOLBOX_HUB_BIN_DIR` > `~/.local/bin`）。
    #[allow(dead_code)] // 保留：不使用显式目录的调用方
    pub fn resolve() -> Self {
        Self {
            bin: bin_dir(),
            data: crate::config::data_dir(),
            config: crate::config::config_dir(),
        }
    }
}

/// 用户级可执行目录。
pub fn bin_dir() -> PathBuf {
    if let Some(raw) = std::env::var_os("TOOLBOX_HUB_BIN_DIR") {
        return PathBuf::from(raw);
    }
    std::env::var_os("HOME")
        .map(|home| PathBuf::from(home).join(".local/bin"))
        .unwrap_or_else(|| PathBuf::from(".local/bin"))
}

// ── 摘要 ────────────────────────────────────────────────────────────────────

/// 一段字节的 SHA-256（小写十六进制）。
pub fn sha256_bytes(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())
}

/// 一个文件的 SHA-256。
pub fn sha256_file(path: &Path) -> Result<String, String> {
    let mut file =
        fs::File::open(path).map_err(|error| format!("读不了 {}：{error}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; 64 * 1024];
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|error| format!("读不了 {}：{error}", path.display()))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

/// 字符串的 SHA-256（测试与比较用）。
#[allow(dead_code)] // 测试与比较用
pub fn sha256_text(text: &str) -> String {
    sha256_bytes(text.as_bytes())
}

// ── 完整性 ──────────────────────────────────────────────────────────────────

/// 这次装的**完整性**凭什么作数。刻意和「来源信任」分开说：
/// 「下载来源可信」与「内容核对过了」是两件事，混成一句就是在骗人。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Integrity {
    /// 有声明哈希且核对一致（网络来源）。
    Verified,
    /// 有声明哈希且核对一致（本地产物）。
    VerifiedLocal,
    /// 来源没提供哈希，用户显式接受了这个风险。
    Unverified,
    /// 没有单独产物，靠逐个文件核对。
    PerFile,
}

impl Integrity {
    pub fn label(self) -> &'static str {
        match self {
            Integrity::Verified => "SHA-256 已核对",
            Integrity::VerifiedLocal => "本地文件 SHA-256 已核对",
            Integrity::Unverified => "未提供哈希（你已确认风险）",
            Integrity::PerFile => "逐个文件核对",
        }
    }

    /// 完整性有没有真的被验证过。
    #[allow(dead_code)] // 给界面/CLI 判断完整性用
    pub fn is_verified(self) -> bool {
        !matches!(self, Integrity::Unverified)
    }
}

// ── 计划 ────────────────────────────────────────────────────────────────────

/// 计划里的一个文件：从包内相对路径，到磁盘上的绝对路径。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlannedFile {
    pub source: String,
    pub target: PathBuf,
    pub kind: FileKind,
    /// 索引声明的单文件哈希（可选）。
    pub sha256: Option<String>,
    /// 目标已存在时的问题说明（None = 没问题）。
    pub conflict: Option<String>,
}

/// 一次安装的完整计划。**在执行之前**就把该给用户看的都摆出来。
#[derive(Clone, Debug)]
pub struct InstallPlan {
    pub id: String,
    #[allow(dead_code)] // 升级列表显示用
    pub name: String,
    pub version: String,
    pub summary: String,
    #[allow(dead_code)] // 详情区以后要显示
    pub description: Option<String>,
    pub license: Option<String>,
    pub author: Option<String>,
    pub source: Option<String>,
    pub repository: String,
    pub repository_name: String,
    pub trust: Trust,
    pub danger: Danger,
    pub requires_root: bool,
    pub artifact: Option<Artifact>,
    pub dependencies: Vec<String>,
    /// 依赖里当前找不到的命令。
    pub missing_dependencies: Vec<String>,
    /// 缺依赖时作者给的安装提示（命令 → 提示）。
    pub dep_hints: Vec<(String, String)>,
    pub files: Vec<PlannedFile>,
    /// 本地已装的版本（升级时用）。
    pub installed_version: Option<String>,
    /// 索引本身的位置。产物 URL 写成相对路径时，相对的就是它 ——
    /// 这样「把整个仓库目录放在本地」也能直接装。
    pub index_base: Option<String>,
    pub warnings: Vec<String>,
}

impl InstallPlan {
    /// 这次是升级还是新装。
    #[allow(dead_code)] // 计划面板用
    pub fn is_upgrade(&self) -> bool {
        self.installed_version.is_some()
    }

    /// 有冲突的目标文件。
    #[allow(dead_code)] // 计划面板用
    pub fn conflicts(&self) -> Vec<&PlannedFile> {
        self.files
            .iter()
            .filter(|file| file.conflict.is_some())
            .collect()
    }

    #[allow(dead_code)] // 计划面板用
    pub fn has_conflicts(&self) -> bool {
        !self.conflicts().is_empty()
    }

    /// 包含可执行脚本（界面上那句警告）。
    pub fn has_executables(&self) -> bool {
        self.files.iter().any(|file| file.kind == FileKind::Bin)
    }

    /// 依赖是否齐全。
    pub fn deps_ready(&self) -> bool {
        self.missing_dependencies.is_empty()
    }

    /// 计划能不能执行；Err 里是给用户看的理由。
    ///
    /// 这里是**所有**硬门槛的集合地：任何一条不满足就别动手。
    pub fn check(&self, _roots: &Roots, allow_unverified: bool) -> Result<(), String> {
        if self.requires_root {
            return Err(format!(
                "「{}」声明需要写系统目录（requires_root = true）。工具箱不会替你提权安装，\
                 请按它自己的说明手动安装",
                self.name
            ));
        }
        if let Some(artifact) = &self.artifact {
            let remote = !artifact.is_local();
            if artifact.sha256().is_none() && remote && !allow_unverified {
                return Err(format!(
                    "「{}」的来源没有提供 SHA-256，无法核对完整性。确认要装的话加 \
                     --allow-unverified（TUI 里会让你就地确认）",
                    self.name
                ));
            }
        }
        if let Some(file) = self.files.iter().find(|file| file.conflict.is_some()) {
            return Err(file.conflict.clone().unwrap_or_default());
        }
        Ok(())
    }
}

/// 造一个安装计划。
pub fn plan(
    meta: &PackageMeta,
    repository: &RepositoryConfig,
    roots: &Roots,
) -> Result<InstallPlan, String> {
    meta.validate()?;
    let id = meta.id.clone();
    let installed_version = installed::load(&roots.data, &id).map(|package| package.version);

    let mut warnings = Vec::new();
    if meta.files.is_empty() {
        warnings.push(String::from(
            "索引没有列出文件清单，装完之前看不到具体落哪些文件",
        ));
    }
    if meta
        .artifact
        .as_ref()
        .is_some_and(|artifact| artifact.sha256().is_none())
    {
        warnings.push(String::from("来源没有提供 SHA-256，装之前无法核对完整性"));
    }

    let mut files = Vec::with_capacity(meta.files.len());
    for entry in &meta.files {
        let relative = paths::safe_relative(&entry.path)?;
        let kind = entry.kind();
        let target = target_for(roots, &id, &relative, kind)?;
        let conflict = conflict_for(roots, &id, &target, entry.sha256().as_deref())?;
        files.push(PlannedFile {
            source: relative.to_string_lossy().to_string(),
            target,
            kind,
            sha256: entry.sha256(),
            conflict,
        });
    }

    let mut dependencies = Vec::new();
    let mut missing_dependencies = Vec::new();
    let mut dep_hints = Vec::new();
    for dependency in &meta.dependencies {
        let command = dependency.command.trim().to_string();
        if command.is_empty() {
            continue;
        }
        if !command_available(&command) {
            missing_dependencies.push(command.clone());
            if let Some(hint) = &dependency.hint {
                dep_hints.push((command.clone(), hint.clone()));
            }
        }
        dependencies.push(command);
    }

    Ok(InstallPlan {
        id,
        name: meta.name.clone(),
        version: meta.version.clone(),
        summary: meta.summary_or_description().to_string(),
        description: meta.description.clone(),
        license: meta.license.clone(),
        author: meta.author.clone(),
        source: meta.source.clone(),
        repository: repository.id.clone(),
        repository_name: repository.name.clone(),
        trust: repository.trust(),
        danger: meta.danger(),
        requires_root: meta.requires_root,
        artifact: meta.artifact.clone(),
        dependencies,
        missing_dependencies,
        dep_hints,
        files,
        installed_version,
        index_base: Some(repository.index.clone()),
        warnings,
    })
}

/// 把产物 URL 解析成可以直接用的地址。
///
/// * 已经是绝对 URL / 绝对路径 → 原样；
/// * 相对路径 → 相对**索引所在目录**（本地索引按目录，远端索引按 URL 前缀）。
///
/// 相对路径要先过一遍路径安全：不接受 `..`，免得索引里的
/// `../../../etc/shadow` 变成一次任意文件读取。
pub fn resolve_url(base: Option<&str>, url: &str) -> String {
    if url.contains("://") || url.starts_with('/') {
        return url.to_string();
    }
    let Some(base) = base else {
        return url.to_string();
    };
    let Ok(relative) = paths::safe_relative(url) else {
        return url.to_string();
    };

    if let Some(rest) = base.strip_prefix("file://") {
        return match Path::new(rest).parent() {
            Some(dir) => dir.join(&relative).to_string_lossy().to_string(),
            None => url.to_string(),
        };
    }
    if !base.contains("://") {
        return match Path::new(base).parent() {
            Some(dir) => dir.join(&relative).to_string_lossy().to_string(),
            None => url.to_string(),
        };
    }
    match base.rfind('/') {
        Some(index) => format!("{}/{}", &base[..index], relative.to_string_lossy()),
        None => url.to_string(),
    }
}

#[cfg(test)]
mod resolve_tests {
    use super::resolve_url;

    #[test]
    fn absolute_locations_are_left_alone() {
        assert_eq!(
            resolve_url(None, "https://x/y.tar.gz"),
            "https://x/y.tar.gz"
        );
        assert_eq!(
            resolve_url(Some("/repo/index.json"), "https://x/y"),
            "https://x/y"
        );
        assert_eq!(resolve_url(Some("/repo/index.json"), "/abs/y"), "/abs/y");
    }

    #[test]
    fn relative_urls_resolve_against_the_index_directory() {
        assert_eq!(
            resolve_url(Some("/repo/index.json"), "artifacts/a.tar.gz"),
            "/repo/artifacts/a.tar.gz"
        );
        assert_eq!(
            resolve_url(Some("file:///repo/index.json"), "artifacts/a.tar.gz"),
            "/repo/artifacts/a.tar.gz"
        );
        assert_eq!(
            resolve_url(Some("https://host/tools/index.json"), "artifacts/a.tar.gz"),
            "https://host/tools/artifacts/a.tar.gz"
        );
    }

    /// 索引里的相对产物路径同样是不可信输入。
    #[test]
    fn traversal_in_a_relative_url_is_refused() {
        // 过不了路径安全就原样返回 → 之后会被当成读不到的路径而失败
        assert_eq!(
            resolve_url(Some("/repo/index.json"), "../../../etc/shadow"),
            "../../../etc/shadow"
        );
    }

    #[test]
    fn a_relative_url_without_a_base_stays_relative() {
        assert_eq!(
            resolve_url(None, "artifacts/a.tar.gz"),
            "artifacts/a.tar.gz"
        );
    }
}

/// 一个文件该落到哪。
fn target_for(roots: &Roots, id: &str, relative: &Path, kind: FileKind) -> Result<PathBuf, String> {
    match kind {
        FileKind::Bin => {
            let name = relative
                .file_name()
                .ok_or_else(|| format!("{} 没有文件名", relative.display()))?;
            let name = paths::safe_relative(&name.to_string_lossy())?;
            Ok(roots.bin.join(name))
        }
        _ => {
            let base = installed::files_dir(&roots.data, id);
            paths::resolve_under(&base, relative)
        }
    }
}

/// 目标已经存在时算什么。
fn conflict_for(
    roots: &Roots,
    id: &str,
    target: &Path,
    expected: Option<&str>,
) -> Result<Option<String>, String> {
    if !target.exists() {
        return Ok(None);
    }
    if target.is_dir() {
        return Ok(Some(format!(
            "{} 已经是一个目录，装不下去",
            target.display()
        )));
    }

    match installed::owner_of(&roots.data, target) {
        // 自己装的（重装 / 升级）：放行。
        Some((owner, _)) if owner == id => Ok(None),
        Some((owner, _)) => Ok(Some(format!(
            "{} 已经被包「{owner}」装着，两个包不能装同一个文件",
            target.display()
        ))),
        None => {
            // 没人管这个文件：内容一样就当它是同一个东西，否则算冲突。
            let existing = sha256_file(target)?;
            match expected {
                Some(wanted) if wanted == existing => Ok(None),
                _ => Ok(Some(format!(
                    "{} 已经存在，而且不是任何包装的（内容不同）",
                    target.display()
                ))),
            }
        }
    }
}

/// 命令在不在 PATH 上。
fn command_available(command: &str) -> bool {
    if command.contains('/') {
        return Path::new(command).is_file();
    }
    std::env::var_os("PATH")
        .map(|path| std::env::split_paths(&path).any(|dir| dir.join(command).is_file()))
        .unwrap_or(false)
}

// ── 执行 ────────────────────────────────────────────────────────────────────

/// 一次安装/更新的结果。
#[derive(Clone, Debug)]
pub struct InstallReport {
    pub id: String,
    pub version: String,
    pub files: Vec<InstalledFile>,
    pub integrity: Integrity,
    pub warnings: Vec<String>,
}

/// 真正安装。
pub fn install(
    plan: &InstallPlan,
    roots: &Roots,
    agent: &ureq::Agent,
    allow_unverified: bool,
) -> Result<InstallReport, String> {
    plan.check(roots, allow_unverified)?;
    if plan.dependencies.is_empty() && plan.files.is_empty() {
        return Err(String::from("这个包既没有文件也没有依赖，装不了"));
    }

    let staging = staging_dir(&roots.data, &plan.id)?;
    // 无论成功失败都清掉暂存目录（连空的父目录一起，不留垃圾）。
    let outcome = install_inner(plan, roots, agent, &staging, allow_unverified);
    let _ = fs::remove_dir_all(&staging);
    if let Some(parent) = staging.parent() {
        let _ = fs::remove_dir(parent);
    }
    outcome
}

fn install_inner(
    plan: &InstallPlan,
    roots: &Roots,
    agent: &ureq::Agent,
    staging: &Path,
    allow_unverified: bool,
) -> Result<InstallReport, String> {
    let mut warnings = plan.warnings.clone();
    let mut integrity = Integrity::PerFile;
    // 升级前先把旧账本取出来：新账本一写，旧文件清单就没了。
    let previous_files = installed::load(&roots.data, &plan.id)
        .map(|package| package.files)
        .unwrap_or_default();

    let payload = staging.join("payload");
    fs::create_dir_all(&payload).map_err(|error| format!("建不了暂存目录：{error}"))?;

    // 1) 产物：下载 → 核对哈希 → 解包
    if let Some(raw_artifact) = &plan.artifact {
        // 相对 URL 先落到「索引所在目录」上再下载。
        let resolved = Artifact {
            url: resolve_url(plan.index_base.as_deref(), &raw_artifact.url),
            ..raw_artifact.clone()
        };
        let artifact = &resolved;

        let archive_dir = staging.join("artifact");
        fs::create_dir_all(&archive_dir).map_err(|error| format!("建不了暂存目录：{error}"))?;
        let downloaded = archive_dir.join("download");
        let declared = artifact.sha256();
        let actual = download(artifact, agent, &downloaded)?;

        match &declared {
            Some(wanted) => {
                if *wanted != actual {
                    return Err(format!(
                        "SHA-256 与索引不一致，拒绝安装\n  索引声明: {wanted}\n  实际得到: {actual}"
                    ));
                }
                integrity = if artifact.is_local() {
                    Integrity::VerifiedLocal
                } else {
                    Integrity::Verified
                };
            }
            None => {
                // check() 已经拦住远程无哈希；走到这里只可能是用户显式接受了。
                integrity = Integrity::Unverified;
                warnings.push(String::from("这次安装没有可核对的哈希"));
            }
        }

        extract(artifact, &downloaded, &payload)?;
        // 2) 元数据验证：包里的 toolbox.toml（如果有）必须和索引说的一致
        verify_inner_metadata(&payload, plan, &mut warnings)?;
    }

    // 3) 只装索引声明的文件
    let files_root = installed::files_dir(&roots.data, &plan.id);
    fs::create_dir_all(&files_root)
        .map_err(|error| format!("建不了 {}：{error}", files_root.display()))?;

    let mut recorded: Vec<InstalledFile> = Vec::new();
    for file in &plan.files {
        let source = resolve_payload(&payload, &file.source);
        let outcome = (|| -> Result<InstalledFile, String> {
            if !source.is_file() {
                return Err(format!("产物里没有索引声明的文件「{}」", file.source));
            }
            let bytes = fs::read(&source)
                .map_err(|error| format!("读不了 {}：{error}", source.display()))?;
            let digest = sha256_bytes(&bytes);
            if let Some(wanted) = &file.sha256
                && *wanted != digest
            {
                return Err(format!(
                    "「{}」的 SHA-256 与索引不一致，拒绝安装\n  索引声明: {wanted}\n  实际得到: {digest}",
                    file.source
                ));
            }
            write_atomically(&file.target, &bytes, file.kind == FileKind::Bin)?;
            Ok(InstalledFile {
                path: file.target.clone(),
                sha256: digest,
                kind: kind_id(file.kind).to_string(),
                source: file.source.clone(),
            })
        })();

        match outcome {
            Ok(entry) => recorded.push(entry),
            Err(problem) => {
                // 装到一半失败必须回滚：账本还没写，留着这些文件就成了
                // 谁也不知道、谁也删不掉的孤儿。
                for written in &recorded {
                    let _ = fs::remove_file(&written.path);
                }
                return Err(problem);
            }
        }
    }
    let installed_targets: BTreeSet<PathBuf> =
        recorded.iter().map(|file| file.path.clone()).collect();

    // 4) 账本
    let package = InstalledPackage {
        schema_version: SCHEMA_VERSION,
        id: plan.id.clone(),
        name: plan.name.clone(),
        version: plan.version.clone(),
        repository: plan.repository.clone(),
        repository_name: plan.repository_name.clone(),
        trust: plan.trust.id().to_string(),
        installed_at: cache::now_secs(),
        source: plan.source.clone(),
        license: plan.license.clone(),
        requires_root: plan.requires_root,
        danger: danger_id(plan.danger).to_string(),
        artifact_sha256: plan
            .artifact
            .as_ref()
            .and_then(|artifact| artifact.sha256()),
        dependencies: plan.dependencies.clone(),
        files: recorded.clone(),
        allow_unverified,
    };
    installed::save(&roots.data, &package)?;

    // 5) 升级时清掉旧版本里这次不再安装的文件。
    //
    // 只清「原样没动」的：用户改过的文件一律留着，和卸载同一套规矩。
    for file in previous_files {
        if installed_targets.contains(&file.path) {
            continue;
        }
        match file.current_state() {
            FileState::Unmodified => match fs::remove_file(&file.path) {
                Ok(()) => warnings.push(format!(
                    "旧版本的文件 {} 已经不再需要，删掉了",
                    file.path.display()
                )),
                Err(error) => warnings.push(format!(
                    "旧版本的文件 {} 删不掉：{error}",
                    file.path.display()
                )),
            },
            FileState::Modified => warnings.push(format!(
                "旧版本的文件 {} 你改过，保留着没有动",
                file.path.display()
            )),
            FileState::Gone => {}
        }
    }

    Ok(InstallReport {
        id: plan.id.clone(),
        version: plan.version.clone(),
        files: recorded,
        integrity,
        warnings,
    })
}

/// 单文件产物在暂存目录里的固定名字。
const SINGLE_FILE_PAYLOAD: &str = "__payload__";

/// 把索引里的包内路径映射到暂存目录里的实际文件。
///
/// 单文件产物（kind = "file"）只有一份内容，落在 __payload__；
/// 索引里那种包的清单通常只有一项，指向哪里就放到哪里。
fn resolve_payload(payload: &Path, source: &str) -> PathBuf {
    let direct = payload.join(source);
    if direct.is_file() {
        return direct;
    }
    let single = payload.join(SINGLE_FILE_PAYLOAD);
    if single.is_file() {
        return single;
    }
    direct
}

fn kind_id(kind: FileKind) -> &'static str {
    match kind {
        FileKind::Bin => "bin",
        FileKind::Manifest => "manifest",
        FileKind::Doc => "doc",
        FileKind::Data => "data",
    }
}

fn danger_id(danger: Danger) -> &'static str {
    match danger {
        Danger::Safe => "safe",
        Danger::Caution => "caution",
    }
}

/// 写文件：先写同目录的临时文件再 rename（同文件系统内是原子的），
/// 这样半截文件不会留在目标位置上。
fn write_atomically(target: &Path, bytes: &[u8], executable: bool) -> Result<(), String> {
    let parent = target
        .parent()
        .ok_or_else(|| format!("{} 没有父目录", target.display()))?;
    fs::create_dir_all(parent).map_err(|error| format!("建不了 {}：{error}", parent.display()))?;
    let name = target
        .file_name()
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_else(|| String::from("file"));
    let temp = parent.join(format!(".{name}.toolbox-tmp-{}", std::process::id()));

    {
        let mut handle = fs::File::create(&temp)
            .map_err(|error| format!("写不了 {}：{error}", temp.display()))?;
        handle
            .write_all(bytes)
            .map_err(|error| format!("写不了 {}：{error}", temp.display()))?;
        handle
            .flush()
            .map_err(|error| format!("写不了 {}：{error}", temp.display()))?;
    }

    if executable {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&temp, fs::Permissions::from_mode(0o755))
                .map_err(|error| format!("设不了可执行位 {}：{error}", temp.display()))?;
        }
    }

    fs::rename(&temp, target).map_err(|error| {
        let _ = fs::remove_file(&temp);
        format!("装不了 {}：{error}", target.display())
    })
}

/// 暂存目录（放在数据目录下，保证和最终目标在同一个文件系统上）。
fn staging_dir(data_dir: &Path, id: &str) -> Result<PathBuf, String> {
    let dir = data_dir
        .join(".staging")
        .join(format!("{id}-{}", std::process::id()));
    if dir.exists() {
        fs::remove_dir_all(&dir).map_err(|error| format!("清不掉旧暂存目录：{error}"))?;
    }
    fs::create_dir_all(&dir)
        .map_err(|error| format!("建不了暂存目录 {}：{error}", dir.display()))?;
    Ok(dir)
}

/// 下载产物，返回实际算出来的 SHA-256。
fn download(artifact: &Artifact, agent: &ureq::Agent, dest: &Path) -> Result<String, String> {
    if let Some(path) = artifact.local_path() {
        let metadata =
            fs::metadata(&path).map_err(|error| format!("读不了 {}：{error}", path.display()))?;
        if metadata.len() > MAX_ARTIFACT_BYTES {
            return Err(format!(
                "{} 太大（{} 字节）",
                path.display(),
                metadata.len()
            ));
        }
        fs::copy(&path, dest).map_err(|error| format!("拷不了 {}：{error}", path.display()))?;
        return sha256_file(dest);
    }

    let mut response = agent
        .get(&artifact.url)
        .call()
        .map_err(|error| format!("下载失败（{}）：{error}", artifact.url))?;

    let declared_size = response
        .headers()
        .get("content-length")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok());
    if let Some(size) = declared_size
        && size > MAX_ARTIFACT_BYTES
    {
        return Err(format!(
            "产物太大（{size} 字节，上限 {MAX_ARTIFACT_BYTES}）"
        ));
    }

    let mut reader = response.body_mut().as_reader();
    let mut file =
        fs::File::create(dest).map_err(|error| format!("写不了 {}：{error}", dest.display()))?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; 64 * 1024];
    let mut total: u64 = 0;
    loop {
        let read = reader
            .read(&mut buffer)
            .map_err(|error| format!("下载中断：{error}"))?;
        if read == 0 {
            break;
        }
        total += read as u64;
        if total > MAX_ARTIFACT_BYTES {
            return Err(format!("产物超过上限 {MAX_ARTIFACT_BYTES} 字节，已中止"));
        }
        hasher.update(&buffer[..read]);
        file.write_all(&buffer[..read])
            .map_err(|error| format!("写不了 {}：{error}", dest.display()))?;
    }
    file.flush()
        .map_err(|error| format!("写不了 {}：{error}", dest.display()))?;
    Ok(format!("{:x}", hasher.finalize()))
}

/// 解包到暂存目录。**拒绝**符号链接、硬链接、设备文件与任何越界路径。
fn extract(artifact: &Artifact, downloaded: &Path, dest: &Path) -> Result<(), String> {
    match artifact.kind() {
        ArtifactKind::File => {
            fs::copy(downloaded, dest.join(SINGLE_FILE_PAYLOAD))
                .map_err(|error| format!("拷不了单文件产物：{error}"))?;
            Ok(())
        }
        ArtifactKind::Tar => {
            let file = fs::File::open(downloaded).map_err(|error| error.to_string())?;
            extract_tar(file, dest)
        }
        ArtifactKind::TarGz => {
            let file = fs::File::open(downloaded).map_err(|error| error.to_string())?;
            let decoder = flate2::read::GzDecoder::new(file);
            extract_tar(decoder, dest)
        }
    }
}

/// 归档根目录条目（```./``` / ```.``` / 空）。
fn is_archive_root(display: &str) -> bool {
    let trimmed = display.trim().trim_matches('/');
    trimmed.is_empty() || trimmed == "."
}

fn extract_tar<R: Read>(reader: R, dest: &Path) -> Result<(), String> {
    let mut archive = tar::Archive::new(reader);
    let entries = archive
        .entries()
        .map_err(|error| format!("产物不是合法的 tar：{error}"))?;

    for entry in entries {
        let mut entry = entry.map_err(|error| format!("读不了 tar 条目：{error}"))?;
        let entry_type = entry.header().entry_type();
        let path = entry
            .path()
            .map_err(|error| format!("tar 里有读不出来的路径：{error}"))?
            .to_path_buf();
        let display = path.to_string_lossy().to_string();

        if entry_type.is_dir() {
            // 归档根目录（tar 常常把它写成 `./`）本身没有内容，跳过 ——
            // 它在规范化之后就什么都不剩了，不该当成「路径非法」。
            // 注意：这只放过根，`../evil/` 这种仍然会被下面拦住。
            if is_archive_root(&display) {
                continue;
            }
            let relative = paths::safe_relative(&display)?;
            let target = paths::resolve_under(dest, &relative)?;
            fs::create_dir_all(&target).map_err(|error| format!("建不了目录：{error}"))?;
            continue;
        }
        if !entry_type.is_file() {
            return Err(format!(
                "产物里有不允许的条目类型（{entry_type:?}）：{display} —— 只接受普通文件与目录"
            ));
        }

        let relative = paths::safe_relative(&display)?;
        let target = paths::resolve_under(dest, &relative)?;
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent).map_err(|error| format!("建不了目录：{error}"))?;
        }
        // 自己拷，不用 unpack：这样不继承 tar 里的权限位、不落 setuid。
        let mut handle = fs::File::create(&target)
            .map_err(|error| format!("写不了 {}：{error}", target.display()))?;
        std::io::copy(&mut entry, &mut handle)
            .map_err(|error| format!("解不开 {}：{error}", target.display()))?;
    }
    Ok(())
}

/// 包内 toolbox.toml 必须和索引说的一致。
///
/// 这不是「安全」检查（内容本来就来自同一处），而是**一致性**检查：
/// 索引说 1.4.0、包里写着 1.3.0 的时候，你装到的东西和你以为的不是一回事。
fn verify_inner_metadata(
    payload: &Path,
    plan: &InstallPlan,
    warnings: &mut Vec<String>,
) -> Result<(), String> {
    let manifest = payload.join("toolbox.toml");
    if !manifest.is_file() {
        return Ok(());
    }
    let text = fs::read_to_string(&manifest)
        .map_err(|error| format!("读不了包内 toolbox.toml：{error}"))?;

    #[derive(serde::Deserialize)]
    struct Inner {
        package: Option<InnerPackage>,
    }
    #[derive(serde::Deserialize)]
    struct InnerPackage {
        id: Option<String>,
        version: Option<String>,
    }

    let parsed: Inner = match toml::from_str(&text) {
        Ok(parsed) => parsed,
        Err(error) => {
            warnings.push(format!("包内 toolbox.toml 读不动（忽略）：{error}"));
            return Ok(());
        }
    };

    let Some(inner) = parsed.package else {
        warnings.push(String::from("包内 toolbox.toml 没有 [package] 段"));
        return Ok(());
    };
    if let Some(id) = inner.id
        && id != plan.id
    {
        return Err(format!(
            "包内 toolbox.toml 声明的 id 是「{id}」，索引说的是「{}」",
            plan.id
        ));
    }
    if let Some(version) = inner.version
        && version != plan.version
        && version::compare(&version, &plan.version).is_some()
    {
        return Err(format!(
            "包内 toolbox.toml 声明的版本是「{version}」，索引说的是「{}」",
            plan.version
        ));
    }
    Ok(())
}

// ── 卸载 ────────────────────────────────────────────────────────────────────

/// 一次卸载的结果。
#[derive(Clone, Debug)]
pub struct UninstallReport {
    pub id: String,
    pub version: String,
    pub removed: Vec<PathBuf>,
    /// 保留下来的文件（用户改过）与理由。
    pub kept: Vec<(PathBuf, String)>,
    pub warnings: Vec<String>,
}

impl UninstallReport {
    pub fn summary(&self) -> String {
        if self.kept.is_empty() {
            format!(
                "卸载 {} {}，删掉 {} 个文件",
                self.id,
                self.version,
                self.removed.len()
            )
        } else {
            format!(
                "卸载 {} {}：删掉 {} 个，保留 {} 个（你改过，没动）",
                self.id,
                self.version,
                self.removed.len(),
                self.kept.len()
            )
        }
    }
}

/// 卸载。改过的文件**不删**，只报告。
pub fn uninstall(
    data_dir: &Path,
    id: &str,
    purge_modified: bool,
) -> Result<UninstallReport, String> {
    let package = installed::load(data_dir, id)
        .ok_or_else(|| format!("没有「{id}」的安装记录（toolbox-hub list 看看装了什么）"))?;

    let mut removed = Vec::new();
    let mut kept = Vec::new();
    let mut warnings = Vec::new();

    for file in &package.files {
        match file.current_state() {
            FileState::Gone => {}
            FileState::Unmodified => match fs::remove_file(&file.path) {
                Ok(()) => removed.push(file.path.clone()),
                Err(error) => warnings.push(format!(
                    "删不掉 {}：{error}（可能要 sudo）",
                    file.path.display()
                )),
            },
            FileState::Modified => {
                if purge_modified {
                    match fs::remove_file(&file.path) {
                        Ok(()) => removed.push(file.path.clone()),
                        Err(error) => {
                            warnings.push(format!("删不掉 {}：{error}", file.path.display()));
                        }
                    }
                } else {
                    kept.push((
                        file.path.clone(),
                        String::from("内容和你装的时候不一样（你改过），没动它"),
                    ));
                }
            }
        }
    }

    installed::remove_package_dir(data_dir, id)?;

    Ok(UninstallReport {
        id: package.id,
        version: package.version,
        removed,
        kept,
        warnings,
    })
}

// ── 更新 ────────────────────────────────────────────────────────────────────

/// 一个可升级的包。
#[derive(Clone, Debug)]
pub struct UpdateCandidate {
    pub id: String,
    #[allow(dead_code)] // 升级列表显示用
    pub name: String,
    pub current_version: String,
    pub available_version: String,
    #[allow(dead_code)] // 升级列表里要显示来自哪个仓库
    pub repository: String,
    pub repository_name: String,
    pub plan: InstallPlan,
}

/// 用当前已装的账本 + 各仓库的索引，算出哪些包有新版。
///
/// 只在**已启用**的仓库里找；同名包按仓库优先级取第一个（和索引查找顺序一致）。
pub fn update_candidates(
    data_dir: &Path,
    roots: &Roots,
    indexes: &[(RepositoryConfig, crate::repository::index::Index)],
) -> (Vec<UpdateCandidate>, Vec<String>) {
    let (packages, mut warnings) = installed::load_all(data_dir);
    let mut candidates = Vec::new();

    for package in packages {
        let mut best: Option<(&RepositoryConfig, &PackageMeta)> = None;
        for (repository, index) in indexes {
            let Some(meta) = index.find(&package.id) else {
                continue;
            };
            if !version::is_upgrade(&meta.version, &package.version) {
                continue;
            }
            let better = best
                .is_none_or(|(_, current)| version::is_upgrade(&meta.version, &current.version));
            if better {
                best = Some((repository, meta));
            }
        }
        let Some((repository, meta)) = best else {
            continue;
        };
        match plan(meta, repository, roots) {
            Ok(plan) => candidates.push(UpdateCandidate {
                id: package.id.clone(),
                name: package.name.clone(),
                current_version: package.version.clone(),
                available_version: meta.version.clone(),
                repository: repository.id.clone(),
                repository_name: repository.name.clone(),
                plan,
            }),
            Err(problem) => warnings.push(format!("{}: {problem}", package.id)),
        }
    }
    candidates.sort_by(|a, b| a.id.cmp(&b.id));
    (candidates, warnings)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repository::index::{Dependency, Index, PackageFile};

    fn root(tag: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("toolbox-hub-install-{tag}-{nanos}"));
        fs::create_dir_all(&dir).expect("mkdir");
        dir
    }

    fn roots_at(base: &Path) -> Roots {
        Roots {
            bin: base.join("bin"),
            data: base.join("data"),
            config: base.join("config"),
        }
    }

    fn repository() -> RepositoryConfig {
        RepositoryConfig {
            id: String::from("official"),
            name: String::from("ToolHub Official"),
            index: String::from("https://example.com/index.json"),
            enabled: true,
            priority: 0,
            trust: Some(String::from("trusted")),
            note: None,
        }
    }

    fn append_file<W: std::io::Write>(builder: &mut tar::Builder<W>, name: &str, bytes: &[u8]) {
        let mut header = tar::Header::new_gnu();
        header.set_size(bytes.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        builder
            .append_data(&mut header, name, bytes)
            .expect("append");
    }

    /// 写一个 tar.gz；inner_manifest 会额外写成 toolbox.toml。
    fn write_tar_gz(path: &Path, files: &[(&str, &str)], inner_manifest: Option<&str>) {
        let file = fs::File::create(path).expect("create");
        let encoder = flate2::write::GzEncoder::new(file, flate2::Compression::fast());
        let mut builder = tar::Builder::new(encoder);
        for (name, content) in files {
            append_file(&mut builder, name, content.as_bytes());
        }
        if let Some(text) = inner_manifest {
            append_file(&mut builder, "toolbox.toml", text.as_bytes());
        }
        let encoder = builder.into_inner().expect("finish tar");
        encoder.finish().expect("finish gz");
    }

    /// 造一个「产物已经在本地」的包。files: (包内路径, 内容, kind)
    fn package_with_artifact(
        base: &Path,
        id: &str,
        version: &str,
        files: &[(&str, &str, &str)],
    ) -> PackageMeta {
        let tar_path = base.join(format!("{id}-{version}.tar.gz"));
        let pairs: Vec<(&str, &str)> = files.iter().map(|(a, b, _)| (*a, *b)).collect();
        write_tar_gz(&tar_path, &pairs, None);

        PackageMeta {
            id: id.to_string(),
            name: format!("包 {id}"),
            version: version.to_string(),
            summary: Some(String::from("测试包")),
            description: None,
            categories: Vec::new(),
            tags: Vec::new(),
            author: Some(String::from("tester")),
            license: Some(String::from("MIT")),
            source: Some(String::from("https://example.com/src")),
            homepage: None,
            dependencies: Vec::new(),
            requires_root: false,
            danger: None,
            artifact: Some(Artifact {
                url: tar_path.display().to_string(),
                sha256: Some(sha256_file(&tar_path).expect("hash")),
                kind: Some(String::from("tar.gz")),
                size: None,
            }),
            files: files
                .iter()
                .map(|(path, content, kind)| PackageFile {
                    path: (*path).to_string(),
                    kind: Some((*kind).to_string()),
                    sha256: Some(sha256_text(content)),
                    executable: None,
                    ..PackageFile::default()
                })
                .collect(),
            extra: Default::default(),
        }
    }

    fn agent() -> ureq::Agent {
        cache::agent()
    }

    #[test]
    fn sha256_matches_known_vectors() {
        assert_eq!(
            sha256_text(""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            sha256_text("abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn a_plan_lays_out_targets_without_touching_disk() {
        let base = root("plan");
        let roots = roots_at(&base);
        let meta = package_with_artifact(
            &base,
            "demo",
            "1.0.0",
            &[
                ("scripts/demo-run", "#!/bin/sh\necho hi\n", "bin"),
                ("manifests/demo.toml", "[[action]]\n", "manifest"),
            ],
        );

        let plan = plan(&meta, &repository(), &roots).expect("应能计划");
        assert_eq!(plan.id, "demo");
        assert!(!plan.is_upgrade());
        assert!(plan.has_executables());
        assert!(plan.deps_ready());
        assert!(!plan.has_conflicts());
        assert_eq!(plan.files.len(), 2);
        assert_eq!(plan.files[0].target, roots.bin.join("demo-run"));
        assert_eq!(
            plan.files[1].target,
            installed::files_dir(&roots.data, "demo").join("manifests/demo.toml")
        );
        // 计划阶段一个字节都不该写
        assert!(!roots.bin.exists());
        assert!(!roots.data.exists());
        fs::remove_dir_all(&base).expect("cleanup");
    }

    #[test]
    fn install_then_uninstall_round_trips() {
        let base = root("round");
        let roots = roots_at(&base);
        let meta = package_with_artifact(
            &base,
            "demo",
            "1.0.0",
            &[
                ("scripts/demo-run", "#!/bin/sh\necho hi\n", "bin"),
                (
                    "manifests/demo.toml",
                    "[[action]]\nname=\"x\"\n",
                    "manifest",
                ),
            ],
        );
        let installing = plan(&meta, &repository(), &roots).expect("plan");
        let report = install(&installing, &roots, &agent(), false).expect("install");

        assert_eq!(report.id, "demo");
        assert_eq!(report.integrity, Integrity::VerifiedLocal);
        assert_eq!(report.files.len(), 2);

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(roots.bin.join("demo-run"))
                .expect("stat")
                .permissions()
                .mode();
            assert_eq!(mode & 0o111, 0o111, "应当可执行");
        }

        let ledger = installed::load(&roots.data, "demo").expect("ledger");
        assert_eq!(ledger.version, "1.0.0");
        assert_eq!(ledger.trust(), Trust::Trusted);
        assert!(ledger.has_executables());

        let report = uninstall(&roots.data, "demo", false).expect("uninstall");
        assert_eq!(report.removed.len(), 2);
        assert!(report.kept.is_empty());
        assert!(!roots.bin.join("demo-run").exists());
        assert!(!installed::package_dir(&roots.data, "demo").exists());
        fs::remove_dir_all(&base).expect("cleanup");
    }

    #[test]
    fn a_hash_mismatch_refuses_the_install() {
        let base = root("hash");
        let roots = roots_at(&base);
        let mut meta = package_with_artifact(
            &base,
            "demo",
            "1.0.0",
            &[("scripts/demo-run", "#!/bin/sh\n", "bin")],
        );
        meta.artifact.as_mut().unwrap().sha256 = Some("f".repeat(64));
        let plan = plan(&meta, &repository(), &roots).expect("plan");

        let problem = install(&plan, &roots, &agent(), false).unwrap_err();
        assert!(problem.contains("SHA-256"), "{problem}");
        assert!(problem.contains("拒绝安装"), "{problem}");
        assert!(!roots.bin.join("demo-run").exists(), "不该有文件落地");
        assert!(installed::load(&roots.data, "demo").is_none());
        fs::remove_dir_all(&base).expect("cleanup");
    }

    #[test]
    fn a_per_file_hash_mismatch_refuses_the_install() {
        let base = root("filehash");
        let roots = roots_at(&base);
        let mut meta = package_with_artifact(
            &base,
            "demo",
            "1.0.0",
            &[("scripts/demo-run", "#!/bin/sh\n", "bin")],
        );
        meta.files[0].sha256 = Some("a".repeat(64));
        let plan = plan(&meta, &repository(), &roots).expect("plan");

        let problem = install(&plan, &roots, &agent(), false).unwrap_err();
        assert!(problem.contains("不一致"), "{problem}");
        assert!(!roots.bin.join("demo-run").exists());
        fs::remove_dir_all(&base).expect("cleanup");
    }

    #[test]
    fn a_root_package_is_refused_not_sudoed() {
        let base = root("root");
        let roots = roots_at(&base);
        let mut meta = package_with_artifact(
            &base,
            "sysdemo",
            "1.0.0",
            &[("scripts/x", "#!/bin/sh\n", "bin")],
        );
        meta.requires_root = true;
        let plan = plan(&meta, &repository(), &roots).expect("plan");

        let problem = plan.check(&roots, false).unwrap_err();
        assert!(problem.contains("requires_root"), "{problem}");
        assert!(problem.contains("不会替你提权"), "{problem}");
        fs::remove_dir_all(&base).expect("cleanup");
    }

    #[test]
    fn a_remote_artifact_without_a_hash_needs_an_explicit_opt_in() {
        let base = root("nohash");
        let roots = roots_at(&base);
        let mut meta = package_with_artifact(
            &base,
            "demo",
            "1.0.0",
            &[("scripts/demo-run", "#!/bin/sh\n", "bin")],
        );
        meta.artifact.as_mut().unwrap().sha256 = None;
        meta.artifact.as_mut().unwrap().url = "https://example.com/a.tar.gz".to_string();

        let plan = plan(&meta, &repository(), &roots).expect("plan");
        let problem = plan.check(&roots, false).unwrap_err();
        assert!(problem.contains("SHA-256"), "{problem}");
        assert!(problem.contains("--allow-unverified"), "{problem}");
        assert!(plan.check(&roots, true).is_ok(), "显式接受之后要放行");
        fs::remove_dir_all(&base).expect("cleanup");
    }

    #[test]
    fn a_file_owned_by_another_package_is_a_conflict() {
        let base = root("conflict");
        let roots = roots_at(&base);
        let meta = package_with_artifact(
            &base,
            "first",
            "1.0.0",
            &[("scripts/shared", "#!/bin/sh\necho first\n", "bin")],
        );
        let first_plan = plan(&meta, &repository(), &roots).expect("plan");
        install(&first_plan, &roots, &agent(), false).expect("install");

        let second = package_with_artifact(
            &base,
            "second",
            "1.0.0",
            &[("scripts/shared", "#!/bin/sh\necho second\n", "bin")],
        );
        let second_plan = plan(&second, &repository(), &roots).expect("plan");
        assert!(
            second_plan.has_conflicts(),
            "同一个文件被两个包认领必须报冲突：{:?}",
            second_plan.files
        );
        assert!(
            second_plan
                .check(&roots, false)
                .unwrap_err()
                .contains("已经被包")
        );
        fs::remove_dir_all(&base).expect("cleanup");
    }

    #[test]
    fn uninstall_keeps_files_the_user_modified() {
        let base = root("modified");
        let roots = roots_at(&base);
        let meta = package_with_artifact(
            &base,
            "demo",
            "1.0.0",
            &[("scripts/demo-run", "#!/bin/sh\necho hi\n", "bin")],
        );
        let plan = plan(&meta, &repository(), &roots).expect("plan");
        install(&plan, &roots, &agent(), false).expect("install");

        let target = roots.bin.join("demo-run");
        fs::write(&target, "#!/bin/sh\necho 我自己改的\n").expect("write");

        let report = uninstall(&roots.data, "demo", false).expect("uninstall");
        assert!(report.removed.is_empty());
        assert_eq!(report.kept.len(), 1);
        assert!(target.exists(), "改过的文件必须留着");
        assert!(report.summary().contains("保留 1 个"));
        assert!(installed::load(&roots.data, "demo").is_none());
        fs::remove_dir_all(&base).expect("cleanup");
    }

    #[test]
    fn uninstall_refuses_an_unknown_package() {
        let base = root("unknown");
        let roots = roots_at(&base);
        let problem = uninstall(&roots.data, "没装过", false).unwrap_err();
        assert!(problem.contains("没有"), "{problem}");
        fs::remove_dir_all(&base).expect("cleanup");
    }

    #[test]
    fn a_plan_upgrades_when_a_newer_version_appears() {
        let base = root("upgrade");
        let roots = roots_at(&base);
        let first = package_with_artifact(
            &base,
            "demo",
            "1.0.0",
            &[("scripts/demo-run", "#!/bin/sh\necho v1\n", "bin")],
        );
        let installing = plan(&first, &repository(), &roots).expect("plan");
        install(&installing, &roots, &agent(), false).expect("install");

        let second = package_with_artifact(
            &base,
            "demo",
            "2.0.0",
            &[("scripts/demo-run", "#!/bin/sh\necho v2\n", "bin")],
        );
        let plan = plan(&second, &repository(), &roots).expect("plan");
        assert!(plan.is_upgrade());
        assert_eq!(plan.installed_version.as_deref(), Some("1.0.0"));

        let report = install(&plan, &roots, &agent(), false).expect("install");
        assert_eq!(report.version, "2.0.0");
        let contents = fs::read_to_string(roots.bin.join("demo-run")).expect("read");
        assert!(contents.contains("v2"));
        fs::remove_dir_all(&base).expect("cleanup");
    }

    #[test]
    fn inner_metadata_must_match_the_index() {
        let base = root("inner");
        let roots = roots_at(&base);
        let tar_path = base.join("inner.tar.gz");
        write_tar_gz(
            &tar_path,
            &[],
            Some("[package]\nid = \"demo\"\nversion = \"9.9.9\"\n"),
        );
        let meta = PackageMeta {
            id: String::from("demo"),
            name: String::from("包 demo"),
            version: String::from("1.0.0"),
            summary: Some(String::from("x")),
            description: None,
            categories: Vec::new(),
            tags: Vec::new(),
            author: None,
            license: None,
            source: None,
            homepage: None,
            dependencies: Vec::new(),
            requires_root: false,
            danger: None,
            artifact: Some(Artifact {
                url: tar_path.display().to_string(),
                sha256: Some(sha256_file(&tar_path).expect("hash")),
                kind: Some(String::from("tar.gz")),
                size: None,
            }),
            files: vec![PackageFile {
                path: "toolbox.toml".to_string(),
                kind: Some("manifest".to_string()),
                ..PackageFile::default()
            }],
            extra: Default::default(),
        };
        let plan = plan(&meta, &repository(), &roots).expect("plan");
        let problem = install(&plan, &roots, &agent(), false).unwrap_err();
        assert!(problem.contains("版本"), "{problem}");
        assert!(problem.contains("9.9.9"), "{problem}");
        fs::remove_dir_all(&base).expect("cleanup");
    }

    #[test]
    fn files_not_declared_in_the_index_are_not_installed() {
        let base = root("extra");
        let roots = roots_at(&base);
        let tar_path = base.join("extra.tar.gz");
        write_tar_gz(
            &tar_path,
            &[
                ("scripts/wanted", "#!/bin/sh\n"),
                ("scripts/unwanted", "#!/bin/sh\necho nope\n"),
            ],
            None,
        );
        let meta = PackageMeta {
            id: String::from("demo"),
            name: String::from("包 demo"),
            version: String::from("1.0.0"),
            summary: Some(String::from("x")),
            description: None,
            categories: Vec::new(),
            tags: Vec::new(),
            author: None,
            license: None,
            source: None,
            homepage: None,
            dependencies: Vec::new(),
            requires_root: false,
            danger: None,
            artifact: Some(Artifact {
                url: tar_path.display().to_string(),
                sha256: Some(sha256_file(&tar_path).expect("hash")),
                kind: Some(String::from("tar.gz")),
                size: None,
            }),
            files: vec![PackageFile {
                path: "scripts/wanted".to_string(),
                kind: Some("bin".to_string()),
                ..PackageFile::default()
            }],
            extra: Default::default(),
        };
        let plan = plan(&meta, &repository(), &roots).expect("plan");
        install(&plan, &roots, &agent(), false).expect("install");
        assert!(roots.bin.join("wanted").exists());
        assert!(!roots.bin.join("unwanted").exists(), "没声明的文件不许装");
        fs::remove_dir_all(&base).expect("cleanup");
    }

    #[test]
    fn missing_dependencies_are_reported_not_installed() {
        let base = root("deps");
        let roots = roots_at(&base);
        let mut meta = package_with_artifact(
            &base,
            "demo",
            "1.0.0",
            &[("scripts/demo-run", "#!/bin/sh\n", "bin")],
        );
        meta.dependencies = vec![
            Dependency {
                command: String::from("definitely-not-a-real-command-xyz"),
                hint: Some(String::from("sudo pacman -S whatever")),
            },
            Dependency {
                command: String::from("sh"),
                hint: None,
            },
        ];
        let plan = plan(&meta, &repository(), &roots).expect("plan");
        assert!(!plan.deps_ready());
        assert_eq!(plan.missing_dependencies.len(), 1);
        assert_eq!(plan.dep_hints.len(), 1);
        // 缺依赖不是不能装
        assert!(plan.check(&roots, false).is_ok());
        fs::remove_dir_all(&base).expect("cleanup");
    }

    #[test]
    fn command_availability_handles_paths_and_bare_names() {
        assert!(command_available("sh"));
        assert!(command_available("/bin/sh"));
        assert!(!command_available("definitely-not-a-real-command-xyz"));
        assert!(!command_available("/definitely/not/here"));
    }

    #[test]
    fn update_candidates_pick_only_newer_versions() {
        let base = root("update");
        let roots = roots_at(&base);
        let old = package_with_artifact(
            &base,
            "demo",
            "1.0.0",
            &[("scripts/demo-run", "#!/bin/sh\necho v1\n", "bin")],
        );
        let plan = plan(&old, &repository(), &roots).expect("plan");
        install(&plan, &roots, &agent(), false).expect("install");

        let index = Index::parse(
            r#"{"schema_version": 1, "packages": [
                 {"id": "demo", "name": "包 demo", "version": "2.0.0",
                  "files": [{"path": "scripts/demo-run", "kind": "bin"}]},
                 {"id": "other", "name": "别的", "version": "9.0.0",
                  "files": [{"path": "scripts/other"}]}
               ]}"#,
        )
        .expect("parse")
        .index;

        let (candidates, warnings) =
            update_candidates(&roots.data, &roots, &[(repository(), index)]);
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(candidates.len(), 1, "只该有 demo 有新版");
        assert_eq!(candidates[0].id, "demo");
        assert_eq!(candidates[0].current_version, "1.0.0");
        assert_eq!(candidates[0].available_version, "2.0.0");

        let same = Index::parse(
            r#"{"schema_version": 1, "packages": [
                 {"id": "demo", "name": "包 demo", "version": "1.0.0",
                  "files": [{"path": "scripts/demo-run", "kind": "bin"}]}
               ]}"#,
        )
        .expect("parse")
        .index;
        let (candidates, _) = update_candidates(&roots.data, &roots, &[(repository(), same)]);
        assert!(candidates.is_empty(), "没有新版就不该有候选");
        fs::remove_dir_all(&base).expect("cleanup");
    }

    /// 安全：产物里的目录穿越必须在解包时被拦住。
    #[test]
    fn tar_entries_escaping_the_staging_dir_are_refused() {
        let base = root("traversal");
        let dest = base.join("dest");
        fs::create_dir_all(&dest).expect("mkdir");

        let mut builder = tar::Builder::new(Vec::new());
        let mut header = tar::Header::new_gnu();
        let payload = b"pwned";
        header.set_size(payload.len() as u64);
        header.set_mode(0o644);
        header.set_path("../escaped.txt").ok();
        header.set_cksum();
        builder.append(&header, &payload[..]).ok();
        let bytes = builder.into_inner().expect("tar bytes");

        let archive_path = base.join("evil.tar");
        fs::write(&archive_path, &bytes).expect("write");
        let outcome = extract_tar(fs::File::open(&archive_path).expect("open"), &dest);
        assert!(outcome.is_err(), "越界条目必须被拒绝");
        assert!(!base.join("escaped.txt").exists());
        fs::remove_dir_all(&base).expect("cleanup");
    }

    /// 安全：符号链接条目直接拒绝。
    #[test]
    fn tar_symlink_entries_are_refused() {
        let base = root("symlink");
        let dest = base.join("dest");
        fs::create_dir_all(&dest).expect("mkdir");

        let mut builder = tar::Builder::new(Vec::new());
        let mut header = tar::Header::new_gnu();
        header.set_entry_type(tar::EntryType::Symlink);
        header.set_size(0);
        header.set_mode(0o777);
        header.set_path("link").expect("path");
        header.set_link_name("/etc/passwd").expect("link");
        header.set_cksum();
        builder.append(&header, std::io::empty()).expect("append");
        let bytes = builder.into_inner().expect("tar bytes");

        let archive_path = base.join("link.tar");
        fs::write(&archive_path, &bytes).expect("write");
        let problem = extract_tar(fs::File::open(&archive_path).expect("open"), &dest).unwrap_err();
        assert!(problem.contains("不允许的条目类型"), "{problem}");
        fs::remove_dir_all(&base).expect("cleanup");
    }

    #[test]
    fn writing_is_atomic_and_leaves_no_temp_files() {
        let base = root("atomic");
        let target = base.join("sub/dir/file");
        write_atomically(&target, b"hello", false).expect("write");
        assert_eq!(fs::read(&target).expect("read"), b"hello");
        let leftovers: Vec<_> = fs::read_dir(target.parent().expect("parent"))
            .expect("read_dir")
            .filter_map(|entry| entry.ok())
            .filter(|entry| entry.file_name().to_string_lossy().contains("toolbox-tmp"))
            .collect();
        assert!(leftovers.is_empty(), "不该留下临时文件");
        fs::remove_dir_all(&base).expect("cleanup");
    }

    #[test]
    fn the_test_fixture_archive_round_trips() {
        let base = root("fixture");
        let path = base.join("a.tar.gz");
        write_tar_gz(&path, &[("scripts/x", "hello")], None);
        let out = base.join("out");
        fs::create_dir_all(&out).expect("mkdir");
        extract(
            &Artifact {
                url: path.display().to_string(),
                sha256: None,
                kind: Some(String::from("tar.gz")),
                size: None,
            },
            &path,
            &out,
        )
        .expect("extract");
        assert_eq!(
            fs::read_to_string(out.join("scripts/x")).expect("read"),
            "hello"
        );
        fs::remove_dir_all(&base).expect("cleanup");
    }

    #[test]
    fn an_unsafe_path_is_refused_before_anything_is_downloaded() {
        let base = root("unsafe");
        let roots = roots_at(&base);
        let mut meta = package_with_artifact(
            &base,
            "demo",
            "1.0.0",
            &[("scripts/demo-run", "#!/bin/sh\n", "bin")],
        );
        meta.files[0].path = "../escape".to_string();
        let problem = plan(&meta, &repository(), &roots).unwrap_err();
        assert!(problem.contains(".."), "{problem}");
        fs::remove_dir_all(&base).expect("cleanup");
    }

    #[test]
    fn a_package_with_nothing_to_install_is_refused() {
        let base = root("empty");
        let roots = roots_at(&base);
        let meta = PackageMeta {
            id: String::from("empty"),
            name: String::from("空包"),
            version: String::from("1.0.0"),
            ..PackageMeta::default()
        };
        assert!(plan(&meta, &repository(), &roots).is_err());
        fs::remove_dir_all(&base).expect("cleanup");
    }

    #[test]
    fn a_single_file_artifact_installs() {
        let base = root("single");
        let roots = roots_at(&base);
        let script = base.join("tool.sh");
        fs::write(&script, "#!/bin/sh\necho single\n").expect("write");

        let meta = PackageMeta {
            id: String::from("single"),
            name: String::from("单文件"),
            version: String::from("1.0.0"),
            summary: Some(String::from("x")),
            artifact: Some(Artifact {
                url: script.display().to_string(),
                sha256: Some(sha256_file(&script).expect("hash")),
                kind: Some(String::from("file")),
                size: None,
            }),
            files: vec![PackageFile {
                path: "scripts/single".to_string(),
                kind: Some("bin".to_string()),
                ..PackageFile::default()
            }],
            ..PackageMeta::default()
        };
        let plan = plan(&meta, &repository(), &roots).expect("plan");
        let report = install(&plan, &roots, &agent(), false).expect("install");
        assert_eq!(report.integrity, Integrity::VerifiedLocal);
        assert!(roots.bin.join("single").exists());
        assert_eq!(
            fs::read_to_string(roots.bin.join("single")).expect("read"),
            "#!/bin/sh\necho single\n"
        );
        fs::remove_dir_all(&base).expect("cleanup");
    }

    #[test]
    fn a_staging_dir_is_cleaned_up_after_a_successful_install() {
        let base = root("staging");
        let roots = roots_at(&base);
        let meta = package_with_artifact(
            &base,
            "demo",
            "1.0.0",
            &[("scripts/demo-run", "#!/bin/sh\n", "bin")],
        );
        let plan = plan(&meta, &repository(), &roots).expect("plan");
        install(&plan, &roots, &agent(), false).expect("install");
        assert!(!roots.data.join(".staging").exists(), "暂存目录要清干净");
        fs::remove_dir_all(&base).expect("cleanup");
    }
}
