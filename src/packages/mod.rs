//! 原生包管理的数据层：搜索、包信息、安装队列、搜索历史、Arch 新闻。
//!
//! 为什么是「原生」而不是启动 pac/pacsea：界面、解析、筛选、队列、状态全在这里，
//! 只有**真正改系统**的那一下交给包管理器（`paru -S`）—— pacsea 也是这么做的。
//!
//! 数据来源与理由：
//!
//! | 数据 | 来源 | 为什么 |
//! | --- | --- | --- |
//! | 官方源搜索 / 信息 | `pacman -Ss` / `pacman -Sii` | 没人会重写 libalpm；一律用 `LC_ALL=C` 跑，免得被本地化标记（`[已安装]`）影响解析 |
//! | AUR 搜索 / 信息 | AUR 官方 RPC（`curl` + JSON） | 比解析 `paru -Ss` 的文本强得多：得票、热度、维护者、是否过期、依赖都有 |
//! | 已安装集合 | `pacman -Qq` 一次拿全 | 搜索结果里标 `[已安装]`，AUR 的命中也要能标，逐个查太慢 |
//! | PKGBUILD | `paru -Gp` | 官方没有第二条路 |
//! | Arch 新闻 | `archlinux.org/feeds/news/` | 用来提醒「有没读过的新闻」，pacsea 的 Arch Status 就是这个意思 |

pub mod probe;

use std::{
    collections::BTreeSet,
    fs,
    path::{Path, PathBuf},
};

/// 一条搜索结果（官方源与 AUR 合并成同一种结构）。
#[derive(Clone, Debug, PartialEq)]
pub struct PackageHit {
    /// 仓库名：`core` / `extra` / `multilib` / `cachyos-v3` / `aur` …
    pub repo: String,
    pub name: String,
    pub version: String,
    pub description: String,
    /// 本地已安装（或已安装但版本不同）。
    pub installed: bool,
    /// 已安装的版本（来自 `pacman -Ss` 的标记；AUR 只知道自己装没装）。
    pub installed_version: Option<String>,
    /// AUR 得票（官方源没有）。
    pub votes: Option<u64>,
    /// AUR 热度（官方源没有）。
    pub popularity: Option<f64>,
    pub maintainer: Option<String>,
    /// AUR 已标记过期（官方源永远是 `false`）。
    pub out_of_date: bool,
}

impl PackageHit {
    pub fn is_aur(&self) -> bool {
        self.repo == "aur"
    }

    /// 结果表里那一列标记。
    pub fn status_label(&self) -> String {
        if self.out_of_date {
            return String::from("! 已过期");
        }
        if !self.installed {
            return String::new();
        }
        match &self.installed_version {
            Some(version) if version != &self.version => format!("↑ 已装 {version}"),
            _ => String::from("✓ 已安装"),
        }
    }
}

/// 极简模糊匹配（fzf 那套手感的骨架）。
///
/// `needle` 的字符按顺序出现在 `haystack` 里就算命中，返回分数：
/// 前缀命中、连续命中、起点靠前都给高分 —— 这样输入 `fz` 时 `fzf` 会排在
/// `fzf-tmux` 前面，和 fzf 的直觉一致。
///
/// 返回 `None` 表示不命中。
pub fn fuzzy_score(needle: &str, haystack: &str) -> Option<i32> {
    if needle.trim().is_empty() {
        return Some(0);
    }

    let hay: Vec<char> = haystack.to_lowercase().chars().collect();
    let mut cursor = 0usize;
    let mut score = 0i32;
    let mut previous: Option<usize> = None;

    for ch in needle.trim().to_lowercase().chars() {
        if ch == ' ' {
            continue;
        }
        let offset = hay.get(cursor..)?.iter().position(|item| *item == ch)?;
        let found = cursor + offset;

        score += 1;
        if found == 0 {
            score += 8; // 命中开头
        }
        match previous {
            Some(prev) if prev + 1 == found => score += 4, // 连续命中
            Some(prev) => score -= ((found - prev) as i32 - 1).min(4), // 跳过的越少越好
            None => score -= (found as i32).min(4),        // 起点越靠前越好
        }

        previous = Some(found);
        cursor = found + 1;
    }

    Some(score)
}

/// 结果排序方式（pacsea 顶栏那个 `Sort v`）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SortMode {
    /// 相关度：服务器给的顺序 + 本地模糊分数。
    Relevance,
    Name,
    Repo,
    /// AUR 的得票（官方源没有票，排后面）。
    Votes,
}

impl SortMode {
    pub const ALL: [SortMode; 4] = [
        SortMode::Relevance,
        SortMode::Name,
        SortMode::Repo,
        SortMode::Votes,
    ];

    pub fn label(self) -> &'static str {
        match self {
            SortMode::Relevance => "相关度",
            SortMode::Name => "名字",
            SortMode::Repo => "仓库",
            SortMode::Votes => "得票",
        }
    }

    pub fn next(self) -> Self {
        let index = Self::ALL.iter().position(|mode| *mode == self).unwrap_or(0);
        Self::ALL[(index + 1) % Self::ALL.len()]
    }
}

/// 解析 `LC_ALL=C pacman -Ss <词>` 的输出。
///
/// 格式（两行一组）：
///
/// ```text
/// extra/fzf 0.74.4-1 [installed]
///     Command-line fuzzy finder
/// ```
///
/// 方括号里可能是 `installed` 或 `installed: 1.2.3-1`（版本不同）。
pub fn parse_official_search(text: &str) -> Vec<PackageHit> {
    let mut hits = Vec::new();
    let mut lines = text.lines().peekable();

    while let Some(line) = lines.next() {
        if line.starts_with(' ') || line.trim().is_empty() {
            continue;
        }
        let Some((repo_name, rest)) = line.split_once(' ') else {
            continue;
        };
        let Some((repo, name)) = repo_name.split_once('/') else {
            continue;
        };

        // 版本之后如果还有内容，就是「已安装」的标记。
        let mut parts = rest.splitn(2, ' ');
        let version = parts.next().unwrap_or_default().to_string();
        let marker = parts.next().unwrap_or_default().trim();
        let installed = marker.starts_with('[');
        let installed_version = (installed && marker.contains(':'))
            .then(|| {
                marker
                    .trim_start_matches('[')
                    .split_once(':')
                    .map(|(_, version)| version.trim().trim_end_matches(']').to_string())
            })
            .flatten();

        // 描述是紧随其后的缩进行（可能有多行，接起来）。
        let mut description = String::new();
        while let Some(next) = lines.peek() {
            if !next.starts_with(' ') {
                break;
            }
            let next = lines.next().unwrap_or_default().trim();
            if !description.is_empty() {
                description.push(' ');
            }
            description.push_str(next);
        }

        hits.push(PackageHit {
            repo: repo.to_string(),
            name: name.to_string(),
            version,
            description,
            installed,
            installed_version,
            votes: None,
            popularity: None,
            maintainer: None,
            out_of_date: false,
        });
    }

    hits
}

/// 解析 `pacman -Qq`（一行一个已安装包名）。
pub fn parse_installed(text: &str) -> BTreeSet<String> {
    text.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_string)
        .collect()
}

/// 解析 `pacman -Sii` / `paru -Sii` 那种 `Key : Value` 输出。
///
/// 关键细节：**长值会折行**，续行以空格开头 —— 要并回上一个字段，
/// 否则「Required By」这种长列表会被截断（实拍确认过）。
pub fn parse_info(text: &str) -> Vec<(String, String)> {
    let mut fields: Vec<(String, String)> = Vec::new();

    for line in text.lines() {
        if line.trim().is_empty() {
            continue;
        }

        // pacman 的对齐会产生多个空格（`Repository      : x`），所以**不能**拿
        // 空格数判断是不是续行；真信号是：续行以空白开头（贴在上一行下面）。
        let is_continuation = line.starts_with(' ') || line.starts_with('\t');
        let split = line.split_once(" : ").or_else(|| line.split_once(':'));
        let (key, value) = match split {
            Some((key, value)) if !is_continuation && !key.trim().is_empty() => {
                (key.trim().to_string(), value.trim().to_string())
            }
            _ => {
                // 续行：接到上一个字段后面
                if let Some(last) = fields.last_mut() {
                    if !last.1.is_empty() {
                        last.1.push(' ');
                    }
                    last.1.push_str(line.trim());
                }
                continue;
            }
        };

        let value = value.trim().to_string();
        // 工具箱关心的字段排前面，其余按原顺序跟着（pacsea 的信息面板也是这个思路）。
        fields.push((key, value));
    }

    fields
}

// ── AUR ─────────────────────────────────────────────────────────────────────

/// AUR RPC 的搜索地址（`by=name-desc` 是 AUR 官方的模糊搜索）。
pub fn aur_search_url(term: &str) -> String {
    format!(
        "https://aur.archlinux.org/rpc/v5/search/{}?by=name-desc",
        url_encode(term)
    )
}

/// AUR RPC 的包信息地址（可以一次问多个）。
pub fn aur_info_url(name: &str) -> String {
    format!(
        "https://aur.archlinux.org/rpc/v5/info?arg[]={}",
        url_encode(name)
    )
}

/// 最小可用的 URL 百分号编码（只想避开空格与保留字符，不引第三方库）。
fn url_encode(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for byte in raw.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*byte as char);
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

#[derive(serde::Deserialize)]
struct AurResponse {
    #[serde(default)]
    results: Vec<AurPackage>,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "PascalCase")]
struct AurPackage {
    name: String,
    #[serde(default)]
    version: String,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    num_votes: u64,
    #[serde(default)]
    popularity: f64,
    #[serde(default)]
    maintainer: Option<String>,
    #[serde(default)]
    out_of_date: Option<u64>,
    #[serde(default, rename = "URL")]
    url: Option<String>,
    #[serde(default)]
    package_base: Option<String>,
    #[serde(default)]
    depends: Option<Vec<String>>,
    #[serde(default)]
    make_depends: Option<Vec<String>>,
    #[serde(default)]
    check_depends: Option<Vec<String>>,
    #[serde(default)]
    opt_depends: Option<Vec<String>>,
    #[serde(default)]
    license: Option<Vec<String>>,
    #[serde(default)]
    provides: Option<Vec<String>>,
    #[serde(default)]
    conflicts: Option<Vec<String>>,
    #[serde(default)]
    keywords: Option<Vec<String>>,
    #[serde(default)]
    first_submitted: Option<u64>,
    #[serde(default)]
    last_modified: Option<u64>,
    #[serde(default)]
    submitter: Option<String>,
    #[serde(default, rename = "URLPath")]
    url_path: Option<String>,
}

fn aur_hit(package: AurPackage) -> PackageHit {
    PackageHit {
        repo: String::from("aur"),
        name: package.name,
        version: package.version,
        description: package.description.unwrap_or_default(),
        installed: false, // 由调用方拿 `pacman -Qq` 标上
        installed_version: None,
        votes: Some(package.num_votes),
        popularity: Some(package.popularity),
        maintainer: package.maintainer,
        out_of_date: package.out_of_date.is_some(),
    }
}

/// 解析 AUR 搜索的 JSON。
pub fn parse_aur_search(json: &str) -> Result<Vec<PackageHit>, String> {
    let response: AurResponse =
        serde_json::from_str(json).map_err(|error| format!("AUR 返回的不是预期 JSON：{error}"))?;
    Ok(response.results.into_iter().map(aur_hit).collect())
}

/// 把 AUR 的包信息整理成和 `pacman -Sii` 一样的面板字段（顺序照着 pacsea 排）。
pub fn parse_aur_info(json: &str) -> Result<Vec<(String, String)>, String> {
    let response: AurResponse =
        serde_json::from_str(json).map_err(|error| format!("AUR 返回的不是预期 JSON：{error}"))?;
    let Some(package) = response.results.into_iter().next() else {
        return Err(String::from("AUR 里没有这个包"));
    };

    let list = |items: Option<Vec<String>>| match items {
        Some(items) if !items.is_empty() => items.join("  "),
        _ => String::from("None"),
    };
    let time = |stamp: Option<u64>| {
        stamp
            .map(format_epoch)
            .unwrap_or_else(|| String::from("None"))
    };

    let mut fields = vec![
        (String::from("Repository"), String::from("aur")),
        (String::from("Name"), package.name.clone()),
        (String::from("Version"), package.version.clone()),
        (
            String::from("Description"),
            package.description.clone().unwrap_or_default(),
        ),
        (String::from("URL"), package.url.clone().unwrap_or_default()),
        (String::from("Licenses"), list(package.license.clone())),
        (
            String::from("Maintainer"),
            package
                .maintainer
                .clone()
                .unwrap_or_else(|| String::from("无（孤儿包）")),
        ),
        (
            String::from("Submitter"),
            package.submitter.clone().unwrap_or_default(),
        ),
        (String::from("Votes"), package.num_votes.to_string()),
        (
            String::from("Popularity"),
            format!("{:.2}", package.popularity),
        ),
        (
            String::from("Out of Date"),
            package
                .out_of_date
                .map(format_epoch)
                .unwrap_or_else(|| String::from("No")),
        ),
        (String::from("Depends On"), list(package.depends.clone())),
        (
            String::from("Make Deps"),
            list(package.make_depends.clone()),
        ),
        (
            String::from("Check Deps"),
            list(package.check_depends.clone()),
        ),
        (
            String::from("Optional Deps"),
            list(package.opt_depends.clone()),
        ),
        (String::from("Provides"), list(package.provides.clone())),
        (
            String::from("Conflicts With"),
            list(package.conflicts.clone()),
        ),
        (String::from("Keywords"), list(package.keywords.clone())),
        (
            String::from("Package Base"),
            package.package_base.clone().unwrap_or_default(),
        ),
        (
            String::from("AUR URL"),
            format!("https://aur.archlinux.org/packages/{}", package.name),
        ),
        (
            String::from("Snapshot"),
            package
                .url_path
                .map(|path| format!("https://aur.archlinux.org{path}"))
                .unwrap_or_default(),
        ),
        (
            String::from("First Submitted"),
            time(package.first_submitted),
        ),
        (String::from("Last Modified"), time(package.last_modified)),
    ];
    fields.retain(|(_, value)| !value.is_empty());
    Ok(fields)
}

/// `1712345678` → `2024-04-05 22:01 UTC`（不引日期库，自己算；**刻意用 UTC 并写明**，
/// 免得 +0800 的你看 AUR 的「最后修改」时对不上钟）。
pub fn format_epoch(epoch: u64) -> String {
    let days = (epoch / 86_400) as i64;
    let seconds = epoch % 86_400;
    let (year, month, day) = civil_from_days(days);
    format!(
        "{year:04}-{month:02}-{day:02} {:02}:{:02} UTC",
        seconds / 3600,
        (seconds % 3600) / 60
    )
}

/// 把 `1970-01-01` 起的天数换成年月日（Howard Hinnant 的算法）。
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

// ── 安装队列 ─────────────────────────────────────────────────────────────────

/// 排队等着装的一个包。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QueuedPackage {
    pub name: String,
    pub origin: String,
    pub version: String,
}

impl QueuedPackage {
    pub fn new(hit: &PackageHit) -> Self {
        Self {
            name: hit.name.clone(),
            origin: hit.repo.clone(),
            version: hit.version.clone(),
        }
    }

    /// 导出成一行：`aur/pacsea-bin 0.8.2-2`（人和脚本都能读）。
    pub fn line(&self) -> String {
        format!("{}/{} {}", self.origin, self.name, self.version)
    }

    /// 从导出的一行读回来。宽容一点：`aur/名字`、`名字`、`名字 版本` 都收。
    pub fn parse(line: &str) -> Option<Self> {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            return None;
        }
        let mut parts = line.split_whitespace();
        let first = parts.next()?;
        let version = parts.next().unwrap_or_default().to_string();
        let (origin, name) = match first.split_once('/') {
            Some((origin, name)) => (origin.to_string(), name.to_string()),
            None => (String::from("?"), first.to_string()),
        };
        (!name.is_empty()).then_some(Self {
            name,
            origin,
            version,
        })
    }
}

/// 把队列写进文件（`paru -S` 一行都装不完时，明天还能接着装）。
pub fn save_queue_to(path: &Path, queue: &[QueuedPackage]) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut body = String::from("# toolbox-hub 安装队列（一行一个：仓库/包名 版本）\n");
    for item in queue {
        body.push_str(&item.line());
        body.push('\n');
    }
    fs::write(path, body)
}

/// 读回队列文件。
pub fn load_queue_from(path: &Path) -> Vec<QueuedPackage> {
    fs::read_to_string(path)
        .map(|text| text.lines().filter_map(QueuedPackage::parse).collect())
        .unwrap_or_default()
}

// ── 搜索历史 ─────────────────────────────────────────────────────────────────

/// 搜索历史最多记多少条。
pub const MAX_SEARCHES: usize = 30;

/// 记一条搜索词（重复的先删掉，最新的排最前）。
pub fn remember_search(history: &mut Vec<String>, term: &str) {
    let term = term.trim();
    if term.is_empty() {
        return;
    }
    history.retain(|item| item != term);
    history.insert(0, term.to_string());
    history.truncate(MAX_SEARCHES);
}

pub fn save_searches_to(path: &Path, history: &[String]) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut body = String::new();
    for term in history {
        // 换行会把历史文件撑坏，换成空格
        body.push_str(&term.replace(['\n', '\r'], " "));
        body.push('\n');
    }
    fs::write(path, body)
}

pub fn load_searches_from(path: &Path) -> Vec<String> {
    fs::read_to_string(path)
        .map(|text| {
            text.lines()
                .map(str::trim)
                .filter(|line| !line.is_empty())
                .take(MAX_SEARCHES)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

// ── Arch 新闻（未读提醒）─────────────────────────────────────────────────────

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NewsItem {
    pub title: String,
    pub published: String,
    pub link: String,
    /// 发布时间（秒）；解析不出来就是 `None`。
    pub epoch: Option<u64>,
}

/// 从 RSS 里挖出条目（只认 `<item>` 块里的 title/pubDate/link，不上 XML 库）。
pub fn parse_news_rss(xml: &str) -> Vec<NewsItem> {
    let mut items = Vec::new();

    for block in xml.split("<item>").skip(1) {
        let take = |open: &str, close: &str| -> Option<String> {
            let start = block.find(open)? + open.len();
            let rest = &block[start..];
            let end = rest.find(close)?;
            Some(decode_entities(rest[..end].trim()))
        };

        let Some(title) = take("<title>", "</title>") else {
            continue;
        };
        let published = take("<pubDate>", "</pubDate>").unwrap_or_default();
        let link = take("<link>", "</link>").unwrap_or_default();
        items.push(NewsItem {
            title,
            epoch: parse_rfc2822(&published),
            published,
            link,
        });
    }

    items
}

fn decode_entities(raw: &str) -> String {
    raw.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
}

/// 解析 RSS 里的 `Sat, 05 Apr 2025 12:34:56 +0000`。
pub fn parse_rfc2822(raw: &str) -> Option<u64> {
    let raw = raw.trim();
    let mut parts = raw.split_whitespace();
    let _weekday = parts.next()?;
    let day: i64 = parts.next()?.trim_end_matches(',').parse().ok()?;
    let month = match parts.next()? {
        "Jan" => 1,
        "Feb" => 2,
        "Mar" => 3,
        "Apr" => 4,
        "May" => 5,
        "Jun" => 6,
        "Jul" => 7,
        "Aug" => 8,
        "Sep" => 9,
        "Oct" => 10,
        "Nov" => 11,
        "Dec" => 12,
        _ => return None,
    };
    let year: i64 = parts.next()?.parse().ok()?;
    let time = parts.next()?;
    let mut clock = time.split(':');
    let hour: i64 = clock.next()?.parse().ok()?;
    let minute: i64 = clock.next()?.parse().ok()?;
    let second: i64 = clock.next().unwrap_or("0").parse().ok()?;

    // 时区偏移（+0800 / -0500 / GMT）
    let offset = match parts.next() {
        Some(zone) if zone.len() == 5 => {
            let sign = if zone.starts_with('-') { -1 } else { 1 };
            let hours: i64 = zone[1..3].parse().ok()?;
            let minutes: i64 = zone[3..5].parse().ok()?;
            sign * (hours * 3600 + minutes * 60)
        }
        _ => 0,
    };

    // 有符号算完再判非负：`00:30+0800` 这类会跨到前一天，中间值是负数
    let total =
        days_from_civil(year, month, day) * 86_400 + hour * 3600 + minute * 60 + second - offset;
    (total >= 0).then_some(total as u64)
}

fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let y = if month <= 2 { year - 1 } else { year };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = if month > 2 { month - 3 } else { month + 9 };
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// 从 `pacman.log` 里找出**最后一次全系统更新**的时间（秒）。
///
/// 比它新的新闻就是「你升级之后才发布的」—— 那才是需要提醒的未读新闻
/// （pacsea 的 Arch Status 就是这个用途）。
pub fn last_upgrade_epoch(log: &str) -> Option<u64> {
    let mut latest = None;
    for line in log.lines() {
        if !line.contains("starting full system upgrade") {
            continue;
        }
        let Some(start) = line.find('[') else {
            continue;
        };
        let Some(end) = line[start..].find(']') else {
            continue;
        };
        let stamp = &line[start + 1..start + end];
        // `2026-09-26T19:53:47+0800`
        if let Some(epoch) = parse_iso8601(stamp) {
            latest = Some(epoch);
        }
    }
    latest
}

/// 解析 pacman 日志里的 `2026-09-26T19:53:47+0800`。
pub fn parse_iso8601(raw: &str) -> Option<u64> {
    let raw = raw.trim();
    if raw.len() < 19 {
        return None;
    }
    let year: i64 = raw.get(0..4)?.parse().ok()?;
    let month: i64 = raw.get(5..7)?.parse().ok()?;
    let day: i64 = raw.get(8..10)?.parse().ok()?;
    let hour: i64 = raw.get(11..13)?.parse().ok()?;
    let minute: i64 = raw.get(14..16)?.parse().ok()?;
    let second: i64 = raw.get(17..19)?.parse().ok()?;

    let offset = raw.get(19..).and_then(|zone| {
        if zone.len() < 3 {
            return None;
        }
        let sign = if zone.starts_with('-') { -1 } else { 1 };
        let hours: i64 = zone.get(1..3)?.parse().ok()?;
        let minutes: i64 = zone.get(3..5).and_then(|m| m.parse().ok()).unwrap_or(0);
        Some(sign * (hours * 3600 + minutes * 60))
    });

    // 注意：`03:00+0800` 减掉时区偏移是**负数**（等于前一天 19:00 UTC）——
    // 必须先有符号算完再进位，直接 `as u64` 会在 debug 下溢出 panic
    // （你 pacman 日志里凌晨的升级记录就是这么把测试打挂的）。
    let days = days_from_civil(year, month, day);
    let total = days * 86_400 + hour * 3600 + minute * 60 + second - offset.unwrap_or(0);
    (total >= 0).then_some(total as u64)
}

/// 哪些新闻是「升级之后才出现的」（需要你看一眼）。
pub fn unread_news(items: &[NewsItem], last_upgrade: Option<u64>) -> usize {
    match last_upgrade {
        Some(mark) => items
            .iter()
            .filter(|item| item.epoch.is_some_and(|epoch| epoch > mark))
            .count(),
        // 不知道上次升级时间：就当全都可能没读过（宁可多提醒）
        None => items.len(),
    }
}

/// 数据文件放哪儿（队列、搜索历史）。
pub fn data_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("TOOLBOX_HUB_DATA") {
        return PathBuf::from(dir);
    }
    let home = std::env::var_os("HOME").unwrap_or_default();
    PathBuf::from(home).join(".local/share/toolbox-hub")
}

pub fn queue_path() -> PathBuf {
    data_dir().join("install-queue.txt")
}

pub fn searches_path() -> PathBuf {
    data_dir().join("package-searches.log")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 真实 `LC_ALL=C pacman -Ss ^fzf$` 的输出（含「已装但版本不同」那种标记）。
    const OFFICIAL_SEARCH: &str = "\
cachyos-extra-v3/fzf 0.74.4-1.1 [installed]
    Command-line fuzzy finder
extra/fzf 0.74.4-1 [installed: 0.74.4-1.1]
    Command-line fuzzy finder
community/fzf-tmux 0.74.4-1
    A tmux wrapper for fzf
";

    /// 真实 `pacman -Sii bash | head -22` 的形状（含折行的长值）。
    const INFO: &str = "\
Repository      : cachyos-v3
Name            : bash
Version         : 5.3.20-2
Description     : The GNU Bourne Again shell
Depends On      : readline  libreadline.so=8-64  glibc  ncurses
Required By     : 4ti2  7zip  7zip-zstd  9base  abcde
                  aconfmgr-git  acpid  adljack
Optional For    : a2jmidid  alsa-oss
Download Size   : 2030.16 KiB
";

    /// 真实 AUR RPC 回复（裁到两个结果，字段一个不少）。
    const AUR_SEARCH: &str = r#"{"resultcount":1,"results":[{"Description":"Fast TUI for searching","FirstSubmitted":1759428378,"ID":2171380,"LastModified":1784573962,"Maintainer":"Firstpick","Name":"pacsea-bin","NumVotes":5,"OutOfDate":null,"PackageBase":"pacsea-bin","PackageBaseID":223019,"Popularity":0.093482,"URL":"https://github.com/Firstp1ck/Pacsea","URLPath":"/cgit/aur.git/snapshot/pacsea-bin.tar.gz","Version":"0.8.2-2"}]}"#;

    #[test]
    fn fuzzy_matching_prefers_prefix_and_consecutive_hits() {
        // 命中与不命中
        assert!(fuzzy_score("fzf", "fzf").is_some());
        assert!(fuzzy_score("fz", "fzf").is_some(), "子序列也算");
        assert!(fuzzy_score("FZ", "fzf").is_some(), "大小写无关");
        assert!(fuzzy_score("zzz", "fzf").is_none());
        assert!(fuzzy_score("", "anything").is_some(), "空词全命中");

        // 前缀 + 连续 > 分散命中；`fzf` 应该压过 `fzf-tmux` 之前的那类
        let tight = fuzzy_score("fzf", "fzf").expect("命中");
        let spread = fuzzy_score("fzf", "foo-zzz-foo").expect("命中");
        assert!(tight > spread, "{tight} 应该大于 {spread}");

        let prefix = fuzzy_score("fz", "fzf").expect("命中");
        let middle = fuzzy_score("fz", "xxfzf").expect("命中");
        assert!(prefix > middle, "前缀应该更高：{prefix} vs {middle}");
    }

    #[test]
    fn sort_modes_cycle_through_all_of_them() {
        let mut mode = SortMode::Relevance;
        for _ in 0..SortMode::ALL.len() {
            mode = mode.next();
        }
        assert_eq!(mode, SortMode::Relevance, "转一圈回到起点");
        assert_eq!(SortMode::Votes.label(), "得票");
    }

    #[test]
    fn official_search_parses_repo_name_version_and_installed_marker() {
        let hits = parse_official_search(OFFICIAL_SEARCH);
        assert_eq!(hits.len(), 3);

        assert_eq!(hits[0].repo, "cachyos-extra-v3");
        assert_eq!(hits[0].name, "fzf");
        assert_eq!(hits[0].version, "0.74.4-1.1");
        assert!(hits[0].installed);
        assert_eq!(hits[0].installed_version, None, "单纯 installed 没有版本");
        assert_eq!(hits[0].description, "Command-line fuzzy finder");

        assert_eq!(
            hits[1].installed_version.as_deref(),
            Some("0.74.4-1.1"),
            "已装但版本不同要能读出来"
        );
        assert_eq!(hits[1].status_label(), "↑ 已装 0.74.4-1.1");

        assert!(!hits[2].installed, "没有标记就是没装");
        assert_eq!(hits[2].status_label(), "");
    }

    #[test]
    fn official_search_ignores_blank_and_continuation_lines_at_the_top() {
        let hits =
            parse_official_search("\n   stray continuation\n\ncore/zsh 5.9-1\n    The Z shell\n");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].name, "zsh");
    }

    #[test]
    fn info_fields_join_wrapped_lines() {
        let fields = parse_info(INFO);
        let get = |key: &str| {
            fields
                .iter()
                .find(|(name, _)| name == key)
                .map(|(_, value)| value.clone())
                .unwrap_or_default()
        };

        assert_eq!(get("Repository"), "cachyos-v3");
        assert_eq!(get("Name"), "bash");
        assert_eq!(
            get("Required By"),
            "4ti2  7zip  7zip-zstd  9base  abcde aconfmgr-git  acpid  adljack",
            "折行的值必须接起来，否则长列表会被截断"
        );
        assert_eq!(
            get("Depends On"),
            "readline  libreadline.so=8-64  glibc  ncurses"
        );
    }

    #[test]
    fn installed_set_is_one_name_per_line() {
        let set = parse_installed("7zip\na52dec\n\n  aalib  \n");
        assert_eq!(set.len(), 3);
        assert!(set.contains("aalib"));
    }

    #[test]
    fn aur_search_json_becomes_hits() {
        let hits = parse_aur_search(AUR_SEARCH).expect("应能解析");
        assert_eq!(hits.len(), 1);
        let hit = &hits[0];
        assert_eq!(hit.repo, "aur");
        assert_eq!(hit.name, "pacsea-bin");
        assert_eq!(hit.version, "0.8.2-2");
        assert_eq!(hit.votes, Some(5));
        assert_eq!(hit.popularity, Some(0.093482));
        assert_eq!(hit.maintainer.as_deref(), Some("Firstpick"));
        assert!(!hit.out_of_date, "OutOfDate 是 null 就是没过期");
        assert!(hit.is_aur());
    }

    #[test]
    fn aur_search_rejects_garbage_instead_of_panicking() {
        assert!(parse_aur_search("not json").is_err());
        assert!(parse_aur_search("{}").expect("空结果也算合法").is_empty());
    }

    #[test]
    fn aur_info_lines_up_with_the_official_panel() {
        let json = r#"{"resultcount":1,"results":[{"Conflicts":["pacsea"],"Depends":["pacman","curl"],"Description":"Fast TUI","FirstSubmitted":1759428378,"Keywords":["tui","pacman"],"LastModified":1784573962,"License":["MIT"],"Maintainer":"Firstpick","Name":"pacsea-bin","NumVotes":5,"OptDepends":["paru: 装包用"],"OutOfDate":null,"PackageBase":"pacsea-bin","Popularity":0.093482,"Provides":["pacsea"],"URL":"https://example.com","URLPath":"/cgit/aur.git/snapshot/pacsea-bin.tar.gz","Version":"0.8.2-2"}]}"#;
        let fields = parse_aur_info(json).expect("应能解析");
        let get = |key: &str| {
            fields
                .iter()
                .find(|(name, _)| name == key)
                .map(|(_, value)| value.clone())
                .unwrap_or_default()
        };

        assert_eq!(get("Repository"), "aur");
        assert_eq!(get("Votes"), "5");
        assert_eq!(get("Popularity"), "0.09");
        assert_eq!(get("Depends On"), "pacman  curl");
        assert_eq!(get("Out of Date"), "No");
        assert_eq!(get("URL"), "https://example.com", "URL 是全大写键");
        assert_eq!(get("Licenses"), "MIT");
        assert!(get("AUR URL").contains("pacsea-bin"));
        assert!(get("Snapshot").starts_with("https://aur.archlinux.org/cgit"));
        assert_eq!(get("Last Modified"), format_epoch(1784573962));
    }

    #[test]
    fn epochs_format_like_a_person_wrote_them() {
        assert_eq!(format_epoch(0), "1970-01-01 00:00 UTC");
        assert_eq!(format_epoch(1_760_000_000), "2025-10-09 08:53 UTC");
        assert_eq!(
            parse_rfc2822("Sat, 05 Apr 2025 12:34:56 +0000"),
            Some(1_743_856_496)
        );
        assert_eq!(parse_rfc2822("garbage"), None);
        assert_eq!(
            parse_iso8601("2026-09-26T19:53:47+0800"),
            Some(1_790_423_627)
        );
        assert_eq!(parse_iso8601("nope"), None);
        // 凌晨 +0800：减掉时区偏移会跨到前一天，且中间值是负数
        assert_eq!(
            parse_iso8601("2026-01-01T03:00:00+0800"),
            parse_iso8601("2025-12-31T19:00:00+0000"),
            "跨天的时区换算要正确"
        );
    }

    #[test]
    fn the_last_upgrade_comes_from_pacman_log() {
        let log = "\
[2026-09-01T10:00:00+0800] [PACMAN] Running 'pacman -Syu'
[2026-09-01T10:00:00+0800] [PACMAN] starting full system upgrade
[2026-09-26T19:53:47+0800] [PACMAN] starting full system upgrade
[2026-09-26T20:00:00+0800] [ALPM] installed foo
";
        assert_eq!(last_upgrade_epoch(log), Some(1_790_423_627), "取最后一次");
        assert_eq!(last_upgrade_epoch("no upgrades here"), None);
    }

    #[test]
    fn unread_news_counts_only_what_came_after_the_upgrade() {
        let xml = r#"<rss><channel>
<item><title>New kernel &amp; you</title><pubDate>Sat, 05 Apr 2025 12:00:00 +0000</pubDate><link>https://a.example/1</link></item>
<item><title>Old news</title><pubDate>Mon, 01 Jan 2024 00:00:00 +0000</pubDate><link>https://a.example/2</link></item>
</channel></rss>"#;
        let items = parse_news_rss(xml);
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].title, "New kernel & you", "实体要解码");
        assert_eq!(items[0].link, "https://a.example/1");

        let after_old = parse_rfc2822("Mon, 01 Jan 2024 00:00:00 +0000").expect("时间");
        assert_eq!(
            unread_news(&items, Some(after_old)),
            1,
            "只有新的一条算未读"
        );
        assert_eq!(unread_news(&items, None), 2, "不知道就全提醒");
    }

    #[test]
    fn queue_round_trips_through_a_file() {
        let path =
            std::env::temp_dir().join(format!("toolbox-hub-queue-{}.txt", std::process::id()));
        let queue = vec![
            QueuedPackage {
                name: String::from("pacsea-bin"),
                origin: String::from("aur"),
                version: String::from("0.8.2-2"),
            },
            QueuedPackage {
                name: String::from("fzf"),
                origin: String::from("extra"),
                version: String::from("0.74.4-1"),
            },
        ];

        save_queue_to(&path, &queue).expect("写队列");
        let back = load_queue_from(&path);
        assert_eq!(back, queue, "导出再导入要一模一样");

        // 宽容解析：只有名字也能读，注释行跳过
        assert_eq!(QueuedPackage::parse("# 注释"), None);
        assert_eq!(
            QueuedPackage::parse("hello").map(|item| item.name),
            Some(String::from("hello"))
        );

        let _ = fs::remove_file(&path);
    }

    #[test]
    fn search_history_keeps_the_newest_and_drops_duplicates() {
        let mut history = Vec::new();
        remember_search(&mut history, "fzf");
        remember_search(&mut history, "bash");
        remember_search(&mut history, "fzf");
        assert_eq!(history, vec![String::from("fzf"), String::from("bash")]);

        remember_search(&mut history, "   ");
        assert_eq!(history.len(), 2, "空词不记");

        for index in 0..MAX_SEARCHES + 5 {
            remember_search(&mut history, &format!("term{index}"));
        }
        assert_eq!(history.len(), MAX_SEARCHES, "有上限");
        assert_eq!(history[0], format!("term{}", MAX_SEARCHES + 4));
    }
}
