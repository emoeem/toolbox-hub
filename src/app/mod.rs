//! 界面状态层。
//!
//! [`App`] 持有全部可变状态；UI 只读它，输入层与 runtime 写它。
//! 数据来源统一走 [`crate::registry::Registry`]，这里不认识任何具体 Provider。

mod input;
pub mod package_view;
mod picker;
mod text_input;

pub use input::{handle_key, handle_mouse, handle_paste};
pub use picker::Picker;
pub use text_input::TextInput;

use std::{
    collections::VecDeque,
    io,
    path::{Path, PathBuf},
    time::SystemTime,
};

use ratatui::widgets::TableState;

use crate::{
    app::package_view::{ConfirmAction, PackageOperation, PackageView},
    history, media,
    model::{ArgKind, Argument, ArgumentValues, Domain, ToolDefinition},
    packages,
    registry::{Registry, ReloadReport},
    runtime::{self, Captured, RunningJob},
    state::{self, State, WORK_DIR_ENV},
};

/// 确认面板按 Enter 之后到底要跑什么（算完就松开 `PackageView` 的借用再执行）。
enum Plan {
    Queue {
        operation: PackageOperation,
        uses_aur: bool,
        names: Vec<String>,
    },
    Upgrade {
        program: &'static str,
    },
    Cache {
        program: String,
        argv: Vec<String>,
    },
    Orphans {
        names: Vec<String>,
    },
}

/// 秒 → `mm:ss` / `h:mm:ss`（进度行用）。
fn short_time(seconds: f64) -> String {
    let total = seconds.max(0.0).round() as u64;
    let (hours, minutes, secs) = (total / 3600, (total % 3600) / 60, total % 60);
    if hours > 0 {
        format!("{hours}:{minutes:02}:{secs:02}")
    } else {
        format!("{minutes:02}:{secs:02}")
    }
}

/// 工作目录里的媒体文件视图（按 `F` 打开）。
///
/// 存在的理由：FFTools 那批脚本靠扫工作目录找文件，而工具箱里**原本没有任何地方**
/// 能让你看见「这个目录里到底有什么」。脚本说「没有文件」时，你无从判断。
pub struct FilesView {
    /// 扫的是哪个目录。
    pub root: PathBuf,
    /// 扫到的全部媒体文件。
    all: Vec<media::MediaFile>,
    /// 过滤后可见的下标。
    visible: Vec<usize>,
    pub selected: usize,
    pub filter: String,
    /// 是不是被上限/时间预算截断了（显示成 ≥N）。
    pub truncated: bool,
}

impl FilesView {
    pub fn len(&self) -> usize {
        self.visible.len()
    }

    pub fn is_empty(&self) -> bool {
        self.visible.is_empty()
    }

    pub fn entry(&self, index: usize) -> Option<&media::MediaFile> {
        self.visible
            .get(index)
            .and_then(|&index| self.all.get(index))
    }

    pub fn selected(&self) -> Option<&media::MediaFile> {
        self.entry(self.selected)
    }

    pub fn apply_filter(&mut self) {
        let needle = self.filter.to_lowercase();
        self.visible = self
            .all
            .iter()
            .enumerate()
            .filter(|(_, file)| needle.is_empty() || file.relative.to_lowercase().contains(&needle))
            .map(|(index, _)| index)
            .collect();
        if self.selected >= self.visible.len() {
            self.selected = self.visible.len().saturating_sub(1);
        }
    }

    /// 还有多少条没被过滤掉（显示用）。
    pub fn total(&self) -> usize {
        self.all.len()
    }
}

/// 文件管理器用哪个程序（默认 yazi；换成 ranger / lf / nnn 也行，
/// 只要它认 `--cwd-file`）。
pub const FILE_MANAGER_ENV: &str = "TOOLBOX_HUB_FILE_MANAGER";

/// 「最近用过的目录」最多记几个。
const RECENT_DIRS: usize = 8;

/// 实时回看保留多少行输出（再多也看不清，而且没必要）。
const TAIL_LINES: usize = 12;

/// 交给后台队列的一次执行。
pub struct CaptureRequest {
    pub tool: ToolDefinition,
    /// 哪些退出码算成功（从动作抄来；`poll_job` 收尾时用它判定）。
    pub ok_exit_codes: Vec<i32>,
    /// 当时的表单取值（历史「回填再改」用）。
    pub values: Vec<(String, String)>,
    /// 真正执行的参数。
    pub argv: Vec<String>,
    /// 写进历史的参数（敏感值已替换；程序名由 [`App::enqueue_captures`] 补上）。
    pub record_argv: Vec<String>,
    /// 进度条用的总秒数（探不到就是 `None`，只显示已跑时间）。
    pub total_seconds: Option<f64>,
}

/// 排队等着后台执行的一件工具。
struct PendingJob {
    tool_id: String,
    tool_name: String,
    program: PathBuf,
    /// 真正执行的参数。
    argv: Vec<String>,
    /// 写进历史的参数（敏感值已替换）。
    record_argv: Vec<String>,
    /// 当时的表单取值。
    values: Vec<(String, String)>,
    ok_exit_codes: Vec<i32>,
    /// 进度条用的总秒数。
    total_seconds: Option<f64>,
}

/// 正在跑的后台任务，外加它的实时输出与进度。
pub struct RunningJobView {
    pub job: RunningJob,
    /// 进度条用的总秒数（探不到就没有百分比）。
    pub total_seconds: Option<f64>,
    tool_id: String,
    tool_name: String,
    record_argv: Vec<String>,
    values: Vec<(String, String)>,
    ok_exit_codes: Vec<i32>,
    /// 最近几行输出（`(是否 stderr, 内容)`）。
    pub tail: VecDeque<(bool, String)>,
    /// 最近一次的进度键值（ffmpeg `-progress` 那种）。
    pub progress: Vec<(String, String)>,
}

impl RunningJobView {
    /// 测试用：直接造一个运行态（这些字段是私有的，外面只能这么建）。
    #[cfg(test)]
    pub fn for_test(
        job: RunningJob,
        tail: Vec<(bool, String)>,
        progress: Vec<(String, String)>,
    ) -> Self {
        Self {
            job,
            total_seconds: None,
            tool_id: String::from("test:job"),
            tool_name: String::from("测试任务"),
            record_argv: Vec::new(),
            values: Vec::new(),
            ok_exit_codes: vec![0],
            tail: tail.into(),
            progress,
        }
    }

    /// 进度百分比（0..=1）：**探到总时长才算得出来**。
    pub fn progress_ratio(&self) -> Option<f64> {
        let total = self.total_seconds?;
        if total <= 0.0 {
            return None;
        }
        let out_time = self
            .progress
            .iter()
            .find(|(key, _)| key == "out_time")
            .map(|(_, value)| value.as_str())?;
        let done = crate::runtime::parse_duration(out_time)?;
        Some((done / total).clamp(0.0, 1.0))
    }

    /// 进度行里的 `已完成 / 总时长` 时:分:秒文本。
    pub fn progress_time_label(&self) -> Option<String> {
        let total = self.total_seconds?;
        let out_time = self
            .progress
            .iter()
            .find(|(key, _)| key == "out_time")
            .map(|(_, value)| value.as_str())?;
        let done = crate::runtime::parse_duration(out_time)?;
        Some(format!("{} / {}", short_time(done), short_time(total)))
    }

    /// 进度显示用的键（按这个顺序展示，一眼能读懂）。
    pub fn progress_label(&self) -> String {
        const ORDER: [&str; 5] = ["out_time", "frame", "fps", "speed", "total_size"];
        let mut parts = Vec::new();
        for key in ORDER {
            if let Some((_, value)) = self
                .progress
                .iter()
                .find(|(existing, _)| existing.as_str() == key)
            {
                parts.push(format!("{key}={value}"));
            }
        }
        parts.join("  ")
    }
}

/// 二级筛选条里代表「不过滤」的那一项。
pub const SUB_ALL: &str = "全部";

/// 内置输出视图的状态。
///
/// 捕获模式的命令跑完后进这里：可以滚动回看，不再「滚过去就没了」。
pub struct Viewer {
    /// 标题：一条命令就是命令本身，多条就是「N 个命令」。
    pub title: String,
    pub body: String,
    /// 状态行：退出码 / 耗时。
    pub status: String,
    /// 已滚动行数；渲染时按可视高度夹紧。
    pub scroll: usize,
    /// 已横向滚动的终端列数。
    pub horizontal_scroll: u16,
    /// 最长那一行的显示宽度（列）。横向滚动按它夹紧 —— 否则一直按 `→`
    /// 会滚进一片空白，看着像界面卡死了。
    pub max_line_width: u16,
}

impl Viewer {
    /// 竖直滚动（负数向上）。渲染时还会按可视高度夹紧。
    pub fn scroll_by(&mut self, delta: isize) {
        self.scroll = (self.scroll as isize + delta).max(0) as usize;
    }

    /// 横向滚动（负数向左）。最多到最长行的最后一列 —— 再往右是空白，
    /// 看不出「已经到头了」。
    pub fn scroll_horizontal_by(&mut self, delta: i16) {
        let limit = self.max_line_width.saturating_sub(1);
        self.horizontal_scroll =
            (self.horizontal_scroll as i32 + i32::from(delta)).clamp(0, i32::from(limit)) as u16;
    }

    pub fn scroll_to_top(&mut self) {
        self.scroll = 0;
    }

    pub fn scroll_to_bottom(&mut self) {
        self.scroll = self.body.lines().count();
    }

    /// 把一个字符串包成可滚动的视图（`toolbox-hub ui pager` 用）。
    pub fn from_text(title: String, body: String) -> Self {
        let body = if body.ends_with('\n') {
            body
        } else {
            format!("{body}\n")
        };
        let max_line_width = body
            .lines()
            .map(unicode_width::UnicodeWidthStr::width)
            .max()
            .unwrap_or(0)
            .min(u16::MAX as usize) as u16;
        Self {
            title,
            body,
            status: String::new(),
            scroll: 0,
            horizontal_scroll: 0,
            max_line_width,
        }
    }

    /// 由一批捕获结果拼成一个视图。没有结果时返回 `None`。
    pub fn from_captured(captured: &[Captured]) -> Option<Self> {
        let first = captured.first()?;
        let multiple = captured.len() > 1;

        let mut body = String::new();
        let mut failed = 0usize;
        for item in captured {
            if multiple {
                body.push_str(&format!("$ {}\n", item.command));
            }
            body.push_str(&item.body());
            body.push('\n');
            if !item.success {
                failed += 1;
            }
        }

        let status = if multiple {
            let elapsed: f64 = captured.iter().map(|item| item.elapsed.as_secs_f64()).sum();
            format!(
                "{} 个命令 · 失败 {failed} 个 · 合计 {elapsed:.2}s",
                captured.len()
            )
        } else {
            first.summary()
        };

        let body = format!("{}\n", body.trim_end());
        let max_line_width = body
            .lines()
            .map(unicode_width::UnicodeWidthStr::width)
            .max()
            .unwrap_or(0)
            .min(u16::MAX as usize) as u16;

        Some(Self {
            title: if multiple {
                format!("{} 个命令", captured.len())
            } else {
                first.command.clone()
            },
            body,
            status,
            scroll: 0,
            horizontal_scroll: 0,
            max_line_width,
        })
    }
}

/// 列表的「视图」：全部 / 只看收藏 / 最近使用。
///
/// 和域、分类是正交的：收藏与最近都**跨域**（切到这两个视图后域与分类不再参与筛选），
/// 但关键词搜索照样叠加。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Scope {
    #[default]
    All,
    Favorites,
    Recent,
}

impl Scope {
    pub fn label(self) -> &'static str {
        match self {
            Scope::All => "全部工具",
            Scope::Favorites => "★ 收藏",
            Scope::Recent => "最近使用",
        }
    }

    pub fn next(self) -> Self {
        match self {
            Scope::All => Scope::Favorites,
            Scope::Favorites => Scope::Recent,
            Scope::Recent => Scope::All,
        }
    }
}

/// 执行历史列表的状态。
pub struct HistoryView {
    /// 最近的在最前。
    pub entries: Vec<history::Entry>,
    pub selected: usize,
}

impl HistoryView {
    pub fn open() -> Self {
        Self {
            entries: history::load(200),
            selected: 0,
        }
    }

    pub fn selected_entry(&self) -> Option<&history::Entry> {
        self.entries.get(self.selected)
    }
}

/// 参数表单的界面状态。
///
/// 表单是**模式**：打开期间 [`crate::ui`] 把表格与详情区换成字段列表与命令预览，
/// 按键也全部交给表单处理（见 [`crate::app::handle_key`]）。
pub struct FormState {
    /// 正在填写的工具在 `registry.tools()` 里的下标。
    pub tool: usize,
    /// 当前取值。
    pub values: ArgumentValues,
    /// 当前字段下标。
    pub field: usize,
    /// 文本类字段是否处于输入态（输入态下普通按键都进文本框）。
    pub editing: bool,
    /// 最近一次「想执行但构建失败」的原因，显示在表单里。
    pub error: Option<String>,
    /// 危险动作是否已经确认过一次；改任何参数都会把它清掉。
    pub confirm: bool,
}

pub struct App {
    pub registry: Registry,
    /// 被扫描的工具目录，只用于顶部栏展示。
    pub bin_dir: PathBuf,
    /// 一级域下标，对应 [`Domain::ALL`]。
    pub domain: usize,
    /// 二级筛选下标，`0` 恒为 [`SUB_ALL`]。
    pub sub: usize,
    /// 当前域的二级筛选项（`[0]` 是「全部」），切域时重建。
    pub sub_tags: Vec<String>,
    /// 当前视图：命中工具在 `registry.tools()` 中的下标。
    pub filtered: Vec<usize>,
    pub selected: usize,
    pub table: TableState,
    pub query: TextInput,
    pub searching: bool,
    /// 与 `registry.tools()` 等长；标记跨筛选、跨域保留。
    pub marked: Vec<bool>,
    pub message: String,
    pub last_reload: SystemTime,
    /// 参数表单；`None` 表示在列表里浏览。
    pub form: Option<FormState>,
    /// 输出视图；`None` 表示没在看输出。
    pub viewer: Option<Viewer>,
    /// 列表模式下「危险动作已经问过一次」的目标，第二次回车才真跑。
    pub pending: Option<Vec<usize>>,
    /// 执行历史列表；`None` 表示没在看历史。
    pub history: Option<HistoryView>,
    /// 当前视图（全部 / 收藏 / 最近）。
    pub scope: Scope,
    /// 用户状态（收藏就在这里）。
    pub state: State,
    /// 状态文件落在哪；测试可以指到临时文件，避免动到真实配置。
    pub state_path: PathBuf,
    /// 执行历史落在哪；`None` = 默认位置（测试可以指到临时文件）。
    pub history_path: Option<PathBuf>,
    /// 需要整屏重画（接管过终端以后必须，见 [`App::take_full_redraw`]）。
    pub needs_full_redraw: bool,
    /// 工具在哪个目录里执行。扫目录的脚本（FFTools 那批）也正是**在这里**找输入文件，
    /// 所以它必须看得见、改得动（按 `d`）。
    pub work_dir: PathBuf,
    /// 「改工作目录」的输入态；`Some` 时所有按键都进这个文本框。
    pub dir_input: Option<String>,
    /// 文件选择器；`Some` 时把表单顶掉，专门用来挑文件。
    pub picker: Option<Picker>,
    /// 帮助屏的滚动位置；`Some` 表示开着。
    pub help: Option<usize>,
    /// 工作目录里的媒体文件（头部显示数量用；有上限也有时间预算）。
    pub media: media::ScanResult,
    /// `media` 是针对哪个目录扫的（换目录后要重扫）。
    pub media_root: PathBuf,
    /// 文件视图；`Some` 表示开着。
    pub files: Option<FilesView>,
    /// 图片预览（F 视图右半边）。懒探测、后台解码，见 [`crate::preview`]。
    pub preview: crate::preview::Preview,
    /// 原生包管理视图；`Some` 表示开着。
    pub packages: Option<PackageView>,
    /// 正在跑的后台任务。
    pub running: Option<RunningJobView>,
    /// 排队等着跑的任务（批量执行时用）。
    job_queue: VecDeque<PendingJob>,
    /// 这一批已经跑完的结果，等全部结束一起进输出视图。
    job_results: Vec<Captured>,
}

impl App {
    pub fn new(registry: Registry, bin_dir: PathBuf, report: ReloadReport) -> Self {
        let marked = vec![false; registry.len()];
        let mut app = Self {
            registry,
            bin_dir,
            domain: 0,
            sub: 0,
            sub_tags: vec![String::from(SUB_ALL)],
            filtered: Vec::new(),
            selected: 0,
            table: TableState::default(),
            query: TextInput::new(),
            searching: false,
            marked,
            message: String::new(),
            last_reload: SystemTime::now(),
            form: None,
            viewer: None,
            pending: None,
            history: None,
            scope: Scope::All,
            state: state::load(),
            state_path: state::path(),
            history_path: None,
            needs_full_redraw: false,
            work_dir: std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
            dir_input: None,
            picker: None,
            help: None,
            media: media::ScanResult::default(),
            media_root: PathBuf::new(),
            files: None,
            preview: crate::preview::Preview::new(),
            packages: None,
            running: None,
            job_queue: VecDeque::new(),
            job_results: Vec::new(),
        };
        app.apply_filter();
        app.message = report.message("就绪");
        app
    }

    pub fn current_domain(&self) -> Domain {
        Domain::ALL[self.domain.min(Domain::ALL.len() - 1)]
    }

    /// 当前二级筛选标签；`None` 表示「全部」。
    pub fn sub_filter(&self) -> Option<&str> {
        if self.sub == 0 {
            None
        } else {
            self.sub_tags.get(self.sub).map(String::as_str)
        }
    }

    /// 当前域是否需要显示二级筛选条。空域和单一分类不显示。
    pub fn has_sub_tabs(&self) -> bool {
        self.sub_tags.len() > 1
    }

    /// 当前高亮的工具。
    pub fn current(&self) -> Option<&ToolDefinition> {
        self.filtered
            .get(self.selected)
            .and_then(|index| self.registry.tools().get(*index))
    }

    /// 重建视图：域 + 二级筛选 + 关键词。
    pub fn apply_filter(&mut self) {
        self.rebuild_sub_tags();
        // 收藏 / 最近是跨域视图：不受当前域与分类限制，但关键词照样能筛。
        let tag = self.sub_filter().map(str::to_string);
        self.filtered = match self.scope {
            Scope::All => {
                self.registry
                    .view(self.current_domain(), tag.as_deref(), self.query.text())
            }
            Scope::Favorites | Scope::Recent => {
                if self.query.text().trim().is_empty() {
                    self.registry.all_indices()
                } else {
                    // 关键词非空时 view 本来就是跨域的。
                    self.registry
                        .view(self.current_domain(), None, self.query.text())
                }
            }
        };
        self.apply_scope();

        if self.filtered.is_empty() {
            self.selected = 0;
            self.table.select(None);
        } else {
            self.selected = self.selected.min(self.filtered.len() - 1);
            self.table.select(Some(self.selected));
        }
    }

    /// 把收藏 / 最近这两个**用户状态**过滤叠上去。
    ///
    /// 放在 App 而不是 Registry：Registry 只管 Provider 给的东西，不认识收藏。
    fn apply_scope(&mut self) {
        match self.scope {
            Scope::All => {}
            Scope::Favorites => {
                let favorites = self.state.favorites.clone();
                let mut kept = Vec::new();
                for &index in &self.filtered {
                    if let Some(tool) = self.registry.tools().get(index)
                        && favorites.iter().any(|id| id == &tool.id)
                    {
                        kept.push(index);
                    }
                }
                self.filtered = kept;
            }
            Scope::Recent => {
                // 最近使用直接来自执行历史，按「新的在前」排。
                let recent = self.recent_tool_ids();
                let mut ranked: Vec<(usize, usize)> = Vec::new();
                for &index in &self.filtered {
                    let Some(tool) = self.registry.tools().get(index) else {
                        continue;
                    };
                    if let Some(rank) = recent.iter().position(|id| id == &tool.id) {
                        ranked.push((rank, index));
                    }
                }
                ranked.sort_unstable();
                self.filtered = ranked.into_iter().map(|(_, index)| index).collect();
            }
        }
    }

    fn rebuild_sub_tags(&mut self) {
        let mut tags = vec![String::from(SUB_ALL)];
        tags.extend(self.registry.tags_for(self.current_domain()));
        self.sub_tags = tags;
        if self.sub >= self.sub_tags.len() {
            self.sub = 0;
        }
    }

    /// 当前是不是跨域搜索（关键词非空即跨域，规则见 [`Registry::view`]）。
    ///
    /// UI 靠它决定：显示域内分类条，还是显示「命中落在哪些域」的说明。
    pub fn is_global_search(&self) -> bool {
        !self.query.text().trim().is_empty()
    }

    /// 当前搜索结果按域统计，只列出有命中的域，顺序按 [`Domain::ALL`]。
    pub fn search_hits_by_domain(&self) -> Vec<(Domain, usize)> {
        let tools = self.registry.tools();
        Domain::ALL
            .into_iter()
            .map(|domain| {
                let hits = self
                    .filtered
                    .iter()
                    .filter(|&&index| tools[index].domain == domain)
                    .count();
                (domain, hits)
            })
            .filter(|(_, hits)| *hits > 0)
            .collect()
    }

    pub fn move_selection(&mut self, delta: isize) {
        if self.filtered.is_empty() {
            return;
        }
        let len = self.filtered.len() as isize;
        self.selected = (self.selected as isize + delta).rem_euclid(len) as usize;
        self.table.select(Some(self.selected));
    }

    pub fn select_first(&mut self) {
        if self.filtered.is_empty() {
            return;
        }
        self.selected = 0;
        self.table.select(Some(0));
    }

    pub fn select_last(&mut self) {
        let Some(last) = self.filtered.len().checked_sub(1) else {
            return;
        };
        self.selected = last;
        self.table.select(Some(last));
    }

    pub fn toggle_mark(&mut self) {
        let Some(&index) = self.filtered.get(self.selected) else {
            return;
        };
        self.marked[index] = !self.marked[index];
        let name = &self.registry.tools()[index].name;
        self.message = if self.marked[index] {
            format!("已加入队列: {name}")
        } else {
            format!("已取消: {name}")
        };
    }

    /// 切换一级域：二级筛选与高亮位置复位。
    pub fn switch_domain(&mut self, domain: usize) {
        self.domain = domain % Domain::ALL.len();
        self.sub = 0;
        self.selected = 0;
        self.apply_filter();

        let current = self.current_domain();
        let count = self.registry.tool_count_in(current);
        self.message = if count == 0 {
            format!("{} · Provider 待接入", current.label())
        } else {
            format!("{} · {} 个工具", current.label(), count)
        };

        // 包管理域的「主界面」就是软件包中心：切进去直接打开它，
        // 而不是先给一张 14 个 CLI 动作的列表（那正是「按 Enter 得到表单」的来源）。
        // 想看动作列表就按 Esc —— 它就在中心底下。
        if current == Domain::Packages && self.packages.is_none() {
            self.open_packages();
            self.message = String::from(
                "软件包中心：打字过滤（全库）· Enter 上网搜 · Space 排队 · Esc 回动作列表",
            );
        }
    }

    /// 切换二级筛选。
    pub fn move_sub(&mut self, delta: isize) {
        if !self.has_sub_tabs() {
            self.message = format!("{} 域没有二级分类", self.current_domain().label());
            return;
        }
        let len = self.sub_tags.len() as isize;
        self.sub = (self.sub as isize + delta).rem_euclid(len) as usize;
        self.selected = 0;
        self.apply_filter();

        let label = self.sub_filter().unwrap_or(SUB_ALL).to_string();
        self.message = format!("分类 · {label} · {} 个工具", self.filtered.len());
    }

    /// 重新发现全部 Provider，并清空标记。
    pub fn reload(&mut self) {
        let report = self.registry.reload();
        self.marked = vec![false; self.registry.len()];
        self.sub = 0;
        self.selected = 0;
        self.apply_filter();
        self.last_reload = SystemTime::now();
        self.message = report.message("已刷新");
    }

    /// 待执行目标：优先已标记项；没有标记时退化为当前高亮项。
    pub fn execution_targets(&self) -> Vec<usize> {
        let mut targets: Vec<usize> = self
            .marked
            .iter()
            .enumerate()
            .filter_map(|(index, marked)| marked.then_some(index))
            .collect();
        if targets.is_empty()
            && let Some(&index) = self.filtered.get(self.selected)
        {
            targets.push(index);
        }
        targets
    }

    pub fn clear_marks(&mut self, targets: &[usize]) {
        for &index in targets {
            if let Some(slot) = self.marked.get_mut(index) {
                *slot = false;
            }
        }
    }

    pub fn marked_count(&self) -> usize {
        self.marked.iter().filter(|marked| **marked).count()
    }

    // ── 参数表单 ──────────────────────────────────────────────────────────

    /// 当前高亮的工具需要填参数时打开表单。
    ///
    /// 返回 `false` 表示这件工具不需要参数（调用方应该直接执行它）。
    pub fn open_form(&mut self) -> bool {
        let Some(&index) = self.filtered.get(self.selected) else {
            return false;
        };
        let Some(action) = self.registry.tools()[index].action.clone() else {
            return false;
        };
        // 一个字段都没有的动作不用进表单：空表单只会让人以为漏了什么
        // （「有哪些更新」这类无参命令就属于这种，直接跑更对）。
        if action.arguments.is_empty() {
            return false;
        }

        self.form = Some(FormState {
            tool: index,
            values: action.default_values(),
            field: 0,
            editing: false,
            error: None,
            confirm: false,
        });
        self.message = String::from("填写参数 · Ctrl-E 执行 · Esc 返回");
        true
    }

    pub fn close_form(&mut self) {
        self.form = None;
        self.message = String::from("已返回列表");
    }

    /// 表单正在填写的工具。
    pub fn form_tool(&self) -> Option<&ToolDefinition> {
        let form = self.form.as_ref()?;
        self.registry.tools().get(form.tool)
    }

    /// 表单字段；没有动作时是空表。
    pub fn form_arguments(&self) -> &[Argument] {
        self.form_tool()
            .and_then(|tool| tool.action.as_ref())
            .map(|action| action.arguments.as_slice())
            .unwrap_or(&[])
    }

    pub fn form_is_editing(&self) -> bool {
        self.form.as_ref().is_some_and(|form| form.editing)
    }

    pub fn form_error(&self) -> Option<&str> {
        self.form.as_ref().and_then(|form| form.error.as_deref())
    }

    /// 当前字段。
    pub fn form_current_argument(&self) -> Option<&Argument> {
        let form = self.form.as_ref()?;
        self.form_arguments().get(form.field)
    }

    /// 上下移动字段（离开输入态）。
    pub fn form_move(&mut self, delta: isize) {
        let len = self.form_arguments().len() as isize;
        if len == 0 {
            return;
        }
        if let Some(form) = self.form.as_mut() {
            form.field = (form.field as isize + delta).rem_euclid(len) as usize;
            form.editing = false;
            form.error = None;
        }
    }

    /// ←→ / 空格 / Enter：`Choice` 换选项、`Toggle` 翻转；文本框不动。
    pub fn form_adjust(&mut self, delta: isize) {
        self.form_disarm();
        let Some(argument) = self.form_current_argument().cloned() else {
            return;
        };
        let Some(form) = self.form.as_mut() else {
            return;
        };
        form.error = None;

        match argument.kind {
            ArgKind::Choice => {
                let current = form.values.get(&argument.key).unwrap_or("").to_string();
                if let Some(next) = argument.cycle_choice(&current, delta) {
                    form.values.set(&argument.key, next);
                }
            }
            ArgKind::Toggle => {
                let next = if form.values.is_on(&argument.key) {
                    "false"
                } else {
                    "true"
                };
                form.values.set(&argument.key, next);
            }
            ArgKind::Text | ArgKind::Path => {}
        }
    }

    /// 文本 / 路径字段进入输入态；`Choice` / `Toggle` 则直接改值。
    pub fn form_activate(&mut self) {
        self.form_disarm();
        let Some(argument) = self.form_current_argument().cloned() else {
            return;
        };
        match argument.kind {
            ArgKind::Text | ArgKind::Path => {
                if let Some(form) = self.form.as_mut() {
                    form.editing = true;
                    form.error = None;
                }
            }
            _ => self.form_adjust(1),
        }
    }

    pub fn form_end_edit(&mut self) {
        if let Some(form) = self.form.as_mut() {
            form.editing = false;
        }
    }

    pub fn form_push_char(&mut self, ch: char) {
        self.form_disarm();
        let Some(argument) = self.form_current_argument().cloned() else {
            return;
        };
        let Some(form) = self.form.as_mut() else {
            return;
        };
        if !form.editing {
            return;
        }
        let mut value = form.values.get(&argument.key).unwrap_or("").to_string();
        value.push(ch);
        form.values.set(&argument.key, value);
    }

    pub fn form_backspace(&mut self) {
        self.form_disarm();
        let Some(argument) = self.form_current_argument().cloned() else {
            return;
        };
        let Some(form) = self.form.as_mut() else {
            return;
        };
        if !form.editing {
            return;
        }
        let mut value = form.values.get(&argument.key).unwrap_or("").to_string();
        value.pop();
        form.values.set(&argument.key, value);
    }

    pub fn form_set_error(&mut self, message: String) {
        if let Some(form) = self.form.as_mut() {
            form.error = Some(message);
        }
    }

    /// 构建当前表单对应的 argv。
    ///
    /// 预览与执行共用这一份逻辑，所以屏幕上看到的命令就是真正会跑的命令。
    pub fn form_build(&self) -> Result<Vec<String>, String> {
        let Some(action) = self.form_tool().and_then(|tool| tool.action.as_ref()) else {
            return Err(String::from("这件工具没有参数"));
        };
        let Some(form) = self.form.as_ref() else {
            return Err(String::from("表单没有打开"));
        };
        action
            .build_argv(&form.values)
            .map_err(|message| message.to_string())
    }

    // ── 输出视图 ──────────────────────────────────────────────────────────

    /// 打开输出视图（捕获模式的执行结果）。
    pub fn open_viewer(&mut self, viewer: Viewer) {
        self.viewer = Some(viewer);
    }

    pub fn close_viewer(&mut self) {
        self.viewer = None;
        self.message = String::from("已关闭输出");
    }

    /// 滚动输出视图；负数向上。渲染时还会按可视高度夹紧。
    pub fn viewer_scroll(&mut self, delta: isize) {
        if let Some(viewer) = self.viewer.as_mut() {
            viewer.scroll_by(delta);
        }
    }

    pub fn viewer_scroll_horizontal(&mut self, delta: i16) {
        if let Some(viewer) = self.viewer.as_mut() {
            viewer.scroll_horizontal_by(delta);
        }
    }

    pub fn viewer_to_top(&mut self) {
        if let Some(viewer) = self.viewer.as_mut() {
            viewer.scroll_to_top();
        }
    }

    pub fn viewer_to_bottom(&mut self) {
        if let Some(viewer) = self.viewer.as_mut() {
            viewer.scroll_to_bottom();
        }
    }

    // ── 工作目录 ──────────────────────────────────────────────────────────

    /// 启动时确定工作目录。
    ///
    /// 顺序：`TOOLBOX_HUB_WORKDIR` > 上次记住的（**还存在**才用）> 启动时所在目录。
    /// 工具就在这个目录里执行，所以「从项目目录 `cargo run`」不会再让扫目录的脚本
    /// 什么都找不到 —— 按 `d` 指到真正放文件的地方就行。
    pub fn resolve_work_dir(&mut self) {
        if let Some(raw) = std::env::var_os(WORK_DIR_ENV) {
            let path = state::expand_home(&raw.to_string_lossy());
            if path.is_dir() {
                self.work_dir = path;
                return;
            }
            self.message = format!("{WORK_DIR_ENV} 不是目录：{}", path.display());
        }

        if let Some(path) = self.state.work_dir.clone() {
            if path.is_dir() {
                self.work_dir = path;
            } else {
                self.message = format!("上次的工作目录已不存在：{}", path.display());
            }
        }

        // 数一下这个目录里有多少媒体文件 —— 这就是「脚本会不会看到东西」的答案。
        self.refresh_media();
    }

    pub fn is_editing_dir(&self) -> bool {
        self.dir_input.is_some()
    }

    /// 进入「改工作目录」输入态（预填当前目录，好改）。
    pub fn open_dir_input(&mut self) {
        self.dir_input = Some(self.work_dir.display().to_string());
        self.message = String::from("输入工作目录 · Enter 确认 · Esc 取消");
    }

    pub fn cancel_dir_input(&mut self) {
        self.dir_input = None;
        self.message = String::from("已取消");
    }

    pub fn dir_input_text(&self) -> &str {
        self.dir_input.as_deref().unwrap_or("")
    }

    pub fn dir_input_push(&mut self, ch: char) {
        if let Some(text) = self.dir_input.as_mut() {
            text.push(ch);
        }
    }

    pub fn dir_input_backspace(&mut self) {
        if let Some(text) = self.dir_input.as_mut() {
            text.pop();
        }
    }

    /// 清空输入（Ctrl-U）：预填的路径常常要整个换掉。
    pub fn dir_input_clear(&mut self) {
        if let Some(text) = self.dir_input.as_mut() {
            text.clear();
        }
    }

    /// 确认工作目录：展开 `~`、必须存在且是目录、然后记住。
    pub fn commit_dir_input(&mut self) {
        let Some(raw) = self.dir_input.clone() else {
            return;
        };
        let typed = raw.trim().to_string();
        if typed.is_empty() {
            self.dir_input = None;
            self.message = String::from("已取消（没有输入目录）");
            return;
        }

        let path = state::expand_home(&typed);
        if !path.is_dir() {
            // 留在输入态让他接着改，不用从头再打一遍。
            self.message = format!("不是目录：{}", path.display());
            return;
        }

        // 存绝对路径：不然走到别处以后相对路径就没意义了。
        let path = std::fs::canonicalize(&path).unwrap_or(path);
        self.dir_input = None;
        self.set_work_dir(path, "已改工作目录");
    }

    // ── 原生包管理视图 ───────────────────────────────────────────────────

    /// 按 `p`：打开原生包管理（搜索 / 信息 / 排队 / 安装）。
    pub fn open_packages(&mut self) {
        let history = packages::load_searches_from(&packages::searches_path());
        // 常驻取数线程：libalpm 句柄与 HTTP 连接都活在它里面（见 packages::worker）。
        // 起不来也把界面打开 —— 每块面板会自己说明「取数线程没起来」。
        let worker = match packages::worker::Worker::start() {
            Ok(worker) => Some(worker),
            Err(error) => {
                self.message = format!("包管理取数线程起不来：{error}");
                None
            }
        };
        // 偏好（默认仓库 / 演练模式 / 保留版本 / 排序 / 模式）。
        // 文件不在或写坏了都用默认值，并把问题挂在状态行上 —— 一个手改错的
        // 配置文件不该让工具打不开。
        let loaded = crate::config::load_packages();
        if let Some(problem) = loaded.problem.clone() {
            self.message = problem;
        }
        let mut view = PackageView::new(history, worker, loaded.value);
        // 上次没装完的队列还能接着装
        let queue = packages::load_queue_from(&packages::queue_path());
        if !queue.is_empty() {
            view.message = format!("上次的队列还在：{} 个（Ctrl+I 也可导入）", queue.len());
            view.queue = queue;
        }
        // 一进来就把全库铺上（paru 那种「一进就有 38869 个包」），别让人对着空屏发呆
        view.ensure_all_packages();
        self.packages = Some(view);
    }

    /// 每帧收一次包管理的后台结果（搜索、包信息、新闻）。
    /// 收一次图片解码的结果（F 视图用）。返回 `true` 表示该重画。
    pub fn poll_preview(&mut self) -> bool {
        self.preview.poll()
    }

    /// 收包管理的后台结果。返回 `true` 表示收到了东西（该重画了）。
    pub fn poll_packages(&mut self) -> bool {
        let Some(view) = self.packages.as_mut() else {
            return false;
        };
        let changed = view.poll();
        // 维护面板要「把明细丢进输出视图」时，得由 App 来做（它才是打开视图的那个人）
        if let Some((title, lines)) = view.pending_view.take() {
            let captured = runtime::Captured {
                label: title,
                command: String::from("（维护检查的输出）"),
                stdout: lines.join("\n"),
                stderr: String::new(),
                status: Some(0),
                success: true,
                elapsed: std::time::Duration::ZERO,
                cancelled: false,
            };
            if let Some(viewer) = Viewer::from_captured(&[captured]) {
                self.open_viewer(viewer);
                return true;
            }
        }
        changed
    }

    /// 维护面板里按 Enter：处理选中的那一项。
    ///
    /// 分两步写：先把「选中项的动作」取出来（只借不可变引用），松开之后再分发 ——
    /// 因为处理动作要改 App（清缓存、系统更新都是），两边同时借 self 是过不去的。
    pub fn run_health_action(&mut self, _cwd: &Path) -> io::Result<()> {
        use packages::health::HealthAction;

        let Some(view) = self.packages.as_ref() else {
            return Ok(());
        };
        let Some(item) = view.health.get(view.health_selected) else {
            return Ok(());
        };
        let title = item.title.to_string();
        let action = item.action.clone();

        match action {
            HealthAction::None => {
                if let Some(view) = self.packages.as_mut() {
                    view.message = format!("{title}：没什么要做的");
                }
            }
            HealthAction::RemoveOrphans(names) => {
                let (program, argv) = packages::orphan_remove_command(&names);
                let (program, argv) = packages::escalate(&program, &argv);
                if let Some(view) = self.packages.as_mut() {
                    view.confirm = Some(crate::app::package_view::Confirm {
                        title: format!("卸载 {} 个孤儿包", names.len()),
                        command: packages::command_preview(&program, &argv),
                        notes: vec![
                            String::from("孤儿 = 没人依赖、你也没点名装过"),
                            names.join("  "),
                        ],
                        action: crate::app::package_view::ConfirmAction::Orphans(names),
                        pending: false,
                    });
                }
            }
            HealthAction::ClearCache => self.arm_cache(),
            HealthAction::Update => self.arm_upgrade(),
            HealthAction::CheckFiles => {
                if let Some(view) = self.packages.as_mut() {
                    view.start_file_integrity();
                }
            }
            HealthAction::Show(lines) => {
                if let Some(view) = self.packages.as_mut() {
                    view.pending_view = Some((title, lines));
                }
            }
        }
        Ok(())
    }

    /// 关闭包管理视图（历史与队列都落盘）。
    pub fn close_packages(&mut self) {
        if let Some(view) = self.packages.take() {
            view.persist_history();
            if !view.queue.is_empty() {
                let _ = packages::save_queue_to(&packages::queue_path(), &view.queue);
            }
        }
    }

    /// `Enter`：把队列马上要跑的命令摆到确认面板上（再按一次 Enter 才真跑）。
    ///
    /// 这一步**不执行任何东西**：dry-run 的意义就在于「先看清楚」。
    pub fn arm_queue(&mut self) {
        if let Some(view) = self.packages.as_mut() {
            view.arm_queue();
        }
    }

    /// `U`：系统更新的确认面板。
    pub fn arm_upgrade(&mut self) {
        if let Some(view) = self.packages.as_mut() {
            view.arm_upgrade();
        }
    }

    /// `c`：清缓存的确认面板。
    ///
    /// 保留几个版本默认来自 `packages.toml`（`cache_keep`），确认面板里 `[` `]`
    /// 还能当场改这一次的。
    pub fn arm_cache(&mut self) {
        if let Some(view) = self.packages.as_mut() {
            let keep = view.cache_keep;
            view.arm_cache(keep);
        }
    }

    /// `O`：清孤儿的确认面板（名单现查，`pacman -Qtdq`）。
    pub fn arm_orphans(&mut self) {
        if let Some(view) = self.packages.as_mut() {
            view.arm_orphans();
        }
    }

    /// 确认面板上按 Enter：真的跑。演练模式下只把命令写进消息。
    pub fn run_confirm(&mut self, cwd: &Path) -> io::Result<()> {
        let Some(view) = self.packages.as_mut() else {
            return Ok(());
        };
        let Some(confirm) = view.take_confirm() else {
            return Ok(());
        };

        if view.dry_run {
            view.message = format!("演练（没有执行）：{}", confirm.command);
            return Ok(());
        }

        // 先把要跑的东西算出来，之后就得松开对 view 的借用（执行要 &mut self）。
        let plan = match &confirm.action {
            ConfirmAction::Queue(operation) => {
                let names = view.queue_names();
                if names.is_empty() {
                    view.message = String::from("队列是空的");
                    return Ok(());
                }
                Plan::Queue {
                    operation: *operation,
                    uses_aur: view.queue_has_aur(),
                    names,
                }
            }
            ConfirmAction::Upgrade => Plan::Upgrade {
                program: if packages::probe::has_paru() {
                    "paru"
                } else {
                    "pacman"
                },
            },
            ConfirmAction::Cache(keep) => {
                let (program, argv) =
                    packages::cache_command(*keep, packages::probe::has_paccache());
                let (program, argv) = packages::escalate(&program, &argv);
                Plan::Cache { program, argv }
            }
            ConfirmAction::Orphans(names) => Plan::Orphans {
                names: names.clone(),
            },
        };

        match plan {
            Plan::Queue {
                operation,
                uses_aur,
                names,
            } => {
                let (program, argv) =
                    packages::escalate(operation.program(uses_aur), &operation.argv(&names));
                let done = format!("{} 个包已{}", names.len(), operation.label());
                if self.run_system_command(&program, &argv, cwd, &done)? {
                    if let Some(view) = self.packages.as_mut() {
                        view.clear_queue();
                        // 装/卸都改变了本地状态：已安装列表作废，搜索结果重标一次
                        view.installed_loaded = false;
                        if matches!(
                            operation,
                            PackageOperation::Install | PackageOperation::Remove
                        ) {
                            view.start_installed();
                            if view.searched.is_some() {
                                view.start_search();
                            }
                        }
                    }
                    let _ = packages::save_queue_to(&packages::queue_path(), &[]);
                }
            }
            Plan::Upgrade { program } => {
                let (program, argv) =
                    packages::escalate(program, &[String::from(packages::UPGRADE_FLAG)]);
                if self.run_system_command(&program, &argv, cwd, "系统更新完成")?
                    && let Some(view) = self.packages.as_mut()
                {
                    view.pending_updates = Some(0);
                }
            }
            Plan::Cache { program, argv } => {
                self.run_system_command(&program, &argv, cwd, "缓存清理完成")?;
            }
            Plan::Orphans { names } => {
                let (program, argv) = packages::orphan_remove_command(&names);
                let (program, argv) = packages::escalate(&program, &argv);
                let done = format!("{} 个孤儿包已卸载", names.len());
                if self.run_system_command(&program, &argv, cwd, &done)?
                    && let Some(view) = self.packages.as_mut()
                {
                    view.installed_loaded = false;
                    view.start_installed();
                }
            }
        }
        Ok(())
    }

    /// 跑一条**会改系统**的命令：把终端交给它（sudo 与 Y/n 都在那里回），
    /// 回来后整屏重画并把结果写进状态行。返回是否成功。
    fn run_system_command(
        &mut self,
        program: &str,
        argv: &[String],
        cwd: &Path,
        done: &str,
    ) -> io::Result<bool> {
        let result = runtime::run_in_terminal(program, argv, cwd);
        // 它接管过终端：回来必须整屏重画。
        self.request_full_redraw();

        let (ok, message) = match result {
            Ok(_) => (true, done.to_string()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                (false, format!("系统里没有 {program}"))
            }
            Err(error) => (false, format!("{program} 跑失败：{error}")),
        };
        if let Some(view) = self.packages.as_mut() {
            view.message = message;
        }
        Ok(ok)
    }

    /// `Ctrl+X`：把 AUR 的 PKGBUILD 拉下来，用输出视图看（装之前该瞄一眼）。
    pub fn show_pkgbuild(&mut self, cwd: &Path) -> io::Result<()> {
        let Some(view) = self.packages.as_ref() else {
            return Ok(());
        };
        let Some(name) = view.selected_hit().map(|hit| hit.name.clone()) else {
            return Ok(());
        };

        let program = PathBuf::from("paru");
        let argv = vec![String::from("-Gp"), name.clone()];
        let captured = runtime::run_captured(&program, &argv, cwd, &format!("PKGBUILD {name}"))?;
        self.request_full_redraw();

        if let Some(viewer) = Viewer::from_captured(std::slice::from_ref(&captured)) {
            self.open_viewer(viewer);
        } else if let Some(view) = self.packages.as_mut() {
            view.message = format!("没拿到 {name} 的 PKGBUILD（{name} 是 AUR 包吗）");
        }
        Ok(())
    }

    /// `Ctrl+K`：PKGBUILD 检查 —— `paru -Gp` 取下来，再过一遍 shellcheck 与 namcap，
    /// 三段一起丢进输出视图（pacsea 的 Show PKGBUILD / ShellCheck / Namcap 就是这个）。
    pub fn check_pkgbuild(&mut self, cwd: &Path) -> io::Result<()> {
        let Some(name) = self
            .packages
            .as_ref()
            .and_then(|view| view.selected_hit().map(|hit| hit.name.clone()))
        else {
            return Ok(());
        };

        if let Some(view) = self.packages.as_mut() {
            view.message = format!("正在取 {name} 的 PKGBUILD 并检查…");
        }

        let paru = PathBuf::from("paru");
        let fetched = runtime::run_captured(
            &paru,
            &[String::from("-Gp"), name.clone()],
            cwd,
            &format!("PKGBUILD {name}"),
        )?;
        self.request_full_redraw();

        if !fetched.success || fetched.stdout.trim().is_empty() {
            if let Some(view) = self.packages.as_mut() {
                view.message = format!("没拿到 {name} 的 PKGBUILD（它是 AUR 包吗？网络通吗？）");
            }
            return Ok(());
        }

        // 写到临时文件：检查工具要的是文件，不是管道。
        let path = std::env::temp_dir().join(format!("toolbox-hub-{name}-PKGBUILD"));
        let _ = std::fs::write(&path, &fetched.stdout);

        let mut captures = vec![fetched];
        for (program, label) in [("shellcheck", "shellcheck"), ("namcap", "namcap")] {
            let captured = runtime::run_captured(
                &PathBuf::from(program),
                &[path.display().to_string()],
                cwd,
                label,
            );
            match captured {
                Ok(mut captured) => {
                    // shellcheck 发现问题是**退出码 1**：那是检查成功、有告警，
                    // 不是「命令失败」（不然头部会写着「失败 N 个」，误导）。
                    if program == "shellcheck" && captured.status == Some(1) {
                        captured.success = true;
                        captured.label = format!("{program}（有告警）");
                    }
                    captures.push(captured);
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    // 没装就明说，别装作检查过了 —— 但也不算「失败」
                    captures.push(runtime::Captured {
                        label: format!("{program}（没装）"),
                        command: format!("{program}（没装：pacman -S {program}）"),
                        stdout: format!(
                            "没装 {program}，这一步跳过了。\n装上它：pacman -S {program}"
                        ),
                        stderr: String::new(),
                        status: Some(0),
                        success: true,
                        elapsed: std::time::Duration::ZERO,
                        cancelled: false,
                    });
                }
                Err(error) => return Err(error),
            }
        }

        if let Some(viewer) = Viewer::from_captured(&captures) {
            self.open_viewer(viewer);
        }
        Ok(())
    }

    /// `o`：在浏览器里打开 AUR 页面（评论、投票、看依赖都在那儿，需要你的登录）。
    pub fn open_aur_page(&mut self) {
        let Some(name) = self
            .packages
            .as_ref()
            .and_then(|view| view.selected_hit().map(|hit| hit.name.clone()))
        else {
            return;
        };
        let url = format!("https://aur.archlinux.org/packages/{name}");
        let message = match packages::probe::open_in_browser(&url) {
            Ok(()) => format!("已在浏览器打开 {url}"),
            Err(error) => error,
        };
        if let Some(view) = self.packages.as_mut() {
            view.message = message;
        }
    }

    /// 新闻模式按 Enter：在浏览器里打开选中的那条。
    pub fn open_news_link(&mut self) {
        let Some(item) = self
            .packages
            .as_ref()
            .and_then(|view| view.selected_news().cloned())
        else {
            return;
        };
        if item.link.trim().is_empty() {
            if let Some(view) = self.packages.as_mut() {
                view.message = String::from("这条新闻没有链接");
            }
            return;
        }
        let message = match packages::probe::open_in_browser(&item.link) {
            Ok(()) => format!("已打开 {}", item.link),
            Err(error) => error,
        };
        if let Some(view) = self.packages.as_mut() {
            view.message = message;
        }
    }

    // ── 文件管理器（yazi） ────────────────────────────────────────────────

    /// 按 `y`：把终端交给文件管理器（默认 yazi），**回来时工作目录跟着你走**。
    ///
    /// 这就是「先逛目录、再用脚本」的粘合剂：yazi 退出时会把所在目录写进
    /// `--cwd-file`，在里面「打开」过的文件会写进 `--chooser-file`。
    /// 于是你可以在 yazi 里一路翻到素材目录，回到工具箱按 Enter 就能跑脚本了。
    pub fn browse_with_file_manager(&mut self) -> io::Result<()> {
        let program = std::env::var(FILE_MANAGER_ENV).unwrap_or_else(|_| String::from("yazi"));

        match runtime::browse_directories(&program, &self.work_dir) {
            Ok(browsed) => {
                // 它接管过终端：回来必须整屏重画。
                self.request_full_redraw();
                self.apply_browsed(browsed);
                Ok(())
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                self.message = format!("没装 {program} · 先用内置的文件视图看看（F）");
                self.open_files();
                Ok(())
            }
            Err(error) => {
                self.request_full_redraw();
                self.message = format!("启动 {program} 失败：{error}");
                Err(error)
            }
        }
    }

    /// 把文件管理器的反馈落到工作目录上（抽出来是为了能测 —— 真跑要一个 TTY）。
    pub fn apply_browsed(&mut self, browsed: runtime::BrowsedBack) {
        // 在里面「打开」过文件 → 用它的目录，这样脚本立刻就能看到它。
        if let Some(file) = browsed.chosen.first() {
            let name = file
                .file_name()
                .map(|name| name.to_string_lossy().to_string())
                .unwrap_or_default();
            if let Some(dir) = file.parent().map(Path::to_path_buf) {
                self.set_work_dir(dir, &format!("文件管理器里选中 {name}"));
                return;
            }
        }

        match browsed.cwd {
            Some(dir) => self.set_work_dir(dir, "文件管理器返回"),
            None => {
                self.message =
                    String::from("文件管理器没报告目录（工作目录不变，直接按 Enter 跑脚本即可）");
            }
        }
    }

    // ── 工作目录里的媒体文件 ──────────────────────────────────────────────

    /// 重新数一遍工作目录里的媒体文件。
    ///
    /// 规则和 FFTools 脚本的 `fd` 调用**完全一致**（后缀、排除目录都复刻），
    /// 所以「这里显示几个」就是「脚本能看到几个」。有上限也有时间预算，不会卡界面。
    pub fn refresh_media(&mut self) {
        self.media = media::scan(&self.work_dir);
        self.media_root = self.work_dir.clone();
    }

    /// 头部那一句：`12 个媒体文件` / `⚠ 没有媒体文件` / `≥500 个媒体文件`。
    pub fn media_label(&self) -> String {
        if self.media_root != self.work_dir {
            return String::from("数一数中…");
        }
        if self.media.files.is_empty() {
            return String::from("⚠ 没有媒体文件");
        }
        if self.media.truncated {
            format!("≥{} 个媒体文件", self.media.files.len())
        } else {
            format!("{} 个媒体文件", self.media.files.len())
        }
    }

    /// 按 `F`：看看这个目录里到底有什么（脚本会看到的就是这些）。
    pub fn open_files(&mut self) {
        self.refresh_media();
        let label = self.media_label();
        self.files = Some(FilesView {
            root: self.work_dir.clone(),
            all: self.media.files.clone(),
            visible: Vec::new(),
            selected: 0,
            filter: String::new(),
            truncated: self.media.truncated,
        });
        if let Some(files) = self.files.as_mut() {
            files.apply_filter();
        }
        self.message = format!("{label} · Enter 切到该文件所在目录");
    }

    pub fn close_files(&mut self) {
        self.files = None;
        // 解码后的位图可能几十 MB，关掉视图就放掉它
        self.preview.clear();
        self.message = String::from("已关闭文件视图");
    }

    pub fn files_move(&mut self, delta: isize) {
        let Some(view) = self.files.as_mut() else {
            return;
        };
        if view.visible.is_empty() {
            view.selected = 0;
            return;
        }
        let len = view.visible.len() as isize;
        view.selected = (view.selected as isize + delta).rem_euclid(len) as usize;
    }

    pub fn files_push(&mut self, ch: char) {
        if let Some(view) = self.files.as_mut() {
            view.filter.push(ch);
            view.selected = 0;
            view.apply_filter();
        }
    }

    /// 退格：过滤词非空就删一个字。
    pub fn files_backspace(&mut self) {
        if let Some(view) = self.files.as_mut() {
            view.filter.pop();
            view.selected = 0;
            view.apply_filter();
        }
    }

    /// 选中文件 → **把它所在目录设成工作目录**。
    ///
    /// 这一招对**全部 33 个脚本**都管用（不管它认不认参数）：脚本扫的就是工作目录，
    /// 目录对了，它自然就看见这个文件了。
    pub fn files_enter(&mut self) {
        let Some(file) = self
            .files
            .as_ref()
            .and_then(|view| view.selected())
            .cloned()
        else {
            return;
        };
        let dir = file
            .path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| self.work_dir.clone());
        self.files = None;
        self.set_work_dir(dir, &format!("已切到 {}", file.name));
    }

    /// 换工作目录（改目录输入、文件视图都走这里，保证该记的都记了）。
    pub fn set_work_dir(&mut self, path: PathBuf, note: &str) {
        self.work_dir = path.clone();
        self.state.work_dir = Some(path.clone());
        self.remember_dir(&path);
        self.media = media::ScanResult::default();
        self.media_root = PathBuf::new();
        self.refresh_media();

        self.message = match state::save_to(&self.state_path, &self.state) {
            Ok(()) => format!(
                "{note} · 工作目录 → {}（{}）",
                path.display(),
                self.media_label()
            ),
            Err(error) => format!(
                "{note} · 工作目录 → {}（状态没写进去：{error}）",
                path.display()
            ),
        };
    }

    // ── 帮助屏 ────────────────────────────────────────────────────────────

    pub fn is_help_open(&self) -> bool {
        self.help.is_some()
    }

    pub fn open_help(&mut self) {
        self.help = Some(0);
    }

    pub fn close_help(&mut self) {
        self.help = None;
    }

    pub fn help_scroll(&self) -> usize {
        self.help.unwrap_or(0)
    }

    /// 滚动帮助内容（负数向上，不会滚成负的）。
    pub fn scroll_help(&mut self, delta: isize) {
        if let Some(scroll) = self.help.as_mut() {
            *scroll = (*scroll as isize + delta).max(0) as usize;
        }
    }

    // ── 后台执行 ──────────────────────────────────────────────────────────

    pub fn is_running(&self) -> bool {
        self.running.is_some()
    }

    /// 把几件工具排进后台队列（第一件立刻开跑）。
    ///
    /// 捕获模式的工具走这里：界面**不会**卡住，还能看实时输出与进度、随时取消。
    pub fn enqueue_captures(&mut self, jobs: Vec<CaptureRequest>) {
        for request in jobs {
            let tool = request.tool;
            // 历史里的命令**要带程序名**才能直接重跑 —— 这里绕过 record_run，
            // 所以得自己补上（这个坑在同步路径上踩过一次）。
            let mut record_argv = request.record_argv;
            record_argv.insert(0, tool.path.display().to_string());

            self.job_queue.push_back(PendingJob {
                tool_id: tool.id.clone(),
                tool_name: tool.name.clone(),
                program: tool.path.clone(),
                argv: request.argv,
                record_argv,
                values: request.values,
                ok_exit_codes: request.ok_exit_codes,
                total_seconds: request.total_seconds,
            });
        }

        if self.running.is_none() {
            let cwd = self.work_dir.clone();
            self.start_next_job(&cwd);
        }
    }

    /// 起队列里的下一件。
    fn start_next_job(&mut self, cwd: &Path) {
        let Some(pending) = self.job_queue.pop_front() else {
            return;
        };

        match runtime::spawn_captured(&pending.program, &pending.argv, cwd, &pending.tool_name) {
            Ok(job) => {
                self.message = format!("执行中 · {} · q 取消", pending.tool_name);
                self.running = Some(RunningJobView {
                    job,
                    total_seconds: pending.total_seconds,
                    tool_id: pending.tool_id,
                    tool_name: pending.tool_name,
                    record_argv: pending.record_argv,
                    values: pending.values,
                    ok_exit_codes: pending.ok_exit_codes,
                    tail: VecDeque::new(),
                    progress: Vec::new(),
                });
            }
            Err(error) => {
                // 起不来只报一声，别把整个界面带下去；顺便试下一件。
                self.message = format!("无法执行 {}: {error}", pending.tool_name);
                let cwd = cwd.to_path_buf();
                self.start_next_job(&cwd);
            }
        }
    }

    /// 主循环每帧调一次：把后台任务的事件取干净。
    ///
    /// 取到 `Done` 就收尾：写历史、起队列里的下一件、都跑完了开输出视图。
    /// 收后台任务的进度与输出。返回 `true` 表示「这一帧有别的东西要画」。
    ///
    /// 主循环拿它决定要不要 `terminal.draw`：后台在跑的时候就一直画（进度条与
    /// 耗时要动），没人干活、也没人按键的时候就**一帧都不画** —— 空闲时每秒十次
    /// 全量重建界面的钱省下来了。
    pub fn poll_job(&mut self, cwd: &Path) -> bool {
        let Some(running) = self.running.as_mut() else {
            return false;
        };

        let mut done: Option<Captured> = None;
        // 任务在跑：耗时/进度每帧都在变，所以这一帧总是要画
        let mut changed = true;
        while let Ok(event) = running.job.events.try_recv() {
            match event {
                runtime::JobEvent::Line { stderr, text } => {
                    running.tail.push_back((stderr, text));
                    while running.tail.len() > TAIL_LINES {
                        running.tail.pop_front();
                    }
                }
                runtime::JobEvent::Progress { key, value } => {
                    match running
                        .progress
                        .iter_mut()
                        .find(|(existing, _)| existing == &key)
                    {
                        Some(slot) => slot.1 = value,
                        None => running.progress.push((key, value)),
                    }
                }
                runtime::JobEvent::Done(captured) => done = Some(*captured),
            }
        }

        if done.is_none() {
            return changed;
        }
        let captured = done.expect("刚判过不是 None");

        // 收尾前先把 running 的借用放掉。
        let Some(running) = self.running.take() else {
            return changed;
        };
        let (tool_id, tool_name, record_argv, values, ok_exit_codes) = (
            running.tool_id,
            running.tool_name,
            running.record_argv,
            running.values,
            running.ok_exit_codes,
        );

        let cancelled = captured.cancelled;
        // 「没匹配到」不等于「失败」：退出码白名单在这里生效
        // （`pacman -Qdt` 没孤儿包时退 1、`checkupdates` 没更新时退 2）。
        let mut captured = captured;
        captured.success = ok_exit_codes.contains(&captured.status.unwrap_or(i32::MIN));
        let success = captured.success;
        self.record_history(
            &tool_id,
            &tool_name,
            &record_argv,
            &values,
            success && !cancelled,
            captured.elapsed.as_millis(),
        );
        self.job_results.push(captured);

        // 还有排队的就接着跑；都跑完了把这一批的输出一起摆出来。
        if self.job_queue.is_empty() {
            let results = std::mem::take(&mut self.job_results);
            let failed = results.iter().filter(|item| !item.success).count();
            self.message = if failed == 0 {
                format!("执行完成 · {} 个", results.len())
            } else {
                format!("执行完成 · {} 个 · {failed} 个失败", results.len())
            };
            if let Some(viewer) = Viewer::from_captured(&results) {
                self.open_viewer(viewer);
            }
        } else {
            self.start_next_job(cwd);
        }
        changed = true;
        changed
    }

    /// 算这次执行的「总时长」（进度条用）。
    ///
    /// 顺序：动作声明的 `limit_from`（裁剪类：这次只做 N 秒）> `duration_from`
    /// 指向那个文件的时长。都拿不到就没有百分比 —— 不猜。
    pub fn total_seconds_for(
        &self,
        action: &crate::model::Action,
        values: &ArgumentValues,
    ) -> Option<f64> {
        if let Some(limit_key) = action.limit_from.as_deref()
            && let Some(seconds) = values
                .get(limit_key)
                .and_then(crate::runtime::parse_duration)
        {
            return Some(seconds);
        }

        let key = action.duration_from.as_deref()?;
        let raw = values.get(key)?.trim();
        if raw.is_empty() {
            return None;
        }

        // 相对路径按工作目录解释 —— 和真正执行时一致。
        let path = if raw.starts_with('/') {
            PathBuf::from(raw)
        } else {
            self.work_dir.join(raw)
        };
        crate::runtime::probe_duration(&path)
    }

    /// 取消正在跑的任务（队列里剩下的也一起清掉）。
    pub fn cancel_job(&mut self) {
        let Some(running) = self.running.as_ref() else {
            return;
        };
        running.job.terminate();
        self.job_queue.clear();
        self.job_results.clear();
        self.message = String::from("已请求取消…");
    }

    // ── 文件选择器 ────────────────────────────────────────────────────────

    /// 从当前表单字段打开选择器（只有路径字段才有意义）。
    pub fn open_picker(&mut self) {
        let Some(argument) = self.form_current_argument() else {
            return;
        };
        if argument.kind != ArgKind::Path {
            self.message = format!("「{}」不是路径字段，不用选文件", argument.label);
            return;
        }

        let field = self.form.as_ref().map_or(0, |form| form.field);
        let current = self
            .form_values()
            .and_then(|values| values.get(&argument.key))
            .unwrap_or("")
            .trim()
            .to_string();

        let start = self.picker_start_dir(&current);
        let (repeatable, dir_only) = (argument.repeatable, argument.dir_only);
        self.message = format!("选文件 · 起点 {}", start.display());
        self.picker = Some(Picker::open(&start, field, repeatable, dir_only));
    }

    /// 选文件的起点：字段里已有路径 → 它的目录；否则最近用过的目录；再否则工作目录。
    ///
    /// 「最近用过的目录」和工作目录是两件事：工作目录是**工具在哪儿跑**，
    /// 而这里记的是**你习惯去哪儿翻文件**（经常不是同一个地方）。
    fn picker_start_dir(&self, value: &str) -> PathBuf {
        let work_dir = self.work_dir.clone();

        if !value.is_empty() {
            // 多值字段取最后一条（刚填的那个）；相对路径按工作目录解释，和执行时一致。
            let last = value.rsplit(',').next().unwrap_or(value).trim();
            let candidate = if last.starts_with('/') {
                PathBuf::from(last)
            } else {
                work_dir.join(last)
            };
            let dir = if candidate.is_dir() {
                candidate
            } else {
                candidate
                    .parent()
                    .map(Path::to_path_buf)
                    .unwrap_or_else(|| work_dir.clone())
            };
            if dir.is_dir() {
                return dir;
            }
        }

        if let Some(dir) = self.recent_dirs().first() {
            return dir.clone();
        }
        work_dir
    }

    /// 记一笔最近用过的目录（去重、只留最近 [`RECENT_DIRS`] 个）。**不落盘**，
    /// 由调用方决定要不要、以及怎么报错。
    fn remember_dir(&mut self, dir: &Path) {
        let dir = dir.to_path_buf();
        self.state.recent_dirs.retain(|item| item != &dir);
        self.state.recent_dirs.insert(0, dir);
        self.state.recent_dirs.truncate(RECENT_DIRS);
    }

    /// 尽力存一下状态：失败只留个话，不影响正在做的事。
    fn save_state_quietly(&mut self) {
        if let Err(error) = state::save_to(&self.state_path, &self.state) {
            self.message.push_str(&format!("（状态没写进去：{error}）"));
        }
    }

    /// 最近用过、**现在还存在**的目录（新的在前）。
    pub fn recent_dirs(&self) -> Vec<PathBuf> {
        self.state
            .recent_dirs
            .iter()
            .filter(|dir| dir.is_dir())
            .cloned()
            .collect()
    }

    /// Tab：标记/取消标记当前文件。
    pub fn picker_toggle_mark(&mut self) {
        if let Some(picker) = self.picker.as_mut() {
            picker.toggle_mark();
        }
    }

    /// Ctrl-D：把**当前目录**填进字段（要填目录的那些字段就是靠这个）。
    pub fn picker_use_current_dir(&mut self) {
        let Some(dir) = self
            .picker
            .as_ref()
            .map(|picker| picker.dir().to_path_buf())
        else {
            return;
        };
        self.write_paths_into_form_paths(vec![dir], false);
        self.picker = None;
    }

    pub fn close_picker(&mut self) {
        self.picker = None;
        self.message = String::from("已取消选择");
    }

    pub fn picker_move(&mut self, delta: isize) {
        if let Some(picker) = self.picker.as_mut() {
            picker.move_selection(delta);
        }
    }

    pub fn picker_select_first(&mut self) {
        if let Some(picker) = self.picker.as_mut() {
            picker.select_first();
        }
    }

    pub fn picker_select_last(&mut self) {
        if let Some(picker) = self.picker.as_mut() {
            picker.select_last();
        }
    }

    /// ←：上翻一层目录。
    pub fn picker_parent(&mut self) {
        if let Some(picker) = self.picker.as_mut() {
            picker.go_to_parent();
        }
    }

    /// 退格：过滤词非空就删一个字，否则上翻一层目录。
    pub fn picker_backspace(&mut self) {
        if let Some(picker) = self.picker.as_mut() {
            picker.backspace();
        }
    }

    pub fn picker_push(&mut self, ch: char) {
        if let Some(picker) = self.picker.as_mut() {
            picker.push_char(ch);
        }
    }

    /// 回车：目录就进去，文件就填进字段。
    pub fn picker_enter(&mut self) {
        let Some(entry) = self
            .picker
            .as_ref()
            .and_then(|picker| picker.selected_entry().cloned())
        else {
            return;
        };

        if entry.is_dir {
            if let Some(picker) = self.picker.as_mut() {
                picker.enter_dir(&entry.path);
            }
            return;
        }
        self.picker_choose();
    }

    /// 把选中的文件填进目标字段。
    pub fn picker_choose(&mut self) {
        let Some(picker) = self.picker.as_ref() else {
            return;
        };
        let (field, repeatable) = (picker.field, picker.repeatable);

        // 有标记就填标记的那些（按列表顺序），否则填当前高亮这一项。
        let mut chosen = picker.marked_files();
        if chosen.is_empty() {
            match picker.selected_entry() {
                Some(entry) if !entry.is_dir => chosen.push(entry.path.clone()),
                // 高亮的是目录：什么都不做（进目录由 picker_enter 负责）。
                Some(_) => return,
                None => return,
            }
        }

        if !repeatable && chosen.len() > 1 {
            self.message = format!(
                "这个字段只能填一个，但标记了 {} 个（多选请用多值字段）",
                chosen.len()
            );
            return;
        }

        // 记住这次是从哪儿翻出来的，下次直接到这儿。
        if let Some(parent) = chosen.first().and_then(|path| path.parent()) {
            let parent = parent.to_path_buf();
            self.remember_dir(&parent);
            self.save_state_quietly();
        }

        self.write_paths_into_form(field, &chosen, repeatable);
        self.picker = None;
    }

    /// Ctrl-U：清空目标字段（多值字段想重选时用）。
    pub fn picker_clear_field(&mut self) {
        let Some(field) = self.picker.as_ref().map(|picker| picker.field) else {
            return;
        };
        let Some(argument) = self.form_argument(field) else {
            return;
        };
        if let Some(form) = self.form.as_mut() {
            form.values.set(&argument.key, String::new());
        }
        self.form_disarm();
        self.message = format!("已清空「{}」", argument.label);
    }

    /// 表单当前工具的第 `index` 个字段。
    pub fn form_argument(&self, index: usize) -> Option<Argument> {
        self.form_tool()?
            .action
            .as_ref()?
            .arguments
            .get(index)
            .cloned()
    }

    /// 把一批路径写进第 `field` 个字段：单值替换、多值追加、已选过的不重复加。
    /// 把填好的表单展开成**一次或多次**执行。
    ///
    /// 动作声明了 `foreach = "input"` 时：那个参数的**每个取值各跑一次**，
    /// 其余字段里的 `{name}` / `{stem}` / `{ext}` / `{dir}` 按当前那个文件替换 ——
    /// 所以输出可以写成 `{stem}_small.mp4`，选 3 个文件就得到 3 个输出。
    ///
    /// 没声明 `foreach` 就是普通的一次执行（`repeatable` 那种「一条命令塞多个参数」
    /// 走的是 `build_argv`，不经过这里）。
    pub fn form_build_runs(&self) -> Result<Vec<(Vec<String>, ArgumentValues)>, String> {
        let tool = self
            .form_tool()
            .ok_or_else(|| String::from("没有打开的表单"))?;
        let action = tool
            .action
            .as_ref()
            .ok_or_else(|| String::from("这件工具不需要参数"))?;
        let values = self.form_values().cloned().unwrap_or_default();

        let Some(key) = action.foreach.as_deref() else {
            return Ok(vec![(action.build_argv(&values)?, values)]);
        };

        let items = action
            .arguments
            .iter()
            .find(|argument| argument.key == key)
            .map(|argument| argument.split_values(values.get(key).unwrap_or("")))
            .unwrap_or_default();
        if items.is_empty() {
            return Err(format!("「{key}」是批量字段，至少要填一条"));
        }

        let mut runs = Vec::with_capacity(items.len());
        for item in &items {
            let path = Path::new(item);
            let name = path
                .file_name()
                .map(|name| name.to_string_lossy().to_string())
                .unwrap_or_else(|| item.clone());
            let stem = path
                .file_stem()
                .map(|stem| stem.to_string_lossy().to_string())
                .unwrap_or_else(|| name.clone());
            let ext = path
                .extension()
                .map(|ext| ext.to_string_lossy().to_string())
                .unwrap_or_default();
            let dir = path
                .parent()
                .map(|dir| dir.display().to_string())
                .unwrap_or_default();

            let mut one = values.clone();
            for argument in &action.arguments {
                if argument.key == key {
                    continue;
                }
                let Some(value) = one.get(&argument.key).map(str::to_string) else {
                    continue;
                };
                let replaced = value
                    .replace("{name}", &name)
                    .replace("{stem}", &stem)
                    .replace("{ext}", &ext)
                    .replace("{dir}", &dir);
                if replaced != value {
                    one.set(&argument.key, replaced);
                }
            }
            one.set(key, item.clone());
            runs.push((action.build_argv(&one)?, one));
        }
        Ok(runs)
    }

    fn write_paths_into_form(&mut self, field: usize, paths: &[PathBuf], repeatable: bool) {
        let Some(argument) = self.form_argument(field) else {
            return;
        };

        let current = self
            .form_values()
            .and_then(|values| values.get(&argument.key))
            .unwrap_or("")
            .trim()
            .to_string();

        // 非多值字段是「替换」，所以起点是空的。
        let mut items: Vec<String> = if repeatable && !current.is_empty() {
            current
                .split(&argument.separator)
                .map(|item| item.trim().to_string())
                .filter(|item| !item.is_empty())
                .collect()
        } else {
            Vec::new()
        };

        let mut added: Vec<String> = Vec::new();
        for path in paths {
            let text = self.display_path(path);
            if items.iter().any(|item| item == &text) {
                continue;
            }
            items.push(text.clone());
            added.push(text);
        }

        // 单值字段只认第一条（调用方已经拦过「标记了多个」的情况）。
        let value = if repeatable {
            let separator = if argument.separator == "," {
                ", "
            } else {
                &argument.separator
            };
            items.join(separator)
        } else {
            items.first().cloned().unwrap_or_default()
        };
        if let Some(form) = self.form.as_mut() {
            form.values.set(&argument.key, value);
        }
        self.form_disarm();

        self.message = match added.len() {
            0 => String::from("这些已经在字段里了"),
            1 => format!("已选 {}", added[0]),
            count => format!("已选 {count} 个：{}", added.join("、")),
        };
    }

    /// [`App::picker_use_current_dir`] 用的入口：只有一个目录，且按「单值」写。
    fn write_paths_into_form_paths(&mut self, paths: Vec<PathBuf>, repeatable: bool) {
        let Some(field) = self.picker.as_ref().map(|picker| picker.field) else {
            return;
        };
        self.write_paths_into_form(field, &paths, repeatable);
    }

    /// 显示路径：在**工作目录**里就用相对路径（和执行时一致），否则用绝对路径。
    pub fn display_path(&self, path: &Path) -> String {
        if let Ok(relative) = path.strip_prefix(&self.work_dir) {
            let text = relative.display().to_string();
            if !text.is_empty() {
                return text;
            }
        }
        path.display().to_string()
    }

    // ── 整屏重画 ──────────────────────────────────────────────────────────

    /// 请求下一帧整屏重画。
    ///
    /// **接管过终端以后必须调用**：ratatui 是双缓冲差分渲染，它的内部缓冲还记着接管前
    /// 那一帧，而 `LeaveAlternateScreen` 已经把物理屏清空了 —— 不丢掉旧帧的话，下一次
    /// `draw` 会认为「屏幕没变化」而一个格子都不写，用户看到的就是一片空白加一行状态。
    /// （复现与验证见 `ui` 模块里的 `a_wiped_screen_needs_a_clear_before_the_next_draw`。）
    pub fn request_full_redraw(&mut self) {
        self.needs_full_redraw = true;
    }

    /// 取出并清掉重画请求（主循环每帧问一次）。
    pub fn take_full_redraw(&mut self) -> bool {
        std::mem::take(&mut self.needs_full_redraw)
    }

    // ── 收藏与视图 ────────────────────────────────────────────────────────

    pub fn is_favorite(&self, tool_id: &str) -> bool {
        self.state.is_favorite(tool_id)
    }

    pub fn favorite_count(&self) -> usize {
        self.state.favorite_count()
    }

    /// 切换当前选中工具的收藏状态。
    pub fn toggle_favorite_current(&mut self) {
        let Some(tool) = self
            .filtered
            .get(self.selected)
            .and_then(|&index| self.registry.tools().get(index))
        else {
            self.message = String::from("没有选中任何工具");
            return;
        };
        let id = tool.id.clone();
        let name = tool.name.clone();

        self.message = if self.state.toggle_favorite(&id) {
            format!("已收藏 {name}")
        } else {
            format!("已取消收藏 {name}")
        };
        if let Err(error) = state::save_to(&self.state_path, &self.state) {
            self.message
                .push_str(&format!("（但没写进状态文件：{error}）"));
        }

        // 在收藏视图里取消收藏，这条就该立刻消失。
        if self.scope == Scope::Favorites {
            self.apply_filter();
        }
    }

    /// 切换视图：全部 → ★ 收藏 → 最近使用 → 全部。
    pub fn cycle_scope(&mut self) {
        self.scope = self.scope.next();
        self.sub = 0;
        self.selected = 0;
        self.apply_filter();

        let hits = self.filtered.len();
        self.message = match (self.scope, hits) {
            (Scope::Favorites, 0) if self.favorite_count() == 0 => {
                String::from("还没有收藏 · 在列表里按 f 收藏当前工具")
            }
            (Scope::Recent, 0) => String::from("还没有执行历史 · 跑一次工具就会出现在这里"),
            (scope, count) => format!("{} · {count} 个", scope.label()),
        };
    }

    /// 最近用过的工具 id，新的在前。
    ///
    /// 直接从执行历史里推，不额外存一份 —— 历史已经在记了。
    pub fn recent_tool_ids(&self) -> Vec<String> {
        let mut ids: Vec<String> = Vec::new();
        for entry in history::load(200) {
            if !ids.contains(&entry.tool_id) {
                ids.push(entry.tool_id);
            }
        }
        ids
    }

    // ── 执行历史 ──────────────────────────────────────────────────────────

    pub fn open_history(&mut self) {
        let view = HistoryView::open();
        self.message = if view.entries.is_empty() {
            String::from("还没有执行历史")
        } else {
            format!("执行历史 · 最近 {} 条", view.entries.len())
        };
        self.history = Some(view);
    }

    pub fn close_history(&mut self) {
        self.history = None;
    }

    pub fn history_move(&mut self, delta: isize) {
        let Some(view) = self.history.as_mut() else {
            return;
        };
        if view.entries.is_empty() {
            return;
        }
        let len = view.entries.len() as isize;
        view.selected = (view.selected as isize + delta).rem_euclid(len) as usize;
    }

    pub fn history_selected(&self) -> Option<history::Entry> {
        self.history.as_ref()?.selected_entry().cloned()
    }

    /// 把历史里那次的**参数填回表单**（然后改一改再跑）。
    ///
    /// 和「原样重跑」是两件事：重跑是把 argv 直接执行一遍，回填是让你接着编辑。
    pub fn refill_from_history(&mut self) {
        let Some(entry) = self.history_selected() else {
            return;
        };

        let Some(index) = self
            .registry
            .tools()
            .iter()
            .position(|tool| tool.id == entry.tool_id)
        else {
            self.message = format!("这件工具已经不在列表里了（{}）", entry.tool_id);
            return;
        };

        // 它可能不在当前视图里，所以先把筛选清掉、切到它所在的域。
        self.scope = Scope::All;
        self.query.clear();
        self.searching = false;
        let domain = self.registry.tools()[index].domain;
        self.switch_domain(domain.index());

        let Some(position) = self.filtered.iter().position(|&item| item == index) else {
            self.message = format!("在列表里找不到 {} —— 换个域或清掉筛选再试", entry.tool_name);
            return;
        };
        self.selected = position;
        self.table.select(Some(position));

        if !self.open_form() {
            self.message = format!("{} 不需要填参数，直接重跑就行（Enter）", entry.tool_name);
            return;
        }

        // 用当时的值覆盖缺省值
        let refilled = entry.values.len();
        if let Some(form) = self.form.as_mut() {
            for (key, value) in &entry.values {
                form.values.set(key, value);
            }
        }
        self.close_history();
        self.message = if refilled == 0 {
            format!("{} 那次没留下参数（老记录），这里是缺省值", entry.tool_name)
        } else {
            format!(
                "已把 {} 的 {refilled} 个参数填回表单 · 改完 Ctrl-E 执行",
                entry.tool_name
            )
        };
    }

    /// 记一条执行历史，**自动把程序名放在 argv 最前面**。
    ///
    /// 历史里的命令要能直接重跑，所以必须带上程序名 —— 少了它，重跑时会把
    /// 第一个参数当成命令（这个坑是实拍历史记录时发现的）。
    pub fn record_run(&self, tool: &ToolDefinition, argv: &[String], success: bool, millis: u128) {
        let mut full = vec![tool.path.display().to_string()];
        full.extend(argv.iter().cloned());
        // 表单还开着的时候顺手把取值抄一份 —— 历史里的「回填再改」全靠它。
        let values = self
            .form_values()
            .map(|values| values.pairs())
            .unwrap_or_default();
        self.record_history(&tool.id, &tool.name, &full, &values, success, millis);
    }

    /// 记一条执行历史（`argv` 必须已含程序名）。
    ///
    /// `argv` 必须是**已经做过敏感值替换**的（用 [`crate::model::Action::redacted`]）。
    /// 历史写不进去只报一声，绝不影响执行本身。
    pub fn record_history(
        &self,
        tool_id: &str,
        tool_name: &str,
        argv: &[String],
        values: &[(String, String)],
        success: bool,
        millis: u128,
    ) {
        let entry = history::Entry {
            epoch: history::now_epoch(),
            tool_id: tool_id.to_string(),
            tool_name: tool_name.to_string(),
            argv: argv.to_vec(),
            success,
            millis,
            values: values.iter().cloned().collect(),
        };
        let written = match self.history_path.as_deref() {
            Some(path) => history::append_to(path, &entry),
            None => history::append(&entry),
        };
        if let Err(error) = written {
            eprintln!("Toolbox: 执行历史写入失败: {error}");
        }
    }

    /// 表单里的当前取值（构建 argv 与做敏感值替换都要用）。
    pub fn form_values(&self) -> Option<&ArgumentValues> {
        self.form.as_ref().map(|form| &form.values)
    }

    /// 把输出视图的正文存成文件。
    pub fn viewer_save(&mut self) {
        let Some(viewer) = self.viewer.as_ref() else {
            return;
        };
        let name = viewer.title.clone();
        let body = viewer.body.clone();
        self.message = match history::save_output(&name, &body) {
            Ok(path) => format!("已保存到 {}", path.display()),
            Err(error) => format!("保存失败: {error}"),
        };
    }

    /// 把输出送进系统剪贴板。
    pub fn viewer_copy(&mut self) {
        let Some(viewer) = self.viewer.as_ref() else {
            return;
        };
        self.message = if history::copy_to_clipboard(&viewer.body) {
            String::from("已复制到剪贴板")
        } else {
            String::from("没找到 wl-copy / xclip，复制不了")
        };
    }

    // ── 危险动作确认 ──────────────────────────────────────────────────────

    /// 待执行的目标里有没有需要确认的危险工具。
    pub fn targets_need_confirm(&self) -> bool {
        let tools = self.registry.tools();
        self.execution_targets().iter().any(|&index| {
            tools
                .get(index)
                .is_some_and(|tool| tool.danger.needs_confirm())
        })
    }

    /// 记下「已经问过一次」，第二次回车才真跑。
    pub fn arm_pending_confirmation(&mut self) {
        let targets = self.execution_targets();
        let names: Vec<String> = targets
            .iter()
            .filter_map(|&index| self.registry.tools().get(index))
            .filter(|tool| tool.danger.needs_confirm())
            .map(|tool| tool.name.clone())
            .collect();
        if names.is_empty() {
            return;
        }
        self.message = format!(
            "⚠ {} 会改动文件 · 再按一次 Enter 确认，其它键取消",
            names.join("、")
        );
        self.pending = Some(targets);
    }

    /// 取出「已确认」标记；返回 `true` 表示这次可以真跑了。
    pub fn take_pending_confirmation(&mut self) -> bool {
        self.pending.take().is_some()
    }

    pub fn clear_pending(&mut self) {
        if self.pending.take().is_some() {
            self.message = String::from("已取消");
        }
    }

    /// 表单里的工具要不要再确认一次（危险 + 还没确认过）。
    pub fn form_needs_confirm(&self) -> bool {
        let Some(form) = self.form.as_ref() else {
            return false;
        };
        !form.confirm
            && self
                .form_tool()
                .is_some_and(|tool| tool.danger.needs_confirm())
    }

    /// 记下表单已经确认过。
    pub fn form_arm_confirm(&mut self) {
        let Some(name) = self.form_tool().map(|tool| tool.name.clone()) else {
            return;
        };
        if let Some(form) = self.form.as_mut() {
            form.confirm = true;
        }
        self.message = format!("⚠ {name} 会改动文件 · 再按一次 Ctrl-E 执行");
    }

    /// 任何一次编辑都撤销「已确认」——改了参数就得重新确认。
    pub fn form_disarm(&mut self) {
        if let Some(form) = self.form.as_mut() {
            form.confirm = false;
        }
    }

    /// 表单当前工具的警告文案（安全工具返回 `None`）。
    pub fn form_danger_note(&self) -> Option<String> {
        let tool = self.form_tool()?;
        tool.danger.needs_confirm().then(|| {
            format!(
                "⚠ {} 级：这个动作会改动/覆盖文件，执行前要再确认一次",
                tool.danger.label()
            )
        })
    }

    /// 距离上次刷新经过了多少秒，顶部栏用它显示活动心跳。
    pub fn seconds_since_reload(&self) -> u64 {
        self.last_reload.elapsed().map(|d| d.as_secs()).unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::{App, CaptureRequest, Scope, Viewer};
    use crate::{
        model::{Danger, Domain, RunMode, ToolDefinition},
        registry::{Registry, ReloadReport},
    };

    fn tool(name: &str, domain: Domain, tags: &[&str]) -> ToolDefinition {
        ToolDefinition {
            id: format!("test:{name}"),
            name: name.to_string(),
            provider: "Test".to_string(),
            domain,
            tags: tags.iter().map(|tag| (*tag).to_string()).collect(),
            summary: "说明".to_string(),
            input: None,
            output: None,
            features: None,
            requires: Vec::new(),
            missing_deps: Vec::new(),
            install_hint: None,
            pin: None,
            action: None,
            mode: RunMode::Interactive,
            danger: Danger::Safe,
            path: PathBuf::from(format!("/tmp/{name}")),
            ready: true,
        }
    }

    fn app() -> App {
        let registry = Registry::from_tools(vec![
            tool("trim-video", Domain::Media, &["编辑"]),
            tool("convert-media", Domain::Media, &["转码"]),
            tool("burn-subs", Domain::Media, &["字幕"]),
            tool("shorin", Domain::Tools, &[]),
        ]);
        App::new(registry, PathBuf::from("/tmp/bin"), ReloadReport::default())
    }

    /// 一件普通脚本 + 一件**真实的**带参数动作（取自 Curated Provider）。
    fn action_app() -> App {
        let action_tool = crate::providers::manifest::bundled_tools()
            .into_iter()
            .next()
            .expect("至少应有一个内置动作");
        let registry = Registry::from_tools(vec![
            tool("plain-script", Domain::Media, &["编辑"]),
            action_tool,
        ]);
        App::new(registry, PathBuf::from("/tmp/bin"), ReloadReport::default())
    }

    /// 列表上直接按 Enter（不走表单）也必须把 `base_argv` 带上。
    ///
    /// 这条抓到过真漏：列表/批量那条路曾经把 `argv` 写成空 `Vec`，于是
    /// `journalctl -p err -b` 变成光跑 `journalctl`（dump 整个日志），
    /// `systemctl` / `lsblk` / `lspci` 那几个则是**静默**给出错误结果。
    #[test]
    fn a_no_argument_capture_tool_keeps_its_base_argv() {
        use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

        // 借一件无参的 capture 动作，只换掉程序（换成 echo，真跑也无害）
        // 与 base_argv（放一个能认出来的标记）。
        let mut action_tool = crate::providers::manifest::bundled_tools()
            .into_iter()
            .find(|tool| {
                tool.mode == RunMode::Capture
                    && tool
                        .action
                        .as_ref()
                        .is_some_and(|action| action.arguments.is_empty())
            })
            .expect("system.toml 提供了无参数的 capture 动作");
        // 放进「媒体」域：`App::new` 默认停在第一个域，域不对列表就是空的。
        action_tool.domain = Domain::Media;
        action_tool.path = PathBuf::from("/usr/bin/echo");
        action_tool.ready = true;
        action_tool.missing_deps.clear();
        action_tool.danger = Danger::Safe;
        action_tool.action.as_mut().expect("带动作").base_argv =
            vec![String::from("base-argv-marker")];

        let registry = Registry::from_tools(vec![action_tool]);
        let mut app = App::new(registry, PathBuf::from("/tmp/bin"), ReloadReport::default());
        let cwd = app.work_dir.clone();

        crate::app::handle_key(
            &mut app,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            &cwd,
        )
        .expect("按下 Enter");

        let command = app
            .running
            .as_ref()
            .map(|running| running.job.command.clone())
            .unwrap_or_default();
        assert!(
            command.contains("base-argv-marker"),
            "无参数动作把 base_argv 丢了，实际跑的是：{command}"
        );
        app.cancel_job();
    }

    #[test]
    fn enter_opens_the_form_only_for_tools_that_need_arguments() {
        let mut app = action_app();

        // 第一件是普通脚本：不该弹表单，调用方应该直接执行它。
        assert!(!app.open_form(), "普通脚本不该弹表单");
        assert!(app.form.is_none());

        app.move_selection(1);
        assert!(app.open_form(), "带参数的工具应该打开表单");
        assert_eq!(
            app.form_arguments().len(),
            5,
            "yt-dlp 下载视频应有 5 个字段"
        );
        assert_eq!(app.form_error(), None);
    }

    #[test]
    fn form_edits_text_choices_and_toggles_then_builds_argv() {
        let mut app = action_app();
        app.move_selection(1);
        app.open_form();

        // 字段 0：链接（文本）—— 进输入态、打字、退格、结束
        assert_eq!(
            app.form_current_argument().map(|a| a.key.as_str()),
            Some("url")
        );
        assert_eq!(
            app.form_build(),
            Err("「视频链接」是必填项".to_string()),
            "链接为空时不该能构建"
        );

        app.form_activate();
        assert!(app.form_is_editing());
        for ch in "https://example.com/v!".chars() {
            app.form_push_char(ch);
        }
        app.form_backspace(); // 去掉那个感叹号
        app.form_end_edit();
        assert!(!app.form_is_editing());

        let argv = app.form_build().expect("填了链接就能构建");
        assert_eq!(
            argv.last().map(String::as_str),
            Some("https://example.com/v"),
            "链接作为位置参数落在最后: {argv:?}"
        );

        // 字段 1：画质（Choice）—— 换一个选项，argv 里的取值跟着变
        app.form_move(1);
        app.form_adjust(1);
        let argv = app.form_build().expect("应能构建");
        assert!(
            argv.windows(2)
                .any(|pair| pair[0] == "-f" && pair[1].contains("height<=1080")),
            "换选项后 argv 也要换: {argv:?}"
        );

        // 字段 2：字幕（Toggle）—— 打开后多一个 flag
        app.form_move(1);
        app.form_adjust(1);
        let argv = app.form_build().expect("应能构建");
        assert!(argv.contains(&"--write-subs".to_string()), "{argv:?}");
    }

    #[test]
    fn form_errors_are_recorded_for_display() {
        let mut app = action_app();
        app.move_selection(1);
        app.open_form();

        app.form_set_error(String::from("「视频链接」是必填项"));
        assert_eq!(app.form_error(), Some("「视频链接」是必填项"));

        // 换个字段就把上一次的错误清掉，避免显示过期信息。
        app.form_move(1);
        assert_eq!(app.form_error(), None);
    }

    #[test]
    fn closing_the_form_returns_to_the_list() {
        let mut app = action_app();
        app.move_selection(1);
        app.open_form();

        app.close_form();
        assert!(app.form.is_none());
        assert_eq!(app.message, "已返回列表");
        assert!(app.form_arguments().is_empty());
        assert_eq!(app.form_build(), Err("这件工具没有参数".to_string()));
    }

    /// 一件「危险」工具（内置动作里 caution 的那件）。
    fn danger_app() -> App {
        let dangerous = crate::providers::manifest::bundled_tools()
            .into_iter()
            .find(|tool| tool.danger.needs_confirm())
            .expect("内置动作里应至少有一个 caution 的");
        let mut app = App::new(
            Registry::from_tools(vec![dangerous.clone()]),
            PathBuf::from("/tmp/bin"),
            ReloadReport::default(),
        );
        app.switch_domain(dangerous.domain.index());
        app
    }

    #[test]
    fn dangerous_targets_ask_once_before_running() {
        let mut app = danger_app();
        assert!(app.targets_need_confirm(), "危险工具应该要求确认");

        app.arm_pending_confirmation();
        assert_eq!(app.pending.as_ref().map(Vec::len), Some(1));
        assert!(app.message.contains("再按一次 Enter"), "{}", app.message);

        // 第二次回车才算确认。
        assert!(app.take_pending_confirmation());
        assert!(app.pending.is_none());

        // 取消路径。
        app.arm_pending_confirmation();
        app.clear_pending();
        assert!(app.pending.is_none());
        assert_eq!(app.message, "已取消");
    }

    #[test]
    fn dangerous_form_asks_once_and_any_edit_disarms_it() {
        let mut app = danger_app();
        assert!(app.open_form());
        assert!(app.form_needs_confirm(), "危险动作要先确认");
        assert!(app.form_danger_note().is_some());

        app.form_arm_confirm();
        assert!(!app.form_needs_confirm(), "确认过就不再拦");
        assert!(app.message.contains("再按一次 Ctrl-E"), "{}", app.message);

        app.form_adjust(1);
        assert!(app.form_needs_confirm(), "改了参数就得重新确认");
    }

    #[test]
    fn viewer_holds_output_and_scrolls_without_going_negative() {
        let captured = crate::runtime::Captured {
            label: "jq".to_string(),
            command: "/usr/bin/jq . /tmp/x.json".to_string(),
            stdout: "a\nb\nc\n".to_string(),
            stderr: String::new(),
            status: Some(0),
            success: true,
            elapsed: std::time::Duration::from_millis(30),
            cancelled: false,
        };

        let mut app = action_app();
        app.open_viewer(Viewer::from_captured(std::slice::from_ref(&captured)).expect("有输出"));
        assert_eq!(
            app.viewer.as_ref().map(|viewer| viewer.title.as_str()),
            Some("/usr/bin/jq . /tmp/x.json")
        );

        app.viewer_scroll(-5);
        assert_eq!(
            app.viewer.as_ref().map(|viewer| viewer.scroll),
            Some(0),
            "往上滚不会变负"
        );
        app.viewer_to_bottom();
        assert!(app.viewer.as_ref().is_some_and(|viewer| viewer.scroll > 0));

        app.close_viewer();
        assert!(app.viewer.is_none());
    }

    /// 两件工具、分属两个域（收藏视图要证明「跨域」就得有第二个域）。
    fn cross_domain_app() -> App {
        let registry = Registry::from_tools(vec![
            tool("媒体工具", Domain::Media, &["编辑"]),
            tool("系统工具", Domain::System, &["信息"]),
        ]);
        let mut app = App::new(registry, PathBuf::from("/tmp/bin"), ReloadReport::default());
        app.domain = Domain::Media.index();
        app.apply_filter();
        app
    }

    /// 收藏与视图：收藏后切到 ★ 收藏只剩它，取消收藏后立刻消失，而且**跨域**。
    ///
    /// 状态文件指到临时文件，免得动到真实配置。
    #[test]
    fn favorites_toggle_and_the_favorites_view_is_cross_domain() {
        let mut app = cross_domain_app();
        app.state = crate::state::State::default();
        app.state_path = std::env::temp_dir().join(format!(
            "toolbox-hub-test-state-{}-favorites.toml",
            std::process::id()
        ));

        assert_eq!(app.favorite_count(), 0);
        app.toggle_favorite_current();
        assert!(app.message.contains("已收藏"), "{}", app.message);
        assert_eq!(app.favorite_count(), 1);

        let favorite_id = app.registry.tools()[app.filtered[app.selected]].id.clone();
        assert!(app.is_favorite(&favorite_id));

        app.cycle_scope();
        assert_eq!(app.scope, Scope::Favorites);
        assert_eq!(app.filtered.len(), 1, "收藏视图里应该只剩收藏的那件");
        assert_eq!(app.registry.tools()[app.filtered[0]].id, favorite_id);

        // 再收藏一件**不在当前域**的系统工具：收藏视图不受域限制。
        let other = app
            .registry
            .tools()
            .iter()
            .position(|tool| tool.domain != app.current_domain())
            .expect("夹具里应有别的域的工具");
        app.filtered = vec![other];
        app.selected = 0;
        app.toggle_favorite_current();
        app.scope = Scope::Favorites;
        app.apply_filter();
        assert_eq!(
            app.filtered.len(),
            2,
            "两件收藏都要出现，且与当前域无关（当前域是 {:?}）",
            app.current_domain()
        );

        // 取消收藏当前这件，列表立刻少一件。
        app.toggle_favorite_current();
        assert!(app.message.contains("已取消收藏"), "{}", app.message);
        assert_eq!(app.favorite_count(), 1);
        assert_eq!(app.filtered.len(), 1, "取消收藏后就不该留在收藏视图里");

        let _ = std::fs::remove_file(&app.state_path);
    }

    /// 最近使用来自执行历史，不额外存一份。
    #[test]
    fn the_recent_view_is_derived_from_history() {
        let mut app = action_app();
        app.scope = Scope::Recent;
        app.apply_filter();

        // 历史里有记录时，顺序应与最近使用的先后一致；没有记录时视图为空。
        let recent = app.recent_tool_ids();
        assert_eq!(
            app.filtered.len(),
            app.registry
                .tools()
                .iter()
                .filter(|tool| recent.contains(&tool.id))
                .count(),
            "最近视图 = 历史里出现过的工具"
        );
    }

    /// 改工作目录：合法的记住、非法的留在输入态、空的算取消。
    ///
    /// 状态文件指到临时文件，免得动到真实配置。
    #[test]
    fn changing_the_work_dir_validates_and_remembers() {
        let mut app = action_app();
        app.state = crate::state::State::default();
        app.state_path = std::env::temp_dir().join(format!(
            "toolbox-hub-test-state-{}-workdir.toml",
            std::process::id()
        ));

        // 预填当前目录，好改
        app.open_dir_input();
        assert!(app.is_editing_dir());
        assert_eq!(app.dir_input_text(), app.work_dir.display().to_string());

        // 不存在的目录：报出来，并且留在输入态让他接着改
        app.dir_input = Some(String::from("/nonexistent/toolbox-hub-workdir"));
        app.commit_dir_input();
        assert!(app.is_editing_dir(), "输入错了要留在输入态");
        assert!(app.message.contains("不是目录"), "{}", app.message);
        assert_eq!(app.dir_input_text(), "/nonexistent/toolbox-hub-workdir");

        // 空输入 = 取消
        app.dir_input = Some(String::new());
        app.commit_dir_input();
        assert!(!app.is_editing_dir());
        assert!(app.message.contains("已取消"), "{}", app.message);

        // 合法目录：改掉 + 记住
        let dir = std::env::temp_dir();
        let expected = std::fs::canonicalize(&dir).expect("canonicalize");
        app.open_dir_input();
        app.dir_input = Some(dir.display().to_string());
        app.commit_dir_input();

        assert!(!app.is_editing_dir(), "确认后要退出输入态");
        assert_eq!(app.work_dir, expected, "要存绝对路径");
        assert!(app.message.contains("工作目录 →"), "{}", app.message);
        assert_eq!(
            crate::state::load_from(&app.state_path).work_dir,
            Some(expected),
            "要真的写进状态文件"
        );

        let _ = std::fs::remove_file(&app.state_path);
    }

    /// 文件选择器：起点怎么定、填进去是相对还是绝对、多值会不会追加。
    #[test]
    fn the_picker_fills_path_fields_from_without_typing_paths() {
        // 临时工作目录，里面放两个文件
        let base =
            std::env::temp_dir().join(format!("toolbox-hub-picker-app-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).expect("mkdir");
        std::fs::write(base.join("a.json"), "{}").expect("write");
        std::fs::write(base.join("b.json"), "{}").expect("write");
        let dir = std::fs::canonicalize(&base).expect("canonical");

        // jq 动作：「表达式」是 text，「JSON 文件」是 path 且多值
        let jq = crate::providers::manifest::bundled_tools()
            .into_iter()
            .find(|tool| tool.id == "manifest:jq-query")
            .expect("应有 jq 动作");
        let mut app = App::new(
            Registry::from_tools(vec![jq]),
            PathBuf::from("/tmp/bin"),
            ReloadReport::default(),
        );
        app.state = crate::state::State::default();
        app.state_path = dir.join("state.toml");
        app.work_dir = dir.clone();
        // jq 在「开发」域，先把域切过去，否则列表是空的
        app.switch_domain(Domain::Dev.index());
        app.apply_filter();
        assert!(app.open_form());

        // 第一个字段是文本 → 不该开选择器
        app.open_picker();
        assert!(app.picker.is_none(), "不是路径字段就不该开选择器");
        assert!(app.message.contains("不是路径字段"), "{}", app.message);

        // 第二个字段是路径 → 能开，起点是工作目录
        app.form_move(1);
        app.open_picker();
        let picker = app.picker.as_ref().expect("路径字段应该能开选择器");
        assert_eq!(picker.dir(), dir.as_path(), "空字段时起点是工作目录");

        // 选 a.json：在工作目录里 → 用相对路径（和执行时一致）
        app.picker.as_mut().expect("picker").selected =
            (0..app.picker.as_ref().expect("picker").len())
                .find(|&index| {
                    app.picker
                        .as_ref()
                        .expect("picker")
                        .entry(index)
                        .is_some_and(|entry| entry.name == "a.json")
                })
                .expect("应有 a.json");
        app.picker_choose();
        assert!(app.picker.is_none(), "选完要关掉");
        assert_eq!(
            app.form_values().and_then(|values| values.get("file")),
            Some("a.json")
        );

        // 再选一个：多值字段是**追加**
        app.open_picker();
        app.picker.as_mut().expect("picker").selected =
            (0..app.picker.as_ref().expect("picker").len())
                .find(|&index| {
                    app.picker
                        .as_ref()
                        .expect("picker")
                        .entry(index)
                        .is_some_and(|entry| entry.name == "b.json")
                })
                .expect("应有 b.json");
        app.picker_choose();
        assert_eq!(
            app.form_values().and_then(|values| values.get("file")),
            Some("a.json, b.json"),
            "多值字段第二次选应该是追加"
        );

        // Ctrl-U 清空
        app.open_picker();
        app.picker_clear_field();
        assert_eq!(
            app.form_values().and_then(|values| values.get("file")),
            Some(""),
            "清空字段"
        );

        // 工作目录之外的文件 → 绝对路径；起点跟着已有值走
        let outside = std::env::temp_dir().join(format!(
            "toolbox-hub-picker-outside-{}.json",
            std::process::id()
        ));
        std::fs::write(&outside, "{}").expect("write");
        app.form
            .as_mut()
            .expect("form")
            .values
            .set("file", outside.display().to_string());
        app.open_picker();
        assert_eq!(
            app.picker.as_ref().expect("picker").dir(),
            outside.parent().expect("parent"),
            "起点应该是已有值所在目录"
        );
        app.picker.as_mut().expect("picker").selected =
            (0..app.picker.as_ref().expect("picker").len())
                .find(|&index| {
                    app.picker
                        .as_ref()
                        .expect("picker")
                        .entry(index)
                        .is_some_and(|entry| entry.path == outside)
                })
                .expect("应有刚写的那个文件");
        app.picker_choose();
        assert_eq!(
            app.form_values().and_then(|values| values.get("file")),
            Some(outside.display().to_string().as_str()),
            "工作目录之外要用绝对路径"
        );

        let _ = std::fs::remove_file(&outside);
        let _ = std::fs::remove_dir_all(&base);
    }

    /// 临时目录 + 一个 jq 表单的 App（选择器相关的测试共用）。
    fn picker_app(tag: &str) -> (App, PathBuf) {
        let base =
            std::env::temp_dir().join(format!("toolbox-hub-picker-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("sub")).expect("mkdir");
        std::fs::write(base.join("a.json"), "{}").expect("write");
        std::fs::write(base.join("b.json"), "{}").expect("write");
        let dir = std::fs::canonicalize(&base).expect("canonical");

        let jq = crate::providers::manifest::bundled_tools()
            .into_iter()
            .find(|tool| tool.id == "manifest:jq-query")
            .expect("应有 jq 动作");
        let mut app = App::new(
            Registry::from_tools(vec![jq]),
            PathBuf::from("/tmp/bin"),
            ReloadReport::default(),
        );
        app.state = crate::state::State::default();
        app.state_path = dir.join("state.toml");
        app.work_dir = dir.clone();
        app.switch_domain(Domain::Dev.index());
        app.apply_filter();
        assert!(app.open_form());
        app.form_move(1); // 到「JSON 文件」（path、多值）
        (app, dir)
    }

    /// Tab 多选：一次把标记的文件都填进去，并记住翻的是哪个目录。
    #[test]
    fn tab_marking_fills_several_files_at_once() {
        let (mut app, dir) = picker_app("multi");
        app.open_picker();
        {
            let picker = app.picker.as_mut().expect("picker");
            // 第一项是目录（标记目录没有意义），先移到文件上
            assert!(picker.entry(0).expect("第一项").is_dir);
            picker.move_selection(1);
            picker.toggle_mark();
            picker.toggle_mark();
        }
        assert_eq!(app.picker.as_ref().expect("picker").marked_count(), 2);

        app.picker_choose();
        assert_eq!(
            app.form_values().and_then(|values| values.get("file")),
            Some("a.json, b.json"),
            "标记的两个都要填进去"
        );
        assert_eq!(
            app.state.recent_dirs.first().map(PathBuf::as_path),
            Some(dir.as_path()),
            "选完要记住这个目录"
        );

        // 下次打开直接从那个目录开始
        app.open_picker();
        assert_eq!(
            app.picker.as_ref().expect("picker").dir(),
            dir.as_path(),
            "起点应该是最近用过的目录"
        );

        // 单值字段标记多个：拒绝，而不是把逗号塞进一个参数
        {
            let picker = app.picker.as_mut().expect("picker");
            picker.repeatable = false;
            picker.move_selection(1);
            picker.toggle_mark();
            picker.toggle_mark();
        }
        app.picker_choose();
        assert!(app.picker.is_some(), "拒绝了就该留在选择器里让他改");
        assert!(app.message.contains("只能填一个"), "{}", app.message);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Ctrl-D：把当前目录本身填进字段（要填目录的字段靠这个）。
    #[test]
    fn ctrl_d_fills_the_current_directory() {
        let (mut app, dir) = picker_app("dir");
        app.open_picker();

        let sub = dir.join("sub");
        app.picker.as_mut().expect("picker").enter_dir(&sub);
        app.picker_use_current_dir();

        assert!(app.picker.is_none(), "填完要关掉");
        assert_eq!(
            app.form_values().and_then(|values| values.get("file")),
            Some("sub"),
            "工作目录里用相对路径"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 回归测试：列表模式下带 Ctrl 的组合键**不该**误触单键功能。
    ///
    /// 实拍发现的：在列表里按 Ctrl-F 会变成「收藏」，状态文件里就多出一条
    /// 没人按过的收藏。同类还有 Ctrl-D / Ctrl-V / Ctrl-H / Ctrl-J / Ctrl-3。
    #[test]
    fn ctrl_combos_in_the_list_do_not_trigger_single_key_actions() {
        use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

        let mut app = action_app();
        app.state = crate::state::State::default();
        app.state_path = std::env::temp_dir().join(format!(
            "toolbox-hub-test-state-{}-ctrl.toml",
            std::process::id()
        ));
        let cwd = app.work_dir.clone();

        for ch in ['f', 'd', 'v', 'h', 'l', 'j', 'k', 'q', '3'] {
            let key = KeyEvent::new(KeyCode::Char(ch), KeyModifiers::CONTROL);
            let quit = crate::app::handle_key(&mut app, key, &cwd).expect("不该出错");
            assert!(!quit, "Ctrl-{ch} 不该退出");
        }

        assert_eq!(app.favorite_count(), 0, "Ctrl-F 不该收藏");
        assert!(!app.is_editing_dir(), "Ctrl-D 不该打开目录输入");
        assert_eq!(app.scope, Scope::All, "Ctrl-V 不该切视图");
        assert!(app.history.is_none(), "Ctrl-H 不该打开历史");
        assert_eq!(app.domain, Domain::Media.index(), "Ctrl-3 不该切域");
        assert_eq!(app.selected, 0, "Ctrl-J 不该移动选择");

        let _ = std::fs::remove_file(&app.state_path);
    }

    /// 后台执行：起任务 → 每帧轮询 → 跑完自动进输出视图、写历史。
    #[test]
    fn a_capture_job_runs_in_the_background_and_opens_the_viewer() {
        let mut app = action_app();
        app.state = crate::state::State::default();
        let base = std::env::temp_dir().join(format!("toolbox-hub-async-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).expect("mkdir");
        app.state_path = base.join("state.toml");
        app.history_path = Some(base.join("history.log"));

        // 一件「假工具」：程序换成 printf，跑起来就吐一行（不用 shell）。
        let mut tool = tool("后台打印", Domain::Media, &["编辑"]);
        tool.path = PathBuf::from("/usr/bin/printf");
        app.enqueue_captures(vec![CaptureRequest {
            tool,
            ok_exit_codes: vec![0],
            values: Vec::new(),
            argv: vec![String::from("后台输出\\n")],
            record_argv: vec![String::from("后台输出\\n")],
            total_seconds: None,
        }]);
        assert!(app.is_running(), "排进去就该在跑");

        // 轮询到结束（给它 10 秒，正常几十毫秒）
        let cwd = app.work_dir.clone();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while app.is_running() && std::time::Instant::now() < deadline {
            app.poll_job(&cwd);
            std::thread::sleep(std::time::Duration::from_millis(10));
        }

        assert!(!app.is_running(), "应该跑完了");
        let viewer = app.viewer.as_ref().expect("跑完要自动进输出视图");
        assert!(viewer.body.contains("后台输出"), "{}", viewer.body);
        assert!(app.message.contains("执行完成"), "{}", app.message);
        let entries = crate::history::load_from(&base.join("history.log"), 10);
        let entry = entries
            .iter()
            .find(|entry| entry.tool_name == "后台打印")
            .expect("后台跑完也要写历史");
        assert_eq!(
            entry.argv.first().map(String::as_str),
            Some("/usr/bin/printf"),
            "历史里的命令要带程序名，否则重跑不了: {:?}",
            entry.argv
        );

        let _ = std::fs::remove_dir_all(&base);
    }

    /// 进度条的总时长：先看 limit_from，再退到 duration_from 探文件。
    #[test]
    fn total_seconds_comes_from_the_declared_argument() {
        use crate::model::{Action, ArgumentValues};

        let app = action_app();

        // 1) limit_from：裁剪类动作这次只做 10 秒
        let action = Action {
            program: "ffmpeg".to_string(),
            base_argv: Vec::new(),
            arguments: Vec::new(),
            duration_from: Some("input".to_string()),
            limit_from: Some("duration".to_string()),
            foreach: None,
            allow_empty: false,
            ok_exit_codes: vec![0],
        };
        let mut values = ArgumentValues::new();
        values.set("duration", "00:00:10");
        assert_eq!(app.total_seconds_for(&action, &values), Some(10.0));

        // 2) 没有 limit_from 时探 duration_from 指向的文件；文件不存在就是 None
        let probe_action = Action {
            duration_from: Some("input".to_string()),
            limit_from: None,
            ..action.clone()
        };
        let mut values = ArgumentValues::new();
        values.set("input", "/nonexistent/definitely-not-here.mp4");
        assert_eq!(app.total_seconds_for(&probe_action, &values), None);

        // 3) 两个都没声明 → 没有百分比（不猜）
        let plain = Action {
            duration_from: None,
            limit_from: None,
            ..action.clone()
        };
        assert_eq!(app.total_seconds_for(&plain, &values), None);

        // 4) 参数是空串也不该去探
        let mut empty = ArgumentValues::new();
        empty.set("input", "   ");
        assert_eq!(app.total_seconds_for(&probe_action, &empty), None);
    }

    /// 起后台任务时**必须已经把总时长算好**。
    ///
    /// 踩过的坑：总时长在 `close_form()` 之后才算，表单已经关了、取值拿不到，
    /// 于是进度条永远不出现（只在真机上看得出来）。
    #[test]
    fn starting_a_job_carries_the_total_seconds() {
        use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

        use crate::model::{Action, ArgumentValues};

        // 一件带 limit_from 的假工具：不用 ffprobe 也能有总时长。
        let action = Action {
            program: "sleep".to_string(),
            base_argv: Vec::new(),
            arguments: vec![crate::model::Argument {
                key: "duration".to_string(),
                label: "时长".to_string(),
                kind: crate::model::ArgKind::Text,
                default: Some("00:00:20".to_string()),
                choices: Vec::new(),
                required: false,
                flag: Some("-t".to_string()),
                flag_join: false,
                placement: crate::model::ArgPlacement::Trailing,
                sensitive: false,
                repeatable: false,
                repeat_flag: false,
                separator: String::from(","),
                dir_only: false,
                help: Some("跑多久".to_string()),
            }],
            duration_from: None,
            limit_from: Some("duration".to_string()),
            foreach: None,
            allow_empty: false,
            ok_exit_codes: vec![0],
        };
        let mut tool = tool("假任务", Domain::Media, &["编辑"]);
        tool.action = Some(action);
        tool.path = PathBuf::from("/usr/bin/sleep");
        // 捕获模式才会走后台队列（交互式会去接管终端，测试里没有 TTY）。
        tool.mode = RunMode::Capture;

        let mut app = App::new(
            Registry::from_tools(vec![tool]),
            PathBuf::from("/tmp/bin"),
            ReloadReport::default(),
        );
        app.state = crate::state::State::default();
        app.state_path = std::env::temp_dir().join(format!(
            "toolbox-hub-test-state-{}-total.toml",
            std::process::id()
        ));
        app.history_path = Some(app.state_path.with_extension("history"));
        app.apply_filter();
        assert!(app.open_form());

        // 表单里 duration 的默认值是 00:00:20
        let cwd = app.work_dir.clone();
        let key = KeyEvent::new(KeyCode::Char('e'), KeyModifiers::CONTROL);
        let _ = crate::app::handle_key(&mut app, key, &cwd).expect("不该出错");

        let running = app.running.as_ref().expect("应该起了后台任务");
        assert_eq!(
            running.total_seconds,
            Some(20.0),
            "起任务时就该带上总时长（否则进度条永远不出现）"
        );
        assert_eq!(running.progress_ratio(), None, "刚开始还没有 out_time");

        // 收尾：杀掉那个 sleep，别留着
        app.cancel_job();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while app.is_running() && std::time::Instant::now() < deadline {
            app.poll_job(&cwd);
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let _ = std::fs::remove_file(&app.state_path);
        let _ = ArgumentValues::new();
    }

    #[test]
    fn typing_q_while_searching_does_not_cancel_a_background_job() {
        use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

        let mut app = action_app();
        let job = crate::runtime::spawn_captured(
            std::path::Path::new("/usr/bin/sleep"),
            &[String::from("5")],
            std::path::Path::new("/tmp"),
            "后台任务",
        )
        .expect("启动后台任务");
        app.running = Some(crate::app::RunningJobView::for_test(
            job,
            Vec::new(),
            Vec::new(),
        ));
        app.searching = true;

        let cwd = app.work_dir.clone();
        let key = KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE);
        let quit = crate::app::handle_key(&mut app, key, &cwd).expect("不该出错");

        assert!(!quit, "q 在搜索输入态不是退出命令");
        assert!(app.is_running(), "搜索输入不应取消后台任务");
        app.cancel_job();
    }

    /// `?` 打开帮助，`q` / `Esc` 关掉；开着时别的键不该漏到列表上。
    #[test]
    fn the_help_overlay_opens_and_closes() {
        use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

        let mut app = action_app();
        let cwd = app.work_dir.clone();
        let press = |app: &mut App, code: KeyCode, modifiers: KeyModifiers| {
            let key = KeyEvent::new(code, modifiers);
            let _ = crate::app::handle_key(app, key, &cwd).expect("不该出错");
        };

        assert!(!app.is_help_open());
        press(&mut app, KeyCode::Char('?'), KeyModifiers::SHIFT);
        assert!(app.is_help_open(), "? 应该打开帮助");

        // 开着时能滚动，而且往上滚不会变负
        press(&mut app, KeyCode::Down, KeyModifiers::NONE);
        assert_eq!(app.help_scroll(), 1);
        press(&mut app, KeyCode::Up, KeyModifiers::NONE);
        press(&mut app, KeyCode::Up, KeyModifiers::NONE);
        assert_eq!(app.help_scroll(), 0);

        press(&mut app, KeyCode::Char('q'), KeyModifiers::NONE);
        assert!(!app.is_help_open(), "q 应该关掉帮助");
    }

    /// 历史「回填再改」：把那次参数填回表单（而不是原样重跑）。
    #[test]
    fn history_refills_the_form_with_the_old_values() {
        use std::collections::BTreeMap;

        let jq = crate::providers::manifest::bundled_tools()
            .into_iter()
            .find(|tool| tool.id == "manifest:jq-query")
            .expect("应有 jq 动作");
        let mut app = App::new(
            Registry::from_tools(vec![jq]),
            PathBuf::from("/tmp/bin"),
            ReloadReport::default(),
        );
        app.switch_domain(Domain::Dev.index());
        app.apply_filter();

        app.history = Some(super::HistoryView {
            entries: vec![crate::history::Entry {
                epoch: crate::history::now_epoch(),
                tool_id: String::from("manifest:jq-query"),
                tool_name: String::from("jq 查询 JSON"),
                argv: vec![String::from("/usr/bin/jq"), String::from(".items[]")],
                success: true,
                millis: 12,
                values: BTreeMap::from([
                    (String::from("filter"), String::from(".items[]")),
                    (String::from("file"), String::from("a.json")),
                    (String::from("compact"), String::from("true")),
                ]),
            }],
            selected: 0,
        });

        app.refill_from_history();

        assert!(app.history.is_none(), "回填完要把历史关掉");
        assert!(app.form.is_some(), "应该打开了参数表单");
        let values = app.form_values().expect("表单取值");
        assert_eq!(values.get("filter"), Some(".items[]"), "填写入过的表达式");
        assert_eq!(values.get("file"), Some("a.json"), "填写入过的文件");
        assert_eq!(values.get("compact"), Some("true"), "开关也要按当时的样子");
        assert!(app.message.contains("填回表单"), "{}", app.message);

        // 工具已经不在了（改了 id）→ 明确说一句，别静悄悄什么都不做
        app.close_form();
        app.history = Some(super::HistoryView {
            entries: vec![crate::history::Entry {
                tool_id: String::from("manifest:早就删了"),
                tool_name: String::from("旧工具"),
                epoch: 0,
                argv: Vec::new(),
                success: true,
                millis: 0,
                values: BTreeMap::new(),
            }],
            selected: 0,
        });
        app.refill_from_history();
        assert!(app.message.contains("不在列表里"), "{}", app.message);
    }

    /// 造一件带 `foreach` 的假工具：`touch <输出>`，输入是批量字段。
    fn foreach_app() -> App {
        use crate::model::{Action, ArgKind, ArgPlacement, Argument};

        let argument =
            |key: &str, label: &str, required: bool, repeatable: bool, default: Option<&str>| {
                Argument {
                    key: key.to_string(),
                    label: label.to_string(),
                    kind: ArgKind::Path,
                    default: default.map(str::to_string),
                    choices: Vec::new(),
                    required,
                    flag: None,
                    flag_join: false,
                    placement: ArgPlacement::Trailing,
                    sensitive: false,
                    repeatable,
                    repeat_flag: false,
                    separator: String::from(","),
                    dir_only: false,
                    help: Some(String::from("测试用")),
                }
            };

        let mut tool = tool("批量假工具", Domain::Media, &["编辑"]);
        tool.action = Some(Action {
            program: String::from("/usr/bin/touch"),
            base_argv: Vec::new(),
            arguments: vec![
                argument("input", "输入", true, true, None),
                argument("output", "输出", true, false, Some("{stem}_out.mp4")),
            ],
            duration_from: None,
            limit_from: None,
            foreach: Some(String::from("input")),
            allow_empty: false,
            ok_exit_codes: vec![0],
        });
        tool.path = PathBuf::from("/usr/bin/touch");
        tool.mode = RunMode::Capture;

        let mut app = App::new(
            Registry::from_tools(vec![tool]),
            PathBuf::from("/tmp/bin"),
            ReloadReport::default(),
        );
        app.state = crate::state::State::default();
        app.state_path = std::env::temp_dir().join(format!(
            "toolbox-hub-test-state-{}-foreach.toml",
            std::process::id()
        ));
        app.history_path = Some(app.state_path.with_extension("history"));
        app.apply_filter();
        assert!(app.open_form());
        app
    }

    /// `foreach`：每个输入各跑一次，输出按 `{stem}` 之类的模板替换。
    #[test]
    fn foreach_expands_into_one_run_per_input() {
        let mut app = foreach_app();

        {
            let form = app.form.as_mut().expect("表单");
            form.values.set("input", "a.mp4, 素材/我的 片段.mov");
        }

        let runs = app.form_build_runs().expect("应能展开");
        assert_eq!(runs.len(), 2, "两个输入 → 两次执行");
        // 两个字段都是位置参数，所以 argv 里输入在前、输出在后（和 touch 的用法一致）
        assert_eq!(runs[0].0, vec!["a.mp4", "a_out.mp4"], "第一次的 argv");
        assert_eq!(
            runs[1].0,
            vec!["素材/我的 片段.mov", "我的 片段_out.mp4"],
            "第二次的 argv（名字里的空格要保住）"
        );
        assert_eq!(
            runs[0].1.get("input"),
            Some("a.mp4"),
            "每次只带自己那个输入"
        );
        assert_eq!(runs[1].1.get("input"), Some("素材/我的 片段.mov"));

        // 空输入 → 明确报错，不猜
        app.form.as_mut().expect("表单").values.set("input", "");
        let error = app.form_build_runs().expect_err("空的批量字段要报错");
        assert!(error.contains("至少要填一条"), "{error}");
    }

    /// 其余占位符：`{name}` / `{ext}` / `{dir}`。
    #[test]
    fn foreach_placeholders_fill_the_other_fields() {
        let mut app = foreach_app();
        {
            let form = app.form.as_mut().expect("表单");
            form.values.set("input", "/素材/片子.mp4");
            form.values.set("output", "{dir}/out/{name}.{ext}");
        }

        let runs = app.form_build_runs().expect("应能展开");
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].0, vec!["/素材/片子.mp4", "/素材/out/片子.mp4.mp4"]);
        assert_eq!(runs[0].1.get("output"), Some("/素材/out/片子.mp4.mp4"));
    }

    /// `foreach` 真的会跑 N 次、产出 N 个文件（走后台队列，用 touch 当假工具）。
    #[test]
    fn foreach_runs_every_input_through_the_queue() {
        let base = std::env::temp_dir().join(format!("toolbox-hub-foreach-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).expect("mkdir");
        let dir = std::fs::canonicalize(&base).expect("canonical");

        let mut app = foreach_app();
        app.work_dir = dir.clone();
        {
            let form = app.form.as_mut().expect("表单");
            form.values.set("input", "one.mp4, two.mp4, three.mp4");
        }

        let runs = app.form_build_runs().expect("展开");
        let requests: Vec<CaptureRequest> = runs
            .into_iter()
            .map(|(argv, values)| CaptureRequest {
                tool: app.registry.tools()[0].clone(),
                ok_exit_codes: vec![0],
                values: values.pairs(),
                argv,
                record_argv: Vec::new(),
                total_seconds: None,
            })
            .collect();
        app.enqueue_captures(requests);

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while (app.is_running() || !app.job_queue.is_empty())
            && std::time::Instant::now() < deadline
        {
            app.poll_job(&dir);
            std::thread::sleep(std::time::Duration::from_millis(10));
        }

        for name in ["one_out.mp4", "two_out.mp4", "three_out.mp4"] {
            assert!(dir.join(name).exists(), "{name} 应该被创建出来");
        }
        // 三次执行都要进历史（各自一条）
        let entries = crate::history::load_from(&app.history_path.clone().expect("路径"), 10);
        assert_eq!(entries.len(), 3, "三次执行各记一条: {entries:?}");

        let _ = std::fs::remove_dir_all(&base);
    }

    /// 退出码白名单走完真队列：`/usr/bin/false` 退 1，但动作声明了 1 算成功。
    ///
    /// 这条盯着的是「`pacman -Qdt` 没孤儿包时退 1，界面却报失败」那个坑
    /// （真跑才发现的）。
    #[test]
    fn a_whitelisted_exit_code_counts_as_success() {
        let argument = |key: &str, label: &str, flag: &str| crate::model::Argument {
            key: key.to_string(),
            label: label.to_string(),
            kind: crate::model::ArgKind::Toggle,
            default: Some(String::from("true")),
            choices: Vec::new(),
            required: false,
            flag: Some(flag.to_string()),
            flag_join: false,
            repeat_flag: false,
            placement: crate::model::ArgPlacement::Trailing,
            sensitive: false,
            repeatable: false,
            separator: String::from(","),
            dir_only: false,
            help: Some(String::from("测试用")),
        };

        let mut tool = tool("假查询", Domain::Packages, &["信息"]);
        tool.path = PathBuf::from("/usr/bin/false");
        tool.mode = RunMode::Capture;
        tool.action = Some(crate::model::Action {
            program: String::from("/usr/bin/false"),
            base_argv: Vec::new(),
            arguments: vec![argument("always", "总是退出 1", "--x")],
            duration_from: None,
            limit_from: None,
            foreach: None,
            ok_exit_codes: vec![0, 1], // ← 关键：1 也算成功
            allow_empty: false,
        });

        let mut app = App::new(
            Registry::from_tools(vec![tool.clone()]),
            PathBuf::from("/tmp/bin"),
            ReloadReport::default(),
        );
        app.state = crate::state::State::default();
        app.state_path =
            std::env::temp_dir().join(format!("toolbox-hub-okcode-{}.toml", std::process::id()));
        app.history_path = Some(app.state_path.with_extension("history"));
        let cwd = std::env::temp_dir();

        app.enqueue_captures(vec![CaptureRequest {
            tool,
            ok_exit_codes: vec![0, 1],
            values: Vec::new(),
            argv: Vec::new(),
            record_argv: Vec::new(),
            total_seconds: None,
        }]);

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while (app.is_running() || !app.job_queue.is_empty())
            && std::time::Instant::now() < deadline
        {
            app.poll_job(&cwd);
            std::thread::sleep(std::time::Duration::from_millis(10));
        }

        let entries = crate::history::load_from(&app.history_path.clone().expect("路径"), 5);
        assert_eq!(entries.len(), 1, "应该记了一条");
        assert!(entries[0].success, "退 1 在白名单里，历史应记成功");
        assert!(
            app.message.contains("失败 0") || app.message.contains("完成"),
            "{}",
            app.message
        );
    }

    /// 文件管理器的反馈怎么决定工作目录：选中过文件用它的目录，否则用退出目录。
    #[test]
    fn the_file_manager_decides_the_work_dir() {
        use crate::runtime::BrowsedBack;

        let base = std::env::temp_dir().join(format!("toolbox-hub-yazi-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("子目录")).expect("mkdir");
        std::fs::write(base.join("子目录/a.mp4"), "x").expect("write");
        std::fs::write(base.join("b.mp4"), "y").expect("write");
        let dir = std::fs::canonicalize(&base).expect("canonical");

        let mut app = action_app();
        app.state = crate::state::State::default();
        app.state_path = dir.join("state.toml");
        app.history_path = Some(dir.join("history.log"));

        // 1) 在里面「打开」过文件 → 用它的目录（脚本立刻能看到它）
        app.apply_browsed(BrowsedBack {
            status: Some(0),
            cwd: Some(dir.clone()),
            chosen: vec![dir.join("子目录/a.mp4")],
        });
        assert_eq!(app.work_dir, dir.join("子目录"), "应该跟着选中的文件走");
        assert!(app.message.contains("选中"), "{}", app.message);
        assert!(
            app.media_label().contains("1 个媒体文件"),
            "{}",
            app.media_label()
        );

        // 2) 只报告了目录 → 用它
        app.apply_browsed(BrowsedBack {
            status: Some(0),
            cwd: Some(dir.clone()),
            chosen: Vec::new(),
        });
        assert_eq!(app.work_dir, dir, "应该跟着 yazi 里最后待的目录走");
        assert!(app.message.contains("文件管理器返回"), "{}", app.message);
        assert!(
            app.media_label().contains("2 个媒体文件"),
            "{}",
            app.media_label()
        );

        // 3) 什么都没报（比如直接 Ctrl-C 掉了）→ 工作目录不变，也别乱说话
        let before = app.work_dir.clone();
        app.apply_browsed(BrowsedBack {
            status: Some(1),
            cwd: None,
            chosen: Vec::new(),
        });
        assert_eq!(app.work_dir, before);
        assert!(app.message.contains("没报告目录"), "{}", app.message);

        let _ = std::fs::remove_dir_all(&base);
    }

    /// 文件视图：看得到目录里有什么，Enter 能切到该文件所在目录。
    #[test]
    fn the_files_view_shows_media_and_can_switch_the_work_dir() {
        // 临时目录：根目录一个 mp4、子目录一个 mov、一个非媒体文件
        let base = std::env::temp_dir().join(format!("toolbox-hub-files-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("子目录")).expect("mkdir");
        std::fs::write(base.join("a.mp4"), "x").expect("write");
        std::fs::write(base.join("子目录/b.mov"), "y").expect("write");
        std::fs::write(base.join("notes.txt"), "z").expect("write");
        let dir = std::fs::canonicalize(&base).expect("canonical");

        let mut app = action_app();
        app.state = crate::state::State::default();
        app.state_path = dir.join("state.toml");
        app.history_path = Some(dir.join("history.log"));

        // 工作目录 = 项目目录时，应该明确说「没有媒体文件」
        app.work_dir = PathBuf::from("/nonexistent/definitely-empty");
        app.refresh_media();
        assert!(app.media.files.is_empty());
        assert!(
            app.media_label().contains("没有媒体文件"),
            "{}",
            app.media_label()
        );

        // 换到有素材的目录：头部立刻能说出来
        app.set_work_dir(dir.clone(), "测试");
        assert_eq!(app.media.files.len(), 2, "两个媒体文件（含子目录）");
        assert!(
            app.media_label().contains("2 个媒体文件"),
            "{}",
            app.media_label()
        );

        // 打开文件视图
        app.open_files();
        let view = app.files.as_ref().expect("文件视图应该开着");
        assert_eq!(app.files.as_ref().expect("view").len(), 2);
        assert!(view.total() == 2);
        assert!(
            view.entry(0).is_some_and(|file| file.name == "a.mp4"),
            "按相对路径排序"
        );

        // 过滤（匹配的是相对路径，所以扩展名也算）
        for ch in "mov".chars() {
            app.files_push(ch);
        }
        let filtered = app.files.as_ref().expect("view");
        assert_eq!(filtered.len(), 1, "只剩子目录里的 .mov");
        assert!(filtered.entry(0).is_some_and(|file| file.name == "b.mov"));
        app.files_push('z');
        assert_eq!(app.files.as_ref().expect("view").len(), 0, "没有匹配");
        app.files_backspace();
        assert_eq!(app.files.as_ref().expect("view").len(), 1);

        // Enter：切到该文件所在目录（对所有脚本都管用的一招）
        app.files_enter();
        assert!(app.files.is_none(), "选完要关掉");
        assert_eq!(
            app.work_dir,
            dir.join("子目录"),
            "工作目录应该切到该文件所在目录"
        );
        assert_eq!(app.media.files.len(), 1, "新目录里只有一个文件");
        assert!(
            app.state
                .recent_dirs
                .iter()
                .any(|item| item == &app.work_dir),
            "顺手记进最近目录"
        );
        assert!(app.message.contains("已切到"), "{}", app.message);

        let _ = std::fs::remove_dir_all(&base);
    }

    /// 空的（或没有媒体的）目录：文件视图要明说，而不是一片空白。
    #[test]
    fn an_empty_files_view_says_so() {
        let base = std::env::temp_dir().join(format!("toolbox-hub-nofiles-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).expect("mkdir");
        std::fs::write(base.join("readme.txt"), "x").expect("write");

        let mut app = action_app();
        app.work_dir = std::fs::canonicalize(&base).expect("canonical");
        app.refresh_media();
        app.open_files();

        let view = app.files.as_ref().expect("开着");
        assert!(view.is_empty());
        assert_eq!(view.total(), 0, "非媒体文件不算数");
        assert!(
            app.media_label().contains("没有媒体文件"),
            "{}",
            app.media_label()
        );

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn form_fields_wrap_around_and_leave_edit_mode() {
        let mut app = action_app();
        app.move_selection(1);
        app.open_form();

        app.form_activate();
        assert!(app.form_is_editing());
        app.form_move(-1); // 从第 0 个往上绕到最后一个，同时退出输入态
        assert!(!app.form_is_editing());
        assert_eq!(
            app.form_current_argument().map(|a| a.key.as_str()),
            Some("output")
        );
    }

    #[test]
    fn starts_on_the_media_domain_with_sub_tabs_for_its_tags() {
        let app = app();
        assert_eq!(app.current_domain(), Domain::Media);
        assert_eq!(app.filtered.len(), 3);
        assert_eq!(
            app.sub_tags.first().map(String::as_str),
            Some(super::SUB_ALL)
        );
        assert!(app.sub_tags.contains(&"转码".to_string()));
        assert!(app.has_sub_tabs());
        assert_eq!(app.sub_filter(), None);
    }

    #[test]
    fn empty_domain_has_no_sub_tabs_and_no_view() {
        let mut app = app();
        app.switch_domain(Domain::Image.index());
        assert_eq!(app.current_domain(), Domain::Image);
        assert!(app.filtered.is_empty());
        // 只有「全部」一项，等于没有筛选条。
        assert_eq!(app.sub_tags.len(), 1);
        assert!(!app.has_sub_tabs());
        assert!(app.current().is_none());
        assert_eq!(app.table.selected(), None);
    }

    #[test]
    fn switching_domain_resets_sub_and_selection() {
        let mut app = app();
        app.move_sub(1);
        assert!(app.sub_filter().is_some(), "应选中一个具体分类");
        assert_eq!(app.filtered.len(), 1);

        app.switch_domain(Domain::Tools.index());
        assert_eq!(app.sub, 0);
        assert_eq!(app.selected, 0);
        assert_eq!(app.filtered.len(), 1);
        assert!(!app.has_sub_tabs(), "无标签的域不该出现筛选条");

        // 从第一个域往左绕回最后一个域（最后一个域会变，所以不写死名字）。
        app.switch_domain(Domain::Media.index());
        app.switch_domain(app.domain + Domain::ALL.len() - 1);
        assert_eq!(
            app.current_domain(),
            Domain::ALL[Domain::ALL.len() - 1],
            "往左绕回应落到最后一个域"
        );
    }

    #[test]
    fn move_sub_cycles_and_updates_the_view() {
        let mut app = app();
        let total = app.sub_tags.len();
        assert_eq!(total, 4, "全部 + 三个真实分类: {:?}", app.sub_tags);

        // 分类顺序来自工具出现顺序（from_tools 没有 Provider 声明顺序）。
        let convert = app
            .sub_tags
            .iter()
            .position(|tag| tag == "转码")
            .expect("存在转码分类");
        for _ in 0..convert {
            app.move_sub(1);
        }
        assert_eq!(app.sub_filter(), Some("转码"));
        assert_eq!(app.filtered.len(), 1);
        assert_eq!(
            app.current().map(|t| t.name.as_str()),
            Some("convert-media")
        );

        // 继续绕回「全部」。
        for _ in convert..total {
            app.move_sub(1);
        }
        assert_eq!(app.sub, 0);
        assert_eq!(app.filtered.len(), 3);
        assert_eq!(app.sub_filter(), None);
    }

    #[test]
    fn marks_survive_filter_changes_and_feed_execution_targets() {
        let mut app = app();
        app.toggle_mark(); // 标记 trim-video
        assert_eq!(app.marked_count(), 1);

        // 切到别的域，标记仍然保留（marked 与 tools 对齐，而不是与视图对齐）。
        app.switch_domain(Domain::Tools.index());
        assert_eq!(app.marked_count(), 1);
        assert_eq!(app.execution_targets(), vec![0]);

        app.clear_marks(&[0]);
        assert_eq!(app.marked_count(), 0);
        // 没有标记时退化为当前高亮项。
        assert_eq!(app.execution_targets(), vec![app.filtered[0]]);
    }

    #[test]
    fn search_reaches_across_domains_while_browsing_stays_in_domain() {
        let mut app = app();
        // 当前停在媒体域，但搜到的是工具域的 shorin —— 这正是跨域搜索要解决的。
        app.query.set("shorin");
        app.apply_filter();
        assert_eq!(app.filtered.len(), 1);
        assert_eq!(app.current().map(|tool| tool.name.as_str()), Some("shorin"));
        assert!(app.is_global_search());

        app.query.set("trim");
        app.apply_filter();
        assert_eq!(app.filtered.len(), 1);

        app.query.clear();
        app.apply_filter();
        assert_eq!(app.filtered.len(), 3, "清空搜索后回到域内浏览");
        assert!(!app.is_global_search());
    }

    #[test]
    fn global_search_reports_hits_per_domain() {
        let mut app = app();
        app.query.set("o");
        app.apply_filter();

        assert_eq!(app.filtered.len(), 3);
        assert_eq!(
            app.search_hits_by_domain(),
            vec![(Domain::Media, 2), (Domain::Tools, 1)],
            "只列出有命中的域，顺序按域顺序"
        );
    }

    #[test]
    fn selection_wraps_around_and_ignores_empty_views() {
        let mut app = app();
        app.move_selection(-1);
        assert_eq!(app.selected, 2);
        app.move_selection(1);
        assert_eq!(app.selected, 0);

        app.switch_domain(Domain::Network.index());
        app.move_selection(1); // 空视图下不该 panic
        assert_eq!(app.selected, 0);
    }

    #[test]
    fn package_center_numbers_switch_modes_and_alt_numbers_filter() {
        use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

        let mut app = action_app();
        let mut packages = crate::app::package_view::PackageView::new(
            Vec::new(),
            None,
            crate::config::PackagePrefs::default(),
        );
        packages.repos = vec![crate::app::package_view::RepoChip {
            name: String::from("extra"),
            enabled: true,
            count: 1,
        }];
        app.packages = Some(packages);
        let cwd = app.work_dir.clone();

        for (digit, expected) in [
            ('1', crate::app::package_view::PackageMode::Search),
            ('2', crate::app::package_view::PackageMode::Installed),
            ('3', crate::app::package_view::PackageMode::News),
            ('4', crate::app::package_view::PackageMode::Health),
        ] {
            crate::app::handle_key(
                &mut app,
                KeyEvent::new(KeyCode::Char(digit), KeyModifiers::NONE),
                &cwd,
            )
            .expect("handle key");
            assert_eq!(app.packages.as_ref().map(|view| view.mode), Some(expected));
        }

        crate::app::handle_key(
            &mut app,
            KeyEvent::new(KeyCode::Char('1'), KeyModifiers::NONE),
            &cwd,
        )
        .expect("search mode");
        crate::app::handle_key(
            &mut app,
            KeyEvent::new(KeyCode::Char('1'), KeyModifiers::ALT),
            &cwd,
        )
        .expect("toggle repo");
        assert!(!app.packages.as_ref().expect("package view").repos[0].enabled);

        crate::app::handle_key(
            &mut app,
            KeyEvent::new(KeyCode::Char('7'), KeyModifiers::NONE),
            &cwd,
        )
        .expect("type search digit");
        assert_eq!(
            app.packages.as_ref().map(|view| view.query.text()),
            Some("7"),
            "普通数字仍可输入搜索词"
        );
    }

    #[test]
    fn list_navigation_keys_jump_and_page_through_results() {
        use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

        let mut app = app();
        let cwd = app.work_dir.clone();
        let press = |app: &mut App, code: KeyCode, modifiers: KeyModifiers| {
            crate::app::handle_key(app, KeyEvent::new(code, modifiers), &cwd).expect("handle key");
        };

        press(&mut app, KeyCode::End, KeyModifiers::NONE);
        assert_eq!(app.selected, 2);
        press(&mut app, KeyCode::Char('g'), KeyModifiers::NONE);
        assert_eq!(app.selected, 0);
        press(&mut app, KeyCode::Char('G'), KeyModifiers::SHIFT);
        assert_eq!(app.selected, 2);
        press(&mut app, KeyCode::Home, KeyModifiers::NONE);
        assert_eq!(app.selected, 0);
        press(&mut app, KeyCode::Char('d'), KeyModifiers::CONTROL);
        assert_eq!(app.selected, 2);
    }

    #[test]
    fn f1_opens_and_closes_help_while_package_center_is_active() {
        use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

        let mut app = action_app();
        app.packages = Some(crate::app::package_view::PackageView::new(
            Vec::new(),
            None,
            crate::config::PackagePrefs::default(),
        ));
        let cwd = app.work_dir.clone();

        crate::app::handle_key(
            &mut app,
            KeyEvent::new(KeyCode::F(1), KeyModifiers::NONE),
            &cwd,
        )
        .expect("open help");
        assert!(app.is_help_open());

        crate::app::handle_key(
            &mut app,
            KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
            &cwd,
        )
        .expect("close help");
        assert!(!app.is_help_open());
        assert!(app.packages.is_some(), "关闭帮助不应退出包中心");
    }
}
