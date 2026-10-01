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

use std::{path::Path, path::PathBuf, time::Instant};

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::{
    app::{App, Viewer},
    model::{Domain, RunMode},
    runtime,
};

/// 处理一次按键。返回 `Ok(true)` 表示请求退出主循环。
pub fn handle_key(
    app: &mut App,
    key: KeyEvent,
    cwd: &Path,
) -> Result<bool, Box<dyn std::error::Error>> {
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

    // 帮助屏开着时它吃掉所有按键（它是盖在最上面的一层）。
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

    // 有任务在跑的时候，q/Esc/Ctrl-C 是「取消」而不是退出
    // （但如果在看输出视图，q 还是关视图 —— 那更符合直觉）。
    if app.is_running()
        && app.viewer.is_none()
        && app.files.is_none()
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
        _ => {}
    }
    Ok(false)
}

/// 重跑历史里的那条命令。
///
/// 参数**原样复用记录里的 argv**（不经 shell，也不用再填一遍表单）。
fn replay_history(app: &mut App, cwd: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let Some(entry) = app.history_selected() else {
        return Ok(());
    };
    let Some((program, argv)) = entry.argv.split_first() else {
        app.message = String::from("这条记录没有参数，重跑不了");
        return Ok(());
    };

    let started = Instant::now();
    let captured = runtime::run_captured(&PathBuf::from(program), argv, cwd, &entry.tool_name)?;
    app.message = format!("重跑 · {}", captured.summary());

    // 重跑也算一次执行，照样进历史（id / name 沿用原记录）。
    app.record_history(
        &entry.tool_id,
        &entry.tool_name,
        &entry.argv,
        captured.success,
        started.elapsed().as_millis(),
    );
    if let Some(viewer) = Viewer::from_captured(std::slice::from_ref(&captured)) {
        app.open_viewer(viewer);
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
        RunMode::Capture => {
            // 总时长要**在关表单之前**算：关了就取不到表单里的值了
            // （这个顺序错一次就会让进度条永远不出现）。
            let total_seconds = app.form_values().and_then(|values| {
                tool.action
                    .as_ref()
                    .and_then(|action| app.total_seconds_for(action, values))
            });

            // 走后台队列：界面不卡，能看实时输出与进度，也能取消。
            app.close_form();
            app.enqueue_captures(vec![crate::app::CaptureRequest {
                tool,
                argv,
                record_argv: redacted,
                total_seconds,
            }]);
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
            app.query.pop();
            app.selected = 0;
            app.apply_filter();
        }
        KeyCode::Char(c) => {
            app.query.push(c);
            app.selected = 0;
            app.apply_filter();
        }
        _ => {}
    }
}

/// 执行选中的工具：挂起 TUI、交给 runtime、回来后清标记。
///
/// 需要填参数的工具（`Curated` 那类）**不在这里执行** —— 它们得先打开表单，
/// 否则就是在替用户瞎猜参数。批量执行时把它们跳过并说明。
fn execute(app: &mut App, cwd: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let targets = app.execution_targets();
    if targets.is_empty() {
        app.message = String::from("没有可执行的工具");
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
        if tool.needs_arguments() {
            needs_form += 1;
            continue;
        }
        match tool.mode {
            RunMode::Capture => capture.push(tool.clone()),
            RunMode::Interactive => interactive.push(tool.clone()),
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
                .map(|tool| crate::app::CaptureRequest {
                    tool,
                    argv: Vec::new(),
                    record_argv: Vec::new(),
                    // 批量走列表路径，没有表单取值可探，所以没有百分比。
                    total_seconds: None,
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
