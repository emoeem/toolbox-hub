//! 文件选择器：在目录里挑文件，填进表单的路径字段。
//!
//! 占的是表单那块地方（它本来就是从表单里打开的）。

use ratatui::{
    layout::{Constraint, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Cell, Paragraph, Row, Table, TableState, Wrap},
};

use crate::{
    app::Picker,
    ui::{theme, window},
};

/// 画文件选择器。
///
/// 只依赖 [`Picker`] 而不是整个 `App` —— 这样 `toolbox-hub ui pick` 那个
/// 给脚本用的独立组件能用同一份渲染（见 `crate::components`）。
pub fn draw(frame: &mut ratatui::Frame, picker: &Picker, area: Rect) {
    let block = theme::panel(" 选文件 ");
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let rows = Layout::vertical([Constraint::Length(2), Constraint::Min(1)]).split(inner);
    let (Some(head), Some(list)) = (rows.first().copied(), rows.get(1).copied()) else {
        return;
    };

    let filter_line = if picker.filter.is_empty() {
        Span::styled("（直接打字就能过滤）", Style::default().fg(theme::FAINT))
    } else {
        Span::styled(
            format!("{}▌", picker.filter.text()),
            Style::default().fg(theme::TEXT),
        )
    };

    let mut status = vec![
        Span::styled("过滤  ", Style::default().fg(theme::DIM)),
        filter_line,
        Span::styled(
            format!("   {} 项", picker.len()),
            Style::default().fg(theme::FAINT),
        ),
    ];
    if picker.marked_count() > 0 {
        status.push(Span::styled(
            format!("   · 已标记 {} 个", picker.marked_count()),
            Style::default().fg(theme::GREEN),
        ));
    }
    if picker.dir_only {
        status.push(Span::styled(
            "   · 这个字段要填目录：Ctrl-D 用当前目录",
            Style::default().fg(theme::CYAN),
        ));
    }

    frame.render_widget(
        Paragraph::new(Text::from(vec![
            Line::from(vec![
                Span::styled("目录  ", Style::default().fg(theme::DIM)),
                Span::styled(
                    picker.dir().display().to_string(),
                    Style::default().fg(theme::CYAN),
                ),
            ]),
            Line::from(status),
        ])),
        head,
    );

    if picker.is_empty() {
        frame.render_widget(
            Paragraph::new("这个目录里没有匹配的文件。\n← 回上一层，或者删掉过滤词。")
                .style(Style::default().fg(theme::DIM))
                .wrap(Wrap { trim: true }),
            list,
        );
        return;
    }

    // 窗口化：每帧只 stat 看得见的那 ~30 行，而不是整个目录。
    let (start, end) = window(picker.len(), picker.selected, list.height);
    let rows = (start..end)
        .filter_map(|index| picker.entry(index))
        .map(|entry| {
            // 目录加个斜杠、用另一种颜色，一眼能和文件分开。
            // 标记占固定的两格，标不标记都不会让文件名错位。
            let mark = if picker.is_marked(&entry.path) {
                "● "
            } else {
                "  "
            };
            let (label, style) = if entry.is_dir {
                (
                    format!("{}/", entry.name),
                    Style::default()
                        .fg(theme::CYAN)
                        .add_modifier(Modifier::BOLD),
                )
            } else {
                (entry.name.clone(), Style::default().fg(theme::TEXT))
            };

            Row::new(vec![
                Cell::from(format!("{mark}{label}")).style(style),
                Cell::from(entry.size_label()).style(Style::default().fg(theme::FAINT)),
            ])
            .height(1)
        });

    let table = Table::new(rows, [Constraint::Min(20), Constraint::Length(10)])
        .row_highlight_style(theme::selected_row())
        .highlight_symbol("➤ ");

    let mut state =
        TableState::default().with_selected(Some(picker.selected.saturating_sub(start)));
    frame.render_stateful_widget(table, list, &mut state);
}
