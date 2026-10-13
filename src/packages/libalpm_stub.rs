//! 未启用 `pacman` feature 时的兼容层。
//!
//! 保留 UI/worker 所需的类型边界，但明确拒绝访问 Arch 数据库；不伪装成空数据库。
use std::{
    collections::{HashMap, HashSet},
    time::Duration,
};

use super::{InstalledPackage, PackageHit};

#[derive(Debug)]
pub struct Db {
    local_versions: HashMap<String, String>,
    sync_names: HashSet<String>,
}

impl Db {
    pub fn open() -> Result<Self, String> {
        Err(String::from(
            "此构建未启用包管理；请使用默认 features 或启用 pacman feature",
        ))
    }
    pub fn search(&self, _term: &str) -> Result<Vec<PackageHit>, String> {
        Err(Self::disabled())
    }
    pub fn info(&self, _name: &str) -> Result<Vec<(String, String)>, String> {
        Err(Self::disabled())
    }
    pub fn installed(&self) -> Result<Vec<InstalledPackage>, String> {
        Err(Self::disabled())
    }
    pub fn orphan_names(&self) -> Vec<String> {
        Vec::new()
    }
    pub fn local_versions(&self) -> &HashMap<String, String> {
        &self.local_versions
    }
    pub fn sync_names(&self) -> &HashSet<String> {
        &self.sync_names
    }
    pub fn all_packages(&self) -> Vec<PackageHit> {
        Vec::new()
    }
    pub fn pending_updates(&self) -> Option<usize> {
        None
    }
    pub fn sync_age(&self) -> Option<Duration> {
        None
    }
    pub fn removal_report(&self, _names: &[String]) -> Vec<String> {
        Vec::new()
    }
    pub fn download_total(&self, _names: &[String]) -> Option<u64> {
        None
    }
    fn disabled() -> String {
        String::from("此构建未启用包管理（pacman feature）")
    }
}

/// 与启用 libalpm 时一致的字节数格式化函数。
pub fn size_text(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.2} {}", UNITS[unit])
    }
}

#[cfg(test)]
mod tests {
    use super::Db;

    #[test]
    fn package_database_reports_feature_disabled_instead_of_fake_empty_data() {
        let error = Db::open().expect_err("未启用 pacman 时不能伪装成空数据库");
        assert!(error.contains("此构建未启用包管理"), "{error}");
    }
}
