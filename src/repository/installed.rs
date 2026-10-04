//! 已安装包的账本。
//!
//! 卸载**不能靠猜路径**：\`rm -rf\` 一个推导出来的目录是这类工具最容易造成灾难的
//! 地方。所以每装一个包，都在它自己的目录里留一份 \`installed.toml\`：
//!
//! \`\`\`text
//! <数据目录>/packages/<包 id>/
//! ├── installed.toml      ← 账本：装了哪些文件、每个文件的 sha256、来源、信任
//! └── files/              ← 包自己的载荷（manifest / 文档 / 数据）
//! \`\`\`
//!
//! 可执行脚本不在 \`files/\` 里 —— 它们装在 \`~/.local/bin\`（或用户配置的目录），
//! 账本里记的是**绝对路径 + 当时的 sha256**。卸载时逐个核对：
//!
//! * 哈希没变 → 是我们装的、用户也没动过 → 删；
//! * 哈希变了 → 用户改过 → **不删**，只报告。
//!
//! 这条规则同样适用于更新：绝不悄悄覆盖用户改过的文件。

use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};

use crate::repository::{atomic, index::FileKind, version::Version};

/// 账本格式版本。
pub const SCHEMA_VERSION: u32 = 1;

/// 账本里记录的一个文件。
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct InstalledFile {
    /// 落到磁盘上的**绝对**路径。
    pub path: PathBuf,
    /// 装好那一刻的 SHA-256（小写）。卸载/更新时拿它判断「用户改过没有」。
    pub sha256: String,
    /// 落地类别。
    pub kind: String,
    /// 包内相对路径（装之前的样子，报错时好定位）。
    pub source: String,
}

impl InstalledFile {
    pub fn file_kind(&self) -> FileKind {
        FileKind::parse(&self.kind).unwrap_or(FileKind::Data)
    }

    /// 这个文件现在还在不在、有没有被改过。
    pub fn current_state(&self) -> FileState {
        match fs::read(&self.path) {
            Err(_) => FileState::Gone,
            Ok(bytes) => {
                if crate::repository::install::sha256_bytes(&bytes) == self.sha256 {
                    FileState::Unmodified
                } else {
                    FileState::Modified
                }
            }
        }
    }
}

/// 账本里那个文件现在的状态。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FileState {
    /// 原样没动 —— 可以安全删除。
    Unmodified,
    /// 内容变了（用户改过，或者被别的程序覆盖）—— 不删，只报告。
    Modified,
    /// 已经不在了（用户自己删了）—— 无事可做。
    Gone,
}

impl FileState {
    #[allow(dead_code)] // 卸载报告里要用的措辞
    pub fn label(self) -> &'static str {
        match self {
            FileState::Unmodified => "原样",
            FileState::Modified => "已修改",
            FileState::Gone => "已不在",
        }
    }
}

/// 一个已安装的包。
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct InstalledPackage {
    pub schema_version: u32,
    pub id: String,
    pub name: String,
    pub version: String,
    /// 从哪个仓库装的（仓库 id 与展示名都留一份：仓库可能后来被删掉）。
    pub repository: String,
    #[serde(default)]
    pub repository_name: String,
    /// 装的时候那个仓库的信任等级（历史事实，不随后续配置改变）。
    pub trust: String,
    pub installed_at: u64,
    #[serde(default)]
    pub source: Option<String>,
    #[serde(default)]
    pub license: Option<String>,
    #[serde(default)]
    pub requires_root: bool,
    #[serde(default)]
    pub danger: String,
    /// 装的时候校验过的产物 SHA-256（本地仓库可能没有）。
    #[serde(default)]
    pub artifact_sha256: Option<String>,
    #[serde(default)]
    pub dependencies: Vec<String>,
    pub files: Vec<InstalledFile>,
    /// 装的时候是不是被用户显式放过「来源不可信」——留个记录，好让 update 一视同仁。
    #[serde(default)]
    pub allow_unverified: bool,
}

impl InstalledPackage {
    #[allow(dead_code)] // 版本比较入口
    pub fn parsed_version(&self) -> Option<Version> {
        Version::parse(&self.version)
    }

    pub fn file_count(&self) -> usize {
        self.files.len()
    }

    /// 有没有可执行脚本（界面上「包含可执行脚本」那句警告用它）。
    pub fn has_executables(&self) -> bool {
        self.files
            .iter()
            .any(|file| file.file_kind() == FileKind::Bin)
    }

    pub fn trust(&self) -> crate::repository::config::Trust {
        crate::repository::config::Trust::parse(&self.trust)
            .unwrap_or(crate::repository::config::Trust::Unknown)
    }

    /// 包自己的载荷目录。
    #[allow(dead_code)] // 载荷目录入口
    pub fn payload_dir(&self, data_dir: &Path) -> PathBuf {
        files_dir(data_dir, &self.id)
    }
}

/// \`<数据目录>/packages\`。
pub fn packages_dir(data_dir: &Path) -> PathBuf {
    data_dir.join("packages")
}

/// \`<数据目录>/packages/<id>\`。
pub fn package_dir(data_dir: &Path, id: &str) -> PathBuf {
    packages_dir(data_dir).join(id)
}

/// 包载荷目录（\`…/<id>/files\`）。manifest 就从这里被 Provider 读走。
pub fn files_dir(data_dir: &Path, id: &str) -> PathBuf {
    package_dir(data_dir, id).join("files")
}

/// 账本文件。
pub fn ledger_path(data_dir: &Path, id: &str) -> PathBuf {
    package_dir(data_dir, id).join("installed.toml")
}

/// 读一个包的账本。没有、读不动、解析不了都返回 \`None\`。
pub fn load(data_dir: &Path, id: &str) -> Option<InstalledPackage> {
    let text = fs::read_to_string(ledger_path(data_dir, id)).ok()?;
    let package: InstalledPackage = toml::from_str(&text).ok()?;
    // 账本里声明的 id 必须和目录名一致，否则按目录名算 —— 免得手改一个字段
    // 就让卸载去删别的包。
    if package.id == id {
        Some(package)
    } else {
        None
    }
}

/// 读全部账本。坏掉的账本**不会**被静默丢掉：它的 id 会出现在警告里，
/// 因为「我装过一个包，但界面里看不见它」是最糟的一种失败。
pub fn load_all(data_dir: &Path) -> (Vec<InstalledPackage>, Vec<String>) {
    let dir = packages_dir(data_dir);
    let Ok(read_dir) = fs::read_dir(&dir) else {
        return (Vec::new(), Vec::new());
    };
    let mut entries: Vec<PathBuf> = read_dir
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| path.is_dir())
        .collect();
    entries.sort();

    let mut packages = Vec::new();
    let mut warnings = Vec::new();
    for path in entries {
        let Some(id) = path
            .file_name()
            .map(|name| name.to_string_lossy().to_string())
        else {
            continue;
        };
        match load(data_dir, &id) {
            Some(package) => packages.push(package),
            None => warnings.push(format!(
                "{} 的账本读不出来（{}），这个包不会被管理",
                id,
                ledger_path(data_dir, &id).display()
            )),
        }
    }
    (packages, warnings)
}

/// 写账本（会自动建目录）。走原子写：写一半断电，账本坏了包就成了孤儿。
pub fn save(data_dir: &Path, package: &InstalledPackage) -> Result<(), String> {
    let text = toml::to_string_pretty(package).map_err(|error| format!("写不了 TOML：{error}"))?;
    let path = ledger_path(data_dir, &package.id);
    atomic::write(&path, text.as_bytes())
        .map_err(|error| format!("写不了 {}：{error}", path.display()))
}

/// 删掉一个包的整个目录（账本 + 载荷）。**不**碰任何 \`files\` 里记的绝对路径 ——
/// 那一步由卸载逻辑逐个判断后再删。
pub fn remove_package_dir(data_dir: &Path, id: &str) -> Result<(), String> {
    let dir = package_dir(data_dir, id);
    if !dir.exists() {
        return Ok(());
    }
    fs::remove_dir_all(&dir).map_err(|error| format!("删不掉 {}：{error}", dir.display()))
}

/// 已安装包的版本表（\`id → 版本\`），用来标「已安装 / 可升级」。
pub fn installed_versions(data_dir: &Path) -> BTreeMap<String, String> {
    let (packages, _) = load_all(data_dir);
    packages
        .into_iter()
        .map(|package| (package.id, package.version))
        .collect()
}

/// 一个文件归谁所有（冲突检测用）。
///
/// 返回 \`(包 id, 该文件当时的 sha256)\` —— 调用方据此决定：同一个包自己重装
/// 可以覆盖；别人的文件就是**冲突**。
/// 只在测试里用：[`Ownership`] 是生产路径（建表一次、二分查找），
/// 这个逐文件重读账本的版本留着当**独立参照** —— 两边对不上就是有一边写错了。
#[cfg(test)]
pub fn owner_of(data_dir: &Path, path: &Path) -> Option<(String, String)> {
    let (packages, _) = load_all(data_dir);
    for package in packages {
        for file in &package.files {
            if file.path == path {
                return Some((package.id.clone(), file.sha256.clone()));
            }
        }
    }
    None
}

/// 一次建好的「文件 → 属主」表（给批量算计划用）。
///
/// [`owner_of`] 每问一个文件就重读一遍全部账本；一个计划有 N 个文件、
/// 一批计划有 M 个包，那就是平方级的磁盘读。批量场景先 `build` 一份再逐个查。
pub struct Ownership {
    /// (路径, 包 id, 安装时的 sha256)，按路径有序，二分查找。
    files: Vec<(PathBuf, String, String)>,
}

impl Ownership {
    pub fn build(data_dir: &Path) -> Self {
        let (packages, _) = load_all(data_dir);
        let mut files = Vec::new();
        for package in packages {
            for file in package.files {
                files.push((file.path, package.id.clone(), file.sha256));
            }
        }
        files.sort_by(|a, b| a.0.cmp(&b.0));
        Self { files }
    }

    /// 文件归谁所有：\`(包 id, 安装时的 sha256)\`。与 [`owner_of`] 同语义。
    pub fn owner_of(&self, path: &Path) -> Option<(&str, &str)> {
        let mut index = self
            .files
            .binary_search_by(|entry| entry.0.as_path().cmp(path))
            .ok()?;
        // 二分命中的一串同路径条目里，取最先记的那个（与 owner_of 一致）。
        while index > 0 && self.files[index - 1].0 == self.files[index].0 {
            index -= 1;
        }
        let entry = &self.files[index];
        Some((entry.1.as_str(), entry.2.as_str()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn root(tag: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("toolbox-hub-installed-{tag}-{nanos}"));
        fs::create_dir_all(&dir).expect("mkdir");
        dir
    }

    fn package(id: &str, version: &str, files: Vec<InstalledFile>) -> InstalledPackage {
        InstalledPackage {
            schema_version: SCHEMA_VERSION,
            id: id.to_string(),
            name: format!("包 {id}"),
            version: version.to_string(),
            repository: "official".to_string(),
            repository_name: "ToolHub Official".to_string(),
            trust: "trusted".to_string(),
            installed_at: 1_700_000_000,
            source: Some("https://example.com".to_string()),
            license: Some("MIT".to_string()),
            requires_root: false,
            danger: "safe".to_string(),
            artifact_sha256: Some("a".repeat(64)),
            dependencies: vec!["jq".to_string()],
            files,
            allow_unverified: false,
        }
    }

    #[test]
    fn a_ledger_round_trips() {
        let data = root("round");
        let file = data.join("bin/a");
        fs::create_dir_all(file.parent().unwrap()).expect("mkdir");
        fs::write(&file, b"hello").expect("write");
        let entry = InstalledFile {
            path: file.clone(),
            sha256: crate::repository::install::sha256_bytes(b"hello"),
            kind: "bin".to_string(),
            source: "scripts/a".to_string(),
        };
        let original = package("a", "1.0.0", vec![entry]);

        save(&data, &original).expect("save");
        let loaded = load(&data, "a").expect("load");
        assert_eq!(loaded, original);
        assert_eq!(loaded.file_count(), 1);
        assert!(loaded.has_executables());
        fs::remove_dir_all(&data).expect("cleanup");
    }

    #[test]
    fn a_missing_or_broken_ledger_is_a_clean_miss() {
        let data = root("broken");
        assert!(load(&data, "nope").is_none());

        fs::create_dir_all(package_dir(&data, "bad")).expect("mkdir");
        fs::write(ledger_path(&data, "bad"), "{{{").expect("write");
        assert!(load(&data, "bad").is_none());

        let (packages, warnings) = load_all(&data);
        assert!(packages.is_empty());
        assert_eq!(warnings.len(), 1, "坏账本要报出来，不能静默消失");
        assert!(warnings[0].contains("bad"));
        fs::remove_dir_all(&data).expect("cleanup");
    }

    /// 账本里的 id 和目录名对不上时按目录名算 —— 免得卸载去删别的包。
    #[test]
    fn a_mismatched_id_in_the_ledger_is_refused() {
        let data = root("mismatch");
        fs::create_dir_all(package_dir(&data, "dir-name")).expect("mkdir");
        let text = toml::to_string(&package("other-name", "1.0.0", Vec::new())).expect("encode");
        fs::write(ledger_path(&data, "dir-name"), text).expect("write");
        assert!(load(&data, "dir-name").is_none());
        fs::remove_dir_all(&data).expect("cleanup");
    }

    #[test]
    fn file_state_tells_unmodified_from_modified_from_gone() {
        let data = root("state");
        let intact = data.join("intact");
        let changed = data.join("changed");
        let gone = data.join("gone");
        fs::write(&intact, b"same").expect("write");
        fs::write(&changed, b"other").expect("write");

        let entry = |path: &Path, bytes: &[u8]| InstalledFile {
            path: path.to_path_buf(),
            sha256: crate::repository::install::sha256_bytes(bytes),
            kind: "data".to_string(),
            source: "x".to_string(),
        };
        assert_eq!(
            entry(&intact, b"same").current_state(),
            FileState::Unmodified
        );
        assert_eq!(
            entry(&changed, b"original").current_state(),
            FileState::Modified
        );
        assert_eq!(entry(&gone, b"x").current_state(), FileState::Gone);
        fs::remove_dir_all(&data).expect("cleanup");
    }

    #[test]
    fn all_ledgers_load_and_the_version_table_is_built() {
        let data = root("all");
        save(&data, &package("a", "1.0.0", Vec::new())).expect("save");
        save(&data, &package("b", "2.3.4", Vec::new())).expect("save");

        let (packages, warnings) = load_all(&data);
        assert!(warnings.is_empty());
        let ids: Vec<&str> = packages.iter().map(|p| p.id.as_str()).collect();
        assert_eq!(ids, vec!["a", "b"]);

        let versions = installed_versions(&data);
        assert_eq!(versions.get("b").map(String::as_str), Some("2.3.4"));
        fs::remove_dir_all(&data).expect("cleanup");
    }

    #[test]
    fn ownership_is_found_across_packages() {
        let data = root("owner");
        let shared = data.join("bin/shared");
        save(
            &data,
            &package(
                "a",
                "1.0.0",
                vec![InstalledFile {
                    path: shared.clone(),
                    sha256: "b".repeat(64),
                    kind: "bin".to_string(),
                    source: "scripts/shared".to_string(),
                }],
            ),
        )
        .expect("save");

        let (owner, hash) = owner_of(&data, &shared).expect("应能找到主人");
        assert_eq!(owner, "a");
        assert_eq!(hash, "b".repeat(64));
        assert!(owner_of(&data, Path::new("/nope")).is_none());
        fs::remove_dir_all(&data).expect("cleanup");
    }

    #[test]
    fn removing_a_package_dir_takes_the_ledger_with_it() {
        let data = root("remove");
        save(&data, &package("a", "1.0.0", Vec::new())).expect("save");
        assert!(package_dir(&data, "a").exists());
        remove_package_dir(&data, "a").expect("remove");
        assert!(!package_dir(&data, "a").exists());
        // 幂等：再删一次不该报错
        remove_package_dir(&data, "a").expect("remove again");
        fs::remove_dir_all(&data).expect("cleanup");
    }

    #[test]
    fn trust_is_read_from_the_ledger_and_degrades_safely() {
        let mut entry = package("a", "1.0.0", Vec::new());
        assert_eq!(entry.trust(), crate::repository::config::Trust::Trusted);
        entry.trust = "乱写".to_string();
        assert_eq!(entry.trust(), crate::repository::config::Trust::Unknown);
        fs::remove_dir_all(root("trust")).ok();
    }
}
