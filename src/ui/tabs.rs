//! 两级导航条。
//!
//! 上一条是 **域**（媒体 / 图像 / 系统 / 网络 / 开发 / 工具），名字后面跟该域的工具数；
//! 数量为 0 的域用暗色显示，一眼能看出哪些 Provider 还没接。
//! 下一条平时是当前域的 **二级筛选**；处于跨域搜索时换成 [`draw_scope`]，
//! 因为那时候「当前域」已经约束不住结果了，必须说清命中的是哪些域。

use ratatui::{
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph, Tabs},
};

use crate::{
    app::{App, Scope},
    model::Domain,
    ui::theme,
};

pub fn draw_domains(frame: &mut ratatui::Frame, app: &App, area: Rect) {
    let current = app.current_domain();

    let titles: Vec<Line> = Domain::ALL
        .iter()
        .map(|domain| {
            let count = app.registry.tool_count_in(*domain);
            let selected = *domain == current;
            let style = if selected {
                Style::default()
                    .fg(theme::BG)
                    .bg(theme::domain_color(*domain))
                    .add_modifier(Modifier::BOLD)
            } else if count == 0 {
                Style::default().fg(theme::FAINT)
            } else {
                Style::default().fg(theme::DIM)
            };
            let label = if count == 0 {
                format!(" {} ", domain.label())
            } else {
                format!(" {} {} ", domain.label(), count)
            };
            Line::from(Span::styled(label, style))
        })
        .collect();

    frame.render_widget(
        Tabs::new(titles)
            .select(app.domain)
            .divider(Span::styled("│", Style::default().fg(theme::FAINT)))
            .block(
                Block::default()
                    .borders(Borders::BOTTOM)
                    .border_style(Style::default().fg(theme::FAINT)),
            ),
        area,
    );
}

pub fn draw_sub(frame: &mut ratatui::Frame, app: &App, area: Rect) {
    let titles: Vec<Line> = app
        .sub_tags
        .iter()
        .enumerate()
        .map(|(index, tag)| {
            let style = if index == app.sub {
                Style::default()
                    .fg(theme::tag_color(tag))
                    .add_modifier(Modifier::BOLD | Modifier::UNDERLINED)
            } else {
                Style::default().fg(theme::DIM)
            };
            Line::from(Span::styled(format!(" {tag} "), style))
        })
        .collect();

    frame.render_widget(
        Tabs::new(titles)
            .select(app.sub)
            .divider(Span::styled("·", Style::default().fg(theme::FAINT)))
            .block(
                Block::default()
                    .borders(Borders::BOTTOM)
                    .border_style(Style::default().fg(theme::FAINT)),
            ),
        area,
    );
}

/// 跨域搜索时顶掉二级筛选条的位置：说明「这是跨域结果、命中都落在哪些域」。
///
/// 搜索期间域 Tabs 仍然高亮着某个域，但结果不再受它限制 —— 不说明的话，
/// 用户会以为自己在看当前域，看到别的域的工具就会莫名其妙。
pub fn draw_scope(frame: &mut ratatui::Frame, app: &App, area: Rect) {
    // 视图（收藏 / 最近）优先：这时候域与分类都不参与筛选。
    let spans = if app.scope == Scope::All {
        search_spans(app)
    } else {
        view_spans(app)
    };

    frame.render_widget(
        Paragraph::new(Line::from(spans)).block(
            Block::default()
                .borders(Borders::BOTTOM)
                .border_style(Style::default().fg(theme::FAINT)),
        ),
        area,
    );
}

/// 「★ 收藏 / 最近使用」这一行的说明。
fn view_spans(app: &App) -> Vec<Span<'static>> {
    let mut spans = vec![
        Span::styled(
            format!(" {} ", app.scope.label()),
            Style::default()
                .fg(theme::BG)
                .bg(theme::PURPLE)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!("  {} 个", app.filtered.len()),
            Style::default().fg(theme::TEXT),
        ),
        Span::styled("  ·  跨所有域", Style::default().fg(theme::FAINT)),
    ];

    if app.scope == Scope::Favorites && app.favorite_count() == 0 {
        spans.push(Span::styled(
            "  ·  按 f 收藏当前选中的工具",
            Style::default().fg(theme::YELLOW),
        ));
    } else if app.scope == Scope::Recent {
        spans.push(Span::styled(
            "  ·  来自执行历史",
            Style::default().fg(theme::FAINT),
        ));
    }
    spans.push(Span::styled(
        "  ·  v 切换视图",
        Style::default().fg(theme::FAINT),
    ));
    spans
}

/// 搜索命中落在哪些域。
fn search_spans(app: &App) -> Vec<Span<'static>> {
    let mut spans = vec![
        Span::styled(
            " 跨域搜索 ",
            Style::default()
                .fg(theme::BG)
                .bg(theme::CYAN)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!("  命中 {} 个", app.filtered.len()),
            Style::default().fg(theme::TEXT),
        ),
    ];

    for (domain, hits) in app.search_hits_by_domain() {
        spans.push(Span::styled("  ·  ", Style::default().fg(theme::FAINT)));
        spans.push(Span::styled(
            format!("{} {hits}", domain.label()),
            Style::default().fg(theme::domain_color(domain)),
        ));
    }

    spans
}
