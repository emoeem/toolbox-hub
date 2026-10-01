//! 执行历史与产物留存。
//!
//! 历史存成一行一条的纯文本（默认 `~/.local/share/toolbox-hub/history.log`），
//! 不引数据库、不引序列化框架：
//!
//! ```text
//! <epoch 秒>\t<ok|fail>\t<毫秒>\t<工具 id>\t<工具名>\targv0\x1fargv1\x1f…
//! ```
//!
//! * 用 `\x1f`（单元分隔符）拼 argv：参数里带空格、制表符也不会串行；
//! * 标成 `sensitive` 的参数值**在记录前**就被换成 `***`（见
//!   [`crate::model::Action::redacted`]）；
//! * 只保留最近 [`MAX_ENTRIES`] 条，超了就重写文件（纯文本，重写代价可忽略）；
//! * 除了 argv，还存一份**当时的表单取值**（`key=value`）—— 这样才能「把上次那套
//!   参数填回表单改一改再跑」，而不只是原样重跑。老记录没有这一列也能读
//!   （当成空）。
//!
//! 读写的核心函数都接受**显式路径**（[`append_to`] / [`load_from`] /
//! [`save_output_to`]），公开的 [`append`] / [`load`] / [`save_output`] 只是把默认
//! 路径接上。这样测试不必去改进程级环境变量 —— 那样会在并行测试里互相踩。
//!
//! 输出留存：[`save_output`] 把捕获到的输出落成文件，[`copy_to_clipboard`]
//! 送进系统剪贴板。

use std::{
    collections::BTreeMap,
    env, fs, io,
    io::Write,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{SystemTime, UNIX_EPOCH},
};

/// 最多保留多少条。
pub const MAX_ENTRIES: usize = 500;

/// 覆盖数据目录的环境变量（换机 / 测试时用；历史和输出都放它下面）。
pub const DATA_ENV: &str = "TOOLBOX_HUB_DATA";

const FIELD_SEP: char = '\t';
const ARG_SEP: char = '\u{1f}';
const VALUE_SEP: char = '\u{1e}';

/// 一次执行。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    /// Unix 时间戳（秒）。存时间戳而不是格式化字符串，避免引入日期库。
    pub epoch: u64,
    pub tool_id: String,
    pub tool_name: String,
    /// 真正执行的参数（**已经**做过敏感值替换）。
    pub argv: Vec<String>,
    pub success: bool,
    pub millis: u128,
    /// 当时的表单取值（参数键 → 值），用于「回填再改」。
    pub values: BTreeMap<String, String>,
}

impl Entry {
    /// 给人看的一行命令。
    pub fn command_line(&self) -> String {
        self.argv.join(" ")
    }

    /// 相对时间：`刚刚` / `12 分钟前` / `3 小时前` / `2 天前`。
    pub fn ago(&self) -> String {
        ago(self.epoch)
    }

    /// 一行的状态文本。
    pub fn status(&self) -> &'static str {
        if self.success { "成功" } else { "失败" }
    }

    fn line(&self) -> String {
        let argv = self
            .argv
            .iter()
            .map(|item| escape(item))
            .collect::<Vec<_>>()
            .join(&ARG_SEP.to_string());

        // 取值列：`key=value` 用另一个分隔符连起来（两个都转义过，值里带分隔符也不怕）。
        let values = self
            .values
            .iter()
            .map(|(key, value)| format!("{}={}", escape(key), escape(value)))
            .collect::<Vec<_>>()
            .join(&VALUE_SEP.to_string());

        format!(
            "{}{sep}{}{sep}{}{sep}{}{sep}{}{sep}{}{sep}{}",
            self.epoch,
            if self.success { "ok" } else { "fail" },
            self.millis,
            escape(&self.tool_id),
            escape(&self.tool_name),
            argv,
            values,
            sep = FIELD_SEP,
        )
    }

    fn parse(line: &str) -> Option<Self> {
        let mut fields = line.split(FIELD_SEP);
        let epoch = fields.next()?.parse().ok()?;
        let success = match fields.next()? {
            "ok" => true,
            "fail" => false,
            _ => return None,
        };
        let millis = fields.next()?.parse().ok()?;
        let tool_id = unescape(fields.next()?);
        let tool_name = unescape(fields.next()?);
        // argv 允许为空（无参数脚本）。
        let argv = match fields.next() {
            Some(raw) if !raw.is_empty() => raw.split(ARG_SEP).map(unescape).collect(),
            _ => Vec::new(),
        };
        // 老记录没有这一列 → 空表（照旧能读）。
        let values = match fields.next() {
            Some(raw) if !raw.is_empty() => raw
                .split(VALUE_SEP)
                .filter_map(|pair| pair.split_once('='))
                .map(|(key, value)| (unescape(key), unescape(value)))
                .collect(),
            _ => BTreeMap::new(),
        };

        Some(Self {
            epoch,
            tool_id,
            tool_name,
            argv,
            success,
            millis,
            values,
        })
    }
}

/// 把可能含有分隔符的文本转义掉。
///
/// 参数里完全可能带制表符甚至换行（例如粘贴进来的多行文本），
/// 不转义就会把行结构撑破 —— 这是测试抓出来的真问题，不是假想。
fn escape(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for ch in value.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '\t' => out.push_str("\\t"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            ARG_SEP => out.push_str("\\u{1f}"),
            VALUE_SEP => out.push_str("\\u{1e}"),
            other => out.push(other),
        }
    }
    out
}

/// [`escape`] 的逆操作。
fn unescape(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut chars = value.chars();
    while let Some(ch) = chars.next() {
        if ch != '\\' {
            out.push(ch);
            continue;
        }
        match chars.next() {
            Some('t') => out.push('\t'),
            Some('n') => out.push('\n'),
            Some('r') => out.push('\r'),
            Some('\\') => out.push('\\'),
            Some('u') => {
                // `\u{1f}`（argv 分隔符）与 `\u{1e}`（取值分隔符）
                let mut digits = String::new();
                for _ in 0..4 {
                    if let Some(ch) = chars.next() {
                        digits.push(ch);
                    }
                }
                out.push(if digits.contains('e') {
                    VALUE_SEP
                } else {
                    ARG_SEP
                });
            }
            Some(other) => {
                out.push('\\');
                out.push(other);
            }
            None => out.push('\\'),
        }
    }
    out
}

/// 当前时间的 Unix 秒。
pub fn now_epoch() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or(0)
}

/// 相对时间文本。
pub fn ago(epoch: u64) -> String {
    let seconds = now_epoch().saturating_sub(epoch);
    match seconds {
        0..=59 => String::from("刚刚"),
        60..=3599 => format!("{} 分钟前", seconds / 60),
        3600..=86_399 => format!("{} 小时前", seconds / 3600),
        _ => format!("{} 天前", seconds / 86_400),
    }
}

/// 数据目录：`$TOOLBOX_HUB_DATA` > `~/.local/share/toolbox-hub`。
pub fn data_dir() -> PathBuf {
    if let Some(raw) = env::var_os(DATA_ENV) {
        return PathBuf::from(raw);
    }
    env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".local/share/toolbox-hub")
}

/// 历史文件位置。
pub fn path() -> PathBuf {
    data_dir().join("history.log")
}

/// 输出留存的目录。
pub fn output_dir() -> PathBuf {
    data_dir().join("output")
}

/// 追加一条；超过 [`MAX_ENTRIES`] 就只保留最近的。
pub fn append(entry: &Entry) -> io::Result<()> {
    append_to(&path(), entry)
}

/// 读最近 `limit` 条，**新的在前**。
pub fn load(limit: usize) -> Vec<Entry> {
    load_from(&path(), limit)
}

/// 把一段输出存成文件，返回存到哪了。
pub fn save_output(tool_name: &str, body: &str) -> io::Result<PathBuf> {
    save_output_to(&output_dir(), tool_name, body)
}

pub(crate) fn append_to(file: &Path, entry: &Entry) -> io::Result<()> {
    if let Some(parent) = file.parent() {
        fs::create_dir_all(parent)?;
    }

    let mut lines: Vec<String> = fs::read_to_string(file)
        .unwrap_or_default()
        .lines()
        .map(str::to_string)
        .collect();
    lines.push(entry.line());

    if lines.len() > MAX_ENTRIES {
        let keep_from = lines.len() - MAX_ENTRIES;
        lines.drain(..keep_from);
    }

    let mut text = lines.join("\n");
    text.push('\n');
    fs::write(file, text)
}

pub(crate) fn load_from(file: &Path, limit: usize) -> Vec<Entry> {
    let Ok(text) = fs::read_to_string(file) else {
        return Vec::new();
    };
    text.lines()
        .rev()
        .filter_map(Entry::parse)
        .take(limit)
        .collect()
}

/// 文件名带上时间戳与工具名，所以不会互相覆盖，事后也认得出是哪次跑的。
fn save_output_to(dir: &Path, tool_name: &str, body: &str) -> io::Result<PathBuf> {
    fs::create_dir_all(dir)?;

    let slug: String = tool_name
        .chars()
        .map(|ch| if ch.is_alphanumeric() { ch } else { '-' })
        .collect();
    let path = dir.join(format!("{}-{}.log", now_epoch(), slug.trim_matches('-')));
    fs::write(&path, body)?;
    Ok(path)
}

/// 把文本送进系统剪贴板（Wayland 用 `wl-copy`，X11 退回 `xclip`）。
///
/// 尽力而为：两个都没有就返回 `false`，调用方给一句提示即可，不算错误。
pub fn copy_to_clipboard(text: &str) -> bool {
    let candidates: [(&str, &[&str]); 2] =
        [("wl-copy", &[]), ("xclip", &["-selection", "clipboard"])];

    for (program, args) in candidates {
        let Ok(mut child) = Command::new(program)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        else {
            continue;
        };

        if let Some(mut stdin) = child.stdin.take() {
            let _ = stdin.write_all(text.as_bytes());
            // stdin 在这里 drop，子进程才能看到 EOF。
        }
        if child.wait().map(|status| status.success()).unwrap_or(false) {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        path::PathBuf,
        time::{SystemTime, UNIX_EPOCH},
    };

    use std::collections::BTreeMap;

    use super::{Entry, MAX_ENTRIES, append_to, load_from, now_epoch, save_output_to};

    /// 独立的临时目录（不碰环境变量，所以并行测试也安全）。
    fn temp_dir(tag: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("toolbox-hub-history-{tag}-{nanos}"));
        fs::create_dir_all(&dir).expect("mkdir");
        dir
    }

    fn temp_file(tag: &str) -> PathBuf {
        temp_dir(tag).join("history.log")
    }

    fn entry(tool: &str, argv: &[&str], success: bool) -> Entry {
        Entry {
            epoch: 1_700_000_000,
            tool_id: format!("manifest:{tool}"),
            tool_name: tool.to_string(),
            argv: argv.iter().map(|item| (*item).to_string()).collect(),
            success,
            millis: 120,
            values: BTreeMap::new(),
        }
    }

    #[test]
    fn appended_entries_come_back_newest_first() {
        let file = temp_file("append");
        append_to(&file, &entry("first", &["/bin/echo", "1"], true)).expect("append");
        append_to(&file, &entry("second", &["/bin/echo", "2"], false)).expect("append");

        let loaded = load_from(&file, 10);
        assert_eq!(loaded.len(), 2);
        assert_eq!(loaded[0].tool_name, "second", "新的在前");
        assert!(!loaded[0].success);
        assert_eq!(loaded[1].tool_name, "first");

        fs::remove_dir_all(file.parent().expect("parent")).expect("cleanup");
    }

    /// 参数里带空格、中文、制表符、引号都要能原样回来。
    #[test]
    fn awkward_arguments_survive_a_round_trip() {
        let file = temp_file("awkward");
        let argv = [
            "/bin/echo",
            "两个 空格",
            "中文 参数",
            "带\t制表符",
            "it's \"quoted\"",
            "x; rm -rf /",
        ];
        append_to(&file, &entry("awkward", &argv, true)).expect("append");

        let loaded = load_from(&file, 1);
        assert_eq!(loaded.len(), 1);
        let expected: Vec<String> = argv.iter().map(|item| (*item).to_string()).collect();
        assert_eq!(loaded[0].argv, expected, "argv 必须一字不差地回来");

        fs::remove_dir_all(file.parent().expect("parent")).expect("cleanup");
    }

    #[test]
    fn an_entry_without_arguments_round_trips() {
        let file = temp_file("noargs");
        append_to(&file, &entry("plain", &[], true)).expect("append");

        let loaded = load_from(&file, 1);
        assert_eq!(loaded.len(), 1);
        assert!(loaded[0].argv.is_empty());
        assert_eq!(loaded[0].command_line(), "");

        fs::remove_dir_all(file.parent().expect("parent")).expect("cleanup");
    }

    #[test]
    fn only_the_most_recent_entries_are_kept() {
        let file = temp_file("trim");
        for index in 0..(MAX_ENTRIES + 5) {
            let mut item = entry("bulk", &["/bin/true"], true);
            item.millis = index as u128;
            append_to(&file, &item).expect("append");
        }

        let loaded = load_from(&file, MAX_ENTRIES + 100);
        assert_eq!(loaded.len(), MAX_ENTRIES, "不该无限增长");
        assert_eq!(
            loaded[0].millis,
            (MAX_ENTRIES + 4) as u128,
            "留下的应是最新的"
        );

        fs::remove_dir_all(file.parent().expect("parent")).expect("cleanup");
    }

    #[test]
    fn broken_lines_are_skipped_instead_of_killing_the_list() {
        let file = temp_file("broken");
        append_to(&file, &entry("good", &["/bin/true"], true)).expect("append");

        let mut text = fs::read_to_string(&file).expect("read");
        text.push_str("这不是一条记录\n");
        text.push_str("1700000000\tmaybe\t1\tx\ty\t\n");
        fs::write(&file, text).expect("write");

        let loaded = load_from(&file, 10);
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].tool_name, "good");

        fs::remove_dir_all(file.parent().expect("parent")).expect("cleanup");
    }

    #[test]
    fn missing_history_file_reads_as_empty() {
        let file = temp_file("missing").with_extension("nope");
        assert!(load_from(&file, 10).is_empty());
    }

    /// 表单取值也要能原样回来 —— 「回填再改」全靠它。
    #[test]
    fn form_values_survive_a_round_trip() {
        let file = temp_file("values");
        let mut item = entry("ffmpeg", &["/usr/bin/ffmpeg", "-i", "a.mp4"], true);
        item.values = BTreeMap::from([
            ("input".to_string(), "a.mp4".to_string()),
            ("crf".to_string(), "28".to_string()),
            // 值里带分隔符、换行、等号都要能回来
            ("output".to_string(), "{stem}=改\n用.mp4".to_string()),
        ]);
        append_to(&file, &item).expect("append");

        let loaded = load_from(&file, 1);
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].values, item.values, "取值必须一字不差地回来");

        fs::remove_dir_all(file.parent().expect("parent")).expect("cleanup");
    }

    /// 老记录（没有取值这一列）也要能读。
    #[test]
    fn an_old_record_without_values_still_parses() {
        let file = temp_file("legacy");
        // 老格式：argv 用 \x1f 连起来，没有第 7 列（取值）。
        fs::write(
            &file,
            "1700000000\tok\t120\tmanifest:jq-query\tjq\t/usr/bin/jq\u{1f}.\u{1f}a.json\n",
        )
        .expect("write");

        let loaded = load_from(&file, 1);
        assert_eq!(loaded.len(), 1);
        assert!(loaded[0].values.is_empty(), "老记录没有取值就是空表");
        assert_eq!(loaded[0].argv.len(), 3);

        fs::remove_dir_all(file.parent().expect("parent")).expect("cleanup");
    }

    #[test]
    fn saved_output_lands_in_its_own_file() {
        let dir = temp_dir("save");
        let path = save_output_to(&dir, "jq 查询 JSON", "hello\nworld\n").expect("save");

        assert_eq!(path.parent(), Some(dir.as_path()));
        assert_eq!(fs::read_to_string(&path).expect("read"), "hello\nworld\n");
        assert!(
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.ends_with(".log")),
            "{path:?}"
        );

        fs::remove_dir_all(&dir).expect("cleanup");
    }

    /// 转义/反转义本身也要能对抗分隔符。
    #[test]
    fn escaping_survives_separators_and_backslashes() {
        for raw in [
            "普通",
            "带\t制表符",
            "带\n换行",
            "带\r回车",
            "带\u{1f}单元分隔符",
            "带\\反斜杠",
            "反斜杠后面跟 t: \\t",
        ] {
            assert_eq!(super::unescape(&super::escape(raw)), raw, "原始值: {raw:?}");
        }
    }

    #[test]
    fn ago_is_human_readable() {
        assert_eq!(super::ago(now_epoch()), "刚刚");
        assert_eq!(super::ago(now_epoch().saturating_sub(120)), "2 分钟前");
        assert_eq!(super::ago(now_epoch().saturating_sub(7200)), "2 小时前");
        assert_eq!(super::ago(now_epoch().saturating_sub(3 * 86_400)), "3 天前");
        // 时钟回拨也不该说出奇怪的话。
        assert_eq!(super::ago(now_epoch() + 1000), "刚刚");
    }
}
