//! 桌面通知：**长**的后台任务跑完时提醒一句。
//!
//! 为什么要有：转码 / 打包这类任务经常跑好几分钟，人早就切到别的窗口去了，
//! 只有盯着终端才知道跑完没有。跑完弹一条系统通知，切走了也能第一时间知道。
//!
//! 三条克制：
//!
//! * **只在够长时发**（见 [`MIN_NOTIFY`]）—— 秒级任务你正看着屏幕，弹通知只会烦人；
//! * **发不出去就算了**：没有 `notify-send`、没有通知服务、非桌面会话（SSH / CI）
//!   都直接跳过。这是锦上添花，不该影响任务本身、更不该拦着程序跑；
//! * 交互式任务（接管了终端的那种）不发：你本来就在看着它。
//!
//! ## 为什么走 `notify-send` 而不是引库
//!
//! `notify-rust` 之类在 Linux 上要么默认走 zbus（实测会把整条 async 运行时
//! —— zbus / zvariant / async-executor… 共 **80+ 个 crate** —— 拖进来），要么退回
//! libdbus（多一个系统库依赖）。对一个「跑完提醒一句」的锦上添花功能来说都太重，
//! 也和本仓库「依赖能省就省」的取向冲突。`notify-send`（libnotify 的命令行）是
//! freedesktop 的标准件、桌面基本都有 —— 和剪贴板那套（`wl-copy` / `xclip`）
//! 是同一个思路：尽力而为，缺了就退化成没有。

use std::process::{Command, Stdio};
use std::time::Duration;

/// 只有跑够这么久的任务才值得发通知；低于它直接跳过。
pub const MIN_NOTIFY: Duration = Duration::from_secs(10);

/// 后台任务收尾时提醒一句。`elapsed` 不够长就什么都不做。
pub fn task_finished(label: &str, ok: bool, elapsed: Duration) {
    if elapsed < MIN_NOTIFY {
        return;
    }
    let summary = if ok {
        "Toolbox Hub · 完成"
    } else {
        "Toolbox Hub · 失败"
    };
    let body = format!("{label} · 用时 {}", human(elapsed));
    send(summary, &body);
}

/// 真正发出去。`notify-send` 不在、或者发送失败都不报错（尽力而为）。
fn send(summary: &str, body: &str) {
    let _ = Command::new("notify-send")
        .arg(summary)
        .arg(body)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

/// `N 秒` / `M:SS` / `H:MM:SS`，给通知正文用。
fn human(elapsed: Duration) -> String {
    let secs = elapsed.as_secs();
    if secs < 60 {
        return format!("{secs} 秒");
    }
    let (hours, minutes, seconds) = (secs / 3600, (secs % 3600) / 60, secs % 60);
    if hours > 0 {
        format!("{hours}:{minutes:02}:{seconds:02}")
    } else {
        format!("{minutes}:{seconds:02}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn human_formats_seconds_minutes_and_hours() {
        assert_eq!(human(Duration::from_secs(5)), "5 秒");
        assert_eq!(human(Duration::from_secs(75)), "1:15");
        assert_eq!(human(Duration::from_secs(3661)), "1:01:01");
    }

    /// 门槛：短任务不该越过 [`MIN_NOTIFY`]（越过才可能去调 `notify-send`）。
    #[test]
    fn short_tasks_are_below_the_threshold() {
        assert!(Duration::from_secs(9) < MIN_NOTIFY);
        assert!(Duration::from_secs(11) >= MIN_NOTIFY);
    }
}
