//! 系统维护检查：把「该看一眼」的东西凑成一屏。
//!
//! 这些检查以前散在三处（孤儿包在包中心、`.pacnew` 谁都没管、依赖完整性要自己
//! 记着敲 `pacman -Dk`）。现在合成一个「维护」模式：一眼看完，能当场处理的就
//! 直接按 Enter 处理。
//!
//! ## 哪些是纯逻辑，哪些要碰系统
//!
//! * [`pacnew_in`]：扫 `*.pacnew` / `*.pacsave`（给个根目录就行，测试拿临时目录跑）
//! * [`parse_missing_deps`]：从 `pacman -Dk` 的输出里挑出问题行
//! * [`dir_size`]：目录里有多少文件、占多少字节
//! * [`scan`]：把上面这些加上 libalpm 的数据拼成一屏
//!
//! ## 关于「贵不贵」
//!
//! 前六项都是几十毫秒级（libalpm 在常驻线程里是热的，扫 `/etc` 与缓存目录是
//! 本地文件系统）。**唯独文件完整性（`pacman -Qk`）要几秒** —— 它得 stat 每个包
//! 里的每个文件。所以那一项不放进 [`scan`]，而是单独一个按需动作，界面上写清楚
//! 「会慢」。

use std::{path::Path, process::Command};

use super::libalpm::Db;

/// 检查结果的分级。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HealthStatus {
    /// 没事。
    Ok,
    /// 值得看一眼。
    Warn,
    /// 该处理了。
    Bad,
}

impl HealthStatus {
    pub fn icon(self) -> &'static str {
        match self {
            Self::Ok => "✓",
            Self::Warn => "!",
            Self::Bad => "✗",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Ok => "正常",
            Self::Warn => "注意",
            Self::Bad => "待处理",
        }
    }
}

/// 这一项能做什么（面板上按 Enter）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HealthAction {
    /// 只能看，没什么可做的。
    None,
    /// 清孤儿包（弹确认面板）。
    RemoveOrphans(Vec<String>),
    /// 清包缓存（弹确认面板）。
    ClearCache,
    /// 把明细丢进输出视图。
    Show(Vec<String>),
    /// 系统更新（弹确认面板）。
    Update,
    /// 按需跑文件完整性检查（慢，几秒）。
    CheckFiles,
}

/// 一屏里的一项。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HealthItem {
    pub title: &'static str,
    pub status: HealthStatus,
    pub summary: String,
    /// 明细（前若干条），信息面板里显示。
    pub detail: Vec<String>,
    pub action: HealthAction,
}

impl HealthItem {
    fn new(
        title: &'static str,
        status: HealthStatus,
        summary: impl Into<String>,
        detail: Vec<String>,
        action: HealthAction,
    ) -> Self {
        Self {
            title,
            status,
            summary: summary.into(),
            detail,
            action,
        }
    }

    /// 明细前几条 + 「还有 N 条」。
    fn preview(lines: &[String], limit: usize) -> Vec<String> {
        let mut out: Vec<String> = lines.iter().take(limit).cloned().collect();
        if lines.len() > limit {
            out.push(format!("…… 还有 {} 条", lines.len() - limit));
        }
        out
    }
}

// ── 纯逻辑（测试盯得住的部分）──────────────────────────────────────────────

/// 扫一个目录树里的 `*.pacnew` / `*.pacsave`（返回排序过的路径）。
///
/// 这两个后缀是 pacman 的约定：升级时它不会动你改过的配置文件，而是把新版写成
/// `.pacnew` 放在旁边，让你自己合。**没人合它就一直躺在那儿** —— 于是你跑着旧配置、
/// 却以为已经升级了。
///
/// 不跟符号链接（免得绕圈），出错就当没有（`.pacnew` 只是提醒，不是关键路径）。
pub fn pacnew_in(root: &Path) -> Vec<String> {
    let mut found = Vec::new();
    walk(root, &mut found, 0);
    found.sort();
    found
}

fn walk(dir: &Path, found: &mut Vec<String>, depth: usize) {
    // 深度限制：/etc 下面本来就浅，加个上限免得撞上什么奇怪的挂载
    if depth > 8 {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        if kind.is_symlink() {
            continue;
        }
        if kind.is_dir() {
            walk(&path, found, depth + 1);
            continue;
        }
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.ends_with(".pacnew") || name.ends_with(".pacsave") {
            found.push(path.to_string_lossy().to_string());
        }
    }
}

/// 从 `pacman -Dk` 的输出里挑出问题行。
///
/// 它没问题是打印一句 `No database errors have been found!`，有问题时是一行一个
/// `error: ...`。这里只认后者；顺带把「没问题」也认出来，好把状态定成 `Ok`。
pub fn parse_missing_deps(output: &str) -> Vec<String> {
    output
        .lines()
        .map(str::trim)
        .filter(|line| line.starts_with("error:") || line.starts_with("warning:"))
        .map(str::to_string)
        .collect()
}

/// 目录里有多少文件、一共多少字节（不跟符号链接，不算目录本身）。
pub fn dir_size(dir: &Path) -> (usize, u64) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return (0, 0);
    };
    let mut count = 0;
    let mut bytes = 0;
    for entry in entries.flatten() {
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        if kind.is_symlink() || !kind.is_file() {
            continue;
        }
        if let Ok(meta) = entry.metadata() {
            count += 1;
            bytes += meta.len();
        }
    }
    (count, bytes)
}

// ── 扫描 ────────────────────────────────────────────────────────────────────

/// 包缓存目录（`pacman.conf` 的 CacheDir 默认值）。
pub const CACHE_DIR: &str = "/var/cache/pacman/pkg";
/// 配置文件的地盘。
pub const ETC_DIR: &str = "/etc";

/// 跑一遍全部检查（除了那个慢的文件完整性）。
///
/// 每一项都**不报错**：单项失败就把那一项标成「查不了」，其余照常显示 ——
/// 维护面板本身挂了就太讽刺了。
pub fn scan(db: &Db) -> Vec<HealthItem> {
    let mut items = Vec::new();

    // 1. 孤儿包：装了但没人要（可以直接清）
    let orphans = db.orphan_names();
    items.push(if orphans.is_empty() {
        HealthItem::new(
            "孤儿包",
            HealthStatus::Ok,
            "没有：装了但没人依赖的包一个都没有",
            Vec::new(),
            HealthAction::None,
        )
    } else {
        HealthItem::new(
            "孤儿包",
            HealthStatus::Warn,
            format!("{} 个（装上之后没人依赖、你也没点名装）", orphans.len()),
            HealthItem::preview(&orphans, 12),
            HealthAction::RemoveOrphans(orphans),
        )
    });

    // 2. 依赖完整性：pacman -Dk 自己说
    match Command::new("pacman").args(["-Dk"]).output() {
        Ok(output) => {
            let text = format!(
                "{}{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            let problems = parse_missing_deps(&text);
            items.push(if problems.is_empty() {
                HealthItem::new(
                    "依赖完整性",
                    HealthStatus::Ok,
                    "pacman -Dk 没发现问题",
                    Vec::new(),
                    HealthAction::None,
                )
            } else {
                HealthItem::new(
                    "依赖完整性",
                    HealthStatus::Bad,
                    format!("{} 条问题（pacman -Dk）", problems.len()),
                    HealthItem::preview(&problems, 12),
                    HealthAction::Show(problems),
                )
            });
        }
        Err(error) => items.push(HealthItem::new(
            "依赖完整性",
            HealthStatus::Warn,
            format!("查不了：{error}"),
            Vec::new(),
            HealthAction::None,
        )),
    }

    // 3. .pacnew / .pacsave：没人合就一直躺在那儿
    let pacnew = pacnew_in(Path::new(ETC_DIR));
    items.push(if pacnew.is_empty() {
        HealthItem::new(
            "配置文件",
            HealthStatus::Ok,
            "没有待处理的 .pacnew / .pacsave",
            Vec::new(),
            HealthAction::None,
        )
    } else {
        HealthItem::new(
            "配置文件",
            HealthStatus::Warn,
            format!("{} 个 .pacnew / .pacsave 等你合", pacnew.len()),
            HealthItem::preview(&pacnew, 12),
            HealthAction::Show(pacnew),
        )
    });

    // 4. 包缓存：占多大、有多少个文件
    let (files, bytes) = dir_size(Path::new(CACHE_DIR));
    items.push(if files == 0 {
        HealthItem::new(
            "包缓存",
            HealthStatus::Ok,
            "空的",
            Vec::new(),
            HealthAction::None,
        )
    } else {
        HealthItem::new(
            "包缓存",
            HealthStatus::Warn,
            format!("{} 个文件 · {}", files, super::libalpm::size_text(bytes)),
            vec![
                format!("目录：{CACHE_DIR}"),
                String::from("按 Enter 清掉旧版本（每个包留几个看配置）"),
            ],
            HealthAction::ClearCache,
        )
    });

    // 5. 可更新数 + 同步库有多旧（这两件事必须一起说：库旧了数字就偏小）
    let updates = db.pending_updates();
    let age = db.sync_age().map(|age| age.as_secs());
    let age_note = match age {
        Some(seconds) if seconds > 86_400 => format!("库 {} 天没同步", seconds / 86_400),
        Some(_) => String::from("库是新的"),
        None => String::from("库龄未知"),
    };
    items.push(match updates {
        Some(0) => HealthItem::new(
            "系统更新",
            HealthStatus::Ok,
            format!("没有可更新的包（{age_note}）"),
            Vec::new(),
            HealthAction::None,
        ),
        Some(count) => HealthItem::new(
            "系统更新",
            HealthStatus::Warn,
            format!("{count} 个包可以更新（{age_note}）"),
            vec![String::from("按 Enter 走一遍系统更新（paru -Syu）")],
            HealthAction::Update,
        ),
        None => HealthItem::new(
            "系统更新",
            HealthStatus::Warn,
            "查不出可更新数（数据库读不了）",
            Vec::new(),
            HealthAction::None,
        ),
    });

    // 6. 上次全系统升级（顺便提醒看一眼新闻）
    let last = super::probe::last_upgrade();
    items.push(match last {
        Some(stamp) => {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|since| since.as_secs())
                .unwrap_or(stamp);
            let days = now.saturating_sub(stamp) / 86_400;
            HealthItem::new(
                "上次全系统升级",
                if days > 30 {
                    HealthStatus::Warn
                } else {
                    HealthStatus::Ok
                },
                format!(
                    "{}（{} 天前）",
                    super::format_epoch(stamp)
                        .split_whitespace()
                        .next()
                        .unwrap_or(""),
                    days
                ),
                Vec::new(),
                HealthAction::None,
            )
        }
        None => HealthItem::new(
            "上次全系统升级",
            HealthStatus::Warn,
            "pacman.log 里找不到全系统升级记录",
            Vec::new(),
            HealthAction::None,
        ),
    });

    // 7. 文件完整性（按需，慢）
    items.push(HealthItem::new(
        "文件完整性",
        HealthStatus::Warn,
        "要按 Enter 才查（pacman -Qk 会 stat 每个包的每个文件，要几秒）",
        vec![
            String::from("查的是「包里的文件还在不在、有没有被改」"),
            String::from("几百个包的话几秒钟，别在等着出门的时候按"),
        ],
        HealthAction::CheckFiles,
    ));

    items
}

/// 一屏里的汇总：`(待处理, 注意)`。
pub fn tally(items: &[HealthItem]) -> (usize, usize) {
    let bad = items
        .iter()
        .filter(|item| item.status == HealthStatus::Bad)
        .count();
    let warn = items
        .iter()
        .filter(|item| item.status == HealthStatus::Warn)
        .count();
    (bad, warn)
}

/// `pacman -Qk` 的输出整理成能看的几行（它自己就是一行一个文件）。
pub fn parse_file_check(output: &str, limit: usize) -> Vec<String> {
    let lines: Vec<String> = output
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_string)
        .collect();
    HealthItem::preview(&lines, limit)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pacnew_scan_finds_both_suffixes_and_ignores_links() {
        let root = std::env::temp_dir().join(format!("toolbox-hub-pacnew-{}", std::process::id()));
        let nested = root.join("sub");
        std::fs::create_dir_all(&nested).expect("建目录");
        std::fs::write(root.join("a.conf.pacnew"), "x").expect("写文件");
        std::fs::write(nested.join("b.conf.pacsave"), "x").expect("写文件");
        std::fs::write(root.join("c.conf"), "x").expect("正常的配置文件不该被算进来");

        let found = pacnew_in(&root);
        assert_eq!(found.len(), 2, "{found:?}");
        assert!(found.iter().any(|path| path.ends_with("a.conf.pacnew")));
        assert!(found.iter().any(|path| path.ends_with("b.conf.pacsave")));
        assert!(
            !found.iter().any(|path| path.ends_with("c.conf")),
            "只在升级时留下的那两个后缀才算"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn missing_deps_come_from_pacman_dk_output() {
        // 真机上 `pacman -Dk` 干净时的输出
        assert!(
            parse_missing_deps("No database errors have been found!").is_empty(),
            "没问题时不该报出问题"
        );

        let output = "\
checking dependencies...
error: missing 'nvidia-utils=610.57.04' dependency for 'linux-nvidia-open'
warning: something odd
";
        let problems = parse_missing_deps(output);
        assert_eq!(problems.len(), 2);
        assert!(problems[0].contains("nvidia-utils"));
    }

    #[test]
    fn dir_size_counts_files_and_bytes() {
        let root = std::env::temp_dir().join(format!("toolbox-hub-size-{}", std::process::id()));
        std::fs::create_dir_all(root.join("sub")).expect("建目录");
        std::fs::write(root.join("one"), vec![0u8; 100]).expect("写");
        std::fs::write(root.join("two"), vec![0u8; 23]).expect("写");
        // 子目录里的不算（只看这一层：缓存目录是平的）
        std::fs::write(root.join("sub").join("deep"), vec![0u8; 999]).expect("写");

        let (count, bytes) = dir_size(&root);
        assert_eq!(count, 2);
        assert_eq!(bytes, 123);

        assert_eq!(dir_size(Path::new("/definitely/not/here")), (0, 0));

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn tally_counts_bad_and_warn() {
        let item = |status| HealthItem::new("x", status, "s", Vec::new(), HealthAction::None);
        let items = vec![
            item(HealthStatus::Ok),
            item(HealthStatus::Warn),
            item(HealthStatus::Bad),
            item(HealthStatus::Bad),
        ];
        assert_eq!(tally(&items), (2, 1));
    }

    #[test]
    fn preview_says_how_many_are_left() {
        let lines: Vec<String> = (0..20).map(|index| format!("line{index}")).collect();
        let preview = HealthItem::preview(&lines, 3);
        assert_eq!(preview.len(), 4);
        assert_eq!(preview[0], "line0");
        assert_eq!(preview[3], "…… 还有 17 条");
    }

    /// 真机扫描：只要求「能跑完、每一项都有话说」，不要求系统一定是干净的。
    #[test]
    #[ignore = "真的读 pacman 数据库、跑 pacman -Dk、扫 /etc（只读），默认跳过"]
    fn smoke_scan_finishes() {
        let db = Db::open().expect("打开数据库");
        let items = scan(&db);
        assert!(items.len() >= 6, "该有的检查项不能少：{items:?}");
        for item in &items {
            assert!(!item.summary.is_empty(), "{} 没说清楚", item.title);
        }
        let (bad, warn) = tally(&items);
        println!("维护检查：{bad} 项待处理 · {warn} 项注意");
        for item in &items {
            println!(
                "  {} {} —— {}",
                item.status.icon(),
                item.title,
                item.summary
            );
        }
    }
}
