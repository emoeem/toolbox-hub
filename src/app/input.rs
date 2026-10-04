//! 按键处理。
//!
//! 键位约定（与第一版保持一致，新增的只有域/分类切换）：
//!
//! | 按键 | 行为 |
//! | --- | --- |
//! | `↑`/`k` `↓`/`j` `PgUp`/`PgDn` | 移动高亮 |
//! | `Enter` | 执行已标记项（没有标记则执行当前项） |
//! | `Tab` / `Space` | 标记 / 取消标记 |
//! | `←` `→` | 切换一级域 |
//! | `h`/`[` `l`/`]` | 切换二级筛选 |
//! | `1`..`6` | 直接跳到某个域 |
//! | `/` | 搜索 |
//! | `Ctrl-R` | 重新发现 Provider |
//! | `q` / `Esc` | 退出（搜索状态下 `Esc` 只清空搜索） |

use std::{path::Path, time::Instant};

use crate::{
    app::{
        App,
        package_view::{ConfirmAction, PackageMode, Pane},
    },
    model::{Domain, RunMode},
    runtime,
};
use ratatui::{
    crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind},
    crossterm::terminal::size,
    layout::{Position, Rect},
};

/// 处理鼠标输入。
///
/// 第一阶段只覆盖高价值交互：主列表点击/双击、滚轮、域 Tabs、包中心结果/队列。
/// 命中区域与渲染布局保持同一套约束，避免“画在这里、点到那里”。
pub fn handle_mouse(
    app: &mut App,
    mouse: MouseEvent,
    cwd: &Path,
) -> Result<(), Box<dyn std::error::Error>> {
    if app.is_help_open() {
        if matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left)) {
            app.close_help();
        }
        return Ok(());
    }

    if app.repository.is_some() {
        return handle_repository_mouse(app, mouse);
    }

    if app.packages.is_some() {
        return handle_packages_mouse(app, mouse, cwd);
    }

    let (width, height) = size().unwrap_or((80, 24));
    let screen = Rect::new(0, 0, width, height);
    let areas = crate::ui::main_layout(app, screen);
    if areas.too_small {
        return Ok(());
    }
    let point = Position::new(mouse.column, mouse.row);
    let content = areas.list.union(areas.detail);

    if app.viewer.is_some() {
        if content.contains(point) {
            match mouse.kind {
                MouseEventKind::ScrollUp => app.viewer_scroll(-3),
                MouseEventKind::ScrollDown => app.viewer_scroll(3),
                _ => {}
            }
        }
        return Ok(());
    }
    if app.files.is_some() {
        if content.contains(point) {
            match mouse.kind {
                MouseEventKind::ScrollUp => app.files_move(-3),
                MouseEventKind::ScrollDown => app.files_move(3),
                _ => {}
            }
        }
        return Ok(());
    }
    if app.history.is_some() {
        if content.contains(point) {
            match mouse.kind {
                MouseEventKind::ScrollUp => app.history_move(-3),
                MouseEventKind::ScrollDown => app.history_move(3),
                _ => {}
            }
        }
        return Ok(());
    }
    if app.picker.is_some() {
        if content.contains(point) {
            match mouse.kind {
                MouseEventKind::ScrollUp => app.picker_move(-3),
                MouseEventKind::ScrollDown => app.picker_move(3),
                _ => {}
            }
        }
        return Ok(());
    }
    if app.form.is_some() {
        if areas.list.contains(point) {
            match mouse.kind {
                MouseEventKind::ScrollUp => app.form_move(-1),
                MouseEventKind::ScrollDown => app.form_move(1),
                _ => {}
            }
        }
        return Ok(());
    }
    if app.is_editing_dir() {
        return Ok(());
    }

    match mouse.kind {
        MouseEventKind::ScrollUp if areas.list.contains(point) => app.move_selection(-3),
        MouseEventKind::ScrollDown if areas.list.contains(point) => app.move_selection(3),
        MouseEventKind::Down(MouseButton::Left) => {
            if areas.domains.contains(point) {
                if let Some(index) = domain_at(areas.domains, mouse.column, app) {
                    app.switch_domain(index);
                }
                return Ok(());
            }

            if areas.list.contains(point)
                && let Some(row) = main_tool_row(areas.list, point)
                && row < app.filtered.len()
            {
                app.selected = row;
                app.table.select(Some(row));
                if is_double_click(mouse) {
                    execute_or_open_form(app, cwd)?;
                }
            }
        }
        _ => {}
    }

    Ok(())
}

fn main_tool_row(list: Rect, point: Position) -> Option<usize> {
    let inner = Rect::new(
        list.x.saturating_add(1),
        list.y.saturating_add(1),
        list.width.saturating_sub(2),
        list.height.saturating_sub(2),
    );
    (inner.contains(point) && point.y > inner.y)
        .then(|| point.y.saturating_sub(inner.y).saturating_sub(1) as usize)
}

fn package_result_row(results: Rect, point: Position) -> Option<usize> {
    results
        .contains(point)
        .then(|| point.y.saturating_sub(results.y) as usize)
}

fn package_queue_row(results: Rect, point: Position) -> Option<usize> {
    let inner = Rect::new(
        results.x.saturating_add(1),
        results.y.saturating_add(1),
        results.width.saturating_sub(2),
        results.height.saturating_sub(2),
    );
    inner
        .contains(point)
        .then(|| point.y.saturating_sub(inner.y) as usize)
}

/// 从当前鼠标事件中获得一个稳定的双击判定。
///
/// Crossterm 本身不会替我们把两个终端 MouseEvent 组合成双击，所以用一个很小的
/// 时间/位置窗口完成。状态放在 App 之外会导致跨实例串扰，因此只使用进程级轻量状态。
fn is_double_click(mouse: MouseEvent) -> bool {
    use std::sync::OnceLock;
    use std::time::{Duration, Instant};
    static LAST: OnceLock<std::sync::Mutex<Option<(Instant, u16, u16)>>> = OnceLock::new();

    let last = LAST.get_or_init(|| std::sync::Mutex::new(None));
    let Ok(mut state) = last.lock() else {
        return false;
    };
    let doubled = state.is_some_and(|(time, x, y)| {
        time.elapsed() < Duration::from_millis(450)
            && x.abs_diff(mouse.column) <= 2
            && y.abs_diff(mouse.row) <= 1
    });
    *state = if doubled {
        None
    } else {
        Some((Instant::now(), mouse.column, mouse.row))
    };
    doubled
}

fn domain_at(area: Rect, x: u16, app: &App) -> Option<usize> {
    crate::ui::domain_tabs(app, area)
        .into_iter()
        .find(|tab| tab.hit.contains(Position::new(x, area.y)))
        .map(|tab| tab.index)
}

fn package_panel_area(app: &App, screen: Rect) -> Rect {
    let layout = crate::ui::main_layout(app, screen);
    if layout.too_small {
        return Rect::default();
    }
    layout.list.union(layout.detail)
}

fn handle_packages_mouse(
    app: &mut App,
    mouse: MouseEvent,
    _cwd: &Path,
) -> Result<(), Box<dyn std::error::Error>> {
    if app.packages.is_none() {
        return Ok(());
    }

    // 浮层开着的时候鼠标不参与：确认面板上「点一下」的语义太危险
    // （你以为在选，它可能在执行），统一留给键盘。
    if app
        .packages
        .as_ref()
        .is_some_and(|view| view.confirm.is_some() || view.sort_menu.is_some())
    {
        return Ok(());
    }

    if matches!(mouse.kind, MouseEventKind::ScrollUp) {
        if let Some(view) = app.packages.as_mut() {
            view.move_selection(-3);
        }
        return Ok(());
    }
    if matches!(mouse.kind, MouseEventKind::ScrollDown) {
        if let Some(view) = app.packages.as_mut() {
            view.move_selection(3);
        }
        return Ok(());
    }
    if !matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left)) {
        return Ok(());
    }

    // 软件包中心自己拥有一套布局：外层面板与渲染保持一致，内层由
    // ui::packages::layout() 统一提供状态栏 / 标签栏 / 结果 / 搜索 / 队列 / 信息区，
    // 标签的命中区也来自同一个 tab_hits()。
    let (width, height) = size().unwrap_or((80, 24));
    let screen = Rect::new(0, 0, width, height);
    let area = package_panel_area(app, screen);
    if area.width < 2 || area.height < 2 {
        return Ok(());
    }
    let inner = Rect::new(
        area.x.saturating_add(1),
        area.y.saturating_add(1),
        area.width.saturating_sub(2),
        area.height.saturating_sub(2),
    );
    let areas = crate::ui::packages::layout(inner);
    let point = Position::new(mouse.column, mouse.row);

    // 标签栏：模式标签与筛选标签共用一套命中区。
    if areas.tabs.contains(point) {
        let hits = app
            .packages
            .as_ref()
            .map(|view| crate::ui::packages::tab_hits(view, areas.tabs))
            .unwrap_or_default();
        for (rect, target) in hits {
            if !rect.contains(point) {
                continue;
            }
            match target {
                crate::ui::packages::TabTarget::Mode(index) => {
                    if let Some(mode) = PackageMode::ALL.get(index).copied()
                        && let Some(view) = app.packages.as_mut()
                    {
                        view.set_mode(mode);
                    }
                }
                crate::ui::packages::TabTarget::Filter(index) => {
                    if let Some(view) = app.packages.as_mut() {
                        view.toggle_chip(index);
                    }
                }
            }
            return Ok(());
        }
        return Ok(());
    }

    // 状态行：点左半边是「开始打字过滤」，点右半边是排序菜单。
    // （旧版式里这儿是单独一行搜索框，现在照 paru 把它并进状态行了。）
    if areas.status.contains(point) {
        let sorts_open = app
            .packages
            .as_ref()
            .is_some_and(|view| view.sort_menu.is_some());
        if sorts_open {
            return Ok(());
        }
        let wide_enough = point.x < areas.status.x + areas.status.width.saturating_sub(40);
        if let Some(view) = app.packages.as_mut() {
            if wide_enough {
                view.pane = Pane::Rows;
                view.message = String::from("按 / 过滤（打字即筛）· Enter 上网搜");
            } else {
                view.open_sort_menu();
            }
        }
        return Ok(());
    }

    if areas.tabs.contains(point) {
        // 上面已经处理过（标签命中区），这里只是兜底：点在标签行的空白处
        return Ok(());
    }

    if areas.info.contains(point) {
        if let Some(view) = app.packages.as_mut() {
            view.pane = Pane::Info;
        }
        return Ok(());
    }

    let double = is_double_click(mouse);

    // 队列在 `Tab` 切过去时整块占着结果表那块地方
    if app
        .packages
        .as_ref()
        .is_some_and(|view| view.pane == Pane::Queue)
    {
        if let Some(row) = package_queue_row(areas.results, point)
            && let Some(view) = app.packages.as_mut()
        {
            if row < view.queue.len() {
                view.queue_selected = row;
                if double {
                    view.toggle_queue();
                }
            } else if double {
                view.toggle_queue();
            }
        }
        return Ok(());
    }

    let Some(row) = package_result_row(areas.results, point) else {
        return Ok(());
    };

    // 搜索、已安装、新闻、维护列表都没有表头；区域第一行就是第一个结果。
    if let Some(view) = app.packages.as_mut() {
        view.pane = Pane::Rows;
        if row < view.rows_len() {
            let delta = row as isize - view.rows_selected() as isize;
            view.move_row(delta);
            if double {
                view.toggle_queue();
            }
        }
    }
    Ok(())
}

/// 处理一次按键。返回 `Ok(true)` 表示请求退出主循环。
pub fn handle_key(
    app: &mut App,
    key: KeyEvent,
    cwd: &Path,
) -> Result<bool, Box<dyn std::error::Error>> {
    if key.code == KeyCode::F(1) {
        if app.is_help_open() {
            app.close_help();
        } else {
            app.open_help();
        }
        return Ok(false);
    }

    // 帮助是盖在所有模式上的全局面板，优先于包管理和其它专属输入处理。
    if app.is_help_open() {
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('?') => app.close_help(),
            KeyCode::Up | KeyCode::Char('k') => app.scroll_help(-1),
            KeyCode::Down | KeyCode::Char('j') => app.scroll_help(1),
            KeyCode::PageUp => app.scroll_help(-10),
            KeyCode::PageDown | KeyCode::Char(' ') => app.scroll_help(10),
            KeyCode::Home | KeyCode::Char('g') => app.help = Some(0),
            _ => {}
        }
        return Ok(false);
    }

    // 工具仓库界面开着时它吃掉所有按键（它自己有三个面板）。
    if app.repository.is_some() {
        let open = match app.repository.as_mut() {
            Some(view) => view.handle_key(key)?,
            None => true,
        };
        if !open {
            app.close_repository();
            app.message = String::from("已离开工具仓库");
        }
        return Ok(false);
    }

    // 原生包管理视图开着时它吃掉所有按键（它有搜索框和队列两种焦点）。
    if app.packages.is_some() {
        return handle_packages_key(app, key, cwd);
    }

    // 文件视图开着时它吃掉所有按键（字母进过滤词，和选择器一致）。
    if app.files.is_some() {
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => app.close_files(),
            KeyCode::Up => app.files_move(-1),
            KeyCode::Down => app.files_move(1),
            KeyCode::PageUp => app.files_move(-10),
            KeyCode::PageDown => app.files_move(10),
            KeyCode::Home => {
                if let Some(view) = app.files.as_mut() {
                    view.selected = 0;
                }
            }
            KeyCode::End => {
                let last = app
                    .files
                    .as_ref()
                    .map_or(0, |view| view.len().saturating_sub(1));
                if let Some(view) = app.files.as_mut() {
                    view.selected = last;
                }
            }
            KeyCode::Enter => app.files_enter(),
            KeyCode::Backspace => app.files_backspace(),
            KeyCode::Char(ch) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                app.files_push(ch);
            }
            _ => {}
        }
        return Ok(false);
    }

    // 有任务在跑的时候，q/Esc/Ctrl-C 是「取消」而不是退出
    // （但如果在看输出视图，q 还是关视图 —— 那更符合直觉）。
    if app.is_running()
        && app.viewer.is_none()
        && app.files.is_none()
        && app.packages.is_none()
        && !app.searching
        && app.history.is_none()
        && app.picker.is_none()
        && app.form.is_none()
        && !app.is_editing_dir()
        && (matches!(key.code, KeyCode::Char('q') | KeyCode::Esc)
            || (matches!(key.code, KeyCode::Char('c')) && key.modifiers == KeyModifiers::CONTROL))
    {
        app.cancel_job();
        return Ok(false);
    }

    if app.searching {
        handle_search_key(app, key);
        return Ok(false);
    }

    if app.viewer.is_some() {
        handle_viewer_key(app, key);
        return Ok(false);
    }

    if app.history.is_some() {
        return handle_history_key(app, key, cwd);
    }

    // 文件选择器：字母全都进过滤词，导航只用方向键。
    if app.picker.is_some() {
        handle_picker_key(app, key);
        return Ok(false);
    }

    // 「改工作目录」输入态：所有按键都进这个文本框。
    if app.is_editing_dir() {
        match key.code {
            KeyCode::Enter => app.commit_dir_input(),
            KeyCode::Esc => app.cancel_dir_input(),
            KeyCode::Backspace => app.dir_input_backspace(),
            KeyCode::Char('u') if key.modifiers == KeyModifiers::CONTROL => app.dir_input_clear(),
            KeyCode::Char(ch) => app.dir_input_push(ch),
            _ => {}
        }
        return Ok(false);
    }

    if app.form.is_some() {
        return handle_form_key(app, key, cwd);
    }

    // 带 Ctrl 的组合键**不能**误触单键功能：实测按 Ctrl-F 会变成「收藏」，
    // 状态文件里就会多出一条没人按过的收藏。所以下面所有单键绑定都加 !ctrl。
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);

    match key.code {
        KeyCode::Char('q') | KeyCode::Esc if !ctrl => return Ok(true),

        KeyCode::Char('/') if !ctrl => {
            app.searching = true;
            app.message = String::from("搜索工具…（Enter 确认 · Esc 清除）");
        }

        KeyCode::Up | KeyCode::Char('k') if !ctrl => app.move_selection(-1),
        KeyCode::Down | KeyCode::Char('j') if !ctrl => app.move_selection(1),
        KeyCode::Char('g') if !ctrl => app.select_first(),
        KeyCode::Char('G') if !ctrl => app.select_last(),
        KeyCode::Home => app.select_first(),
        KeyCode::End => app.select_last(),
        KeyCode::Char('u') if ctrl => app.move_selection(-8),
        KeyCode::Char('d') if ctrl => app.move_selection(8),
        KeyCode::PageUp => app.move_selection(-8),
        KeyCode::PageDown => app.move_selection(8),

        KeyCode::Tab | KeyCode::Char(' ') => app.toggle_mark(),

        KeyCode::Char('r') if ctrl => app.reload(),

        // 大写 H：看执行历史。
        KeyCode::Char('H') if !ctrl => app.open_history(),

        // ?：这份帮助（6 个模式、30 多个键，得有个能查的地方）。
        KeyCode::Char('?') if !ctrl => app.open_help(),

        // F：看看工作目录里有哪些媒体文件（与脚本用的是同一套规则）。
        KeyCode::Char('F') if !ctrl => app.open_files(),

        // y：把终端交给文件管理器（yazi），回来工作目录跟着走 ——
        // 「先逛目录、再用脚本」就靠它。
        KeyCode::Char('y') if !ctrl => app.browse_with_file_manager()?,

        // p：原生包管理（搜索官方源 + AUR、看信息、排队、装）。
        KeyCode::Char('p') if !ctrl => app.open_packages(),

        // d：改工作目录 —— 扫目录的脚本就是在这里找输入文件的。
        KeyCode::Char('d') if !ctrl => app.open_dir_input(),

        // f：收藏 / 取消收藏；v：切换视图（全部 → ★ 收藏 → 最近使用）。
        KeyCode::Char('f') if !ctrl => app.toggle_favorite_current(),
        KeyCode::Char('v') if !ctrl => app.cycle_scope(),

        // 域：左右循环切换。
        KeyCode::Left => app.switch_domain(app.domain + Domain::ALL.len() - 1),
        KeyCode::Right => app.switch_domain(app.domain + 1),

        // 二级筛选：h/l 与 [/] 等价。
        KeyCode::Char('h') | KeyCode::Char('[') if !ctrl => app.move_sub(-1),
        KeyCode::Char('l') | KeyCode::Char(']') if !ctrl => app.move_sub(1),

        // 数字键直达某个域。
        KeyCode::Char(digit @ '1'..='9') if !ctrl => {
            if let Some(domain) = Domain::from_digit(digit) {
                app.switch_domain(domain.index());
            }
        }

        KeyCode::Enter => execute_or_open_form(app, cwd)?,

        _ => {}
    }

    // 除了 Enter，按任何键都算「取消危险操作」。
    if !matches!(key.code, KeyCode::Enter) {
        app.clear_pending();
    }
    Ok(false)
}

/// 输出视图的按键：只看和滚，不执行、不改筛选。
fn handle_viewer_key(app: &mut App, key: KeyEvent) {
    match key.code {
        KeyCode::Esc | KeyCode::Char('q') => app.close_viewer(),
        KeyCode::Left => app.viewer_scroll_horizontal(-8),
        KeyCode::Right => app.viewer_scroll_horizontal(8),
        KeyCode::Up | KeyCode::Char('k') => app.viewer_scroll(-1),
        KeyCode::Down | KeyCode::Char('j') => app.viewer_scroll(1),
        KeyCode::PageUp | KeyCode::Char('b') => app.viewer_scroll(-15),
        KeyCode::PageDown | KeyCode::Char(' ') => app.viewer_scroll(15),
        KeyCode::Home | KeyCode::Char('g') => app.viewer_to_top(),
        KeyCode::End | KeyCode::Char('G') => app.viewer_to_bottom(),
        KeyCode::Char('s') => app.viewer_save(),
        KeyCode::Char('c') => app.viewer_copy(),
        _ => {}
    }
}

/// 文件选择器的按键。
///
/// 字母**全部**进过滤词，导航一律用方向键 —— 和 fzf 一致。
/// 试过让 j/k 也能上下移动，代价是没法筛 `j` 开头的文件，不划算。
fn handle_picker_key(app: &mut App, key: KeyEvent) {
    match key.code {
        KeyCode::Esc => app.close_picker(),
        KeyCode::Up => app.picker_move(-1),
        KeyCode::Down => app.picker_move(1),
        KeyCode::PageUp => app.picker_move(-10),
        KeyCode::PageDown => app.picker_move(10),
        KeyCode::Home => app.picker_select_first(),
        KeyCode::End => app.picker_select_last(),
        KeyCode::Left => app.picker_parent(),
        KeyCode::Tab => app.picker_toggle_mark(),
        KeyCode::Char('d') if key.modifiers == KeyModifiers::CONTROL => {
            app.picker_use_current_dir();
        }
        KeyCode::Backspace => app.picker_backspace(),
        KeyCode::Enter => app.picker_enter(),
        KeyCode::Char('u') if key.modifiers == KeyModifiers::CONTROL => app.picker_clear_field(),
        KeyCode::Char(ch) if !key.modifiers.contains(KeyModifiers::CONTROL) => app.picker_push(ch),
        _ => {}
    }
}

/// 执行历史模式下的按键。
fn handle_history_key(
    app: &mut App,
    key: KeyEvent,
    cwd: &Path,
) -> Result<bool, Box<dyn std::error::Error>> {
    match key.code {
        KeyCode::Esc | KeyCode::Char('q') => app.close_history(),
        KeyCode::Up | KeyCode::Char('k') => app.history_move(-1),
        KeyCode::Down | KeyCode::Char('j') => app.history_move(1),
        KeyCode::PageUp => app.history_move(-10),
        KeyCode::PageDown => app.history_move(10),
        KeyCode::Enter => replay_history(app, cwd)?,
        // e：不是原样重跑，而是把那次参数**填回表单**再改
        KeyCode::Char('e') => app.refill_from_history(),
        _ => {}
    }
    Ok(false)
}

/// 重跑历史里的那条命令。
///
/// 参数**原样复用记录里的 argv**（不经 shell，也不用再填一遍表单）。
///
/// 这件事**不在这里同步做**：历史里的 argv 可能是转码那种长任务，阻塞执行会把
/// 整个界面冻住。排进后台队列，实时输出/取消/收尾都由 `App::replay_from_history`
/// 那条路管。
fn replay_history(app: &mut App, cwd: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let Some(entry) = app.history_selected() else {
        return Ok(());
    };

    if app.replay_from_history(&entry, cwd) {
        // 关掉历史浮层，让「执行中」面板露出来 —— 不然实时输出画在浮层后面，
        // 看起来就像什么都没发生。
        app.close_history();
    }
    Ok(())
}

/// 回车：需要填参数的工具先进表单，其余直接执行。
fn execute_or_open_form(app: &mut App, cwd: &Path) -> Result<(), Box<dyn std::error::Error>> {
    if app.open_form() {
        return Ok(());
    }

    // 危险动作：第一次回车只是问一声，第二次才真跑。
    if app.take_pending_confirmation() {
        return execute(app, cwd);
    }
    if app.targets_need_confirm() {
        app.arm_pending_confirmation();
        return Ok(());
    }

    execute(app, cwd)
}

/// 参数表单模式下的按键。
///
/// 输入态只有 `Enter` / `Esc` / `Backspace` / 普通字符会进文本框，其余按键忽略 ——
/// 否则在输入路径时按 `q` 就会退出程序。
fn handle_form_key(
    app: &mut App,
    key: KeyEvent,
    cwd: &Path,
) -> Result<bool, Box<dyn std::error::Error>> {
    if app.form_is_editing() {
        match key.code {
            KeyCode::Enter | KeyCode::Esc => app.form_end_edit(),
            KeyCode::Backspace => app.form_backspace(),
            // 带 Ctrl 的组合键不该当成普通字符打进去（否则 Ctrl-F 会插入一个 f）。
            KeyCode::Char(ch) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                app.form_push_char(ch);
            }
            _ => {}
        }
        app.form_disarm();
        return Ok(false);
    }

    match key.code {
        KeyCode::Esc | KeyCode::Char('q') => app.close_form(),

        KeyCode::Up | KeyCode::Char('k') => app.form_move(-1),
        KeyCode::Down | KeyCode::Char('j') => app.form_move(1),

        // 左右 / 空格：Choice 换选项、Toggle 翻转。
        KeyCode::Left | KeyCode::Char('h') => app.form_adjust(-1),
        KeyCode::Right | KeyCode::Char('l') | KeyCode::Char(' ') => app.form_adjust(1),

        // 回车：文本 / 路径进输入态，Choice / Toggle 直接改值。
        KeyCode::Enter => app.form_activate(),

        // Ctrl-F：挑文件（编辑态下也认，省得先按 Enter 结束编辑）。
        KeyCode::Char('f') if key.modifiers == KeyModifiers::CONTROL => {
            app.form_end_edit();
            app.open_picker();
            return Ok(false);
        }

        KeyCode::Char('e') if key.modifiers == KeyModifiers::CONTROL => {
            execute_form(app, cwd)?;
            return Ok(false);
        }

        _ => {}
    }

    // 改过任何东西（换字段、改选项）就撤销上回的确认。
    app.form_disarm();
    Ok(false)
}

/// 从表单发起执行：先把 argv 构建出来，缺必填项就只报错、不执行。
fn execute_form(app: &mut App, cwd: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let argv = match app.form_build() {
        Ok(argv) => argv,
        Err(message) => {
            app.form_set_error(message);
            return Ok(());
        }
    };

    // 危险动作：再按一次 Ctrl-E 才真跑。
    if app.form_needs_confirm() {
        app.form_arm_confirm();
        return Ok(());
    }

    let Some(tool) = app.form_tool().cloned() else {
        return Ok(());
    };

    // 记历史用「脱敏后的 argv」：sensitive 参数的值在这里就被换掉。
    let redacted = match (tool.action.as_ref(), app.form_values()) {
        (Some(action), Some(values)) => action.redacted(values, &argv),
        _ => argv.clone(),
    };
    let started = Instant::now();

    match tool.mode {
        // 内置界面：不起进程、也不要参数，直接开对应视图
        RunMode::Native => {
            let view = tool
                .action
                .as_ref()
                .map(|action| action.program.clone())
                .unwrap_or_default();
            open_native_view(app, &view);
            app.form = None;
            return Ok(());
        }
        RunMode::Capture => {
            // 展开成一次或多次执行（`foreach` 声明了就是「每个输入各跑一次」）。
            // 注意：这一步必须在**关表单之前**做 —— 关了就取不到表单里的值了。
            let runs = match app.form_build_runs() {
                Ok(runs) => runs,
                Err(message) => {
                    app.form_set_error(message);
                    return Ok(());
                }
            };

            let count = runs.len();
            let requests: Vec<crate::app::CaptureRequest> = runs
                .into_iter()
                .map(|(argv, values)| {
                    // 单文件可同步探测源时长；foreach 可能有很多文件，不能在 UI
                    // 线程里逐个等待 ffprobe。批量时仍保留用户显式填写的裁剪时长。
                    let total_seconds = tool.action.as_ref().and_then(|action| {
                        if count > 1 {
                            action
                                .limit_from
                                .as_deref()
                                .and_then(|key| values.get(key))
                                .and_then(crate::runtime::parse_duration)
                        } else {
                            app.total_seconds_for(action, &values)
                        }
                    });
                    let record_argv = match tool.action.as_ref() {
                        Some(action) => action.redacted(&values, &argv),
                        None => argv.clone(),
                    };
                    crate::app::CaptureRequest {
                        tool: tool.clone(),
                        ok_exit_codes: tool
                            .action
                            .as_ref()
                            .map(|action| action.ok_exit_codes.clone())
                            .unwrap_or_else(|| vec![0]),
                        values: values.pairs(),
                        argv,
                        record_argv,
                        total_seconds,
                    }
                })
                .collect();

            app.close_form();
            app.enqueue_captures(requests);
            if count > 1 {
                app.message = format!("{count} 个输入 → 各自一个输出，依次执行中…");
            }
        }
        RunMode::Interactive => {
            let report = runtime::execute_action(&tool, &argv, cwd)?;
            // 它接管过终端：回来后必须整屏重画（见 App::request_full_redraw）。
            app.request_full_redraw();
            app.record_run(
                &tool,
                &redacted,
                report.failed == 0,
                started.elapsed().as_millis(),
            );
            app.close_form();
            app.message = report.message();
        }
    }
    Ok(())
}

/// 搜索模式下的按键：只改查询串，不触发执行。
fn handle_search_key(app: &mut App, key: KeyEvent) {
    match key.code {
        KeyCode::Esc => {
            app.searching = false;
            app.query.clear();
            app.selected = 0;
            app.apply_filter();
            app.message = String::from("已清除搜索");
        }
        KeyCode::Enter => {
            app.searching = false;
            app.message = match app.filtered.len() {
                0 => String::from("没有匹配的工具"),
                hits => format!("搜索 · {hits} 个命中"),
            };
        }
        KeyCode::Backspace => {
            app.query.backspace();
            app.selected = 0;
            app.apply_filter();
        }
        KeyCode::Delete => {
            app.query.delete();
            app.selected = 0;
            app.apply_filter();
        }
        // 光标移动：以前只有 push/pop，打错开头只能全删重打
        KeyCode::Left => app.query.left(),
        KeyCode::Right => app.query.right(),
        KeyCode::Home => app.query.home(),
        KeyCode::End => app.query.end(),
        KeyCode::Char(c) => {
            app.query.insert(c);
            app.selected = 0;
            app.apply_filter();
        }
        _ => {}
    }
}

/// 软件包中心的按键。
///
/// ## 模型：输入是**模态**的（`/` 进，`Enter` 收工，`Esc` 清空退出）
///
/// 以前是「按 i 才进输入态」，于是搜完一次之后 `editing` 被置回 false，
/// 用户再打字**毫无反应** —— 看起来像界面卡死（实拍反馈）。
/// 现在打字永远是过滤，命令一律走 `Ctrl`：
///
/// | 键 | 干什么 |
/// | --- | --- |
/// | 打字 | 过滤（本地全库，即时） |
/// | `↑↓` `PgUp/PgDn` | 选 |
/// | `Space` | 多选：加入/移出队列 |
/// | `Enter` | 摆出要跑的命令（再按一次才执行） |
/// | `Tab` | 结果 → 队列 → 包信息 |
/// | `[` `]` | 切模式 |
/// | `Esc` | 有筛选词就清空，空着就关界面 |
/// | `Ctrl+R` | 用当前词上网搜（官方源 + AUR） |
/// | `Ctrl+U` | 清空筛选 |
/// | `Ctrl+D` | 清空队列 |
/// | `Ctrl+S` `Ctrl+M` `Ctrl+T` `Ctrl+O` | 排序 / 队列操作 / 演练模式 / 清孤儿 |
/// | `Ctrl+X` `Ctrl+K` `Ctrl+N` `Ctrl+A` | PKGBUILD / 检查 / 新闻 / AUR 页面 |
/// | `Ctrl+E` `Ctrl+I` | 导出 / 导入队列 |
fn handle_packages_key(
    app: &mut App,
    key: KeyEvent,
    cwd: &Path,
) -> Result<bool, Box<dyn std::error::Error>> {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);
    let shift = key.modifiers.contains(KeyModifiers::SHIFT);
    let mode = app
        .packages
        .as_ref()
        .map(|view| view.mode)
        .unwrap_or(PackageMode::Search);

    // ── 第一优先：确认面板 ──
    if let Some(confirm) = app.packages.as_ref().and_then(|view| view.confirm.clone()) {
        match key.code {
            KeyCode::Enter => app.run_confirm(cwd)?,
            KeyCode::Esc | KeyCode::Char('q') => {
                if let Some(view) = app.packages.as_mut() {
                    view.cancel_confirm();
                }
            }
            // 清缓存保留几个版本，当场就能调（别的动作没有可调项）
            KeyCode::Char('[') | KeyCode::Char(']') => {
                if let ConfirmAction::Cache(keep) = confirm.action {
                    let next = if key.code == KeyCode::Char('[') {
                        keep.saturating_sub(1).max(1)
                    } else {
                        (keep + 1).min(9)
                    };
                    if let Some(view) = app.packages.as_mut() {
                        view.cache_keep = next;
                    }
                    app.arm_cache();
                }
            }
            _ => {}
        }
        return Ok(false);
    }

    // ── 第二优先：排序菜单 ──
    if app
        .packages
        .as_ref()
        .is_some_and(|view| view.sort_menu.is_some())
    {
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => {
                if let Some(view) = app.packages.as_mut() {
                    view.sort_menu_step(-1);
                }
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if let Some(view) = app.packages.as_mut() {
                    view.sort_menu_step(1);
                }
            }
            KeyCode::Enter => {
                if let Some(view) = app.packages.as_mut() {
                    view.apply_sort_menu();
                }
            }
            _ => {
                if let Some(view) = app.packages.as_mut() {
                    view.close_sort_menu();
                }
            }
        }
        return Ok(false);
    }

    // ── 搜索历史（Alt+↑↓）：`↑↓` 归列表，所以历史留在 Alt 上 ──
    if alt {
        match key.code {
            KeyCode::Up => {
                if let Some(view) = app.packages.as_mut() {
                    view.history_step(-1);
                    view.refilter();
                }
                return Ok(false);
            }
            KeyCode::Down => {
                if let Some(view) = app.packages.as_mut() {
                    view.history_step(1);
                    view.refilter();
                }
                return Ok(false);
            }
            _ => {}
        }
    }

    // ── 输入态：只有编辑键，其余一律不抢 ──
    //
    // 输入改模态是这一版键位的**前提**。以前输入框常驻，字母和数字都得让给过滤框，
    // 命令只能全挤在 `Ctrl+` 上（`Ctrl+S/M/T/O/X/K/N/A/E/I/W`）—— 那不是设计出来的
    // 键位，是被输入框挤出来的。现在 `/` 进来、`Enter` 收工、`Esc` 清空并退出。
    if app.packages.as_ref().is_some_and(|view| view.typing) {
        match key.code {
            // `Enter` 收工但**保留**筛选词；`Esc` 清空并退出 —— voicefox 的 `/` 也这样
            KeyCode::Enter => {
                if let Some(view) = app.packages.as_mut() {
                    view.typing = false;
                    view.message = if view.query.is_empty() {
                        String::from("没填过滤词 · `/` 再进输入")
                    } else {
                        format!("筛选「{}」· `/` 再改", view.query.text())
                    };
                }
            }
            KeyCode::Esc => {
                if let Some(view) = app.packages.as_mut() {
                    view.clear_query();
                    view.typing = false;
                    view.message = String::from("已清空筛选");
                }
            }
            KeyCode::Backspace => {
                if let Some(view) = app.packages.as_mut() {
                    view.query.backspace();
                    view.refilter();
                }
            }
            KeyCode::Delete => {
                if let Some(view) = app.packages.as_mut() {
                    view.query.delete();
                    view.refilter();
                }
            }
            KeyCode::Left => {
                if let Some(view) = app.packages.as_mut() {
                    view.query.left();
                }
            }
            KeyCode::Right => {
                if let Some(view) = app.packages.as_mut() {
                    view.query.right();
                }
            }
            KeyCode::Home => {
                if let Some(view) = app.packages.as_mut() {
                    view.query.home();
                }
            }
            KeyCode::End => {
                if let Some(view) = app.packages.as_mut() {
                    view.query.end();
                }
            }
            // 输入态里也认 Ctrl-U：清空这个词（Ctrl 组合键不会插字符）
            KeyCode::Char('u') if ctrl => {
                if let Some(view) = app.packages.as_mut() {
                    view.clear_query();
                }
            }
            KeyCode::Char(ch) if !ctrl => {
                if let Some(view) = app.packages.as_mut() {
                    view.query.insert(ch);
                    view.refilter();
                }
            }
            _ => {}
        }
        return Ok(false);
    }

    // ── Esc：非输入态就是「回上层」（动作列表）──
    if key.code == KeyCode::Esc {
        app.close_packages();
        return Ok(false);
    }

    // ── Ctrl 命令：只留没有对应字母键的那些 ──
    if ctrl {
        match key.code {
            KeyCode::Char('q') | KeyCode::Char('c') => app.close_packages(),
            // 强制重搜（字母键里没有「重新上网找一遍」这个动作）
            KeyCode::Char('r') => {
                if let Some(view) = app.packages.as_mut() {
                    view.start_search();
                }
            }
            // 结果区里是翻页；别的面板上是清空队列
            KeyCode::Char('d') => {
                if let Some(view) = app.packages.as_mut() {
                    if view.pane == Pane::Rows {
                        view.move_selection(10);
                    } else {
                        view.clear_queue();
                    }
                }
            }
            KeyCode::Char('u') => {
                if let Some(view) = app.packages.as_mut() {
                    view.move_selection(-10);
                }
            }
            // 清单的导出 / 导入（字母键不够用了，留在 Ctrl 上）
            KeyCode::Char('e') => {
                let path = crate::app::package_view::default_queue_path();
                if let Some(view) = app.packages.as_mut() {
                    view.export_queue(&path);
                }
            }
            KeyCode::Char('i') => {
                let path = crate::app::package_view::default_queue_path();
                if let Some(view) = app.packages.as_mut() {
                    view.import_queue(&path);
                }
            }
            _ => {}
        }
        return Ok(false);
    }

    // ── 其余的键：非输入态下，字母就是命令 ──
    match key.code {
        // `/`：进过滤（唯一入口）
        KeyCode::Char('/') => {
            if let Some(view) = app.packages.as_mut() {
                view.typing = true;
                view.message = String::from("过滤：打字即筛 · Enter 收工 · Esc 清空");
            }
        }

        // `1`-`4`：直达四种模式（标签上就写着数字）
        KeyCode::Char(digit @ '1'..='4') => {
            let index = digit as usize - '1' as usize;
            if let Some(mode) = PackageMode::ALL.get(index).copied()
                && let Some(view) = app.packages.as_mut()
            {
                view.set_mode(mode);
            }
        }

        // 焦点：结果 → 队列 → 包信息 → 标签
        KeyCode::Tab => {
            if let Some(view) = app.packages.as_mut() {
                if shift {
                    view.toggle_focus_back();
                } else {
                    view.toggle_focus();
                }
            }
        }

        // 标签行：`[` `]` 与 `←→`（`h` `l`）都走这里 —— 和主界面「h/l 切分类」一个手感
        KeyCode::Char('[') | KeyCode::Left | KeyCode::Char('h') => {
            if let Some(view) = app.packages.as_mut() {
                view.move_chip_focus(-1);
            }
        }
        KeyCode::Char(']') | KeyCode::Right | KeyCode::Char('l') => {
            if let Some(view) = app.packages.as_mut() {
                view.move_chip_focus(1);
            }
        }

        // 上下移动（焦点在哪块就动哪块）
        KeyCode::Up | KeyCode::Char('k') => {
            if let Some(view) = app.packages.as_mut() {
                view.move_selection(-1);
            }
        }
        KeyCode::Down | KeyCode::Char('j') => {
            if let Some(view) = app.packages.as_mut() {
                view.move_selection(1);
            }
        }
        KeyCode::PageUp => {
            if let Some(view) = app.packages.as_mut() {
                view.move_selection(-10);
            }
        }
        KeyCode::PageDown => {
            if let Some(view) = app.packages.as_mut() {
                view.move_selection(10);
            }
        }
        KeyCode::Home | KeyCode::Char('g') => {
            if let Some(view) = app.packages.as_mut() {
                view.select_first();
            }
        }
        KeyCode::End | KeyCode::Char('G') => {
            if let Some(view) = app.packages.as_mut() {
                view.select_last();
            }
        }

        // `Space`：结果里排队 / 再按取消；标签行开关标签；队列里移出
        KeyCode::Char(' ') => {
            if let Some(view) = app.packages.as_mut() {
                match view.pane {
                    Pane::Tabs => view.toggle_focused_chip(),
                    Pane::Queue => view.remove_from_queue(),
                    _ => view.toggle_queue(),
                }
            }
        }
        KeyCode::Delete => {
            if let Some(view) = app.packages.as_mut() {
                view.remove_from_queue();
            }
        }

        // ── 动作：字母键 ──
        // `i` 装 / `r` 卸：都先把要跑的命令摆出来，再按一次才动系统
        KeyCode::Char('i') => {
            if let Some(view) = app.packages.as_mut() {
                view.operation = crate::packages::PackageOperation::Install;
            }
            app.arm_queue();
        }
        KeyCode::Char('r') => {
            if let Some(view) = app.packages.as_mut() {
                view.operation = crate::packages::PackageOperation::Remove;
            }
            app.arm_queue();
        }
        KeyCode::Char('u') => app.arm_upgrade(),
        KeyCode::Char('c') => app.arm_cache(),
        KeyCode::Char('o') => app.arm_orphans(),
        KeyCode::Char('D') => {
            if let Some(view) = app.packages.as_mut() {
                view.toggle_dry_run();
            }
        }
        KeyCode::Char('s') => {
            if let Some(view) = app.packages.as_mut() {
                view.open_sort_menu();
            }
        }
        // AUR 那三件事：检查（namcap + shellcheck）、看 PKGBUILD、开 AUR 页面
        KeyCode::Char('K') => app.check_pkgbuild(cwd)?,
        KeyCode::Char('X') => app.show_pkgbuild(cwd)?,
        KeyCode::Char('A') => app.open_aur_page(),
        // 新闻的已读标记
        KeyCode::Char('m') => {
            if let Some(view) = app.packages.as_mut() {
                view.toggle_news_read();
            }
        }
        KeyCode::Char('M') => {
            if let Some(view) = app.packages.as_mut() {
                view.mark_visible_news_read();
            }
        }
        KeyCode::Char('?') => app.open_help(),
        KeyCode::Char('q') => app.close_packages(),

        // Enter：上网搜 / 摆出要跑的命令 / 处理维护项
        KeyCode::Enter => match mode {
            PackageMode::Search => {
                let has_results = app
                    .packages
                    .as_ref()
                    .is_some_and(|view| !view.visible.is_empty());
                if has_results {
                    app.arm_queue();
                } else if let Some(view) = app.packages.as_mut() {
                    // 本地一个都没有 —— 这时候「搜」才是你要的
                    view.start_search();
                }
            }
            PackageMode::Installed => {
                let loaded = app
                    .packages
                    .as_ref()
                    .is_some_and(|view| view.installed_loaded);
                if loaded {
                    app.arm_queue();
                } else if let Some(view) = app.packages.as_mut() {
                    view.start_installed();
                }
            }
            PackageMode::News => {
                let has_news = app
                    .packages
                    .as_ref()
                    .is_some_and(|view| !view.news.is_empty());
                if has_news {
                    app.open_news_link();
                } else if let Some(view) = app.packages.as_mut() {
                    view.start_news();
                }
            }
            PackageMode::Health => app.run_health_action(cwd)?,
        },
        _ => {}
    }
    Ok(false)
}
/// 处理一次粘贴（终端开了 bracketed paste 才会有这个事件）。
///
/// 交给当前**正在输入**的那个框：软件包中心 > 全局搜索 > 文件选择器。
/// 用事件而不是一串按键，一来快，二来终端不会往里塞回车 ——
/// 粘一段多行文本不会顺手把命令发出去。
pub fn handle_paste(app: &mut App, text: &str) {
    if app.packages.is_some() {
        if let Some(view) = app.packages.as_mut() {
            // 粘进来的东西几乎一定是想过滤 —— 顺手进输入态，
            // 否则这些字会掉进命令里（输入改模态之后，字母键是命令）。
            view.typing = true;
            view.query.insert_str(text);
            view.refilter();
            view.message = String::from("过滤：打字即筛 · Enter 收工 · Esc 清空");
        }
        return;
    }
    if app.searching {
        app.query.insert_str(text);
        app.selected = 0;
        app.apply_filter();
        return;
    }
    if let Some(picker) = app.picker.as_mut() {
        picker.filter.insert_str(text);
        picker.refilter();
    }
}

/// 打开一个内置界面（`program` 就是界面名）。
fn open_native_view(app: &mut App, view: &str) {
    // `package-center[:模式]` —— 冒号后面是打开时停在哪个模式
    // （`package-center:installed` 就是「已安装 · 卸载」那个入口）。
    let (name, argument) = match view.split_once(':') {
        Some((name, argument)) => (name, Some(argument)),
        None => (view, None),
    };

    match name {
        "repository-center" => {
            app.open_repository();
            if let Some(mode) = argument.and_then(crate::app::repo_view::RepoMode::from_id)
                && let Some(view) = app.repository.as_mut()
            {
                view.set_mode(mode);
            }
        }
        "package-center" => {
            app.close_help();
            app.open_packages();
            if let Some(mode) = argument.and_then(PackageMode::from_id)
                && let Some(view) = app.packages.as_mut()
            {
                view.set_mode(mode);
            }
            app.message = String::from(
                "软件包中心：`/` 进过滤 · `1-4` 切面板 · Space 排队 · Enter 摆出要跑的命令",
            );
        }
        other => app.message = format!("不认识的界面「{other}」"),
    }
}

fn execute(app: &mut App, cwd: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let targets = app.execution_targets();
    if targets.is_empty() {
        app.message = String::from("没有可执行的工具");
        return Ok(());
    }

    // 内置界面（`RunMode::Native`）：不起进程、不要参数，直接开对应视图。
    //
    // 这一段以前**只在「表单提交」那条路上有**（`execute_form`），于是无参数的
    // native 动作在列表上按 Enter 什么都不发生 —— 它没有表单可开，掉进下面那个
    // `RunMode::Native => {}` 就没了。`软件包中心` 正是这种动作。
    let native_view = if targets.len() == 1 {
        app.registry
            .tools()
            .get(targets[0])
            .filter(|tool| tool.mode == RunMode::Native)
            .and_then(|tool| tool.action.as_ref())
            .map(|action| action.program.clone())
    } else {
        None
    };
    if let Some(view) = native_view {
        open_native_view(app, &view);
        app.clear_marks(&targets);
        return Ok(());
    }

    // 取出定义副本，之后才能自由改 App 状态。
    let mut capture = Vec::new();
    let mut interactive = Vec::new();
    let mut needs_form = 0usize;
    for &index in &targets {
        let Some(tool) = app.registry.tools().get(index) else {
            continue;
        };
        if tool.needs_form() {
            needs_form += 1;
            continue;
        }
        match tool.mode {
            RunMode::Capture => capture.push(tool.clone()),
            RunMode::Interactive => interactive.push(tool.clone()),
            // 单个 native 动作上面已经开完视图了；混在批量里没有意义，跳过。
            RunMode::Native => {}
        }
    }

    if capture.is_empty() && interactive.is_empty() {
        app.message = if needs_form > 0 {
            format!("{needs_form} 个工具需要先填参数：选中后按 Enter 打开表单")
        } else {
            String::from("没有可执行的工具")
        };
        return Ok(());
    }

    // 捕获的丢进后台队列（一件一件跑，跑完一起看输出）；接管的放最后，
    // 因为它要拿走终端。
    let mut parts = Vec::new();
    if !capture.is_empty() {
        let count = capture.len();
        app.enqueue_captures(
            capture
                .into_iter()
                .map(|tool| {
                    // 无参数的动作也必须把 `base_argv` 带上。`journalctl -p err -b`
                    // 这类动作的参数全在 base_argv 里，丢了就变成光跑 `journalctl`：
                    // 实测会 dump 整个日志；`systemctl` / `lsblk` / `lspci` 那几个
                    // 则是**静默**给出错误结果（列出全部单元、丢掉 -f / -k 的信息）。
                    // 和 interactive 路径共用同一个函数：两条路都得把 base_argv 带上
                    let argv = runtime::default_argv(&tool);
                    crate::app::CaptureRequest {
                        tool: tool.clone(),
                        ok_exit_codes: tool
                            .action
                            .as_ref()
                            .map(|action| action.ok_exit_codes.clone())
                            .unwrap_or_else(|| vec![0]),
                        // 列表路径没有表单取值，所以没有百分比。
                        total_seconds: None,
                        record_argv: argv.clone(),
                        values: Vec::new(),
                        argv,
                    }
                })
                .collect(),
        );
        parts.push(format!("{count} 个转入后台"));
    }
    if !interactive.is_empty() {
        let started = Instant::now();
        let report: runtime::ExecReport = runtime::execute_tools(&interactive, cwd)?;
        // 它接管过终端：回来后必须整屏重画（见 App::request_full_redraw）。
        app.request_full_redraw();
        let millis = started.elapsed().as_millis();
        // 批量执行只拿到整体结果，所以逐个工具记同一份结论（混合批次下这一点不够精确）。
        for tool in &interactive {
            app.record_run(tool, &[], report.failed == 0, millis);
        }
        parts.push(report.message());
    }
    if needs_form > 0 {
        parts.push(format!("{needs_form} 个需填参数已跳过"));
    }

    app.clear_marks(&targets);
    app.message = parts.join(" · ");
    Ok(())
}

/// 工具仓库界面的鼠标：滚轮滚动，左键整行选中。
fn handle_repository_mouse(
    app: &mut App,
    mouse: MouseEvent,
) -> Result<(), Box<dyn std::error::Error>> {
    let (width, height) = size().unwrap_or((80, 24));
    let screen = Rect::new(0, 0, width, height);
    let areas = crate::ui::main_layout(app, screen);
    if areas.too_small {
        return Ok(());
    }
    let content = areas.list.union(areas.detail);
    let point = Position::new(mouse.column, mouse.row);

    match mouse.kind {
        MouseEventKind::ScrollUp => {
            if let Some(view) = app.repository.as_mut() {
                view.handle_scroll(-3);
            }
        }
        MouseEventKind::ScrollDown => {
            if let Some(view) = app.repository.as_mut() {
                view.handle_scroll(3);
            }
        }
        MouseEventKind::Down(MouseButton::Left) => {
            let rows = crate::ui::repository::rows_rect(content);
            if rows.contains(point) {
                let visible = rows.height as usize;
                let row = (mouse.row.saturating_sub(rows.y)) as usize;
                if let Some(view) = app.repository.as_mut() {
                    view.click_viewport_row(row, visible);
                }
            }
        }
        _ => {}
    }
    Ok(())
}

#[cfg(test)]
mod mouse_row_tests {
    use ratatui::layout::{Position, Rect};

    use super::{main_tool_row, package_queue_row, package_result_row};

    #[test]
    fn main_table_mouse_mapping_skips_border_and_header() {
        let list = Rect::new(0, 10, 80, 10);

        assert_eq!(
            main_tool_row(list, Position::new(3, 10)),
            None,
            "边框不命中"
        );
        assert_eq!(
            main_tool_row(list, Position::new(3, 11)),
            None,
            "表头不命中"
        );
        assert_eq!(main_tool_row(list, Position::new(3, 12)), Some(0));
        assert_eq!(main_tool_row(list, Position::new(3, 13)), Some(1));
    }

    #[test]
    fn package_result_mouse_mapping_does_not_skip_the_first_row() {
        let results = Rect::new(5, 8, 60, 12);

        assert_eq!(package_result_row(results, Position::new(5, 8)), Some(0));
        assert_eq!(package_result_row(results, Position::new(5, 9)), Some(1));
        assert_eq!(package_result_row(results, Position::new(5, 20)), None);
    }

    #[test]
    fn package_queue_mouse_mapping_ignores_its_panel_border() {
        let results = Rect::new(5, 8, 60, 12);

        assert_eq!(package_queue_row(results, Position::new(5, 8)), None);
        assert_eq!(package_queue_row(results, Position::new(6, 9)), Some(0));
        assert_eq!(package_queue_row(results, Position::new(6, 10)), Some(1));
    }
}
