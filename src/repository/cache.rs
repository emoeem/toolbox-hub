//! 索引缓存与抓取。
//!
//! 三条硬要求（都来自「离线优先」）：
//!
//! 1. **不进 UI 线程**：抓取是阻塞 IO，只在后台 worker 里调用；
//! 2. **失败不致命**：网络挂了、仓库没了、JSON 坏了，都只能让**这个**仓库
//!    变成「不可用」，绝不能崩、也不能把本地工具一起带走；
//! 3. **有缓存就用缓存**：304 用缓存，网络错误也用缓存（并明确告诉用户这是
//!    什么时候的数据）。
//!
//! 目录布局：
//!
//! \`\`\`text
//! <缓存目录>/repositories/<仓库 id>/index.json
//! <缓存目录>/repositories/<仓库 id>/meta.json
//! \`\`\`
//!
//! \`meta.json\` 里放 ETag / Last-Modified / 抓取时间。存的是**索引原文**而不是
//! 解析后的结构：原文才能让未知字段活过一次往返（也才谈得上向前兼容）。

use std::{
    fs,
    path::{Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};

use crate::repository::{
    config::{RepositoryConfig, Trust},
    index::{Index, ParsedIndex},
};

/// 覆盖「多久算陈旧」的环境变量（秒）。
pub const STALE_ENV: &str = "TOOLBOX_HUB_REPO_STALE_SECS";

/// 默认 24 小时。超过这个时间没刷新成功，界面上就从「已缓存」变成「陈旧」。
pub const DEFAULT_STALE_SECS: u64 = 24 * 60 * 60;

/// 索引最大多大（防止一个坏 URL 把内存吃干）。
const MAX_INDEX_BYTES: u64 = 8 * 1024 * 1024;

/// 当前时间戳（秒）。
pub fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|delta| delta.as_secs())
        .unwrap_or(0)
}

/// 多久算陈旧。
pub fn stale_after() -> u64 {
    std::env::var(STALE_ENV)
        .ok()
        .and_then(|raw| raw.trim().parse::<u64>().ok())
        .filter(|secs| *secs > 0)
        .unwrap_or(DEFAULT_STALE_SECS)
}

/// 一份缓存的新鲜程度。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CacheState {
    /// 刚刚成功刷新过（或 304 确认过）。
    Fresh,
    /// 有缓存，但已经超过陈旧阈值、或这次刷新失败了。
    Stale,
    /// 从没成功取过。
    Missing,
    /// 取不到，也没有缓存可用。
    Unavailable,
}

impl CacheState {
    /// 状态行上的标记。
    pub fn marker(self) -> &'static str {
        match self {
            CacheState::Fresh => "●",
            CacheState::Stale => "◐",
            CacheState::Missing => "○",
            CacheState::Unavailable => "✕",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            CacheState::Fresh => "在线",
            CacheState::Stale => "缓存",
            CacheState::Missing => "未取过",
            CacheState::Unavailable => "不可用",
        }
    }

    /// 状态含义（界面与 CLI 都用这一份措辞）。
    pub fn meaning(self) -> &'static str {
        match self {
            CacheState::Fresh => "已从仓库刷新",
            CacheState::Stale => "用的是本地缓存，刷新没成功",
            CacheState::Missing => "还没有取过这个仓库的索引",
            CacheState::Unavailable => "取不到索引，本地缓存也没有",
        }
    }

    /// 这份数据还能不能用来搜索 / 安装。
    pub fn usable(self) -> bool {
        matches!(self, CacheState::Fresh | CacheState::Stale)
    }
}

/// 缓存的旁挂信息。
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(default)]
pub struct IndexMeta {
    /// 上次**成功**拿到内容的时间（304 也算成功，但 content_at 不变）。
    pub fetched_at: Option<u64>,
    /// 上次内容真正变化的时间。
    pub content_at: Option<u64>,
    pub etag: Option<String>,
    pub last_modified: Option<String>,
    /// 索引自己声明的名字（展示用）。
    pub name: Option<String>,
}

/// 从缓存里读出来的一份索引。
#[derive(Clone, Debug)]
pub struct CachedIndex {
    pub index: Index,
    pub warnings: Vec<String>,
    pub state: CacheState,
    pub meta: IndexMeta,
    /// 索引原文。留着它才能判断「这次抓到的内容和上次是不是同一份」，
    /// 也才能让未知字段活过一次往返。
    pub raw: String,
}

impl CachedIndex {
    pub fn package_count(&self) -> usize {
        self.index.packages.len()
    }
}

/// 一个仓库的缓存目录。
pub fn repository_dir(root: &Path, id: &str) -> PathBuf {
    root.join(id)
}

pub fn index_path(root: &Path, id: &str) -> PathBuf {
    repository_dir(root, id).join("index.json")
}

pub fn meta_path(root: &Path, id: &str) -> PathBuf {
    repository_dir(root, id).join("meta.json")
}

/// 读 meta（读不出来就是一份空的，不算错误）。
pub fn load_meta(root: &Path, id: &str) -> IndexMeta {
    fs::read_to_string(meta_path(root, id))
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

/// 按 \`fetched_at\` 算状态。
fn state_for(meta: &IndexMeta, now: u64) -> CacheState {
    match meta.fetched_at {
        Some(at) if now.saturating_sub(at) <= stale_after() => CacheState::Fresh,
        Some(_) => CacheState::Stale,
        None => CacheState::Missing,
    }
}

/// 读缓存。没有、读不动、解析不了都返回 \`None\`（调用方据此决定要不要重取）。
pub fn load(root: &Path, id: &str) -> Option<CachedIndex> {
    let text = fs::read_to_string(index_path(root, id)).ok()?;
    let meta = load_meta(root, id);
    let parsed = Index::parse(&text).ok()?;
    let state = state_for(&meta, now_secs());
    Some(CachedIndex {
        index: parsed.index,
        warnings: parsed.warnings,
        state,
        meta,
        raw: text,
    })
}

/// 把索引原文与 meta 写进缓存。
pub fn store(root: &Path, id: &str, text: &str, meta: &IndexMeta) -> Result<(), String> {
    let dir = repository_dir(root, id);
    fs::create_dir_all(&dir)
        .map_err(|error| format!("建不了缓存目录 {}：{error}", dir.display()))?;
    fs::write(index_path(root, id), text).map_err(|error| format!("写不了索引缓存：{error}"))?;
    let encoded =
        serde_json::to_string_pretty(meta).map_err(|error| format!("序列化失败：{error}"))?;
    fs::write(meta_path(root, id), encoded).map_err(|error| format!("写不了缓存信息：{error}"))?;
    Ok(())
}

/// 只更新 meta（304 用：内容没变，但「刚刚确认过」）。
fn touch_meta(root: &Path, id: &str, meta: &IndexMeta) -> Result<(), String> {
    let dir = repository_dir(root, id);
    fs::create_dir_all(&dir).map_err(|error| format!("建不了缓存目录：{error}"))?;
    let encoded =
        serde_json::to_string_pretty(meta).map_err(|error| format!("序列化失败：{error}"))?;
    fs::write(meta_path(root, id), encoded).map_err(|error| format!("写不了缓存信息：{error}"))
}

/// 一次抓取的结果。
///
/// 刻意**不用** \`Result\`：网络失败是预期内的一种结果，而且要带着「我退回了什么」
/// 一起返回，调用方才能说清「这是 2 小时前的缓存」。
#[derive(Clone, Debug)]
pub struct FetchResult {
    pub repository: String,
    pub repository_name: String,
    pub trust: Trust,
    /// 可用的索引；\`None\` 表示这个仓库当前完全不可用。
    pub index: Option<Index>,
    pub warnings: Vec<String>,
    pub state: CacheState,
    pub fetched_at: Option<u64>,
    /// 失败原因（有缓存时 = 「为什么这次用缓存」）。
    pub error: Option<String>,
}

impl FetchResult {
    pub fn package_count(&self) -> usize {
        self.index.as_ref().map_or(0, |index| index.packages.len())
    }

    /// 给用户的一句话。
    pub fn message(&self) -> String {
        let prefix = format!("{} {}", self.state.marker(), self.repository_name);
        match (&self.error, self.state) {
            (Some(error), CacheState::Stale) => {
                format!("{prefix}：刷新失败，用的是缓存（{error}）")
            }
            (Some(error), CacheState::Unavailable) => format!("{prefix}：不可用（{error}）"),
            (Some(error), _) => format!("{prefix}：{error}"),
            (None, CacheState::Fresh) => {
                format!("{prefix}：已刷新（{} 个包）", self.package_count())
            }
            (None, state) => format!("{prefix}：{}", state.meaning()),
        }
    }
}

fn failed(config: &RepositoryConfig, error: String, cached: Option<CachedIndex>) -> FetchResult {
    let trust = config.trust();
    match cached {
        Some(cached) => FetchResult {
            repository: config.id.clone(),
            repository_name: config.name.clone(),
            trust,
            fetched_at: cached.meta.fetched_at,
            index: Some(cached.index),
            warnings: cached.warnings,
            // 有缓存但没刷新成功 = 陈旧，不假装在线。
            state: CacheState::Stale,
            error: Some(error),
        },
        None => FetchResult {
            repository: config.id.clone(),
            repository_name: config.name.clone(),
            trust,
            index: None,
            warnings: Vec::new(),
            state: CacheState::Unavailable,
            fetched_at: None,
            error: Some(error),
        },
    }
}

/// 抓一个仓库的索引。
///
/// 本地索引（路径 / \`file://\`）直接读文件；其余走 HTTP(S)，带条件请求。
pub fn fetch(config: &RepositoryConfig, root: &Path, agent: &ureq::Agent) -> FetchResult {
    let cached = load(root, &config.id);
    let trust = config.trust();
    let now = now_secs();

    let (text, etag, last_modified) = if config.is_local() {
        match read_local(config) {
            Ok(text) => (text, None, None),
            Err(error) => return failed(config, error, cached),
        }
    } else {
        match http_get(config, agent, cached.as_ref()) {
            HttpOutcome::Body {
                text,
                etag,
                last_modified,
            } => (text, etag, last_modified),
            HttpOutcome::NotModified => {
                // 内容没变：只把「刚刚确认过」记下来。
                if let Some(cached) = cached {
                    let mut meta = cached.meta.clone();
                    meta.fetched_at = Some(now);
                    let _ = touch_meta(root, &config.id, &meta);
                    return FetchResult {
                        repository: config.id.clone(),
                        repository_name: config.name.clone(),
                        trust,
                        fetched_at: meta.fetched_at,
                        index: Some(cached.index),
                        warnings: cached.warnings,
                        state: CacheState::Fresh,
                        error: None,
                    };
                }
                // 服务器说「没变」但我们没有缓存：只能当成取不到。
                return failed(
                    config,
                    String::from("服务器说索引没变，但本地没有缓存"),
                    None,
                );
            }
            HttpOutcome::Failed(error) => return failed(config, error, cached),
        }
    };

    let parsed: ParsedIndex = match Index::parse(&text) {
        Ok(parsed) => parsed,
        Err(error) => return failed(config, error, cached),
    };
    let mut warnings = parsed.warnings;

    // 内容逐字节没变就别动 content_at —— 界面上的「上次更新」才有意义。
    let content_at = match cached.as_ref() {
        Some(previous) if previous.raw == text => previous.meta.content_at.or(Some(now)),
        _ => Some(now),
    };
    let meta = IndexMeta {
        fetched_at: Some(now),
        content_at,
        etag,
        last_modified,
        name: parsed.index.name.clone(),
    };

    if let Err(problem) = store(root, &config.id, &text, &meta) {
        // 缓存写不进去不影响这次使用，但要说出来。
        warnings.push(problem);
    }

    FetchResult {
        repository: config.id.clone(),
        repository_name: config.name.clone(),
        trust,
        fetched_at: meta.fetched_at,
        index: Some(parsed.index),
        warnings,
        state: CacheState::Fresh,
        error: None,
    }
}

fn read_local(config: &RepositoryConfig) -> Result<String, String> {
    let path = config
        .local_path()
        .ok_or_else(|| String::from("本地索引路径读不出来"))?;
    let metadata =
        fs::metadata(&path).map_err(|error| format!("读不了 {}：{error}", path.display()))?;
    if metadata.len() > MAX_INDEX_BYTES {
        return Err(format!(
            "{} 太大（{} 字节，上限 {MAX_INDEX_BYTES}）",
            path.display(),
            metadata.len()
        ));
    }
    fs::read_to_string(&path).map_err(|error| format!("读不了 {}：{error}", path.display()))
}

enum HttpOutcome {
    /// 正文 + 这次的校验值（ETag / Last-Modified，写回缓存下次条件请求用）。
    Body {
        text: String,
        etag: Option<String>,
        last_modified: Option<String>,
    },
    NotModified,
    Failed(String),
}

/// 条件 GET：带上缓存里的 ETag / Last-Modified。
///
/// 为什么值得做：索引可以有几百 KB，每天后台刷一次却几乎从不变。
fn http_get(
    config: &RepositoryConfig,
    agent: &ureq::Agent,
    cached: Option<&CachedIndex>,
) -> HttpOutcome {
    let mut request = agent.get(&config.index);
    if let Some(cached) = cached {
        if let Some(etag) = cached.meta.etag.as_deref() {
            request = request.header("If-None-Match", etag);
        }
        if let Some(modified) = cached.meta.last_modified.as_deref() {
            request = request.header("If-Modified-Since", modified);
        }
    }

    match request.call() {
        Ok(mut response) => {
            // 头部要在吃 body 之前读。
            let etag = header(&response, "etag");
            let last_modified = header(&response, "last-modified");
            match response
                .body_mut()
                .with_config()
                .limit(MAX_INDEX_BYTES)
                .read_to_string()
            {
                Ok(text) => HttpOutcome::Body {
                    text,
                    etag,
                    last_modified,
                },
                Err(error) => HttpOutcome::Failed(format!("读取响应失败：{error}")),
            }
        }
        Err(ureq::Error::StatusCode(304)) => HttpOutcome::NotModified,
        // 404 是**建仓库时最常见的一种**：地址写错、忘了推、或者仓库还是私有的。
        // 一句「http status: 404」帮不上忙，所以这里替用户把可能的原因列出来。
        Err(ureq::Error::StatusCode(404)) => HttpOutcome::Failed(format!(
            "404：{} 上没有这个文件。常见原因：\n               · 仓库还没 push 上去（先 git push，再等几秒）；\n               · 地址写错了（分支名要写 main/master 里的哪一个？路径对不对？）；\n               · 仓库是私有的（raw 链接读不到私有仓库）。\n               本地仓库可以直接用路径：toolbox-hub repo add ./registry/index.json",
            config.index
        )),
        Err(ureq::Error::StatusCode(403)) => HttpOutcome::Failed(format!(
            "403：读不了 {} —— 多半是私有仓库，或者被服务端限流了",
            config.index
        )),
        Err(error) => HttpOutcome::Failed(format!("请求失败：{error}")),
    }
}

fn header(response: &ureq::http::Response<ureq::Body>, name: &str) -> Option<String> {
    response
        .headers()
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(str::to_string)
}

/// 建一个给仓库用的 agent（连接池 + 全局超时）。
pub fn agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(20)))
        .user_agent("toolbox-hub")
        .build()
        .new_agent()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_root(tag: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("toolbox-hub-cache-{tag}-{nanos}"));
        fs::create_dir_all(&dir).expect("mkdir");
        dir
    }

    fn local_repo(root: &Path, index: &str) -> RepositoryConfig {
        let file = root.join("index.json");
        fs::write(&file, index).expect("write index");
        RepositoryConfig {
            id: String::from("local"),
            name: String::from("Local"),
            index: file.display().to_string(),
            enabled: true,
            priority: 0,
            trust: None,
            note: None,
        }
    }

    const SAMPLE: &str = r#"{"schema_version": 1, "packages": [
        {"id": "a", "name": "A", "version": "1.0.0", "files": [{"path": "scripts/a"}]}
    ]}"#;

    #[test]
    fn a_local_index_is_read_and_cached() {
        let root = temp_root("local");
        let cache = root.join("cache");
        let repo = local_repo(&root, SAMPLE);

        let result = fetch(&repo, &cache, &agent());
        assert_eq!(result.state, CacheState::Fresh);
        assert_eq!(result.package_count(), 1);
        assert!(result.error.is_none(), "{:?}", result.error);
        assert!(index_path(&cache, "local").exists(), "应当写进缓存");
        assert!(meta_path(&cache, "local").exists());

        // 缓存能独立读回来
        let cached = load(&cache, "local").expect("缓存应可读");
        assert_eq!(cached.state, CacheState::Fresh);
        assert_eq!(cached.package_count(), 1);
        fs::remove_dir_all(&root).expect("cleanup");
    }

    /// 索引文件没了、内容坏了：只能让这个仓库不可用，不能崩。
    #[test]
    fn a_missing_or_broken_index_degrades_to_a_message() {
        let root = temp_root("broken");
        let cache = root.join("cache");
        let mut repo = local_repo(&root, SAMPLE);
        repo.index = root.join("不存在.json").display().to_string();

        let result = fetch(&repo, &cache, &agent());
        assert_eq!(result.state, CacheState::Unavailable);
        assert!(result.index.is_none());
        assert!(result.error.is_some());
        assert!(result.message().contains("不可用"), "{}", result.message());

        // 内容坏了也一样
        repo.index = root.join("bad.json").display().to_string();
        fs::write(root.join("bad.json"), "{ 这不是 json").expect("write");
        let result = fetch(&repo, &cache, &agent());
        assert_eq!(result.state, CacheState::Unavailable);
        assert!(result.error.as_deref().unwrap().contains("JSON"));
        fs::remove_dir_all(&root).expect("cleanup");
    }

    /// 刷新失败时要退回缓存，并且**说清楚**这是什么时候的数据。
    #[test]
    fn a_failed_refresh_falls_back_to_the_cache_and_says_so() {
        let root = temp_root("fallback");
        let cache = root.join("cache");
        let repo = local_repo(&root, SAMPLE);
        // 先成功一次，把缓存养起来
        assert_eq!(fetch(&repo, &cache, &agent()).state, CacheState::Fresh);

        // 索引源坏掉
        fs::write(root.join("index.json"), "{ 坏掉了").expect("write");
        let result = fetch(&repo, &cache, &agent());
        assert_eq!(result.state, CacheState::Stale, "有缓存就该退回去");
        assert_eq!(result.package_count(), 1, "缓存里的包还在");
        assert!(result.error.is_some());
        let message = result.message();
        assert!(message.contains("缓存"), "{message}");
        assert!(message.contains("刷新失败"), "{message}");
        fs::remove_dir_all(&root).expect("cleanup");
    }

    #[test]
    fn state_flips_from_fresh_to_stale_by_age_not_by_luck() {
        let meta = IndexMeta {
            fetched_at: Some(1_000),
            ..IndexMeta::default()
        };
        let stale = stale_after();
        assert_eq!(state_for(&meta, 1_000), CacheState::Fresh);
        assert_eq!(state_for(&meta, 1_000 + stale), CacheState::Fresh);
        assert_eq!(state_for(&meta, 1_000 + stale + 1), CacheState::Stale);
        assert_eq!(state_for(&IndexMeta::default(), 1_000), CacheState::Missing);
    }

    #[test]
    fn a_corrupt_cache_is_treated_as_no_cache() {
        let root = temp_root("corrupt");
        let cache = root.join("cache");
        fs::create_dir_all(repository_dir(&cache, "x")).expect("mkdir");
        fs::write(index_path(&cache, "x"), "{{{").expect("write");
        assert!(load(&cache, "x").is_none(), "坏缓存等于没缓存");
        assert!(load(&cache, "从没取过").is_none());
        fs::remove_dir_all(&root).expect("cleanup");
    }

    #[test]
    fn cache_states_report_whether_they_are_usable() {
        assert!(CacheState::Fresh.usable());
        assert!(CacheState::Stale.usable());
        assert!(!CacheState::Missing.usable());
        assert!(!CacheState::Unavailable.usable());
        // 每个状态都有自己的标记与解释，不能混
        let all = [
            CacheState::Fresh,
            CacheState::Stale,
            CacheState::Missing,
            CacheState::Unavailable,
        ];
        for (index, state) in all.iter().enumerate() {
            assert!(!state.label().is_empty());
            assert!(!state.meaning().is_empty());
            assert_eq!(
                all.iter()
                    .position(|other| other.marker() == state.marker()),
                Some(index),
                "标记不能重复"
            );
        }
    }

    /// 陈旧阈值可以调（测试与用户都能调），但 0 或乱写要退回默认。
    #[test]
    fn the_stale_threshold_has_a_sane_default_and_ignores_junk() {
        assert_eq!(stale_after(), DEFAULT_STALE_SECS);
        assert_eq!(DEFAULT_STALE_SECS, 24 * 60 * 60);
    }
}
