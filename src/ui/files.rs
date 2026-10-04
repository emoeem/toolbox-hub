//! 文件视图：工作目录里的媒体文件。
//!
//! 回答的是一个很具体的问题：「脚本说没有文件，那这个目录里到底有什么？」
//! 用的规则和脚本的 `fd` 调用完全一致（见 [`crate::media`]），所以这里显示什么，
//! 脚本就看到什么。

use ratatui::{
    layout::{Constraint, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Cell, Paragraph, Row, Table, TableState, Wrap},
};

use ratatui_image::{StatefulImage, protocol::StatefulProtocol};

use crate::{
    app::App,
    media::human_size,
    preview::Preview,
    ui::{short_path, theme, window},
};

pub fn draw(frame: &mut ratatui::Frame, app: &mut App, area: Rect) {
    let Some(view) = app.files.as_ref() else {
        return;
    };

    let block = theme::panel(" 文件 ");
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let rows = Layout::vertical([Constraint::Length(2), Constraint::Min(1)]).split(inner);
    let (Some(head), Some(body)) = (rows.first().copied(), rows.get(1).copied()) else {
        return;
    };

    // 右边分一块给预览：只在够宽够高时才分（窄终端下把列表挤成一条更糟）
    let preview_here = Preview::worth_showing(body.width / 2, body.height);
    let (list, preview_area) = if preview_here {
        let columns = Layout::horizontal([Constraint::Percentage(55), Constraint::Percentage(45)])
            .split(body);
        (columns[0], Some(columns[1]))
    } else {
        (body, None)
    };

    let mut status = vec![
        Span::styled("目录  ", Style::default().fg(theme::DIM)),
        Span::styled(short_path(&view.root), Style::default().fg(theme::CYAN)),
        Span::styled(
            format!("   {} 个媒体文件", view.total()),
            Style::default()
                .fg(theme::TEXT)
                .add_modifier(Modifier::BOLD),
        ),
    ];
    if view.truncated {
        status.push(Span::styled(
            "（只数了前 500 个）",
            Style::default().fg(theme::YELLOW),
        ));
    }

    let filter_line = if view.filter.is_empty() {
        Span::styled("（直接打字就能过滤）", Style::default().fg(theme::FAINT))
    } else {
        Span::styled(
            format!("{}▌", view.filter),
            Style::default().fg(theme::TEXT),
        )
    };

    frame.render_widget(
        Paragraph::new(Text::from(vec![
            Line::from(status),
            Line::from(vec![
                Span::styled("过滤  ", Style::default().fg(theme::DIM)),
                filter_line,
                Span::styled(
                    format!("   {} 项", view.len()),
                    Style::default().fg(theme::FAINT),
                ),
            ]),
        ])),
        head,
    );

    if view.is_empty() {
        let text = if view.total() == 0 {
            "这个目录（含子目录）里没有媒体文件。\n脚本也会找不到东西 —— 按 d 换成真正放素材的目录，或者按 Esc 回车列表用 Enter 跑脚本。"
        } else {
            "没有匹配的文件。删掉过滤词试试。"
        };
        frame.render_widget(
            Paragraph::new(text)
                .style(Style::default().fg(theme::YELLOW))
                .wrap(Wrap { trim: true }),
            list,
        );
        return;
    }

    // 只构造看得见的那些行（见 ui::window）：目录里上万条时每帧重造一遍会把滚动拖涩。
    let (start, end) = window(view.len(), view.selected, list.height);
    let rows = (start..end)
        .filter_map(|index| view.entry(index))
        .map(|file| {
            Row::new(vec![
                Cell::from(file.name.clone()).style(Style::default().fg(theme::TEXT)),
                Cell::from(human_size(file.size)).style(Style::default().fg(theme::FAINT)),
                Cell::from(file.relative.clone()).style(Style::default().fg(theme::DIM)),
            ])
            .height(1)
        });

    let table = Table::new(
        rows,
        [
            Constraint::Min(18),
            Constraint::Length(10),
            Constraint::Min(20),
        ],
    )
    .row_highlight_style(theme::selected_row())
    .highlight_symbol("➤ ");

    let mut state = TableState::default().with_selected(Some(view.selected.saturating_sub(start)));
    frame.render_stateful_widget(table, list, &mut state);

    if let Some(area) = preview_area {
        draw_preview(frame, app, area);
    }
}

/// 右半边：图片预览。
///
/// 三种情况都要说人话：能画就画；在解码就说「解码中」；不是图片就说清楚
/// 「这个类型不预览」—— 免得看着一块空白以为坏了。
fn draw_preview(frame: &mut ratatui::Frame, app: &mut App, area: Rect) {
    let selected = app
        .files
        .as_ref()
        .and_then(|view| view.entry(view.selected))
        .map(|file| (file.name.clone(), file.path.clone()));

    let Some((name, path)) = selected else {
        return;
    };

    // 选中项一变就登记一次（内部会去重 + 后台解码）
    app.preview.request(&path);

    let title = format!(" 预览 · {} ", app.preview.protocol());
    let block = theme::panel(&title);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    if let Some(protocol) = app.preview.protocol_state() {
        // 图已经好了：把它画进去（StatefulImage 自己按区域缩放）
        // 泛型要写出来：StatefulImage<T> 的 T 单靠 new() 推不出来
        frame.render_stateful_widget(StatefulImage::<StatefulProtocol>::new(), inner, protocol);
        return;
    }

    let hint = if app.preview.is_decoding() {
        format!(
            "解码中…
{name}"
        )
    } else if let Some(problem) = app.preview.problem.clone() {
        format!(
            "看不了这张：
{problem}"
        )
    } else if crate::preview::looks_like_image(&path) {
        String::from("等一下，正在准备预览")
    } else {
        format!(
            "{name}

这个类型不预览（只画图片）。\n要看内容就用别的工具打开它。"
        )
    };

    frame.render_widget(
        Paragraph::new(hint)
            .style(Style::default().fg(theme::FAINT))
            .wrap(Wrap { trim: true }),
        inner,
    );
}
