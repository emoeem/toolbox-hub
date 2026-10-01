//! 「执行中」面板：实时输出尾巴 + 结构化进度 + 怎么取消。
//!
//! 它只占**详情区**那一块，列表照常能用 —— 长任务跑着的时候你还能翻别的工具。

use ratatui::{
    layout::{Constraint, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Paragraph, Wrap},
};

use crate::{app::App, ui::theme};

/// 转圈字符：按时间取一个，看起来就在动。
const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// 进度条宽度固定：百分比数字跳动时条本身不会左右晃。
const BAR_WIDTH: usize = 24;

/// 画进度条（实心 + 空心）。
fn bar(ratio: f64) -> String {
    let filled = ((ratio * BAR_WIDTH as f64).round() as usize).min(BAR_WIDTH);
    format!("{}{}", "█".repeat(filled), "░".repeat(BAR_WIDTH - filled))
}

pub fn draw(frame: &mut ratatui::Frame, app: &App, area: Rect) {
    let Some(running) = app.running.as_ref() else {
        return;
    };

    let block = theme::panel(" 执行中 ");
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let rows = Layout::vertical([Constraint::Length(3), Constraint::Min(1)]).split(inner);
    let (Some(head), Some(body)) = (rows.first().copied(), rows.get(1).copied()) else {
        return;
    };

    let elapsed = running.job.elapsed();
    let tick = (elapsed.as_millis() / 90) as usize % SPINNER.len();
    let progress = running.progress_label();
    let ratio = running.progress_ratio();

    // 第二行：探到总时长就画进度条，否则只报「已跑多久」。
    let status_line = match ratio {
        Some(ratio) => Line::from(vec![
            Span::styled(
                format!("[{}] ", bar(ratio)),
                Style::default().fg(theme::GREEN),
            ),
            Span::styled(
                format!("{:>5.1}%", ratio * 100.0),
                Style::default()
                    .fg(theme::TEXT)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                running
                    .progress_time_label()
                    .map(|label| format!("   {label}"))
                    .unwrap_or_default(),
                Style::default().fg(theme::DIM),
            ),
            Span::styled(
                format!("   已跑 {:.1}s", elapsed.as_secs_f64()),
                Style::default().fg(theme::FAINT),
            ),
            Span::styled(
                if progress.is_empty() {
                    String::new()
                } else {
                    format!("   {progress}")
                },
                Style::default().fg(theme::PURPLE),
            ),
        ]),
        None => Line::from(vec![
            Span::styled(
                format!("已跑 {:.1}s", elapsed.as_secs_f64()),
                Style::default().fg(theme::TEXT),
            ),
            Span::styled(
                if progress.is_empty() {
                    String::new()
                } else {
                    format!("   {progress}")
                },
                Style::default().fg(theme::PURPLE),
            ),
        ]),
    };

    // 进度是按时间转的，所以这一帧的「动」不用等新事件。
    frame.render_widget(
        Paragraph::new(Text::from(vec![
            Line::from(vec![
                Span::styled(
                    format!("{} ", SPINNER[tick]),
                    Style::default().fg(theme::GREEN),
                ),
                Span::styled("$ ", Style::default().fg(theme::FAINT)),
                Span::styled(
                    running.job.command.clone(),
                    Style::default().fg(theme::CYAN),
                ),
            ]),
            status_line,
            Line::from(Span::styled(
                "q 取消 · 列表与搜索照常可用",
                Style::default().fg(theme::FAINT),
            )),
        ])),
        head,
    );

    if running.tail.is_empty() {
        frame.render_widget(
            Paragraph::new("（还没有输出）").style(Style::default().fg(theme::FAINT)),
            body,
        );
        return;
    }

    // stderr 用另一种颜色：出错时一眼能分出来。
    let lines: Vec<Line> = running
        .tail
        .iter()
        .map(|(stderr, text)| {
            let style = if *stderr {
                Style::default().fg(theme::YELLOW)
            } else {
                Style::default().fg(theme::TEXT)
            };
            Line::from(Span::styled(text.clone(), style))
        })
        .collect();

    frame.render_widget(
        Paragraph::new(Text::from(lines)).wrap(Wrap { trim: false }),
        body,
    );
}
