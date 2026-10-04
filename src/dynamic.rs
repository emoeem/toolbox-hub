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
    io::{BufRead, BufReader, Read},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, Sender, TryRecvError},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

/// 候选数量上限（防止一条命令吐出几万行把界面拖死）。
pub const MAX_CANDIDATES: usize = 500;

/// 一条候选命令最多跑多久。
///
/// 动态候选是「顺手问机器一句」的东西，不该让用户等：`docker ps` 连不上 daemon
/// 这类场景会一直挂着，而它跑在后台线程里 —— 界面只会永远显示「进行中」。
/// 超过这个时间就整组杀掉，按「取不到候选」报一句能显示的话。
///
/// 5 秒的取舍：`pacman -Qq` 是几十毫秒、`docker ps` 是几百毫秒级，5 秒只是
/// 「真的卡住了」的兜底，不会误伤正常命令。
pub const COMMAND_TIMEOUT: Duration = Duration::from_secs(5);

/// 轮询子进程状态的间隔。正常路径一次 `try_wait` 就出，10ms 只是「不烧 CPU」
/// 与「超时立刻生效」之间的取舍。
const WAIT_POLL_INTERVAL: Duration = Duration::from_millis(10);

/// 子进程结束之后，最多再等这么久把管道里剩下的输出读完。
///
/// 之所以要有上限：命令自己拉起来的后台进程会**继承这两根管道**，它不退，
/// `read` 就永远等不到 EOF。宁可少几行候选，也不能让一个候选源把解析线程
/// 永远钉在那里（退出时也一样）。
const READ_GRACE: Duration = Duration::from_secs(2);

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

/// 按 source 算出候选（阻塞）—— **唯一入口**：超时、取消、进程组回收都在里面。
///
/// * 界面走 [`Resolver`]：它在后台线程里用 [`COMMAND_TIMEOUT`] 调这个函数；
/// * 想同步调（CLI 之类）：`timeout` 传 [`COMMAND_TIMEOUT`]，`cancel` 传一个
///   永不置位的 [`AtomicBool`]。
///
/// 以前这里还有 `resolve` / `resolve_with_limit` 两个「默认参数」包装，但全仓
/// 没有调用点（CLI 那条路不走动态候选）—— 没人用的包装只会烂在那里，删掉。
/// 参数化同时是测试能用很短超时的原因。
///
/// * `timeout` —— 一条命令最多跑多久，超了整组杀掉并按「取不到候选」报错；
/// * `cancel` —— 主人已经走了（界面在退出）：杀掉手里这条命令、别再算了。
///
/// 这两条兜底不是「优化」，是**界面不许被一条外部命令拖住**：命令是用户写的
///（可能是个死循环、可能连不上 daemon），而它就跑在这条解析线程上。
fn resolve_with(
    spec: &str,
    work_dir: &Path,
    limit: usize,
    timeout: Duration,
    cancel: &AtomicBool,
) -> Result<Vec<String>, String> {
    let trimmed = spec.trim();
    if trimmed.is_empty() {
        return Err(String::from("dynamic 参数没有写 source"));
    }
    let argv = command_for(trimmed);
    let Some((program, rest)) = argv.split_first() else {
        return Err(format!("source「{trimmed}」解析不出要跑的命令"));
    };

    let mut command = Command::new(program);
    command
        .args(rest)
        .current_dir(work_dir)
        // 不给它 stdin：候选命令跑在后台，用户看不见它 —— 让它去读 stdin 只会
        // 得到一个永远等不到输入、也永远不退出的进程。
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // 自成进程组：超时/取消时一次把整组带走，命令拉起来的孙子进程也不留下。
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }

    let mut child = command.spawn().map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            format!("候选来源要跑 {program}，但系统里没有它")
        } else {
            format!("{program} 跑不起来：{error}")
        }
    })?;

    let stdout = read_stream(child.stdout.take());
    let stderr = read_stream(child.stderr.take());

    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {}
            Err(error) => {
                stop(&mut child);
                return Err(format!("{program} 的状态取不到了：{error}"));
            }
        }
        if cancel.load(Ordering::SeqCst) {
            // 界面已经走了：结果没人要，把命令停掉就走。
            stop(&mut child);
            return Err(String::from("候选解析器已经关掉了，这条命令不再算"));
        }
        if Instant::now() >= deadline {
            stop(&mut child);
            return Err(format!(
                "{program} 超过 {:.1} 秒还没跑完，已经把它停掉了（候选按取不到处理）",
                timeout.as_secs_f64()
            ));
        }
        thread::sleep(WAIT_POLL_INTERVAL);
    };

    // 进程已经结束，管道里剩下的输出就在这儿（有上限，见 READ_GRACE）。
    let stdout = drain(stdout);
    let stderr = drain(stderr);

    let candidates = match trimmed {
        "ffmpeg-video-codecs" => parse_ffmpeg_encoders(&stdout, 'V'),
        "ffmpeg-audio-codecs" => parse_ffmpeg_encoders(&stdout, 'A'),
        "ffmpeg-formats" => parse_ffmpeg_formats(&stdout),
        _ => candidates_from_output(&stdout, limit),
    };

    if !status.success() && candidates.is_empty() {
        let first = stderr.lines().next().unwrap_or("").trim().to_string();
        return Err(if first.is_empty() {
            format!("{program} 退出码 {:?}，也没有给出候选", status.code())
        } else {
            format!("{program} 失败了：{first}")
        });
    }
    Ok(candidates)
}

/// 杀掉这条命令（整个进程组），并把它收掉。
fn stop(child: &mut Child) {
    // 整组 SIGKILL：命令自己拉起来的子进程也不该留下（和 runtime::exec 一个做法）。
    let _ = signal_group(child.id(), libc::SIGKILL);
    let _ = child.kill();
    // kill 之后还要 wait 一次，否则它会挂在进程表里当僵尸。
    let _ = child.wait();
}

/// 给「进程组」发信号（`kill(-pgid)`）。失败返回 false，调用方自己兜底。
fn signal_group(pid: u32, signal: libc::c_int) -> bool {
    // SAFETY: `kill` 是纯系统调用，参数就是 pid 与信号号；负号表示「进程组」。
    unsafe { libc::kill(-(pid as libc::pid_t), signal) == 0 }
}

/// 起一条读线程，把整条流读成文本，读完通过通道交回来。
///
/// 为什么不直接 `join()` 读线程：读也可能被卡住（见 [READ_GRACE]），而 `join`
/// 没有超时。用通道 + `recv_timeout` 才能「读不到就算了」。
fn read_stream(stream: Option<impl Read + Send + 'static>) -> Receiver<String> {
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let mut collected = String::new();
        if let Some(stream) = stream {
            let mut reader = BufReader::new(stream);
            let mut buffer = Vec::new();
            loop {
                buffer.clear();
                match reader.read_until(b'\n', &mut buffer) {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {}
                }
                collected.push_str(&String::from_utf8_lossy(&buffer));
            }
        }
        let _ = tx.send(collected);
    });
    rx
}

/// 取回读线程的成果；等不到（还在读）就当空 —— 见 [READ_GRACE]。
fn drain(collected: Receiver<String>) -> String {
    collected.recv_timeout(READ_GRACE).unwrap_or_default()
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

/// 后台解析线程的回应。
///
/// 以前这里是裸元组 `(key, Result<...>)`。改成枚举是为了让「线程没了」这件事
/// 有地方放 —— 它和「某个字段的结果」不是一回事。
pub enum Response {
    /// 一个字段的候选算好了（失败是一句能直接显示的话）。
    Candidates {
        key: String,
        outcome: Result<Vec<String>, String>,
    },
    /// 后台线程没了（panic，或者提前退出）。**只报一次。**
    ///
    /// 语义和 [`crate::repository::worker::Response::ThreadGone`] 完全一样，只是
    /// 名字跟着这里叫：线程一死，回应通道就断开。界面若把「断开」当成「暂时没
    /// 消息」，那个动态字段会永远停在「进行中」—— 这正是要修的那个观感。
    WorkerGone,
}

/// 后台解析器。
///
/// 为什么不是同步跑：pacman -Qq 在慢机器上要几百毫秒，docker ps 要连 daemon。
/// 跑在 UI 线程上就是「打开表单卡一下」，和这个项目在包管理中心吃过的那次亏一样。
pub struct Resolver {
    jobs: Sender<Job>,
    results: Receiver<Response>,
    handle: Option<JoinHandle<()>>,
    /// 主人走了没有（Drop 时置位）。解析线程据此把手里那条命令停掉。
    cancel: Arc<AtomicBool>,
    /// 线程死亡报过没有（WorkerGone 只发一次）。
    reported_gone: std::cell::Cell<bool>,
}

impl Resolver {
    pub fn start() -> Result<Self, String> {
        let (job_tx, job_rx) = mpsc::channel::<Job>();
        let (result_tx, result_rx) = mpsc::channel::<Response>();
        let cancel = Arc::new(AtomicBool::new(false));
        let thread_cancel = Arc::clone(&cancel);

        let handle = thread::Builder::new()
            .name(String::from("toolbox-hub-dynamic"))
            .spawn(move || {
                while let Ok(job) = job_rx.recv() {
                    let outcome = resolve_with(
                        &job.spec,
                        &job.work_dir,
                        MAX_CANDIDATES,
                        COMMAND_TIMEOUT,
                        &thread_cancel,
                    );
                    // 主人已经走了：手里那条命令刚被停掉，结果没人要，直接收摊。
                    if thread_cancel.load(Ordering::SeqCst) {
                        break;
                    }
                    let sent = result_tx.send(Response::Candidates {
                        key: job.key,
                        outcome,
                    });
                    if sent.is_err() {
                        break;
                    }
                }
            })
            .map_err(|error| format!("起不了候选解析线程：{error}"))?;

        Ok(Self {
            jobs: job_tx,
            results: result_rx,
            handle: Some(handle),
            cancel,
            reported_gone: std::cell::Cell::new(false),
        })
    }

    pub fn request(&self, job: Job) -> Result<(), String> {
        self.jobs
            .send(job)
            .map_err(|_| String::from("候选解析线程已经不在了"))
    }

    /// 取一个回应（没有就是 None）。界面每帧调一次。
    pub fn try_recv(&self) -> Option<Response> {
        match self.results.try_recv() {
            Ok(response) => Some(response),
            Err(TryRecvError::Empty) => None,
            // 线程死了不能当成「暂时没消息」：那个字段会永远停在「进行中」。
            // 只报一次，别每帧刷屏。
            Err(TryRecvError::Disconnected) if !self.reported_gone.replace(true) => {
                Some(Response::WorkerGone)
            }
            Err(TryRecvError::Disconnected) => None,
        }
    }
}

impl Drop for Resolver {
    /// 退出路径：**不 join，只发取消信号**。
    ///
    /// 以前这里是 `handle.join()`：如果那个线程正卡在一条外部命令上（候选源
    /// 什么命令都可能跑），退出就要陪它等完 —— 用户按 q 半天退不出去，根源就在
    /// 这一行。现在的取舍：
    ///
    /// * 先丢掉请求端：线程手里的活干完、`recv` 拿到 `Err` 就自己退；
    /// * 同时置 `cancel`：线程会把它**手里那条命令整组杀掉**，不会变成孤儿继续
    ///   跑（比单纯 detach 干净）；
    /// * `JoinHandle` 就地丢弃（分离）：线程自己收尾，不阻塞退出；进程真退了，
    ///   所有线程本来就会终止。
    ///
    /// 代价：线程真正结束的时刻可能比 `drop` 晚一个轮询间隔（毫秒级）。这点延迟
    /// 换来的是「退出永远不等待」，值得。
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::SeqCst);
        let (dead, _) = mpsc::channel::<Job>();
        let _ = std::mem::replace(&mut self.jobs, dead);
        self.handle = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 测试里的同步入口：默认超时 + 永不取消。
    ///
    /// 生产代码不再有这层包装（见 [`resolve_with`] 的文档），但测试要的是
    ///「默认参数下那条路」，所以在这儿还原它。
    fn resolve(spec: &str, work_dir: &Path) -> Result<Vec<String>, String> {
        let never = AtomicBool::new(false);
        resolve_with(spec, work_dir, MAX_CANDIDATES, COMMAND_TIMEOUT, &never)
    }

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
            if let Some(Response::Candidates { key, outcome }) = resolver.try_recv() {
                seen = Some((key, outcome));
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
            if let Some(Response::Candidates { key, outcome }) = resolver.try_recv() {
                seen = Some((key, outcome));
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let (key, outcome) = seen.expect("要有结果");
        assert_eq!(key, "x");
        assert!(outcome.is_err());
    }

    /// 命令跑飞了必须被超时截断，**不能把解析线程挂住**。
    ///
    /// 假命令是 `sh -c 'sleep 30'`：没有超时的话这条测试要等 30 秒。
    #[test]
    fn a_slow_candidate_command_times_out_instead_of_hanging() {
        let never = std::sync::atomic::AtomicBool::new(false);
        let started = std::time::Instant::now();
        let problem = resolve_with(
            "command:sh -c 'sleep 30'",
            &std::env::temp_dir(),
            10,
            std::time::Duration::from_millis(300),
            &never,
        )
        .expect_err("睡着不动的命令必须报超时");
        let elapsed = started.elapsed();
        eprintln!("超时路径实测耗时：{elapsed:?}（命令本身要睡 30s）");
        assert!(problem.contains("超过"), "{problem}");
        assert!(
            elapsed < std::time::Duration::from_secs(3),
            "超时没生效：{elapsed:?}"
        );
    }

    /// 默认那条 5 秒兜底也真的会咬人（连 `sleep 30` 都拦得住）。
    ///
    /// 要睡满 5 秒，所以默认 ignore；要验就
    /// `cargo test the_default_timeout_still_bites -- --ignored --nocapture`。
    #[test]
    #[ignore = "要睡满 5 秒，默认不跑"]
    fn the_default_timeout_still_bites() {
        let never = std::sync::atomic::AtomicBool::new(false);
        let started = std::time::Instant::now();
        let problem = resolve_with(
            "command:sh -c 'sleep 30'",
            &std::env::temp_dir(),
            MAX_CANDIDATES,
            COMMAND_TIMEOUT,
            &never,
        )
        .expect_err("默认超时也必须拦住它");
        let elapsed = started.elapsed();
        eprintln!("默认 5s 超时实测耗时：{elapsed:?}（命令要睡 30s）");
        assert!(problem.contains("超过"), "{problem}");
        assert!(
            elapsed >= std::time::Duration::from_secs(5),
            "还没到超时就返回了：{elapsed:?}"
        );
        assert!(elapsed < std::time::Duration::from_secs(8), "{elapsed:?}");
    }

    /// Drop 不等线程：那条候选命令还在睡，`drop` 必须立刻回来。
    ///
    /// 顺带钉住取消信号：命令不能变成孤儿继续睡（pid 要真的消失）。
    #[test]
    fn dropping_the_resolver_does_not_wait_for_a_slow_command() {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let pidfile = std::env::temp_dir().join(format!("toolbox-hub-dynamic-drop-{nanos}.pid"));
        let _ = std::fs::remove_file(&pidfile);

        let resolver = Resolver::start().expect("start");
        let spec = format!("command:sh -c 'echo $$ > {}; sleep 30'", pidfile.display());
        resolver
            .request(Job {
                key: String::from("slow"),
                spec,
                work_dir: std::env::temp_dir(),
            })
            .expect("request");

        // 等命令真的跑起来（pid 落盘了）。
        let mut pid = None;
        for _ in 0..300 {
            if let Ok(text) = std::fs::read_to_string(&pidfile)
                && let Ok(value) = text.trim().parse::<i32>()
            {
                pid = Some(value);
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let pid = pid.expect("候选命令的 pid 应该落盘了");

        let started = std::time::Instant::now();
        drop(resolver);
        let waited = started.elapsed();
        eprintln!("drop(resolver) 实测耗时：{waited:?}（旧实现会 join 这条 sleep 30）");
        assert!(
            waited < std::time::Duration::from_millis(500),
            "drop 在等那条命令/那个线程：{waited:?}"
        );

        // 取消信号要真的把命令带走，而不是留个孤儿在后台接着睡。
        let mut gone = false;
        for _ in 0..250 {
            if !alive(pid) {
                gone = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert!(
            gone,
            "drop 之后那条候选命令还活着（pid {pid}）—— 取消信号没生效"
        );
        let _ = std::fs::remove_file(&pidfile);
    }

    /// 后台线程没了要看得见，而且**只报一次**。
    ///
    /// 这里制造的是「线程异常结束」：线程把两个通道端都丢掉就走了。panic 走的
    /// 是同一条路 —— unwind 时局部变量一样被丢掉，回应通道一样断开。
    #[test]
    fn a_dead_worker_is_reported_exactly_once() {
        let (job_tx, job_rx) = std::sync::mpsc::channel::<Job>();
        let (result_tx, result_rx) = std::sync::mpsc::channel::<Response>();
        std::thread::spawn(move || {
            // 手里那两个端一起丢掉，然后线程结束（模拟异常退出）。
            drop(job_rx);
            drop(result_tx);
        })
        .join()
        .expect("起线程");

        let resolver = Resolver {
            jobs: job_tx,
            results: result_rx,
            handle: None,
            cancel: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            reported_gone: std::cell::Cell::new(false),
        };

        assert!(matches!(resolver.try_recv(), Some(Response::WorkerGone)));
        assert!(resolver.try_recv().is_none(), "只报一次，不能每帧刷屏");
        assert!(resolver.try_recv().is_none());

        // 线程已经不在，再派活要给一句能显示的话，而不是静默。
        let failed = resolver.request(Job {
            key: String::from("after-death"),
            spec: String::from("command:true"),
            work_dir: std::env::temp_dir(),
        });
        assert!(failed.is_err(), "死掉的线程不能装作还能接活");
    }

    /// panic 走的也是同一条路：unwind 会把回应端丢掉，通道一样断开。
    ///
    /// 默认 ignore —— 它会**故意 panic 一次**（stderr 会多一行 panic 信息），
    /// 日常跑测试没必要看这个；要验就
    /// `cargo test a_panicking_worker_is_reported_once -- --ignored --nocapture`。
    #[test]
    #[ignore = "会故意 panic 一次，默认不跑"]
    fn a_panicking_worker_is_reported_once() {
        let (job_tx, job_rx) = std::sync::mpsc::channel::<Job>();
        let (result_tx, result_rx) = std::sync::mpsc::channel::<Response>();
        let handle = std::thread::spawn(move || {
            // 两个端都跟着线程：unwind 时一起被丢掉，回应通道随即断开。
            let _keep = (job_rx, result_tx);
            panic!("模拟候选解析线程 panic");
        });
        assert!(handle.join().is_err(), "那条线程确实是 panic 掉的");

        let resolver = Resolver {
            jobs: job_tx,
            results: result_rx,
            handle: None,
            cancel: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            reported_gone: std::cell::Cell::new(false),
        };
        assert!(matches!(resolver.try_recv(), Some(Response::WorkerGone)));
        assert!(resolver.try_recv().is_none(), "只报一次");
    }

    /// 这个 pid 还活着吗（和 runtime::exec 那份同一个判法：僵尸不算活）。
    fn alive(pid: i32) -> bool {
        if let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat"))
            && let Some(rest) = stat.rsplit_once(')').map(|(_, rest)| rest)
            && rest.split_whitespace().next() == Some("Z")
        {
            return false;
        }
        // SAFETY: 信号 0 不发送任何东西，只检查进程是否存在。
        unsafe { libc::kill(pid, 0) == 0 }
    }
}
