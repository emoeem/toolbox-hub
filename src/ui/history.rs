//! 执行历史列表。
//!
//! 用表格而不是纯文本：时间 / 工具 / 结果 / 耗时 / 命令，一屏看完。
//! `Enter` 重跑选中的那条（argv 直接来自记录，不经过 shell）。

use ratatui::{
    layout::{Constraint, Rect},
    style::{Modifier, Style},
    widgets::{Cell, Paragraph, Row, Table, TableState, Wrap},
};

use crate::{app::App, ui::theme, ui::window};

pub fn draw(frame: &mut ratatui::Frame, app: &App, area: Rect) {
    let Some(view) = app.history.as_ref() else {
        return;
    };

    let block = theme::panel(" 执行历史 ");
    let inner = block.inner(area);
    frame.render_widget(block, area);

    if view.entries.is_empty() {
        frame.render_widget(
            Paragraph::new(
                "还没有执行过任何工具。\n跑一次之后，这里会记下时间、参数、结果和耗时。",
            )
            .style(Style::default().fg(theme::DIM))
            .wrap(Wrap { trim: true }),
            inner,
        );
        return;
    }

    let header = Row::new(vec![
        Cell::from("时间"),
        Cell::from("工具"),
        Cell::from("结果"),
        Cell::from("耗时"),
        Cell::from("命令"),
    ])
    .style(Style::default().fg(theme::DIM).add_modifier(Modifier::BOLD));

    let rows_room = inner.height.saturating_sub(1); // 表头占一行
    let (start, end) = window(view.entries.len(), view.selected, rows_room);
    let selected = view.selected.min(view.entries.len().saturating_sub(1));
    let rows = view.entries[start..end].iter().map(|entry| {
        let status_style = if entry.success {
            Style::default().fg(theme::GREEN)
        } else {
            Style::default().fg(theme::YELLOW)
        };

        Row::new(vec![
            Cell::from(entry.ago()).style(Style::default().fg(theme::FAINT)),
            Cell::from(entry.tool_name.clone()).style(Style::default().fg(theme::TEXT)),
            Cell::from(entry.status()).style(status_style),
            Cell::from(format!("{:.2}s", entry.millis as f64 / 1000.0))
                .style(Style::default().fg(theme::DIM)),
            Cell::from(entry.command_line()).style(Style::default().fg(theme::FAINT)),
        ])
        .height(1)
    });

    let table = Table::new(
        rows,
        [
            Constraint::Length(9),
            Constraint::Percentage(22),
            Constraint::Length(6),
            Constraint::Length(8),
            Constraint::Min(20),
        ],
    )
    .header(header)
    .row_highlight_style(Style::default().bg(theme::HIGHLIGHT).fg(theme::TEXT))
    .highlight_symbol("▸ ");

    // 高亮完全由 `view.selected` 决定，每帧现造一个 TableState 就够了。
    let mut state = TableState::default().with_selected(Some(selected - start));
    frame.render_stateful_widget(table, inner, &mut state);
}
