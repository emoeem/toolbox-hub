//! 「这个目录里有哪些媒体文件」—— 和 FFTools 那批脚本用的是**同一套规则**。
//!
//! 那些脚本靠脚本里的 `fd` 调用扫目录：
//!
//! ```text
//! fd --type f --ignore-case '\.(mp4|mkv|…)$' --hidden --follow \
//!    --exclude .git --exclude node_modules --exclude target --exclude .cache .
//! ```
//!
//! 这里刻意复刻同一套后缀与排除项：**工具箱看到的，就是脚本会看到的**。
//! 不然「我明明有文件」和「脚本说没有」就会对不上，那比不显示更糟。
//!
//! 两个自我保护：最多收 [`DEFAULT_LIMIT`] 个文件、最多扫 [`DEFAULT_BUDGET`] 时间 ——
//! 扫 $HOME 这种大树时不能让界面等它。

use std::{
    fs,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

/// 一次扫描最多收多少个文件。
pub const DEFAULT_LIMIT: usize = 500;

/// 一次扫描最多花多少时间。
pub const DEFAULT_BUDGET: Duration = Duration::from_millis(250);

/// 扫目录时整个跳过的目录名（和脚本里的 `--exclude` 一致）。
const SKIP_DIRS: [&str; 4] = [".git", "node_modules", "target", ".cache"];

/// 认哪些后缀（和脚本里的正则一致）。
const EXTENSIONS: [&str; 26] = [
    "mp4", "mkv", "mov", "avi", "webm", "ts", "m4v", "flv", "mpeg", "mpg", "wmv", "3gp", "mp3",
    "flac", "wav", "aac", "m4a", "opus", "ogg", "wma", "ac3", "aiff", "ape", "jpg", "jpeg", "png",
];

/// 是不是媒体文件（按后缀，不区分大小写）。
pub fn is_media(path: &Path) -> bool {
    let Some(ext) = path.extension().and_then(|ext| ext.to_str()) else {
        return false;
    };
    let ext = ext.to_lowercase();
    EXTENSIONS.contains(&ext.as_str())
}

/// `2.3 MB` 这种给人看的体积（选择器与文件视图共用）。
pub fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KB", "MB", "GB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// 扫出来的一个文件。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MediaFile {
    pub path: PathBuf,
    /// 相对扫描根的路径（显示用，短一些）。
    pub relative: String,
    pub name: String,
    pub size: u64,
}

/// 一次扫描的结果。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ScanResult {
    pub files: Vec<MediaFile>,
    /// 是不是**因为上限或超时**提前收工的（那时候显示要写成「≥N」）。
    pub truncated: bool,
}

/// 从 `root` 往下找媒体文件，按相对路径排序。
pub fn scan(root: &Path) -> ScanResult {
    scan_with(root, DEFAULT_LIMIT, DEFAULT_BUDGET)
}

/// 同上，但可以指定上限与时间预算（测试用）。
pub fn scan_with(root: &Path, limit: usize, budget: Duration) -> ScanResult {
    let started = Instant::now();
    let mut files = Vec::new();
    let mut truncated = false;
    let mut stack = vec![root.to_path_buf()];

    while let Some(dir) = stack.pop() {
        if files.len() >= limit || started.elapsed() > budget {
            truncated = true;
            break;
        }

        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };

        for item in entries.flatten() {
            let path = item.path();
            let name = item.file_name().to_string_lossy().to_string();

            // `file_type()` 不跟随符号链接：软链接目录就不往里钻了（防止绕圈），
            // 但软链接指向的**文件**照样算（下面用 metadata 判断）。
            let Ok(kind) = item.file_type() else {
                continue;
            };

            if kind.is_dir() {
                if !SKIP_DIRS.contains(&name.as_str()) {
                    stack.push(path);
                }
                continue;
            }

            if !is_media(&path) {
                continue;
            }

            let size = fs::metadata(&path).map(|meta| meta.len()).unwrap_or(0);
            files.push(MediaFile {
                relative: path
                    .strip_prefix(root)
                    .unwrap_or(&path)
                    .display()
                    .to_string(),
                path,
                name,
                size,
            });

            if files.len() >= limit {
                truncated = true;
                break;
            }
        }
    }

    files.sort_by(|a, b| a.relative.cmp(&b.relative));
    // 排序之后上限可能截在中间，所以再裁一次，保证数量符合承诺。
    if files.len() > limit {
        files.truncate(limit);
        truncated = true;
    }

    ScanResult { files, truncated }
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        path::PathBuf,
        time::{SystemTime, UNIX_EPOCH},
    };

    use super::{ScanResult, human_size, is_media, scan_with};

    fn temp_tree(tag: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("toolbox-hub-media-{tag}-{nanos}"));
        fs::create_dir_all(dir.join("子目录")).expect("mkdir");
        fs::create_dir_all(dir.join("target")).expect("mkdir");
        fs::create_dir_all(dir.join("node_modules")).expect("mkdir");

        fs::write(dir.join("a.mp4"), vec![0u8; 1024]).expect("write");
        fs::write(dir.join("b.MKV"), vec![0u8; 2048]).expect("write");
        fs::write(dir.join("notes.txt"), "x").expect("write");
        fs::write(dir.join("子目录/c.mov"), "x").expect("write");
        // 这两个目录要被整个跳过（脚本也是这么排除的）
        fs::write(dir.join("target/d.mp4"), "x").expect("write");
        fs::write(dir.join("node_modules/e.mp4"), "x").expect("write");
        dir
    }

    #[test]
    fn human_size_reads_like_a_person_wrote_it() {
        assert_eq!(human_size(0), "0 B");
        assert_eq!(human_size(999), "999 B");
        assert_eq!(human_size(2048), "2.0 KB");
        assert_eq!(human_size(1024 * 1024 * 3 / 2), "1.5 MB");
        assert_eq!(human_size(5 * 1024 * 1024 * 1024), "5.0 GB");
    }

    #[test]
    fn only_media_extensions_count_and_case_does_not_matter() {
        assert!(is_media(PathBuf::from("/x/a.mp4").as_path()));
        assert!(is_media(PathBuf::from("/x/a.MKV").as_path()));
        assert!(!is_media(PathBuf::from("/x/a.txt").as_path()));
        assert!(!is_media(PathBuf::from("/x/a").as_path()));
    }

    #[test]
    fn scanning_finds_media_recursively_and_skips_excluded_dirs() {
        let dir = temp_tree("scan");
        let result = scan_with(&dir, 100, std::time::Duration::from_secs(5));

        let names: Vec<&str> = result.files.iter().map(|file| file.name.as_str()).collect();
        assert_eq!(
            names,
            vec!["a.mp4", "b.MKV", "c.mov"],
            "递归 + 跳过排除目录"
        );
        assert!(!result.truncated);
        assert_eq!(result.files[0].size, 1024, "体积要读出来");
        assert_eq!(result.files[0].relative, "a.mp4");
        assert!(
            result
                .files
                .iter()
                .any(|file| file.relative == "子目录/c.mov"),
            "相对路径要带上子目录"
        );

        fs::remove_dir_all(&dir).expect("cleanup");
    }

    #[test]
    fn the_limit_is_respected_and_reported() {
        let dir = temp_tree("limit");
        let result = scan_with(&dir, 2, std::time::Duration::from_secs(5));

        assert_eq!(result.files.len(), 2, "最多就收 2 个");
        assert!(result.truncated, "被截断要如实说出来（显示成 ≥2）");

        fs::remove_dir_all(&dir).expect("cleanup");
    }

    #[test]
    fn an_empty_or_missing_directory_scans_to_nothing() {
        let missing = PathBuf::from("/nonexistent/toolbox-hub-media");
        assert_eq!(
            scan_with(&missing, 10, std::time::Duration::from_millis(50)),
            ScanResult::default()
        );

        let dir = temp_tree("empty");
        fs::remove_file(dir.join("a.mp4")).expect("rm");
        fs::remove_file(dir.join("b.MKV")).expect("rm");
        fs::remove_file(dir.join("子目录/c.mov")).expect("rm");
        let result = scan_with(&dir, 10, std::time::Duration::from_secs(5));
        assert!(result.files.is_empty());
        assert!(!result.truncated);

        fs::remove_dir_all(&dir).expect("cleanup");
    }
}
