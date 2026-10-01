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

/// 域的代表色，用于 Tabs 与详情区。
pub fn domain_color(domain: Domain) -> Color {
    match domain {
        Domain::Media => GREEN,
        Domain::Image => YELLOW,
        Domain::System => BLUE,
        Domain::Network => CYAN,
        Domain::Dev => PURPLE,
        Domain::Tools => TEXT,
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
