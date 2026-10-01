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
mod history;
mod media;
mod model;
mod packages;
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
        event::{self, Event},
        execute,
        terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
    },
};

use crate::{app::App, registry::Registry};

type Tui = Terminal<CrosstermBackend<io::Stdout>>;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let bin_dir = resolve_bin_dir();
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

/// 工具目录解析顺序：`argv[1]` > `FZF_FFTOOLS_BIN_DIR` > `$HOME/.local/bin`。
fn resolve_bin_dir() -> PathBuf {
    env::args()
        .nth(1)
        .map(PathBuf::from)
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
        let _ = execute!(io::stdout(), LeaveAlternateScreen, cursor::Show);
        original(info);
    }));
}

fn setup_terminal() -> io::Result<Tui> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    Terminal::new(CrosstermBackend::new(stdout))
}

fn restore_terminal(terminal: &mut Tui) -> io::Result<()> {
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()
}

fn run(terminal: &mut Tui, app: &mut App) -> Result<(), Box<dyn std::error::Error>> {
    loop {
        // 接管过终端（跑了交互式工具）以后，必须先丢掉 ratatui 的旧帧：
        // 它记着接管前那一帧，而离开备用屏幕时物理屏已经空了，直接 draw 会因为
        // 「没变化」而一个格子都不写 —— 界面看起来就只剩一行状态文字。
        if app.take_full_redraw() {
            terminal.clear()?;
        }
        terminal.draw(|frame| ui::draw(frame, app))?;
        // 每轮都重新取一次：用户可能刚按 `d` 改过工作目录。
        let cwd = app.work_dir.clone();

        // 尺寸变化不做特殊处理：下一帧 draw 会按新的 area 重新布局。
        // 有按键就处理；没有按键也**不能** continue（后台事件还得取）。
        if event::poll(Duration::from_millis(100))?
            && let Event::Key(key) = event::read()?
            && app::handle_key(app, key, &cwd)?
        {
            break;
        }

        // 后台任务的事件每帧都要取一次：
        // * 只放在「有按键」的分支后面是不行的 —— 没人敲键盘时进度就不动了
        //   （实测：面板上耗时在走、进度和输出一直空着，按一下键才刷出来）；
        // * 放在按键**之后**是为了让「按 q 取消」不会顺手把刚打开的输出视图关掉。
        app.poll_job(&cwd);
        app.poll_packages();
    }
    Ok(())
}
