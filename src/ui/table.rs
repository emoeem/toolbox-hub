//! 工具表格。
//!
//! 列变化（相对第一版）：把「分类」拆成 `Provider` + `分类`，
//! 因为进入两级模型后，同一张表里可能同时出现不同 Provider 的工具。

use ratatui::{
    layout::{Constraint, Rect},
    style::{Modifier, Style},
    widgets::{Cell, Paragraph, Row, Table, Wrap},
};

use crate::{
    app::{App, Scope},
    ui::theme,
};

pub fn draw(frame: &mut ratatui::Frame, app: &mut App, area: Rect) {
    if app.filtered.is_empty() {
        draw_empty(frame, app, area);
        return;
    }

    let header = Row::new(vec![
        Cell::from("工具"),
        Cell::from("域"),
        Cell::from("Provider"),
        Cell::from("分类"),
        Cell::from("状态"),
        Cell::from("说明"),
    ])
    .style(Style::default().fg(theme::DIM).add_modifier(Modifier::BOLD));

    let rows = app.filtered.iter().map(|&index| {
        let tool = &app.registry.tools()[index];
        let mark = if app.marked[index] { "● " } else { "  " };
        // 星标固定占两格，收藏与否都不会让工具名错位。
        let star = if app.is_favorite(&tool.id) {
            "★ "
        } else {
            "  "
        };
        let tags = if tool.tags.is_empty() {
            "-".to_string()
        } else {
            tool.tags.join("/")
        };
        let tag_color = tool
            .tags
            .first()
            .map_or(theme::PURPLE, |tag| theme::tag_color(tag));
        let status_style = if tool.ready {
            Style::default().fg(theme::GREEN)
        } else {
            Style::default().fg(theme::YELLOW)
        };

        Row::new(vec![
            Cell::from(format!("{mark}{star}{}", tool.name)),
            // 跨域搜索时结果可能来自任何域，这一列让「这条是哪个域的」仍然一眼可见。
            Cell::from(tool.domain.label())
                .style(Style::default().fg(theme::domain_color(tool.domain))),
            Cell::from(tool.provider.clone()).style(Style::default().fg(theme::DIM)),
            Cell::from(tags).style(Style::default().fg(tag_color)),
            Cell::from(tool.status_label()).style(status_style),
            Cell::from(tool.summary.clone()).style(Style::default().fg(theme::DIM)),
        ])
        .height(1)
    });

    let table = Table::new(
        rows,
        [
            Constraint::Percentage(24),
            Constraint::Length(6),
            Constraint::Length(10),
            Constraint::Length(8),
            Constraint::Length(13),
            Constraint::Min(16),
        ],
    )
    .header(header)
    .block(theme::panel(" 工具 "))
    .row_highlight_style(Style::default().bg(theme::HIGHLIGHT).fg(theme::TEXT))
    .highlight_symbol("▸ ");

    frame.render_stateful_widget(table, area, &mut app.table);
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
