//! Toolbox Hub —— 统一 Linux 工具箱的 TUI 入口。
//!
//! 本文件只做四件事：解析启动参数、装配 Registry、初始化/还原终端、驱动事件循环。
//! 其余职责全部下沉到模块：
//!
//! * [`model`]     —— Provider 无关的数据结构（域、工具定义）
//! * [`providers`] —— 具体工具来源（当前只有 FFTools）
//! * [`registry`]  —— Provider 聚合、发现、重载、过滤
//! * [`app`]       —— 界面状态与按键处理
//! * [`ui`]        —— 纯渲染
//! * [`runtime`]   —— 工具执行（保持 Toolbox 启动时的原始工作目录）

mod app;
mod cli;
mod components;
mod config;
mod history;
mod media;
mod model;
mod packages;
mod preview;
mod providers;
mod registry;
mod runtime;
mod state;
mod ui;

use std::{env, io, path::PathBuf, time::Duration};

use ratatui::{
    Terminal,
    backend::CrosstermBackend,
    crossterm::{
        cursor,
        event::{
            self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste,
            EnableMouseCapture, Event,
        },
        execute,
        terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
    },
};

use crate::{app::App, registry::Registry};

type Tui = Terminal<CrosstermBackend<io::Stdout>>;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    install_panic_hook();

    // 先看这次是「命令行模式」还是「进 TUI」：带动作参数时干完就退，
    // 一个终端都不进（`toolbox-hub -s fzf | head` 要能在管道里安静地跑）。
    let invocation = match cli::parse(env::args().skip(1)) {
        Ok(invocation) => invocation,
        Err(error) => {
            eprintln!("toolbox-hub: {error}");
            std::process::exit(2);
        }
    };
    let bin_dir = match invocation {
        cli::Invocation::Tui { bin_dir, dirs } => {
            // 目录必须在**任何人读路径之前**定下来（state / tools.d / 队列都在后面读）
            config::configure(dirs.config, dirs.data);
            // 第一次跑就写一份带注释的 packages.toml —— 不写的话没人知道有它
            config::ensure_packages_template();
            resolve_bin_dir(bin_dir)
        }
        cli::Invocation::Command(options) => {
            config::configure(options.dirs.config.clone(), options.dirs.data.clone());
            if let Err(error) = cli::run(&options) {
                eprintln!("toolbox-hub: {error}");
                std::process::exit(1);
            }
            return Ok(());
        }
    };

    let (registry, report) = Registry::discover(&bin_dir);
    let mut app = App::new(registry, bin_dir, report);
    // 工作目录决定工具在哪儿找文件（FFTools 那批脚本尤其依赖它），
    // 顺序：TOOLBOX_HUB_WORKDIR > 上次记住的 > 启动时所在目录。
    app.resolve_work_dir();

    let mut terminal = setup_terminal()?;
    // 崩了也要把终端还回去（不然留在 raw + 备用屏幕里，终端会花）。
    install_panic_hook();

    let result = run(&mut terminal, &mut app);

    // 退出前把还在跑的后台任务带走，别留下孤儿进程闷头写文件。
    // （先 SIGTERM 让它收尾；RunningJob 的 Drop 会兜底 SIGKILL。）
    app.cancel_job();
    restore_terminal(&mut terminal)?;
    result
}

/// 工具目录解析顺序：命令行给的位置参数 > `FZF_FFTOOLS_BIN_DIR` > `$HOME/.local/bin`。
fn resolve_bin_dir(from_args: Option<PathBuf>) -> PathBuf {
    from_args
        .or_else(|| env::var_os("FZF_FFTOOLS_BIN_DIR").map(PathBuf::from))
        .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/bin")))
        .unwrap_or_else(|| PathBuf::from(".local/bin"))
}

/// 安装 panic 钩子：先把终端恢复成正常模样，再交给原来的钩子打印信息。
///
/// TUI 程序崩在 raw mode 里是最讨厌的一种崩法 —— 信息看不见、终端还花掉。
fn install_panic_hook() {
    let original = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = disable_raw_mode();
        let _ = execute!(
            io::stdout(),
            LeaveAlternateScreen,
            DisableMouseCapture,
            DisableBracketedPaste,
            cursor::Show
        );
        original(info);
    }));
}

fn setup_terminal() -> io::Result<Tui> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    // 打开括号粘贴：终端会把「粘贴」当成**一个事件**送过来，而不是几十次按键。
    // 好处有二：快；而且粘进来的多行文本不会被当成一串回车（以前粘一条带换行的
    // 命令，就会顺手把命令发出去）。
    execute!(
        stdout,
        EnterAlternateScreen,
        EnableMouseCapture,
        EnableBracketedPaste
    )?;
    Terminal::new(CrosstermBackend::new(stdout))
}

fn restore_terminal(terminal: &mut Tui) -> io::Result<()> {
    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        DisableMouseCapture,
        DisableBracketedPaste
    )?;
    terminal.show_cursor()
}

fn run(terminal: &mut Tui, app: &mut App) -> Result<(), Box<dyn std::error::Error>> {
    // 只在「有事发生」的那一帧才画。
    //
    // 以前是每轮无条件 `terminal.draw`，而主循环 100ms 一轮 —— 没人操作也每秒画
    // 十帧，每帧还要把所有行重新构造一遍（已安装列表 2271 行就是上万次分配）。
    // 实测空转 3~4% CPU，纯属白烧。现在只有按键/鼠标/后台有变动/需要整屏重画
    // 这几种情况才画。
    let mut dirty = true;
    let mut shown_reload_second = app.seconds_since_reload();
    loop {
        let reload_second = app.seconds_since_reload();
        if reload_second != shown_reload_second {
            shown_reload_second = reload_second;
            dirty = true;
        }
        // 接管过终端（跑了交互式工具）以后，必须先丢掉 ratatui 的旧帧：
        // 它记着接管前那一帧，而离开备用屏幕时物理屏已经空了，直接 draw 会因为
        // 「没变化」而一个格子都不写 —— 界面看起来就只剩一行状态文字。
        if app.take_full_redraw() {
            terminal.clear()?;
            dirty = true;
        }
        if dirty {
            terminal.draw(|frame| ui::draw(frame, app))?;
            dirty = false;
        }
        // 能力探测会同步等待终端应答；让 UI 先画出“准备预览”，并且不要放在 draw 回调里。
        if app.preview.needs_detection() {
            app.preview.detect();
            dirty = true;
        }
        // 每轮都重新取一次：用户可能刚按 `d` 改过工作目录。
        let cwd = app.work_dir.clone();

        // 尺寸变化不做特殊处理：下一帧 draw 会按新的 area 重新布局。
        // 有按键就处理；没有按键也**不能** continue（后台事件还得取）。
        if event::poll(Duration::from_millis(100))? {
            dirty = true; // 有输入（含窗口尺寸变化）就重画
            match event::read()? {
                Event::Key(key) if app::handle_key(app, key, &cwd)? => break,
                Event::Mouse(mouse) => app::handle_mouse(app, mouse, &cwd)?,
                Event::Paste(text) => app::handle_paste(app, &text),
                _ => {}
            }
        }

        // 后台任务的事件每帧都要取一次：
        // * 只放在「有按键」的分支后面是不行的 —— 没人敲键盘时进度就不动了
        //   （实测：面板上耗时在走、进度和输出一直空着，按一下键才刷出来）；
        // * 放在按键**之后**是为了让「按 q 取消」不会顺手把刚打开的输出视图关掉。
        if app.poll_job(&cwd) {
            dirty = true;
        }
        if app.poll_packages() {
            dirty = true;
        }
        if app.poll_preview() {
            dirty = true;
        }
    }
    Ok(())
}
