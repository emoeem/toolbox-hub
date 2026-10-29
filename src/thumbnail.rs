//! 视频缩略图缓存：把「选中一个 mp4 什么都看不到」变成「看到第一帧」。
//!
//! 生成一张缩略图要调 ffmpeg 解一段视频，动辄几百毫秒 —— 所以结果必须落盘缓存，
//! 否则每次上下翻过这个文件都要重来一遍。缓存放在 `$XDG_CACHE_HOME/toolbox-hub/thumbs`，
//! **整个目录都可以随时删**：删了只会重新生成，不会有别的后果。
//!
//! ## 缓存键为什么带体积和 mtime
//!
//! 只按路径做键会踩到缩略图缓存最经典的那个 bug：
//!
//! ```text
//! 剪一个视频 → 另存为同名文件（或者视频被重新编码）→ 缩略图还是旧的
//! ```
//!
//! 用户看到的是一张**根本不存在的画面**，而且怎么刷新都不变。带上体积和 mtime
//! 之后，文件一变键就变，旧的那张自然被冷落（[`prune`] 会把它清掉）。
//! 这不是密码学意义上的内容指纹（同秒内改成长度相同的文件看不出来），
//! 但对「换个视频」这件事足够了 —— 真要做到逐字节，得先把整个文件读一遍，
//! 那比重新生成缩略图还贵。
//!
//! ## 两条自律
//!
//! * ffmpeg 用 **argv** 调用，绝不拼 `sh -c`：文件名里一个空格或分号，
//!   拼出来的命令就不是原来那条了；
//! * 先写临时文件再 `rename`：半张 PNG 被当成成品缓存下来，以后每次都画不出来。

use std::{
    ffi::OsStr,
    fs,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use sha2::{Digest, Sha256};

/// 覆盖 ffmpeg 的位置（测试注入桩程序用）。
pub const FFMPEG_ENV: &str = "TOOLBOX_HUB_FFMPEG";

/// 缓存目录里放缩略图的那个子目录。
const THUMBS_LEAF: &str = "thumbs";

/// 一张缩略图最多能生成多久（超时就放弃，界面不能被一个坏文件拖住）。
pub const GENERATE_BUDGET: Duration = Duration::from_secs(8);

/// 缓存最多留多少张，超了从最旧的开始删。
///
/// 一张缩略图几十 KB，512 张也就几十 MB —— 上限的意义不是省这点磁盘，
/// 而是别让一个「扫过一万个视频」的目录把卡片塞满。
pub const MAX_CACHED: usize = 512;

/// 缩略图宽度（像素）。终端里能画出来的区域本来就不大，480 足够清晰，
/// 也足够小到解码快。
const WIDTH: u32 = 480;

/// 从第几秒取帧：太靠开头往往是黑场或台标。
const SEEK_SECONDS: &str = "1";

/// 只认视频（音频与图片不在这里：`media.rs` 那套是给扫描用的混合清单）。
const VIDEO_EXTENSIONS: [&str; 12] = [
    "mp4", "mkv", "mov", "avi", "webm", "ts", "m4v", "flv", "mpeg", "mpg", "wmv", "3gp",
];

/// 是不是视频文件（按后缀，不区分大小写）。
pub fn is_video(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .map(|extension| extension.to_lowercase())
        .is_some_and(|extension| VIDEO_EXTENSIONS.contains(&extension.as_str()))
}

/// 缓存根目录：`TOOLBOX_HUB_CACHE` > `$XDG_CACHE_HOME` > `$HOME/.cache`。
///
/// 和 [`crate::config::cache_dir`] 是同一套优先级，但这里把环境值当参数收进来 ——
/// 测试要能指定自己的目录，而改 `std::env` 在 edition 2024 里是 `unsafe`。
pub fn cache_dir_from(
    explicit: Option<&OsStr>,
    xdg_cache_home: Option<&OsStr>,
    home: Option<&OsStr>,
) -> PathBuf {
    if let Some(dir) = explicit {
        let path = PathBuf::from(dir);
        // 相对路径在多进程下含义不稳定，按 XDG 规范的惯例忽略它。
        if path.is_absolute() {
            return path.join(THUMBS_LEAF);
        }
    }
    if let Some(dir) = xdg_cache_home {
        let path = PathBuf::from(dir);
        if path.is_absolute() {
            return path.join("toolbox-hub").join(THUMBS_LEAF);
        }
    }
    home.map(PathBuf::from)
        .unwrap_or_default()
        .join(".cache/toolbox-hub")
        .join(THUMBS_LEAF)
}

/// 真实环境下的缓存目录。
pub fn cache_dir() -> PathBuf {
    cache_dir_from(
        std::env::var_os("TOOLBOX_HUB_CACHE").as_deref(),
        std::env::var_os("XDG_CACHE_HOME").as_deref(),
        std::env::var_os("HOME").as_deref(),
    )
}

/// 缓存键：绝对路径 + 体积 + mtime。见模块头对「为什么带后两个」的说明。
pub fn cache_key(path: &Path, size: u64, mtime_secs: i64) -> String {
    let absolute = fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let mut hasher = Sha256::new();
    hasher.update(absolute.as_os_str().as_encoded_bytes());
    hasher.update(size.to_le_bytes());
    hasher.update(mtime_secs.to_le_bytes());
    let digest = format!("{:x}", hasher.finalize());
    // 取前 32 位十六进制（128 bit）：撞键的概率低到不用管，文件名也短。
    digest[..32].to_string()
}

fn mtime_secs(meta: &fs::Metadata) -> i64 {
    meta.modified()
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map(|since| since.as_secs() as i64)
        .unwrap_or(0)
}

/// 一个文件的缓存键（读不到 metadata 就是 `None`）。
pub fn key_for(path: &Path) -> Option<String> {
    let meta = fs::metadata(path).ok()?;
    Some(cache_key(path, meta.len(), mtime_secs(&meta)))
}

/// 用哪个 ffmpeg：环境变量优先，否则 PATH 上的 `ffmpeg`。
pub fn ffmpeg_program() -> String {
    std::env::var(FFMPEG_ENV)
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| String::from("ffmpeg"))
}

/// 拿一张缩略图（命中缓存直接返回，否则调 ffmpeg 生成）。
pub fn ensure(path: &Path) -> Result<PathBuf, String> {
    ensure_with(&cache_dir(), &ffmpeg_program(), path, GENERATE_BUDGET)
}

/// 同上，但缓存目录、ffmpeg 位置、时间预算都由调用方给（测试直接用这个）。
pub fn ensure_with(
    dir: &Path,
    ffmpeg: &str,
    path: &Path,
    budget: Duration,
) -> Result<PathBuf, String> {
    let key = key_for(path).ok_or_else(|| format!("读不到文件信息：{}", path.display()))?;
    let target = dir.join(format!("{key}.png"));

    // 命中：文件还在、还不是空的。空文件是「上次写到一半崩了」的痕迹。
    if fs::metadata(&target).is_ok_and(|meta| meta.len() > 0) {
        return Ok(target);
    }

    fs::create_dir_all(dir).map_err(|error| format!("建不了缓存目录：{error}"))?;

    // 先写临时文件：半张图不能被当成成品缓存下来。
    //
    // 名字**必须以 `.png` 结尾**：`-f image2` 是按输出文件的后缀挑编码器的，
    // 后缀不认识就退回 mjpeg —— 那样写出来的是一个叫 `xxx.png` 的 JPEG，
    // 之后每次解码都失败（这个坑是靠真跑 ffmpeg 的那条测试抓到的）。
    let temporary = dir.join(format!(".{key}.{}.tmp.png", std::process::id()));
    let result = run_ffmpeg(ffmpeg, path, &temporary, SEEK_SECONDS, budget).or_else(|first| {
        // 从 1 秒处取不到帧很常见：不足 1 秒的短片、或者开头就没有关键帧。
        // 退到第 0 秒再试一次，两次都失败才认输。
        run_ffmpeg(ffmpeg, path, &temporary, "0", budget)
            .map_err(|second| format!("{first}；从头取帧也不行：{second}"))
    });

    match result {
        Ok(()) => {
            fs::rename(&temporary, &target).map_err(|error| {
                let _ = fs::remove_file(&temporary);
                format!("挪不了缩略图：{error}")
            })?;
            prune(dir, MAX_CACHED);
            Ok(target)
        }
        Err(error) => {
            let _ = fs::remove_file(&temporary);
            Err(error)
        }
    }
}

/// 调一次 ffmpeg。成功 = 退出码 0 **且**真的产出了非空文件。
fn run_ffmpeg(
    ffmpeg: &str,
    input: &Path,
    output: &Path,
    seek: &str,
    budget: Duration,
) -> Result<(), String> {
    let mut child = Command::new(ffmpeg)
        .args([
            "-nostdin",
            "-loglevel",
            "error",
            "-y",
            // -ss 放在 -i 前面是「快速定位」：只跳到关键帧，不用先解完整条流。
            "-ss",
            seek,
            "-i",
        ])
        .arg(input)
        .args(["-frames:v", "1", "-vf"])
        .arg(format!(
            "scale={WIDTH}:-1:force_original_aspect_ratio=decrease"
        ))
        // 编码器写死 png：不靠文件名后缀去猜（见上面临时文件名的注释）。
        .args(["-c:v", "png", "-f", "image2"])
        .arg(output)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                format!("没找到 {ffmpeg}（装一个 ffmpeg，或设 {FFMPEG_ENV} 指到它）")
            } else {
                format!("起不了 {ffmpeg}：{error}")
            }
        })?;

    // 自己看着时间：坏文件能让 ffmpeg 卡很久，而界面只该等这么一会儿。
    // 轮询 + 超时杀进程这一份在 `runtime::wait_with_timeout` 里（探时长也用同一份）。
    let status = match crate::runtime::wait_with_timeout(&mut child, budget) {
        crate::runtime::WaitOutcome::Exited(status) => status,
        crate::runtime::WaitOutcome::TimedOut => {
            return Err(format!("ffmpeg 超过 {} 秒没出图", budget.as_secs()));
        }
        crate::runtime::WaitOutcome::Failed(error) => {
            return Err(format!("等 ffmpeg 失败：{error}"));
        }
    };

    if !status.success() {
        let mut reason = String::new();
        if let Some(mut stderr) = child.stderr.take() {
            use std::io::Read;
            let _ = stderr.read_to_string(&mut reason);
        }
        let reason = reason.trim();
        return Err(if reason.is_empty() {
            format!("ffmpeg 退出码 {}", status.code().unwrap_or(-1))
        } else {
            format!("ffmpeg 报错：{}", reason.lines().next().unwrap_or(reason))
        });
    }

    if fs::metadata(output).map(|meta| meta.len()).unwrap_or(0) == 0 {
        return Err(String::from("ffmpeg 说成功，但没写出图"));
    }
    Ok(())
}

/// 缓存太多就从最旧的开始删。删除失败不算错：缓存坏掉不该影响预览。
fn prune(dir: &Path, keep: usize) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    let mut files: Vec<(SystemTime, PathBuf)> = vec![];
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|extension| extension.to_str()) != Some("png") {
            continue;
        }
        let Ok(meta) = entry.metadata() else {
            continue;
        };
        files.push((meta.modified().unwrap_or(UNIX_EPOCH), path));
    }
    if files.len() <= keep {
        return;
    }
    files.sort();
    let excess = files.len() - keep;
    for (_, path) in files.into_iter().take(excess) {
        let _ = fs::remove_file(path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("toolbox-hub-thumb-{tag}-{nanos}"));
        fs::create_dir_all(&dir).expect("mkdir");
        dir
    }

    /// 造一个「假视频」与一个「假 ffmpeg」。
    ///
    /// 桩程序把预置的 PNG 复制到 ffmpeg 本该写的位置，并把每次调用记进日志 ——
    /// 有日志才能证明「第二次确实没再调 ffmpeg」。
    fn stub_ffmpeg(dir: &Path) -> (PathBuf, PathBuf, PathBuf) {
        let png = dir.join("frame.png");
        image::DynamicImage::new_rgb8(8, 6)
            .save(&png)
            .expect("写测试图");

        let log = dir.join("calls.log");
        let program = dir.join("fake-ffmpeg");
        fs::write(
            &program,
            format!(
                r#"#!/usr/bin/env bash
set -euo pipefail
echo "called $*" >> "{log}"
for arg in "$@"; do out="$arg"; done
cp "{png}" "$out"
"#,
                log = log.display(),
                png = png.display()
            ),
        )
        .expect("写桩程序");
        let mut permissions = fs::metadata(&program).expect("meta").permissions();
        use std::os::unix::fs::PermissionsExt;
        permissions.set_mode(0o755);
        fs::set_permissions(&program, permissions).expect("chmod");

        let video = dir.join("clip.mp4");
        fs::write(&video, vec![0u8; 4096]).expect("写假视频");
        (program, video, log)
    }

    fn call_count(log: &Path) -> usize {
        fs::read_to_string(log)
            .map(|text| text.lines().count())
            .unwrap_or(0)
    }

    #[test]
    fn only_video_suffixes_count_as_video() {
        assert!(is_video(Path::new("/x/a.mp4")));
        assert!(is_video(Path::new("/x/a.MKV")));
        assert!(is_video(Path::new("/x/a.webm")));
        // 音频与图片不归这里管：它们的预览各有各的路。
        assert!(!is_video(Path::new("/x/a.mp3")));
        assert!(!is_video(Path::new("/x/a.png")));
        assert!(!is_video(Path::new("/x/a")));
    }

    #[test]
    fn cache_key_changes_when_the_file_changes() {
        let dir = temp_dir("key");
        let file = dir.join("clip.mp4");
        fs::write(&file, vec![0u8; 100]).expect("write");

        let first = key_for(&file).expect("key");
        // 同一个文件、什么都没动：键必须稳定，否则缓存永远不命中。
        assert_eq!(key_for(&file).expect("key"), first);

        // 换成另一个文件（长度不同）：键必须变。
        let other = dir.join("clip2.mp4");
        fs::write(&other, vec![0u8; 200]).expect("write");
        assert_ne!(key_for(&other).expect("key"), first);

        // 同名文件被替换（体积不同）—— 这正是「缩略图还是旧的」那个 bug 的场景。
        fs::write(&file, vec![0u8; 101]).expect("rewrite");
        assert_ne!(
            key_for(&file).expect("key"),
            first,
            "文件内容变了，缓存键必须跟着变"
        );

        fs::remove_dir_all(&dir).expect("cleanup");
    }

    #[test]
    fn cache_dir_follows_xdg_and_can_be_overridden() {
        use std::ffi::OsStr;
        assert_eq!(
            cache_dir_from(
                Some(OsStr::new("/tmp/explicit")),
                Some(OsStr::new("/xdg")),
                Some(OsStr::new("/home/u"))
            ),
            PathBuf::from("/tmp/explicit/thumbs")
        );
        assert_eq!(
            cache_dir_from(None, Some(OsStr::new("/xdg")), Some(OsStr::new("/home/u"))),
            PathBuf::from("/xdg/toolbox-hub/thumbs")
        );
        assert_eq!(
            cache_dir_from(None, None, Some(OsStr::new("/home/u"))),
            PathBuf::from("/home/u/.cache/toolbox-hub/thumbs")
        );
        // 相对路径按 XDG 惯例忽略，别在多进程下指向不同的地方。
        assert_eq!(
            cache_dir_from(
                Some(OsStr::new("relative")),
                None,
                Some(OsStr::new("/home/u"))
            ),
            PathBuf::from("/home/u/.cache/toolbox-hub/thumbs")
        );
    }

    #[test]
    fn generating_a_thumbnail_runs_once_and_then_hits_the_cache() {
        let dir = temp_dir("once");
        let (ffmpeg, video, log) = stub_ffmpeg(&dir);
        let cache = dir.join("cache");

        let first = ensure_with(
            &cache,
            ffmpeg.to_str().expect("utf8"),
            &video,
            GENERATE_BUDGET,
        )
        .expect("第一次生成");
        assert!(first.is_file(), "缩略图应当落到缓存目录里");
        assert_eq!(call_count(&log), 1);

        let second = ensure_with(
            &cache,
            ffmpeg.to_str().expect("utf8"),
            &video,
            GENERATE_BUDGET,
        )
        .expect("第二次命中");
        assert_eq!(second, first);
        assert_eq!(call_count(&log), 1, "命中缓存时不该再调 ffmpeg");

        // 缓存被删掉之后要能自己重建 —— 缓存目录本来就允许随时删。
        fs::remove_file(&first).expect("删缓存");
        ensure_with(
            &cache,
            ffmpeg.to_str().expect("utf8"),
            &video,
            GENERATE_BUDGET,
        )
        .expect("重建");
        assert_eq!(call_count(&log), 2);

        fs::remove_dir_all(&dir).expect("cleanup");
    }

    #[test]
    fn a_changed_video_does_not_reuse_the_old_thumbnail() {
        let dir = temp_dir("changed");
        let (ffmpeg, video, log) = stub_ffmpeg(&dir);
        let cache = dir.join("cache");
        let program = ffmpeg.to_str().expect("utf8");

        let first = ensure_with(&cache, program, &video, GENERATE_BUDGET).expect("生成");
        // 换了内容（这里用追加字节模拟「重新编码过的同一个文件名」）。
        let mut bytes = fs::read(&video).expect("read");
        bytes.extend_from_slice(&[1, 2, 3]);
        fs::write(&video, bytes).expect("rewrite");

        let second = ensure_with(&cache, program, &video, GENERATE_BUDGET).expect("重新生成");
        assert_ne!(second, first, "内容变了就是另一张缩略图");
        assert_eq!(call_count(&log), 2);

        fs::remove_dir_all(&dir).expect("cleanup");
    }

    #[test]
    fn a_broken_ffmpeg_is_reported_not_panicked() {
        let dir = temp_dir("broken");
        let (_, video, _) = stub_ffmpeg(&dir);
        let cache = dir.join("cache");

        // 不存在的程序：要报「没找到」并带上怎么办，而不是 panic。
        let error =
            ensure_with(&cache, "/no/such/ffmpeg", &video, GENERATE_BUDGET).expect_err("该报错");
        assert!(error.contains("没找到"), "错误信息要说清楚：{error}");

        // 退出码非零的程序：同样要报错，而且不该留下垃圾文件。
        let failing = dir.join("failing-ffmpeg");
        fs::write(&failing, "#!/usr/bin/env bash\necho 'boom' >&2\nexit 1\n").expect("write");
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = fs::metadata(&failing).expect("meta").permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&failing, permissions).expect("chmod");

        let error = ensure_with(
            &cache,
            failing.to_str().expect("utf8"),
            &video,
            GENERATE_BUDGET,
        )
        .expect_err("该报错");
        assert!(error.contains("boom"), "要把 ffmpeg 的原因带出来：{error}");

        let leftovers: Vec<_> = fs::read_dir(&cache)
            .map(|entries| {
                entries
                    .flatten()
                    .map(|entry| entry.file_name().to_string_lossy().to_string())
                    .collect()
            })
            .unwrap_or_default();
        assert!(
            leftovers.iter().all(|name| !name.starts_with('.')),
            "失败的中间文件要清掉，实际留下：{leftovers:?}"
        );

        fs::remove_dir_all(&dir).expect("cleanup");
    }

    #[test]
    fn a_hanging_ffmpeg_is_killed_at_the_budget() {
        let dir = temp_dir("hang");
        let (_, video, _) = stub_ffmpeg(&dir);
        let cache = dir.join("cache");

        let hanging = dir.join("hanging-ffmpeg");
        fs::write(&hanging, "#!/usr/bin/env bash\nsleep 30\n").expect("write");
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = fs::metadata(&hanging).expect("meta").permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&hanging, permissions).expect("chmod");

        let started = std::time::Instant::now();
        let error = ensure_with(
            &cache,
            hanging.to_str().expect("utf8"),
            &video,
            Duration::from_millis(300),
        )
        .expect_err("该超时");
        assert!(error.contains("没出图"), "{error}");
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "超时之后必须马上回来，实际等了 {:?}",
            started.elapsed()
        );

        fs::remove_dir_all(&dir).expect("cleanup");
    }

    #[test]
    fn the_cache_does_not_grow_without_bound() {
        let dir = temp_dir("prune");
        let cache = dir.join("cache");
        fs::create_dir_all(&cache).expect("mkdir");
        // 逐个写入并让 mtime 有区别（文件系统的时间戳精度可能到秒）。
        for index in 0..20 {
            let path = cache.join(format!("{index:032}.png"));
            fs::write(&path, b"x").expect("write");
            std::thread::sleep(Duration::from_millis(2));
        }
        prune(&cache, 8);
        let left = fs::read_dir(&cache).expect("read").flatten().count();
        assert_eq!(left, 8, "上限之外的应当从最旧的开始删");

        fs::remove_dir_all(&dir).expect("cleanup");
    }

    #[test]
    fn pruning_ignores_non_thumbnail_files() {
        let dir = temp_dir("prune-other");
        let cache = dir.join("cache");
        fs::create_dir_all(&cache).expect("mkdir");
        for index in 0..10 {
            fs::write(cache.join(format!("{index:032}.png")), b"x").expect("write");
        }
        // 用户自己往里放的东西不该被我们删掉（只认 .png）。
        fs::write(cache.join("README.md"), b"do not delete me").expect("write");
        prune(&cache, 3);
        assert!(cache.join("README.md").is_file());
        assert_eq!(fs::read_dir(&cache).expect("read").flatten().count(), 4);

        fs::remove_dir_all(&dir).expect("cleanup");
    }

    #[test]
    fn a_real_video_gets_a_real_thumbnail_when_ffmpeg_is_present() {
        // 真跑 ffmpeg（机器上没装就跳过）。这一段是「调用方式对不对」的唯一证据：
        // 参数写错了，桩程序永远看不出来。
        if Command::new("ffmpeg").arg("-version").output().is_err() {
            return;
        }
        let dir = temp_dir("real");
        let video = dir.join("clip.mp4");
        // 明确指定 **mpeg4**（软件编码器，ffmpeg 自带）：默认挑编码器时，带硬件加速
        // 的发行版会挑到 `h264_rkmpp` 这类设备编码器，在 CI/容器/无设备的环境里
        // 直接初始化失败 —— 那样这个测试会「悄悄跳过」，看着绿其实什么都没验。
        let created = Command::new("ffmpeg")
            .args([
                "-nostdin",
                "-loglevel",
                "error",
                "-y",
                "-f",
                "lavfi",
                "-i",
                "testsrc=size=160x120:rate=5:duration=3",
                "-c:v",
                "mpeg4",
                "-pix_fmt",
                "yuv420p",
            ])
            .arg(&video)
            .output()
            .expect("造测试视频");
        assert!(
            created.status.success(),
            "造不出测试视频，这条测试就白跑了：{}",
            String::from_utf8_lossy(&created.stderr)
        );

        let cache = dir.join("cache");
        let thumbnail = ensure_with(&cache, "ffmpeg", &video, GENERATE_BUDGET).expect("生成缩略图");
        let decoded = image::open(&thumbnail).expect("缩略图要能解码");
        assert!(decoded.width() <= WIDTH, "缩略图不该超过设定宽度");
        assert!(decoded.width() > 0 && decoded.height() > 0);

        fs::remove_dir_all(&dir).expect("cleanup");
    }
}
