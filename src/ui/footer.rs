//! 底部状态栏：左快捷键提示，右状态消息与队列计数。
//!
//! 状态消息按内容宽度占位，剩余空间交给快捷键提示；空间不足时优先保留状态消息。
//!
//! 快捷键按重要性从左到右排列，被截断时先丢掉的是不常用的那几个。

use ratatui::{
    layout::{Constraint, Layout, Rect},
    style::Style,
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph},
};
use unicode_width::UnicodeWidthStr;

use crate::{app::App, ui::theme};

pub fn draw(frame: &mut ratatui::Frame, app: &App, area: Rect) {
    let all_keys = if app.packages.is_some() {
        "打字即过滤 · Space 多选/取消 · Enter 装 · Esc 清空筛选 · Tab 队列 · Ctrl+R 上网搜 · Ctrl+K 检查"
    } else if app.files.is_some() {
        "↑↓ 选择 · Enter 切到该文件所在目录 · 打字过滤 · Esc 关闭"
    } else if app.is_help_open() {
        "↑↓ 滚动 · q 关闭帮助"
    } else if app.running.is_some() {
        "执行中 · q 取消 · 其余按键照常可用"
    } else if app.picker.is_some() {
        "↑↓ 选择 · Enter 选中 · Tab 标记 · ← 上级 · 打字过滤 · Ctrl-U 清空 · Esc 取消"
    } else if app.history.is_some() {
        "↑↓ 选择 · Enter 重跑 · e 回填参数再改 · PgUp/PgDn 翻页 · q 关闭"
    } else if app.viewer.is_some() {
        "↑↓/jk 滚动 · ←→ 横移 · PgUp/PgDn 翻页 · g/G 顶/底 · s 保存 · c 复制 · q 关闭"
    } else if app.form.is_some() {
        "↑↓ 选字段 · ←→ 改选项 · Enter 编辑 · Ctrl-F 选文件 · Ctrl-E 执行 · Esc 返回"
    } else if app.searching {
        "输入搜索 · Enter 确认 · Esc 清除"
    } else {
        "↑↓/jk 选择 · Enter 执行 · / 搜索 · F 文件 · y 浏览 · p 包管理 · f 收藏 · v 视图 · H 历史 · d 目录 · q 退出"
    };

    let queue = match app.marked_count() {
        0 => String::new(),
        count => format!(" · 队列 {count}"),
    };
    // 包管理视图有自己的消息（排队、安装确认、导出结果…），优先显示它 ——
    // 否则那些反馈全在状态里，你看不到（实拍发现的一次自己的疏漏）。
    let message = match app.packages.as_ref() {
        Some(view) if !view.message.is_empty() => view.message.clone(),
        _ => format!("{}{queue}", app.message),
    };

    let block = Block::default()
        .borders(Borders::TOP)
        .border_style(Style::default().fg(theme::BORDER));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let right_width = message_column_width(inner.width, &message);
    let left_width = inner.width.saturating_sub(right_width);
    let columns = Layout::horizontal([
        Constraint::Length(left_width),
        Constraint::Length(right_width),
    ])
    .split(inner);
    let (Some(left), Some(right)) = (columns.first().copied(), columns.get(1).copied()) else {
        return;
    };
    let keys = fit_key_hints(all_keys, left.width.saturating_sub(1) as usize);

    frame.render_widget(
        Paragraph::new(Span::styled(
            format!(" {keys}"),
            Style::default().fg(theme::DIM),
        )),
        left,
    );
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("│ ", Style::default().fg(theme::BORDER)),
            Span::styled(message, Style::default().fg(theme::TEXT)),
        ])),
        right,
    );
}

fn fit_key_hints(hints: &str, width: usize) -> String {
    let mut fitted = String::new();
    for hint in hints.split(" · ") {
        let separator_width = if fitted.is_empty() {
            0
        } else {
            UnicodeWidthStr::width(" · ")
        };
        let needed = separator_width + UnicodeWidthStr::width(hint);
        if UnicodeWidthStr::width(fitted.as_str()) + needed > width {
            break;
        }
        if !fitted.is_empty() {
            fitted.push_str(" · ");
        }
        fitted.push_str(hint);
    }
    fitted
}

fn message_column_width(available: u16, message: &str) -> u16 {
    (UnicodeWidthStr::width(message) + 2).min(available.saturating_sub(12) as usize) as u16
}

#[cfg(test)]
mod tests {
    use super::{fit_key_hints, message_column_width};
    use unicode_width::UnicodeWidthStr;

    #[test]
    fn key_hints_fit_by_terminal_width_without_cutting_a_hint() {
        let hints = "搜索 · Enter执行 · Ctrl+R上网搜";
        let fitted = fit_key_hints(hints, 17);

        assert_eq!(fitted, "搜索 · Enter执行");
        assert!(UnicodeWidthStr::width(fitted.as_str()) <= 17);
    }

    #[test]
    fn status_column_uses_its_text_width_and_leaves_room_for_key_hints() {
        let message = "就绪 · 79 个工具";
        assert_eq!(
            message_column_width(100, message) as usize,
            UnicodeWidthStr::width(message) + 2
        );
        assert_eq!(
            message_column_width(200, message) as usize,
            UnicodeWidthStr::width(message) + 2
        );
        assert_eq!(message_column_width(20, message), 8);
    }
}
