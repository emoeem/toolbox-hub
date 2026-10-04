//! Repository 服务层 —— TUI 与 CLI 唯一的一处实现。
//!
//! 这条纪律是这个功能能不能维护下去的关键：界面里能做的事和命令行里能做的事
//! 必须**是同一段代码**。所以两边都只调用这里，谁也不许自己拼一遍。
//!
//! 全部操作都是**同步**的（网络、哈希、解包都是阻塞的）。要放进界面时由
//! [crate::repository::worker] 搬到后台线程去跑，而不是在这里偷偷开线程 ——
//! 那样命令行就没法复用了。
//!
//! [crate::repository::worker]: crate::repository::worker

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use crate::repository::{
    cache::{self, CacheState, FetchResult},
    config::{Repositories, RepositoryConfig, Trust},
    index::{Index, PackageMeta},
    install::{self, InstallPlan, InstallReport, Roots, UninstallReport, UpdateCandidate},
    installed::{self, InstalledPackage},
};

/// 搜索范围。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SearchScope {
    /// 已装的 + 可装的。
    #[default]
    All,
    /// 只找还没装的。
    Available,
    /// 只找已装的。
    Installed,
    /// 只找有新版可升的。
    Upgradable,
}

impl SearchScope {
    #[allow(dead_code)] // 范围标签（CLI 与界面共用）
    pub fn label(self) -> &'static str {
        match self {
            SearchScope::All => "全部",
            SearchScope::Available => "可安装",
            SearchScope::Installed => "已安装",
            SearchScope::Upgradable => "可升级",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_lowercase().as_str() {
            "all" | "全部" => Some(SearchScope::All),
            "available" | "可安装" | "未安装" => Some(SearchScope::Available),
            "installed" | "已安装" => Some(SearchScope::Installed),
            "upgradable" | "可升级" | "有更新" => Some(SearchScope::Upgradable),
            _ => None,
        }
    }
}

/// 一条搜索结果。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PackageHit {
    pub id: String,
    pub name: String,
    pub version: String,
    pub summary: String,
    pub repository: String,
    pub repository_name: String,
    pub trust: Trust,
    pub state: CacheState,
    pub installed_version: Option<String>,
    // 升级过后的版本
    pub upgradable: bool,
    /// 索引提供了产物哈希（完整性**可以**被核对）。
    pub has_hash: bool,
    pub tags: Vec<String>,
    pub categories: Vec<String>,
}

impl PackageHit {
    pub fn is_installed(&self) -> bool {
        self.installed_version.is_some()
    }

    /// 状态文案（CLI 与 TUI 共用）。
    pub fn status(&self) -> &'static str {
        match (&self.installed_version, self.upgradable) {
            (Some(_), true) => "↑ 可升级",
            (Some(_), false) => "● 已安装",
            (None, _) => "",
        }
    }
}

/// 仓库列表里的一行。
#[derive(Clone, Debug)]
pub struct RepositoryStatus {
    pub config: RepositoryConfig,
    pub trust: Trust,
    pub state: CacheState,
    pub package_count: usize,
    pub fetched_at: Option<u64>,
}

/// 服务：配置好了目录与仓库清单，之后每个操作都是自包含的。
///
/// Clone 很便宜：只有配置与一个 ureq agent（内部是 Arc）—— 后台线程拿的是副本。
/// 解析缓存（[`ServiceCache`]）也在 Arc 里：副本们共享同一份，谁刷新了磁盘，
/// 谁的文件指纹就变了，另一边下次用的时候自动重解析。
#[derive(Clone)]
pub struct Service {
    pub repositories: Repositories,
    pub roots: Roots,
    pub cache_root: PathBuf,
    /// 读配置时踩到的问题（界面/CLI 决定要不要说一句）。
    pub config_problem: Option<String>,
    agent: ureq::Agent,
    cache: Arc<ServiceCache>,
}

/// 一个仓库在缓存里的样子（[`ServiceCache`] 的条目）。
struct IndexEntry {
    config: RepositoryConfig,
    index: Index,
    state: CacheState,
}

#[derive(Default)]
struct IndexesCache {
    fingerprint: Vec<RepoFingerprint>,
    entries: Vec<IndexEntry>,
}

/// 单个仓库缓存文件的指纹。带 inode：mtime 粒度粗（或被刻意回拨）时，
/// 重写的文件也能被认出来。
#[derive(PartialEq)]
struct RepoFingerprint {
    id: String,
    index_file: Option<FileStamp>,
    meta_file: Option<FileStamp>,
}

type FileStamp = (u128, u64, u64);

#[derive(Default)]
struct VersionsCache {
    fingerprint: Vec<(String, Option<FileStamp>)>,
    versions: HashMap<String, String>,
}

/// 服务内部的解析缓存：索引与账本的**解析结果**留在内存里，按键搜索只做
/// 内存过滤。自校验：每次先用文件指纹（stat，微秒级）确认底层没变过，
/// 变过就重解析 —— 所以刷新 / 安装 / 手工改文件都不需要谁记得来失效。
#[derive(Default)]
struct ServiceCache {
    indexes: Mutex<Option<IndexesCache>>,
    versions: Mutex<Option<VersionsCache>>,
}

/// 仓库 id 不认识时的那句话（带上「你是不是想找 X」）。
///
/// 单独一个函数是因为有三个地方要报同一件事：刷新 / 删除 / 启用停用。
/// 打错字是命令行里最常见的错误，每次都要给同一个像样的答复。
pub(crate) fn unknown_repository(id: &str, repositories: &Repositories) -> String {
    let hint = crate::repository::suggest::did_you_mean(
        id,
        repositories
            .repositories
            .iter()
            .map(|repo| repo.id.as_str()),
    );
    match hint {
        Some(hint) => format!("没有叫「{id}」的仓库 —— {hint}\n（toolbox-hub repo list 看全部）"),
        None => format!("没有叫「{id}」的仓库（toolbox-hub repo list）"),
    }
}

impl Service {
    /// 用当前的配置目录/数据目录/缓存目录装配服务。**不联网**。
    pub fn from_config() -> Self {
        Self::with_dirs(
            crate::config::config_dir(),
            crate::config::data_dir(),
            crate::config::cache_dir(),
            install::bin_dir(),
        )
    }

    /// 显式指定四个目录（给 --config-dir / --data-dir 与测试用）。
    ///
    /// 显式指定是刻意的：测试不该去改进程级的环境变量 —— 那样并行测试会互相踩。
    pub fn with_dirs(
        config_dir: PathBuf,
        data_dir: PathBuf,
        cache_dir: PathBuf,
        bin_dir: PathBuf,
    ) -> Self {
        let loaded = crate::repository::config::load_from(&config_dir.join("repositories.toml"));
        Self {
            repositories: loaded.value,
            roots: Roots {
                bin: bin_dir,
                data: data_dir,
                config: config_dir,
            },
            cache_root: cache_dir.join("repositories"),
            config_problem: loaded.problem,
            agent: cache::agent(),
            cache: Arc::new(ServiceCache::default()),
        }
    }

    /// 给测试用：显式指定目录与仓库清单。
    pub fn with_parts(repositories: Repositories, roots: Roots, cache_root: PathBuf) -> Self {
        Self {
            repositories,
            roots,
            cache_root,
            config_problem: None,
            agent: cache::agent(),
            cache: Arc::new(ServiceCache::default()),
        }
    }

    fn data_dir(&self) -> &Path {
        &self.roots.data
    }

    // ── 解析缓存 ────────────────────────────────────────────────────────────
    //
    // 索引全文解析与账本 TOML 解析都是毫秒级起步的活，绝不能落在按键路径上。
    // 缓存按「底层文件指纹」自校验：每次先用 stat（微秒级）确认文件没变过，
    // 变过才重解析。所以刷新 / 安装 / 手工改文件都不需要谁记得来失效。

    fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
        // 缓存只是优化：哪次 panic 把 Mutex 毒死了，也不该把整个界面卡死。
        mutex.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// 单个文件的指纹：(mtime 纳秒, 字节数, inode)。带 inode 是因为
    /// 原子写是 rename 换文件，新文件的 mtime/size 都可能恰好和旧的相同。
    fn file_stamp(path: &Path) -> Option<FileStamp> {
        use std::os::unix::fs::MetadataExt;
        let metadata = std::fs::metadata(path).ok()?;
        let nanos = metadata
            .modified()
            .ok()
            .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|duration| duration.as_nanos())
            .unwrap_or_default();
        Some((nanos, metadata.len(), metadata.ino()))
    }

    fn indexes_fingerprint(&self) -> Vec<RepoFingerprint> {
        self.repositories
            .enabled()
            .into_iter()
            .map(|config| RepoFingerprint {
                id: config.id.clone(),
                index_file: Self::file_stamp(&cache::index_path(&self.cache_root, &config.id)),
                meta_file: Self::file_stamp(&cache::meta_path(&self.cache_root, &config.id)),
            })
            .collect()
    }

    fn versions_fingerprint(&self) -> Vec<(String, Option<FileStamp>)> {
        let Ok(read_dir) = std::fs::read_dir(installed::packages_dir(self.data_dir())) else {
            return Vec::new();
        };
        let mut out: Vec<_> = read_dir
            .filter_map(|entry| entry.ok())
            .map(|entry| {
                let name = entry.file_name().to_string_lossy().to_string();
                let stamp = Self::file_stamp(&entry.path().join("installed.toml"));
                (name, stamp)
            })
            .collect();
        out.sort_by(|a, b| a.0.cmp(&b.0));
        out
    }

    /// 拿到（必要时重建）已启用仓库的解析索引。守卫在返回的切片后面，
    /// 用完即还，别存起来。
    fn fresh_indexes<'a>(&self, cache: &'a mut Option<IndexesCache>) -> &'a [IndexEntry] {
        let cache = cache.get_or_insert_with(IndexesCache::default);
        let fingerprint = self.indexes_fingerprint();
        if cache.fingerprint != fingerprint {
            cache.fingerprint = fingerprint;
            cache.entries = self.load_indexes();
        }
        &cache.entries
    }

    fn load_indexes(&self) -> Vec<IndexEntry> {
        self.repositories
            .enabled()
            .into_iter()
            .filter_map(|config| {
                let cached = cache::load(&self.cache_root, &config.id)?;
                if !cached.state.usable() {
                    return None;
                }
                Some(IndexEntry {
                    config: config.clone(),
                    index: cached.index,
                    state: cached.state,
                })
            })
            .collect()
    }

    /// 拿到（必要时重建）已装包的版本表。同上，守卫别存。
    fn fresh_versions<'a>(
        &self,
        cache: &'a mut Option<VersionsCache>,
    ) -> &'a HashMap<String, String> {
        let cache = cache.get_or_insert_with(VersionsCache::default);
        let fingerprint = self.versions_fingerprint();
        if cache.fingerprint != fingerprint {
            cache.fingerprint = fingerprint;
            cache.versions = installed::installed_versions(self.data_dir())
                .into_iter()
                .collect();
        }
        &cache.versions
    }

    // ── 仓库 ────────────────────────────────────────────────────────────────

    /// 每个仓库一行状态（只读本地缓存，不联网）。
    pub fn statuses(&self) -> Vec<RepositoryStatus> {
        self.repositories
            .sorted()
            .into_iter()
            .map(|config| {
                let cached = cache::load(&self.cache_root, &config.id);
                let state = match &cached {
                    Some(cached) => cached.state,
                    None => CacheState::Missing,
                };
                RepositoryStatus {
                    trust: config.trust(),
                    state,
                    package_count: cached.as_ref().map_or(0, |c| c.package_count()),
                    fetched_at: cached.as_ref().and_then(|c| c.meta.fetched_at),
                    config: config.clone(),
                }
            })
            .collect()
    }

    /// 刷新一个仓库（阻塞，会联网或读本地索引）。
    pub fn refresh(&self, id: &str) -> Result<FetchResult, String> {
        let Some(found) = self.repositories.find(id) else {
            return Err(unknown_repository(id, &self.repositories));
        };
        let config = found.clone();
        Ok(cache::fetch(&config, &self.cache_root, &self.agent))
    }

    /// 刷新所有**已启用**的仓库。
    ///
    /// 一个仓库失败不影响其它仓库：每个都拿到自己的结果。
    pub fn refresh_all(&self) -> Vec<FetchResult> {
        self.repositories
            .enabled()
            .into_iter()
            .map(|config| cache::fetch(config, &self.cache_root, &self.agent))
            .collect()
    }

    /// 已启用仓库里能用的索引（含缓存）。
    ///
    /// 返回的是**克隆**：只读遍历请用搜索 / find 走的缓存路径，别为看一眼
    /// 把整个索引复制一遍。
    /// 只在测试里用：生产路径走 [`Self::find`] / [`Self::fresh_indexes()`]
    /// （那两处不会把整份索引 clone 出来）。
    #[cfg(test)]
    pub fn indexes(&self) -> Vec<(RepositoryConfig, Index, CacheState)> {
        let mut guard = Self::lock(&self.cache.indexes);
        self.fresh_indexes(&mut guard)
            .iter()
            .map(|entry| (entry.config.clone(), entry.index.clone(), entry.state))
            .collect()
    }

    /// 有没有任何一个已启用仓库带着可用的索引（比 `indexes().is_empty()` 便宜）。
    pub fn has_indexes(&self) -> bool {
        let mut guard = Self::lock(&self.cache.indexes);
        !self.fresh_indexes(&mut guard).is_empty()
    }

    /// 找一个包：按仓库优先级取第一个命中的（和安装顺序一致）。
    pub fn find(&self, id: &str) -> Option<(RepositoryConfig, PackageMeta, CacheState)> {
        let mut guard = Self::lock(&self.cache.indexes);
        for entry in self.fresh_indexes(&mut guard) {
            if let Some(meta) = entry.index.find(id) {
                return Some((entry.config.clone(), meta.clone(), entry.state));
            }
        }
        None
    }

    // ── 搜索 ────────────────────────────────────────────────────────────────

    /// 在已启用仓库的索引里搜索。
    ///
    /// 索引来自**本地缓存**：输入一个字母就联网是绝对不能接受的。
    /// 解析结果留在内存里（见 [`ServiceCache`]），这里只做内存过滤。
    pub fn search(&self, query: &str, scope: SearchScope) -> Vec<PackageHit> {
        let needle = query.trim().to_lowercase();
        let mut indexes_guard = Self::lock(&self.cache.indexes);
        let entries = self.fresh_indexes(&mut indexes_guard);
        let mut versions_guard = Self::lock(&self.cache.versions);
        let installed = self.fresh_versions(&mut versions_guard);
        let mut hits: Vec<PackageHit> = Vec::new();
        let mut seen: HashSet<&str> = HashSet::new();

        for entry in entries {
            let (config, index, state) = (&entry.config, &entry.index, entry.state);
            for meta in &index.packages {
                if !meta.matches(&needle) {
                    continue;
                }
                // 同一个包在多个仓库里出现时，优先级高的仓库先被看到 ——
                // 后来的同名包直接跳过，避免列表里出现两条一样的。
                if !seen.insert(meta.id.as_str()) {
                    continue;
                }
                let installed_version = installed.get(&meta.id).cloned();
                let upgradable = installed_version
                    .as_deref()
                    .map(|current| crate::repository::version::is_upgrade(&meta.version, current))
                    .unwrap_or(false);

                let hit = PackageHit {
                    id: meta.id.clone(),
                    name: meta.name.clone(),
                    version: meta.version.clone(),
                    summary: meta.summary_or_description().to_string(),
                    repository: config.id.clone(),
                    repository_name: config.name.clone(),
                    trust: config.trust(),
                    state,
                    installed_version,
                    upgradable,
                    has_hash: meta
                        .artifact
                        .as_ref()
                        .is_some_and(|artifact| artifact.sha256().is_some()),
                    tags: meta.tags.clone(),
                    categories: meta.categories.clone(),
                };
                if scope.matches(&hit) {
                    hits.push(hit);
                }
            }
        }

        hits.sort_by(|a, b| {
            // 已装的排前面（用户更可能是在找它），再按相关度，再按名字。
            b.is_installed()
                .cmp(&a.is_installed())
                .then_with(|| rank(&b.id, &b.name, &needle).cmp(&rank(&a.id, &a.name, &needle)))
                .then_with(|| a.name.cmp(&b.name))
        });
        hits
    }

    /// 已安装的包。
    pub fn installed(&self) -> Vec<InstalledPackage> {
        installed::load_all(self.data_dir()).0
    }

    // ── 安装 / 卸载 / 更新 ──────────────────────────────────────────────────

    /// 为一个包造安装计划。
    pub fn plan(&self, id: &str) -> Result<InstallPlan, String> {
        if let Some((config, meta, _)) = self.find(id) {
            return install::plan(&meta, &config, &self.roots);
        }
        let mut guard = Self::lock(&self.cache.indexes);
        let hint = crate::repository::suggest::did_you_mean(
            id,
            self.fresh_indexes(&mut guard)
                .iter()
                .flat_map(|entry| entry.index.packages.iter())
                .map(|package| package.id.as_str()),
        );
        drop(guard);
        Err(match hint {
            Some(hint) => {
                format!("在已启用的仓库里找不到「{id}」—— {hint}（没取过索引先 repo update）")
            }
            None => format!("在已启用的仓库里找不到「{id}」（先 repo update）"),
        })
    }

    /// 执行一个已经确认过的计划。
    pub fn install(
        &self,
        plan: &InstallPlan,
        allow_unverified: bool,
    ) -> Result<InstallReport, String> {
        install::install(plan, &self.roots, &self.agent, allow_unverified)
    }

    /// 卸载。
    pub fn uninstall(&self, id: &str, purge_modified: bool) -> Result<UninstallReport, String> {
        install::uninstall(self.data_dir(), id, purge_modified)
    }

    /// 算可升级的包。
    pub fn update_candidates(&self) -> (Vec<UpdateCandidate>, Vec<String>) {
        let mut guard = Self::lock(&self.cache.indexes);
        let indexes: Vec<(RepositoryConfig, Index)> = self
            .fresh_indexes(&mut guard)
            .iter()
            .map(|entry| (entry.config.clone(), entry.index.clone()))
            .collect();
        drop(guard);
        install::update_candidates(self.data_dir(), &self.roots, &indexes)
    }

    /// 数「有几个包有新版」。不造计划、不哈希文件 —— 给启动时的可升级徽标用。
    pub fn update_count(&self) -> usize {
        let mut guard = Self::lock(&self.cache.indexes);
        let indexes: Vec<&Index> = self
            .fresh_indexes(&mut guard)
            .iter()
            .map(|entry| &entry.index)
            .collect();
        // 借用着缓存里的索引，守卫得活到调用结束。
        install::update_count(self.data_dir(), &indexes)
    }

    /// 装一个包（CLI 的一步到位路径：计划 → 检查 → 安装）。
    pub fn install_by_id(&self, id: &str, allow_unverified: bool) -> Result<InstallReport, String> {
        let plan = self.plan(id)?;
        plan.check(&self.roots, allow_unverified)?;
        self.install(&plan, allow_unverified)
    }
}

/// 搜索相关度：id 完全命中 > id 前缀 > 名字命中 > 其它。
fn rank(id: &str, name: &str, needle: &str) -> i32 {
    if needle.is_empty() {
        return 0;
    }
    let id = id.to_lowercase();
    let name = name.to_lowercase();
    if id == needle {
        100
    } else if id.starts_with(needle) {
        80
    } else if id.contains(needle) {
        60
    } else if name.contains(needle) {
        40
    } else {
        20
    }
}

impl SearchScope {
    /// 这一条命中不在这个范围里？
    pub fn matches(self, hit: &PackageHit) -> bool {
        match self {
            SearchScope::All => true,
            SearchScope::Available => !hit.is_installed(),
            SearchScope::Installed => hit.is_installed(),
            SearchScope::Upgradable => hit.upgradable,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repository::{
        cache::IndexMeta,
        config::Repositories,
        installed::{InstalledFile, InstalledPackage, SCHEMA_VERSION},
    };

    fn temp(tag: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("toolbox-hub-service-{tag}-{nanos}"));
        std::fs::create_dir_all(&dir).expect("mkdir");
        dir
    }

    const INDEX: &str = r#"{"schema_version": 1, "packages": [
        {"id": "hello-tool", "name": "Hello Tool", "version": "1.0.0",
         "summary": "打招呼", "tags": ["demo"], "categories": ["tools"],
         "artifact": {"url": "artifacts/h.sh", "sha256": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"},
         "files": [{"path": "scripts/hello-tool", "kind": "bin"}]},
        {"id": "video-tools", "name": "视频工具", "version": "2.0.0",
         "summary": "压缩视频", "tags": ["ffmpeg"],
         "files": [{"path": "manifests/v.toml", "kind": "manifest"}]}
    ]}"#;

    /// 造一个服务：一个本地仓库（索引已写进缓存），一个数据目录。
    fn service(base: &Path, index_text: &str) -> Service {
        let cache_root = base.join("cache");
        let repo_id = "official";
        let meta = IndexMeta {
            fetched_at: Some(cache::now_secs()),
            content_at: Some(cache::now_secs()),
            etag: None,
            last_modified: None,
            name: Some(String::from("ToolHub Official")),
        };
        cache::store(&cache_root, repo_id, index_text, &meta).expect("store");

        let repositories = Repositories {
            repositories: vec![RepositoryConfig {
                id: repo_id.to_string(),
                name: String::from("ToolHub Official"),
                index: base.join("index.json").display().to_string(),
                enabled: true,
                priority: 0,
                trust: Some(String::from("trusted")),
                note: None,
            }],
        };
        let roots = Roots {
            bin: base.join("bin"),
            data: base.join("data"),
            config: base.join("config"),
        };
        Service::with_parts(repositories, roots, cache_root)
    }

    fn mark_installed(base: &Path, id: &str, version: &str) {
        let data = base.join("data");
        let package = InstalledPackage {
            schema_version: SCHEMA_VERSION,
            id: id.to_string(),
            name: id.to_string(),
            version: version.to_string(),
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
            files: Vec::<InstalledFile>::new(),
            allow_unverified: false,
        };
        installed::save(&data, &package).expect("save");
    }

    #[test]
    fn search_covers_id_name_summary_and_tags() {
        let base = temp("search");
        let service = service(&base, INDEX);
        assert_eq!(service.search("", SearchScope::All).len(), 2);
        assert_eq!(service.search("hello", SearchScope::All).len(), 1);
        assert_eq!(service.search("打招呼", SearchScope::All).len(), 1);
        assert_eq!(service.search("ffmpeg", SearchScope::All).len(), 1);
        assert!(service.search("不存在的东西", SearchScope::All).is_empty());
        std::fs::remove_dir_all(&base).expect("cleanup");
    }

    /// 同一份索引里的重复 id 只出现一次（仓库优先级第一份胜出）。
    #[test]
    fn a_duplicate_across_repositories_appears_once() {
        let base = temp("dupe");
        let service = service(&base, INDEX);
        // 再存一个同名仓库
        let meta = IndexMeta {
            fetched_at: Some(cache::now_secs()),
            ..IndexMeta::default()
        };
        cache::store(&service.cache_root, "second", INDEX, &meta).expect("store");
        let mut repositories = service.repositories.clone();
        repositories.repositories.push(RepositoryConfig {
            id: String::from("second"),
            name: String::from("Second"),
            index: base.join("index.json").display().to_string(),
            enabled: true,
            priority: 10,
            trust: None,
            note: None,
        });
        let service = Service::with_parts(
            repositories,
            service.roots.clone(),
            service.cache_root.clone(),
        );
        let hits = service.search("", SearchScope::All);
        assert_eq!(hits.len(), 2, "重复包不该出现两条：{hits:?}");
        std::fs::remove_dir_all(&base).expect("cleanup");
    }

    #[test]
    fn scope_filters_split_installed_available_and_upgradable() {
        let base = temp("scope");
        let service = service(&base, INDEX);
        mark_installed(&base, "hello-tool", "0.9.0");

        let all = service.search("", SearchScope::All);
        assert_eq!(all.len(), 2);
        assert_eq!(service.search("", SearchScope::Available).len(), 1);
        let installed_hits = service.search("", SearchScope::Installed);
        assert_eq!(installed_hits.len(), 1);
        assert_eq!(installed_hits[0].id, "hello-tool");
        assert!(installed_hits[0].upgradable, "0.9.0 → 1.0.0 是升级");
        assert_eq!(installed_hits[0].status(), "↑ 可升级");

        assert_eq!(service.search("", SearchScope::Upgradable).len(), 1);
        std::fs::remove_dir_all(&base).expect("cleanup");
    }

    #[test]
    fn an_up_to_date_package_is_not_upgradable() {
        let base = temp("uptodate");
        let service = service(&base, INDEX);
        mark_installed(&base, "hello-tool", "1.0.0");
        let hits = service.search("hello", SearchScope::All);
        assert!(!hits[0].upgradable);
        assert_eq!(hits[0].status(), "● 已安装");
        std::fs::remove_dir_all(&base).expect("cleanup");
    }

    #[test]
    fn statuses_report_each_repository_without_networking() {
        let base = temp("status");
        let service = service(&base, INDEX);
        let statuses = service.statuses();
        assert_eq!(statuses.len(), 1);
        assert_eq!(statuses[0].config.id, "official");
        assert_eq!(statuses[0].trust, Trust::Trusted);
        assert_eq!(statuses[0].state, CacheState::Fresh);
        assert_eq!(statuses[0].package_count, 2);
        assert!(statuses[0].fetched_at.is_some());
        std::fs::remove_dir_all(&base).expect("cleanup");
    }

    #[test]
    fn a_repository_without_a_cache_is_marked_missing_and_contributes_nothing() {
        let base = temp("nocache");
        let repositories = Repositories {
            repositories: vec![RepositoryConfig {
                id: String::from("fresh"),
                name: String::from("Fresh"),
                index: base.join("nope.json").display().to_string(),
                enabled: true,
                priority: 0,
                trust: None,
                note: None,
            }],
        };
        let roots = Roots {
            bin: base.join("bin"),
            data: base.join("data"),
            config: base.join("config"),
        };
        let service = Service::with_parts(repositories, roots, base.join("cache"));
        assert_eq!(service.statuses()[0].state, CacheState::Missing);
        assert!(service.search("", SearchScope::All).is_empty());
        assert!(service.indexes().is_empty());
        assert!(service.find("hello-tool").is_none());
        std::fs::remove_dir_all(&base).expect("cleanup");
    }

    /// 禁用的仓库不参与搜索。
    #[test]
    fn a_disabled_repository_is_ignored() {
        let base = temp("disabled");
        let mut service = service(&base, INDEX);
        service.repositories.repositories[0].enabled = false;
        assert!(service.search("", SearchScope::All).is_empty());
        assert!(service.indexes().is_empty());
        // 但列表里还是要看得见它
        assert_eq!(service.statuses().len(), 1);
        std::fs::remove_dir_all(&base).expect("cleanup");
    }

    #[test]
    fn finding_a_package_reports_which_repository_it_came_from() {
        let base = temp("find");
        let service = service(&base, INDEX);
        let (config, meta, state) = service.find("hello-tool").expect("应能找到");
        assert_eq!(config.id, "official");
        assert_eq!(config.trust(), Trust::Trusted);
        assert_eq!(meta.version, "1.0.0");
        assert_eq!(state, CacheState::Fresh);
        assert!(service.find("nope").is_none());
        std::fs::remove_dir_all(&base).expect("cleanup");
    }

    #[test]
    fn planning_an_unknown_package_says_what_to_do() {
        let base = temp("planmissing");
        let service = service(&base, INDEX);
        let problem = service.plan("nope").unwrap_err();
        assert!(problem.contains("找不到"), "{problem}");
        assert!(problem.contains("repo update"), "{problem}");
        std::fs::remove_dir_all(&base).expect("cleanup");
    }

    #[test]
    fn refreshing_a_local_repository_works_and_an_unknown_one_complains() {
        let base = temp("refresh");
        let index_file = base.join("index.json");
        std::fs::write(&index_file, INDEX).expect("write");
        let repositories = Repositories {
            repositories: vec![RepositoryConfig {
                id: String::from("local"),
                name: String::from("Local"),
                index: index_file.display().to_string(),
                enabled: true,
                priority: 0,
                trust: None,
                note: None,
            }],
        };
        let roots = Roots {
            bin: base.join("bin"),
            data: base.join("data"),
            config: base.join("config"),
        };
        let service = Service::with_parts(repositories, roots, base.join("cache"));

        let result = service.refresh("local").expect("应能刷新");
        assert_eq!(result.state, CacheState::Fresh);
        assert_eq!(result.package_count(), 2);
        // 刷新之后服务就能看见它了
        assert_eq!(service.search("", SearchScope::All).len(), 2);

        assert!(service.refresh("没有这个").is_err());
        std::fs::remove_dir_all(&base).expect("cleanup");
    }

    #[test]
    fn scope_labels_and_parsing_round_trip() {
        for scope in [
            SearchScope::All,
            SearchScope::Available,
            SearchScope::Installed,
            SearchScope::Upgradable,
        ] {
            assert_eq!(SearchScope::parse(scope.label()), Some(scope));
            assert!(!scope.label().is_empty());
        }
        assert_eq!(SearchScope::default(), SearchScope::All);
        assert_eq!(SearchScope::parse("乱写"), None);
    }

    #[test]
    fn relevance_prefers_an_exact_id_over_a_loose_match() {
        assert!(rank("hello", "x", "hello") > rank("hello-tool", "x", "hello"));
        assert!(rank("hello-tool", "x", "hello") > rank("x", "hello there", "hello"));
        assert_eq!(rank("a", "b", ""), 0);
    }
}
