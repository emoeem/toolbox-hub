//! 通用小工具。
//!
//! 放这里的唯一门槛：**与业务无关**。一旦某个函数开始认识工具、仓库或界面
//!（哪怕只是引用了一个结构体），它就该搬回对应的模块，而不是在这里安家。
//!
//! 目前有两件东西：

pub mod path;

use std::time::{SystemTime, UNIX_EPOCH};

/// 临时文件名里的唯一片段：`<pid>-<纳秒>`。
///
/// 为什么不只用 pid：`/tmp` 是粘性目录，**路径可预测就等于把「写哪个文件」的决定权
/// 交给别人** —— 别人可以先建一个同名符号链接，等本进程往里写（把终端交给外部
/// 文件管理器、写 PKGBUILD 给 shellcheck 检查，走的都是这条路）。pid 会复用，也能
/// 枚举；再掺一个纳秒时间戳，撞上就得先猜中时刻。
///
/// 内核的 `fs.protected_symlinks` 能挡掉大部分，但那是**发行版的默认值**，不是
/// 本程序能保证的东西。
pub fn temp_tag() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or_default();
    format!("{}-{nanos}", std::process::id())
}
