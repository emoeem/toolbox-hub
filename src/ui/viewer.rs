//! 内置输出视图：捕获模式的命令跑完后在这里回看输出。
//!
//! 刻意**不换行**：换行后「滚了多少行」和「看到多少行」就对不上了，
//! 末页会滚不到。宁可把超长行裁掉，也要让滚动是准的（横向滚动以后再加）。

use ratatui::{
    layout::{Constraint, Layout, Rect},
    style::Style,
    text::{Line, Span, Text},
    widgets::Paragraph,
};

use crate::{app::App, ui::theme};

pub fn draw(frame: &mut ratatui::Frame, app: &App, area: Rect) {
    let Some(viewer) = app.viewer.as_ref() else {
        return;
    };

    let block = theme::panel(" 输出 ");
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let rows = Layout::vertical([Constraint::Length(2), Constraint::Min(1)]).split(inner);
    let (Some(head), Some(body)) = (rows.first().copied(), rows.get(1).copied()) else {
        return;
    };

    let total = viewer.body.lines().count();
    let visible = body.height as usize;
    let max_scroll = total.saturating_sub(visible);
    let scroll = viewer.scroll.min(max_scroll);

    frame.render_widget(
        Paragraph::new(Text::from(vec![
            Line::from(vec![
                Span::styled("$ ", Style::default().fg(theme::FAINT)),
                Span::styled(viewer.title.clone(), Style::default().fg(theme::CYAN)),
            ]),
            Line::from(vec![
                Span::styled(viewer.status.clone(), Style::default().fg(theme::DIM)),
                Span::styled(
                    format!(
                        "      {} 行   第 {}-{} 行",
                        total,
                        if total == 0 { 0 } else { scroll + 1 },
                        (scroll + visible).min(total)
                    ),
                    Style::default().fg(theme::FAINT),
                ),
            ]),
        ])),
        head,
    );

    frame.render_widget(
        Paragraph::new(viewer.body.clone())
            .scroll((scroll as u16, 0))
            .style(Style::default().fg(theme::TEXT)),
        body,
    );
}
