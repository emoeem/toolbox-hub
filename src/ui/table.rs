//! 工具表格。
//!
//! 列变化（相对第一版）：把「分类」拆成 `Provider` + `分类`，
//! 因为进入两级模型后，同一张表里可能同时出现不同 Provider 的工具。

use ratatui::{
    layout::{Constraint, Rect},
    style::{Modifier, Style},
    widgets::{Cell, Paragraph, Row, Table, TableState, Wrap},
};

use crate::{
    app::{App, Scope},
    ui::{theme, window},
};

pub fn draw(frame: &mut ratatui::Frame, app: &mut App, area: Rect) {
    if app.filtered.is_empty() {
        draw_empty(frame, app, area);
        return;
    }

    let compact = area.width < 72;
    let headers = if compact {
        vec!["工具", "域", "状态", "说明"]
    } else {
        vec!["工具", "域", "Provider", "分类", "状态", "说明"]
    };
    let header =
        Row::new(headers).style(Style::default().fg(theme::DIM).add_modifier(Modifier::BOLD));

    // 只构造看得见的那些行（见 ui::window 的说明）。选中是全局下标，
    // 高亮行用窗口内的相对下标。
    let rows_room = crate::ui::main_rows_room(area);
    let selected = app.selected.min(app.filtered.len() - 1);
    let (start, end) = window(app.filtered.len(), selected, rows_room);
    let rows = app.filtered[start..end].iter().map(|&index| {
        let tool = &app.registry.tools()[index];
        let mark = if app.marked[index] { "● " } else { "  " };
        // 星标固定占两格，收藏与否都不会让工具名错位。
        let star = if app.is_favorite(&tool.id) {
            "★ "
        } else {
            "  "
        };
        let status_style = if tool.ready {
            Style::default().fg(theme::GREEN)
        } else {
            Style::default().fg(theme::YELLOW)
        };

        let mut cells = vec![
            Cell::from(format!("{mark}{star}{}", tool.name)),
            // 跨域搜索时结果可能来自任何域，这一列让「这条是哪个域的」仍然一眼可见。
            Cell::from(tool.domain.label())
                .style(Style::default().fg(theme::domain_color(tool.domain))),
        ];
        if !compact {
            let tags = if tool.tags.is_empty() {
                "-".to_string()
            } else {
                tool.tags.join("/")
            };
            let tag_color = tool
                .tags
                .first()
                .map_or(theme::PURPLE, |tag| theme::tag_color(tag));
            cells.push(Cell::from(tool.provider.clone()).style(Style::default().fg(theme::DIM)));
            cells.push(Cell::from(tags).style(Style::default().fg(tag_color)));
        }
        cells.push(Cell::from(tool.status_label()).style(status_style));
        cells.push(Cell::from(tool.summary.clone()).style(Style::default().fg(theme::DIM)));
        Row::new(cells).height(1)
    });

    let widths = if compact {
        vec![
            Constraint::Percentage(45),
            Constraint::Length(6),
            Constraint::Length(13),
            Constraint::Min(8),
        ]
    } else {
        vec![
            Constraint::Percentage(24),
            Constraint::Length(6),
            Constraint::Length(10),
            Constraint::Length(8),
            Constraint::Length(13),
            Constraint::Min(16),
        ]
    };
    let table = Table::new(rows, widths)
        .header(header)
        .block(theme::panel(" 工具 "))
        .row_highlight_style(Style::default().bg(theme::HIGHLIGHT).fg(theme::TEXT))
        .highlight_symbol("▸ ");

    let mut state = TableState::default().with_selected(Some(selected - start));
    frame.render_stateful_widget(table, area, &mut state);
}

/// 空视图不是错误：可能是域没接 Provider、分类下没工具，或者搜索没命中。
fn draw_empty(frame: &mut ratatui::Frame, app: &App, area: Rect) {
    let domain = app.current_domain();
    let query = app.query.text().trim().to_string();

    let message = if app.scope == Scope::Favorites && app.favorite_count() == 0 {
        String::from("还没有收藏任何工具。\n在列表里选中一件，按 f 收藏；再按 v 可以切回全部工具。")
    } else if app.scope == Scope::Favorites {
        String::from("这个视图下没有匹配的收藏（试试清掉搜索词，或按 v 切换视图）。")
    } else if app.scope == Scope::Recent {
        String::from("最近还没有跑过工具。\n执行一次之后，这里会按使用先后列出它们。")
    } else if app.registry.is_empty() {
        // 一个工具都没有：问题不在筛选，而在扫描目录或 Provider 注册。
        format!(
            "没有发现任何工具。\n请检查扫描目录：{}\n以及 providers 模块里注册的 Provider。",
            app.bin_dir.display()
        )
    } else if !query.is_empty() {
        format!("没有匹配「{query}」的工具。")
    } else if app.registry.tool_count_in(domain) == 0 {
        format!(
            "{} 域暂无工具 · Provider 待接入\n{}",
            domain.label(),
            domain.description()
        )
    } else if let Some(tag) = app.sub_filter() {
        format!("分类「{tag}」下没有工具。")
    } else {
        format!("{} 域下没有工具。", domain.label())
    };

    frame.render_widget(
        Paragraph::new(message)
            .style(Style::default().fg(theme::DIM))
            .wrap(Wrap { trim: true })
            .block(theme::panel(" 工具 ")),
        area,
    );
}
