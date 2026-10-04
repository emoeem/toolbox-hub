//! 内置输出视图：捕获模式的命令跑完后在这里回看输出。
//!
//! 刻意**不换行**：换行后「滚了多少行」和「看到多少行」就对不上了，末页会滚不到。
//! 代价是超长行会被裁掉 —— 用 `←→` 横向滚动来看（按最长行夹紧，见
//! [`crate::app::Viewer::scroll_horizontal_by`]）。

use ratatui::{
    layout::{Constraint, Layout, Rect},
    style::Style,
    text::{Line, Span, Text},
    widgets::Paragraph,
};

use crate::{app::Viewer, ui::theme};

/// 画输出视图。
///
/// 只依赖 [`Viewer`] 而不是整个 `App` —— 这样 `toolbox-hub ui pager` 那个
/// 给脚本用的独立组件能用同一份渲染（见 `crate::components`）。
pub fn draw(frame: &mut ratatui::Frame, viewer: &Viewer, area: Rect) {
    let block = theme::panel(" 输出 ");
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let rows = Layout::vertical([Constraint::Length(2), Constraint::Min(1)]).split(inner);
    let (Some(head), Some(body)) = (rows.first().copied(), rows.get(1).copied()) else {
        return;
    };

    let total = viewer.lines;
    let visible = body.height as usize;
    let max_scroll = total.saturating_sub(visible);
    let scroll = viewer.scroll.min(max_scroll);

    // 横向滚过就报出当前列号：不然光标停在右边界，看着和没滚一样。
    let column = if viewer.horizontal_scroll > 0 {
        format!("   第 {} 列", viewer.horizontal_scroll + 1)
    } else {
        String::new()
    };

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
                        "      {} 行   第 {}-{} 行{column}",
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

    // 借用而不是 clone：正文是整段捕获输出，几百 KB 也不稀奇，每帧克隆一次
    // 等于每帧白烧一份内存（Paragraph 只需要 Into<Text>，&str 就够）。
    frame.render_widget(
        Paragraph::new(viewer.body.as_str())
            .scroll((scroll as u16, viewer.horizontal_scroll))
            .style(Style::default().fg(theme::TEXT)),
        body,
    );
}
