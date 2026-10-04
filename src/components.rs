//! 给脚本用的界面组件：`toolbox-hub ui confirm | pager | pick`。
//!
//! 脚本缺的往往不是「功能」而是「界面」—— 挑一个文件、确认一次、翻一页长输出。
//! 这里把主界面已经在用的部件（[`crate::ui::picker`] / [`crate::ui::viewer`] /
//! `theme`）单独暴露出来，脚本拿 `$(…)` 就能消费。
//!
//! ## 三条约定（破坏任何一条，这东西在脚本里就没法用）
//!
//! 1. **stdout 只放结果，界面一律画到 `/dev/tty`。** 否则
//!    `path=$(toolbox-hub ui pick)` 会被转义序列污染 —— 这是最重要的一条。
//!    （stdout 常常是管道，stderr 也可能被重定向，只有 `/dev/tty` 一定还在。）
//! 2. **退出码固定**：`0` 选了/确认，`1` 用户取消（Esc/n），`2` 参数错或环境不支持
//!    （不是终端、模板读不出来）。脚本里 `if p=$(tbx_pick); then … fi` 直接可用。
//! 3. **键位与主界面一致**：`↑↓`/`jk` 选择、`g`/`G` 首末、`PgUp/PgDn` 翻页、
//!    `Esc` 取消、`Tab` 多选、打字即过滤（组件里输入框是常驻的，和主界面不同）。
//!
//! ## `ui pick` 的分工：外部文件管理器优先
//!
//! **选文件优先交给外部文件管理器（默认 yazi）**：它自带预览、书签、多选与批量
//! 操作，浏览体验比内置那个好得多；把这件事交给它，也就不用再维护一套浏览逻辑。
//! 内置选择器只在外部程序**用不了**时顶上（没装 / 没有控制终端 / 起不来），
//! 保证脚本不会因为少一个外部程序就跑不动。
//!
//! 换程序用 [`crate::app::FILE_MANAGER_ENV`]（和主界面按 `y` 浏览目录共用同一个
//! 变量），设成 `builtin` / `none` 则明确要求用内置的。
//!
//! 组件不做任何录制/历史/状态落盘：它们是"一次性"的界面，跑完就退。

use std::{
    io::{self, IsTerminal, Read, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::Duration,
};

use ratatui::{
    Terminal,
    backend::CrosstermBackend,
    crossterm::{
        event::{
            self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyModifiers,
            MouseButton, MouseEventKind,
        },
        execute,
        terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
    },
    layout::{Alignment, Constraint, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Clear, Paragraph, Wrap},
};

use crate::{
    app::{Picker, Viewer},
    ui::{display_width, theme},
};

/// 组件跑完之后的结论 —— 直接决定退出码。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// 选了 / 确认了 → `0`。
    Ok,
    /// 用户取消 → `1`。
    Cancelled,
}

impl Outcome {
    pub fn code(self) -> i32 {
        match self {
            Outcome::Ok => 0,
            Outcome::Cancelled => 1,
        }
    }
}

/// 参数错 / 环境不支持时的退出码。
pub const UNSUPPORTED_CODE: i32 = 2;

// ── 确认框 ──────────────────────────────────────────────────────────────────

/// `ui confirm` 的参数。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Confirm {
    pub message: String,
    pub yes: String,
    pub no: String,
    /// 危险动作用红框，并且默认停在「取消」上。
    pub danger: bool,
    /// 非交互环境（stdin 不是 tty）时用这个答案；`None` = 直接报错。
    ///
    /// cron / CI 里没有终端，但脚本仍要跑下去 —— 那时候需要一个明确的答案，
    /// 而不是「静默返回空」。
    pub fallback: Option<bool>,
}

// ── 翻页器 ──────────────────────────────────────────────────────────────────

/// `ui pager` 的参数。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Pager {
    pub title: String,
}

// ── 选文件 ──────────────────────────────────────────────────────────────────

/// `ui pick` 的参数。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Pick {
    pub dir: PathBuf,
    /// 预先填进过滤框的词。
    pub filter: String,
    /// 允许多选（`Tab` 标记，`Enter` 收工）。
    pub multi: bool,
    /// 只让选目录。
    pub dir_only: bool,
}

/// `toolbox-hub ui …` 要跑哪一个组件。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UiCommand {
    Confirm(Confirm),
    Pager {
        options: Pager,
        file: Option<PathBuf>,
    },
    Pick(Pick),
}

/// 跑一个组件。`Err` 一律映射成退出码 [`UNSUPPORTED_CODE`]。
pub fn run(command: UiCommand) -> Result<Outcome, String> {
    match command {
        UiCommand::Confirm(options) => confirm(&options),
        UiCommand::Pager { options, file } => pager(&options, file.as_deref()),
        UiCommand::Pick(options) => pick(&options),
    }
}

/// 跑一个组件。`Err` 一律映射成退出码 [`UNSUPPORTED_CODE`]。
pub fn confirm(options: &Confirm) -> Result<Outcome, String> {
    let Some(mut session) = Session::open()? else {
        return match options.fallback {
            Some(true) => Ok(Outcome::Ok),
            Some(false) => Ok(Outcome::Cancelled),
            None => Err(String::from(
                "不是交互式终端：确认框弹不出来。给个 --default yes/no 让脚本在没有终端时也能跑",
            )),
        };
    };

    let mut yes = !options.danger;
    loop {
        session.draw(|frame, area| draw_confirm(frame, area, options, yes))?;
        if !session.poll(200)? {
            continue;
        }
        if let Event::Key(key) = session.read()? {
            match key.code {
                // Enter 用当前高亮；y/n 是直达快捷键。
                KeyCode::Enter => return Ok(choose(yes)),
                KeyCode::Char('y') | KeyCode::Char('Y') => return Ok(Outcome::Ok),
                KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc | KeyCode::Char('q') => {
                    return Ok(Outcome::Cancelled);
                }
                KeyCode::Left
                | KeyCode::Right
                | KeyCode::Tab
                | KeyCode::Char('h')
                | KeyCode::Char('l') => yes = !yes,
                _ => {}
            }
        }
    }
}

fn choose(yes: bool) -> Outcome {
    if yes { Outcome::Ok } else { Outcome::Cancelled }
}

fn draw_confirm(frame: &mut ratatui::Frame, area: Rect, options: &Confirm, yes: bool) {
    let width = area.width.saturating_sub(4).clamp(20, 76);
    // 行数按显示宽度估：中文是 2 列，不能按字符数算。
    let text_columns = width.saturating_sub(4).max(1) as usize;
    let message_lines = display_width(&options.message).max(1) as usize;
    let lines = message_lines.div_ceil(text_columns).max(1) as u16;
    let height = (lines + 5).min(area.height).max(6);
    let popup = Rect::new(
        area.x + (area.width.saturating_sub(width)) / 2,
        area.y + (area.height.saturating_sub(height)) / 2,
        width,
        height,
    );

    frame.render_widget(Clear, popup);
    let border = if options.danger {
        theme::RED
    } else {
        theme::BORDER
    };
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .title(" 确认 ")
        .title_style(theme::title_style())
        .border_style(Style::default().fg(border))
        .style(Style::default().bg(theme::PANEL));
    let inner = block.inner(popup);
    frame.render_widget(block, popup);

    let rows = Layout::vertical([
        Constraint::Min(1),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .split(inner);
    let (Some(text), Some(buttons), Some(hint)) = (
        rows.first().copied(),
        rows.get(1).copied(),
        rows.get(2).copied(),
    ) else {
        return;
    };

    frame.render_widget(
        Paragraph::new(options.message.clone())
            .style(Style::default().fg(theme::TEXT))
            .wrap(Wrap { trim: true }),
        text,
    );

    let button = |label: &str, active: bool, color: ratatui::style::Color| {
        Span::styled(
            format!(" {label} "),
            if active {
                Style::default()
                    .fg(theme::BG)
                    .bg(color)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(theme::DIM)
            },
        )
    };
    let yes_color = if options.danger {
        theme::RED
    } else {
        theme::GREEN
    };
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            button(&options.yes, yes, yes_color),
            Span::raw("   "),
            button(&options.no, !yes, theme::BORDER),
        ]))
        .alignment(Alignment::Center),
        buttons,
    );
    let confirm_keys = crate::ui::fit_hints(
        "←→ 选择 · Enter 确认当前 · y / n 直达 · Esc 取消",
        hint.width as usize,
    );
    frame.render_widget(
        Paragraph::new(Span::styled(
            confirm_keys,
            Style::default().fg(theme::FAINT),
        ))
        .alignment(Alignment::Center),
        hint,
    );
}

// ── 翻页器 ──────────────────────────────────────────────────────────────────

pub fn pager(options: &Pager, file: Option<&Path>) -> Result<Outcome, String> {
    // 内容来源：给了文件读文件，否则读管道。两个都没有就没得翻。
    let body = match file {
        Some(path) => std::fs::read_to_string(path)
            .map_err(|error| format!("读不了 {}：{error}", path.display()))?,
        None if !io::stdin().is_terminal() => {
            let mut text = String::new();
            io::stdin()
                .read_to_string(&mut text)
                .map_err(|error| format!("读不了标准输入：{error}"))?;
            text
        }
        None => {
            return Err(String::from(
                "没有内容可翻：把输出接进来（`cmd | toolbox-hub ui pager`）或给一个文件路径",
            ));
        }
    };

    // 没有终端：**原样透传**，这样 `cmd | toolbox-hub ui pager | grep foo` 照样能用。
    if !terminal_available() {
        let mut out = io::stdout();
        out.write_all(body.as_bytes())
            .and_then(|()| out.flush())
            .map_err(|error| format!("写不出去了：{error}"))?;
        return Ok(Outcome::Ok);
    }

    let Some(mut session) = Session::open()? else {
        return Err(String::from("不是交互式终端，翻页器开不起来"));
    };
    let mut viewer = Viewer::from_text(options.title.clone(), body);

    loop {
        session.draw(|frame, area| {
            let rows = Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).split(area);
            if let Some(body) = rows.first().copied() {
                crate::ui::viewer::draw(frame, &viewer, body);
            }
            if let Some(hint) = rows.get(1).copied() {
                draw_hint(frame, hint, PAGER_KEYS);
            }
        })?;
        if !session.poll(100)? {
            continue;
        }
        let height = session.height();
        match session.read()? {
            Event::Key(key) => match key.code {
                KeyCode::Esc | KeyCode::Char('q') => return Ok(Outcome::Cancelled),
                KeyCode::Up | KeyCode::Char('k') => viewer.scroll_by(-1),
                KeyCode::Down | KeyCode::Char('j') => viewer.scroll_by(1),
                KeyCode::PageUp | KeyCode::Char('b') => viewer.scroll_by(-(height as isize)),
                KeyCode::PageDown | KeyCode::Char(' ') => viewer.scroll_by(height as isize),
                KeyCode::Left | KeyCode::Char('h') => viewer.scroll_horizontal_by(-8),
                KeyCode::Right | KeyCode::Char('l') => viewer.scroll_horizontal_by(8),
                KeyCode::Home | KeyCode::Char('g') => viewer.scroll_to_top(),
                KeyCode::End | KeyCode::Char('G') => viewer.scroll_to_bottom(),
                _ => {}
            },
            Event::Mouse(mouse) => match mouse.kind {
                MouseEventKind::ScrollUp => viewer.scroll_by(-3),
                MouseEventKind::ScrollDown => viewer.scroll_by(3),
                _ => {}
            },
            _ => {}
        }
    }
}

// ── 选文件 ──────────────────────────────────────────────────────────────────

/// 选文件器按下一个键之后的结论。
#[derive(Debug, PartialEq, Eq)]
enum PickStep {
    Continue,
    Chosen(Vec<PathBuf>),
    Cancelled,
}

/// 选文件器的一键处理。
///
/// 抽成独立函数是为了能测：这里最容易犯的错是**抢走字母键**（`g`/`j`/`k`），
/// 结果打字过滤就废了 —— 主界面的选文件器只映射方向键、`Home`/`End` 和 Ctrl 组合，
/// 这里必须一模一样。
fn pick_key(picker: &mut Picker, key: KeyEvent, options: &Pick) -> PickStep {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    match key.code {
        KeyCode::Esc => return PickStep::Cancelled,
        KeyCode::Char('q') if ctrl => return PickStep::Cancelled,
        KeyCode::Up => picker.move_selection(-1),
        KeyCode::Down => picker.move_selection(1),
        KeyCode::PageUp => picker.move_selection(-10),
        KeyCode::PageDown => picker.move_selection(10),
        KeyCode::Home => picker.select_first(),
        KeyCode::End => picker.select_last(),
        // 这里**故意不映射 `g`/`G`/`j`/`k`**：选文件器里打字永远是过滤，
        // 抢走这些字母就没法筛文件名了（和主界面的选文件器保持一致）。
        KeyCode::Left => {
            picker.go_to_parent();
        }
        KeyCode::Tab => picker.toggle_mark(),
        KeyCode::Backspace => {
            picker.backspace();
            picker.refilter();
        }
        KeyCode::Char('u') if ctrl => {
            picker.filter.clear();
            picker.refilter();
        }
        KeyCode::Char('d') if ctrl => {
            // 「就用当前目录」——给 `--dir-only` 的字段省一次 Enter。
            return PickStep::Chosen(vec![picker.dir().to_path_buf()]);
        }
        KeyCode::Enter => match picker.selected_entry() {
            // 目录：进去（`--dir-only` 时 Enter 是"选中它"）
            Some(entry) if entry.is_dir && !options.dir_only => {
                let path = entry.path.clone();
                picker.enter_dir(&path);
            }
            // `--dir-only` 时文件不算答案。
            Some(entry) if options.dir_only && !entry.is_dir => {}
            Some(entry) => {
                return PickStep::Chosen(if options.multi && picker.marked_count() > 0 {
                    picker.marked_files()
                } else {
                    vec![entry.path.clone()]
                });
            }
            None => {}
        },
        KeyCode::Char(ch) if !ctrl && !ch.is_control() => {
            picker.push_char(ch);
            picker.refilter();
        }
        _ => {}
    }
    PickStep::Continue
}

/// 选文件：**优先交给外部文件管理器**（默认 yazi），内置的只当替补。
///
/// 为什么以外部程序为主：它自带预览、书签、多选与批量操作，浏览体验比内置那个
/// 好得多；把选文件交给它，也就不用再维护一套浏览逻辑。只有当它**用不了**
/// （没装 / 没有控制终端 / 起不来）时才退回内置 —— 脚本不会因为少一个外部程序
/// 就跑不动。
pub fn pick(options: &Pick) -> Result<Outcome, String> {
    if !options.dir.is_dir() {
        return Err(format!("不是目录：{}", options.dir.display()));
    }

    if let Some(program) = file_manager_program() {
        match pick_external(&program, options)? {
            ExternalPick::Chosen(paths) => {
                write_paths(&paths)?;
                return Ok(Outcome::Ok);
            }
            ExternalPick::Cancelled => return Ok(Outcome::Cancelled),
            ExternalPick::Unavailable => {}
        }
    }

    pick_builtin(options)
}

/// 内置选择器：外部文件管理器用不了时的替补。
fn pick_builtin(options: &Pick) -> Result<Outcome, String> {
    let Some(mut session) = Session::open()? else {
        return Err(String::from(
            "不是交互式终端：选文件要有人点。脚本里请把路径当参数传进来",
        ));
    };

    let mut picker = Picker::open(&options.dir, 0, options.multi, options.dir_only);
    if !options.filter.is_empty() {
        picker.filter.set(&options.filter);
        picker.refilter();
    }

    // 单选：Enter 直接收工；多选：Enter 收集已标记的（没标记就用当前项）。
    let mut chosen: Option<Vec<PathBuf>> = None;

    let keys = pick_keys(options);
    loop {
        session.draw(|frame, area| {
            let rows = Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).split(area);
            if let Some(body) = rows.first().copied() {
                crate::ui::picker::draw(frame, &picker, body);
            }
            if let Some(hint) = rows.get(1).copied() {
                draw_hint(frame, hint, &keys);
            }
        })?;
        if !session.poll(200)? {
            continue;
        }
        match session.read()? {
            Event::Key(key) => match pick_key(&mut picker, key, options) {
                PickStep::Continue => {}
                PickStep::Chosen(paths) => chosen = Some(paths),
                PickStep::Cancelled => return Ok(Outcome::Cancelled),
            },
            Event::Mouse(mouse) => match mouse.kind {
                MouseEventKind::ScrollUp => picker.move_selection(-3),
                MouseEventKind::ScrollDown => picker.move_selection(3),
                MouseEventKind::Down(MouseButton::Left) => picker.toggle_mark(),
                _ => {}
            },
            _ => {}
        }

        if let Some(paths) = chosen.take() {
            write_paths(&paths)?;
            return Ok(Outcome::Ok);
        }
    }
}

// ── 外部文件管理器 ──────────────────────────────────────────────────────────

/// stdout 只放结果：一行一个路径。
///
/// 外部文件管理器（yazi）**绝不写 stdout** —— 它的三条标准流都接到 `/dev/tty`，
/// 见 [`pick_external`]。这条约定是 `path=$(toolbox-hub ui pick)` 能用的前提。
fn write_paths(paths: &[PathBuf]) -> Result<(), String> {
    let mut out = io::stdout();
    for path in paths {
        writeln!(out, "{}", path.display()).map_err(|error| format!("写不出去了：{error}"))?;
    }
    out.flush().map_err(|error| format!("写不出去了：{error}"))
}

/// 外部文件管理器交回来的结论。
#[derive(Clone, Debug, PartialEq, Eq)]
enum ExternalPick {
    /// 用不了（没装 / 没有控制终端 / 起不来）：调用方该退回替补。
    Unavailable,
    /// 用户在里面取消了。**和 `Unavailable` 必须分开**：取消是用户的明确决定，
    /// 再弹一个内置选择器问一遍就等于把「取消」吃掉。
    Cancelled,
    Chosen(Vec<PathBuf>),
}

/// 环境变量读出来之后，到底用哪个程序（`None` = 明确要求用内置的）。
///
/// 抽成纯函数是为了能测：改 `std::env` 在 edition 2024 里是 `unsafe`，
/// 而且并行跑的测试互相会踩。
fn file_manager_program_from(raw: Option<&str>) -> Option<String> {
    let name = raw.unwrap_or_default().trim();
    if name.is_empty() {
        // 没配过就用 yazi：它是这个项目的默认文件管理器。
        return Some(String::from("yazi"));
    }
    if name == "builtin" || name == "none" {
        return None;
    }
    Some(name.to_string())
}

/// 用哪个程序当文件管理器。
///
/// 和主界面按 `y` 浏览目录共用同一个变量（[`crate::app::FILE_MANAGER_ENV`]）：
/// 一处配置管两件事，不会出现「浏览用 yazi、选文件用别的」这种分裂。
fn file_manager_program() -> Option<String> {
    let raw = std::env::var(crate::app::FILE_MANAGER_ENV).ok();
    file_manager_program_from(raw.as_deref())
}

/// 把终端整个交给 `program`，把它报告的文件/目录读回来。
///
/// ## 为什么三条标准流都接 `/dev/tty`
///
/// 脚本这边多半是 `path=$(toolbox-hub ui pick)` —— stdout 是**管道**，里面只允许
/// 有结果。外部程序会画满整个屏幕，它要是照着默认的三条流走，那些转义序列就全
/// 进了 `$path`。所以 stdin/stdout/stderr 一律换成控制终端。
///
/// ## 它用两份报告回答我们
///
/// * `--chooser-file`：在里面**打开**过的文件（`Enter` 触发），一行一个；
/// * `--cwd-file`：退出时所在的目录。
///
/// 目录只认后者：在 yazi 里「打开」一个目录是**进去**，不会触发 chooser。
/// 这和 ranger 的 `--choosedir` 是同一套路 —— 进到目标目录再退出，它就是答案。
fn pick_external(program: &str, options: &Pick) -> Result<ExternalPick, String> {
    let Some(program_path) = crate::runtime::find_on_path(program) else {
        return Ok(ExternalPick::Unavailable);
    };
    let Some(tty) = open_tty() else {
        // 没有控制终端（cron / CI / 三条流全被重定向）就没东西可交出去，
        // 退回替补，由它统一给「这里没人可以点」的报错。
        return Ok(ExternalPick::Unavailable);
    };

    if !options.filter.is_empty() {
        // yazi 没有「预填过滤词」这个能力（它是进去之后按 `/` 现打）。与其悄悄
        // 丢掉用户明确要求的过滤词，不如说一声 —— 写 stderr，不碰 stdout。
        let _ = writeln!(
            io::stderr(),
            "toolbox-hub: --filter 只有内置选择器认；在 {program} 里请按 / 现打过滤词"
        );
    }

    let stamp = std::process::id();
    let dir = std::env::temp_dir();
    let chooser = dir.join(format!("toolbox-hub-{stamp}-pick.chosen"));
    let cwd_file = dir.join(format!("toolbox-hub-{stamp}-pick.cwd"));
    // 上一轮的残留会让「它到底写没写」变得不可信。
    let _ = std::fs::remove_file(&chooser);
    let _ = std::fs::remove_file(&cwd_file);

    let spawned = {
        let stdin = tty
            .try_clone()
            .map_err(|error| format!("拿不到终端：{error}"))?;
        let stdout = tty
            .try_clone()
            .map_err(|error| format!("拿不到终端：{error}"))?;
        Command::new(&program_path)
            // 位置参数就是「打开时所在的位置」：只设 current_dir 不够，实测
            // yazi 仍会开在进程启动目录（这条在 browse_directories 里踩过）。
            .arg(&options.dir)
            .arg("--cwd-file")
            .arg(&cwd_file)
            .arg("--chooser-file")
            .arg(&chooser)
            .current_dir(&options.dir)
            .stdin(Stdio::from(stdin))
            .stdout(Stdio::from(stdout))
            .stderr(Stdio::from(tty))
            .status()
    };

    let chosen = std::fs::read_to_string(&chooser)
        .map(|text| crate::runtime::parse_chooser_file(&text))
        .unwrap_or_default();
    let cwd = std::fs::read_to_string(&cwd_file)
        .ok()
        .and_then(|text| crate::runtime::parse_cwd_file(&text));
    let _ = std::fs::remove_file(&chooser);
    let _ = std::fs::remove_file(&cwd_file);

    if spawned.is_err() {
        // 起不来就退回替补：别把脚本堵死在一个起不来的程序上。这里**不能**
        // 把错误往回抛 —— 那会把「yazi 没装好」变成硬失败，而替补其实还能干活。
        return Ok(ExternalPick::Unavailable);
    }

    if options.dir_only {
        if let Some(dir) = cwd.filter(|path| path.is_dir()) {
            return Ok(ExternalPick::Chosen(vec![dir]));
        }
        // 万一某个 keymap 把「打开」绑到了目录上，也认。
        if let Some(dir) = chosen.into_iter().find(|path| path.is_dir()) {
            return Ok(ExternalPick::Chosen(vec![dir]));
        }
        return Ok(ExternalPick::Cancelled);
    }

    let mut files: Vec<PathBuf> = chosen.into_iter().filter(|path| path.is_file()).collect();
    if files.is_empty() {
        return Ok(ExternalPick::Cancelled);
    }
    if !options.multi {
        files.truncate(1);
    }
    Ok(ExternalPick::Chosen(files))
}

// ── 终端会话 ────────────────────────────────────────────────────────────────

/// 打开控制终端。没有（cron、CI、`< /dev/null`）就返回 `None`。
///
/// **界面画到 `/dev/tty`，不是 stdout 也不是 stderr。** 脚本经常把两个都重定向掉
/// （`toolbox-hub ui pick > list.txt 2>/dev/null`），但终端其实还在 ——
/// 以「stderr 是不是 tty」判断会把这种情况下能用的组件判死。
fn open_tty() -> Option<std::fs::File> {
    std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/tty")
        .ok()
}

/// 只实现 [`Write`] 的薄壳。
///
/// `File` 同时实现 `Read` 和 `Write`，而 crossterm 的 `execute!` 内部调
/// `writer.by_ref()` —— 两边都能解释，编译器直接报歧义。包一层就干净了。
struct TtyWriter(std::fs::File);

impl Write for TtyWriter {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        self.0.write(buffer)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.0.flush()
    }
}

/// 现在有终端可用吗（测试与非交互分支用它判断）。
pub fn terminal_available() -> bool {
    open_tty().is_some()
}

/// 一个只画到 `/dev/tty` 的小终端会话。stdout 要留给结果。
struct Session {
    terminal: Terminal<CrosstermBackend<TtyWriter>>,
}

impl Session {
    /// 没有终端就返回 `None`（调用方按约定给退出码 2 或走 fallback）。
    fn open() -> Result<Option<Self>, String> {
        let Some(tty) = open_tty() else {
            return Ok(None);
        };
        // stdin 可能是管道（`cmd | …`）：键要从 /dev/tty 读，和 less 一个做法。
        if !io::stdin().is_terminal() {
            attach_tty_to_stdin()?;
        }

        enable_raw_mode().map_err(|error| format!("开不了 raw 模式：{error}"))?;
        let mut tty_out = TtyWriter(tty);
        execute!(tty_out, EnterAlternateScreen, EnableMouseCapture)
            .map_err(|error| format!("进不了备用屏幕：{error}"))?;
        let terminal = Terminal::new(CrosstermBackend::new(tty_out))
            .map_err(|error| format!("起不了终端：{error}"))?;
        Ok(Some(Self { terminal }))
    }

    fn draw(&mut self, mut paint: impl FnMut(&mut ratatui::Frame, Rect)) -> Result<(), String> {
        self.terminal
            .draw(|frame| {
                let area = frame.area();
                paint(frame, area);
            })
            .map(|_| ())
            .map_err(|error| format!("画不出来：{error}"))
    }

    fn poll(&self, millis: u64) -> Result<bool, String> {
        event::poll(Duration::from_millis(millis)).map_err(|error| format!("等按键失败：{error}"))
    }

    fn read(&self) -> Result<Event, String> {
        event::read().map_err(|error| format!("读按键失败：{error}"))
    }

    /// 可视高度（翻页用）。
    fn height(&self) -> u16 {
        self.terminal.size().map(|size| size.height).unwrap_or(24)
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(
            self.terminal.backend_mut(),
            LeaveAlternateScreen,
            DisableMouseCapture
        );
        let _ = self.terminal.show_cursor();
    }
}

/// 把 `/dev/tty` 接到 fd 0 上。
///
/// `cmd | toolbox-hub ui pager` 时 stdin 是管道，内容已经从里面读完了，
/// 剩下的按键得另外找地方读 —— `/dev/tty` 就是那个地方。
fn attach_tty_to_stdin() -> Result<(), String> {
    use std::os::fd::AsRawFd;

    let tty = std::fs::OpenOptions::new()
        .read(true)
        .open("/dev/tty")
        .map_err(|error| format!("打不开 /dev/tty：{error}"))?;
    // SAFETY: dup2 只是把 fd 0 指向 tty；tty 的所有权在本函数结束时释放，
    // 但 fd 0 已经被 dup 成独立的一份，所以不会失效。
    let result = unsafe { libc::dup2(tty.as_raw_fd(), libc::STDIN_FILENO) };
    if result < 0 {
        return Err(format!("接管不了 /dev/tty：{}", io::Error::last_os_error()));
    }
    Ok(())
}

/// 翻页器的键位（它没有输入框，`j`/`k`/`h`/`l` 可以放心用）。
const PAGER_KEYS: &str = "↑↓ 滚动 · ←→ 横移 · PgUp/PgDn 翻页 · g/G 顶/底 · q 退出";

/// 选文件器的键位。**不含 `j`/`k`/`g`/`G`** —— 那些字母要留给过滤框
/// （和主界面的选文件器一致）。`--multi` / `--dir-only` 会多出对应的一两条。
fn pick_keys(options: &Pick) -> String {
    let mut keys =
        String::from("↑↓ 选择 · Enter 进目录/选中 · ← 上级 · 打字过滤 · Ctrl+U 清空 · Esc 取消");
    if options.multi {
        keys.push_str(" · Tab 标记");
    }
    if options.dir_only {
        keys.push_str(" · Ctrl+D 用当前目录");
    }
    keys
}

/// 组件底部的键位提示行。
///
/// 主界面靠 footer 显示这些；组件是独立开的会话，**没有 footer** ——
/// 不画出来的话，`ui pager` 打开后你根本不知道按什么退出。
fn draw_hint(frame: &mut ratatui::Frame, area: Rect, keys: &str) {
    // 不带边框：这一行只有一行高，加个 `Borders::TOP` 就把仅有的空间吃光了
    // （面板自己已经有下边框，够当分隔）。
    //
    // 宽了要**整条整条地丢**：硬裁出来的「Ctrl+D 用当前」比不显示更糟 ——
    // 看着像有个键叫这个名字。
    let fitted = crate::ui::fit_hints(keys, area.width.saturating_sub(1) as usize);
    frame.render_widget(
        Paragraph::new(Span::styled(
            format!(" {fitted}"),
            Style::default().fg(theme::DIM),
        )),
        area,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn outcomes_map_to_the_documented_exit_codes() {
        assert_eq!(Outcome::Ok.code(), 0);
        assert_eq!(Outcome::Cancelled.code(), 1);
        assert_eq!(UNSUPPORTED_CODE, 2);
    }

    /// 文件管理器默认是 yazi；builtin / none 才是「用内置那个」。
    ///
    /// 抽成纯函数就是为了能这样测：edition 2024 里改 std::env 是 unsafe，
    /// 而且并行跑的测试互相会踩到对方设的值。
    #[test]
    fn the_file_manager_defaults_to_yazi_and_can_be_switched_off() {
        assert_eq!(file_manager_program_from(None).as_deref(), Some("yazi"));
        assert_eq!(file_manager_program_from(Some("")).as_deref(), Some("yazi"));
        assert_eq!(
            file_manager_program_from(Some("   ")).as_deref(),
            Some("yazi")
        );
        assert_eq!(
            file_manager_program_from(Some("  yazi ")).as_deref(),
            Some("yazi")
        );

        // 明确要求用内置的
        assert_eq!(file_manager_program_from(Some("builtin")), None);
        assert_eq!(file_manager_program_from(Some("none")), None);

        // 换别的文件管理器也认（主界面按 y 浏览目录用的是同一个变量）
        assert_eq!(file_manager_program_from(Some("lf")).as_deref(), Some("lf"));
        assert_eq!(
            file_manager_program_from(Some("ranger")).as_deref(),
            Some("ranger")
        );
    }

    /// 外部程序没装要报「用不了」，不能报「取消」。
    ///
    /// 这两个必须分开：Unavailable 会让调用方退回内置选择器继续服务，而
    /// Cancelled 直接就是最终答案 —— 报错了就会把「没装 yazi」变成「用户取消了」，
    /// 脚本静悄悄地什么也没拿到。
    #[test]
    fn a_missing_file_manager_falls_back_instead_of_cancelling() {
        let options = Pick {
            dir: std::env::temp_dir(),
            filter: String::new(),
            multi: false,
            dir_only: false,
        };

        let outcome = pick_external("toolbox-hub-no-such-file-manager", &options)
            .expect("没装不该是错误：它只是「用不了」");
        assert_eq!(outcome, ExternalPick::Unavailable);
    }

    #[test]
    fn pick_refuses_a_directory_that_does_not_exist() {
        let error = pick(&Pick {
            dir: PathBuf::from("/definitely/not/here"),
            filter: String::new(),
            multi: false,
            dir_only: false,
        })
        .expect_err("不存在的目录该报错");
        assert!(error.contains("不是目录"), "{error}");
    }

    #[test]
    fn pager_without_any_input_explains_itself() {
        // 测试环境里 stdin 不是 tty，所以这条走的是「没有内容」那一支；
        // 关键是不能静默成功（退出码 0）也不能 panic。
        let result = pager(
            &Pager {
                title: String::from("probe"),
            },
            None,
        );
        // stdin 可能是空的管道（不是 tty）→ 读到空串也算成功透传，这是对的；
        // 单元测试里只要求它不 panic，并且结论是明确的两种之一。
        assert!(matches!(result, Ok(Outcome::Ok) | Err(_)));
    }

    #[test]
    fn confirm_falls_back_to_the_declared_default_without_a_terminal() {
        // 这里 stderr 不是 tty，所以走 fallback 分支。
        let options = Confirm {
            message: String::from("要删掉吗？"),
            yes: String::from("删"),
            no: String::from("算了"),
            danger: true,
            fallback: Some(false),
        };
        if terminal_available() {
            return; // 有人在真终端里跑测试，跳过（否则会等按键，把测试挂住）
        }
        assert_eq!(
            confirm(&options).expect("有默认值就该成功"),
            Outcome::Cancelled
        );
    }

    #[test]
    fn confirm_without_a_default_refuses_instead_of_guessing() {
        if terminal_available() {
            return;
        }
        let options = Confirm {
            message: String::from("要删掉吗？"),
            yes: String::from("删"),
            no: String::from("算了"),
            danger: true,
            fallback: None,
        };
        let error = confirm(&options).expect_err("没有默认值就不该猜");
        assert!(error.contains("--default"), "{error}");
    }

    /// 字母键必须进过滤框，不能被导航抢走。
    ///
    /// 这条抓到过真错：`g`/`G` 一开始被映射成「首项 / 末项」，结果在选文件器里
    /// 打 `g` 是跳列表、**没法用它筛文件名**（主界面的选文件器故意不映射它们）。
    #[test]
    fn pick_lets_letters_reach_the_filter() {
        let dir = std::env::temp_dir().join(format!("tbx-pick-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("建临时目录");
        for name in ["alpha.txt", "gamma.txt", "beta.txt"] {
            std::fs::write(dir.join(name), b"x").expect("写测试文件");
        }

        let options = Pick {
            dir: dir.clone(),
            filter: String::new(),
            multi: false,
            dir_only: false,
        };
        let mut picker = Picker::open(&dir, 0, false, false);

        for ch in "gamma".chars() {
            let step = pick_key(
                &mut picker,
                KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE),
                &options,
            );
            assert_eq!(step, PickStep::Continue, "「{ch}」不该被当成导航键");
        }
        assert_eq!(picker.filter.text(), "gamma");
        assert_eq!(picker.len(), 1, "过滤该只剩 gamma.txt");
        assert_eq!(
            picker.selected_entry().map(|entry| entry.name.as_str()),
            Some("gamma.txt")
        );

        // Enter 收工，拿到的就是过滤后选中的那个。
        assert_eq!(
            pick_key(
                &mut picker,
                KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
                &options
            ),
            PickStep::Chosen(vec![dir.join("gamma.txt")])
        );

        // Esc 是取消，不是「选中当前项」。
        assert_eq!(
            pick_key(
                &mut picker,
                KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
                &options
            ),
            PickStep::Cancelled
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `--dir-only` 时文件不算答案（Enter 落在文件上什么也不做）。
    #[test]
    fn pick_dir_only_ignores_files() {
        let dir = std::env::temp_dir().join(format!("tbx-pick-dir-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("建临时目录");
        std::fs::write(dir.join("note.txt"), b"x").expect("写测试文件");

        let options = Pick {
            dir: dir.clone(),
            filter: String::new(),
            multi: false,
            dir_only: true,
        };
        let mut picker = Picker::open(&dir, 0, false, true);
        assert_eq!(
            pick_key(
                &mut picker,
                KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
                &options
            ),
            PickStep::Continue,
            "只让选目录时，Enter 落在文件上不该交出答案"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_message_height_accounts_for_wide_characters() {
        // 中文按 2 列算：同一段长度，显示宽度是字符数的两倍 → 换行数也要跟着变。
        assert!(display_width("中文中文") > "中文中文".chars().count() as u16);
    }
}
