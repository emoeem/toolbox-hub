//! 配色与面板样式。整个 UI 只有这一处定义颜色。

use ratatui::{
    style::{Color, Modifier, Style},
    widgets::{Block, BorderType},
};

use crate::model::Domain;

pub const BG: Color = Color::Rgb(30, 30, 46);
pub const PANEL: Color = Color::Rgb(36, 36, 52);
pub const HIGHLIGHT: Color = Color::Rgb(49, 50, 68);
pub const BORDER: Color = Color::Rgb(82, 84, 104);
pub const FAINT: Color = Color::Rgb(88, 91, 112);

pub const TEXT: Color = Color::Rgb(205, 214, 244);
pub const DIM: Color = Color::Rgb(166, 173, 200);
pub const PURPLE: Color = Color::Rgb(203, 166, 247);
pub const CYAN: Color = Color::Rgb(137, 220, 235);
pub const GREEN: Color = Color::Rgb(166, 227, 161);
pub const YELLOW: Color = Color::Rgb(249, 226, 175);
pub const BLUE: Color = Color::Rgb(137, 180, 250);
/// 错误与「已过期」这类要你注意的红。
pub const RED: Color = Color::Rgb(243, 139, 168);
pub const PINK: Color = Color::Rgb(245, 194, 231);

/// 圆角面板：沿用 FFTools 的视觉基调。
pub fn panel(title: &str) -> Block<'static> {
    Block::bordered()
        .border_type(BorderType::Rounded)
        .title(title.to_string())
        .title_style(title_style())
        .border_style(Style::default().fg(BORDER))
        .style(Style::default().bg(PANEL))
}

pub fn title_style() -> Style {
    Style::default().fg(PURPLE).add_modifier(Modifier::BOLD)
}

/// 表格里「当前选中行」的高亮：HIGHLIGHT 底 + 正文色。
///
/// 只表达「这一行被选中」这一件事。焦点不在表上时各处的退化方式**并不一样**
/// （包管理结果表只去掉底色、安装清单干脆什么都不加），那是刻意的差异，
/// 所以那两处只共用这个函数的高亮分支，不要顺手把 else 也统一掉。
pub fn selected_row() -> Style {
    Style::default().bg(HIGHLIGHT).fg(TEXT)
}

/// 域的代表色，用于 Tabs 与详情区。
pub fn domain_color(domain: Domain) -> Color {
    match domain {
        Domain::Media => GREEN,
        Domain::Image => YELLOW,
        Domain::System => BLUE,
        Domain::Network => CYAN,
        Domain::Dev => PURPLE,
        Domain::Tools => TEXT,
        // 包管理用琥珀色：和开发紫、系统蓝区分得开，又不像危险色那样刺眼。
        Domain::Packages => YELLOW,
        // 打包用粉：和包管理的琥珀、开发紫都分得开，又不像红那样像"出事了"。
        Domain::Packaging => PINK,
        // 发现用青绿：一眼认出「这是去外面拿东西」的那个面板。
        Domain::Discover => CYAN,
    }
}

/// 二级分类的代表色。未知标签统一用主题紫。
pub fn tag_color(tag: &str) -> Color {
    match tag {
        "转码" => CYAN,
        "编辑" => YELLOW,
        "媒体" => GREEN,
        "字幕" => PINK,
        "分析" => BLUE,
        _ => PURPLE,
    }
}
