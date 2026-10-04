//! 动态参数候选 —— 「这个字段能填什么」交给机器回答。
//!
//! 从 navi 的 cheatsheet 学来的一条：让用户去记「有哪些分支 / 哪些容器 / ffmpeg
//! 支持哪些编码器」是没道理的，机器本来就知道。所以 manifest 可以写：
//!
//! ```toml
//! [[action.argument]]
//! key = "branch"
//! label = "分支"
//! kind = "dynamic"
//! source = "git-branches"
//! ```
//!
//! source 有两种写法：
//!
//! * **内置名字** —— 见 SOURCES（git-branches / docker-containers /
//!   pacman-packages / ffmpeg-video-codecs …）；
//! * **一条命令** —— command:docker ps --format '{{.Names}}'，或者任何不像内置
//!   名字的字符串。
//!
//! # 这里没有 shell
//!
//! 命令串只在空白与引号处切开，然后作为 **argv** 交给进程（见 split_command）。
//! 所以 command:ls | wc -l 不会去数行数 —— 管道不会被解释，竖线会作为普通参数传给
//! ls，然后 ls 报错。这是刻意的：不引 shell 就没有注入面。要跑复杂的东西，写一个
//! 脚本，然后 source 指向那个脚本。

use std::{
    path::{Path, PathBuf},
    process::Command,
    sync::mpsc::{self, Receiver, Sender, TryRecvError},
    thread::{self, JoinHandle},
};

/// 候选数量上限（防止一条命令吐出几万行把界面拖死）。
pub const MAX_CANDIDATES: usize = 500;

/// 一个内置候选源。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SourceInfo {
    /// 写进 source = "…" 的名字。
    pub id: &'static str,
    pub label: &'static str,
    /// 会跑什么（详情与帮助里显示，用户有权知道）。
    pub command: &'static str,
}

/// 内置候选源。顺序即帮助里的展示顺序。
pub const SOURCES: &[SourceInfo] = &[
    SourceInfo {
        id: "git-branches",
        label: "git 分支",
        command: "git branch --format=%(refname:short)",
    },
    SourceInfo {
        id: "git-tags",
        label: "git 标签",
        command: "git tag",
    },
    SourceInfo {
        id: "git-remotes",
        label: "git 远端",
        command: "git remote",
    },
    SourceInfo {
        id: "docker-containers",
        label: "容器名",
        command: "docker ps --format {{.Names}}",
    },
    SourceInfo {
        id: "docker-images",
        label: "镜像",
        command: "docker images --format {{.Repository}}:{{.Tag}}",
    },
    SourceInfo {
        id: "pacman-packages",
        label: "已装软件包",
        command: "pacman -Qq",
    },
    SourceInfo {
        id: "pacman-explicit",
        label: "显式安装的软件包",
        command: "pacman -Qqe",
    },
    SourceInfo {
        id: "ffmpeg-video-codecs",
        label: "ffmpeg 视频编码器",
        command: "ffmpeg -hide_banner -encoders",
    },
    SourceInfo {
        id: "ffmpeg-audio-codecs",
        label: "ffmpeg 音频编码器",
        command: "ffmpeg -hide_banner -encoders",
    },
    SourceInfo {
        id: "ffmpeg-formats",
        label: "ffmpeg 容器格式",
        command: "ffmpeg -hide_banner -formats",
    },
];

/// 按名字找内置源。
pub fn source(id: &str) -> Option<&'static SourceInfo> {
    SOURCES.iter().find(|source| source.id == id)
}

/// 把一条命令串切成 argv。
///
/// 支持单引号与双引号分组；**没有别的规则** —— 不展开变量、不认管道、不认转义。
/// 落单的引号按普通字符处理，交给命令自己去报错。
pub fn split_command(line: &str) -> Vec<String> {
    let mut parts: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut quote: Option<char> = None;
    let mut started = false;

    for ch in line.chars() {
        match quote {
            Some(active) if ch == active => {
                quote = None;
            }
            Some(_) => current.push(ch),
            None if ch == '\'' || ch == '"' => {
                quote = Some(ch);
                started = true;
            }
            None if ch.is_whitespace() => {
                if started || !current.is_empty() {
                    parts.push(std::mem::take(&mut current));
                    started = false;
                }
            }
            None => {
                current.push(ch);
                started = true;
            }
        }
    }
    if started || !current.is_empty() {
        parts.push(current);
    }
    parts.retain(|part| !part.is_empty());
    parts
}

/// 这条 source 要跑的 argv。内置名字映射到固定命令，其余当命令串切。
pub fn command_for(spec: &str) -> Vec<String> {
    match source(spec.trim()) {
        Some(info) => split_command(info.command),
        None => {
            let raw = spec.trim();
            let rest = raw.strip_prefix("command:").unwrap_or(raw);
            split_command(rest)
        }
    }
}

/// 从命令输出里取候选：逐行去空白、丢空行、去重、截断。
pub fn candidates_from_output(text: &str, limit: usize) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for line in text.lines() {
        let item = line.trim();
        if item.is_empty() || out.iter().any(|existing| existing == item) {
            continue;
        }
        out.push(item.to_string());
        if out.len() >= limit {
            break;
        }
    }
    out
}

/// 解析 ffmpeg 的编码器列表，挑出视频（V）或音频（A）编码器。
///
/// 行形如 「 V....D libx264   H.264 / AVC …」：第一个非空字符是 V/A/S。
pub fn parse_ffmpeg_encoders(text: &str, want: char) -> Vec<String> {
    let mut out = Vec::new();
    for line in text.lines() {
        let trimmed = line.trim_start();
        let mut chars = trimmed.chars();
        if chars.next() != Some(want) {
            continue;
        }
        // 名字是第二个字段（第一个是 V..... 那串标记）。
        if let Some(name) = trimmed.split_whitespace().nth(1)
            && !out.contains(&name.to_string())
        {
            out.push(name.to_string());
        }
    }
    out
}

/// 解析 ffmpeg 的容器格式列表（形如 「 DE mkv」的行）。
pub fn parse_ffmpeg_formats(text: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for line in text.lines() {
        let trimmed = line.trim_start();
        if trimmed.len() < 4 || !line.starts_with(' ') {
            continue;
        }
        if let Some(name) = trimmed.split_whitespace().nth(1)
            && name
                .chars()
                .all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
            && !out.contains(&name.to_string())
        {
            out.push(name.to_string());
        }
    }
    out
}

/// 按 source 算出候选（阻塞）。CLI 直接用；界面请用 Resolver。
pub fn resolve(spec: &str, work_dir: &Path) -> Result<Vec<String>, String> {
    resolve_with_limit(spec, work_dir, MAX_CANDIDATES)
}

pub fn resolve_with_limit(
    spec: &str,
    work_dir: &Path,
    limit: usize,
) -> Result<Vec<String>, String> {
    let trimmed = spec.trim();
    if trimmed.is_empty() {
        return Err(String::from("dynamic 参数没有写 source"));
    }
    let argv = command_for(trimmed);
    let Some((program, rest)) = argv.split_first() else {
        return Err(format!("source「{trimmed}」解析不出要跑的命令"));
    };

    let output = Command::new(program)
        .args(rest)
        .current_dir(work_dir)
        .output()
        .map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                format!("候选来源要跑 {program}，但系统里没有它")
            } else {
                format!("{program} 跑不起来：{error}")
            }
        })?;

    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let candidates = match trimmed {
        "ffmpeg-video-codecs" => parse_ffmpeg_encoders(&stdout, 'V'),
        "ffmpeg-audio-codecs" => parse_ffmpeg_encoders(&stdout, 'A'),
        "ffmpeg-formats" => parse_ffmpeg_formats(&stdout),
        _ => candidates_from_output(&stdout, limit),
    };

    if !output.status.success() && candidates.is_empty() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let first = stderr.lines().next().unwrap_or("").trim().to_string();
        return Err(if first.is_empty() {
            format!(
                "{program} 退出码 {:?}，也没有给出候选",
                output.status.code()
            )
        } else {
            format!("{program} 失败了：{first}")
        });
    }
    Ok(candidates)
}

// ── 界面用的后台解析 ────────────────────────────────────────────────────────

/// 一次解析请求：把哪个字段的哪个 source 算出来。
#[derive(Clone, Debug)]
pub struct Job {
    /// 表单字段的 key（结果按它归属）。
    pub key: String,
    pub spec: String,
    pub work_dir: PathBuf,
}

/// 后台解析器。
///
/// 为什么不是同步跑：pacman -Qq 在慢机器上要几百毫秒，docker ps 要连 daemon。
/// 跑在 UI 线程上就是「打开表单卡一下」，和这个项目在包管理中心吃过的那次亏一样。
pub struct Resolver {
    jobs: Sender<Job>,
    results: Receiver<(String, Result<Vec<String>, String>)>,
    handle: Option<JoinHandle<()>>,
}

impl Resolver {
    pub fn start() -> Result<Self, String> {
        let (job_tx, job_rx) = mpsc::channel::<Job>();
        let (result_tx, result_rx) = mpsc::channel::<(String, Result<Vec<String>, String>)>();

        let handle = thread::Builder::new()
            .name(String::from("toolbox-hub-dynamic"))
            .spawn(move || {
                while let Ok(job) = job_rx.recv() {
                    let outcome = resolve(&job.spec, &job.work_dir);
                    if result_tx.send((job.key, outcome)).is_err() {
                        break;
                    }
                }
            })
            .map_err(|error| format!("起不了候选解析线程：{error}"))?;

        Ok(Self {
            jobs: job_tx,
            results: result_rx,
            handle: Some(handle),
        })
    }

    pub fn request(&self, job: Job) -> Result<(), String> {
        self.jobs
            .send(job)
            .map_err(|_| String::from("候选解析线程已经不在了"))
    }

    pub fn try_recv(&self) -> Option<(String, Result<Vec<String>, String>)> {
        match self.results.try_recv() {
            Ok(item) => Some(item),
            Err(TryRecvError::Empty) | Err(TryRecvError::Disconnected) => None,
        }
    }
}

impl Drop for Resolver {
    fn drop(&mut self) {
        let (dead, _) = mpsc::channel::<Job>();
        let _ = std::mem::replace(&mut self.jobs, dead);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_sources_are_well_formed_and_unique() {
        for info in SOURCES {
            assert!(!info.id.is_empty());
            assert!(!info.label.is_empty());
            assert!(!info.command.is_empty());
            assert!(
                !split_command(info.command).is_empty(),
                "{} 的命令切不出 argv",
                info.id
            );
            assert_eq!(source(info.id).map(|found| found.id), Some(info.id));
        }
        let ids: Vec<&str> = SOURCES.iter().map(|info| info.id).collect();
        let mut sorted = ids.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(ids.len(), sorted.len(), "内置源 id 不能重复");
    }

    /// 核心安全性质：命令串**不经过 shell**。
    #[test]
    fn splitting_never_introduces_shell_semantics() {
        assert_eq!(split_command("ls"), vec!["ls"]);
        assert_eq!(split_command("  ls   -la  "), vec!["ls", "-la"]);
        assert_eq!(
            split_command("docker ps --format {{.Names}}"),
            vec!["docker", "ps", "--format", "{{.Names}}"]
        );
        // 引号只是分组，不展开
        assert_eq!(split_command("echo 'a b'"), vec!["echo", "a b"]);
        assert_eq!(split_command("echo \"a b\""), vec!["echo", "a b"]);
        // 管道 / 重定向 / 变量展开都不会发生：它们就是普通参数
        assert_eq!(split_command("ls | wc -l"), vec!["ls", "|", "wc", "-l"]);
        assert_eq!(split_command("ls > /tmp/x"), vec!["ls", ">", "/tmp/x"]);
        assert_eq!(split_command("echo $HOME"), vec!["echo", "$HOME"]);
        // 反引号命令替换也不认
        let backtick = (96u8 as char).to_string();
        let raw = format!("echo {backtick}id{backtick}");
        assert_eq!(
            split_command(&raw),
            vec!["echo", raw.split(' ').nth(1).unwrap()]
        );
        assert_eq!(
            split_command("echo ;rm -rf /"),
            vec!["echo", ";rm", "-rf", "/"]
        );
        // 空与落单引号不该崩
        assert!(split_command("   ").is_empty());
        assert_eq!(split_command("echo 'unclosed"), vec!["echo", "unclosed"]);
    }

    #[test]
    fn a_builtin_name_maps_to_its_command_and_anything_else_is_a_command() {
        assert_eq!(
            command_for("git-branches"),
            vec!["git", "branch", "--format=%(refname:short)"]
        );
        assert_eq!(command_for("pacman-packages"), vec!["pacman", "-Qq"]);
        assert_eq!(
            command_for("command:docker ps --format '{{.Names}}'"),
            vec!["docker", "ps", "--format", "{{.Names}}"]
        );
        // 没有 command: 前缀、又不是内置名字，也当命令处理
        assert_eq!(command_for("git tag --list"), vec!["git", "tag", "--list"]);
        assert!(command_for("   ").is_empty());
    }

    #[test]
    fn output_becomes_deduped_trimmed_candidates() {
        assert_eq!(
            candidates_from_output("alpha\n  beta  \n\nalpha\ngamma\n", 10),
            vec!["alpha", "beta", "gamma"]
        );
        // 上限真的会截断
        let many: String = (0..100).map(|index| format!("line{index}\n")).collect();
        assert_eq!(candidates_from_output(&many, 5).len(), 5);
        assert!(candidates_from_output("", 10).is_empty());
    }

    #[test]
    fn ffmpeg_encoders_are_split_by_media_type() {
        let text = r#"Encoders:
 V..... = Video
 ------
 V....D libx264              H.264 / AVC
 V....D libx265              H.265 / HEVC
 A....D aac                  AAC
 A....D libmp3lame           MP3
 S..... srt                  SubRip
"#;
        let video = parse_ffmpeg_encoders(text, 'V');
        assert!(video.contains(&String::from("libx264")), "{video:?}");
        assert!(video.contains(&String::from("libx265")), "{video:?}");
        assert!(!video.contains(&String::from("aac")), "{video:?}");

        let audio = parse_ffmpeg_encoders(text, 'A');
        assert!(audio.contains(&String::from("aac")), "{audio:?}");
        assert!(audio.contains(&String::from("libmp3lame")), "{audio:?}");
        assert!(!audio.contains(&String::from("libx264")), "{audio:?}");
    }

    #[test]
    fn ffmpeg_formats_are_parsed() {
        let text = r#"File formats:
 D. = Demuxing supported
 .E = Muxing supported
 --
 DE matroska,webm     Matroska / WebM
  E mp4               MP4
 DE mp3               MP3
"#;
        let formats = parse_ffmpeg_formats(text);
        assert!(formats.contains(&String::from("mp4")), "{formats:?}");
        assert!(formats.contains(&String::from("mp3")), "{formats:?}");
    }

    /// 真的跑一条命令（用 sh，本机一定有）。
    #[test]
    fn resolve_actually_runs_a_command() {
        let dir = std::env::temp_dir();
        let candidates = resolve("command:sh -c 'echo one; echo two'", &dir).expect("应能跑");
        // 引号里的分号由 sh 自己解释 —— 这正是「要复杂逻辑就写清楚命令」的意思。
        assert_eq!(candidates, vec!["one", "two"]);
    }

    #[test]
    fn a_missing_program_is_a_message_not_a_panic() {
        let dir = std::env::temp_dir();
        let problem = resolve("command:definitely-not-a-real-command-xyz", &dir).unwrap_err();
        assert!(problem.contains("系统里没有"), "{problem}");
        assert!(resolve("   ", &dir).is_err());
    }

    #[test]
    fn a_failing_command_with_no_output_is_reported() {
        let dir = std::env::temp_dir();
        let problem = resolve("command:sh -c 'exit 3'", &dir).unwrap_err();
        assert!(
            problem.contains("退出码") || problem.contains("失败"),
            "{problem}"
        );
    }

    /// 空输出不是错误：候选就是空的。
    #[test]
    fn a_successful_command_with_no_output_yields_no_candidates() {
        let dir = std::env::temp_dir();
        assert!(
            resolve("command:true", &dir)
                .expect("true 是成功的")
                .is_empty()
        );
    }

    #[test]
    fn the_resolver_answers_on_a_background_thread() {
        let resolver = Resolver::start().expect("start");
        resolver
            .request(Job {
                key: String::from("branch"),
                spec: String::from("command:sh -c 'echo alpha; echo beta'"),
                work_dir: std::env::temp_dir(),
            })
            .expect("request");

        let mut seen = None;
        for _ in 0..300 {
            if let Some(item) = resolver.try_recv() {
                seen = Some(item);
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let (key, outcome) = seen.expect("要有结果");
        assert_eq!(key, "branch");
        assert_eq!(outcome.expect("应成功"), vec!["alpha", "beta"]);
    }

    #[test]
    fn the_resolver_reports_failures_per_key() {
        let resolver = Resolver::start().expect("start");
        resolver
            .request(Job {
                key: String::from("x"),
                spec: String::from("command:definitely-not-a-real-command-xyz"),
                work_dir: std::env::temp_dir(),
            })
            .expect("request");

        let mut seen = None;
        for _ in 0..300 {
            if let Some(item) = resolver.try_recv() {
                seen = Some(item);
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let (key, outcome) = seen.expect("要有结果");
        assert_eq!(key, "x");
        assert!(outcome.is_err());
    }
}
