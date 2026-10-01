//! 用户状态：收藏，以及以后可能加进来的偏好。
//!
//! 存 `~/.config/toolbox-hub/state.toml` —— 是**配置**不是数据：
//! 换机器时你要带走的是它，而执行历史（`~/.local/share/...`）是可再生的。
//!
//! ```toml
//! favorites = ["manifest:jq-query", "fftools:fzf-trim-video"]
//! ```
//!
//! 读不出来、写不进去都不该影响使用：收藏只是锦上添花。
//! 解析刻意**宽容**（多余字段忽略、坏文件退化成默认值），
//! 免得手改一次就把收藏全弄丢。

use std::{
    env, fs, io,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};

/// 覆盖状态文件位置的环境变量（换机 / 测试时用）。
pub const STATE_ENV: &str = "TOOLBOX_HUB_STATE";

/// 覆盖启动工作目录的环境变量。
pub const WORK_DIR_ENV: &str = "TOOLBOX_HUB_WORKDIR";

/// 把开头的 `~` 换成 `$HOME`（只认「`~` 单独一个」和「`~/…`」两种写法）。
pub fn expand_home(raw: &str) -> PathBuf {
    let trimmed = raw.trim();
    let Some(home) = env::var_os("HOME").map(PathBuf::from) else {
        return PathBuf::from(trimmed);
    };

    if trimmed == "~" {
        return home;
    }
    match trimmed.strip_prefix("~/") {
        Some(rest) => home.join(rest),
        None => PathBuf::from(trimmed),
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct State {
    /// 收藏的工具 id。顺序不重要，展示时按工具顺序排。
    pub favorites: Vec<String>,
    /// 上次用的工作目录：工具在这个目录里执行，扫目录的脚本也在这里找输入文件。
    pub work_dir: Option<PathBuf>,
    /// 最近用过的目录（新的在前）：选文件与改工作目录时都记一笔，
    /// 下次打开选择器就从这里开始，不用每次从 `/` 翻。
    pub recent_dirs: Vec<PathBuf>,
}

impl State {
    pub fn is_favorite(&self, tool_id: &str) -> bool {
        self.favorites.iter().any(|id| id == tool_id)
    }

    /// 切换收藏，返回切换**之后**是否已收藏。
    pub fn toggle_favorite(&mut self, tool_id: &str) -> bool {
        if let Some(index) = self.favorites.iter().position(|id| id == tool_id) {
            self.favorites.remove(index);
            false
        } else {
            self.favorites.push(tool_id.to_string());
            true
        }
    }

    pub fn favorite_count(&self) -> usize {
        self.favorites.len()
    }
}

/// 状态文件位置：`$TOOLBOX_HUB_STATE` > `<配置目录>/state.toml`。
///
/// 配置目录由 [`crate::config`] 统一定（`--config-dir` > `TOOLBOX_HUB_CONFIG` >
/// `~/.config/toolbox-hub`），这里不再自己拼。
pub fn path() -> PathBuf {
    if let Some(raw) = env::var_os(STATE_ENV) {
        return PathBuf::from(raw);
    }
    crate::config::config_dir().join("state.toml")
}

pub fn load() -> State {
    load_from(&path())
}

pub(crate) fn load_from(file: &Path) -> State {
    fs::read_to_string(file)
        .ok()
        .and_then(|text| toml::from_str(&text).ok())
        .unwrap_or_default()
}

pub(crate) fn save_to(file: &Path, state: &State) -> io::Result<()> {
    if let Some(parent) = file.parent() {
        fs::create_dir_all(parent)?;
    }
    let text = toml::to_string(state).map_err(io::Error::other)?;
    fs::write(file, text)
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        path::PathBuf,
        time::{SystemTime, UNIX_EPOCH},
    };

    use super::{State, load_from, save_to};

    fn temp_file(tag: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("toolbox-hub-state-{tag}-{nanos}"));
        fs::create_dir_all(&dir).expect("mkdir");
        dir.join("state.toml")
    }

    #[test]
    fn toggling_adds_then_removes() {
        let mut state = State::default();
        assert!(!state.is_favorite("a"));
        assert!(state.toggle_favorite("a"), "第一次切换是加上");
        assert!(state.is_favorite("a"));
        assert_eq!(state.favorite_count(), 1);

        assert!(!state.toggle_favorite("a"), "第二次切换是去掉");
        assert!(!state.is_favorite("a"));
        assert_eq!(state.favorite_count(), 0);
    }

    #[test]
    fn favorites_survive_a_round_trip() {
        let file = temp_file("round");
        let mut state = State::default();
        state.toggle_favorite("manifest:jq-query");
        state.toggle_favorite("fftools:fzf-trim-video");
        save_to(&file, &state).expect("save");

        let loaded = load_from(&file);
        assert_eq!(loaded, state, "收藏必须原样回来");

        fs::remove_dir_all(file.parent().expect("parent")).expect("cleanup");
    }

    #[test]
    fn the_remembered_work_dir_survives_a_round_trip() {
        let file = temp_file("workdir");
        let mut state = State::default();
        assert_eq!(state.work_dir, None, "默认没有记住任何目录");

        state.work_dir = Some(PathBuf::from("/home/emo/Pictures"));
        save_to(&file, &state).expect("save");

        let loaded = load_from(&file);
        assert_eq!(loaded.work_dir, state.work_dir, "工作目录必须原样回来");

        fs::remove_dir_all(file.parent().expect("parent")).expect("cleanup");
    }

    #[test]
    fn recent_dirs_survive_a_round_trip() {
        let file = temp_file("recent");
        let mut state = State::default();
        assert!(state.recent_dirs.is_empty());

        state.recent_dirs = vec![
            PathBuf::from("/mnt/media"),
            PathBuf::from("/home/emo/Pictures"),
        ];
        save_to(&file, &state).expect("save");

        let loaded = load_from(&file);
        assert_eq!(loaded.recent_dirs, state.recent_dirs);

        fs::remove_dir_all(file.parent().expect("parent")).expect("cleanup");
    }

    #[test]
    fn expand_home_handles_only_a_leading_tilde() {
        let Some(home) = std::env::var_os("HOME").map(PathBuf::from) else {
            return;
        };
        assert_eq!(super::expand_home("~"), home);
        assert_eq!(super::expand_home("~/Pictures"), home.join("Pictures"));
        assert_eq!(
            super::expand_home("  ~/a  "),
            home.join("a"),
            "两边的空白要去掉"
        );
        assert_eq!(
            super::expand_home("/abs/path"),
            PathBuf::from("/abs/path"),
            "绝对路径原样返回"
        );
        assert_eq!(
            super::expand_home("rel/path"),
            PathBuf::from("rel/path"),
            "相对路径原样返回"
        );
        assert_eq!(
            super::expand_home("~other/x"),
            PathBuf::from("~other/x"),
            "不认 ~user 写法，原样当路径（让它去报不存在）"
        );
    }

    #[test]
    fn a_missing_file_reads_as_empty_and_a_broken_one_degrades_gracefully() {
        let file = temp_file("broken");
        assert_eq!(load_from(&file), State::default(), "没有文件就是空状态");

        fs::write(&file, "这不是 TOML {{{").expect("write");
        assert_eq!(load_from(&file), State::default(), "坏文件不该让程序崩");

        fs::remove_dir_all(file.parent().expect("parent")).expect("cleanup");
    }

    #[test]
    fn unknown_fields_are_tolerated_so_a_typo_does_not_wipe_favorites() {
        let file = temp_file("extra");
        fs::write(&file, "favorites = [\"a\"]\n\"未来才有的字段\" = 1\n").expect("write");

        let loaded = load_from(&file);
        assert_eq!(loaded.favorites, vec!["a".to_string()]);

        fs::remove_dir_all(file.parent().expect("parent")).expect("cleanup");
    }
}
