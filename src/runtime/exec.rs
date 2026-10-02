use std::{
    fs,
    io::{self, BufRead, BufReader},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver},
    },
    thread,
    time::{Duration, Instant},
};

use ratatui::crossterm::{
    event::{DisableMouseCapture, EnableMouseCapture},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};

use crate::model::ToolDefinition;

/// 一次批量执行的结果。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ExecReport {
    /// 尝试启动的工具数。
    pub launched: usize,
    /// 启动失败或非零退出的工具数。
    pub failed: usize,
}

impl ExecReport {
    /// 状态栏文案。
    pub fn message(&self) -> String {
        if self.failed == 0 {
            format!("执行完成 · {} 个工具", self.launched)
        } else {
            format!(
                "执行完成 · {} 个成功 / {} 个失败",
                self.launched.saturating_sub(self.failed),
                self.failed
            )
        }
    }
}

/// 一条待执行的命令：程序 + 参数，**永远是 argv，不拼 shell 字符串**。
struct Job {
    /// 出错时报给用户的名字。
    label: String,
    program: PathBuf,
    argv: Vec<String>,
}

/// 顺序执行工具，每个都在 `cwd` 里运行。
///
/// `cwd` 必须是 **Toolbox 启动时的原始工作目录**：很多 FFTools 脚本默认
/// 在当前目录里挑文件，所以这里既不能用工具目录，也不能用进程后来切换到的目录。
///
/// 执行期间会挂起 TUI（离开备用屏幕 + 关闭 raw mode），把终端完整交给工具，
/// 全部跑完后等一次回车再恢复界面。
pub fn execute_tools(tools: &[ToolDefinition], cwd: &Path) -> io::Result<ExecReport> {
    let jobs: Vec<Job> = tools
        .iter()
        .map(|tool| Job {
            label: tool.name.clone(),
            program: tool.path.clone(),
            argv: Vec::new(),
        })
        .collect();
    run_batch(&jobs, cwd)
}

/// 执行一件带参数的动作：程序取自工具，参数由表单构建。
///
/// `argv` 是 [`crate::model::Action::build_argv`] 的产物，每个元素原样成为一个
/// 进程参数 —— 用户填的 `;`、`|`、`$(…)`、反引号都只是普通字符，不存在注入面。
pub fn execute_action(
    tool: &ToolDefinition,
    argv: &[String],
    cwd: &Path,
) -> io::Result<ExecReport> {
    let job = Job {
        label: tool.name.clone(),
        program: tool.path.clone(),
        argv: argv.to_vec(),
    };
    run_batch(&[job], cwd)
}

/// 在 `$PATH` 里找一个可执行文件。
///
/// 为什么这里自己扫一遍：`runtime` 的约定是只依赖 [`crate::model`]，
/// 不去碰 `providers` 里的工具发现逻辑（那边有一份带缓存的同类实现）。
/// 这点有意为之的重复，比让执行层反过来依赖 Provider 层划算。
fn find_on_path(program: &str) -> Option<PathBuf> {
    let paths = std::env::var_os("PATH")?;
    std::env::split_paths(&paths)
        .map(|dir| dir.join(program))
        .find(|candidate| is_executable(candidate))
}

fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    path.metadata()
        .map(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

/// 把终端交给某个程序跑一遍（安装、交互式确认这类）。
///
/// 和 [`browse_directories`] 的区别：这个不读回任何状态，只等它退出。
/// 找不到程序返回 `NotFound`，调用方据此给一句「没装」。
pub fn run_in_terminal(program: &str, argv: &[String], cwd: &Path) -> io::Result<Option<i32>> {
    let program_path = if program.contains('/') {
        let path = PathBuf::from(program);
        if !is_executable(&path) {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("找不到 {program}"),
            ));
        }
        path
    } else {
        find_on_path(program).ok_or_else(|| {
            io::Error::new(io::ErrorKind::NotFound, format!("$PATH 里没有 {program}"))
        })?
    };

    suspend_terminal()?;
    let status = Command::new(&program_path)
        .args(argv)
        .current_dir(cwd)
        .status();
    resume_terminal()?;
    Ok(status?.code())
}

/// 文件管理器逛完之后反馈回来的东西。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BrowsedBack {
    /// 程序退出码。
    pub status: Option<i32>,
    /// 退出时所在的目录（`--cwd-file`）。
    pub cwd: Option<PathBuf>,
    /// 在里面「打开」过的文件（`--chooser-file`）。
    pub chosen: Vec<PathBuf>,
}

/// 解析 `--cwd-file` 的内容：第一行非空文本。
pub fn parse_cwd_file(text: &str) -> Option<PathBuf> {
    let line = text.lines().map(str::trim).find(|line| !line.is_empty())?;
    Some(PathBuf::from(line))
}

/// 解析 `--chooser-file` 的内容：每行一个路径。
pub fn parse_chooser_file(text: &str) -> Vec<PathBuf> {
    text.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(PathBuf::from)
        .collect()
}

/// 把终端交给一个文件管理器，让它**在 `cwd` 里**跑，然后把它报告的目录/文件读回来。
///
/// 约定：
/// * 接受一个**起始路径位置参数**（yazi 的 `[ENTRIES]...` 就是它）；
/// * `--cwd-file <路径>`：退出时把当前目录写进去；
/// * `--chooser-file <路径>`：在里面「打开」文件时把路径写进去。
///
/// yazi 三条都认；换别的文件管理器就设 `TOOLBOX_HUB_FILE_MANAGER`。
///
/// 找不到这个程序会返回 `ErrorKind::NotFound`，调用方据此提示「没装」。
pub fn browse_directories(program: &str, cwd: &Path) -> io::Result<BrowsedBack> {
    let stamp = std::process::id();
    let dir = std::env::temp_dir();
    let cwd_file = dir.join(format!("toolbox-hub-{stamp}-browse.cwd"));
    let chooser_file = dir.join(format!("toolbox-hub-{stamp}-browse.chosen"));
    // 上一轮的残留会让「它到底写没写」变得不可信，所以先清掉。
    let _ = fs::remove_file(&cwd_file);
    let _ = fs::remove_file(&chooser_file);

    // **先确认这个程序真的存在**，再去动终端：不然「没装」会被终端的错误盖掉，
    // 调用方就没法据此给一句「没装 yazi」了（这条顺序是测试抓出来的）。
    let program_path = if program.contains('/') {
        let path = PathBuf::from(program);
        if !is_executable(&path) {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("找不到 {program}"),
            ));
        }
        path
    } else {
        find_on_path(program).ok_or_else(|| {
            io::Error::new(io::ErrorKind::NotFound, format!("$PATH 里没有 {program}"))
        })?
    };

    let status = {
        suspend_terminal()?;
        let result = Command::new(&program_path)
            // **显式把起始目录传给它**（yazi 的位置参数就是「当前工作条目」）。
            // 只设 current_dir 是不够的 —— 实测 yazi 仍然开在进程启动目录下。
            .arg(cwd)
            .arg("--cwd-file")
            .arg(&cwd_file)
            .arg("--chooser-file")
            .arg(&chooser_file)
            .current_dir(cwd)
            .status();
        // 不管跑成没跑成，终端都要还回来。
        resume_terminal()?;
        result
    };

    let status = match status {
        Ok(status) => status,
        Err(error) => {
            let _ = fs::remove_file(&cwd_file);
            let _ = fs::remove_file(&chooser_file);
            return Err(error);
        }
    };

    let browsed = BrowsedBack {
        status: status.code(),
        cwd: fs::read_to_string(&cwd_file)
            .ok()
            .and_then(|text| parse_cwd_file(&text))
            .filter(|path| path.is_dir()),
        chosen: fs::read_to_string(&chooser_file)
            .ok()
            .map(|text| {
                parse_chooser_file(&text)
                    .into_iter()
                    .filter(|path| path.exists())
                    .collect()
            })
            .unwrap_or_default(),
    };

    let _ = fs::remove_file(&cwd_file);
    let _ = fs::remove_file(&chooser_file);
    Ok(browsed)
}

/// 一次「捕获输出」执行的结果。
///
/// 和 [`execute_tools`] 的区别：**不挂起 TUI**，把输出收下来交给内置视图，
/// 所以跑完还能回看、还能接着滚。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Captured {
    /// 出问题时报给用户的名字。
    pub label: String,
    /// 真正跑的命令（给人看的一行）。
    pub command: String,
    pub stdout: String,
    pub stderr: String,
    pub status: Option<i32>,
    pub success: bool,
    pub elapsed: Duration,
    /// 是不是被用户取消的（取消也算「没成功」，但原因不一样）。
    pub cancelled: bool,
}

impl Captured {
    /// 状态行：`退出码 0 · 0.3s`。
    pub fn summary(&self) -> String {
        let code = match self.status {
            Some(0) => String::from("退出码 0"),
            Some(code) => format!("退出码 {code}"),
            None => String::from("被信号终止"),
        };
        format!("{} · {:.2}s", code, self.elapsed.as_secs_f64())
    }

    /// 输出视图里的正文：先 stdout 再 stderr，各自带小标题（空的那段不占地方）。
    pub fn body(&self) -> String {
        let mut text = String::new();
        if !self.stdout.trim().is_empty() {
            text.push_str(self.stdout.trim_end());
            text.push('\n');
        }
        if !self.stderr.trim().is_empty() {
            if !text.is_empty() {
                text.push('\n');
            }
            text.push_str("── stderr ──\n");
            text.push_str(self.stderr.trim_end());
            text.push('\n');
        }
        if text.is_empty() {
            text.push_str("(没有任何输出)\n");
        }
        text
    }
}

/// 解析 ffmpeg 风格的时间：`00:01:02.5` / `62.5` / `01:02`。
///
/// 裁剪参数、`out_time` 进度都是这种写法，所以两边共用。
pub fn parse_duration(text: &str) -> Option<f64> {
    let text = text.trim();
    if text.is_empty() {
        return None;
    }

    let parts: Vec<&str> = text.split(':').collect();
    if parts.len() > 3 {
        return None;
    }

    let mut seconds = 0.0;
    for part in parts {
        seconds = seconds * 60.0 + part.trim().parse::<f64>().ok()?;
    }
    if seconds.is_finite() {
        Some(seconds)
    } else {
        None
    }
}

/// 探一个媒体文件的总时长（秒）。
///
/// 只有声明了 `duration_from` 的动作才会走到这里 —— 工具箱本身不假设任何工具，
/// 这是「媒体时长」这一个具体需求的实现。ffprobe 不在、或文件读不出来就返回
/// `None`，进度条退化成「已跑时间」而已，不影响执行。
pub fn probe_duration(path: &Path) -> Option<f64> {
    let output = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-show_entries",
            "format=duration",
            "-of",
            "csv=p=0",
        ])
        .arg(path)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    parse_duration(&String::from_utf8_lossy(&output.stdout))
}

/// 后台任务发回来的事件。
pub enum JobEvent {
    /// 一行输出。`stderr` 区分是哪条流。
    Line { stderr: bool, text: String },
    /// 结构化进度，例如 ffmpeg `-progress pipe:1` 吐的 `out_time=00:00:04`。
    Progress { key: String, value: String },
    /// 跑完了（正常结束、失败、被取消都会走到这里）。
    Done(Box<Captured>),
}

/// 一次后台捕获任务的把手。
///
/// 命令在**后台线程**里跑，主循环每帧非阻塞地取事件 —— 所以跑长命令时界面不卡，
/// 还能看实时输出与进度，也能取消。
pub struct RunningJob {
    pub command: String,
    /// 事件通道（`try_recv` 非阻塞取，`recv_timeout` 给测试用）。
    pub events: Receiver<JobEvent>,
    child: Arc<Mutex<Child>>,
    cancelled: Arc<AtomicBool>,
    started: Instant,
}

impl RunningJob {
    pub fn elapsed(&self) -> Duration {
        self.started.elapsed()
    }

    /// 子进程 pid（测试用，出问题时也好排查）。
    pub fn pid(&self) -> Option<u32> {
        self.child.lock().ok().map(|child| child.id())
    }

    /// 取消：**先请它自己退**（SIGTERM），给 ffmpeg 一个把输出文件收尾的机会。
    /// 之后还会收到一个 `Done`（`cancelled = true`）。
    ///
    /// 以前这里是 spawn 一个 `kill -TERM <pid>`：为了不引依赖绕的路，代价是
    /// 多起一个进程、还依赖那个程序存在。现在直接 `libc::kill` —— 一次系统调用。
    ///
    /// 发**整个进程组**（`-pid`）而不是单个进程：工具自己拉起来的子进程
    /// （ffmpeg 的管道、shell 脚本里的后台任务）也该一起收到，不然它们会变成孤儿
    /// 继续闷头写文件。任务是以自己的进程组启动的（见 `spawn`）。
    pub fn terminate(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
        let Some(pid) = self.pid() else {
            return;
        };
        if !signal_group(pid, libc::SIGTERM) {
            self.cancel();
        }
    }

    /// 直接杀掉（SIGKILL）。
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
        // 先整组 SIGKILL（子进程也别漏），再退回 Child::kill 兜底。
        if let Some(pid) = self.pid() {
            let _ = signal_group(pid, libc::SIGKILL);
        }
        // 注意：收尾线程只在两条流 EOF 之后才会持有这把锁去 wait，所以这里不会卡住。
        if let Ok(mut child) = self.child.lock() {
            let _ = child.kill();
        }
    }
}

/// 给「进程组」发信号（`kill(-pgid)`）。失败返回 false，调用方自己兜底。
///
/// 用组而不是单个 pid：`program` 底下可能还有孙子进程，只杀父进程会把它们留下。
fn signal_group(pid: u32, signal: libc::c_int) -> bool {
    // SAFETY: `kill` 是纯系统调用，参数就是 pid 与信号号；负号表示「进程组」。
    unsafe { libc::kill(-(pid as libc::pid_t), signal) == 0 }
}

/// 这一行是不是 ffmpeg `-progress` 吐的进度键。
///
/// 表要齐：漏掉的键会当成普通输出显示在实时尾巴里，把真正的输出挤掉
/// （实测漏了 `out_time_us` / `progress` / `stream_0_0_q` 就出现这个问题）。
fn is_progress_key(key: &str) -> bool {
    const KEYS: [&str; 12] = [
        "frame",
        "fps",
        "bitrate",
        "total_size",
        "out_time",
        "out_time_us",
        "out_time_ms",
        "dup_frames",
        "drop_frames",
        "speed",
        "progress",
        "stream_0_0_q",
    ];
    KEYS.contains(&key) || (key.starts_with("stream_") && key.ends_with("_q"))
}

/// 在**后台**跑一条命令并捕获输出。
///
/// 和 [`run_captured`] 的区别：它立刻返回，主循环随后通过事件通道拿实时输出、
/// 结构化进度和最终结果 —— 所以界面不会卡在一条长命令上。
pub fn spawn_captured(
    program: &Path,
    argv: &[String],
    cwd: &Path,
    label: &str,
) -> io::Result<RunningJob> {
    let mut command = Command::new(program);
    command
        .args(argv)
        .current_dir(cwd)
        // 不给它 stdin：后台任务不该等着人喂输入。
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // 让它自成进程组（pgid = 自己的 pid）：取消时才能一次把整组带走，
    // 不然工具拉起来的孙子进程会变成孤儿接着跑、接着写文件。
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    let mut child = command.spawn()?;

    let stdout = child.stdout.take();
    let stderr = child.stderr.take();

    let child = Arc::new(Mutex::new(child));
    let cancelled = Arc::new(AtomicBool::new(false));
    let (tx, events) = mpsc::channel();

    let stdout_reader = reader_thread(stdout, false, tx.clone());
    let stderr_reader = reader_thread(stderr, true, tx.clone());

    let command = format!("{} {}", program.display(), argv.join(" "))
        .trim_end()
        .to_string();
    let label = label.to_string();
    let started = Instant::now();

    // 收尾线程：等两条流读完、等子进程退出，然后发一条 Done。
    let finished_child = Arc::clone(&child);
    let finished_flag = Arc::clone(&cancelled);
    let finished_label = label.clone();
    thread::spawn(move || {
        let out = stdout_reader.join().ok().unwrap_or_default();
        let err = stderr_reader.join().ok().unwrap_or_default();
        // 两条流都 EOF 了，子进程基本已经结束，这里 wait 不会久等。
        let status = finished_child
            .lock()
            .ok()
            .and_then(|mut child| child.wait().ok());

        let _ = tx.send(JobEvent::Done(Box::new(Captured {
            label: finished_label,
            command,
            stdout: out,
            stderr: err,
            status: status.and_then(|status| status.code()),
            success: status.is_some_and(|status| status.success()),
            elapsed: started.elapsed(),
            cancelled: finished_flag.load(Ordering::SeqCst),
        })));
    });

    Ok(RunningJob {
        command: format!("{} {}", program.display(), argv.join(" "))
            .trim_end()
            .to_string(),
        events,
        child,
        cancelled,
        started,
    })
}

impl Drop for RunningJob {
    /// 兜底：任务对象被丢掉时（退出 TUI、切走、panic）**一定**把子进程带走。
    ///
    /// 不然一个后台 ffmpeg 会变成孤儿进程，继续闷头写你的输出文件。
    /// 已经 `wait` 过的子进程上再 `kill` 会返回 `InvalidInput`（不会误伤别人），
    /// 所以正常跑完的路径也走这里，是安全的。
    fn drop(&mut self) {
        if let Ok(mut child) = self.child.lock() {
            let _ = child.kill();
        }
    }
}

/// 一条流的读线程：逐行发 `Line` / `Progress`，返回攒下来的全文。
///
/// ffmpeg 的进度用 `\n` 分隔，统计行用 `\r`，所以两种都当行分隔符处理。
fn reader_thread(
    stream: Option<impl io::Read + Send + 'static>,
    is_stderr: bool,
    tx: mpsc::Sender<JobEvent>,
) -> thread::JoinHandle<String> {
    thread::spawn(move || {
        let mut collected = String::new();
        let Some(stream) = stream else {
            return collected;
        };

        let mut reader = BufReader::new(stream);
        let mut buffer = Vec::new();
        loop {
            buffer.clear();
            match reader.read_until(b'\n', &mut buffer) {
                Ok(0) | Err(_) => break,
                Ok(_) => {}
            }

            let chunk = String::from_utf8_lossy(&buffer).to_string();
            for raw in chunk.split(['\n', '\r']) {
                let line = raw.trim();
                if line.is_empty() {
                    continue;
                }

                // 进度行只走事件通道，**不进最终输出**：否则输出视图会被几十行
                // `frame=… / out_time=…` 刷屏，真正的结果反而看不见了。
                if let Some((key, value)) = line.split_once('=')
                    && is_progress_key(key.trim())
                {
                    let _ = tx.send(JobEvent::Progress {
                        key: key.trim().to_string(),
                        value: value.trim().to_string(),
                    });
                    continue;
                }

                let _ = tx.send(JobEvent::Line {
                    stderr: is_stderr,
                    text: line.to_string(),
                });
                collected.push_str(line);
                collected.push('\n');
            }
        }
        collected
    })
}

/// 留在 TUI 里执行一条命令并捕获输出。
///
/// 同样只传 argv、不经 shell；同样在 `cwd`（Toolbox 启动时的目录）里执行。
/// 注意它是**同步阻塞**的：输出很快的命令（jq / magick / pandoc / mediainfo）没问题，
/// 想看进度条的长时间任务应该用 `mode = "interactive"`。
pub fn run_captured(
    program: &Path,
    argv: &[String],
    cwd: &Path,
    label: &str,
) -> io::Result<Captured> {
    let started = Instant::now();
    let output = Command::new(program).args(argv).current_dir(cwd).output()?;
    let elapsed = started.elapsed();

    Ok(Captured {
        label: label.to_string(),
        command: format!("{} {}", program.display(), argv.join(" "))
            .trim_end()
            .to_string(),
        stdout: String::from_utf8_lossy(&output.stdout).to_string(),
        stderr: String::from_utf8_lossy(&output.stderr).to_string(),
        status: output.status.code(),
        success: output.status.success(),
        elapsed,
        cancelled: false,
    })
}

fn run_batch(jobs: &[Job], cwd: &Path) -> io::Result<ExecReport> {
    if jobs.is_empty() {
        return Ok(ExecReport::default());
    }

    suspend_terminal()?;

    let mut report = ExecReport::default();
    for job in jobs {
        report.launched += 1;

        // 带参数的动作先把要跑的命令打出来：用户既看得见结果，也顺手学到命令怎么写。
        // 这只是展示，真正执行仍然只传 argv。
        if !job.argv.is_empty() {
            println!("$ {} {}", job.program.display(), job.argv.join(" "));
        }

        let mut command = Command::new(&job.program);
        command.args(&job.argv);
        match command.current_dir(cwd).status() {
            Ok(status) if status.success() => {}
            Ok(status) => {
                report.failed += 1;
                eprintln!(
                    "Toolbox: {} 退出码 {}",
                    job.label,
                    status.code().unwrap_or(-1)
                );
            }
            Err(error) => {
                report.failed += 1;
                eprintln!("Toolbox: 无法执行 {}: {error}", job.label);
            }
        }
    }

    println!("\n按 Enter 返回 Toolbox…");
    let mut line = String::new();
    let _ = io::stdin().read_line(&mut line);

    resume_terminal()?;
    Ok(report)
}

/// 离开备用屏幕并恢复规范终端模式，把终端交给子进程。
fn suspend_terminal() -> io::Result<()> {
    disable_raw_mode()?;
    execute!(io::stdout(), LeaveAlternateScreen, DisableMouseCapture)?;
    Ok(())
}

/// 重新进入备用屏幕并打开 raw mode。
fn resume_terminal() -> io::Result<()> {
    execute!(io::stdout(), EnterAlternateScreen, EnableMouseCapture)?;
    enable_raw_mode()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use std::path::PathBuf;

    use super::{Captured, ExecReport, JobEvent, is_executable, spawn_captured};

    #[test]
    fn executable_check_requires_an_execute_permission_bit() {
        use std::os::unix::fs::PermissionsExt;

        let path =
            std::env::temp_dir().join(format!("toolbox-hub-not-executable-{}", std::process::id()));
        std::fs::write(&path, "not an executable").expect("write fixture");
        let mut permissions = std::fs::metadata(&path).expect("metadata").permissions();
        permissions.set_mode(0o644);
        std::fs::set_permissions(&path, permissions.clone()).expect("remove execute bit");
        assert!(!is_executable(&path));

        permissions.set_mode(0o755);
        std::fs::set_permissions(&path, permissions).expect("add execute bit");
        assert!(is_executable(&path));

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn captured_splits_stdout_and_stderr_and_summarises() {
        let captured = Captured {
            label: "jq".to_string(),
            command: "/usr/bin/jq . /tmp/x.json".to_string(),
            stdout: "a\nb\n".to_string(),
            stderr: "警告：某处\n".to_string(),
            status: Some(0),
            success: true,
            elapsed: Duration::from_millis(1200),
            cancelled: false,
        };

        let body = captured.body();
        assert!(body.contains("a\nb"), "{body}");
        assert!(body.contains("── stderr ──"), "stderr 要分开标注: {body}");
        assert!(body.contains("警告"), "{body}");
        assert_eq!(captured.summary(), "退出码 0 · 1.20s");
        assert!(captured.success);
    }

    #[test]
    fn captured_reports_empty_output_and_signal_death() {
        let captured = Captured {
            label: "true".to_string(),
            command: "true".to_string(),
            stdout: String::new(),
            stderr: String::new(),
            status: None,
            success: false,
            elapsed: Duration::from_millis(0),
            cancelled: false,
        };
        assert!(captured.body().contains("没有任何输出"));
        assert!(captured.summary().contains("被信号终止"));
    }

    /// 后台任务：实时行、结构化进度、最终结果。
    #[test]
    fn a_background_job_reports_lines_progress_and_result() {
        use std::{path::PathBuf, sync::mpsc::RecvTimeoutError};

        // 不用 shell：让 printf 直接吐几行，其中两行长得像 ffmpeg 的进度。
        let program = PathBuf::from("/usr/bin/printf");
        let argv = vec![String::from("hello\nframe=7\nout_time=00:00:03\nbye\n")];
        let job = spawn_captured(&program, &argv, &PathBuf::from("/tmp"), "printf").expect("spawn");

        let mut lines = Vec::new();
        let mut progress = Vec::new();
        let captured = loop {
            match job.events.recv_timeout(Duration::from_secs(10)) {
                Ok(JobEvent::Line { text, .. }) => lines.push(text),
                Ok(JobEvent::Progress { key, value }) => progress.push((key, value)),
                Ok(JobEvent::Done(captured)) => break captured,
                Err(RecvTimeoutError::Timeout) => panic!("等超时了，任务没发 Done"),
                Err(RecvTimeoutError::Disconnected) => panic!("事件通道断了"),
            }
        };

        assert_eq!(lines, vec!["hello", "bye"], "进度行不该混进普通输出");
        assert_eq!(
            progress,
            vec![
                (String::from("frame"), String::from("7")),
                (String::from("out_time"), String::from("00:00:03")),
            ]
        );
        assert!(captured.success);
        assert_eq!(captured.status, Some(0));
        assert!(captured.stdout.contains("hello") && captured.stdout.contains("bye"));
        assert!(
            !captured.stdout.contains("frame=7") && !captured.stdout.contains("out_time="),
            "进度行要从最终输出里剔掉，否则输出视图会被进度刷屏: {:?}",
            captured.stdout
        );
        assert!(!captured.cancelled);
    }

    /// 取消：长命令要能立刻打断，而且必须明确标成「被取消」。
    /// 任务对象被丢掉时必须把子进程带走 —— 否则退出 TUI 会留下孤儿进程。
    #[test]
    fn dropping_a_job_kills_its_child() {
        use std::path::PathBuf;

        let job = spawn_captured(
            &PathBuf::from("/usr/bin/sleep"),
            &[String::from("30")],
            &PathBuf::from("/tmp"),
            "sleep",
        )
        .expect("spawn");
        let pid = job.pid().expect("有 pid");
        let marker = PathBuf::from(format!("/proc/{pid}"));
        assert!(marker.exists(), "刚起来应该在");

        drop(job);

        // 收尾线程会把它回收掉，所以 /proc 里的目录最终会消失。
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while std::time::Instant::now() < deadline && marker.exists() {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(!marker.exists(), "drop 之后子进程必须已经死了（pid {pid}）");
    }

    /// `terminate()` 走的是**整个进程组**：工具自己拉起来的孙子进程也要一起走。
    ///
    /// 造一个真的孙子进程（非交互 shell 里的 `&` 子进程与 shell 同组）：
    /// 只杀父进程的话它会变成孤儿接着跑 —— 那正是「取消之后还有东西在写文件」
    /// 的来源。这条测试就是钉住「整组带走」。
    #[test]
    fn terminate_takes_the_whole_process_group() {
        use std::{path::PathBuf, sync::mpsc::RecvTimeoutError};

        let pidfile = std::env::temp_dir().join(format!("toolbox-hub-pgid-{}", std::process::id()));
        let _ = std::fs::remove_file(&pidfile);

        let script = format!("sleep 30 & echo $! > {}; wait", pidfile.display());
        let job = spawn_captured(
            &PathBuf::from("/bin/sh"),
            &[String::from("-c"), script],
            &PathBuf::from("/tmp"),
            "sh",
        )
        .expect("spawn");

        // 等孙子进程的 pid 落盘
        let mut grandchild = None;
        for _ in 0..100 {
            if let Ok(text) = std::fs::read_to_string(&pidfile)
                && let Ok(pid) = text.trim().parse::<i32>()
            {
                grandchild = Some(pid);
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let grandchild = grandchild.expect("孙子进程的 pid 应该写进文件了");
        assert!(alive(grandchild), "还没取消，它当然活着");

        job.terminate();

        let captured = loop {
            match job.events.recv_timeout(Duration::from_secs(10)) {
                Ok(JobEvent::Done(captured)) => break captured,
                Ok(_) => {}
                Err(RecvTimeoutError::Timeout) => panic!("SIGTERM 之后还是没结束"),
                Err(RecvTimeoutError::Disconnected) => panic!("事件通道断了"),
            }
        };
        assert!(captured.cancelled, "要标成被取消");

        // 孙子进程要跟着走（SIGTERM 之后可能先变僵尸，所以给它一点时间）
        let mut gone = false;
        for _ in 0..100 {
            if !alive(grandchild) {
                gone = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(gone, "孙子进程 {grandchild} 还活着 —— 进程组没带走");

        let _ = std::fs::remove_file(&pidfile);
    }

    /// 这个 pid 还活着吗（`kill(pid, 0)` 不发信号，只做存在性检查）。
    fn alive(pid: i32) -> bool {
        // SAFETY: 信号 0 不发送任何东西，只检查进程是否存在。
        unsafe { libc::kill(pid, 0) == 0 }
    }

    #[test]
    fn a_background_job_can_be_cancelled() {
        use std::{path::PathBuf, sync::mpsc::RecvTimeoutError, time::Instant};

        let started = Instant::now();
        let job = spawn_captured(
            &PathBuf::from("/usr/bin/sleep"),
            &[String::from("30")],
            &PathBuf::from("/tmp"),
            "sleep",
        )
        .expect("spawn");
        job.cancel();

        let captured = loop {
            match job.events.recv_timeout(Duration::from_secs(10)) {
                Ok(JobEvent::Done(captured)) => break captured,
                Ok(_) => {}
                Err(RecvTimeoutError::Timeout) => panic!("取消之后还是没结束"),
                Err(RecvTimeoutError::Disconnected) => panic!("事件通道断了"),
            }
        };

        assert!(captured.cancelled, "要标成被取消");
        assert!(!captured.success, "取消不算成功");
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "取消应该立刻生效，实际 {:?}",
            started.elapsed()
        );
    }

    #[test]
    fn ffmpeg_style_durations_parse() {
        assert_eq!(super::parse_duration("62.5"), Some(62.5));
        assert_eq!(super::parse_duration("01:02"), Some(62.0));
        assert_eq!(super::parse_duration("00:01:02.5"), Some(62.5));
        assert_eq!(super::parse_duration("  00:00:03  "), Some(3.0));
        assert_eq!(super::parse_duration(""), None);
        assert_eq!(super::parse_duration("abc"), None);
        assert_eq!(super::parse_duration("1:2:3:4"), None, "层数不对就不认");
    }

    #[test]
    fn browse_files_parse_into_paths() {
        assert_eq!(
            super::parse_cwd_file("/tmp/media\n"),
            Some(PathBuf::from("/tmp/media"))
        );
        assert_eq!(
            super::parse_cwd_file("\n  /tmp/media  \n"),
            Some(PathBuf::from("/tmp/media")),
            "空行要跳过、两边空白要去掉"
        );
        assert_eq!(super::parse_cwd_file("   \n"), None);

        assert_eq!(
            super::parse_chooser_file("/tmp/a.mp4\n/tmp/b.mp4\n"),
            vec![PathBuf::from("/tmp/a.mp4"), PathBuf::from("/tmp/b.mp4")]
        );
        assert!(super::parse_chooser_file("\n\n").is_empty());
    }

    /// 没装的程序要报 `NotFound`，调用方好据此提示而不是崩掉。
    #[test]
    fn a_missing_file_manager_reports_not_found() {
        let error =
            super::browse_directories("definitely-not-a-real-program-xyz", &PathBuf::from("/tmp"))
                .expect_err("应该报错");
        assert_eq!(error.kind(), std::io::ErrorKind::NotFound);
    }

    #[test]
    fn message_summarises_success_and_failure() {
        assert_eq!(
            ExecReport {
                launched: 3,
                failed: 0
            }
            .message(),
            "执行完成 · 3 个工具"
        );
        assert_eq!(
            ExecReport {
                launched: 3,
                failed: 1
            }
            .message(),
            "执行完成 · 2 个成功 / 1 个失败"
        );
        assert_eq!(ExecReport::default().message(), "执行完成 · 0 个工具");
    }
}
