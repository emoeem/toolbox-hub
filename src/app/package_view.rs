//! 软件包中心的状态机（pacseek / pacsea 那一套的 Rust 版）。
//!
//! 三块屏共用一个视图：**搜索**（官方源 + AUR）、**已安装**（显式/依赖/外来/孤儿）、
//! **新闻**（未读/已读/全部）。数据由 [`crate::packages::probe`] 在后台线程里取，
//! 每帧 [`PackageView::poll`] 收一次结果 —— 和后台任务同一套模式（线程 + mpsc +
//! 每帧 poll），界面永远不卡。
//!
//! 一条硬规矩：**任何会改系统的动作都先经过 `confirm`（dry-run 预览）**。
//! 那个面板上显示的命令和真正跑的命令是同一个函数算出来的
//! （[`crate::packages::command_preview`]），不存在「看到的和跑的不一样」。

use super::TextInput;
use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
};

use crate::{
    config::PackagePrefs,
    packages::health::{self, HealthItem},
    packages::{
        self, InstalledFilter, InstalledPackage, NewsFilter, NewsItem, PackageHit, QueuedPackage,
        SortMode, fuzzy_score,
        worker::{Response, Worker},
    },
};

/// 队列要执行的操作（真身定义在 [`crate::packages`]，命令行模式也要用；
/// 这里再导一次是为了让 UI 那层能用 `package_view::PackageOperation` 这个老路径）。
pub use crate::packages::PackageOperation;

/// 结果区的行高上限：再多的行也滚不到底，但滚动位置要有个界。
const MAX_INFO_SCROLL: usize = 400;

/// 软件包中心的三个模式。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PackageMode {
    Search,
    Installed,
    News,
    /// 维护：孤儿包 / 依赖完整性 / .pacnew / 缓存 / 更新 / 上次升级。
    Health,
}

impl PackageMode {
    pub const ALL: [PackageMode; 4] = [Self::Search, Self::Installed, Self::News, Self::Health];

    pub fn label(self) -> &'static str {
        match self {
            Self::Search => "搜索",
            Self::Installed => "已安装",
            Self::News => "新闻",
            Self::Health => "维护",
        }
    }

    pub fn next(self) -> Self {
        match self {
            Self::Search => Self::Installed,
            Self::Installed => Self::News,
            Self::News => Self::Health,
            Self::Health => Self::Search,
        }
    }
}

/// 焦点在哪个面板（`Tab` 循环：结果 → 队列 → 包信息）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Pane {
    Rows,
    Queue,
    Info,
}

impl Pane {
    pub fn next(self) -> Self {
        match self {
            Self::Rows => Self::Queue,
            Self::Queue => Self::Info,
            Self::Info => Self::Rows,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Rows => "结果",
            Self::Queue => "队列",
            Self::Info => "包信息",
        }
    }
}

/// 仓库标签（带条数 —— pacseek 顶栏那种 `AUR 14 · core 42 …`）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RepoChip {
    pub name: String,
    pub enabled: bool,
    /// 当前搜索词在这个仓库里的命中数（**不受**开关影响，开关只管显示不显示）。
    pub count: usize,
}

/// 「要看清楚了再执行」的那个确认面板。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Confirm {
    pub title: String,
    /// 逐字等于马上要跑的那条命令。
    pub command: String,
    /// 补充说明与警告（依赖、AUR 编译、被依赖……）。
    pub notes: Vec<String>,
    pub action: ConfirmAction,
    /// 影响分析还在后台跑（面板上显示「分析中…」）。
    pub pending: bool,
}

/// 确认之后到底干什么。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConfirmAction {
    /// 用当前的 `operation` 执行队列。
    Queue(PackageOperation),
    Upgrade,
    /// 清缓存，保留 `keep` 个版本。
    Cache(u8),
    /// 卸载这些孤儿包。
    Orphans(Vec<String>),
}

pub struct PackageView {
    // ── 公共状态 ──
    pub mode: PackageMode,
    pub pane: Pane,
    /// 演练模式：确认后只把命令写进消息，不动系统（pacsea 的 `--dry-run`）。
    pub dry_run: bool,
    /// 队列的默认操作：安装 / 卸载 / 仅下载。
    pub operation: PackageOperation,
    pub message: String,

    // ── 搜索 ──
    pub query: TextInput,
    /// 搜索框是不是在输入状态（字母会进 query）。
    pub editing: bool,
    pub hits: Vec<PackageHit>,
    pub visible: Vec<usize>,
    pub repos: Vec<RepoChip>,
    pub sort: SortMode,
    /// 排序菜单开着时，高亮的是第几项。
    pub sort_menu: Option<usize>,
    pub selected: usize,
    pub searched: Option<String>,
    pub searching: bool,
    pub errors: Vec<String>,

    // ── 已安装 ──
    pub installed: Vec<InstalledPackage>,
    pub installed_visible: Vec<usize>,
    pub installed_filter: InstalledFilter,
    pub installed_selected: usize,
    pub installed_loading: bool,
    pub installed_loaded: bool,

    // ── 新闻 ──
    pub news: Vec<NewsItem>,
    pub news_visible: Vec<usize>,
    pub news_filter: NewsFilter,
    pub news_selected: usize,
    pub read_news: BTreeSet<String>,
    /// 有几条是「上次全系统升级之后发布的」（只是建议看一眼，不等于未读）。
    pub news_after_upgrade: Option<usize>,
    /// 那次升级的时间点；列表拿它给单条打 `★`。
    pub news_mark: Option<u64>,
    /// 有多少个包可以更新（libalpm 按本地同步库算）。
    pub pending_updates: Option<usize>,
    /// 同步库有多旧（秒）；超过一天就会在「待更新」旁边标出来。
    pub sync_age: Option<u64>,

    // ── 包信息 ──
    pub info: Option<(String, Vec<(String, String)>)>,
    pub info_error: Option<String>,
    /// 正在取的包名（避免每帧重复发起；UI 拿它显示「取包信息中…」）。
    pub info_pending: Option<String>,
    pub info_scroll: usize,

    // ── 安装队列 ──
    pub queue: Vec<QueuedPackage>,
    pub queue_selected: usize,

    // ── 确认面板 ──
    pub confirm: Option<Confirm>,

    // ── 维护 ──
    /// 一屏检查结果（进「维护」模式时拉一次）。
    pub health: Vec<HealthItem>,
    pub health_selected: usize,
    pub health_loading: bool,
    pub health_loaded: bool,
    /// 文件完整性检查在跑（那一步要几秒，界面上要说清楚）。
    pub health_checking_files: bool,
    /// 有明细要交给 App 打开输出视图（`(标题, 行)`）：由 `App::poll_packages` 取走。
    pub pending_view: Option<(String, Vec<String>)>,

    // ── 配置文件带来的偏好 ──
    /// `c` 清缓存时保留几个版本（`[` `]` 可以当场改这一次的）。
    pub cache_keep: u8,
    /// 默认只显示这些仓库（空 = 全看）。
    wanted_repos: Vec<String>,

    // ── 历史 ──
    pub history: Vec<String>,
    history_index: Option<usize>,

    // ── 在飞的请求 ──
    /// 常驻取数线程（libalpm 句柄 + HTTP 连接都活在里面，见 `packages::worker`）。
    worker: Option<Worker>,
    /// 整个同步库（浏览模式的基础：一进来就有东西看，像 paru 那样）。
    pub all_hits: Vec<PackageHit>,
    /// 全部包还在读。
    pub loading_all: bool,
    /// 官方源的命中（与 AUR 分开存：两路各回各的，谁先到谁先上屏）。
    official_hits: Vec<PackageHit>,
    aur_hits: Vec<PackageHit>,
    /// 还在等哪一路（官方源 / AUR）。
    awaiting_official: bool,
    awaiting_aur: bool,
}

impl PackageView {
    pub fn new(history: Vec<String>, worker: Option<Worker>, prefs: PackagePrefs) -> Self {
        let mut view = Self {
            mode: PackageMode::Search,
            pane: Pane::Rows,
            dry_run: prefs.dry_run,
            operation: PackageOperation::Install,
            message: String::new(),
            query: TextInput::new(),
            editing: true,
            hits: Vec::new(),
            visible: Vec::new(),
            repos: Vec::new(),
            sort: SortMode::Relevance,
            sort_menu: None,
            selected: 0,
            searched: None,
            searching: false,
            errors: Vec::new(),
            installed: Vec::new(),
            installed_visible: Vec::new(),
            installed_filter: InstalledFilter::All,
            installed_selected: 0,
            installed_loading: false,
            installed_loaded: false,
            news: Vec::new(),
            news_visible: Vec::new(),
            news_filter: NewsFilter::Unread,
            news_selected: 0,
            read_news: BTreeSet::new(),
            news_after_upgrade: None,
            news_mark: None,
            pending_updates: None,
            sync_age: None,
            info: None,
            info_error: None,
            info_pending: None,
            info_scroll: 0,
            queue: Vec::new(),
            queue_selected: 0,
            confirm: None,
            health: Vec::new(),
            health_selected: 0,
            health_loading: false,
            health_loaded: false,
            health_checking_files: false,
            pending_view: None,
            cache_keep: prefs.cache_keep(),
            wanted_repos: prefs.repos.clone(),
            history,
            history_index: None,
            worker,
            all_hits: Vec::new(),
            loading_all: false,
            official_hits: Vec::new(),
            aur_hits: Vec::new(),
            awaiting_official: false,
            awaiting_aur: false,
        };
        view.read_news = packages::load_read_news(&packages::read_news_path());
        view.sort = prefs.sort();
        view.wanted_repos = prefs.repos.clone();
        if let Some(mode) = prefs.mode() {
            view.mode = mode;
        }
        view
    }

    // ── 模式 ─────────────────────────────────────────────────────────────

    /// 切模式；该模式的数据没来过就顺手拉一次（懒加载）。
    pub fn set_mode(&mut self, mode: PackageMode) {
        if self.mode != mode {
            self.mode = mode;
            self.pane = Pane::Rows;
            self.confirm = None;
            self.message = format!("{} 模式", mode.label());
        }
        match mode {
            PackageMode::Search => self.ensure_all_packages(),
            PackageMode::Installed => self.ensure_installed(),
            PackageMode::News => self.ensure_news(),
            PackageMode::Health => self.ensure_health(),
        }
        // 输入框里的词跟着模式走：切过去就该按新列表重筛一次
        self.refilter();
    }

    pub fn cycle_mode(&mut self) {
        self.set_mode(self.mode.next());
    }

    /// 浏览模式：把整个同步库铺上（这是 paru 的第一屏：`< 38869/38869`）。
    ///
    /// 只拉一次：之后打字是**本地**过滤，Enter 才上网搜。
    pub fn ensure_all_packages(&mut self) {
        if self.loading_all || !self.all_hits.is_empty() {
            return;
        }
        let Some(worker) = self.worker.as_ref() else {
            // 没有取数线程时也要能说明白，而不是留一块空屏
            if self.message.is_empty() {
                self.message = String::from("取数线程没起来：只能按 Enter 上网搜，列不出全部包");
            }
            return;
        };
        self.loading_all = true;
        self.message = String::from("正在读全部包…");
        worker.all_packages();
    }

    fn ensure_installed(&mut self) {
        if self.installed_loaded || self.installed_loading {
            return;
        }
        self.start_installed();
    }

    /// 维护模式：每次进来都重扫一遍（这东西的价值就在于「现在是什么样」）。
    fn ensure_health(&mut self) {
        self.start_health();
    }

    fn ensure_news(&mut self) {
        if self.news.is_empty() {
            self.start_news();
        }
    }

    // ── 搜索 ─────────────────────────────────────────────────────────────

    /// 发起一次搜索：官方源与 AUR **各回各的**，谁先回来谁先上屏。
    ///
    /// 之前是「两路都回来才算数」，于是官方源 0.45 秒就有结果，你却要盯着
    /// 「搜索中…」等 AUR 那十几秒。现在官方源一到就显示，AUR 到了再补进来。
    pub fn start_search(&mut self) {
        let term = self.query.text().trim().to_string();
        if term.is_empty() {
            self.message = String::from("先填个搜索词");
            return;
        }
        let Some(worker) = self.worker.as_ref() else {
            self.message = String::from("取数线程没起来，搜不了");
            return;
        };
        if self.searching {
            return;
        }

        packages::remember_search(&mut self.history, &term);
        let _ = packages::save_searches_to(&packages::searches_path(), &self.history);

        self.searching = true;
        self.awaiting_official = true;
        self.awaiting_aur = true;
        self.searched = Some(term.clone());
        self.editing = false;
        self.history_index = None;
        self.confirm = None;
        self.errors.clear();
        self.info = None;
        self.info_pending = None;
        self.official_hits.clear();
        self.aur_hits.clear();
        self.hits.clear();
        self.visible.clear();
        self.selected = 0;
        self.message = format!("搜「{term}」…（官方源 + AUR 两路并行，先到的先显示）");

        worker.search(&term);
    }

    /// 已安装包浏览器：`pacman -Q/-Qe/-Qm/-Qtd`（都是本地查询，很快）。
    pub fn start_installed(&mut self) {
        if self.installed_loading {
            return;
        }
        let Some(worker) = self.worker.as_ref() else {
            self.message = String::from("取数线程没起来，读不了已安装列表");
            return;
        };
        self.installed_loading = true;
        self.message = String::from("正在读已安装的包…");
        worker.installed();
    }

    /// 新闻 + 未读判断 + 「有几个包能更新」（`Ctrl+N`，进新闻模式也会自动拉一次）。
    ///
    /// 可更新数与新闻是**两个请求**：前者走数据库线程（毫秒级），后者走网络。
    /// 以前它俩串在同一个线程里，而「可更新」那一步是 18 秒的 `checkupdates` ——
    /// 新闻就被它堵在后面。
    pub fn start_news(&mut self) {
        let Some(worker) = self.worker.as_ref() else {
            self.message = String::from("取数线程没起来，看不了新闻");
            return;
        };
        self.read_news = packages::load_read_news(&packages::read_news_path());
        self.message = String::from("正在看 Arch 新闻…");
        worker.news();
        worker.updates();
    }

    /// 扫一遍维护检查。
    pub fn start_health(&mut self) {
        let Some(worker) = self.worker.as_ref() else {
            self.message = String::from("取数线程没起来，扫不了");
            return;
        };
        self.health_loading = true;
        self.message = String::from("正在检查系统状态…");
        worker.health();
    }

    /// 文件完整性（几秒，用户按了才跑）。
    pub fn start_file_integrity(&mut self) {
        let Some(worker) = self.worker.as_ref() else {
            return;
        };
        self.health_checking_files = true;
        self.message = String::from("正在查文件完整性（pacman -Qk，要几秒）…");
        worker.file_integrity();
    }

    /// 每帧把两个常驻线程攒下的回答取干净。
    ///
    /// 以前这里有六个接收器、六段几乎一样的 `try_recv` 样板；现在只有一个
    /// [`Response`]，分发集中在一处 —— 加一种取数只需动 `worker` 和这里各一行。
    ///
    /// 返回值是「这一帧有东西变了」：主循环据此决定要不要重画（见 `main::run`）。
    pub fn poll(&mut self) -> bool {
        // 先把回答收干净再处理：`try_recv` 借着 worker（也就是 self），
        // 而 `handle` 要改 self —— 不先收完就是借用冲突。
        let mut responses = Vec::new();
        if let Some(worker) = self.worker.as_ref() {
            while let Some(response) = worker.try_recv() {
                responses.push(response);
            }
        }
        let handled = !responses.is_empty();
        for response in responses {
            self.handle(response);
        }
        let asked = self.follow_selection();
        handled || asked
    }

    fn handle(&mut self, response: Response) {
        match response {
            Response::AllPackages(hits) => {
                self.loading_all = false;
                self.all_hits = hits;
                // 用户已经开始搜了就别拿全库覆盖搜索结果
                if self.searched.is_none() {
                    self.rebuild_hits();
                }
            }
            Response::Official(hits) => {
                self.awaiting_official = false;
                self.official_hits = hits;
                self.rebuild_hits();
            }
            Response::Aur(hits) => {
                self.awaiting_aur = false;
                self.aur_hits = hits;
                self.rebuild_hits();
            }
            Response::Failed { source, error } => {
                if source == "官方源" {
                    self.awaiting_official = false;
                } else {
                    self.awaiting_aur = false;
                }
                self.errors.push(format!("{source}：{error}"));
                self.rebuild_hits();
            }
            Response::Info(outcome) => {
                self.info_pending = None;
                self.info_scroll = 0;
                if let Some(error) = outcome.error {
                    self.info_error = Some(error);
                    self.info = Some((outcome.name, Vec::new()));
                } else {
                    self.info_error = None;
                    self.info = Some((outcome.name, outcome.fields));
                }
            }
            Response::Installed(Ok(packages)) => {
                self.installed_loading = false;
                self.installed_loaded = true;
                let orphans = packages.iter().filter(|item| item.orphan).count();
                let foreign = packages.iter().filter(|item| item.foreign).count();
                self.message = format!(
                    "{} 个已安装 · 外来 {foreign} · 孤儿 {orphans}",
                    packages.len()
                );
                self.installed = packages;
                self.installed_selected = 0;
                self.apply_installed_filter();
            }
            Response::Installed(Err(error)) => {
                self.installed_loading = false;
                self.message = format!("读已安装包失败：{error}");
            }
            Response::Updates { count, age } => {
                self.pending_updates = count;
                self.sync_age = age;
            }
            Response::Removal(notes) => self.append_confirm_notes(notes),
            Response::DownloadTotal(bytes) => {
                let note = bytes.map(|bytes| {
                    format!(
                        "要下载 {}（AUR 包的体积要等编译时才知道）",
                        packages::libalpm::size_text(bytes)
                    )
                });
                self.append_confirm_notes(note.into_iter().collect());
            }
            Response::Health(items) => {
                self.health_loading = false;
                self.health_loaded = true;
                let (bad, warn) = health::tally(&items);
                self.message = if bad + warn == 0 {
                    String::from("维护检查：一切正常")
                } else {
                    format!("维护检查：{bad} 项待处理 · {warn} 项注意")
                };
                self.health = items;
                if self.health_selected >= self.health.len() {
                    self.health_selected = self.health.len().saturating_sub(1);
                }
            }
            Response::FileIntegrity(lines) => {
                self.health_checking_files = false;
                self.message = format!("文件完整性：{} 行输出", lines.len());
                self.pending_view = Some((String::from("pacman -Qk"), lines));
            }
            Response::OrphanNames(names) => {
                if names.is_empty() {
                    self.message = String::from("没有孤儿包，系统很干净");
                    return;
                }
                let (program, argv) = packages::orphan_remove_command(&names);
                let (program, argv) = packages::escalate(&program, &argv);
                self.confirm = Some(Confirm {
                    title: format!("卸载 {} 个孤儿包", names.len()),
                    command: packages::command_preview(&program, &argv),
                    notes: vec![
                        String::from("孤儿 = 没人依赖、你也没点名装过"),
                        names.join("  "),
                    ],
                    action: ConfirmAction::Orphans(names),
                    pending: false,
                });
            }
            Response::News(Ok(chunk)) => {
                self.news_after_upgrade = Some(chunk.after_upgrade);
                self.news_mark = chunk.mark;
                let unread = chunk
                    .items
                    .iter()
                    .filter(|item| !self.read_news.contains(&packages::news_key(item)))
                    .count();
                self.message = format!(
                    "Arch 新闻 {} 条 · 未读 {unread} · 升级后发布 {}",
                    chunk.items.len(),
                    chunk.after_upgrade
                );
                self.news = chunk.items;
                self.rebuild_news();
            }
            Response::News(Err(error)) => {
                self.message = format!("抓新闻失败：{error}");
            }
        }
    }

    /// 让信息面板跟上当前选中的包。
    ///
    /// 两个数据源分工明确：搜索模式查**仓库里**的包（官方源走 libalpm、AUR 走
    /// RPC），已安装模式查**本地**的那份 —— 外来包在同步库里根本不存在。
    fn follow_selection(&mut self) -> bool {
        if self.info_pending.is_some() {
            return false;
        }
        // 先把「要查谁」定下来（这一段只借 self 的不可变引用），再去发请求，
        // 免得「借 worker」和「改 info_pending」同时要 self。
        enum Target {
            Repo(PackageHit),
            Local(String),
        }
        let (name, target) = match self.mode {
            PackageMode::Search => match self.selected_hit() {
                Some(hit) => (hit.name.clone(), Target::Repo(hit.clone())),
                None => return false,
            },
            PackageMode::Installed => match self.selected_installed() {
                Some(package) => (package.name.clone(), Target::Local(package.name.clone())),
                None => return false,
            },
            PackageMode::News | PackageMode::Health => return false,
        };

        if self.info.as_ref().map(|(shown, _)| shown.as_str()) == Some(name.as_str()) {
            return false;
        }
        self.info_pending = Some(name.clone());

        let Some(worker) = self.worker.as_ref() else {
            return false;
        };
        let hit = match target {
            Target::Repo(hit) => hit,
            // 本地包用 仓库=local 的壳：worker 对非 AUR 一律走 libalpm，
            // 同步库里没有就自动落到本地库（外来包也能看信息）。
            Target::Local(name) => PackageHit {
                repo: String::from("local"),
                name,
                version: String::new(),
                description: String::new(),
                installed_state: crate::packages::InstalledState::NotInstalled,
                votes: None,
                popularity: None,
                maintainer: None,
                out_of_date: false,
            },
        };
        worker.info(&hit);
        true
    }

    /// 两路结果合流：排序 + 去重 + 重建筛选，尽量保住当前选中的那个包。
    ///
    /// 「保住选中」很重要：AUR 那一批晚几秒到，如果不保，你刚用 ↑↓ 选中的行会
    /// 在结果补进来的一瞬间跳走。
    fn rebuild_hits(&mut self) {
        let keep = self.selected_hit().map(|hit| hit.name.clone());

        // 浏览模式（还没按过 Enter）：铺全库；搜过之后：铺搜索结果
        let browsing = self.searched.is_none();
        let mut hits: Vec<PackageHit> = if browsing {
            self.all_hits.clone()
        } else {
            self.official_hits
                .iter()
                .chain(self.aur_hits.iter())
                .cloned()
                .collect()
        };

        let needle = self
            .searched
            .clone()
            .unwrap_or_default()
            .trim()
            .trim_start_matches('^')
            .trim_end_matches('$')
            .to_string();
        if browsing {
            // 浏览全库时按名字排（搜索那种「相关的排前」在全库上没有意义）
            hits.sort_by(|a, b| a.name.cmp(&b.name).then(a.repo.cmp(&b.repo)));
        } else {
            hits.sort_by(|a, b| {
                let exact = |hit: &PackageHit| hit.name.eq_ignore_ascii_case(&needle) as u8;
                b.is_installed()
                    .cmp(&a.is_installed())
                    .then(exact(b).cmp(&exact(a)))
                    .then(b.votes.unwrap_or(0).cmp(&a.votes.unwrap_or(0)))
                    .then(a.repo.cmp(&b.repo))
                    .then(a.name.cmp(&b.name))
            });
        }
        hits.dedup_by(|a, b| a.name == b.name && a.version == b.version);
        self.hits = hits;

        self.rebuild_repos();
        self.apply_filter();

        if let Some(name) = keep
            && let Some(row) = self
                .visible
                .iter()
                .position(|&index| self.hits[index].name == name)
        {
            self.selected = row;
        }

        // 两路都回来了才算搜完
        self.searching = !browsing && (self.awaiting_official || self.awaiting_aur);
        self.message = if browsing {
            if self.loading_all {
                String::from("正在读全部包…")
            } else {
                format!("全部 {} 个包", self.all_hits.len())
            }
        } else if self.searching {
            let waiting = match (self.awaiting_official, self.awaiting_aur) {
                (true, true) => "官方源 + AUR",
                (true, false) => "官方源",
                _ => "AUR",
            };
            format!("{} 个结果，还在等 {waiting}…", self.hits.len())
        } else if self.hits.is_empty() {
            String::from("没有匹配的包")
        } else {
            format!("{} 个结果", self.hits.len())
        };
    }

    /// 确认面板的补充信息到了就接上（体积 / 谁依赖它们）。
    fn append_confirm_notes(&mut self, notes: Vec<String>) {
        if let Some(confirm) = self.confirm.as_mut() {
            confirm.pending = false;
            confirm.notes.extend(notes);
        }
    }

    // ── 结果与筛选 ───────────────────────────────────────────────────────

    /// 结果里出现过哪些仓库（筛选用），带命中数。
    fn rebuild_repos(&mut self) {
        let previous: Vec<RepoChip> = self.repos.clone();
        let mut repos: Vec<RepoChip> = Vec::new();
        for hit in &self.hits {
            if let Some(chip) = repos.iter_mut().find(|chip| chip.name == hit.repo) {
                chip.count += 1;
                continue;
            }
            // 之前关掉的仓库保持关着（刷新结果不该把筛选重置）；
            // 第一次见到的仓库看配置：`packages.toml` 里列了名单就只开名单里的。
            let enabled = previous
                .iter()
                .find(|chip| chip.name == hit.repo)
                .map(|chip| chip.enabled)
                .unwrap_or_else(|| {
                    self.wanted_repos.is_empty() || self.wanted_repos.contains(&hit.repo)
                });
            repos.push(RepoChip {
                name: hit.repo.clone(),
                enabled,
                count: 1,
            });
        }
        // AUR 排最后（官方源的包更常用）
        repos.sort_by(|a, b| {
            (a.name == "aur")
                .cmp(&(b.name == "aur"))
                .then(a.name.cmp(&b.name))
        });
        self.repos = repos;
    }

    /// 重新算可见列表：**仓库标签 + 本地模糊过滤 + 排序**。
    ///
    /// 搜索词同时干两件事：`Enter` 拿去问官方源与 AUR（远端），打字则**本地**
    /// 模糊过滤已有结果 —— 这就是 pac（fzf 那一层）的手感：键入即筛、回车才上网找。
    pub fn apply_filter(&mut self) {
        let needle = self.query.text().trim().to_string();
        let mut rows: Vec<(usize, i32)> = Vec::new();

        for (index, hit) in self.hits.iter().enumerate() {
            let repo_on = self
                .repos
                .iter()
                .find(|chip| chip.name == hit.repo)
                .map(|chip| chip.enabled)
                .unwrap_or(true);
            if !repo_on {
                continue;
            }

            if needle.is_empty() {
                rows.push((index, 0));
                continue;
            }

            // 名字优先，其次描述与仓库名（各降一档）
            let mut best = fuzzy_score(&needle, &hit.name);
            if let Some(score) = fuzzy_score(&needle, &hit.description) {
                let score = score - 2;
                best = Some(best.map_or(score, |current: i32| current.max(score)));
            }
            if let Some(score) = fuzzy_score(&needle, &hit.repo) {
                let score = score - 4;
                best = Some(best.map_or(score, |current: i32| current.max(score)));
            }
            if let Some(score) = best {
                rows.push((index, score));
            }
        }

        match self.sort {
            SortMode::Relevance if !needle.is_empty() => {
                rows.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
            }
            SortMode::Relevance => {}
            SortMode::Name => rows.sort_by(|a, b| {
                self.hits[a.0]
                    .name
                    .cmp(&self.hits[b.0].name)
                    .then(a.0.cmp(&b.0))
            }),
            SortMode::Repo => rows.sort_by(|a, b| {
                self.hits[a.0]
                    .repo
                    .cmp(&self.hits[b.0].repo)
                    .then(self.hits[a.0].name.cmp(&self.hits[b.0].name))
            }),
            SortMode::Votes => rows.sort_by(|a, b| {
                self.hits[b.0]
                    .votes
                    .unwrap_or(0)
                    .cmp(&self.hits[a.0].votes.unwrap_or(0))
                    .then(a.0.cmp(&b.0))
            }),
            SortMode::Version => rows.sort_by(|a, b| {
                self.hits[a.0]
                    .version
                    .cmp(&self.hits[b.0].version)
                    .then(a.0.cmp(&b.0))
            }),
        }

        self.visible = rows.into_iter().map(|(index, _)| index).collect();
        if self.selected >= self.visible.len() {
            self.selected = self.visible.len().saturating_sub(1);
        }
    }

    /// 已安装列表的筛选 + 排序。
    ///
    /// 搜索框里的词在这里也生效：三种模式共用同一个输入框，
    /// 「输入即筛」在哪个模式下都得成立，不然切过去就得先把词删掉。
    pub fn apply_installed_filter(&mut self) {
        let needle = self.query.text().trim().to_string();
        let mut rows: Vec<usize> = self
            .installed
            .iter()
            .enumerate()
            .filter(|(_, package)| self.installed_filter.matches(package))
            .filter(|(_, package)| {
                needle.is_empty() || fuzzy_score(&needle, &package.name).is_some()
            })
            .map(|(index, _)| index)
            .collect();

        match self.sort {
            SortMode::Name | SortMode::Relevance => {
                rows.sort_by(|a, b| self.installed[*a].name.cmp(&self.installed[*b].name))
            }
            SortMode::Version => rows.sort_by(|a, b| {
                self.installed[*a]
                    .version
                    .cmp(&self.installed[*b].version)
                    .then(self.installed[*a].name.cmp(&self.installed[*b].name))
            }),
            SortMode::Repo | SortMode::Votes => rows.sort_by(|a, b| {
                self.installed[*a]
                    .tag()
                    .cmp(self.installed[*b].tag())
                    .then(self.installed[*a].name.cmp(&self.installed[*b].name))
            }),
        }

        self.installed_visible = rows;
        if self.installed_selected >= self.installed_visible.len() {
            self.installed_selected = self.installed_visible.len().saturating_sub(1);
        }
    }

    /// 新闻列表重算（按发布时间倒序 —— 新的一直在最上面）。
    ///
    /// 和已安装列表一样，搜索框里的词在这里也是**本地过滤**（标题命中即可）。
    pub fn rebuild_news(&mut self) {
        let needle = self.query.text().trim().to_string();
        let mut rows: Vec<usize> = (0..self.news.len())
            .filter(|&index| self.news_filter.matches(&self.news[index], &self.read_news))
            .filter(|&index| {
                needle.is_empty() || fuzzy_score(&needle, &self.news[index].title).is_some()
            })
            .collect();
        rows.sort_by(|a, b| self.news[*b].epoch.cmp(&self.news[*a].epoch).then(a.cmp(b)));
        self.news_visible = rows;
        if self.news_selected >= self.news_visible.len() {
            self.news_selected = self.news_visible.len().saturating_sub(1);
        }
    }

    /// 搜索框里的词变了：按**当前模式**重算可见列表。
    ///
    /// 三种模式共用一个输入框，所以不能在每个按键处各写一遍分发 —— 那是重复，
    /// 也是「某个模式忘了刷新」的来源。
    pub fn refilter(&mut self) {
        match self.mode {
            PackageMode::Search => self.apply_filter(),
            PackageMode::Installed => self.apply_installed_filter(),
            PackageMode::News => self.rebuild_news(),
            // 维护面板是系统状态，不是能筛的列表
            PackageMode::Health => {}
        }
    }

    /// 当前模式能用的排序方式（搜索与已安装看的东西不一样）。
    pub fn available_sorts(&self) -> Vec<SortMode> {
        match self.mode {
            PackageMode::Search => vec![
                SortMode::Relevance,
                SortMode::Name,
                SortMode::Repo,
                SortMode::Votes,
            ],
            PackageMode::Installed => vec![SortMode::Name, SortMode::Version, SortMode::Repo],
            // 新闻固定按时间；维护面板的顺序是「要紧的在前」，都不给排序菜单
            PackageMode::News | PackageMode::Health => Vec::new(),
        }
    }

    /// `s`：打开排序菜单（pacseek 顶栏那个 `Sort v`）。
    pub fn open_sort_menu(&mut self) {
        let sorts = self.available_sorts();
        if sorts.is_empty() {
            self.message = String::from("新闻固定按时间倒序");
            return;
        }
        let current = sorts
            .iter()
            .position(|mode| *mode == self.sort)
            .unwrap_or(0);
        self.sort_menu = Some(current);
        self.message = String::from("排序：↑↓ 选 · Enter 确定 · Esc 取消");
    }

    /// 排序菜单里上下移动。
    pub fn sort_menu_step(&mut self, delta: isize) {
        let len = self.available_sorts().len() as isize;
        if len == 0 {
            return;
        }
        let Some(current) = self.sort_menu else {
            return;
        };
        self.sort_menu = Some((current as isize + delta).rem_euclid(len) as usize);
    }

    /// 排序菜单里按 Enter：落实选择。
    pub fn apply_sort_menu(&mut self) {
        let sorts = self.available_sorts();
        if let Some(index) = self.sort_menu.take()
            && let Some(mode) = sorts.get(index).copied()
        {
            self.sort = mode;
            self.message = format!("排序：{}", mode.label());
            match self.mode {
                PackageMode::Installed => self.apply_installed_filter(),
                PackageMode::News => self.rebuild_news(),
                PackageMode::Search => self.apply_filter(),
                PackageMode::Health => {}
            }
        }
    }

    pub fn close_sort_menu(&mut self) {
        if self.sort_menu.take().is_some() {
            self.message = String::from("取消排序");
        }
    }

    /// 切换队列的执行方式：安装 → 卸载 → 仅下载。
    pub fn cycle_operation(&mut self) {
        self.operation = self.operation.next();
        self.confirm = None;
        self.message = format!("队列操作：{}", self.operation.label());
    }

    // ── 各模式的可见行 ───────────────────────────────────────────────────

    /// 结果区当前有多少行（跟着模式走）。
    pub fn rows_len(&self) -> usize {
        match self.mode {
            PackageMode::Search => self.visible.len(),
            PackageMode::Installed => self.installed_visible.len(),
            PackageMode::News => self.news_visible.len(),
            PackageMode::Health => self.health.len(),
        }
    }

    pub fn rows_selected(&self) -> usize {
        match self.mode {
            PackageMode::Search => self.selected,
            PackageMode::Installed => self.installed_selected,
            PackageMode::News => self.news_selected,
            PackageMode::Health => self.health_selected,
        }
    }

    pub fn visible_hit(&self, row: usize) -> Option<&PackageHit> {
        self.visible
            .get(row)
            .and_then(|&index| self.hits.get(index))
    }

    pub fn selected_hit(&self) -> Option<&PackageHit> {
        self.visible_hit(self.selected)
    }

    pub fn installed_hit(&self, row: usize) -> Option<&InstalledPackage> {
        self.installed_visible
            .get(row)
            .and_then(|&index| self.installed.get(index))
    }

    pub fn selected_installed(&self) -> Option<&InstalledPackage> {
        self.installed_hit(self.installed_selected)
    }

    pub fn news_hit(&self, row: usize) -> Option<&NewsItem> {
        self.news_visible
            .get(row)
            .and_then(|&index| self.news.get(index))
    }

    pub fn selected_news(&self) -> Option<&NewsItem> {
        self.news_hit(self.news_selected)
    }

    /// 信息面板正在显示（或正要显示）的包名。
    pub fn info_title(&self) -> Option<String> {
        self.info
            .as_ref()
            .map(|(name, _)| name.clone())
            .or_else(|| self.info_pending.clone())
    }

    /// 切换某个筛选标签：搜索模式是仓库开关，已安装是四类筛选，新闻是已读筛选。
    pub fn toggle_chip(&mut self, index: usize) {
        match self.mode {
            PackageMode::Search => {
                if let Some(chip) = self.repos.get_mut(index) {
                    chip.enabled = !chip.enabled;
                    let (name, enabled) = (chip.name.clone(), chip.enabled);
                    self.message = format!("{} {}", if enabled { "显示" } else { "隐藏" }, name);
                    self.apply_filter();
                }
            }
            PackageMode::Installed => {
                if let Some(filter) = InstalledFilter::ALL.get(index).copied() {
                    self.installed_filter = filter;
                    self.installed_selected = 0;
                    self.message = format!("只看：{}", filter.label());
                    self.apply_installed_filter();
                }
            }
            PackageMode::News => {
                if let Some(filter) = NewsFilter::ALL.get(index).copied() {
                    self.news_filter = filter;
                    self.news_selected = 0;
                    self.message = format!("新闻：{}", filter.label());
                    self.rebuild_news();
                }
            }
            PackageMode::Health => {}
        }
    }

    /// 全部打开（数字键 `0`）。
    pub fn enable_all_chips(&mut self) {
        match self.mode {
            PackageMode::Search => {
                for chip in self.repos.iter_mut() {
                    chip.enabled = true;
                }
                self.apply_filter();
                self.message = String::from("仓库标签全开");
            }
            PackageMode::Installed => {
                self.installed_filter = InstalledFilter::All;
                self.apply_installed_filter();
            }
            PackageMode::News => {
                self.news_filter = NewsFilter::All;
                self.rebuild_news();
            }
            PackageMode::Health => {}
        }
    }

    /// 搜索框里按 ↑↓：拿历史里的词填进来（最新的在上）。
    pub fn history_step(&mut self, delta: isize) {
        if self.history.is_empty() {
            return;
        }
        let len = self.history.len() as isize;
        let next = match self.history_index {
            Some(index) => (index as isize + delta).clamp(0, len - 1) as usize,
            None => {
                if delta < 0 {
                    0
                } else {
                    len as usize - 1
                }
            }
        };
        self.history_index = Some(next);
        self.query.set(&self.history[next]);
    }

    /// 结果区上下移动（跟着模式走）。
    pub fn move_row(&mut self, delta: isize) {
        let len = self.rows_len() as isize;
        if len == 0 {
            return;
        }
        let current = self.rows_selected() as isize;
        let next = (current + delta).rem_euclid(len) as usize;
        match self.mode {
            PackageMode::Search => self.selected = next,
            PackageMode::Installed => self.installed_selected = next,
            PackageMode::News => self.news_selected = next,
            PackageMode::Health => self.health_selected = next,
        }
    }

    /// 队列上下移动。
    pub fn move_queue(&mut self, delta: isize) {
        let len = self.queue.len() as isize;
        if len == 0 {
            self.queue_selected = 0;
            return;
        }
        self.queue_selected = (self.queue_selected as isize + delta).rem_euclid(len) as usize;
    }

    /// 兼容老名字：默认在结果区上移动。
    pub fn move_selection(&mut self, delta: isize) {
        match self.pane {
            Pane::Queue => self.move_queue(delta),
            Pane::Info => self.scroll_info(delta),
            Pane::Rows => self.move_row(delta),
        }
    }

    /// 包信息面板滚动。
    pub fn scroll_info(&mut self, delta: isize) {
        let next = self.info_scroll as isize + delta;
        self.info_scroll = next.clamp(0, MAX_INFO_SCROLL as isize) as usize;
    }

    /// 焦点在结果 / 队列 / 包信息之间循环。
    pub fn toggle_focus(&mut self) {
        self.pane = self.pane.next();
        self.message = format!("焦点：{}", self.pane.label());
    }

    // ── 安装队列 ─────────────────────────────────────────────────────────

    /// `Space`：把选中的包加进队列，或者（焦点在队列时）移出去。
    pub fn toggle_queue(&mut self) {
        if self.pane == Pane::Queue {
            if self.queue_selected < self.queue.len() {
                let removed = self.queue.remove(self.queue_selected);
                self.message = format!("从队列移除 {}", removed.name);
                if self.queue_selected >= self.queue.len() {
                    self.queue_selected = self.queue.len().saturating_sub(1);
                }
            }
            return;
        }

        // 已安装模式：排队是为了**卸载**
        if self.mode == PackageMode::Installed {
            let Some(package) = self.selected_installed().cloned() else {
                return;
            };
            if let Some(position) = self.queue.iter().position(|item| item.name == package.name) {
                self.queue.remove(position);
                self.message = format!("从队列移除 {}", package.name);
                return;
            }
            self.queue.push(QueuedPackage {
                name: package.name.clone(),
                origin: package.tag().to_string(),
                version: package.version.clone(),
            });
            // 排队卸载时自动把队列操作切到卸载，省得还要按 m 转一圈
            if self.operation != PackageOperation::Remove {
                self.operation = PackageOperation::Remove;
            }
            self.message = format!("已加入卸载队列：{}（Enter 看命令）", package.name);
            return;
        }

        let Some(hit) = self.selected_hit().cloned() else {
            return;
        };
        if let Some(position) = self.queue.iter().position(|item| item.name == hit.name) {
            self.queue.remove(position);
            self.message = format!("从队列移除 {}", hit.name);
            return;
        }
        self.queue.push(QueuedPackage::new(&hit));
        self.message = format!(
            "已加入队列：{} {}（Space 再加，Enter 看命令）",
            hit.name,
            if hit.is_aur() { "AUR" } else { &hit.repo }
        );
    }

    /// 队列里的包名（拼命令用）。
    pub fn queue_names(&self) -> Vec<String> {
        self.queue.iter().map(|item| item.name.clone()).collect()
    }

    /// 队列里有几个 AUR 包。
    pub fn queue_aur_count(&self) -> usize {
        self.queue
            .iter()
            .filter(|item| item.origin == "aur")
            .count()
    }

    /// 排队的包里有 AUR 的吗（安装时用来说明「要编译」）。
    pub fn queue_has_aur(&self) -> bool {
        self.queue_aur_count() > 0
    }

    /// 清空队列（清之前也走一次确认 —— 队列可能是你攒了很久的）。
    pub fn clear_queue(&mut self) {
        self.queue.clear();
        self.queue_selected = 0;
        self.confirm = None;
        self.message = String::from("队列已清空");
    }

    pub fn export_queue(&mut self, path: &Path) {
        match packages::save_queue_to(path, &self.queue) {
            Ok(()) => {
                self.message = format!("{} 个包已写出到 {}", self.queue.len(), path.display())
            }
            Err(error) => self.message = format!("导出失败：{error}"),
        }
    }

    pub fn import_queue(&mut self, path: &Path) {
        let items = packages::load_queue_from(path);
        if items.is_empty() {
            self.message = format!("{} 里没有可导入的包", path.display());
            return;
        }
        let count = items.len();
        for item in items {
            if !self.queue.iter().any(|existing| existing.name == item.name) {
                self.queue.push(item);
            }
        }
        self.message = format!("从 {} 导入 {count} 个", path.display());
    }

    // ── 确认（dry-run 预览）───────────────────────────────────────────────

    /// 执行队列前先摆出命令与影响（再按一次 Enter 才真跑）。
    pub fn arm_queue(&mut self) {
        if self.queue.is_empty() {
            self.message = String::from("队列是空的：Space 把包加进来");
            return;
        }
        let names = self.queue_names();
        let uses_aur = self.queue_has_aur();
        // 该提权就提权，而且**在预览里就体现出来** —— 看到的就是跑的
        let (program, argv) = packages::escalate(
            self.operation.program(uses_aur),
            &self.operation.argv(&names),
        );

        let mut notes = vec![self.operation.detail().to_string()];
        let aur = self.queue_aur_count();
        if aur > 0 {
            notes.push(format!(
                "{aur} 个 AUR 包会现场编译（要等一会儿；编译完的包留在 pacman 缓存里）"
            ));
        }
        let operation = self.operation;
        self.confirm = Some(Confirm {
            title: format!("{} {} 个包", self.operation.label(), names.len()),
            command: packages::command_preview(&program, &argv),
            notes,
            action: ConfirmAction::Queue(operation),
            // 补充信息（体积 / 谁依赖它们）马上在后台算，面板先出来
            pending: true,
        });

        if let Some(worker) = self.worker.as_ref() {
            match operation {
                PackageOperation::Remove => worker.removal(names),
                _ => worker.download_total(names),
            }
        }
    }

    /// 系统更新的确认面板。
    pub fn arm_upgrade(&mut self) {
        let program = if packages::probe::has_paru() {
            "paru"
        } else {
            "pacman"
        };
        let (program, argv) = packages::escalate(program, &[String::from(packages::UPGRADE_FLAG)]);
        self.confirm = Some(Confirm {
            title: String::from("系统更新"),
            command: packages::command_preview(&program, &argv),
            notes: vec![
                String::from("同步仓库后升级所有包（含 AUR，若装了 paru）"),
                String::from("改的是整个系统，升完最好看一眼 Arch 新闻"),
            ],
            action: ConfirmAction::Upgrade,
            pending: true,
        });

        // 实时数一遍（走数据库线程；界面不等它）
        if let Some(worker) = self.worker.as_ref() {
            worker.updates();
        }
    }

    /// 清缓存的确认面板。
    pub fn arm_cache(&mut self, keep: u8) {
        let (program, argv) = packages::cache_command(keep, packages::probe::has_paccache());
        let (program, argv) = packages::escalate(&program, &argv);
        let mut notes = vec![format!("只保留每个包最近 {keep} 个版本，更旧的从缓存删掉")];
        if program.ends_with("pacman") {
            notes.push(String::from(
                "没装 paccache（pacman-contrib），退回 `pacman -Sc`：它会清掉仓库里已经没有的包",
            ));
        }
        self.confirm = Some(Confirm {
            title: String::from("清包缓存"),
            command: packages::command_preview(&program, &argv),
            notes,
            action: ConfirmAction::Cache(keep),
            pending: false,
        });
    }

    /// 清孤儿的确认面板：名单现查（libalpm 的孤儿判定，见 [`packages::libalpm`]）。
    pub fn arm_orphans(&mut self) {
        let Some(worker) = self.worker.as_ref() else {
            self.message = String::from("取数线程没起来");
            return;
        };
        self.message = String::from("正在找孤儿包…");
        worker.orphan_names();
    }

    pub fn cancel_confirm(&mut self) {
        if self.confirm.take().is_some() {
            self.message = String::from("已取消");
        }
    }

    /// 取出确认面板（执行者用完就没了）。
    pub fn take_confirm(&mut self) -> Option<Confirm> {
        self.confirm.take()
    }

    pub fn toggle_dry_run(&mut self) {
        self.dry_run = !self.dry_run;
        self.message = if self.dry_run {
            String::from("演练模式：确认后只显示命令，不动系统")
        } else {
            String::from("演练模式已关：确认后真的执行")
        };
    }

    // ── 新闻已读 ─────────────────────────────────────────────────────────

    /// `r`：把选中的新闻标记为已读 / 未读。
    pub fn toggle_news_read(&mut self) {
        let Some(item) = self.selected_news().cloned() else {
            return;
        };
        let key = packages::news_key(&item);
        let now_read = if self.read_news.remove(&key) {
            false
        } else {
            self.read_news.insert(key);
            true
        };
        match packages::save_read_news(&packages::read_news_path(), &self.read_news) {
            Ok(()) => {
                self.message = format!(
                    "{}：{}",
                    if now_read {
                        "标记已读"
                    } else {
                        "标记未读"
                    },
                    item.title
                );
            }
            Err(error) => self.message = format!("已读状态没写进去：{error}"),
        }
        self.rebuild_news();
    }

    /// `R`：把当前列表里看到的都标记为已读。
    pub fn mark_visible_news_read(&mut self) {
        if self.news_visible.is_empty() {
            return;
        }
        let keys: Vec<String> = self
            .news_visible
            .iter()
            .map(|&index| packages::news_key(&self.news[index]))
            .collect();
        let count = keys.len();
        self.read_news.extend(keys);
        let _ = packages::save_read_news(&packages::read_news_path(), &self.read_news);
        self.message = format!("{count} 条标记为已读");
        self.rebuild_news();
    }

    /// 同步库的一句话（超过一天才值得说）。
    pub fn sync_age_note(&self) -> Option<String> {
        let seconds = self.sync_age?;
        (seconds > 24 * 3600).then(|| {
            let days = seconds / 86_400;
            format!("库 {days} 天没同步")
        })
    }

    /// 未读条数（状态行用）。
    pub fn news_unread(&self) -> Option<usize> {
        (!self.news.is_empty()).then(|| {
            self.news
                .iter()
                .filter(|item| !self.read_news.contains(&packages::news_key(item)))
                .count()
        })
    }

    // ── 给 App 与 UI 用的信息 ────────────────────────────────────────────

    /// 状态行右边那句话（pacseek 顶栏中间那一段）。
    pub fn status_line(&self) -> String {
        match self.mode {
            PackageMode::Search => {
                if self.searching {
                    String::from("搜索中…")
                } else if let Some(term) = &self.searched {
                    format!("「{term}」{} 个结果", self.visible.len())
                } else if self.loading_all {
                    String::from("正在读全部包…")
                } else if self.all_hits.is_empty() {
                    String::from("输入关键词回车搜索（官方源 + AUR）")
                } else {
                    format!("全部 {} 个包 · 打字即时过滤", self.all_hits.len())
                }
            }
            PackageMode::Installed => {
                if self.installed_loading {
                    String::from("读已安装的包…")
                } else if !self.installed_loaded {
                    String::from("按 Enter 读一遍已安装的包")
                } else {
                    format!(
                        "{} / {} 已安装",
                        self.installed_visible.len(),
                        self.installed.len()
                    )
                }
            }
            PackageMode::News => {
                if self.news.is_empty() {
                    String::from("按 Enter 抓一次 Arch 新闻")
                } else {
                    format!("{} / {} 条新闻", self.news_visible.len(), self.news.len())
                }
            }
            PackageMode::Health => {
                if self.health_loading {
                    String::from("检查中…")
                } else if !self.health_loaded {
                    String::from("按 Enter 扫一遍")
                } else {
                    let (bad, warn) = health::tally(&self.health);
                    format!("{} 项检查 · 待处理 {bad} · 注意 {warn}", self.health.len())
                }
            }
        }
    }

    /// 当前模式顶栏要画的标签（搜索=仓库，已安装=分类，新闻=已读状态）。
    pub fn chips(&self) -> Vec<(String, bool, usize)> {
        match self.mode {
            PackageMode::Search => self
                .repos
                .iter()
                .map(|chip| (chip.name.clone(), chip.enabled, chip.count))
                .collect(),
            PackageMode::Installed => InstalledFilter::ALL
                .iter()
                .map(|filter| {
                    let count = self
                        .installed
                        .iter()
                        .filter(|package| filter.matches(package))
                        .count();
                    (
                        filter.label().to_string(),
                        self.installed_filter == *filter,
                        count,
                    )
                })
                .collect(),
            PackageMode::News => NewsFilter::ALL
                .iter()
                .map(|filter| {
                    let count = self
                        .news
                        .iter()
                        .filter(|item| filter.matches(item, &self.read_news))
                        .count();
                    (
                        filter.label().to_string(),
                        self.news_filter == *filter,
                        count,
                    )
                })
                .collect(),
            // 维护面板没有筛选标签：一屏六个检查项，筛它没意义
            PackageMode::Health => Vec::new(),
        }
    }

    /// 模式标签（带当前高亮）给 UI 画。
    pub fn mode_tabs(&self) -> Vec<(&'static str, bool)> {
        PackageMode::ALL
            .iter()
            .map(|mode| (mode.label(), *mode == self.mode))
            .collect()
    }

    /// 是否需要把某件东西写进历史文件（App 在关闭视图时调用）。
    pub fn persist_history(&self) {
        let _ = packages::save_searches_to(&packages::searches_path(), &self.history);
        let _ = packages::save_read_news(&packages::read_news_path(), &self.read_news);
    }
}

/// 队列导出/导入用的默认路径。
pub fn default_queue_path() -> PathBuf {
    packages::queue_path()
}

#[cfg(test)]
impl PackageView {
    /// 测试专用：把确认面板收掉，好继续摆弄状态。
    fn dismiss_for_test(&mut self) {
        self.confirm = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hit(repo: &str, name: &str, votes: Option<u64>) -> PackageHit {
        PackageHit {
            repo: repo.to_string(),
            name: name.to_string(),
            version: String::from("1.0-1"),
            description: format!("{name} 的说明"),
            installed_state: crate::packages::InstalledState::NotInstalled,
            votes,
            popularity: None,
            maintainer: None,
            out_of_date: false,
        }
    }

    fn installed(name: &str, explicit: bool, foreign: bool, orphan: bool) -> InstalledPackage {
        InstalledPackage {
            name: name.to_string(),
            version: String::from("1.0-1"),
            explicit,
            foreign,
            orphan,
        }
    }

    fn view_with(hits: Vec<PackageHit>) -> PackageView {
        let mut view = PackageView::new(Vec::new(), None, crate::config::PackagePrefs::default());
        view.hits = hits;
        view.rebuild_repos();
        view.apply_filter();
        view
    }

    /// 打字就是**本地模糊过滤**（pac 的手感），回车才上网搜。
    #[test]
    fn typing_filters_locally_with_fuzzy_matching() {
        let mut view = view_with(vec![
            hit("extra", "fzf", None),
            hit("aur", "sysz", Some(23)),
            hit("aur", "dotbare", Some(4)),
        ]);
        assert_eq!(view.rows_len(), 3, "一开始全都在");

        view.query.set("fzf");
        view.apply_filter();
        assert_eq!(view.rows_len(), 1);
        assert_eq!(
            view.selected_hit().map(|hit| hit.name.as_str()),
            Some("fzf")
        );

        view.query.set("sz");
        view.apply_filter();
        assert!(
            view.visible
                .iter()
                .any(|&index| view.hits[index].name == "sysz")
        );

        view.query.set("dotbare 的说明");
        view.apply_filter();
        assert_eq!(view.rows_len(), 1);

        view.query.set("zzzz");
        view.apply_filter();
        assert_eq!(view.rows_len(), 0);
    }

    /// 仓库标签与排序都作用在同一份结果上；标签带命中数。
    #[test]
    fn repo_chips_and_sort_modes_control_the_list() {
        let mut view = view_with(vec![
            hit("extra", "zsh", None),
            hit("core", "bash", None),
            hit("aur", "popular-thing", Some(99)),
            hit("aur", "quiet-thing", Some(1)),
        ]);

        let chips = view.chips();
        let aur = chips
            .iter()
            .position(|(name, _, _)| name == "aur")
            .expect("有 aur 标签");
        assert_eq!(chips[aur].2, 2, "AUR 的命中数要算出来");

        view.toggle_chip(aur);
        assert_eq!(view.rows_len(), 2, "AUR 被筛掉");
        assert_eq!(view.chips()[aur].2, 2, "关掉标签不该把条数也清零");

        // 排序：名字
        view.open_sort_menu();
        view.sort_menu_step(1);
        view.apply_sort_menu();
        assert_eq!(view.sort, SortMode::Name);
        assert_eq!(
            view.selected_hit().map(|hit| hit.name.as_str()),
            Some("bash")
        );

        // 排序：得票（AUR 被关掉了，官方源都没票，顺序保持稳定）
        view.open_sort_menu();
        view.sort_menu_step(1);
        view.sort_menu_step(1);
        view.apply_sort_menu();
        assert_eq!(view.sort, SortMode::Votes);

        // 再把 aur 打开：票最高的应该冒到前面
        view.toggle_chip(aur);
        assert_eq!(
            view.selected_hit().map(|hit| hit.name.as_str()),
            Some("popular-thing"),
            "按得票排，AUR 那个 99 票的该在最前"
        );
    }

    /// 排序菜单：Esc 取消不动排序，Enter 才落实。
    #[test]
    fn sort_menu_only_applies_on_enter() {
        let mut view = view_with(vec![hit("extra", "zsh", None), hit("core", "bash", None)]);
        view.open_sort_menu();
        assert_eq!(view.sort_menu, Some(0), "打开时高亮当前那个");
        view.sort_menu_step(1);
        view.close_sort_menu();
        assert_eq!(view.sort, SortMode::Relevance, "取消不落实");
        assert_eq!(view.sort_menu, None);
    }

    #[test]
    fn package_operation_cycles_through_install_remove_download() {
        let mut view = PackageView::new(Vec::new(), None, crate::config::PackagePrefs::default());
        assert_eq!(view.operation, PackageOperation::Install);
        view.cycle_operation();
        assert_eq!(view.operation, PackageOperation::Remove);
        view.cycle_operation();
        assert_eq!(view.operation, PackageOperation::Download);
        view.cycle_operation();
        assert_eq!(view.operation, PackageOperation::Install);
    }

    /// 队列：加入、去重、导出导入、清空。
    #[test]
    fn the_install_queue_keeps_unique_names() {
        let mut view = view_with(vec![hit("aur", "sysz", Some(23))]);
        view.toggle_queue();
        assert_eq!(view.queue.len(), 1);
        view.toggle_queue();
        assert_eq!(view.queue.len(), 0, "再按一次移出");

        let path = std::env::temp_dir().join(format!("toolbox-hub-qv-{}.txt", std::process::id()));
        view.toggle_queue();
        view.export_queue(&path);
        assert!(path.exists());

        let mut other = PackageView::new(Vec::new(), None, crate::config::PackagePrefs::default());
        other.import_queue(&path);
        assert_eq!(other.queue, view.queue, "导出再导入要一样");

        other.clear_queue();
        assert!(other.queue.is_empty());

        let _ = std::fs::remove_file(&path);
    }

    /// 已安装模式的四个筛选 + 排队卸载会自动把操作切成「卸载」。
    #[test]
    fn installed_filters_and_queue_switch_to_remove() {
        let mut view = PackageView::new(Vec::new(), None, crate::config::PackagePrefs::default());
        view.installed = vec![
            installed("bash", true, false, false),
            installed("readline", false, false, false),
            installed("aur-thing", true, true, false),
            installed("stale-lib", true, false, true),
        ];
        view.installed_loaded = true;
        view.set_mode(PackageMode::Installed);
        view.apply_installed_filter();

        let counts: Vec<usize> = view.chips().iter().map(|(_, _, count)| *count).collect();
        assert_eq!(counts, vec![4, 2, 1, 1, 1], "全部/显式/依赖/外来/孤儿");

        view.toggle_chip(4); // 孤儿
        assert_eq!(view.rows_len(), 1);
        assert_eq!(
            view.selected_installed().map(|item| item.name.as_str()),
            Some("stale-lib")
        );

        view.toggle_queue();
        assert_eq!(view.queue.len(), 1);
        assert_eq!(
            view.operation,
            PackageOperation::Remove,
            "从已安装列表排队就是为了卸载"
        );
    }

    /// 确认面板上的命令和真正要跑的命令是同一个函数算的（dry-run 的意义所在）。
    #[test]
    fn arming_the_queue_shows_the_exact_command() {
        let mut view = view_with(vec![hit("extra", "fzf", None)]);
        view.toggle_queue();
        view.arm_queue();

        let confirm = view.confirm.as_ref().expect("有确认面板");
        // 非 root 时前面会有 `sudo`（root 下没有），所以断言尾部而不是整串
        assert!(
            confirm.command.ends_with("pacman -S --needed fzf"),
            "{}",
            confirm.command
        );
        assert!(confirm.pending, "体积要现算，面板先出来（后台线程补一行）");
        assert_eq!(
            confirm.action,
            ConfirmAction::Queue(PackageOperation::Install)
        );

        // 队列里混进 AUR 就换 paru，并且说明要编译
        view.dismiss_for_test();
        view.hits.push(hit("aur", "sysz", Some(23)));
        view.rebuild_repos();
        view.apply_filter();
        view.selected = view
            .visible
            .iter()
            .position(|&index| view.hits[index].name == "sysz")
            .expect("找得到 sysz");
        view.toggle_queue();
        view.arm_queue();
        let confirm = view.confirm.as_ref().expect("有确认面板");
        assert!(
            confirm.command.starts_with("paru -S --needed"),
            "{}",
            confirm.command
        );
        assert!(
            confirm.notes.iter().any(|note| note.contains("编译")),
            "AUR 要说明会现场编译：{:?}",
            confirm.notes
        );
    }

    /// 卸载的确认面板会挂上「谁依赖它们」的分析（这里只验证流程，不跑 pacman）。
    #[test]
    fn removal_confirm_waits_for_the_impact_report() {
        let mut view = PackageView::new(Vec::new(), None, crate::config::PackagePrefs::default());
        view.queue.push(QueuedPackage {
            name: String::from("bash"),
            origin: String::from("core"),
            version: String::from("5.3-1"),
        });
        view.operation = PackageOperation::Remove;
        view.arm_queue();
        let confirm = view.confirm.as_ref().expect("有确认面板");
        assert!(confirm.pending, "影响分析还在后台跑");
        assert!(
            confirm.command.ends_with("pacman -Rns bash"),
            "{}",
            confirm.command
        );
    }

    /// 新闻：未读/已读筛选跟着已读集合走，且标记会落盘。
    #[test]
    fn news_read_state_drives_the_filter() {
        let mut view = PackageView::new(Vec::new(), None, crate::config::PackagePrefs::default());
        // 直接摆状态，不走 set_mode（那会真的去抓一次新闻）
        view.mode = PackageMode::News;
        view.news = vec![
            NewsItem {
                title: String::from("新内核"),
                published: String::new(),
                link: String::from("https://a/1"),
                epoch: Some(200),
            },
            NewsItem {
                title: String::from("旧消息"),
                published: String::new(),
                link: String::from("https://a/2"),
                epoch: Some(100),
            },
        ];
        view.read_news.clear();
        view.news_filter = NewsFilter::Unread;
        view.rebuild_news();
        assert_eq!(view.rows_len(), 2, "默认都是未读");
        assert_eq!(view.news_unread(), Some(2));

        view.toggle_news_read();
        assert_eq!(view.news_unread(), Some(1));
        assert_eq!(view.rows_len(), 1, "标已读后从未读列表里消失");
        assert_eq!(
            view.selected_news().map(|item| item.title.as_str()),
            Some("旧消息"),
            "剩下的那条才是未读"
        );

        view.news_filter = NewsFilter::Read;
        view.rebuild_news();
        assert_eq!(view.rows_len(), 1);
        assert_eq!(
            view.selected_news().map(|item| item.title.as_str()),
            Some("新内核")
        );
    }

    /// 三种模式共用一个输入框：输入即筛，在哪一屏都成立。
    #[test]
    fn the_search_box_filters_whichever_mode_you_are_in() {
        let mut view = PackageView::new(Vec::new(), None, crate::config::PackagePrefs::default());
        view.installed = vec![
            installed("bash", true, false, false),
            installed("readline", false, false, false),
        ];
        view.installed_loaded = true;
        view.news = vec![
            NewsItem {
                title: String::from("新内核"),
                published: String::new(),
                link: String::from("https://a/1"),
                epoch: Some(2),
            },
            NewsItem {
                title: String::from("老消息"),
                published: String::new(),
                link: String::from("https://a/2"),
                epoch: Some(1),
            },
        ];
        view.read_news.clear();

        // 已安装：按名字模糊筛
        view.mode = PackageMode::Installed;
        view.query.set("bash");
        view.refilter();
        assert_eq!(view.rows_len(), 1);
        assert_eq!(
            view.selected_installed().map(|item| item.name.as_str()),
            Some("bash")
        );

        // 新闻：按标题筛
        view.mode = PackageMode::News;
        view.query.set("内核");
        view.refilter();
        assert_eq!(view.rows_len(), 1);
        assert_eq!(
            view.selected_news().map(|item| item.title.as_str()),
            Some("新内核")
        );

        // 搜索：还是那套（远端结果上的本地模糊）
        view.mode = PackageMode::Search;
        view.hits = vec![hit("extra", "fzf", None)];
        view.repos = vec![];
        view.query.clear();
        view.refilter();
        assert_eq!(view.rows_len(), 1);
    }

    /// 模式标签：搜索/已安装/新闻都能切，且切过去会自动带上该有的数据。
    #[test]
    fn mode_tabs_cycle() {
        let mut view = PackageView::new(Vec::new(), None, crate::config::PackagePrefs::default());
        assert_eq!(view.mode, PackageMode::Search);
        view.cycle_mode();
        assert_eq!(view.mode, PackageMode::Installed);
        // 测试里没有取数线程：这时**不许装样子**（显示「正在读…」却没人在读），
        // 要老老实实说清楚 —— 这条钉住的就是「失败要说人话」。
        assert!(!view.installed_loading);
        assert!(view.message.contains("取数线程"), "{}", view.message);
        view.cycle_mode();
        assert_eq!(view.mode, PackageMode::News);
        view.cycle_mode();
        assert_eq!(view.mode, PackageMode::Health);
        // 同上：没有取数线程时要说实话，而不是显示「检查中…」却没人检查
        assert!(!view.health_loading);
        assert!(view.message.contains("取数线程"), "{}", view.message);
        view.cycle_mode();
        assert_eq!(view.mode, PackageMode::Search);
    }
}
