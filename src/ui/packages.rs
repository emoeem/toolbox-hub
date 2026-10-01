//! 原生包管理视图的渲染（布局照 pacsea：结果 + 搜索 + 信息 + 队列）。
//!
//! 数据全在 [`crate::app::PackageView`] 里，这里只负责画。

use ratatui::{
    layout::{Constraint, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Cell, Paragraph, Row, Table, TableState, Wrap},
};

use crate::{
    app::{App, package_view::PackageView},
    ui::theme,
};

pub fn draw(frame: &mut ratatui::Frame, app: &App, area: Rect) {
    let Some(view) = app.packages.as_ref() else {
        return;
    };

    let block = theme::panel(" 包管理 ");
    let inner = block.inner(area);
    frame.render_widget(block, area);

    // 高度自适应：矮终端先把信息面板压小（和主界面一个思路）。
    let info_height = if inner.height >= 28 { 12 } else { 6 };
    let rows = Layout::vertical([
        Constraint::Length(1),           // 状态行
        Constraint::Min(5),              // 结果 / 队列
        Constraint::Length(1),           // 搜索框
        Constraint::Length(info_height), // 包信息
    ])
    .split(inner);
    let (Some(status), Some(middle), Some(search), Some(info)) = (
        rows.first().copied(),
        rows.get(1).copied(),
        rows.get(2).copied(),
        rows.get(3).copied(),
    ) else {
        return;
    };

    draw_status(frame, view, status);
    if view.focus_queue {
        draw_queue(frame, view, middle);
    } else {
        draw_results(frame, view, middle);
    }
    draw_search(frame, view, search);
    draw_info(frame, view, info);
}

fn draw_status(frame: &mut ratatui::Frame, view: &PackageView, area: Rect) {
    let mut spans = vec![
        Span::styled(" 结果 ", Style::default().fg(theme::DIM)),
        Span::styled(
            format!("{}", view.visible_len()),
            Style::default()
                .fg(theme::TEXT)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(" 条 ", Style::default().fg(theme::DIM)),
    ];

    // 仓库标签：[core✓] / [aur✗]
    for (name, enabled) in view.repo_chips() {
        let color = if enabled {
            if name == "aur" {
                theme::PURPLE
            } else {
                theme::CYAN
            }
        } else {
            theme::FAINT
        };
        spans.push(Span::styled(
            format!("[{name}{}]", if enabled { "✓" } else { "·" }),
            Style::default().fg(color),
        ));
        spans.push(Span::raw(" "));
    }

    if !view.queue.is_empty() {
        spans.push(Span::styled(
            format!("· 队列 {} ", view.queue.len()),
            Style::default()
                .fg(theme::YELLOW)
                .add_modifier(Modifier::BOLD),
        ));
    }
    if let Some(unread) = view.news_unread.filter(|count| *count > 0) {
        spans.push(Span::styled(
            format!("· 新闻 {unread} 未读 "),
            Style::default().fg(theme::YELLOW),
        ));
    }

    spans.push(Span::styled(
        view.status_line(),
        Style::default().fg(theme::DIM),
    ));
    if let Some(error) = view.errors.first() {
        spans.push(Span::styled(
            format!("  ! {error}"),
            Style::default().fg(theme::RED),
        ));
    }

    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn draw_results(frame: &mut ratatui::Frame, view: &PackageView, area: Rect) {
    if view.visible_len() == 0 {
        let text = if view.searching {
            "搜索中…（官方源 + AUR 各取一路）"
        } else if view.hits.is_empty() {
            "还没有结果：在上面输入关键词回车（例如 fzf、pacsea）"
        } else {
            "都被仓库标签筛掉了：Space 打开某个标签"
        };
        frame.render_widget(
            Paragraph::new(text)
                .style(Style::default().fg(theme::FAINT))
                .wrap(Wrap { trim: true }),
            area,
        );
        return;
    }

    let rows = (0..view.visible_len())
        .filter_map(|row| view.visible_hit(row))
        .map(|hit| {
            let repo_color = if hit.is_aur() {
                theme::PURPLE
            } else {
                theme::CYAN
            };
            let mut extra = String::new();
            if let Some(votes) = hit.votes {
                extra = format!("票 {votes}");
                if let Some(popularity) = hit.popularity {
                    extra.push_str(&format!("·{popularity:.2}"));
                }
            }
            if hit.out_of_date {
                extra = format!("! 已过期 {extra}");
            }

            Row::new(vec![
                Cell::from(hit.repo.clone()).style(Style::default().fg(repo_color)),
                Cell::from(hit.name.clone()).style(
                    Style::default()
                        .fg(theme::TEXT)
                        .add_modifier(Modifier::BOLD),
                ),
                Cell::from(hit.version.clone()).style(Style::default().fg(theme::DIM)),
                Cell::from(hit.description.clone()).style(Style::default().fg(theme::FAINT)),
                Cell::from(extra).style(Style::default().fg(theme::YELLOW)),
                Cell::from(hit.status_label()).style(Style::default().fg(theme::GREEN)),
            ])
            .height(1)
        });

    let table = Table::new(
        rows,
        [
            Constraint::Length(14),
            Constraint::Length(20),
            Constraint::Length(12),
            Constraint::Min(20),
            Constraint::Length(14),
            Constraint::Length(14),
        ],
    )
    .row_highlight_style(Style::default().bg(theme::HIGHLIGHT).fg(theme::TEXT))
    .highlight_symbol("➤ ");

    let mut state = TableState::default().with_selected(Some(view.selected));
    frame.render_stateful_widget(table, area, &mut state);
}

fn draw_queue(frame: &mut ratatui::Frame, view: &PackageView, area: Rect) {
    if view.queue.is_empty() {
        frame.render_widget(
            Paragraph::new("队列是空的：在结果里按 Space 把包加进来，然后 Enter 一起装")
                .style(Style::default().fg(theme::FAINT))
                .wrap(Wrap { trim: true }),
            area,
        );
        return;
    }

    let rows = view.queue.iter().map(|item| {
        Row::new(vec![
            Cell::from(if item.origin == "aur" {
                "aur"
            } else {
                item.origin.as_str()
            })
            .style(Style::default().fg(if item.origin == "aur" {
                theme::PURPLE
            } else {
                theme::CYAN
            })),
            Cell::from(item.name.clone()).style(
                Style::default()
                    .fg(theme::TEXT)
                    .add_modifier(Modifier::BOLD),
            ),
            Cell::from(item.version.clone()).style(Style::default().fg(theme::DIM)),
        ])
        .height(1)
    });

    let table = Table::new(
        rows,
        [
            Constraint::Length(14),
            Constraint::Min(20),
            Constraint::Length(16),
        ],
    )
    .row_highlight_style(Style::default().bg(theme::HIGHLIGHT).fg(theme::TEXT))
    .highlight_symbol("➤ ");

    let mut state = TableState::default().with_selected(Some(view.queue_selected));
    frame.render_stateful_widget(table, area, &mut state);
}

fn draw_search(frame: &mut ratatui::Frame, view: &PackageView, area: Rect) {
    let mut spans = vec![Span::styled(
        " 搜索 ",
        Style::default().fg(if view.editing {
            theme::GREEN
        } else {
            theme::DIM
        }),
    )];

    if view.editing {
        spans.push(Span::styled(
            format!("{}▏", view.query),
            Style::default()
                .fg(theme::TEXT)
                .add_modifier(Modifier::BOLD),
        ));
        if view.query.trim().is_empty() && !view.history.is_empty() {
            let hint = view
                .history
                .iter()
                .take(4)
                .cloned()
                .collect::<Vec<_>>()
                .join(" · ");
            spans.push(Span::styled(
                format!("   ↑↓ 历史：{hint}"),
                Style::default().fg(theme::FAINT),
            ));
        } else {
            spans.push(Span::styled(
                "   Enter 搜索 · Esc 退出输入 · Ctrl+U 清空",
                Style::default().fg(theme::FAINT),
            ));
        }
    } else {
        spans.push(Span::styled(
            view.query.clone(),
            Style::default().fg(theme::TEXT),
        ));
        spans.push(Span::styled(
            "   i 或 / 改词 · Ctrl+R 重搜",
            Style::default().fg(theme::FAINT),
        ));
    }

    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn draw_info(frame: &mut ratatui::Frame, view: &PackageView, area: Rect) {
    let title = view
        .info
        .as_ref()
        .map(|(name, _)| name.clone())
        .or_else(|| view.info_pending.clone())
        .unwrap_or_else(|| String::from("（选中一个包看信息）"));

    let mut lines = vec![Line::from(vec![
        Span::styled(" 包信息 ", Style::default().fg(theme::DIM)),
        Span::styled(
            title,
            Style::default()
                .fg(theme::TEXT)
                .add_modifier(Modifier::BOLD),
        ),
    ])];

    if let Some(error) = &view.info_error {
        lines.push(Line::from(Span::styled(
            format!("  {error}"),
            Style::default().fg(theme::RED),
        )));
    } else if view.info_pending.is_some() && view.info.is_none() {
        lines.push(Line::from(Span::styled(
            "  取包信息中…",
            Style::default().fg(theme::FAINT),
        )));
    }

    if let Some((_, fields)) = &view.info {
        // 两列并排：字段名对整齐，长值自动截断（详情在输出视图里看）
        for pair in fields.chunks(2) {
            let mut spans = Vec::new();
            for (key, value) in pair {
                let value = if value.chars().count() > 52 {
                    format!("{}…", value.chars().take(51).collect::<String>())
                } else {
                    value.clone()
                };
                spans.push(Span::styled(
                    format!(" {key:<16}"),
                    Style::default().fg(theme::DIM),
                ));
                spans.push(Span::styled(value, Style::default().fg(theme::TEXT)));
            }
            lines.push(Line::from(spans));
        }
    }

    if !view.news.is_empty() {
        lines.push(Line::from(Span::styled(
            format!(" Arch 新闻（{} 条）", view.news.len()),
            Style::default().fg(theme::DIM),
        )));
        for item in view.news.iter().take(3) {
            lines.push(Line::from(Span::styled(
                format!("  · {}", item.title),
                Style::default().fg(theme::FAINT),
            )));
        }
    }

    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), area);
}
