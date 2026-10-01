//! 原生包管理的数据层：搜索、包信息、安装队列、搜索历史、Arch 新闻。
//!
//! 为什么是「原生」：界面、解析、筛选、队列、状态全在 Toolbox Hub 里，
//! 只有**真正改系统**的那一下交给 pacman / paru，终端接管负责 sudo 与交互确认。
//!
//! 数据来源与理由：
//!
//! | 数据 | 来源 | 为什么 |
//! | --- | --- | --- |
//! | 官方源搜索 / 信息 / 已安装 / 可更新 | **libalpm 直连**（[`libalpm`]） | 结构化数据、pacman 自己的 vercmp、比起进程快 1~3 个数量级（实测表见 [`libalpm`] 开头） |
//! | AUR 搜索 / 信息 | AUR 官方 RPC（`curl` + JSON） | 比解析 `paru -Ss` 的文本强得多：得票、热度、维护者、是否过期、依赖都有 |
//! | 已安装集合 | `pacman -Qq` 一次拿全 | 搜索结果里标 `[已安装]`，AUR 的命中也要能标，逐个查太慢 |
//! | PKGBUILD | `paru -Gp` | 官方没有第二条路 |
//! | Arch 新闻 | `archlinux.org/feeds/news/` | 用来提醒「有没读过的新闻」，pacsea 的 Arch Status 就是这个意思 |

pub mod health;
pub mod libalpm;
pub mod probe;
pub mod worker;

use std::{
    collections::BTreeSet,
    fs,
    path::{Path, PathBuf},
};

/// 本地已装版本与「正在看的这个版本」的关系。
///
/// 为什么不只是一个 `installed: bool` 加字符串比较：pacman 的版本序有 epoch、
/// pkgrel、字母数字段一堆特例，字符串比会把**方向**搞反 —— 实拍过：
/// `cachyos-extra-v3/fzf 0.74.4-1.1` 已装时，`extra/fzf 0.74.4-1` 被标成
/// 「↑ 已装 0.74.4-1.1」，看着像有更新，其实本地那个更新。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum InstalledState {
    #[default]
    NotInstalled,
    /// 装的就是这个版本。
    Same,
    /// 本地是旧版本，仓库里有新的 → 可以升级。
    Older(String),
    /// 本地版本更新（第三方仓库常常比官方源新）→ 别乱降级。
    Newer(String),
}

impl InstalledState {
    /// 由 pacman 的 vercmp 结果得出（比较交给 libalpm，别自己写）。
    pub fn from_ordering(ordering: std::cmp::Ordering, installed: String) -> Self {
        match ordering {
            std::cmp::Ordering::Equal => Self::Same,
            std::cmp::Ordering::Greater => Self::Older(installed),
            std::cmp::Ordering::Less => Self::Newer(installed),
        }
    }

    pub fn is_installed(&self) -> bool {
        !matches!(self, Self::NotInstalled)
    }

    /// 结果表里那一列标记（`None` = 没什么好说的）。
    pub fn label(&self) -> Option<String> {
        match self {
            Self::NotInstalled => None,
            Self::Same => Some(String::from("✓ 已安装")),
            Self::Older(version) => Some(format!("↑ 可升级（已装 {version}）")),
            Self::Newer(version) => Some(format!("✓ 已装更新版 {version}")),
        }
    }
}

/// 一条搜索结果（官方源与 AUR 合并成同一种结构）。
#[derive(Clone, Debug, PartialEq)]
pub struct PackageHit {
    /// 仓库名：`core` / `extra` / `multilib` / `cachyos-v3` / `aur` …
    pub repo: String,
    pub name: String,
    pub version: String,
    pub description: String,
    /// 本地装的那个与这个的关系（含「没装」）。
    pub installed_state: InstalledState,
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

    pub fn is_installed(&self) -> bool {
        self.installed_state.is_installed()
    }

    /// 结果表里那一列标记。
    pub fn status_label(&self) -> String {
        if self.out_of_date {
            return String::from("! 已过期");
        }
        self.installed_state.label().unwrap_or_default()
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

/// 结果排序方式（pacseek 顶栏那个 `Sort v`）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SortMode {
    /// 相关度：服务器给的顺序 + 本地模糊分数。
    Relevance,
    Name,
    Repo,
    /// AUR 的得票（官方源没有票，排后面）。
    Votes,
    /// 版本号（已安装列表用；搜索结果的版本排序意义不大，但留着无妨）。
    Version,
}

impl SortMode {
    pub fn label(self) -> &'static str {
        match self {
            SortMode::Relevance => "相关度",
            SortMode::Name => "名字",
            SortMode::Repo => "仓库",
            SortMode::Votes => "得票",
            SortMode::Version => "版本",
        }
    }
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

/// AUR RPC 返回的一个包。
///
/// **整个结构都留着**，不只在搜索时拧成一条 [`PackageHit`]：信息面板要的
/// Depends / License / Provides / Keywords / 得票 …… 全都在这个响应里，
/// 留着它，选中 AUR 结果时就**不需要再发一次 RPC**（那是又一次冷握手的钱）。
#[derive(Clone, Debug, serde::Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct AurPackage {
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

impl AurPackage {
    /// 搜索结果表里的一行。
    ///
    /// 「本地装没装」AUR 不知道，由调用方拿本地库补上
    /// （[`crate::packages::probe::Net::search_aur`] 会做这件事）。
    pub fn hit(&self) -> PackageHit {
        PackageHit {
            repo: String::from("aur"),
            name: self.name.clone(),
            version: self.version.clone(),
            description: self.description.clone().unwrap_or_default(),
            installed_state: InstalledState::NotInstalled,
            votes: Some(self.num_votes),
            popularity: Some(self.popularity),
            maintainer: self.maintainer.clone(),
            out_of_date: self.out_of_date.is_some(),
        }
    }
}

/// 解析 AUR 搜索的 JSON：**把整个响应留着**，不急着拧成 `PackageHit`。
pub fn parse_aur_search(json: &str) -> Result<Vec<AurPackage>, String> {
    let response: AurResponse =
        serde_json::from_str(json).map_err(|error| format!("AUR 返回的不是预期 JSON：{error}"))?;
    Ok(response.results)
}

/// 解析 AUR `info` 的 JSON（拿不到包就报错）。
pub fn parse_aur_one(json: &str) -> Result<AurPackage, String> {
    let response: AurResponse =
        serde_json::from_str(json).map_err(|error| format!("AUR 返回的不是预期 JSON：{error}"))?;
    response
        .results
        .into_iter()
        .next()
        .ok_or_else(|| String::from("AUR 里没有这个包"))
}

impl AurPackage {
    /// 整理成和官方源同一套的信息面板字段（顺序照着 paru / pacsea 排）。
    ///
    /// 关键在于它**不需要联网**：这些字段早就在搜索响应里了。
    pub fn info_fields(&self) -> Vec<(String, String)> {
        let package = self;
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
                    .clone()
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
        fields
    }
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

// ── 执行什么：把「操作 + 队列」翻译成一条真实命令 ────────────────────────────
//
// 这一层**只生成 argv，绝不执行**。两个理由：
//
// 1. dry-run：界面要把「马上要跑的那条命令」原样给你看（pacsea 的 --dry-run 就是
//    这个意思），确认面板上显示的命令必须**逐字**等于真跑的命令；
// 2. 复用：TUI 的确认面板与 `toolbox-hub -i` 的命令行模式共用同一份翻译，
//    不会出现「界面里写的东西」和「CLI 里写的东西」各跑各的。

/// 队列要执行的操作。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PackageOperation {
    Install,
    Remove,
    Download,
}

impl PackageOperation {
    pub fn label(self) -> &'static str {
        match self {
            Self::Install => "安装",
            Self::Remove => "卸载",
            Self::Download => "仅下载",
        }
    }

    /// 更长的说明，给确认面板用（`仅下载` 两个字说不清会发生什么）。
    pub fn detail(self) -> &'static str {
        match self {
            Self::Install => "下载并安装（缺的依赖会一起装）",
            Self::Remove => "卸载，并清掉只被它们依赖的依赖（-Rns）",
            Self::Download => "只下到 pacman 缓存，不安装",
        }
    }

    pub fn next(self) -> Self {
        match self {
            Self::Install => Self::Remove,
            Self::Remove => Self::Download,
            Self::Download => Self::Install,
        }
    }

    /// 哪个程序来干这活。
    ///
    /// 只有**队列里真有 AUR 包**时才请 paru —— 官方源的事 pacman 就够，
    /// 少一层包装少一份意外；`--needed` 这类保护也由我们显式写出来。
    pub fn program(self, uses_aur: bool) -> &'static str {
        if uses_aur && self != Self::Download {
            "paru"
        } else {
            "pacman"
        }
    }

    /// 包名之外的开关。
    pub fn flags(self) -> &'static [&'static str] {
        match self {
            Self::Install => &["-S", "--needed"],
            Self::Remove => &["-Rns"],
            // `-Sw` 是「只下载」；AUR 包走的是源码快照，这个开关对它没意义，
            // 所以 program() 在这一档**不会**切到 paru（免得看着像能下 AUR 一样）。
            Self::Download => &["-Sw", "--needed"],
        }
    }

    pub fn argv(self, names: &[String]) -> Vec<String> {
        let mut argv: Vec<String> = self
            .flags()
            .iter()
            .map(|flag| (*flag).to_string())
            .collect();
        argv.extend(names.iter().cloned());
        argv
    }
}

/// 系统更新的开关（paru 与 pacman 都认）。
pub const UPGRADE_FLAG: &str = "-Syu";

/// 当前进程是不是 root（读 `/proc/self/status` 的 `Uid:` 行，不引 libc）。
///
/// 为什么需要它：`pacman` / `paccache` **必须**以 root 跑，而 `paru` / `yay`
/// 自己会去调 sudo（而且它们**必须**保持非 root，否则拒绝干活）。搞混这两种
/// 语义的后果很实在 —— 界面里显示 `pacman -S fzf`、按下去却回一句
/// 「you cannot perform this operation unless you are root」。
pub fn is_root() -> bool {
    std::fs::read_to_string("/proc/self/status")
        .map(|text| {
            text.lines()
                .any(|line| line.starts_with("Uid:") && line.split_whitespace().nth(1) == Some("0"))
        })
        .unwrap_or(false)
}

/// 这个程序自己管提权吗（paru / yay 会自己 sudo，前面再加一层反而出错）。
fn handles_own_escalation(program: &str) -> bool {
    matches!(program, "paru" | "yay")
}

/// 把「程序 + argv」变成**最终真的要执行**的那条命令：需要时补上 `sudo`。
///
/// 提权结果会一路带到确认面板上，所以你在界面上看到的那条命令
/// （`sudo pacman -S --needed fzf`）和真正跑的完全一致。
pub fn escalate(program: &str, argv: &[String]) -> (String, Vec<String>) {
    if !handles_own_escalation(program) && !is_root() {
        let mut escalated = Vec::with_capacity(argv.len() + 1);
        escalated.push(program.to_string());
        escalated.extend(argv.iter().cloned());
        (String::from("sudo"), escalated)
    } else {
        (program.to_string(), argv.to_vec())
    }
}

/// 把 `program + argv` 渲染成能直接粘进 shell 的一行。
///
/// dry-run 与确认面板都用它：**看到的就是跑的**。
pub fn command_preview(program: &str, argv: &[String]) -> String {
    let mut out = String::from(program);
    for arg in argv {
        out.push(' ');
        out.push_str(&shell_quote(arg));
    }
    out
}

/// 只在必要时加引号：命令预览是给人看的，满屏引号反而看不清重点。
fn shell_quote(raw: &str) -> String {
    let plain = !raw.is_empty()
        && raw
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || "-_./@%+=:,".contains(ch));
    if plain {
        raw.to_string()
    } else {
        format!("'{}'", raw.replace('\'', r"'\''"))
    }
}

/// 清下载缓存：优先 `paccache`（能只留最近 N 个版本），没有就退到 `pacman -Sc`。
///
/// 为什么不是 `pacman -Sc` 打头：它会把**当前仓库里已经没有的包**全删掉，
/// 而 paccache 只动版本历史，两种语义差别很大 —— 默认走温和的那种。
pub fn cache_command(keep: u8, has_paccache: bool) -> (String, Vec<String>) {
    if has_paccache {
        (
            String::from("paccache"),
            vec![format!("-rk{}", keep.clamp(1, 9))],
        )
    } else {
        (String::from("pacman"), vec![String::from("-Sc")])
    }
}

/// 卸载孤儿包的命令（`pacman -Qtdq` 拿名单，这里只负责拼 argv）。
pub fn orphan_remove_command(names: &[String]) -> (String, Vec<String>) {
    let mut argv = vec![String::from("-Rns")];
    argv.extend(names.iter().cloned());
    (String::from("pacman"), argv)
}

// ── 已安装包浏览器（pacsea 的 --list / --exp / --imp / --all）────────────────

/// 本地已安装包的一行。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InstalledPackage {
    pub name: String,
    pub version: String,
    /// `pacman -Qe`：你自己点名装的（对立面是「被依赖拖进来」）。
    pub explicit: bool,
    /// `pacman -Qm`：不在官方源里（AUR、手工编译、本地包）。
    pub foreign: bool,
    /// `pacman -Qtd`：没人依赖、你也不是点名装的 —— 可以清的孤儿。
    pub orphan: bool,
}

impl InstalledPackage {
    /// 列表右边那一列标记，一眼能分出四类。
    pub fn tag(&self) -> &'static str {
        if self.orphan {
            "孤儿"
        } else if self.foreign {
            "外来"
        } else if self.explicit {
            "显式"
        } else {
            "依赖"
        }
    }
}

/// 已安装列表的筛选项（对应 pacsea 的 `--exp` / `--imp` / `--all`，另加孤儿与外来）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InstalledFilter {
    All,
    Explicit,
    Dependency,
    Foreign,
    Orphan,
}

impl InstalledFilter {
    pub const ALL: [InstalledFilter; 5] = [
        Self::All,
        Self::Explicit,
        Self::Dependency,
        Self::Foreign,
        Self::Orphan,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::All => "全部",
            Self::Explicit => "显式",
            Self::Dependency => "依赖",
            Self::Foreign => "外来",
            Self::Orphan => "孤儿",
        }
    }

    pub fn matches(self, package: &InstalledPackage) -> bool {
        match self {
            Self::All => true,
            // 孤儿虽然也算「显式装的」，看「显式」时不该把它混进来 —— 那正是想清掉的那堆。
            Self::Explicit => package.explicit && !package.orphan,
            Self::Dependency => !package.explicit,
            Self::Foreign => package.foreign,
            Self::Orphan => package.orphan,
        }
    }
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

/// 哪些新闻是「升级之后才发布的」（**建议看一眼**，不等于未读）。
///
/// 这是 pacsea 的 Arch Status 那个意思：真正的未读靠 [`load_read_news`] 记，
/// 而「升级之后」只是提醒「这条可能和刚才那次升级有关」。
pub fn news_published_since(items: &[NewsItem], last_upgrade: Option<u64>) -> usize {
    match last_upgrade {
        Some(mark) => items
            .iter()
            .filter(|item| item.epoch.is_some_and(|epoch| epoch > mark))
            .count(),
        // 不知道上次升级时间：就当全都可能相关（宁可多提醒）
        None => items.len(),
    }
}

// ── 新闻已读状态（pacsea 的 --unread / --read / --all-news）──────────────────
//
// 只拿 `pacman.log` 猜「升级之后」是不够的：你昨天看过的那条今天还会被算成新的。
// 所以额外落一份**已读集合**，`r` 就是「这条我看过了」。

/// 一条新闻的唯一键：链接最稳（标题偶尔会被改）。
pub fn news_key(item: &NewsItem) -> String {
    if item.link.trim().is_empty() {
        item.title.clone()
    } else {
        item.link.clone()
    }
}

/// 新闻筛选：未读 / 已读 / 全部。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NewsFilter {
    Unread,
    Read,
    All,
}

impl NewsFilter {
    pub const ALL: [NewsFilter; 3] = [Self::Unread, Self::Read, Self::All];

    pub fn label(self) -> &'static str {
        match self {
            Self::Unread => "未读",
            Self::Read => "已读",
            Self::All => "全部",
        }
    }

    pub fn matches(self, item: &NewsItem, read: &BTreeSet<String>) -> bool {
        let is_read = read.contains(&news_key(item));
        match self {
            Self::Unread => !is_read,
            Self::Read => is_read,
            Self::All => true,
        }
    }
}

pub fn read_news_path() -> PathBuf {
    data_dir().join("news-read.log")
}

/// 读已读集合（一行一个键，纯文本，能直接看/改）。
///
/// 开头的 `#` 注释行要跳过：文件头部有一行说明，不跳的话它会被当成
/// 「有一条叫这个名字的新闻已经读过了」。
pub fn load_read_news(path: &Path) -> BTreeSet<String> {
    fs::read_to_string(path)
        .map(|text| {
            text.lines()
                .map(str::trim)
                .filter(|line| !line.is_empty() && !line.starts_with('#'))
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

/// 写已读集合。`BTreeSet` 保证输出稳定，diff 不会因为顺序乱跳。
pub fn save_read_news(path: &Path, read: &BTreeSet<String>) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut body = String::from("# toolbox-hub 已读新闻（一行一个链接）\n");
    for key in read {
        body.push_str(&key.replace(['\n', '\r'], " "));
        body.push('\n');
    }
    fs::write(path, body)
}

/// 数据文件放哪儿（队列、搜索历史、已读新闻）。
///
/// 由 [`crate::config`] 统一定：`--data-dir` > `TOOLBOX_HUB_DATA` >
/// `~/.local/share/toolbox-hub`。
pub fn data_dir() -> PathBuf {
    crate::config::data_dir()
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
    fn sort_modes_are_labelled_for_the_menu() {
        assert_eq!(SortMode::Votes.label(), "得票");
        assert_eq!(SortMode::Version.label(), "版本");
        assert_eq!(PackageOperation::Remove.label(), "卸载");
        assert_eq!(PackageOperation::Install.next(), PackageOperation::Remove);
        assert_eq!(PackageOperation::Download.next(), PackageOperation::Install);
    }

    /// 搜索响应留的是整个 [`AurPackage`]，转成结果行是它自己的事
    /// —— 这样信息面板就不用为了几个字段再发一次请求。
    #[test]
    fn aur_search_json_becomes_packages_and_hits() {
        let packages = parse_aur_search(AUR_SEARCH).expect("应能解析");
        assert_eq!(packages.len(), 1);
        assert_eq!(packages[0].name, "pacsea-bin");

        let hit = packages[0].hit();
        assert_eq!(hit.repo, "aur");
        assert_eq!(hit.name, "pacsea-bin");
        assert_eq!(hit.version, "0.8.2-2");
        assert_eq!(hit.votes, Some(5));
        assert_eq!(hit.popularity, Some(0.093482));
        assert_eq!(hit.maintainer.as_deref(), Some("Firstpick"));
        assert!(!hit.out_of_date, "OutOfDate 是 null 就是没过期");
        assert!(hit.is_aur());

        // 同一个对象就能拼出信息面板：这一条是「看 AUR 信息不再联网」的根据
        let fields = packages[0].info_fields();
        assert!(fields.iter().any(|(key, _)| key == "Votes"));
        assert!(fields.iter().any(|(key, _)| key == "Depends On"));
    }

    #[test]
    fn aur_search_rejects_garbage_instead_of_panicking() {
        assert!(parse_aur_search("not json").is_err());
        assert!(parse_aur_search("{}").expect("空结果也算合法").is_empty());
    }

    #[test]
    fn aur_info_lines_up_with_the_official_panel() {
        let json = r#"{"resultcount":1,"results":[{"Conflicts":["pacsea"],"Depends":["pacman","curl"],"Description":"Fast TUI","FirstSubmitted":1759428378,"Keywords":["tui","pacman"],"LastModified":1784573962,"License":["MIT"],"Maintainer":"Firstpick","Name":"pacsea-bin","NumVotes":5,"OptDepends":["paru: 装包用"],"OutOfDate":null,"PackageBase":"pacsea-bin","Popularity":0.093482,"Provides":["pacsea"],"URL":"https://example.com","URLPath":"/cgit/aur.git/snapshot/pacsea-bin.tar.gz","Version":"0.8.2-2"}]}"#;
        let fields = parse_aur_one(json).expect("应能解析").info_fields();
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
    fn news_published_since_upgrade_counts_only_the_new_ones() {
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
            news_published_since(&items, Some(after_old)),
            1,
            "只有升级之后发布的那条算「新的」"
        );
        assert_eq!(
            news_published_since(&items, None),
            2,
            "不知道上次升级时间就全提醒"
        );
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

    /// 命令翻译：官方源只用 pacman，只有队列里真有 AUR 才换 paru。
    #[test]
    fn operations_translate_to_the_right_program() {
        let names = vec![String::from("fzf"), String::from("ripgrep")];

        assert_eq!(
            PackageOperation::Install.argv(&names),
            vec!["-S", "--needed", "fzf", "ripgrep"]
        );
        assert_eq!(PackageOperation::Install.program(false), "pacman");
        assert_eq!(PackageOperation::Install.program(true), "paru");
        assert_eq!(PackageOperation::Remove.program(true), "paru");
        assert_eq!(
            PackageOperation::Remove.argv(&names),
            vec!["-Rns", "fzf", "ripgrep"]
        );
        // 仅下载永远走 pacman：`paru -Sw` 对 AUR 的语义不是「下载源码」
        assert_eq!(PackageOperation::Download.program(true), "pacman");
        assert_eq!(
            PackageOperation::Download.argv(&names),
            vec!["-Sw", "--needed", "fzf", "ripgrep"]
        );
    }

    /// 预览的那条命令要能直接粘进 shell（该加引号的地方要加）。
    #[test]
    fn command_preview_is_pasteable() {
        let argv = vec![
            String::from("-S"),
            String::from("fzf"),
            String::from("weird name"),
            String::from("it's"),
        ];
        assert_eq!(
            command_preview("paru", &argv),
            // 注意这是**原始字符串**：命令预览里那个 `\` 是 POSIX 转义的一部分
            // （`'it'\''s'`），写成普通字符串会被 Rust 先吃掉一层。
            r"paru -S fzf 'weird name' 'it'\''s'"
        );
        assert_eq!(command_preview("pacman", &[]), "pacman");
        // 版本号里的 `-` `.` `:` 不该被引号包起来（预览给人看，越干净越好）
        assert_eq!(
            command_preview("pacman", &[String::from("0.74.4-1")]),
            "pacman 0.74.4-1"
        );
    }

    /// 清缓存：有 paccache 就温和，没有才退到 `pacman -Sc`。
    #[test]
    fn cache_command_prefers_paccache() {
        assert_eq!(
            cache_command(1, true),
            (String::from("paccache"), vec![String::from("-rk1")])
        );
        assert_eq!(
            cache_command(3, true),
            (String::from("paccache"), vec![String::from("-rk3")])
        );
        // 越界要夹住：`-rk0` 会把所有版本都删掉，不能让它出现
        assert_eq!(
            cache_command(0, true),
            (String::from("paccache"), vec![String::from("-rk1")])
        );
        assert_eq!(
            cache_command(1, false),
            (String::from("pacman"), vec![String::from("-Sc")])
        );
    }

    #[test]
    fn orphan_removal_builds_a_plain_pacman_command() {
        let (program, argv) = orphan_remove_command(&[String::from("a"), String::from("b")]);
        assert_eq!(program, "pacman");
        assert_eq!(argv, vec!["-Rns", "a", "b"]);
    }

    /// 已读新闻：落盘再读回来要一模一样。
    #[test]
    fn read_news_round_trips() {
        let path =
            std::env::temp_dir().join(format!("toolbox-hub-news-{}.log", std::process::id()));
        let mut read = BTreeSet::new();
        read.insert(String::from("https://a/2"));
        read.insert(String::from("https://a/1"));

        save_read_news(&path, &read).expect("写得进去");
        assert_eq!(load_read_news(&path), read, "读回来要一样");
        // 注释行不该被当成链接
        assert!(!load_read_news(&path).contains("# toolbox-hub 已读新闻（一行一个链接）"));

        let item = NewsItem {
            title: String::from("标题"),
            published: String::new(),
            link: String::from("https://a/1"),
            epoch: Some(1),
        };
        assert_eq!(news_key(&item), "https://a/1");
        assert!(NewsFilter::Read.matches(&item, &read));
        assert!(!NewsFilter::Unread.matches(&item, &read));

        // 没链接就退回标题（RSS 偶尔会缺 link）
        let bare = NewsItem {
            link: String::new(),
            ..item
        };
        assert_eq!(news_key(&bare), "标题");

        let _ = fs::remove_file(&path);
    }
}
