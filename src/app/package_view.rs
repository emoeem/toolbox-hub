//! 原生包管理视图的状态机。
//!
//! 布局照 pacsea：结果表 + 搜索框 + 包信息面板 + 安装队列；数据由
//! [`crate::packages::probe`] 在后台线程里取，每帧 [`PackageView::poll`] 收结果 ——
//! 和后台任务的模式一致（线程 + mpsc + 每帧 poll），界面不卡。

use std::{
    path::{Path, PathBuf},
    sync::mpsc::{self, Receiver},
    thread,
};

use crate::packages::{self, NewsItem, PackageHit, QueuedPackage, probe};

/// 一次包信息面板能显示的字段上限（再多也放不下）。
const INFO_FIELDS_MAX: usize = 24;

/// 新闻线程回来的东西：条目 + 未读条数，或者一句错误。
type NewsResult = Result<(Vec<NewsItem>, Option<usize>), String>;

pub struct PackageView {
    /// 搜索框里的词。
    pub query: String,
    /// 搜索框是不是在输入状态（字母会进 query）。
    pub editing: bool,
    /// 两路搜索合并后的全部命中。
    pub hits: Vec<PackageHit>,
    /// 仓库筛选后可见的下标。
    pub visible: Vec<usize>,
    /// 仓库标签（名字 + 是否启用），按结果里出现的顺序。
    pub repos: Vec<(String, bool)>,
    pub selected: usize,
    /// 信息面板：包名 + 字段。
    pub info: Option<(String, Vec<(String, String)>)>,
    pub info_error: Option<String>,
    /// 正在取的包名（避免每帧重复发起；UI 拿它显示「取包信息中…」）。
    pub info_pending: Option<String>,
    /// 已经搜过的词（状态行显示用）。
    pub searched: Option<String>,
    pub searching: bool,
    pub errors: Vec<String>,
    /// 安装队列。
    pub queue: Vec<QueuedPackage>,
    /// 焦点在队列上还是在结果上。
    pub focus_queue: bool,
    pub queue_selected: usize,
    /// 搜索历史（最近的在前）。
    pub history: Vec<String>,
    /// Arch 新闻与未读条数。
    pub news: Vec<NewsItem>,
    pub news_unread: Option<usize>,
    /// 安装确认：第一次 Enter 只是问一声，第二次才真装（危险动作的老规矩）。
    pub install_armed: bool,
    /// 在搜索框里翻历史的位置。
    history_index: Option<usize>,
    /// 底部一句话（导出成功、看 PKGBUILD 之类）。
    pub message: String,
    search_rx: Option<Receiver<probe::SearchOutcome>>,
    info_rx: Option<Receiver<probe::InfoOutcome>>,
    news_rx: Option<Receiver<NewsResult>>,
}

impl PackageView {
    pub fn new(history: Vec<String>) -> Self {
        Self {
            query: String::new(),
            editing: true,
            hits: Vec::new(),
            visible: Vec::new(),
            repos: Vec::new(),
            selected: 0,
            info: None,
            info_error: None,
            info_pending: None,
            searched: None,
            searching: false,
            errors: Vec::new(),
            queue: Vec::new(),
            focus_queue: false,
            queue_selected: 0,
            history,
            news: Vec::new(),
            news_unread: None,
            install_armed: false,
            history_index: None,
            message: String::new(),
            search_rx: None,
            info_rx: None,
            news_rx: None,
        }
    }

    // ── 搜索 ─────────────────────────────────────────────────────────────

    /// 发起一次搜索（官方源 + AUR 都在后台线程里取）。
    pub fn start_search(&mut self) {
        let term = self.query.trim().to_string();
        if term.is_empty() {
            self.message = String::from("先填个搜索词");
            return;
        }
        if self.searching {
            return;
        }

        // 记进历史（最新的在最前）
        packages::remember_search(&mut self.history, &term);
        let _ = packages::save_searches_to(&packages::searches_path(), &self.history);

        self.searching = true;
        self.searched = Some(term.clone());
        self.editing = false;
        self.history_index = None;
        self.install_armed = false;
        self.errors.clear();
        self.info = None;
        self.info_pending = None;
        self.message = String::from("搜索中…（官方源 + AUR）");

        let (tx, rx) = mpsc::channel();
        let spawn_term = term;
        let spawned = thread::Builder::new()
            .name(String::from("pkg-search"))
            .spawn(move || {
                let outcome = probe::search(&spawn_term);
                let _ = tx.send(outcome);
            });
        match spawned {
            Ok(_) => self.search_rx = Some(rx),
            Err(error) => {
                self.searching = false;
                self.message = format!("开不了搜索线程：{error}");
            }
        }
    }

    /// 发起新闻抓取（`Ctrl+N`）。
    pub fn start_news(&mut self) {
        if self.news_rx.is_some() {
            return;
        }
        self.message = String::from("正在看 Arch 新闻…");
        let (tx, rx) = mpsc::channel();
        let spawned = thread::Builder::new()
            .name(String::from("pkg-news"))
            .spawn(move || {
                let result = probe::news().map(|items| {
                    let unread = packages::unread_news(&items, probe::last_upgrade());
                    (items, Some(unread))
                });
                let _ = tx.send(result);
            });
        match spawned {
            Ok(_) => self.news_rx = Some(rx),
            Err(error) => self.message = format!("开不了新闻线程：{error}"),
        }
    }

    /// 每帧收一次结果（和 `App::poll_job` 一样的位置调用）。
    pub fn poll(&mut self) {
        if let Some(rx) = &self.search_rx {
            match rx.try_recv() {
                Ok(outcome) => {
                    self.searching = false;
                    self.errors = outcome.errors;
                    self.hits = outcome.hits;
                    self.selected = 0;
                    self.rebuild_repos();
                    self.apply_filter();
                    self.message = if self.hits.is_empty() {
                        String::from("没有匹配的包")
                    } else {
                        format!("{} 个结果", self.hits.len())
                    };
                    self.search_rx = None;
                }
                Err(mpsc::TryRecvError::Empty) => {}
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.searching = false;
                    self.search_rx = None;
                    self.message = String::from("搜索线程没了");
                }
            }
        }

        if let Some(rx) = &self.info_rx {
            match rx.try_recv() {
                Ok(outcome) => {
                    self.info_pending = None;
                    if let Some(error) = outcome.error {
                        self.info_error = Some(error);
                        self.info = Some((outcome.name, Vec::new()));
                    } else {
                        self.info_error = None;
                        let mut fields = outcome.fields;
                        fields.truncate(INFO_FIELDS_MAX);
                        self.info = Some((outcome.name, fields));
                    }
                    self.info_rx = None;
                }
                Err(mpsc::TryRecvError::Empty) => {}
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.info_pending = None;
                    self.info_rx = None;
                }
            }
        }

        if let Some(rx) = &self.news_rx {
            match rx.try_recv() {
                Ok(Ok((items, unread))) => {
                    self.news_unread = unread;
                    self.message = match unread {
                        Some(0) => format!("Arch 新闻 {} 条，都读过了", items.len()),
                        Some(count) => format!("Arch 新闻：{count} 条是升级之后发布的"),
                        None => format!("Arch 新闻 {} 条", items.len()),
                    };
                    self.news = items;
                    self.news_rx = None;
                }
                Ok(Err(error)) => {
                    self.message = format!("抓新闻失败：{error}");
                    self.news_rx = None;
                }
                Err(mpsc::TryRecvError::Empty) => {}
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.news_rx = None;
                }
            }
        }

        // 选中的包变了就自动取包信息（去重 + 一次只飞一个请求）
        self.follow_selection();
    }

    /// 让信息面板跟上当前选中的包。
    fn follow_selection(&mut self) {
        if self.info_rx.is_some() {
            return;
        }
        let Some(hit) = self.selected_hit().cloned() else {
            return;
        };
        let shown = self.info.as_ref().map(|(name, _)| name.as_str());
        if shown == Some(hit.name.as_str())
            || self.info_pending.as_deref() == Some(hit.name.as_str())
        {
            return;
        }

        self.info_pending = Some(hit.name.clone());
        let (tx, rx) = mpsc::channel();
        let spawned = thread::Builder::new()
            .name(String::from("pkg-info"))
            .spawn(move || {
                let _ = tx.send(probe::info(&hit));
            });
        if spawned.is_ok() {
            self.info_rx = Some(rx);
        } else {
            self.info_pending = None;
        }
    }

    // ── 结果与筛选 ───────────────────────────────────────────────────────

    /// 结果里出现过哪些仓库（筛选用）。
    fn rebuild_repos(&mut self) {
        let previous: Vec<(String, bool)> = self.repos.clone();
        let mut repos: Vec<(String, bool)> = Vec::new();
        for hit in &self.hits {
            if repos.iter().any(|(name, _)| name == &hit.repo) {
                continue;
            }
            // 之前关掉的仓库保持关着（刷新结果不该把筛选重置）
            let enabled = previous
                .iter()
                .find(|(name, _)| name == &hit.repo)
                .map(|(_, enabled)| *enabled)
                .unwrap_or(true);
            repos.push((hit.repo.clone(), enabled));
        }
        // AUR 排最后（官方源的包更常用）
        repos.sort_by_key(|(name, _)| name == "aur");
        self.repos = repos;
    }

    pub fn apply_filter(&mut self) {
        self.visible = self
            .hits
            .iter()
            .enumerate()
            .filter(|(_, hit)| {
                self.repos
                    .iter()
                    .find(|(name, _)| name == &hit.repo)
                    .map(|(_, enabled)| *enabled)
                    .unwrap_or(true)
            })
            .map(|(index, _)| index)
            .collect();
        if self.selected >= self.visible.len() {
            self.selected = self.visible.len().saturating_sub(1);
        }
    }

    pub fn visible_len(&self) -> usize {
        self.visible.len()
    }

    pub fn visible_hit(&self, row: usize) -> Option<&PackageHit> {
        self.visible
            .get(row)
            .and_then(|&index| self.hits.get(index))
    }

    pub fn selected_hit(&self) -> Option<&PackageHit> {
        self.visible_hit(self.selected)
    }

    /// 切换某个仓库标签的开关。
    pub fn toggle_repo(&mut self, index: usize) {
        if let Some(entry) = self.repos.get_mut(index) {
            entry.1 = !entry.1;
            let name = entry.0.clone();
            let enabled = entry.1;
            self.message = format!("{} {}", if enabled { "显示" } else { "隐藏" }, name);
            self.apply_filter();
            self.selected = self.selected.min(self.visible.len().saturating_sub(1));
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
        self.query = self.history[next].clone();
    }

    pub fn move_selection(&mut self, delta: isize) {
        if self.focus_queue {
            let len = self.queue.len() as isize;
            if len == 0 {
                self.queue_selected = 0;
                return;
            }
            self.queue_selected = (self.queue_selected as isize + delta).rem_euclid(len) as usize;
            return;
        }
        let len = self.visible.len() as isize;
        if len == 0 {
            self.selected = 0;
            return;
        }
        self.selected = (self.selected as isize + delta).rem_euclid(len) as usize;
    }

    /// 焦点在结果 / 队列之间切换。
    pub fn toggle_focus(&mut self) {
        self.focus_queue = !self.focus_queue;
        self.message = if self.focus_queue {
            format!("队列 {} 个（Del 移除）", self.queue.len())
        } else {
            String::from("回到结果")
        };
    }

    // ── 安装队列 ─────────────────────────────────────────────────────────

    /// `Space`：把选中的包加进队列，或者（焦点在队列时）移出去。
    pub fn toggle_queue(&mut self) {
        if self.focus_queue {
            if self.queue_selected < self.queue.len() {
                let removed = self.queue.remove(self.queue_selected);
                self.message = format!("从队列移除 {}", removed.name);
                if self.queue_selected >= self.queue.len() {
                    self.queue_selected = self.queue.len().saturating_sub(1);
                }
            }
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
            "已加入队列：{} {}（Esc 上面看队列，Enter 安装）",
            hit.name,
            if hit.is_aur() { "AUR" } else { &hit.repo }
        );
    }

    /// 队列里的包名（安装命令用）。
    pub fn queue_names(&self) -> Vec<String> {
        self.queue.iter().map(|item| item.name.clone()).collect()
    }

    /// 清空队列。
    pub fn clear_queue(&mut self) {
        self.queue.clear();
        self.queue_selected = 0;
        self.focus_queue = false;
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

    /// 排队的包里有 AUR 的吗（安装时用来说明「要编译」）。
    pub fn queue_has_aur(&self) -> bool {
        self.queue.iter().any(|item| item.origin == "aur")
    }

    // ── 给 App 用的信息 ──────────────────────────────────────────────────

    /// 状态行右边那句话。
    pub fn status_line(&self) -> String {
        if self.searching {
            return String::from("搜索中…");
        }
        if let Some(term) = &self.searched {
            format!("「{term}」{} 个结果", self.visible.len())
        } else {
            String::from("输入关键词回车搜索（官方源 + AUR）")
        }
    }

    /// 变了的仓库标签（UI 上 `[core✓]` 这种）。
    pub fn repo_chips(&self) -> Vec<(String, bool)> {
        self.repos.clone()
    }

    /// 是否需要把某件东西写进历史文件（App 在关闭视图时调用）。
    pub fn persist_history(&self) {
        let _ = packages::save_searches_to(&packages::searches_path(), &self.history);
    }
}

/// 队列导出/导入用的默认路径。
pub fn default_queue_path() -> PathBuf {
    packages::queue_path()
}
