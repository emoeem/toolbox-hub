//! 参数表单：把 [`crate::model::Action`] 的字段画成可填的表格，并预览真正会跑的命令。
//!
//! 表单是**模式**：打开期间它顶掉原来的表格与详情区（见 [`crate::ui::draw`]）。
//! 这样不用为表单另造一套布局，字段多的时候还能自动占用更多行。

use ratatui::{
    layout::{Constraint, Rect},
    style::{Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Cell, Paragraph, Row, Table, TableState, Wrap},
};

use crate::{app::App, model::ArgKind, ui::theme};

/// 字段列表：占原来表格的位置。
pub fn draw_fields(frame: &mut ratatui::Frame, app: &App, area: Rect) {
    let block = theme::panel(" 填写参数 ");
    let Some(form) = app.form.as_ref() else {
        frame.render_widget(Paragraph::new("没有打开的表单").block(block), area);
        return;
    };

    let header = Row::new(vec![
        Cell::from("字段"),
        Cell::from("值"),
        Cell::from("说明"),
    ])
    .style(Style::default().fg(theme::DIM).add_modifier(Modifier::BOLD));

    let rows = app
        .form_arguments()
        .iter()
        .enumerate()
        .map(|(index, argument)| {
            let editing = index == form.field && form.editing;
            let raw = form.values.get(&argument.key).unwrap_or("");

            let value = match argument.kind {
                ArgKind::Toggle => {
                    if form.values.is_on(&argument.key) {
                        "✓ 打开".to_string()
                    } else {
                        "✗ 关闭".to_string()
                    }
                }
                // 用箭头暗示「这里可以左右换」。
                ArgKind::Choice => format!("◀ {} ▶", argument.choice_label(raw)),
                ArgKind::Text | ArgKind::Path => {
                    if editing {
                        format!("{raw}▌")
                    } else if raw.is_empty() {
                        String::from("（空）")
                    } else {
                        raw.to_string()
                    }
                }
            };

            let value_style = match argument.kind {
                ArgKind::Toggle => {
                    if form.values.is_on(&argument.key) {
                        Style::default().fg(theme::GREEN)
                    } else {
                        Style::default().fg(theme::FAINT)
                    }
                }
                ArgKind::Choice => Style::default().fg(theme::CYAN),
                ArgKind::Text | ArgKind::Path => {
                    if editing || (raw.is_empty() && argument.required) {
                        Style::default().fg(theme::YELLOW)
                    } else {
                        Style::default().fg(theme::TEXT)
                    }
                }
            };

            let label = if argument.required {
                format!("{} *", argument.label)
            } else {
                argument.label.clone()
            };

            Row::new(vec![
                Cell::from(label),
                Cell::from(value).style(value_style),
                Cell::from(argument.help.clone().unwrap_or_default())
                    .style(Style::default().fg(theme::DIM)),
            ])
            .height(1)
        });

    let table = Table::new(
        rows,
        [
            Constraint::Length(14),
            Constraint::Percentage(42),
            Constraint::Min(20),
        ],
    )
    .header(header)
    .block(block)
    .row_highlight_style(Style::default().bg(theme::HIGHLIGHT).fg(theme::TEXT))
    .highlight_symbol("▸ ");

    // 高亮完全由 `form.field` 决定，所以每帧现造一个 TableState 就够了。
    let mut state = TableState::default().with_selected(Some(form.field));
    frame.render_stateful_widget(table, area, &mut state);
}

/// 命令预览：占原来详情区的位置。
///
/// 预览与执行共用 [`App::form_build`]，所以屏幕上看到的就是真正会跑的命令。
pub fn draw_preview(frame: &mut ratatui::Frame, app: &App, area: Rect) {
    let mut lines: Vec<Line> = Vec::new();

    if let Some(tool) = app.form_tool() {
        lines.push(Line::from(vec![
            Span::styled(
                tool.name.clone(),
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
        ]));
        lines.push(Line::from(vec![
            Span::styled("说明  ", Style::default().fg(theme::DIM)),
            Span::styled(tool.summary.clone(), Style::default().fg(theme::TEXT)),
        ]));
    }

    match app.form_build() {
        Ok(argv) => {
            let program = app
                .form_tool()
                .map(|tool| tool.path.display().to_string())
                .unwrap_or_default();
            lines.push(Line::from(vec![
                Span::styled("命令  ", Style::default().fg(theme::DIM)),
                Span::styled(
                    format!("{program} {}", argv.join(" ")),
                    Style::default().fg(theme::GREEN),
                ),
            ]));
            lines.push(Line::from(Span::styled(
                format!(
                    "      共 {} 个参数元素，逐个原样传给程序，不经过 shell",
                    argv.len()
                ),
                Style::default().fg(theme::FAINT),
            )));
            lines.push(Line::from(vec![
                Span::styled("目录  ", Style::default().fg(theme::DIM)),
                Span::styled(
                    crate::ui::short_path(&app.work_dir),
                    Style::default().fg(theme::CYAN),
                ),
                Span::styled(
                    "   （工具在这个目录里执行）",
                    Style::default().fg(theme::FAINT),
                ),
            ]));
        }
        Err(message) => {
            lines.push(Line::from(vec![
                Span::styled("还差  ", Style::default().fg(theme::DIM)),
                Span::styled(message, Style::default().fg(theme::YELLOW)),
            ]));
        }
    }

    if let Some(note) = app.form_danger_note() {
        let suffix = if app.form_needs_confirm() {
            "（第一次 Ctrl-E 只是确认）"
        } else {
            "（已确认：再按一次 Ctrl-E 执行）"
        };
        lines.push(Line::from(vec![
            Span::styled(note, Style::default().fg(theme::YELLOW)),
            Span::styled(suffix, Style::default().fg(theme::FAINT)),
        ]));
    }

    // 上面「还差 …」那行已经在说同一个原因了，就不重复一遍。
    if let Some(error) = app.form_error()
        && app.form_build().err().as_deref() != Some(error)
    {
        lines.push(Line::from(vec![
            Span::styled("错误  ", Style::default().fg(theme::DIM)),
            Span::styled(error.to_string(), Style::default().fg(theme::YELLOW)),
        ]));
    }

    lines.push(Line::from(Span::styled(
        "Ctrl-E 执行 · Esc 返回列表",
        Style::default().fg(theme::DIM),
    )));

    frame.render_widget(
        Paragraph::new(Text::from(lines))
            .wrap(Wrap { trim: true })
            .block(theme::panel(" 将要执行 ")),
        area,
    );
}
