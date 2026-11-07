//! MCP 服务端的热更新（计划书 P4-04）：监视 `tools.d/`、`tools/`、`mcp.toml`，
//! 变更后由主线程**原子重建**工具目录快照，并按协议发 `notifications/tools/list_changed`。
//!
//! 为什么用 mtime 轮询而不是 inotify（`watch.rs` 的做法）：主循环阻塞在 stdin 的
//! `read_line` 上，事件只有「来了一条消息」和「轮询线程发现指纹变了」两种唤醒时机；
//! 轮询把「监视」压到一个独立线程里，指纹比对是纯只读操作，最坏 1 秒延迟 —— 对
//! 「用户改完配置、AI 下一轮对话看到新工具」的节奏完全够用。
//!
//! 原子性纪律（P4-04 验收）：
//!
//! * 重建在 watcher 线程完成（发现 + 目录构建，纯只读 + 纯计算），产出一个
//!   **完整**的 [`ReloadSnapshot`]，主线程一次 `take` 整体换入 —— 不存在半初始化状态；
//! * 非法 Manifest 由既有的发现层逐文件跳过（`ReloadReport::warnings`），不会崩；
//! * `ToolCatalog` 的排序只由内容决定，相同目录内容永远产生相同顺序与相同名字
//!   （`NameRegistry` 每次从新目录整体重建，不增量注册，重复 id 不可能）；
//! * 已在执行的调用拿的是构造请求时的 `ToolDefinition` 副本，快照换入不影响它们。

use std::{
    fs,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    thread::{self, JoinHandle},
    time::Duration,
};

use crate::{registry::Registry, security::policy::SafetyPolicy, service::catalog::ToolCatalog};

/// 两次指纹扫描的间隔。
const POLL_INTERVAL: Duration = Duration::from_secs(1);

/// 递归扫描的深度上限：防符号链接成环把 watcher 拖死。用户配置树远到不了这个深度。
const MAX_DEPTH: usize = 8;

/// 一次热更新要整体换入的新状态。
pub(crate) struct ReloadSnapshot {
    pub catalog: Arc<ToolCatalog>,
    pub policy: SafetyPolicy,
    /// `mcp.toml` 读不动时 fail-closed 的说明（主线程负责打到 stderr）。
    pub problem: Option<String>,
}

/// watcher 线程与主线程之间的一次性信箱：有就取走，没有就是 `None`。
pub(crate) struct HotReload {
    pending: Mutex<Option<ReloadSnapshot>>,
}

impl HotReload {
    pub fn new() -> Self {
        Self {
            pending: Mutex::new(None),
        }
    }

    /// 取走待换入的快照（主线程在每条消息前调用）。
    pub fn take(&self) -> Option<ReloadSnapshot> {
        self.pending
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take()
    }

    /// 存入待换入的快照（watcher 线程与测试用）。
    pub(crate) fn store(&self, snapshot: ReloadSnapshot) {
        *self
            .pending
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(snapshot);
    }
}

/// 启动 watcher 线程；配置目录不存在也照常起（目录出现后自然开始监视）。
///
/// 线程与主循环同生命周期：stdio 循环退出、进程结束时它随之结束，无需手工停。
pub(crate) fn spawn_watcher(
    config_dir: PathBuf,
    bin_dir: PathBuf,
    sink: Arc<HotReload>,
) -> JoinHandle<()> {
    thread::Builder::new()
        .name(String::from("mcp-hot-reload"))
        .spawn(move || {
            let mut last = fingerprint(&config_dir);
            loop {
                thread::sleep(POLL_INTERVAL);
                let current = fingerprint(&config_dir);
                if current == last {
                    continue;
                }
                last = current;

                // mcp.toml fail-closed：读不动退回全拒绝，问题说明交给主线程打日志。
                let (policy, problem) = SafetyPolicy::from_file();
                let (registry, report) = Registry::discover(&bin_dir);
                for warning in &report.warnings {
                    eprintln!("toolbox-hub mcp reload: {warning}");
                }
                for error in &report.errors {
                    eprintln!("toolbox-hub mcp reload: {error}");
                }
                sink.store(ReloadSnapshot {
                    catalog: Arc::new(ToolCatalog::from_registry(&registry)),
                    policy,
                    problem,
                });
            }
        })
        .expect("起热更新线程")
}

/// 监视目标的指纹：三个用户内容源里**每个文件**的（路径, mtime 纳秒, 大小），按路径排序。
///
/// 与 `watch.rs` 同一条边界：只认用户手写的子树（`tools.d/`、`tools/`）和
/// `mcp.toml`；`state.toml` / `packages.toml` 这些程序自己写的文件不在监视范围。
pub(crate) fn fingerprint(config_dir: &Path) -> Vec<(PathBuf, u64, u64)> {
    let mut entries = Vec::new();
    for target in ["mcp.toml", "tools.d", "tools"] {
        let path = config_dir.join(target);
        scan(&path, 0, &mut entries);
    }
    entries.sort();
    entries
}

/// 递归收集文件的（路径, mtime 纳秒, 大小）；目录不存在是常态，安静返回空。
fn scan(path: &Path, depth: usize, entries: &mut Vec<(PathBuf, u64, u64)>) {
    let Ok(meta) = fs::symlink_metadata(path) else {
        return;
    };
    if meta.is_file() {
        let mtime = meta
            .modified()
            .ok()
            .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|duration| duration.as_nanos() as u64)
            .unwrap_or(0);
        entries.push((path.to_path_buf(), mtime, meta.len()));
        return;
    }
    if !meta.is_dir() || depth >= MAX_DEPTH {
        return;
    }
    let Ok(read_dir) = fs::read_dir(path) else {
        return;
    };
    for entry in read_dir.flatten() {
        scan(&entry.path(), depth + 1, entries);
    }
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        path::PathBuf,
        sync::Arc,
        thread,
        time::{Duration, Instant, SystemTime, UNIX_EPOCH},
    };

    use super::{HotReload, fingerprint, spawn_watcher};
    use crate::service::catalog::ToolCatalog;

    fn temp_dir(tag: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("toolbox-hub-mcp-reload-{tag}-{nanos}"));
        fs::create_dir_all(&dir).expect("mkdir");
        dir
    }

    #[test]
    fn fingerprint_reflects_content_changes_in_the_three_watched_targets() {
        let dir = temp_dir("fp");
        let empty = fingerprint(&dir);
        assert!(empty.is_empty());

        fs::write(dir.join("mcp.toml"), "[mcp]\nenabled = true\n").expect("write");
        fs::create_dir_all(dir.join("tools.d")).expect("mkdir");
        fs::write(dir.join("tools.d/a.toml"), "[[action]]\n").expect("write");
        fs::create_dir_all(dir.join("tools/media")).expect("mkdir");
        fs::write(dir.join("tools/media/s.sh"), "#!/bin/sh\n").expect("write");

        let full = fingerprint(&dir);
        assert_eq!(full.len(), 3, "mcp.toml + manifest + 脚本：{full:?}");
        // 路径有序：相同目录内容产生相同排序。
        let mut sorted = full.clone();
        sorted.sort();
        assert_eq!(full, sorted);

        // 只改内容不改数量：指纹要变（mtime 变化）。
        let before = fingerprint(&dir);
        thread::sleep(Duration::from_millis(5));
        fs::write(dir.join("tools.d/a.toml"), "[[action]]\nid = \"x\"\n").expect("rewrite");
        let after = fingerprint(&dir);
        assert_ne!(before, after);

        // 程序自己的文件不在监视范围（与 watch.rs 同一条边界）。
        fs::write(dir.join("state.toml"), "favorites = []\n").expect("write");
        let untouched = fingerprint(&dir);
        assert_eq!(untouched.len(), 3, "state.toml 不计入");

        fs::remove_dir_all(&dir).expect("cleanup");
    }

    #[test]
    fn the_mailbox_hands_over_exactly_once() {
        let mailbox = HotReload::new();
        assert!(mailbox.take().is_none());
        mailbox.store(super::ReloadSnapshot {
            catalog: Arc::new(ToolCatalog::new([])),
            policy: crate::security::policy::SafetyPolicy::new(Default::default()),
            problem: None,
        });
        assert!(mailbox.take().is_some(), "取走");
        assert!(mailbox.take().is_none(), "只交一次");
    }

    /// watcher 真线程：起线程**之后**往 tools.d 新增文件，1 秒轮询内要产出快照；
    /// 非法 TOML 只被发现层逐文件跳过，watcher 不崩、下一次变更照常重建。
    ///
    /// 注意顺序：先放好初始内容再起 watcher（否则首次基线可能与写入竞态，
    /// 之后永远等不到 diff）—— 生产路径 run_stdio 也是先发现、后起线程。
    #[test]
    fn the_watcher_thread_rebuilds_on_change_and_survives_invalid_manifests() {
        let dir = temp_dir("watcher");
        fs::create_dir_all(dir.join("tools.d")).expect("mkdir");
        fs::write(dir.join("tools.d/good.toml"), "[[action]]\n").expect("write good");

        let mailbox = Arc::new(HotReload::new());
        let handle = spawn_watcher(dir.clone(), dir.clone(), Arc::clone(&mailbox));

        // 等过一次完整轮询（基线必然已落定），再写触发文件 —— 排除「基线把
        // 触发文件也扫进去」的调度竞态。
        thread::sleep(super::POLL_INTERVAL + Duration::from_millis(300));
        fs::write(dir.join("tools.d/extra.toml"), "[[action]]\nid = \"x\"\n").expect("write extra");
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut first = None;
        while Instant::now() < deadline {
            if let Some(snapshot) = mailbox.take() {
                first = Some(snapshot);
                break;
            }
            thread::sleep(Duration::from_millis(20));
        }
        assert!(first.is_some(), "变更要产出快照");

        // 非法 manifest：发现层逐文件跳过，watcher 不崩，下一次变更照常产出快照。
        fs::write(dir.join("tools.d/bad.toml"), "这不是 TOML (((\n").expect("write bad");
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut second = None;
        while Instant::now() < deadline {
            if let Some(snapshot) = mailbox.take() {
                second = Some(snapshot);
                break;
            }
            thread::sleep(Duration::from_millis(20));
        }
        assert!(second.is_some(), "非法 manifest 后的重建照常发生");

        drop(handle);
        fs::remove_dir_all(&dir).expect("cleanup");
    }
}
