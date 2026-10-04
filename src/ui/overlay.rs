//! 浮层几何：把「已经算好宽高的框」摆到屏幕中间。
//!
//! 共享的**只有「居中」这一步**。宽高怎么来，各家不一样 —— 帮助页按内容行数夹、
//! 仓库确认框按终端面积夹、包管理菜单按宽度百分比 —— 那是三种不同的语义，
//! 硬合并只会把「谁在算什么」藏进参数里。所以这里只提供居中，不提供「算多大」。

use ratatui::layout::{Constraint, Flex, Layout, Rect};

/// 把已知宽高的矩形在 `area` 里居中。
///
/// 宽高超过 `area` 时夹到 `area`：浮层不该比屏幕还大，而且返回的这个矩形就是
/// `Clear` 和鼠标命中用的那个，夹一次总比让每个调用点各自记着夹好。
///
/// 余下的格数是奇数时，多出来的那一格留在上/左（`Flex::Center` 就是这么分的，
/// 见本文件的测试），不是下/右。
pub fn centered_rect(area: Rect, width: u16, height: u16) -> Rect {
    let width = width.min(area.width);
    let height = height.min(area.height);
    Rect {
        x: area.x + area.width.saturating_sub(width).div_ceil(2),
        y: area.y + area.height.saturating_sub(height).div_ceil(2),
        width,
        height,
    }
}

/// 「宽度按 `area` 的百分比、高度按固定行数」的浮层。
///
/// 包管理的排序菜单与执行确认是这个语义（宽度跟着终端走），跟按固定宽高的调用点
/// 不是一回事，所以分开写，而不是给 [`centered_rect`] 塞一个百分比开关。
///
/// 百分比那一步交给 `Constraint::Percentage` 自己算：它的取整是布局求解器的行为
/// （不是简单的 floor，窄终端上逐格试出来的），重写一遍只会写歪。
pub fn centered_percent(area: Rect, width_percent: u16, height: u16) -> Rect {
    let vertical = Layout::vertical([Constraint::Length(height)])
        .flex(Flex::Center)
        .split(area);
    Layout::horizontal([Constraint::Percentage(width_percent)])
        .flex(Flex::Center)
        .split(vertical[0])[0]
}

#[cfg(test)]
mod tests {
    use super::{centered_percent, centered_rect};
    use ratatui::layout::{Constraint, Flex, Layout, Rect};

    fn layout_center(area: Rect, width: u16, height: u16) -> Rect {
        let vertical = Layout::vertical([Constraint::Length(height)])
            .flex(Flex::Center)
            .split(area);
        Layout::horizontal([Constraint::Length(width)])
            .flex(Flex::Center)
            .split(vertical[0])[0]
    }

    /// 逐格比对 `Layout + Flex::Center`：调用点换到这个函数以后，浮层落在哪儿
    /// 必须和换之前一模一样 —— 这个测试就是那句话的证明（奇数余数的方向在里面）。
    #[test]
    fn centered_rect_lands_where_flex_center_lands() {
        for width in 0..=20u16 {
            for height in 0..=6u16 {
                let area = Rect::new(3, 5, width, height);
                for box_width in 0..=width + 2 {
                    for box_height in 0..=height + 2 {
                        assert_eq!(
                            centered_rect(area, box_width, box_height),
                            layout_center(area, box_width, box_height),
                            "area {area:?} box {box_width}x{box_height}"
                        );
                    }
                }
            }
        }
    }

    /// 百分比浮层直接走 `Constraint::Percentage`，这里连 x 一起锁住：
    /// 包管理那两个菜单（40% / 76%）以前就是这个组合算出来的。
    #[test]
    fn centered_percent_matches_the_percentage_constraint() {
        for width in 0..=120u16 {
            for height in [12u16, 24, 25] {
                let area = Rect::new(1, 2, width, height);
                for percent in [0u16, 1, 40, 50, 76, 99, 100] {
                    let expected = Layout::vertical([Constraint::Length(8)])
                        .flex(Flex::Center)
                        .split(area)[0];
                    let expected = Layout::horizontal([Constraint::Percentage(percent)])
                        .flex(Flex::Center)
                        .split(expected)[0];
                    assert_eq!(
                        centered_percent(area, percent, 8),
                        expected,
                        "{percent}% of {width}x{height}"
                    );
                }
            }
        }
    }

    /// 比屏幕还大的框：夹到 area，而不是算出负数坐标画到别处去。
    #[test]
    fn an_oversized_box_is_clamped_to_the_area() {
        let area = Rect::new(10, 10, 20, 6);
        assert_eq!(centered_rect(area, 100, 100), area);
        assert_eq!(centered_rect(area, 100, 100), layout_center(area, 20, 6));
    }
}
