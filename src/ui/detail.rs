//! 当前工具详情面板。

use ratatui::{
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Paragraph, Wrap},
};

use crate::{app::App, ui::theme};

pub fn draw(frame: &mut ratatui::Frame, app: &App, area: Rect) {
    let Some(tool) = app.current() else {
        draw_placeholder(frame, app, area);
        return;
    };

    let status = Span::styled(
        tool.status_label(),
        Style::default().fg(if tool.ready {
            theme::GREEN
        } else {
            theme::YELLOW
        }),
    );

    let mut lines = vec![
        Line::from(vec![
            Span::styled(
                &tool.name,
                Style::default()
                    .fg(theme::PURPLE)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw("   "),
            Span::styled(
                tool.domain.label(),
                Style::default().fg(theme::domain_color(tool.domain)),
            ),
            Span::raw("   "),
            Span::styled(tool.provider.clone(), Style::default().fg(theme::DIM)),
            Span::raw("   "),
            status,
        ]),
        Line::from(vec![
            Span::styled("功能  ", Style::default().fg(theme::DIM)),
            Span::styled(tool.summary.clone(), Style::default().fg(theme::TEXT)),
        ]),
        Line::from(vec![
            Span::styled("输入  ", Style::default().fg(theme::DIM)),
            Span::styled(
                tool.input_or_default().to_string(),
                Style::default().fg(theme::TEXT),
            ),
            Span::styled("    输出  ", Style::default().fg(theme::DIM)),
            Span::styled(
                tool.output_or_default().to_string(),
                Style::default().fg(theme::TEXT),
            ),
        ]),
        Line::from(vec![
            Span::styled("特性  ", Style::default().fg(theme::DIM)),
            Span::styled(
                tool.features_or_summary().to_string(),
                Style::default().fg(theme::CYAN),
            ),
        ]),
        Line::from(vec![
            Span::styled("依赖  ", Style::default().fg(theme::DIM)),
            Span::styled(tool.deps_label(), Style::default().fg(theme::CYAN)),
        ]),
        Line::from(vec![
            Span::styled("路径  ", Style::default().fg(theme::DIM)),
            Span::styled(
                tool.path.display().to_string(),
                Style::default().fg(theme::DIM),
            ),
            Span::styled("    ID  ", Style::default().fg(theme::DIM)),
            Span::styled(tool.id.clone(), Style::default().fg(theme::FAINT)),
        ]),
    ];

    // 只有缺依赖时才多画这一行：直接告诉用户怎么把缺的装上。
    if let Some(hint) = tool.install_label() {
        lines.push(Line::from(vec![
            Span::styled("安装  ", Style::default().fg(theme::DIM)),
            Span::styled(hint, Style::default().fg(theme::YELLOW)),
        ]));
    }

    frame.render_widget(
        Paragraph::new(Text::from(lines))
            .wrap(Wrap { trim: true })
            .block(theme::panel(" 当前工具 ")),
        area,
    );
}

/// 没有可展示的工具时，说明「为什么空」而不是留一片空白。
fn draw_placeholder(frame: &mut ratatui::Frame, app: &App, area: Rect) {
    let domain = app.current_domain();
    let text = if !app.query.text().trim().is_empty() {
        format!("搜索「{}」没有命中。", app.query.text().trim())
    } else if app.registry.tool_count_in(domain) == 0 {
        format!(
            "{} · {}\n该域的 Provider 尚未接入。",
            domain.label(),
            domain.description()
        )
    } else {
        String::from("没有匹配的工具。")
    };

    frame.render_widget(
        Paragraph::new(text)
            .style(Style::default().fg(theme::DIM))
            .wrap(Wrap { trim: true })
            .block(theme::panel(" 当前工具 ")),
        area,
    );
}
