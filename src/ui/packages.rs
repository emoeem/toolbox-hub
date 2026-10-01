//! 软件包中心的渲染（pacseek 那套版式的 Rust 版）。
//!
//! 一屏装下四块：**结果表**（搜索 / 已安装 / 新闻三种模式共用）、**搜索框**、
//! **安装清单**、**包信息**。所有坐标只有 [`layout`] 一个来源 —— 渲染与鼠标命中
//! 都调它，避免两边各算一套导致「点得到的地方和看得见的地方」对不上。
//!
//! 另外两个浮层（排序菜单、执行确认）盖在最上面：确认面板上那条命令就是马上要跑
//! 的那条（[`crate::packages::command_preview`] 算的），不是另写一遍的说明文字。

use ratatui::{
    layout::{Constraint, Flex, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Cell, Clear, Paragraph, Row, Table, TableState, Wrap},
};

use crate::{
    app::{
        App,
        package_view::{Confirm, PackageMode, PackageView, Pane},
    },
    ui::theme,
};

#[derive(Clone, Copy, Debug)]
pub struct PackageLayout {
    /// 顶栏状态（结果数 / 排序 / 队列 / 待更新 / 新闻）。
    pub status: Rect,
    /// 模式标签 + 仓库/分类标签。
    pub tabs: Rect,
    pub results: Rect,
    /// 搜索框（结果表下面那一行）。
    pub search: Rect,
    /// 安装清单。
    pub queue: Rect,
    /// 右侧包信息。
    pub info: Rect,
}

/// 软件包中心唯一的布局来源。
pub fn layout(area: Rect) -> PackageLayout {
    let rows = Layout::vertical([
        Constraint::Length(1), // 状态
        Constraint::Length(1), // 标签
        Constraint::Min(8),    // 主体
    ])
    .split(area);

    // 左边一列从上到下：结果表 → 搜索框 → 安装清单；右边整列是包信息。
    let columns =
        Layout::horizontal([Constraint::Percentage(62), Constraint::Percentage(38)]).split(rows[2]);
    let left = Layout::vertical([
        Constraint::Min(4),
        Constraint::Length(1),
        Constraint::Length(6),
    ])
    .split(columns[0]);

    PackageLayout {
        status: rows[0],
        tabs: rows[1],
        results: left[0],
        search: left[1],
        queue: left[2],
        info: columns[1],
    }
}

/// 顶栏标签的命中区：给鼠标用，渲染时按同一套顺序画。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TabTarget {
    /// 切模式（搜索 / 已安装 / 新闻）。
    Mode(usize),
    /// 开关一个仓库 / 分类 / 已读标签。
    Filter(usize),
}

/// 顶栏每个标签的矩形。
pub fn tab_hits(view: &PackageView, area: Rect) -> Vec<(Rect, TabTarget)> {
    let mut hits = Vec::new();
    let mut x = area.x;

    for (index, (label, _)) in view.mode_tabs().iter().enumerate() {
        let width = label.chars().count() as u16 + 4; // [ 搜索 ]
        hits.push((Rect::new(x, area.y, width, 1), TabTarget::Mode(index)));
        x = x.saturating_add(width + 1);
    }

    x = x.saturating_add(1); // 模式与筛选之间的分隔

    for (index, (label, _, count)) in view.chips().iter().enumerate() {
        let width = label.chars().count() as u16 + digits(*count) + 3; // [core 42✓]
        hits.push((Rect::new(x, area.y, width, 1), TabTarget::Filter(index)));
        x = x.saturating_add(width + 1);
    }

    hits
}

fn digits(value: usize) -> u16 {
    value.to_string().len() as u16
}

pub fn draw(frame: &mut ratatui::Frame, app: &App, area: Rect) {
    let Some(view) = app.packages.as_ref() else {
        return;
    };

    let block = theme::panel(" 软件包中心 ");
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let areas = layout(inner);
    draw_status(frame, view, areas.status);
    draw_tabs(frame, view, areas.tabs);
    draw_rows(frame, view, areas.results);
    draw_search(frame, view, areas.search);
    draw_queue(frame, view, areas.queue);
    draw_info(frame, view, areas.info);

    // 浮层最后画，保证盖在上面
    if let Some(confirm) = &view.confirm {
        draw_confirm(frame, confirm, inner);
    } else if let Some(selected) = view.sort_menu {
        draw_sort_menu(frame, view, selected, inner);
    }
}

// ── 顶栏 ────────────────────────────────────────────────────────────────────

fn draw_status(frame: &mut ratatui::Frame, view: &PackageView, area: Rect) {
    let mut spans = vec![
        Span::styled(" 结果 ", Style::default().fg(theme::DIM)),
        Span::styled(
            format!("{}", view.rows_len()),
            Style::default()
                .fg(theme::CYAN)
                .add_modifier(Modifier::BOLD),
        ),
    ];

    if view.dry_run {
        spans.push(Span::styled(
            "  演练 ",
            Style::default()
                .fg(theme::BG)
                .bg(theme::YELLOW)
                .add_modifier(Modifier::BOLD),
        ));
    }

    if view.mode != PackageMode::News {
        spans.push(Span::styled(
            format!("  排序 {}▾", view.sort.label()),
            Style::default().fg(theme::PURPLE),
        ));
    }

    spans.push(Span::styled(
        format!("  操作 {}  ", view.operation.label()),
        Style::default().fg(theme::YELLOW),
    ));
    spans.push(Span::styled(
        format!("队列 {}", view.queue.len()),
        Style::default().fg(if view.queue.is_empty() {
            theme::DIM
        } else {
            theme::YELLOW
        }),
    ));

    if let Some(count) = view.pending_updates {
        spans.push(Span::styled(
            format!("  待更新 {count}"),
            Style::default().fg(if count > 0 {
                theme::YELLOW
            } else {
                theme::GREEN
            }),
        ));
    }
    if let Some(unread) = view.news_unread().filter(|count| *count > 0) {
        spans.push(Span::styled(
            format!("  新闻 {unread} 未读"),
            Style::default().fg(theme::YELLOW),
        ));
    }

    spans.push(Span::styled(
        format!("   {}", view.status_line()),
        Style::default().fg(theme::FAINT),
    ));
    if let Some(error) = view.errors.first() {
        spans.push(Span::styled(
            format!("  ! {error}"),
            Style::default().fg(theme::RED),
        ));
    }

    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn draw_tabs(frame: &mut ratatui::Frame, view: &PackageView, area: Rect) {
    let mut spans = Vec::new();

    for (label, active) in view.mode_tabs() {
        spans.push(Span::styled(
            format!(" {} ", label),
            if active {
                Style::default()
                    .fg(theme::BG)
                    .bg(theme::PURPLE)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(theme::DIM)
            },
        ));
        spans.push(Span::raw(" "));
    }

    spans.push(Span::styled("│ ", Style::default().fg(theme::FAINT)));

    for (label, on, count) in view.chips() {
        let color = if on {
            if label == "aur" {
                theme::PURPLE
            } else {
                theme::CYAN
            }
        } else {
            theme::FAINT
        };
        spans.push(Span::styled(
            format!("[{label} {count}{}]", if on { "✓" } else { "·" }),
            Style::default().fg(color),
        ));
        spans.push(Span::raw(" "));
    }

    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

// ── 结果区（三种模式）───────────────────────────────────────────────────────

fn draw_rows(frame: &mut ratatui::Frame, view: &PackageView, area: Rect) {
    match view.mode {
        PackageMode::Search => draw_search_rows(frame, view, area),
        PackageMode::Installed => draw_installed_rows(frame, view, area),
        PackageMode::News => draw_news_rows(frame, view, area),
    }
}

/// 结果表统一的高亮样式（焦点不在结果区时不高亮，免得看错地方）。
fn row_style(view: &PackageView) -> Style {
    if view.pane == Pane::Rows {
        Style::default().bg(theme::HIGHLIGHT).fg(theme::TEXT)
    } else {
        Style::default().fg(theme::TEXT)
    }
}

fn empty_hint(frame: &mut ratatui::Frame, text: &str, area: Rect) {
    frame.render_widget(
        Paragraph::new(text)
            .style(Style::default().fg(theme::FAINT))
            .wrap(Wrap { trim: true }),
        area,
    );
}

fn draw_search_rows(frame: &mut ratatui::Frame, view: &PackageView, area: Rect) {
    if view.rows_len() == 0 {
        let text = if view.searching {
            "搜索中…（官方源 + AUR 两路并行）"
        } else if view.hits.is_empty() {
            "还没有结果：在下面输入关键词回车（例如 fzf、ripgrep）"
        } else {
            "都被仓库标签筛掉了：点标签或按 0 全开"
        };
        empty_hint(frame, text, area);
        return;
    }

    let rows = (0..view.rows_len())
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
                extra = format!("! 过期 {extra}");
            }
            let queued = view.queue.iter().any(|item| item.name == hit.name);

            Row::new(vec![
                Cell::from(hit.repo.clone()).style(Style::default().fg(repo_color)),
                Cell::from(format!("{}{}", if queued { "● " } else { "" }, hit.name)).style(
                    Style::default()
                        .fg(if queued { theme::YELLOW } else { theme::TEXT })
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
            Constraint::Length(13),
            Constraint::Min(16),
            Constraint::Length(13),
            Constraint::Length(13),
        ],
    )
    .row_highlight_style(row_style(view))
    .highlight_symbol("➤ ");

    let mut state = TableState::default().with_selected(Some(view.rows_selected()));
    frame.render_stateful_widget(table, area, &mut state);
}

fn draw_installed_rows(frame: &mut ratatui::Frame, view: &PackageView, area: Rect) {
    if view.installed_loading && view.installed.is_empty() {
        empty_hint(frame, "正在读已安装的包…", area);
        return;
    }
    if view.rows_len() == 0 {
        empty_hint(
            frame,
            if view.installed.is_empty() {
                "按 Enter 读一遍已安装的包（显式 / 依赖 / 外来 / 孤儿）"
            } else {
                "这个分类里没有包：点上面的标签换一个"
            },
            area,
        );
        return;
    }

    let rows = (0..view.rows_len())
        .filter_map(|row| view.installed_hit(row))
        .map(|package| {
            let tag_color = if package.orphan {
                theme::YELLOW
            } else if package.foreign {
                theme::PURPLE
            } else if package.explicit {
                theme::GREEN
            } else {
                theme::FAINT
            };
            let queued = view.queue.iter().any(|item| item.name == package.name);
            Row::new(vec![
                Cell::from(package.tag()).style(Style::default().fg(tag_color)),
                Cell::from(format!(
                    "{}{}",
                    if queued { "● " } else { "" },
                    package.name
                ))
                .style(
                    Style::default()
                        .fg(if queued { theme::YELLOW } else { theme::TEXT })
                        .add_modifier(Modifier::BOLD),
                ),
                Cell::from(package.version.clone()).style(Style::default().fg(theme::DIM)),
            ])
            .height(1)
        });

    let table = Table::new(
        rows,
        [
            Constraint::Length(6),
            Constraint::Min(20),
            Constraint::Length(18),
        ],
    )
    .row_highlight_style(row_style(view))
    .highlight_symbol("➤ ");

    let mut state = TableState::default().with_selected(Some(view.rows_selected()));
    frame.render_stateful_widget(table, area, &mut state);
}

fn draw_news_rows(frame: &mut ratatui::Frame, view: &PackageView, area: Rect) {
    if view.rows_len() == 0 {
        empty_hint(
            frame,
            if view.news.is_empty() {
                "按 Enter 抓一次 Arch 新闻（archlinux.org/feeds/news）"
            } else {
                "这个筛选下没有新闻：点上面的标签换一个"
            },
            area,
        );
        return;
    }

    let rows = (0..view.rows_len())
        .filter_map(|row| view.news_hit(row))
        .map(|item| {
            let key = crate::packages::news_key(item);
            let is_read = view.read_news.contains(&key);
            // 「升级之后才发布的」单独打个记号：那几条最可能和刚才那次升级有关。
            let after_upgrade = view
                .news_mark
                .is_some_and(|mark| item.epoch.is_some_and(|epoch| epoch > mark));
            Row::new(vec![
                Cell::from(if is_read { "已读" } else { "未读" })
                    .style(Style::default().fg(if is_read { theme::FAINT } else { theme::YELLOW })),
                Cell::from(format!(
                    "{}{}",
                    if after_upgrade { "★ " } else { "" },
                    item.title
                ))
                .style(
                    Style::default()
                        .fg(if is_read { theme::DIM } else { theme::TEXT })
                        .add_modifier(if is_read {
                            Modifier::empty()
                        } else {
                            Modifier::BOLD
                        }),
                ),
                Cell::from(item.published.clone()).style(Style::default().fg(theme::FAINT)),
            ])
            .height(1)
        });

    let table = Table::new(
        rows,
        [
            Constraint::Length(5),
            Constraint::Min(24),
            Constraint::Length(31),
        ],
    )
    .row_highlight_style(row_style(view))
    .highlight_symbol("➤ ");

    let mut state = TableState::default().with_selected(Some(view.rows_selected()));
    frame.render_stateful_widget(table, area, &mut state);
}

// ── 搜索框 / 安装清单 / 包信息 ──────────────────────────────────────────────

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
        spans.push(Span::styled(
            "   Enter 上网搜 · ↑↓ 翻历史 · Esc 退出输入",
            Style::default().fg(theme::FAINT),
        ));
    } else {
        spans.push(Span::styled(
            view.query.clone(),
            Style::default().fg(theme::TEXT),
        ));
        spans.push(Span::styled(
            "   i 或 / 改词 · Ctrl+R 重搜 · Enter 搜/读",
            Style::default().fg(theme::FAINT),
        ));
    }

    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn draw_queue(frame: &mut ratatui::Frame, view: &PackageView, area: Rect) {
    let focused = view.pane == Pane::Queue;
    let title = format!(
        " 安装清单 ({}){} ",
        view.queue.len(),
        if focused { " · 焦点" } else { "" }
    );
    let mut block = theme::panel(&title);
    if focused {
        block = block.border_style(Style::default().fg(theme::PURPLE));
    }
    let inner = block.inner(area);
    frame.render_widget(block, area);

    if view.queue.is_empty() {
        frame.render_widget(
            Paragraph::new("空：结果里 Space 加进来，Enter 看要跑的命令")
                .style(Style::default().fg(theme::FAINT)),
            inner,
        );
        return;
    }

    let rows = view.queue.iter().map(|item| {
        Row::new(vec![
            Cell::from(item.origin.clone()).style(Style::default().fg(if item.origin == "aur" {
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
            Constraint::Length(10),
            Constraint::Min(16),
            Constraint::Length(16),
        ],
    )
    .row_highlight_style(if focused {
        Style::default().bg(theme::HIGHLIGHT).fg(theme::TEXT)
    } else {
        Style::default()
    })
    .highlight_symbol("➤ ");

    let mut state = TableState::default().with_selected(Some(view.queue_selected));
    frame.render_stateful_widget(table, inner, &mut state);
}

fn draw_info(frame: &mut ratatui::Frame, view: &PackageView, area: Rect) {
    let title = view
        .info_title()
        .unwrap_or_else(|| String::from("选中一个包看信息"));
    let focused = view.pane == Pane::Info;
    let mut block = theme::panel(&format!(
        " 包信息 · {title}{} ",
        if focused { " · 焦点" } else { "" }
    ));
    if focused {
        block = block.border_style(Style::default().fg(theme::PURPLE));
    }
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let mut lines: Vec<Line> = Vec::new();

    if let Some(error) = &view.info_error {
        lines.push(Line::from(Span::styled(
            format!(" {error}"),
            Style::default().fg(theme::RED),
        )));
    } else if view.info_pending.is_some() && view.info.is_none() {
        lines.push(Line::from(Span::styled(
            " 取包信息中…",
            Style::default().fg(theme::FAINT),
        )));
    }

    if let Some((_, fields)) = &view.info {
        for (key, value) in fields {
            lines.push(Line::from(vec![
                Span::styled(format!(" {key:<16}"), Style::default().fg(theme::DIM)),
                Span::styled(value.clone(), Style::default().fg(theme::TEXT)),
            ]));
        }
    }

    if lines.is_empty() {
        lines.push(Line::from(Span::styled(
            " 还没选中包：↑↓ 挑一个，信息会自己跟上来",
            Style::default().fg(theme::FAINT),
        )));
    }

    frame.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .scroll((view.info_scroll.min(u16::MAX as usize) as u16, 0)),
        inner,
    );
}

// ── 浮层：排序菜单与执行确认 ────────────────────────────────────────────────

fn centered(area: Rect, width_percent: u16, height: u16) -> Rect {
    let vertical = Layout::vertical([Constraint::Length(height)])
        .flex(Flex::Center)
        .split(area);
    let horizontal = Layout::horizontal([Constraint::Percentage(width_percent)])
        .flex(Flex::Center)
        .split(vertical[0]);
    horizontal[0]
}

fn draw_sort_menu(frame: &mut ratatui::Frame, view: &PackageView, selected: usize, area: Rect) {
    let sorts = view.available_sorts();
    let height = sorts.len() as u16 + 2;
    let popup = centered(area, 40, height);
    frame.render_widget(Clear, popup);

    let lines: Vec<Line> = sorts
        .iter()
        .enumerate()
        .map(|(index, mode)| {
            let active = index == selected;
            Line::from(Span::styled(
                format!(" {} {}", if active { "➤" } else { " " }, mode.label()),
                if active {
                    Style::default()
                        .fg(theme::BG)
                        .bg(theme::PURPLE)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(theme::TEXT)
                },
            ))
        })
        .collect();

    frame.render_widget(
        Paragraph::new(lines).block(theme::panel(" 排序 · Enter 确定 ")),
        popup,
    );
}

fn draw_confirm(frame: &mut ratatui::Frame, confirm: &Confirm, area: Rect) {
    let height = (confirm.notes.len() as u16 + 6).min(area.height);
    let popup = centered(area, 76, height);
    frame.render_widget(Clear, popup);

    let mut lines = vec![
        Line::from(vec![
            Span::styled(" 将执行  ", Style::default().fg(theme::DIM)),
            Span::styled(
                confirm.command.clone(),
                Style::default()
                    .fg(theme::GREEN)
                    .add_modifier(Modifier::BOLD),
            ),
        ]),
        Line::from(""),
    ];

    for note in &confirm.notes {
        let color = if note.starts_with("有 ") && note.contains("依赖") {
            theme::YELLOW
        } else {
            theme::DIM
        };
        lines.push(Line::from(Span::styled(
            format!("  · {note}"),
            Style::default().fg(color),
        )));
    }

    if confirm.pending {
        lines.push(Line::from(Span::styled(
            "  · 影响分析中…",
            Style::default().fg(theme::FAINT),
        )));
    }

    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "  Enter 执行 · Esc 取消（队列留着）",
        Style::default().fg(theme::PURPLE),
    )));

    frame.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: true })
            .block(theme::panel(&format!(" {} · 确认 ", confirm.title))),
        popup,
    );
}
