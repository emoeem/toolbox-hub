//! 文件视图：工作目录里的媒体文件。
//!
//! 回答的是一个很具体的问题：「脚本说没有文件，那这个目录里到底有什么？」
//! 用的规则和脚本的 `fd` 调用完全一致（见 [`crate::media`]），所以这里显示什么，
//! 脚本就看到什么。

use ratatui::{
    layout::{Constraint, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Cell, Paragraph, Row, Table, TableState, Wrap},
};

use crate::{
    app::App,
    media::human_size,
    ui::{short_path, theme},
};

pub fn draw(frame: &mut ratatui::Frame, app: &App, area: Rect) {
    let Some(view) = app.files.as_ref() else {
        return;
    };

    let block = theme::panel(" 文件 ");
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let rows = Layout::vertical([Constraint::Length(2), Constraint::Min(1)]).split(inner);
    let (Some(head), Some(list)) = (rows.first().copied(), rows.get(1).copied()) else {
        return;
    };

    let mut status = vec![
        Span::styled("目录  ", Style::default().fg(theme::DIM)),
        Span::styled(short_path(&view.root), Style::default().fg(theme::CYAN)),
        Span::styled(
            format!("   {} 个媒体文件", view.total()),
            Style::default()
                .fg(theme::TEXT)
                .add_modifier(Modifier::BOLD),
        ),
    ];
    if view.truncated {
        status.push(Span::styled(
            "（只数了前 500 个）",
            Style::default().fg(theme::YELLOW),
        ));
    }

    let filter_line = if view.filter.is_empty() {
        Span::styled("（直接打字就能过滤）", Style::default().fg(theme::FAINT))
    } else {
        Span::styled(
            format!("{}▌", view.filter),
            Style::default().fg(theme::TEXT),
        )
    };

    frame.render_widget(
        Paragraph::new(Text::from(vec![
            Line::from(status),
            Line::from(vec![
                Span::styled("过滤  ", Style::default().fg(theme::DIM)),
                filter_line,
                Span::styled(
                    format!("   {} 项", view.len()),
                    Style::default().fg(theme::FAINT),
                ),
            ]),
        ])),
        head,
    );

    if view.is_empty() {
        let text = if view.total() == 0 {
            "这个目录（含子目录）里没有媒体文件。\n脚本也会找不到东西 —— 按 d 换成真正放素材的目录，或者按 Esc 回车列表用 Enter 跑脚本。"
        } else {
            "没有匹配的文件。删掉过滤词试试。"
        };
        frame.render_widget(
            Paragraph::new(text)
                .style(Style::default().fg(theme::YELLOW))
                .wrap(Wrap { trim: true }),
            list,
        );
        return;
    }

    let rows = (0..view.len())
        .filter_map(|index| view.entry(index))
        .map(|file| {
            Row::new(vec![
                Cell::from(file.name.clone()).style(Style::default().fg(theme::TEXT)),
                Cell::from(human_size(file.size)).style(Style::default().fg(theme::FAINT)),
                Cell::from(file.relative.clone()).style(Style::default().fg(theme::DIM)),
            ])
            .height(1)
        });

    let table = Table::new(
        rows,
        [
            Constraint::Min(18),
            Constraint::Length(10),
            Constraint::Min(20),
        ],
    )
    .row_highlight_style(Style::default().bg(theme::HIGHLIGHT).fg(theme::TEXT))
    .highlight_symbol("➤ ");

    let mut state = TableState::default().with_selected(Some(view.selected));
    frame.render_stateful_widget(table, list, &mut state);
}
