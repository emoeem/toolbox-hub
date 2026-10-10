//! 监听配置目录：改了 `tools.d/*.toml` 或 `tools/` 里的脚本，主界面自动重载。
//!
//! 以前只能手按 `r` 重新发现一遍 —— 写插件时「改一下、切回来、按 `r`」要重复
//! 几十次。这里用 inotify 盯着配置目录，变了就放下一个标志位，主循环下一帧读它、
//! 触发一次 [`crate::app::App::reload`]。
//!
//! 三条边界，都是实测踩出来的：
//!
//! * **只认用户手写的两个子树**（见 [`USER_CONTENT_DIRS`]）。配置目录里还躺着
//!   `state.toml`（收藏、上次工作目录）、`repositories.toml`、`packages.toml` ——
//!   那些是**程序自己**在正常操作里写的；一锅端就会出现「按 `f` 收藏一个工具，
//!   界面自己跳回第一行、提示从『已收藏 x』变成『已刷新』」这种鬼打墙。
//! * **只认真的改了内容的事件**。inotify 连它自己的递归扫描（打开目录列内容）都
//!   报成 `Access(Open)`；不看事件类型的话，启动时一眼 `tools.d/` 就白白重载一次。
//! * **配置目录还不存在就挂不上**（第一次跑）。调用方可以在它出现之后再试一次，
//!   见 `main.rs` 里的补挂：挂上之前建的文件不会补发事件，所以补挂的那一刻要
//!   自己先重扫一遍。
//!
//! 挂不上不拦程序：自动重载用不了而已，手动 `r` 还在。

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use notify::{RecommendedWatcher, RecursiveMode, Watcher};

use crate::providers::{manifest, scripted};

/// 只有这两个子树里的改动算「用户改了配置」：用户的 TOML manifest 和用户脚本。
///
/// 名字直接引用各自 Provider 里的常量，免得哪天目录改了名，这里悄悄盯错地方。
const USER_CONTENT_DIRS: [&str; 2] = [manifest::USER_DIR, scripted::USER_TOOLS_DIR];

/// 一个「配置目录有变化」的标志位，外加背后的 watcher 把手。
///
/// watcher 随结构体一起 drop：句柄没了，监听线程自然收尾，不需要手工停。
pub struct ConfigWatcher {
    changed: Arc<AtomicBool>,
    _watcher: RecommendedWatcher,
}

impl ConfigWatcher {
    /// 盯住配置目录（递归）。目录都还不存在就返回 `None`。
    pub fn spawn(config_dir: &Path) -> Option<Self> {
        if !config_dir.is_dir() {
            return None;
        }
        let changed = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&changed);
        let root = config_dir.to_path_buf();
        // 回调只判路径、置位，不做任何重活：它在 notify 自己的线程上跑，卡住会漏事件。
        let mut watcher =
            notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
                let Ok(event) = event else {
                    return;
                };
                // `Access` 不算「改了东西」：inotify 把它自己的递归扫描（打开目录
                // 列内容）也报成 `Access(Open)`，于是启动时盯上 `tools.d/` 就会凭空
                // 触发一次重载 —— 刚起来的界面会被白白重置回第一行。真正的改动走
                // Create / Modify / Remove。
                if matches!(event.kind, notify::EventKind::Access(_)) {
                    return;
                }
                if event.paths.iter().any(|path| is_user_content(path, &root)) {
                    flag.store(true, Ordering::SeqCst);
                }
            })
            .ok()?;

        watcher.watch(config_dir, RecursiveMode::Recursive).ok()?;
        Some(Self {
            changed,
            _watcher: watcher,
        })
    }

    /// 自上次以来用户改过配置没有（读一次就清零）。一次编辑常常触发多条事件，
    /// 标志位把它们收敛成一次重载。
    pub fn take_changed(&self) -> bool {
        self.changed.swap(false, Ordering::SeqCst)
    }
}

/// 事件路径是不是落在 `tools.d/` 或 `tools/` 里。
///
/// 按**路径组件**比（[`Path::starts_with`]），不是字符串前缀：`tools.d` 不是
/// `tools` 的子路径，`tools-extra/` 也不是，`state.toml` 更不是。
fn is_user_content(path: &Path, config_dir: &Path) -> bool {
    USER_CONTENT_DIRS
        .iter()
        .any(|dir| path.starts_with(config_dir.join(dir)))
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        path::PathBuf,
        time::{Duration, Instant, SystemTime, UNIX_EPOCH},
    };

    use super::*;

    /// 空临时目录（跟本仓库其它测试一个写法：进程号 + 纳秒，免得并行时撞车）。
    fn temp_dir(tag: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("toolbox-hub-watch-{tag}-{}", nanos));
        fs::create_dir_all(&dir).expect("mkdir");
        dir
    }

    /// 路径过滤是纯函数，先把边界钉死 —— 尤其 `tools` 与 `tools.d` 的前缀关系。
    #[test]
    fn only_the_two_user_content_dirs_count() {
        let root = Path::new("/home/me/.config/toolbox-hub");

        assert!(is_user_content(&root.join("tools.d/a.toml"), root));
        assert!(is_user_content(&root.join("tools/音视频/x.sh"), root));
        // 目录本身被建出来（`mkdir tools.d`）也得算 —— 第一次放 manifest 就是这一步。
        assert!(is_user_content(&root.join("tools.d"), root));

        // 程序自己写的三个文件：一碰就重载会让界面自己跳。
        assert!(!is_user_content(&root.join("state.toml"), root));
        assert!(!is_user_content(&root.join("repositories.toml"), root));
        assert!(!is_user_content(&root.join("packages.toml"), root));

        // 前缀像但不是一个目录（字符串 starts_with 在这里会错）。
        assert!(!is_user_content(&root.join("tools.dx/a.toml"), root));
        assert!(!is_user_content(&root.join("tools-extra/a.sh"), root));
        // 配置目录本身的事件（比如整个目录被换掉）不算。
        assert!(!is_user_content(root, root));
        // 别人家的同名目录不算。
        assert!(!is_user_content(
            Path::new("/home/me/.config/other/tools.d/a.toml"),
            root
        ));
    }

    #[test]
    fn a_missing_config_dir_cannot_be_watched() {
        let dir = temp_dir("missing");
        // 配置目录还不存在：挂不上，调用方稍后再试（见 main.rs 的补挂）。
        assert!(ConfigWatcher::spawn(&dir.join("还不存在的配置目录")).is_none());
        fs::remove_dir_all(&dir).expect("cleanup");
    }

    /// 真挂一个 inotify：写 `tools.d/` 里的文件要触发；写 `state.toml` 不能触发。
    #[test]
    fn edits_under_tools_d_flip_the_flag_but_state_toml_does_not() {
        let root = temp_dir("live");
        fs::create_dir_all(root.join("tools.d")).expect("mkdir");
        let watcher = ConfigWatcher::spawn(&root).expect("真目录应当挂得上");

        // 先写「自家文件」：这一条要是红的，说明过滤没生效 —— 正常用起来就是
        // 「按 `f` 收藏 → state.toml 落盘 → 界面被自己弹回顶部」。
        fs::write(root.join("state.toml"), "favorites = []\n").expect("写 state.toml");
        std::thread::sleep(Duration::from_millis(300));
        assert!(
            !watcher.take_changed(),
            "程序自己写的 state.toml 不该要求重载"
        );

        // 再写用户 manifest：应当在几秒内翻起来（inotify 是即时的，留足余量）。
        fs::write(root.join("tools.d/my.toml"), "[[action]]\n").expect("写 manifest");
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut flipped = false;
        while Instant::now() < deadline {
            if watcher.take_changed() {
                flipped = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(flipped, "写了 tools.d 里的 manifest 应当要求重载");

        fs::remove_dir_all(&root).expect("cleanup");
    }
}
