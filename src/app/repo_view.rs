//! 「发现 / 仓库 / 已安装」这个界面的状态机。
//!
//! 和软件包中心同一个套路：它是一个内置界面（RunMode::Native），由「发现」域里
//! 那个 repository-center 动作打开。
//!
//! 界面本身不做任何网络或磁盘重活：装 / 卸 / 刷新全部交给
//! repository::worker，这里只管「现在显示什么、选中了谁、下一步问什么」。
//! 这条纪律和包管理中心一样，也是那次「打开界面卡三秒」的教训换来的。

use std::collections::BTreeMap;

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::{
    app::TextInput,
    repository::{
        config::RepositoryConfig,
        install::{InstallPlan, InstallReport, UninstallReport},
        installed::InstalledPackage,
        service::{PackageHit, RepositoryStatus, SearchScope, Service},
        worker::{RepositoryWorker, Response},
    },
};

/// 这个界面里的三个面板。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RepoMode {
    /// 在所有已启用仓库里搜包。
    Discover,
    /// 本机已经装了哪些工具包。
    Installed,
    /// 有哪些仓库、开没开。
    Repositories,
}

impl RepoMode {
    pub const ALL: [RepoMode; 3] = [
        RepoMode::Discover,
        RepoMode::Installed,
        RepoMode::Repositories,
    ];

    pub fn label(self) -> &'static str {
        match self {
            RepoMode::Discover => "发现",
            RepoMode::Installed => "已安装",
            RepoMode::Repositories => "仓库",
        }
    }

    pub fn id(self) -> &'static str {
        match self {
            RepoMode::Discover => "discover",
            RepoMode::Installed => "installed",
            RepoMode::Repositories => "repositories",
        }
    }

    pub fn from_id(raw: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|mode| mode.id() == raw.trim())
    }

    /// 面板这一行下面那句提示。
    pub fn hint(self) -> &'static str {
        match self {
            RepoMode::Discover => "Enter 安装 · u 刷新索引 · / 搜索 · Tab 切面板",
            RepoMode::Installed => "Enter 看详情 · Delete 卸载 · U 升级",
            RepoMode::Repositories => "Space 启用/停用 · a 添加 · Delete 删除 · u 刷新",
        }
    }
}

/// 安装确认面板的内容。
#[derive(Clone, Debug)]
pub struct Confirm {
    pub plan: InstallPlan,
    /// 来源没给哈希时用户必须显式接受。
    pub allow_unverified: bool,
    /// 上一条为真时，用户还得再按一次 —— 这一屏不是走个流程。
    pub acknowledged: bool,
    /// 要执行的是「升级」还是「安装」。
    pub upgrading: bool,
}

impl Confirm {
    /// 还没确认时，面板上那句「再按一次」的理由；已确认就是 °None°。
    pub fn pending_reason(&self) -> Option<String> {
        if self.acknowledged {
            return None;
        }
        let mut reasons = Vec::new();
        if self.allow_unverified {
            reasons.push(String::from("来源没有提供 SHA-256，内容没法核对"));
        }
        if self.plan.needs_caution_ack() {
            reasons.push(format!(
                "这个包自标为「{}」，它的动作会改动系统",
                self.plan.danger.label()
            ));
        }
        if self.plan.needs_modified_ack() {
            reasons.push(format!(
                "有 {} 个文件你装完之后改过，这次会覆盖它们",
                self.plan.modified_files().len()
            ));
        }
        if reasons.is_empty() {
            reasons.push(String::from("请再确认一次"));
        }
        Some(reasons.join("；"))
    }
}

/// 加仓库时的输入态。
#[derive(Clone, Debug)]
pub struct AddInput {
    pub text: TextInput,
}

/// 这个界面的全部状态。
pub struct RepositoryView {
    /// 服务层（CLI 用的是同一个）。界面只读它，慢活交给 worker。
    pub service: Service,
    worker: RepositoryWorker,
    pub mode: RepoMode,
    pub query: TextInput,
    pub searching: bool,
    pub scope: SearchScope,
    pub hits: Vec<PackageHit>,
    pub installed: Vec<InstalledPackage>,
    pub statuses: Vec<RepositoryStatus>,
    pub selected: usize,
    pub confirm: Option<Box<Confirm>>,
    pub add: Option<AddInput>,
    /// 正在进行的事（后台线程报上来的）。
    pub busy: Option<String>,
    pub message: String,
    /// 索引里那些非致命的问题（某个包坏了之类）。
    pub warnings: Vec<String>,
}

impl RepositoryView {
    /// 打开界面：读缓存、起后台线程。不联网。
    pub fn open(service: Service) -> Result<Self, String> {
        let worker = RepositoryWorker::start(service.clone())?;
        let mut view = Self {
            service,
            worker,
            mode: RepoMode::Discover,
            query: TextInput::new(),
            searching: false,
            scope: SearchScope::All,
            hits: Vec::new(),
            installed: Vec::new(),
            statuses: Vec::new(),
            selected: 0,
            confirm: None,
            add: None,
            busy: None,
            message: String::new(),
            warnings: Vec::new(),
        };
        view.reload();
        Ok(view)
    }

    /// 重新读本地数据（不联网）。
    pub fn reload(&mut self) {
        self.hits = self.service.search(self.query.text(), self.scope);
        self.installed = self.service.installed();
        self.statuses = self.service.statuses();
        self.clamp_selection();
    }

    pub fn set_mode(&mut self, mode: RepoMode) {
        self.mode = mode;
        self.selected = 0;
        self.confirm = None;
        self.add = None;
        self.searching = false;
        self.clamp_selection();
    }

    pub fn cycle_mode(&mut self, delta: isize) {
        let index = RepoMode::ALL
            .iter()
            .position(|mode| *mode == self.mode)
            .unwrap_or(0) as isize;
        let len = RepoMode::ALL.len() as isize;
        let next = (index + delta).rem_euclid(len) as usize;
        self.set_mode(RepoMode::ALL[next]);
    }

    /// 当前面板里有多少行。
    pub fn len(&self) -> usize {
        match self.mode {
            RepoMode::Discover => self.hits.len(),
            RepoMode::Installed => self.installed.len(),
            RepoMode::Repositories => self.statuses.len(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn clamp_selection(&mut self) {
        let len = self.len();
        if len == 0 {
            self.selected = 0;
        } else if self.selected >= len {
            self.selected = len - 1;
        }
    }

    pub fn move_selection(&mut self, delta: isize) {
        let len = self.len() as isize;
        if len == 0 {
            return;
        }
        self.selected = (self.selected as isize + delta).clamp(0, len - 1) as usize;
    }

    pub fn select_first(&mut self) {
        self.selected = 0;
    }

    pub fn select_last(&mut self) {
        self.selected = self.len().saturating_sub(1);
    }

    pub fn selected_hit(&self) -> Option<&PackageHit> {
        self.hits.get(self.selected)
    }

    pub fn selected_installed(&self) -> Option<&InstalledPackage> {
        self.installed.get(self.selected)
    }

    pub fn selected_status(&self) -> Option<&RepositoryStatus> {
        self.statuses.get(self.selected)
    }

    /// 有没有哪个仓库还没取过索引（用来提示「先刷新」）。
    pub fn has_usable_index(&self) -> bool {
        self.statuses
            .iter()
            .any(|status| status.config.enabled && status.state.usable())
    }

    // ── 后台 ────────────────────────────────────────────────────────────────

    /// 收后台线程的回应。返回「有没有变化」。
    pub fn poll(&mut self) -> bool {
        let mut changed = false;
        while let Some(response) = self.worker.try_recv() {
            changed = true;
            match response {
                Response::Progress(text) => self.busy = Some(text),
                Response::Refreshed(fetch) => {
                    self.busy = None;
                    let when = fetch
                        .fetched_at
                        .map(crate::repository::cli::relative_time)
                        .unwrap_or_else(|| String::from("还没取过"));
                    self.message =
                        format!("{} · {} · {when}", fetch.message(), fetch.trust.label());
                    let _ = &fetch.repository;
                    self.warnings = fetch.warnings.clone();
                }
                Response::RefreshFinished(results) => {
                    self.busy = None;
                    let ok = results
                        .iter()
                        .filter(|result| result.state.usable())
                        .count();
                    self.message = format!("刷新完成：{ok}/{} 个仓库可用", results.len());
                    self.warnings = results
                        .iter()
                        .flat_map(|result| result.warnings.clone())
                        .collect();
                }
                Response::Installed { id, outcome } => {
                    self.busy = None;
                    self.confirm = None;
                    self.message = install_message(&id, outcome);
                    self.reload();
                }
                Response::Upgraded { id, outcome } => {
                    self.busy = None;
                    self.confirm = None;
                    self.message = install_message(&id, outcome);
                    self.reload();
                }
                Response::Uninstalled { id, outcome } => {
                    self.busy = None;
                    self.confirm = None;
                    self.message = uninstall_message(&id, outcome);
                    self.reload();
                }
            }
        }
        changed
    }

    /// 刷新全部已启用仓库（联网，走后台）。
    pub fn refresh_all(&mut self) {
        match self.worker.refresh_all() {
            Ok(()) => {
                self.busy = Some(String::from("正在刷新全部仓库…"));
                self.message = String::from("正在刷新索引…");
            }
            Err(problem) => self.message = problem,
        }
    }

    /// 刷新选中的那个仓库。
    pub fn refresh_selected(&mut self) {
        let Some(status) = self.selected_status() else {
            self.refresh_all();
            return;
        };
        let id = status.config.id.clone();
        self.refresh_by_id(&id);
    }

    fn refresh_by_id(&mut self, id: &str) {
        match self.worker.refresh(id.to_string()) {
            Ok(()) => {
                self.busy = Some(format!("正在刷新 {id}…"));
                self.message = format!("正在刷新 {id}…");
            }
            Err(problem) => self.message = problem,
        }
    }

    // ── 动作 ────────────────────────────────────────────────────────────────

    /// 准备安装 / 升级选中的包：先算计划，再摆给用户看。
    pub fn ask_install(&mut self) {
        if self.mode == RepoMode::Installed {
            self.show_installed_detail();
            return;
        }
        let Some(hit) = self.selected_hit() else {
            return;
        };
        let id = hit.id.clone();
        let upgrading = hit.is_installed();
        let plan = match self.service.plan(&id) {
            Ok(plan) => plan,
            Err(problem) => {
                self.message = problem;
                return;
            }
        };

        self.offer(plan, upgrading);
    }

    /// 把一个安装计划摆成确认面板。
    ///
    /// 两种「要多按一次」的情况都在这里收口：
    ///   * 来源没给哈希（内容没法核对）；
    ///   * 包自标为「注意」（它的动作会改动系统）。
    ///
    /// 安装与升级都走它 —— 升级同样是下载并执行别人新写的代码。
    fn offer(&mut self, plan: InstallPlan, upgrading: bool) {
        let caution = plan.needs_caution_ack();
        // 你改过的文件也会被覆盖 —— 卸载那边是保留的，这里不能装作没看见。
        let modified = plan.needs_modified_ack();
        // 先按「不额外开绿灯」检查一遍；只有「来源没给哈希」这一条能被放宽。
        match plan.check(&self.service.roots, false) {
            Ok(()) => {
                if modified {
                    self.message = format!(
                        "有 {} 个文件你装完之后改过，这次会覆盖它们 —— 再按一次 Enter 表示你接受",
                        plan.modified_files().len()
                    );
                } else if caution {
                    self.message = String::from(
                        "这个包自标为「注意」：它的动作会改动系统 —— 再按一次 Enter 表示你知道",
                    );
                }
                self.confirm = Some(Box::new(Confirm {
                    plan,
                    allow_unverified: false,
                    acknowledged: !(caution || modified),
                    upgrading,
                }));
            }
            Err(problem) => {
                if plan.check(&self.service.roots, true).is_ok() {
                    self.message = String::from("来源没有提供哈希 —— 再按一次 Enter 表示你接受");
                    self.confirm = Some(Box::new(Confirm {
                        plan,
                        allow_unverified: true,
                        acknowledged: false,
                        upgrading,
                    }));
                } else {
                    self.message = problem;
                }
            }
        }
    }

    /// 确认面板上按 Enter。
    pub fn confirm_accept(&mut self) {
        let Some(confirm) = self.confirm.as_mut() else {
            return;
        };
        if !confirm.acknowledged {
            confirm.acknowledged = true;
            let what = if confirm.upgrading { "升级" } else { "装" };
            let reason = confirm
                .pending_reason()
                .unwrap_or_else(|| String::from("确认过了"));
            self.message = format!("{reason} —— 再按一次 Enter 才真的{what}");
            return;
        }
        let plan = confirm.plan.clone();
        let allow_unverified = confirm.allow_unverified;
        let upgrading = confirm.upgrading;
        // 升级不用面板上那份计划：摆出来之后索引可能已经刷新过，按 id 重算一遍
        // 再装才是「升到最新」。worker 的 Upgrade 分支就是干这个的。
        let outcome = if upgrading {
            self.worker.upgrade(plan.id.clone(), allow_unverified)
        } else {
            self.worker.install(plan, allow_unverified)
        };
        match outcome {
            Ok(()) => {
                self.busy = Some(String::from(if upgrading {
                    "正在升级…"
                } else {
                    "正在安装…"
                }));
            }
            Err(problem) => {
                self.message = problem;
                self.confirm = None;
            }
        }
    }

    pub fn confirm_cancel(&mut self) {
        self.confirm = None;
        self.message = String::from("已取消");
    }

    /// 卸载选中的包。
    pub fn ask_uninstall(&mut self) {
        let Some(package) = self.selected_installed() else {
            return;
        };
        let id = package.id.clone();
        let name = package.name.clone();
        match self.worker.uninstall(id.clone(), false) {
            Ok(()) => self.busy = Some(format!("正在卸载 {name}…")),
            Err(problem) => self.message = problem,
        }
    }

    /// 升级选中的包。
    pub fn upgrade_selected(&mut self) {
        let id = match self.mode {
            RepoMode::Installed => self.selected_installed().map(|package| package.id.clone()),
            _ => self
                .selected_hit()
                .filter(|hit| hit.upgradable)
                .map(|hit| hit.id.clone()),
        };
        let Some(id) = id else {
            self.message = String::from("这一项没有可升级的版本");
            return;
        };
        // 升级同样是下载并执行别人新写的代码，所以走和安装同一条确认路径。
        match self.service.plan(&id) {
            Ok(plan) => self.offer(plan, true),
            Err(problem) => self.message = problem,
        }
    }

    /// 在「仓库」面板里切换启用 / 停用。
    pub fn toggle_enabled(&mut self) {
        let Some(status) = self.selected_status() else {
            return;
        };
        let id = status.config.id.clone();
        let next = !status.config.enabled;
        let mut repositories = self.service.repositories.clone();
        if !repositories.set_enabled(&id, next) {
            return;
        }
        let what = if next { "已启用" } else { "已停用" };
        match self.save_repositories(repositories) {
            Ok(()) => self.message = format!("{what}「{id}」"),
            Err(problem) => self.message = problem,
        }
    }

    /// 删掉选中的仓库。
    pub fn remove_selected_repository(&mut self) {
        let Some(status) = self.selected_status() else {
            return;
        };
        let id = status.config.id.clone();
        let mut repositories = self.service.repositories.clone();
        if !repositories.remove(&id) {
            return;
        }
        match self.save_repositories(repositories) {
            Ok(()) => self.message = format!("已删除仓库「{id}」"),
            Err(problem) => self.message = problem,
        }
    }

    fn save_repositories(
        &mut self,
        repositories: crate::repository::config::Repositories,
    ) -> Result<(), String> {
        let path = self.service.roots.config.join("repositories.toml");
        crate::repository::config::save_to(&path, &repositories)?;
        self.service = Service::with_parts(
            repositories,
            self.service.roots.clone(),
            self.service.cache_root.clone(),
        );
        self.reload();
        Ok(())
    }

    /// 开始输入新仓库的地址。
    pub fn start_add(&mut self) {
        self.add = Some(AddInput {
            text: TextInput::new(),
        });
        self.message = String::from("输入索引地址（URL / 本地路径 / file://…），Enter 确认");
    }

    /// 提交新仓库。
    pub fn commit_add(&mut self) {
        let Some(add) = self.add.take() else {
            return;
        };
        let location = add.text.text().trim().to_string();
        if location.is_empty() {
            self.message = String::from("地址是空的，没有添加");
            return;
        }
        let name = crate::repository::config::slug(&location);
        let config = match RepositoryConfig::user_added(&name, &location) {
            Ok(config) => config,
            Err(problem) => {
                self.message = problem;
                return;
            }
        };
        let mut repositories = self.service.repositories.clone();
        let id = repositories.add(config);
        match self.save_repositories(repositories) {
            Ok(()) => {
                self.message = format!("已添加仓库「{id}」，正在取索引…");
                self.refresh_by_id(&id);
            }
            Err(problem) => self.message = problem,
        }
    }

    pub fn cancel_add(&mut self) {
        self.add = None;
        self.message = String::from("已取消");
    }

    /// 「已安装」面板里的详情：把账本里的文件数说清楚。
    pub fn show_installed_detail(&mut self) {
        let Some(package) = self.selected_installed() else {
            return;
        };
        self.message = format!(
            "{} {}：{} 个文件 · 来自 {}",
            package.id,
            package.version,
            package.file_count(),
            package.repository_name
        );
    }

    /// 关键词变了（索引在本地，就地重算，不联网）。
    pub fn apply_query(&mut self) {
        self.hits = self.service.search(self.query.text(), self.scope);
        self.selected = 0;
    }

    // ── 输入 ────────────────────────────────────────────────────────────────

    /// 处理一个按键。返回 Ok(false) 表示「请求关闭这个界面」。
    pub fn handle_key(&mut self, key: KeyEvent) -> Result<bool, String> {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);

        // 输入态优先：加仓库的地址行。
        if self.add.is_some() {
            match key.code {
                KeyCode::Enter => self.commit_add(),
                KeyCode::Esc => self.cancel_add(),
                KeyCode::Backspace => {
                    if let Some(add) = self.add.as_mut() {
                        add.text.backspace();
                    }
                }
                KeyCode::Char(ch) if !ctrl => {
                    if let Some(add) = self.add.as_mut() {
                        add.text.insert(ch);
                    }
                }
                _ => {}
            }
            return Ok(true);
        }

        // 搜索框。
        if self.searching {
            match key.code {
                KeyCode::Enter | KeyCode::Esc => self.searching = false,
                KeyCode::Backspace => {
                    self.query.backspace();
                    self.apply_query();
                }
                KeyCode::Char('u') if ctrl => {
                    self.query.clear();
                    self.apply_query();
                }
                KeyCode::Char(ch) if !ctrl => {
                    self.query.insert(ch);
                    self.apply_query();
                }
                _ => {}
            }
            return Ok(true);
        }

        // 确认面板：只有 Enter / Esc / n 有意义。
        if self.confirm.is_some() {
            match key.code {
                KeyCode::Enter => self.confirm_accept(),
                KeyCode::Esc | KeyCode::Char('n') | KeyCode::Char('q') => self.confirm_cancel(),
                _ => {}
            }
            return Ok(true);
        }

        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => return Ok(false),
            KeyCode::Tab => self.cycle_mode(1),
            KeyCode::BackTab => self.cycle_mode(-1),
            KeyCode::Char('1') => self.set_mode(RepoMode::Discover),
            KeyCode::Char('2') => self.set_mode(RepoMode::Installed),
            KeyCode::Char('3') => self.set_mode(RepoMode::Repositories),
            KeyCode::Char('/') => self.searching = true,
            KeyCode::Up | KeyCode::Char('k') if !ctrl => self.move_selection(-1),
            KeyCode::Down | KeyCode::Char('j') if !ctrl => self.move_selection(1),
            KeyCode::PageUp | KeyCode::Char('u') if ctrl => self.move_selection(-8),
            KeyCode::PageDown | KeyCode::Char('d') if ctrl => self.move_selection(8),
            KeyCode::Home | KeyCode::Char('g') => self.select_first(),
            KeyCode::End | KeyCode::Char('G') => self.select_last(),
            KeyCode::Enter => match self.mode {
                RepoMode::Discover => self.ask_install(),
                RepoMode::Installed => self.show_installed_detail(),
                RepoMode::Repositories => self.toggle_enabled(),
            },
            KeyCode::Char('i') => self.ask_install(),
            KeyCode::Char('U') => self.upgrade_selected(),
            KeyCode::Char('u') => self.refresh_all(),
            KeyCode::Char('r') => self.refresh_selected(),
            KeyCode::Char('a') => self.start_add(),
            KeyCode::Char(' ') if self.mode == RepoMode::Repositories => self.toggle_enabled(),
            KeyCode::Delete | KeyCode::Char('x') => match self.mode {
                RepoMode::Repositories => self.remove_selected_repository(),
                _ => self.ask_uninstall(),
            },
            _ => {}
        }
        Ok(true)
    }

    /// 鼠标滚轮：滚动列表。
    pub fn handle_scroll(&mut self, delta: isize) {
        self.move_selection(delta);
    }

    /// 列表视口的起始下标。
    ///
    /// 列表比屏幕长时视口跟随选中行；绘制和鼠标点击都从这里取同一个值，
    /// 否则「看到的行」和「点到的行」会差一屏。
    pub fn viewport_start(&self, visible: usize) -> usize {
        if visible == 0 {
            return 0;
        }
        self.selected.saturating_add(1).saturating_sub(visible)
    }

    /// 鼠标点在视口里的第 row 行。
    pub fn click_viewport_row(&mut self, row: usize, visible: usize) {
        let index = self.viewport_start(visible) + row;
        if index < self.len() {
            self.selected = index;
        }
    }
}

fn install_message(id: &str, outcome: Result<InstallReport, String>) -> String {
    match outcome {
        Ok(report) => format!(
            "已安装 {} {}（{}）：{} 个文件",
            report.id,
            report.version,
            report.integrity.label(),
            report.files.len()
        ),
        Err(problem) => format!("安装 {id} 失败：{problem}"),
    }
}

fn uninstall_message(id: &str, outcome: Result<UninstallReport, String>) -> String {
    match outcome {
        Ok(report) => report.summary(),
        Err(problem) => format!("卸载 {id} 失败：{problem}"),
    }
}

/// 索引里那些坏包的问题按来源归堆（详情区显示用）。
#[allow(dead_code)] // 按来源归堆索引问题
pub fn group_warnings(warnings: &[String]) -> BTreeMap<String, Vec<String>> {
    let mut grouped: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for warning in warnings {
        let key = warning
            .split([':', '：'])
            .next()
            .unwrap_or("其它")
            .trim()
            .to_string();
        grouped.entry(key).or_default().push(warning.clone());
    }
    grouped
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use super::*;
    use crate::repository::{
        cache,
        config::{Repositories, RepositoryConfig},
        install::Roots,
    };

    fn temp(tag: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("toolbox-hub-repoview-{tag}-{nanos}"));
        std::fs::create_dir_all(&dir).expect("mkdir");
        dir
    }

    const INDEX: &str = r#"{"schema_version": 1, "packages": [
        {"id": "hello-tool", "name": "Hello", "version": "1.0.0", "summary": "打招呼",
         "artifact": {"url": "artifacts/h.sh", "sha256": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"},
         "files": [{"path": "scripts/hello-tool", "kind": "bin"}]},
        {"id": "other", "name": "别的", "version": "2.0.0", "summary": "别的东西",
         "files": [{"path": "scripts/other", "kind": "bin"}]}
    ]}"#;

    fn service(base: &Path, index: &str) -> Service {
        let meta = cache::IndexMeta {
            fetched_at: Some(cache::now_secs()),
            ..cache::IndexMeta::default()
        };
        cache::store(&base.join("cache"), "official", index, &meta).expect("store");
        let repositories = Repositories {
            repositories: vec![RepositoryConfig {
                id: String::from("official"),
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
        Service::with_parts(repositories, roots, base.join("cache"))
    }

    fn view(base: &Path) -> RepositoryView {
        RepositoryView::open(service(base, INDEX)).expect("open")
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn opening_lists_what_the_cache_knows_without_networking() {
        let base = temp("open");
        let view = view(&base);
        assert_eq!(view.mode, RepoMode::Discover);
        assert_eq!(view.hits.len(), 2);
        assert_eq!(view.statuses.len(), 1);
        assert!(view.installed.is_empty());
        assert!(view.has_usable_index());
        std::fs::remove_dir_all(&base).expect("cleanup");
    }

    #[test]
    fn tabs_cycle_and_number_keys_jump() {
        let base = temp("modes");
        let mut view = view(&base);
        view.handle_key(key(KeyCode::Tab)).expect("tab");
        assert_eq!(view.mode, RepoMode::Installed);
        view.handle_key(key(KeyCode::Tab)).expect("tab");
        assert_eq!(view.mode, RepoMode::Repositories);
        view.handle_key(key(KeyCode::Tab)).expect("tab");
        assert_eq!(view.mode, RepoMode::Discover, "绕回来了");
        view.handle_key(key(KeyCode::Char('3'))).expect("3");
        assert_eq!(view.mode, RepoMode::Repositories);
        for mode in RepoMode::ALL {
            assert!(!mode.hint().is_empty());
            assert_eq!(RepoMode::from_id(mode.id()), Some(mode));
        }
        std::fs::remove_dir_all(&base).expect("cleanup");
    }

    /// 搜索是本地重算：打一个字母就筛，不发网络请求。
    #[test]
    fn searching_filters_the_local_index() {
        let base = temp("search");
        let mut view = view(&base);
        view.handle_key(key(KeyCode::Char('/'))).expect("/");
        assert!(view.searching);
        for ch in "hello".chars() {
            view.handle_key(key(KeyCode::Char(ch))).expect("type");
        }
        assert_eq!(view.hits.len(), 1);
        assert_eq!(view.hits[0].id, "hello-tool");
        view.handle_key(key(KeyCode::Enter)).expect("enter");
        assert!(!view.searching, "Enter 退出输入态但结果留着");
        assert_eq!(view.hits.len(), 1);
        std::fs::remove_dir_all(&base).expect("cleanup");
    }

    #[test]
    fn selection_moves_and_stays_inside() {
        let base = temp("move");
        let mut view = view(&base);
        view.move_selection(-1);
        assert_eq!(view.selected, 0, "往上越界停在原地");
        view.move_selection(1);
        assert_eq!(view.selected, 1);
        view.move_selection(10);
        assert_eq!(view.selected, 1, "往下越界停在最后一行");
        view.select_last();
        assert_eq!(view.selected, 1);
        view.select_first();
        assert_eq!(view.selected, 0);
        // 视口起点与点击用的是同一个换算，否则会差一屏
        assert_eq!(view.viewport_start(10), 0);
        view.selected = 1;
        assert_eq!(view.viewport_start(1), 1);
        view.click_viewport_row(0, 1);
        assert_eq!(view.selected, 1);
        view.click_viewport_row(9, 1);
        assert_eq!(view.selected, 1, "点空白不改选中");
        std::fs::remove_dir_all(&base).expect("cleanup");
    }

    /// 按「安装」先把计划摆出来，而不是直接动手。
    #[test]
    fn asking_to_install_shows_a_plan_first() {
        let base = temp("plan");
        let mut view = view(&base);
        view.ask_install();
        let confirm = view.confirm.as_ref().expect("应当出现确认面板");
        assert_eq!(confirm.plan.id, "hello-tool");
        assert!(!confirm.upgrading);
        assert!(!confirm.allow_unverified, "有哈希就不该要求额外确认");
        assert_eq!(confirm.plan.files.len(), 1);
        assert!(view.busy.is_none(), "确认之前不该开始装");
        std::fs::remove_dir_all(&base).expect("cleanup");
    }

    /// 来源没有哈希时不能一次 Enter 就装。
    #[test]
    fn a_hashless_source_needs_two_deliberate_steps() {
        let base = temp("nohash");
        let index = r#"{"schema_version": 1, "packages": [
            {"id": "risky", "name": "Risky", "version": "1.0.0",
             "artifact": {"url": "https://example.com/r.tar.gz", "kind": "tar.gz"},
             "files": [{"path": "scripts/risky", "kind": "bin"}]}
        ]}"#;
        let mut view = RepositoryView::open(service(&base, index)).expect("open");
        view.ask_install();
        let confirm = view.confirm.as_ref().expect("应当出现确认面板");
        assert!(confirm.allow_unverified, "没哈希就要标出来");
        assert!(!confirm.acknowledged, "第一次 Enter 还不算确认");

        view.confirm_accept();
        assert!(view.confirm.is_some(), "第一步之后面板还在");
        assert!(view.confirm.as_ref().expect("还在").acknowledged);
        assert!(view.busy.is_none(), "确认前不许开始装");
        std::fs::remove_dir_all(&base).expect("cleanup");
    }

    #[test]
    fn cancelling_a_confirm_leaves_nothing_behind() {
        let base = temp("cancel");
        let mut view = view(&base);
        view.ask_install();
        assert!(view.confirm.is_some());
        view.confirm_cancel();
        assert!(view.confirm.is_none());
        assert!(view.busy.is_none());
        assert!(view.message.contains("取消"));
        std::fs::remove_dir_all(&base).expect("cleanup");
    }

    /// 确认面板开着时 q / Esc 是「取消」，不是「退出界面」。
    #[test]
    fn a_confirm_panel_swallows_the_quit_keys() {
        let base = temp("swallow");
        let mut view = view(&base);
        view.ask_install();
        assert!(view.handle_key(key(KeyCode::Char('q'))).expect("key"));
        assert!(view.confirm.is_none());
        std::fs::remove_dir_all(&base).expect("cleanup");
    }

    #[test]
    fn quitting_is_reported_to_the_caller() {
        let base = temp("quit");
        let mut view = view(&base);
        assert!(!view.handle_key(key(KeyCode::Esc)).expect("esc"));
        assert!(!view.handle_key(key(KeyCode::Char('q'))).expect("q"));
        std::fs::remove_dir_all(&base).expect("cleanup");
    }

    #[test]
    fn the_repository_panel_toggles_enabled_and_writes_the_config() {
        let base = temp("toggle");
        let mut view = view(&base);
        view.set_mode(RepoMode::Repositories);
        assert!(view.statuses[0].config.enabled);

        view.toggle_enabled();
        assert!(!view.service.repositories.repositories[0].enabled);
        assert!(base.join("config/repositories.toml").exists(), "要写进配置");

        assert!(
            view.hits.is_empty(),
            "停用之后索引不再参与搜索：{:?}",
            view.hits
        );
        view.toggle_enabled();
        assert!(view.service.repositories.repositories[0].enabled);
        std::fs::remove_dir_all(&base).expect("cleanup");
    }

    #[test]
    fn removing_a_repository_takes_it_off_the_list() {
        let base = temp("remove");
        let mut view = view(&base);
        view.set_mode(RepoMode::Repositories);
        view.remove_selected_repository();
        assert!(view.service.repositories.repositories.is_empty());
        assert!(view.statuses.is_empty());
        std::fs::remove_dir_all(&base).expect("cleanup");
    }

    #[test]
    fn adding_a_repository_goes_through_an_input_line() {
        let base = temp("add");
        let mut view = view(&base);
        view.handle_key(key(KeyCode::Char('a'))).expect("a");
        assert!(view.add.is_some());
        for ch in "/tmp/myrepo/index.json".chars() {
            view.handle_key(key(KeyCode::Char(ch))).expect("type");
        }
        view.handle_key(key(KeyCode::Enter)).expect("enter");
        assert!(view.add.is_none(), "提交之后输入态结束");
        assert_eq!(view.service.repositories.repositories.len(), 2);
        assert!(
            view.service
                .repositories
                .repositories
                .iter()
                .any(|repo| repo.index == "/tmp/myrepo/index.json")
        );
        std::fs::remove_dir_all(&base).expect("cleanup");
    }

    #[test]
    fn an_empty_address_is_not_added_and_esc_changes_nothing() {
        let base = temp("addempty");
        let mut view = view(&base);
        view.start_add();
        view.handle_key(key(KeyCode::Enter)).expect("enter");
        assert_eq!(view.service.repositories.repositories.len(), 1);
        assert!(view.message.contains("地址是空的"));

        view.start_add();
        view.handle_key(key(KeyCode::Char('x'))).expect("type");
        view.handle_key(key(KeyCode::Esc)).expect("esc");
        assert!(view.add.is_none());
        assert_eq!(view.service.repositories.repositories.len(), 1);
        std::fs::remove_dir_all(&base).expect("cleanup");
    }

    /// 后台线程真的会回话，界面据此更新状态行。
    #[test]
    fn the_background_refresh_updates_the_message() {
        let base = temp("bg");
        let index = base.join("index.json");
        std::fs::write(&index, INDEX).expect("write");
        let mut service = service(&base, INDEX);
        service.repositories.repositories[0].index = index.display().to_string();

        let mut view = RepositoryView::open(service).expect("open");
        view.refresh_all();
        assert!(view.busy.is_some(), "立刻要有个「正在…」");

        for _ in 0..300 {
            if view.poll() && view.busy.is_none() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(view.busy.is_none(), "跑完就该清掉进行中");
        assert!(view.message.contains("刷新完成"), "{}", view.message);
        std::fs::remove_dir_all(&base).expect("cleanup");
    }

    #[test]
    fn uninstalling_with_nothing_installed_does_nothing() {
        let base = temp("uninstall");
        let mut view = view(&base);
        view.set_mode(RepoMode::Installed);
        view.ask_uninstall();
        assert!(view.busy.is_none(), "没东西可卸就不该起任务");
        std::fs::remove_dir_all(&base).expect("cleanup");
    }

    #[test]
    fn upgrading_without_a_candidate_says_so() {
        let base = temp("upgrade");
        let mut view = view(&base);
        view.upgrade_selected();
        assert!(view.message.contains("没有可升级"), "{}", view.message);
        std::fs::remove_dir_all(&base).expect("cleanup");
    }

    #[test]
    fn scrolling_moves_the_selection() {
        let base = temp("scroll");
        let mut view = view(&base);
        view.handle_scroll(5);
        assert_eq!(view.selected, 1);
        view.handle_scroll(-3);
        assert_eq!(view.selected, 0);
        std::fs::remove_dir_all(&base).expect("cleanup");
    }

    #[test]
    fn a_view_without_any_repository_says_it_has_none() {
        let base = temp("noindex");
        let mut service = service(&base, INDEX);
        service.repositories.repositories.clear();
        let view = RepositoryView::open(service).expect("open");
        assert!(!view.has_usable_index());
        assert!(view.hits.is_empty());
        std::fs::remove_dir_all(&base).expect("cleanup");
    }
}

/// 自标为「注意」的包：安装/升级确认要多按一次。
///
/// 单独一个模块是为了自带夹具 —— 主测试模块里的 helper 是给「普通包」用的，
/// 这里要的是一个 danger = "caution" 的索引。
#[cfg(test)]
mod caution_tests {
    use super::*;
    use std::path::PathBuf;

    use crate::repository::{
        cache,
        config::{Repositories, RepositoryConfig},
        install::Roots,
        service::Service,
    };

    fn temp(tag: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("toolbox-hub-caution-{tag}-{nanos}"));
        std::fs::create_dir_all(&dir).expect("mkdir");
        dir
    }

    /// 一个自标为「注意」的包（外加一个普通包作对照）。
    fn service(base: &std::path::Path) -> Service {
        let index = r#"{"schema_version": 1, "packages": [
            {"id": "risky-tools", "name": "会改系统的工具", "version": "1.0.0",
             "summary": "删缓存", "danger": "caution",
             "artifact": {"url": "artifacts/r.tar.gz",
                          "sha256": "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"},
             "files": [{"path": "scripts/risky", "kind": "bin"}]},
            {"id": "safe-tool", "name": "安全工具", "version": "1.0.0", "summary": "只读",
             "artifact": {"url": "artifacts/s.tar.gz",
                          "sha256": "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc"},
             "files": [{"path": "scripts/safe", "kind": "bin"}]}
        ]}"#;
        let meta = cache::IndexMeta {
            fetched_at: Some(cache::now_secs()),
            ..cache::IndexMeta::default()
        };
        cache::store(&base.join("cache"), "official", index, &meta).expect("store");
        let repositories = Repositories {
            repositories: vec![RepositoryConfig {
                id: String::from("official"),
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
        Service::with_parts(repositories, roots, base.join("cache"))
    }

    fn pick(view: &mut RepositoryView, id: &str) {
        let index = view
            .hits
            .iter()
            .position(|hit| hit.id == id)
            .expect("索引里有这个包");
        view.selected = index;
    }

    #[test]
    fn a_caution_package_needs_a_second_enter() {
        let base = temp("second");
        let mut view = RepositoryView::open(service(&base)).expect("open");

        pick(&mut view, "risky-tools");
        view.ask_install();
        let confirm = view.confirm.as_ref().expect("应当出现确认面板");
        assert!(confirm.plan.needs_caution_ack(), "caution 包要认出来");
        assert!(!confirm.acknowledged, "第一次 Enter 还不算确认");
        let reason = confirm.pending_reason().expect("要给理由");
        assert!(reason.contains("会改动系统"), "{reason}");
        assert!(reason.contains("注意"), "{reason}");

        view.confirm_accept();
        assert!(view.confirm.is_some(), "第一步之后面板还在");
        assert!(view.confirm.as_ref().expect("还在").acknowledged);
        assert!(view.busy.is_none(), "确认之前不许开始装");
        assert!(
            view.confirm
                .as_ref()
                .expect("还在")
                .pending_reason()
                .is_none()
        );
        std::fs::remove_dir_all(&base).expect("cleanup");
    }

    /// 普通的 safe 包不该多要一次 —— 否则「多按一次」会变成噪音，谁也不看。
    #[test]
    fn a_safe_package_is_confirmed_once() {
        let base = temp("safe");
        let mut view = RepositoryView::open(service(&base)).expect("open");
        pick(&mut view, "safe-tool");
        view.ask_install();
        let confirm = view.confirm.as_ref().expect("应当出现确认面板");
        assert!(!confirm.plan.needs_caution_ack());
        assert!(confirm.acknowledged, "safe 包一次确认就够");
        assert!(confirm.pending_reason().is_none());
        std::fs::remove_dir_all(&base).expect("cleanup");
    }

    /// 升级走同一条确认路径 —— 升级同样是下载并执行别人新写的代码。
    #[test]
    fn upgrading_a_caution_package_also_asks_twice() {
        use crate::repository::installed::{self, SCHEMA_VERSION};

        let base = temp("upgrade");
        // 先让它「已安装」—— 没装过就谈不上升级（那条路径会提前返回）。
        let package = installed::InstalledPackage {
            schema_version: SCHEMA_VERSION,
            id: String::from("risky-tools"),
            name: String::from("会改系统的工具"),
            version: String::from("0.9.0"),
            repository: String::from("official"),
            repository_name: String::from("ToolHub Official"),
            trust: String::from("trusted"),
            installed_at: 0,
            source: None,
            license: None,
            requires_root: false,
            danger: String::from("caution"),
            artifact_sha256: None,
            dependencies: Vec::new(),
            files: Vec::new(),
            allow_unverified: false,
        };
        installed::save(&base.join("data"), &package).expect("save");

        let mut view = RepositoryView::open(service(&base)).expect("open");
        pick(&mut view, "risky-tools");
        view.upgrade_selected();

        // 升级照样要过确认面板，而不是直接开跑。
        let confirm = view.confirm.as_ref().expect("应当摆出确认面板");
        assert!(!confirm.acknowledged, "第一次 Enter 还不算确认");
        assert!(view.busy.is_none(), "确认之前不许开始升级");
        assert!(confirm.upgrading, "走的是升级语义");

        view.confirm_accept();
        assert!(view.confirm.as_ref().expect("还在").acknowledged);
        std::fs::remove_dir_all(&base).expect("cleanup");
    }
}
