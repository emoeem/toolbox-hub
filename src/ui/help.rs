//! 帮助屏：把所有模式的键位列全。
//!
//! 底部那一行只放最常用的几个，100 列还会被裁 —— 现在 6 个模式、30 多个键，
//! 需要有个能查的地方。**这里是键位的唯一权威**，改键位就改这里。

use ratatui::{
    layout::{Constraint, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, Borders, Clear, Paragraph, Wrap},
};

use crate::{app::App, ui::theme};

/// 帮助内容：一个模式一段。
const SECTIONS: &[(&str, &[(&str, &str)])] = &[
    (
        "列表",
        &[
            ("↑↓ / j k", "选择工具"),
            ("Enter", "执行（要参数的工具会先打开表单）"),
            ("/", "搜索（跨所有域）"),
            ("←→ / 1-7", "切域 · 数字直达"),
            ("h l / [ ]", "切页内分类"),
            ("Tab / Space", "标记（批量执行）"),
            ("f", "收藏 / 取消收藏"),
            ("v", "切换视图：全部 → ★收藏 → 最近使用"),
            ("H", "执行历史（进去按 Enter 可重跑）"),
            ("F", "看工作目录里的媒体文件（脚本看到的就是这些）"),
            ("y", "在文件管理器（yazi）里逛 · 回来工作目录跟着走"),
            ("p", "原生包管理：搜官方源 + AUR、看信息、排队、安装"),
            ("d", "改工作目录（工具在这里执行、找文件）"),
            ("Ctrl-R", "重新扫描工具"),
            ("?", "这份帮助"),
            ("q / Esc", "退出"),
        ],
    ),
    (
        "包管理（p）",
        &[
            ("打字 + Enter", "搜官方源 + AUR（两路并行）"),
            ("↑↓ / PgUp PgDn", "选择"),
            ("打字", "**本地模糊过滤**已有结果（键入即筛，fzf 的手感）"),
            ("Enter", "上网搜（官方源 + AUR 两路并行）"),
            ("1-9 / 0", "开关仓库标签 / 全开"),
            ("s", "排序：相关度 → 名字 → 仓库 → 得票"),
            ("Space", "加入安装队列（再按移出）"),
            ("Tab", "结果 ⇄ 队列"),
            ("Enter", "装队列里的包（按两次；sudo 在终端里输）"),
            ("Ctrl+X", "看 AUR 的 PKGBUILD"),
            (
                "Ctrl+K",
                "PKGBUILD 检查：shellcheck + namcap（没装的会说明）",
            ),
            (
                "Ctrl+A",
                "借 pac 做 AUR 的 AI 审查（用它配好的供应商，只查不装）",
            ),
            ("Ctrl+N", "看 Arch 新闻（含升级后的未读）"),
            ("Ctrl+E / Ctrl+I", "导出 / 导入队列"),
            ("Ctrl+D", "清空队列"),
            ("Ctrl+R", "重搜"),
            ("o", "浏览器打开 AUR 页面（评论/投票要你的登录）"),
            ("Esc / q", "返回列表"),
        ],
    ),
    (
        "文件视图（F）",
        &[
            ("↑↓ / PgUp PgDn", "选择"),
            ("Enter", "把该文件所在目录设成工作目录"),
            ("打字", "过滤（纯内存）"),
            ("Esc / q", "关闭"),
        ],
    ),
    (
        "表单",
        &[
            ("↑↓ / j k", "换字段"),
            ("←→ / Space", "改选项（选项与开关）"),
            ("Enter", "开始编辑（再按结束）"),
            ("Ctrl-F", "挑文件（路径字段）"),
            ("Ctrl-E", "执行 · 危险动作要按两次"),
            ("Esc", "返回列表"),
        ],
    ),
    (
        "选文件",
        &[
            ("↑↓", "选择"),
            ("Enter", "进目录 · 选中文件"),
            ("Tab", "标记（多选，配多值字段）"),
            ("←", "回上一层目录"),
            ("打字", "过滤（纯内存，不碰磁盘）"),
            ("Ctrl-D", "用当前目录（要填目录的字段）"),
            ("Ctrl-U", "清空目标字段"),
            ("Esc", "取消"),
        ],
    ),
    (
        "执行中",
        &[
            ("q", "取消（先 SIGTERM，让工具自己收尾）"),
            ("其它键", "照常可用：列表、搜索都不耽误"),
        ],
    ),
    (
        "输出视图",
        &[
            ("↑↓ / PgUp PgDn", "滚动"),
            ("g / G", "到顶 / 到底"),
            ("s", "保存到文件"),
            ("c", "复制到剪贴板"),
            ("q / Esc", "关闭"),
        ],
    ),
    (
        "执行历史",
        &[
            ("↑↓", "选择"),
            ("Enter", "重跑那一条（参数原样复用，不用再填）"),
            ("e", "把那次参数**填回表单** → 改一改再跑"),
            ("q / Esc", "关闭"),
        ],
    ),
];

/// 帮助内容一共多少行（含段标题和段间空行），供滚动夹紧用。
fn total_lines() -> usize {
    SECTIONS
        .iter()
        .map(|(_, keys)| keys.len() + 2)
        .sum::<usize>()
}

pub fn draw(frame: &mut ratatui::Frame, app: &App) {
    if !app.is_help_open() {
        return;
    }

    let area = frame.area();
    // 居中的浮层，别铺满 —— 一眼能看出这是「盖在上面的一层」。
    let width = area.width.saturating_sub(8).min(76);
    let height = area.height.saturating_sub(4).min(total_lines() as u16 + 4);
    let popup = Rect {
        x: area.x + (area.width.saturating_sub(width)) / 2,
        y: area.y + (area.height.saturating_sub(height)) / 2,
        width,
        height,
    };

    // 先把底下那层清掉，否则会透过来。
    frame.render_widget(Clear, popup);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme::CYAN))
        .title(Span::styled(
            " 帮助 · ↑↓ 滚动 · q 关闭 ",
            Style::default()
                .fg(theme::CYAN)
                .add_modifier(Modifier::BOLD),
        ));
    let inner = block.inner(popup);
    frame.render_widget(block, popup);

    let rows = Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).split(inner);
    let (Some(body), Some(hint)) = (rows.first().copied(), rows.get(1).copied()) else {
        return;
    };

    let mut lines: Vec<Line> = Vec::new();
    for (title, keys) in SECTIONS {
        lines.push(Line::from(Span::styled(
            (*title).to_string(),
            Style::default()
                .fg(theme::BG)
                .bg(theme::PURPLE)
                .add_modifier(Modifier::BOLD),
        )));
        for (key, what) in keys.iter() {
            lines.push(Line::from(vec![
                Span::styled(format!("  {key:<18}"), Style::default().fg(theme::GREEN)),
                Span::styled(*what, Style::default().fg(theme::TEXT)),
            ]));
        }
        lines.push(Line::from(""));
    }

    // 夹到最后**一屏**：滚过头会看到一片空白，那没有意义。
    let max_scroll = lines.len().saturating_sub(body.height as usize);
    let scroll = app.help_scroll().min(max_scroll) as u16;
    frame.render_widget(
        Paragraph::new(Text::from(lines))
            .scroll((scroll, 0))
            .wrap(Wrap { trim: false }),
        body,
    );
    frame.render_widget(
        Paragraph::new(Span::styled(
            "工作目录决定工具在哪儿找文件 · 按 d 可改",
            Style::default().fg(theme::FAINT),
        )),
        hint,
    );
}
