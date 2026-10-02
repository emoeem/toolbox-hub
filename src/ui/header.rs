//! 顶部栏：左标题（Toolbox · Linux 工具箱 · Provider 列表），
//! 右状态（命中/总数、刷新心跳、退出），第二行是搜索框或扫描目录提示。

use ratatui::{
    layout::{Alignment, Constraint, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::Paragraph,
};

use crate::{app::App, ui::theme};

pub fn draw(frame: &mut ratatui::Frame, app: &App, area: Rect) {
    let block = theme::panel("");
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let rows = Layout::vertical([Constraint::Length(1), Constraint::Length(1)]).split(inner);
    let (Some(top), Some(bottom)) = (rows.first().copied(), rows.get(1).copied()) else {
        return;
    };

    // 右侧状态区固定 28 列就够放下「41 / 41   ↻ 0s   q 退出」；
    // 留宽了会挤掉左侧的 Provider 列表（80 列终端下最容易发生）。
    let columns = Layout::horizontal([Constraint::Min(24), Constraint::Length(28)]).split(top);
    let (Some(left), Some(right)) = (columns.first().copied(), columns.get(1).copied()) else {
        return;
    };

    let providers = app.registry.provider_labels().join(" / ");
    let providers = if providers.is_empty() {
        String::from("无 Provider")
    } else {
        providers
    };

    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(" Toolbox ", theme::title_style()),
            Span::styled("· ", Style::default().fg(theme::DIM)),
            Span::styled("Linux 工具箱", Style::default().fg(theme::TEXT)),
            Span::styled("  ·  ", Style::default().fg(theme::FAINT)),
            Span::styled(providers, Style::default().fg(theme::DIM)),
        ])),
        left,
    );

    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(
                format!("{} / {}", app.filtered.len(), app.registry.len()),
                Style::default()
                    .fg(theme::CYAN)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled("   ", Style::default()),
            Span::styled(
                format!("↻ {}s", app.seconds_since_reload()),
                Style::default().fg(theme::FAINT),
            ),
            Span::styled("   ", Style::default()),
            // 软件包中心里 `q` 是**打字**（输入框常驻），所以那儿只能说 Esc
            Span::styled(
                if app.packages.is_some() {
                    "Esc 退出"
                } else {
                    "q 退出"
                },
                Style::default().fg(theme::DIM),
            ),
        ]))
        .alignment(Alignment::Right),
        right,
    );

    let second = if app.is_editing_dir() {
        // 正在改工作目录：这一行本身就是输入框。
        Line::from(vec![
            Span::styled("  工作目录 → ", Style::default().fg(theme::CYAN)),
            Span::styled(
                format!("{}▌", app.dir_input_text()),
                Style::default().fg(theme::TEXT),
            ),
            Span::styled(
                "   Enter 确认 · Ctrl-U 清空 · Esc 取消",
                Style::default().fg(theme::DIM),
            ),
        ])
    } else if app.history.is_some() {
        Line::from(vec![
            Span::styled("  执行历史", Style::default().fg(theme::DIM)),
            Span::styled("  ·  ", Style::default().fg(theme::FAINT)),
            Span::styled("Enter 重跑", theme::title_style()),
            Span::styled("  ·  ", Style::default().fg(theme::FAINT)),
            Span::styled("q 关闭", Style::default().fg(theme::DIM)),
        ])
    } else if app.viewer.is_some() {
        Line::from(vec![
            Span::styled("  输出视图", Style::default().fg(theme::DIM)),
            Span::styled("  ·  ", Style::default().fg(theme::FAINT)),
            Span::styled("←→ 横向滚动", Style::default().fg(theme::DIM)),
            Span::styled("  ·  ", Style::default().fg(theme::FAINT)),
            Span::styled("q 关闭", theme::title_style()),
        ])
    } else if app.form.is_some() {
        Line::from(vec![
            Span::styled("  填写参数", Style::default().fg(theme::DIM)),
            Span::styled("  ·  ", Style::default().fg(theme::FAINT)),
            Span::styled("Ctrl-E 执行", theme::title_style()),
            Span::styled("  ·  ", Style::default().fg(theme::FAINT)),
            Span::styled("Esc 返回列表", Style::default().fg(theme::DIM)),
        ])
    } else if app.searching {
        Line::from(vec![
            Span::styled("  / ", theme::title_style()),
            Span::styled(
                app.query.text().to_string(),
                Style::default().fg(theme::CYAN),
            ),
            Span::styled("▌", Style::default().fg(theme::CYAN)),
        ])
    } else {
        Line::from(vec![
            Span::styled(
                format!("  工作目录 {} ", crate::ui::short_path(&app.work_dir)),
                Style::default().fg(theme::CYAN),
            ),
            // 这一句直接回答「脚本为什么说没有文件」。
            Span::styled(
                format!("· {} ", app.media_label()),
                if app.media.files.is_empty() {
                    Style::default().fg(theme::YELLOW)
                } else {
                    Style::default().fg(theme::GREEN)
                },
            ),
            Span::styled("· d 改目录", Style::default().fg(theme::DIM)),
            Span::styled("  ·  ", Style::default().fg(theme::FAINT)),
            Span::styled(
                format!("扫描 {}", crate::ui::short_path(&app.bin_dir)),
                Style::default().fg(theme::DIM),
            ),
        ])
    };
    frame.render_widget(Paragraph::new(second), bottom);
}
