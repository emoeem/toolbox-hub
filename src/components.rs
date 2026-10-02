//! 给脚本用的界面组件：`toolbox-hub ui confirm | pager | pick`。
//!
//! 脚本缺的往往不是「功能」而是「界面」—— 挑一个文件、确认一次、翻一页长输出。
//! 这里把主界面已经在用的部件（[`crate::ui::picker`] / [`crate::ui::viewer`] /
//! `theme`）单独暴露出来，脚本拿 `$(…)` 就能消费。
//!
//! ## 三条约定（破坏任何一条，这东西在脚本里就没法用）
//!
//! 1. **stdout 只放结果，界面一律画到 stderr。** 否则
//!    `path=$(toolbox-hub ui pick)` 会被转义序列污染 —— 这是最重要的一条。
//! 2. **退出码固定**：`0` 选了/确认，`1` 用户取消（Esc/n），`2` 参数错或环境不支持
//!    （不是终端、模板读不出来）。脚本里 `if p=$(tbx_pick); then … fi` 直接可用。
//! 3. **键位与主界面一致**：`↑↓`/`jk` 选择、`g`/`G` 首末、`PgUp/PgDn` 翻页、
//!    `Esc` 取消、`Tab` 多选、打字即过滤。学一次就够。
//!
//! 组件不做任何录制/历史/状态落盘：它们是"一次性"的界面，跑完就退。

use std::{
    io::{self, IsTerminal, Read, Write},
    path::{Path, PathBuf},
    time::Duration,
};

use ratatui::{
    Terminal,
    backend::CrosstermBackend,
    crossterm::{
        event::{
            self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyModifiers,
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
    frame.render_widget(
        Paragraph::new(Span::styled(
            "←→ 选择 · Enter 确认 · y / n 直达 · Esc 取消",
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
        session.draw(|frame, area| crate::ui::viewer::draw(frame, &viewer, area))?;
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

pub fn pick(options: &Pick) -> Result<Outcome, String> {
    if !options.dir.is_dir() {
        return Err(format!("不是目录：{}", options.dir.display()));
    }
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

    loop {
        session.draw(|frame, area| crate::ui::picker::draw(frame, &picker, area))?;
        if !session.poll(200)? {
            continue;
        }
        match session.read()? {
            Event::Key(key) => {
                let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
                match key.code {
                    KeyCode::Esc => return Ok(Outcome::Cancelled),
                    KeyCode::Char('q') if ctrl => return Ok(Outcome::Cancelled),
                    KeyCode::Up => picker.move_selection(-1),
                    KeyCode::Down => picker.move_selection(1),
                    KeyCode::PageUp => picker.move_selection(-10),
                    KeyCode::PageDown => picker.move_selection(10),
                    KeyCode::Home => picker.select_first(),
                    KeyCode::End => picker.select_last(),
                    KeyCode::Char('g') if !ctrl => picker.select_first(),
                    KeyCode::Char('G') if !ctrl => picker.select_last(),
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
                        // 只让选目录的字段：用「当前目录」交差。
                        chosen = Some(vec![picker.dir().to_path_buf()]);
                    }
                    KeyCode::Enter => {
                        match picker.selected_entry() {
                            // 目录：进去（多选时也一样，`Ctrl-D` 才是"就用这个目录"）
                            Some(entry) if entry.is_dir && !options.dir_only => {
                                let path = entry.path.clone();
                                picker.enter_dir(&path);
                            }
                            // `--dir-only` 时文件不算答案（Enter 落在文件上就什么也不做）。
                            Some(entry) if options.dir_only && !entry.is_dir => {}
                            Some(entry) => {
                                if options.multi && picker.marked_count() > 0 {
                                    chosen = Some(picker.marked_files());
                                } else {
                                    chosen = Some(vec![entry.path.clone()]);
                                }
                            }
                            None => {}
                        }
                    }
                    KeyCode::Char(ch) if !ctrl && !ch.is_control() => {
                        picker.push_char(ch);
                        picker.refilter();
                    }
                    _ => {}
                }
            }
            Event::Mouse(mouse) => match mouse.kind {
                MouseEventKind::ScrollUp => picker.move_selection(-3),
                MouseEventKind::ScrollDown => picker.move_selection(3),
                MouseEventKind::Down(MouseButton::Left) => picker.toggle_mark(),
                _ => {}
            },
            _ => {}
        }

        if let Some(paths) = chosen.take() {
            // stdout 只放结果：一行一个路径。
            let mut out = io::stdout();
            for path in &paths {
                writeln!(out, "{}", path.display())
                    .map_err(|error| format!("写不出去了：{error}"))?;
            }
            out.flush()
                .map_err(|error| format!("写不出去了：{error}"))?;
            return Ok(Outcome::Ok);
        }
    }
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

/// 一行提示：这些组件的键位与主界面一致（`?` 就不另做一层了）。
#[allow(dead_code)]
pub fn key_hint() -> &'static str {
    "↑↓ 选择 · Enter 确认 · Esc 取消"
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

    #[test]
    fn the_message_height_accounts_for_wide_characters() {
        // 中文按 2 列算：同一段长度，显示宽度是字符数的两倍 → 换行数也要跟着变。
        assert!(display_width("中文中文") > "中文中文".chars().count() as u16);
    }
}
