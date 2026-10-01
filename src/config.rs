//! 配置目录与用户偏好。
//!
//! ## 两个目录，别混
//!
//! | 目录 | 放什么 | 默认 | 覆盖 |
//! | --- | --- | --- | --- |
//! | **配置** | `state.toml`（收藏）、`tools.d/`（你的 manifest）、`tools/`（你的脚本）、`packages.toml` | `~/.config/toolbox-hub` | `--config-dir` > `TOOLBOX_HUB_CONFIG` |
//! | **数据** | 执行历史、安装队列、搜索历史、已读新闻、输出留存 | `~/.local/share/toolbox-hub` | `--data-dir` > `TOOLBOX_HUB_DATA` |
//!
//! 为什么要分开：配置是换机器时要**带走**的东西，数据是可再生的。`--config-dir`
//! 只挪配置；想让整套（含数据）都进一个目录，两个参数都给。
//!
//! 这层用 [`OnceLock`] 存「启动时定下来的目录」：命令行解析完之后立刻
//! [`configure`]，之后所有模块都从这里问路径 —— 免得同一个路径在五个地方
//! 各算一遍（以前就是这样：state、manifest、scripted、packages 各硬编码了一处）。
//!
//! ## 偏好文件
//!
//! `packages.toml` 管软件包中心的默认值（默认仓库、演练模式、paccache 保留版本……）。
//! 解析原则和 `state.toml` 一样**宽容**：读不出来、字段写错、值不认识都退化成默认值
//! 并给一句提示，绝不让一个手改错的配置文件把工具卡死。

use std::{env, fs, path::PathBuf, sync::OnceLock};

use serde::Deserialize;

use crate::packages::SortMode;

/// 覆盖配置目录的环境变量。
pub const CONFIG_ENV: &str = "TOOLBOX_HUB_CONFIG";
/// 覆盖数据目录的环境变量。
pub const DATA_ENV: &str = "TOOLBOX_HUB_DATA";

/// 配置目录名（`$XDG_CONFIG_HOME` 或 `~/.config` 之下）。
const CONFIG_LEAF: &str = "toolbox-hub";
/// 数据目录名（`$XDG_DATA_HOME` 或 `~/.local/share` 之下）。
const DATA_LEAF: &str = "toolbox-hub";

static CONFIG_DIR: OnceLock<PathBuf> = OnceLock::new();
static DATA_DIR: OnceLock<PathBuf> = OnceLock::new();

/// 启动时定下两个目录（`None` 表示「用默认/环境变量」）。
///
/// 只认第一次调用：之后再调也不改 —— 运行中途挪配置目录会让「已经读过的」
/// 和「接下来要写的」落到两个地方。
pub fn configure(config_dir: Option<PathBuf>, data_dir: Option<PathBuf>) {
    let _ = CONFIG_DIR.set(config_dir.unwrap_or_else(default_config_dir));
    let _ = DATA_DIR.set(data_dir.unwrap_or_else(default_data_dir));
}

/// 配置目录（已经定下来了就直接给）。
pub fn config_dir() -> PathBuf {
    CONFIG_DIR.get().cloned().unwrap_or_else(default_config_dir)
}

/// 数据目录。
pub fn data_dir() -> PathBuf {
    DATA_DIR.get().cloned().unwrap_or_else(default_data_dir)
}

fn default_config_dir() -> PathBuf {
    if let Some(dir) = env::var_os(CONFIG_ENV) {
        return PathBuf::from(dir);
    }
    xdg("XDG_CONFIG_HOME", ".config").join(CONFIG_LEAF)
}

fn default_data_dir() -> PathBuf {
    if let Some(dir) = env::var_os(DATA_ENV) {
        return PathBuf::from(dir);
    }
    xdg("XDG_DATA_HOME", ".local/share").join(DATA_LEAF)
}

/// XDG 那套：环境变量指了就用它（相对路径按规范忽略），否则 `$HOME/<fallback>`。
fn xdg(variable: &str, fallback: &str) -> PathBuf {
    if let Some(dir) = env::var_os(variable) {
        let path = PathBuf::from(dir);
        // 规范要求忽略相对路径：相对路径在多进程下含义不稳定
        if path.is_absolute() {
            return path;
        }
    }
    let home = env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
    home.join(fallback)
}

// ── 软件包中心的偏好 ────────────────────────────────────────────────────────

/// `packages.toml` 的内容（每个字段都有默认值，缺了就用默认）。
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PackagePrefs {
    /// 打开时默认只看这些仓库（空 = 全看）。名字就是 pacman.conf 里的段名。
    pub repos: Vec<String>,
    /// 默认打开演练模式。
    pub dry_run: bool,
    /// 清缓存时每个包保留几个版本。
    pub cache_keep: Option<u8>,
    /// 结果表默认排序（写中文标签，和界面上显示的一致）。
    pub sort: Option<String>,
    /// 打开时默认停在哪个模式（搜索 / 已安装 / 新闻 / 维护）。
    pub mode: Option<String>,
}

impl PackagePrefs {
    pub fn cache_keep(&self) -> u8 {
        self.cache_keep.unwrap_or(1).clamp(1, 9)
    }

    /// 默认模式；写错了（或没写）就 `None`（由界面用默认的「搜索」）。
    pub fn mode(&self) -> Option<crate::app::package_view::PackageMode> {
        use crate::app::package_view::PackageMode;
        match self.mode.as_deref().map(str::trim) {
            Some("已安装") => Some(PackageMode::Installed),
            Some("新闻") => Some(PackageMode::News),
            // 「维护」模式见下一条提交（那之前写了也只是回落到默认）
            Some("搜索") => Some(PackageMode::Search),
            _ => None,
        }
    }

    /// 排序：写错了就用默认（相关度），不报错也不崩。
    pub fn sort(&self) -> SortMode {
        match self.sort.as_deref().map(str::trim) {
            Some("名字") => SortMode::Name,
            Some("仓库") => SortMode::Repo,
            Some("得票") => SortMode::Votes,
            Some("版本") => SortMode::Version,
            _ => SortMode::Relevance,
        }
    }
}

/// 一次加载的结果：偏好 + 读文件时踩到的坑（界面上说一句）。
#[derive(Clone, Debug)]
pub struct Loaded<T> {
    pub value: T,
    /// 非致命的问题（读不了、解析失败……）。
    pub problem: Option<String>,
}

pub fn packages_path() -> PathBuf {
    config_dir().join("packages.toml")
}

/// 读软件包中心的偏好；文件不存在就是一份默认值（不算问题）。
pub fn load_packages() -> Loaded<PackagePrefs> {
    let path = packages_path();
    let Ok(text) = fs::read_to_string(&path) else {
        return Loaded {
            value: PackagePrefs::default(),
            problem: None,
        };
    };
    match toml::from_str::<PackagePrefs>(&text) {
        Ok(prefs) => Loaded {
            value: prefs,
            problem: None,
        },
        Err(error) => Loaded {
            value: PackagePrefs::default(),
            // 只说第一行：toml 的报错常常几行，状态行放不下
            problem: Some(format!(
                "{} 读不动（用默认值）：{}",
                path.display(),
                error.message()
            )),
        },
    }
}

/// 第一次跑的时候写一份**带注释的**模板出来。
///
/// 为什么自动写：不写的话没人知道有这么一个文件 —— 功能等于没有。
/// 已经存在就一个字节都不动（你的注释和改动都留着）。
pub fn ensure_packages_template() -> bool {
    let path = packages_path();
    if path.exists() {
        return false;
    }
    let Some(parent) = path.parent() else {
        return false;
    };
    if fs::create_dir_all(parent).is_err() {
        return false;
    }
    fs::write(&path, PACKAGES_TEMPLATE).is_ok()
}

/// 模板内容：每一行都是默认值，注释说明改了会怎样。
const PACKAGES_TEMPLATE: &str = r#"# 软件包中心（工具箱里按 p）的默认偏好。
# 删掉某一行 = 用它的默认值；文件整个删掉 = 全默认，下次启动会再写一份。

# 打开时默认只看这些仓库（空着 = 全都看）。
# 名字就是 /etc/pacman.conf 里的段名，例如 core / extra / multilib / aur。
repos = []

# 打开时是不是直接进「演练模式」（确认之后只显示命令，不动系统）。
dry_run = false

# 清缓存（c 键）时每个包保留几个版本。界面里还能用 [ ] 当场改。
cache_keep = 1

# 结果表默认排序：相关度 / 名字 / 仓库 / 得票 / 版本
sort = "相关度"

# 打开时默认停在哪个模式：搜索 / 已安装 / 新闻 / 维护
mode = "搜索"
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_sane_without_a_file() {
        let prefs = PackagePrefs::default();
        assert!(prefs.repos.is_empty(), "默认全看");
        assert!(!prefs.dry_run);
        assert_eq!(prefs.cache_keep(), 1);
        assert_eq!(prefs.sort(), SortMode::Relevance);
    }

    /// 写错的值退化成默认，不报错更不崩 —— 配置文件是手改的，手就会打错。
    #[test]
    fn bad_values_fall_back_instead_of_failing() {
        let prefs: PackagePrefs = toml::from_str(
            r#"
            repos = ["core", "extra"]
            dry_run = true
            cache_keep = 99
            sort = "乱写的"
            mode = "无此模式"
            "#,
        )
        .expect("字段名对就该能解析");

        assert_eq!(prefs.repos, vec!["core", "extra"]);
        assert!(prefs.dry_run);
        assert_eq!(prefs.cache_keep(), 9, "越界要夹住");
        assert_eq!(prefs.sort(), SortMode::Relevance, "不认识的排序回默认");
    }

    /// 字段名打错要**报出来**（deny_unknown_fields），但仍然是默认值 ——
    /// 最坏的情况是「你以为配上了，其实没有」，那个必须说。
    #[test]
    fn unknown_fields_are_reported_but_not_fatal() {
        let error = toml::from_str::<PackagePrefs>("dry_run = true\nkeep = 3\n")
            .expect_err("拼错的字段名该被拦住");
        assert!(error.message().contains("keep"), "{}", error.message());
    }

    /// 环境变量没设（或用的是相对路径）时回落到 `$HOME/.config`。
    #[test]
    fn xdg_falls_back_to_home() {
        let base = xdg("TOOLBOX_HUB_TEST_XDG_SHOULD_NOT_EXIST", ".config");
        assert!(
            base.to_string_lossy().starts_with('/'),
            "回落结果必须是绝对路径：{}",
            base.display()
        );
        assert!(base.ends_with(".config"));

        // 默认配置目录 = 上面那个 + 应用名
        assert!(default_config_dir().ends_with(CONFIG_LEAF));
        assert!(default_data_dir().ends_with(DATA_LEAF));
    }
}
