//! 命令行模式：不进 TUI 也能搜、装、卸、更新、看新闻、列已安装、清缓存。
//!
//! 为什么值得单独做一层：pacsea 好用的地方一半在 `-s/-i/-r/-u/-n/--list/--clear-cache`
//! 那一半 —— 它能被**别的程序调用**（`toolbox-hub -s fzf | head`、写进脚本、绑到快捷键）。
//! TUI 负责「看和挑」，CLI 负责「被调用时别啰嗦」。
//!
//! 两条硬约束：
//!
//! 1. **命令翻译只有一份**：CLI 与 TUI 都走 [`crate::packages::PackageOperation`] +
//!    [`crate::packages::escalate`]，不会出现「界面里算出来的命令」和「命令行里
//!    算出来的命令」不一样；
//! 2. `--dry-run` 就是「只打印、不执行」，和 TUI 里那个演练模式一回事。

use std::{io::Write, path::PathBuf, process::Command};

use crate::packages::{
    self, InstalledFilter, InstalledPackage, NewsFilter, NewsItem, PackageHit, PackageOperation,
    libalpm::Db, probe::Net,
};

/// 一次命令行调用想干什么。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Action {
    /// `-s`：搜官方源 + AUR。
    Search(String),
    /// `-i`：装这些包。
    Install(Vec<String>),
    /// `-r`：卸这些包。
    Remove(Vec<String>),
    /// `-u`：系统更新。
    Update,
    /// `-n`：看 Arch 新闻（范围由 `--unread/--read/--all-news` 决定）。
    News(NewsFilter),
    /// `-l`：列已安装包（范围由 `--exp/--imp/--all` 决定）。
    List(InstalledFilter),
    /// `--remove-orphans`：清孤儿包。
    RemoveOrphans,
    /// `--clear-cache`：清下载缓存。
    ClearCache,
    Help,
    Version,
}

/// 命令行调用的全部参数。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Options {
    /// 只打印将要执行的命令，不动系统。
    pub dry_run: bool,
    pub action: Action,
}

/// 解析启动参数之后得到的东西。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Invocation {
    /// 没有动作参数：进 TUI（可选一个脚本目录）。
    Tui { bin_dir: Option<PathBuf> },
    /// 有动作参数：干完就退。
    Command(Options),
}

/// 解析参数。
///
/// 位置参数沿用老语义：`toolbox-hub [脚本目录]`。一旦出现动作参数，位置参数就
/// 不再是脚本目录了（那多半是打错字），直接报错，别默默忽略。
pub fn parse<I>(args: I) -> Result<Invocation, String>
where
    I: IntoIterator<Item = String>,
{
    let mut action: Option<Action> = None;
    let mut dry_run = false;
    let mut bin_dir: Option<PathBuf> = None;
    let mut news_scope: Option<NewsFilter> = None;
    let mut list_scope: Option<InstalledFilter> = None;

    let mut args = args.into_iter().peekable();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--dry-run" => dry_run = true,
            "-h" | "--help" => return Ok(command(dry_run, Action::Help)),
            "-V" | "--version" => return Ok(command(dry_run, Action::Version)),

            "-s" | "--search" => {
                let term = one_value(&mut args, &arg)?;
                set_action(&mut action, Action::Search(term))?;
            }
            "-i" | "--install" => {
                let names = package_values(&mut args)?;
                set_action(&mut action, Action::Install(names))?;
            }
            "-r" | "--remove" => {
                let names = package_values(&mut args)?;
                set_action(&mut action, Action::Remove(names))?;
            }
            "-u" | "--update" => set_action(&mut action, Action::Update)?,
            "-n" | "--news" => set_action(&mut action, Action::News(NewsFilter::All))?,
            "--unread" => news_scope = Some(NewsFilter::Unread),
            "--read" => news_scope = Some(NewsFilter::Read),
            "-a" | "--all-news" => news_scope = Some(NewsFilter::All),

            "-l" | "--list" => set_action(&mut action, Action::List(InstalledFilter::All))?,
            "--exp" => list_scope = Some(InstalledFilter::Explicit),
            "--imp" => list_scope = Some(InstalledFilter::Dependency),
            // `--all` 同时给新闻和已安装列表兜底（哪个动作在跑就跟哪个走）
            "--all" => {
                news_scope = Some(NewsFilter::All);
                list_scope = Some(InstalledFilter::All);
            }
            "--orphans" => set_action(&mut action, Action::List(InstalledFilter::Orphan))?,
            "--remove-orphans" => set_action(&mut action, Action::RemoveOrphans)?,
            "--clear-cache" => set_action(&mut action, Action::ClearCache)?,

            other if other.starts_with('-') && other != "-" => {
                return Err(format!("不认识的选项：{other}（看 toolbox-hub --help）"));
            }
            other => {
                if action.is_some() {
                    return Err(format!("多余的参数：{other}"));
                }
                bin_dir = Some(PathBuf::from(other));
            }
        }
    }

    let Some(mut chosen) = action else {
        return Ok(Invocation::Tui { bin_dir });
    };

    // 范围参数搭在动作上：`--unread` 只对 `-n` 有意义，`--exp` 只对 `-l` 有意义。
    //
    // 注意只在用户**显式**给了范围时才覆盖：`--orphans` 自己就带着
    // `InstalledFilter::Orphan`，无脑用默认值覆盖会把它冲成「全部」。
    match &mut chosen {
        Action::News(scope) => {
            if let Some(explicit) = news_scope {
                *scope = explicit;
            }
        }
        Action::List(scope) => {
            if let Some(explicit) = list_scope {
                *scope = explicit;
            }
        }
        _ => {}
    }

    Ok(Invocation::Command(Options {
        dry_run,
        action: chosen,
    }))
}

fn command(dry_run: bool, action: Action) -> Invocation {
    Invocation::Command(Options { dry_run, action })
}

/// 一次只能有一个动作参数（`-s` 和 `-i` 同时给是打错了，不是「先搜再装」）。
fn set_action(slot: &mut Option<Action>, action: Action) -> Result<(), String> {
    if slot.is_some() {
        return Err(String::from(
            "一次只能给一个动作（-s / -i / -r / -u / -n / -l / --clear-cache）",
        ));
    }
    *slot = Some(action);
    Ok(())
}

fn one_value<I: Iterator<Item = String>>(
    args: &mut std::iter::Peekable<I>,
    flag: &str,
) -> Result<String, String> {
    let value = args
        .next()
        .ok_or_else(|| format!("{flag} 后面要跟一个值"))?;
    if value.trim().is_empty() {
        return Err(format!("{flag} 后面的值是空的"));
    }
    Ok(value)
}

/// 包名列表：一直吃到下一个选项为止；逗号分隔也认（pacsea 两种都收）。
fn package_values<I: Iterator<Item = String>>(
    args: &mut std::iter::Peekable<I>,
) -> Result<Vec<String>, String> {
    let mut names = Vec::new();
    while let Some(next) = args.peek() {
        if next.starts_with('-') && next.len() > 1 {
            break;
        }
        let next = args.next().unwrap_or_default();
        names.extend(
            next.split(',')
                .map(str::trim)
                .filter(|name| !name.is_empty())
                .map(str::to_string),
        );
    }
    if names.is_empty() {
        return Err(String::from("这里至少要一个包名"));
    }
    Ok(names)
}

/// 跑一个动作。返回给用户看的错误（已经打印过的就不重复了）。
pub fn run(options: &Options) -> Result<(), String> {
    match &options.action {
        Action::Help => {
            print!("{USAGE}");
            Ok(())
        }
        Action::Version => {
            println!("toolbox-hub {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        Action::Search(term) => search(term),
        Action::Install(names) => package_op(PackageOperation::Install, names, options.dry_run),
        Action::Remove(names) => package_op(PackageOperation::Remove, names, options.dry_run),
        Action::Update => {
            let program = if packages::probe::has_paru() {
                "paru"
            } else {
                "pacman"
            };
            let (program, argv) =
                packages::escalate(program, &[String::from(packages::UPGRADE_FLAG)]);
            execute(&program, &argv, options.dry_run, "系统更新")
        }
        Action::News(scope) => news(*scope),
        Action::List(scope) => list_installed(*scope),
        Action::RemoveOrphans => remove_orphans(options.dry_run),
        Action::ClearCache => {
            let (program, argv) = packages::cache_command(1, packages::probe::has_paccache());
            let (program, argv) = packages::escalate(&program, &argv);
            execute(&program, &argv, options.dry_run, "清包缓存")
        }
    }
}

const USAGE: &str = "\
Toolbox Hub —— Linux CLI 工具箱（TUI + 命令行两用）

用法: toolbox-hub [选项] [脚本目录]

不带选项时进入 TUI；带上下面任一动作就干完即退（可以写进脚本）。

动作:
  -s, --search <词>          搜官方源 + AUR
  -i, --install <包...>      安装（空格或逗号分隔；含 AUR 时自动走 paru）
  -r, --remove <包...>       卸载（-Rns，会清掉只被它们依赖的依赖）
  -u, --update               系统更新（有 paru 用 paru，否则 sudo pacman -Syu）
  -n, --news                 看 Arch 新闻
      --unread               只看未读（与 -n 搭配）
      --read                 只看已读（与 -n 搭配）
  -l, --list                 列已安装包
      --exp                  只看自己点名装的（与 -l 搭配）
      --imp                  只看被依赖拖进来的（与 -l 搭配）
      --all                  全部（-n 与 -l 都认；也是它们的默认值）
      --orphans              列出孤儿包（没人依赖、你也没点名装）
      --remove-orphans       卸载孤儿包
      --clear-cache          清包缓存（paccache -rk1，没有 paccache 才退回 pacman -Sc）

通用:
      --dry-run              只打印将要执行的命令，不动系统
  -h, --help                 显示这份帮助
  -V, --version              显示版本

TUI 里的按键: 进界面按 ? 看全部（包管理是 p 键）。
";

// ── 各个动作 ────────────────────────────────────────────────────────────────

fn search(term: &str) -> Result<(), String> {
    let db = Db::open()?;
    let mut net = Net::new();

    let mut outcome = crate::packages::probe::SearchOutcome::default();
    match db.search(term) {
        Ok(hits) => outcome.hits.extend(hits),
        Err(error) => outcome.errors.push(format!("官方源：{error}")),
    }
    match net.search_aur(term, db.local_versions()) {
        Ok(hits) => outcome.hits.extend(hits),
        Err(error) => outcome.errors.push(format!("AUR：{error}")),
    }
    outcome.hits.sort_by(|a, b| {
        b.is_installed()
            .cmp(&a.is_installed())
            .then(b.votes.unwrap_or(0).cmp(&a.votes.unwrap_or(0)))
            .then(a.name.cmp(&b.name))
    });

    for error in &outcome.errors {
        eprintln!("（一路失败，可接受）：{error}");
    }
    if outcome.hits.is_empty() {
        println!("没有匹配「{term}」的包");
        return Ok(());
    }

    println!(
        "{} 个结果（仓库 · 名字 · 版本 · 说明 · 状态）",
        outcome.hits.len()
    );
    for hit in &outcome.hits {
        println!("{}", hit_line(hit));
    }
    Ok(())
}

/// 一行结果：`extra  fzf  0.74.4-1  Command-line fuzzy finder  ✓ 已安装`
fn hit_line(hit: &PackageHit) -> String {
    let mut line = format!(
        "{:<16} {:<28} {:<14} {}",
        hit.repo, hit.name, hit.version, hit.description
    );
    let status = hit.status_label();
    if !status.is_empty() {
        line.push_str("  ");
        line.push_str(&status);
    }
    if let Some(votes) = hit.votes {
        line.push_str(&format!("  票 {votes}"));
    }
    line
}

fn package_op(operation: PackageOperation, names: &[String], dry_run: bool) -> Result<(), String> {
    // 队列里有没有 AUR？CLI 没法从队列里知道，只能问一次「这个名字在同步库里吗」。
    // 宁可多问一次也别默认用 pacman：AUR 包用 pacman 装只会得到一句「找不到目标」。
    let uses_aur = names.iter().any(|name| is_aur_only(name));
    let (program, argv) = packages::escalate(operation.program(uses_aur), &operation.argv(names));
    execute(&program, &argv, dry_run, operation.label())
}

/// 这个名字是不是只有 AUR 有（不在官方源里）。
///
/// 查的是 libalpm（本地数据库），不起 `pacman -Ss` 进程 —— 这也是「装的时候用
/// pacman 还是 paru」的判断依据。库打不开就当它不是 AUR 包（宁可让 pacman 报错，
/// 也不要莫名其妙把活派给 paru）。
fn is_aur_only(name: &str) -> bool {
    Db::open()
        .and_then(|db| db.search(name))
        .map(|hits| !hits.iter().any(|hit| hit.name == name))
        .unwrap_or(false)
}

fn execute(program: &str, argv: &[String], dry_run: bool, what: &str) -> Result<(), String> {
    let preview = packages::command_preview(program, argv);
    if dry_run {
        println!("（演练，没有执行）{preview}");
        return Ok(());
    }

    println!("$ {preview}");
    // 交给外面之前把已经打印的东西冲出去，免得 sudo 的提示插在输出中间
    let _ = std::io::stdout().flush();

    let status = Command::new(program).args(argv).status().map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            format!("系统里没有 {program}")
        } else {
            format!("{program} 跑不起来：{error}")
        }
    })?;

    if status.success() {
        println!("{what}完成");
        Ok(())
    } else {
        Err(format!("{what}失败（退出码 {:?}）", status.code()))
    }
}

fn news(scope: NewsFilter) -> Result<(), String> {
    let items = Net::new().news()?;
    let read = packages::load_read_news(&packages::read_news_path());
    let mark = packages::probe::last_upgrade();

    let visible: Vec<&NewsItem> = items
        .iter()
        .filter(|item| scope.matches(item, &read))
        .collect();
    if visible.is_empty() {
        println!("没有{}的新闻（一共 {} 条）", scope.label(), items.len());
        return Ok(());
    }

    println!(
        "Arch 新闻 {} 条（{}）· 未读 {} · 升级后发布 {}",
        visible.len(),
        scope.label(),
        items
            .iter()
            .filter(|item| !read.contains(&packages::news_key(item)))
            .count(),
        packages::news_published_since(&items, mark)
    );
    for item in visible {
        let mark_char = if read.contains(&packages::news_key(item)) {
            " "
        } else {
            "●"
        };
        println!("{mark_char} {}", item.title);
        println!("    {}  {}", item.published, item.link);
    }
    Ok(())
}

fn list_installed(scope: InstalledFilter) -> Result<(), String> {
    let all = Db::open()?.installed()?;
    let shown: Vec<&InstalledPackage> = all
        .iter()
        .filter(|package| scope.matches(package))
        .collect();

    println!(
        "{} 个包（{}）· 外来 {} · 孤儿 {}",
        shown.len(),
        scope.label(),
        all.iter().filter(|package| package.foreign).count(),
        all.iter().filter(|package| package.orphan).count()
    );
    for package in shown {
        println!(
            "{:<6} {:<32} {}",
            package.tag(),
            package.name,
            package.version
        );
    }
    Ok(())
}

fn remove_orphans(dry_run: bool) -> Result<(), String> {
    let names = Db::open()?.orphan_names();
    if names.is_empty() {
        println!("没有孤儿包，系统很干净");
        return Ok(());
    }
    let (program, argv) = packages::orphan_remove_command(&names);
    let (program, argv) = packages::escalate(&program, &argv);
    execute(
        &program,
        &argv,
        dry_run,
        &format!("卸载 {} 个孤儿包", names.len()),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_args(args: &[&str]) -> Invocation {
        parse(args.iter().map(|arg| arg.to_string())).expect("应该能解析")
    }

    fn action(args: &[&str]) -> Action {
        match parse_args(args) {
            Invocation::Command(options) => options.action,
            other => panic!("期望是个动作，得到 {other:?}"),
        }
    }

    #[test]
    fn no_arguments_means_tui() {
        assert_eq!(parse_args(&[]), Invocation::Tui { bin_dir: None });
        // 老语义还在：位置参数就是脚本目录
        assert_eq!(
            parse_args(&["/tmp/scripts"]),
            Invocation::Tui {
                bin_dir: Some(PathBuf::from("/tmp/scripts"))
            }
        );
    }

    #[test]
    fn short_and_long_flags_are_the_same_thing() {
        assert_eq!(action(&["-s", "fzf"]), Action::Search(String::from("fzf")));
        assert_eq!(
            action(&["--search", "fzf"]),
            Action::Search(String::from("fzf"))
        );
        assert_eq!(action(&["-u"]), Action::Update);
        assert_eq!(action(&["--update"]), Action::Update);
    }

    #[test]
    fn install_takes_space_and_comma_separated_names() {
        assert_eq!(
            action(&["-i", "fzf", "ripgrep"]),
            Action::Install(vec![String::from("fzf"), String::from("ripgrep")])
        );
        assert_eq!(
            action(&["--install", "fzf,ripgrep"]),
            Action::Install(vec![String::from("fzf"), String::from("ripgrep")])
        );
        // 后面跟着选项就停住
        assert_eq!(
            action(&["-i", "fzf", "--dry-run"]),
            Action::Install(vec![String::from("fzf")])
        );
    }

    #[test]
    fn news_and_list_scopes_attach_to_their_action() {
        assert_eq!(action(&["-n"]), Action::News(NewsFilter::All));
        assert_eq!(
            action(&["-n", "--unread"]),
            Action::News(NewsFilter::Unread)
        );
        assert_eq!(action(&["--read", "-n"]), Action::News(NewsFilter::Read));
        assert_eq!(action(&["-l"]), Action::List(InstalledFilter::All));
        assert_eq!(
            action(&["-l", "--exp"]),
            Action::List(InstalledFilter::Explicit)
        );
        assert_eq!(
            action(&["--imp", "--list"]),
            Action::List(InstalledFilter::Dependency)
        );
        assert_eq!(
            action(&["--orphans"]),
            Action::List(InstalledFilter::Orphan)
        );
        // `--all` 两边都兜底
        assert_eq!(action(&["-n", "--all"]), Action::News(NewsFilter::All));
    }

    #[test]
    fn dry_run_is_orthogonal_to_every_action() {
        match parse_args(&["--dry-run", "-u"]) {
            Invocation::Command(options) => {
                assert!(options.dry_run);
                assert_eq!(options.action, Action::Update);
            }
            other => panic!("期望是个动作：{other:?}"),
        }
    }

    #[test]
    fn mistakes_are_reported_instead_of_ignored() {
        let bad = |args: &[&str]| parse(args.iter().map(|arg| arg.to_string())).unwrap_err();

        assert!(bad(&["-s"]).contains("要跟一个值"), "缺值要说清楚");
        assert!(bad(&["-i"]).contains("至少要一个包名"));
        assert!(bad(&["-s", "fzf", "-i", "fzf"]).contains("只能给一个动作"));
        assert!(bad(&["--nope"]).contains("不认识的选项"));
        // 有动作参数时位置参数不再当脚本目录
        assert!(bad(&["-u", "/tmp/scripts"]).contains("多余的参数"));
    }

    #[test]
    fn help_and_version_short_circuit() {
        assert_eq!(action(&["--help"]), Action::Help);
        assert_eq!(action(&["-V"]), Action::Version);
        // 混在别的东西里也照样短路：`--help` 永远赢，不会因为你前面写了 -i 就忽略它
        assert_eq!(action(&["-i", "fzf", "--help"]), Action::Help);
        assert_eq!(action(&["--dry-run", "-h"]), Action::Help);
    }
}
