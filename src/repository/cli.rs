//! 命令行下的仓库与工具操作。
//!
//! 这一层**薄**是刻意的：它只负责解析参数、把结果排版成给人看的样子，
//! 真正干活的是 repository::service —— 所以 TUI 与 CLI 不会做出两套行为。
//!
//! # 和软件包命令怎么区分
//!
//! 带单横线的老命令（-s / -i / -r / -u / -l）管的是 **pacman / AUR 的软件包**。
//! 这里这批动词管的是 **ToolHub 自己的工具包**。两者刻意用了不同的名字，
//! 因为把它们混在一起是最容易出事的地方 —— 卸载一个 Arch 包和卸载一个工具包，
//! 后果完全不同。

use std::{
    io::Write,
    path::{Path, PathBuf},
    process::Command as Process,
};

use unicode_width::UnicodeWidthStr;

use crate::{
    model::ToolDefinition,
    registry::Registry,
    repository::{
        cache::CacheState,
        config::{Repositories, RepositoryConfig, Trust},
        install::{self, InstallPlan},
        service::{SearchScope, Service},
    },
};

/// 认识的那几个动词。
pub const VERBS: &[&str] = &[
    "repo",
    "search",
    "info",
    "install",
    "uninstall",
    "update",
    "list",
    "run",
    "new",
    "check",
    "build",
];

/// 这个词是不是一个动词（区别于「脚本目录」这个老的位置参数）。
pub fn is_verb(word: &str) -> bool {
    VERBS.contains(&word)
}

/// 一次仓库/工具操作。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Command {
    Repo(RepoCommand),
    Tool(ToolCommand),
    /// 作者工具：写插件的人用的（脚手架 / 校验 / 打包）。
    Author(AuthorCommand),
}

/// toolbox-hub new|check|build —— 给**写插件的人**用的。
///
/// 和上面两组分开是有意的：repo/install/uninstall 服务的是「装包的人」，
/// 这三个服务的是「写包的人」。混在一起，作者会以为自己在操作自己的仓库配置。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AuthorCommand {
    /// 造一个新包骨架。
    New {
        name: String,
        dir: Option<PathBuf>,
        kind: crate::repository::author::ScaffoldKind,
        description: Option<String>,
        domain: Option<String>,
        program: Option<String>,
    },
    /// 校验一个包目录，或一整个仓库。
    Check { path: Option<PathBuf> },
    /// 打包 + 重建索引。
    Build { path: Option<PathBuf>, stamp: bool },
}

/// `toolbox-hub repo …`
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RepoCommand {
    List,
    Add {
        location: String,
        name: Option<String>,
        trust: Option<String>,
        priority: Option<i32>,
    },
    Remove(String),
    Enable(String),
    Disable(String),
    /// None = 刷新全部。
    Update(Option<String>),
    Search {
        query: String,
        scope: SearchScope,
    },
    /// 打印索引缓存目录（排障用）。
    Path,
}

/// 其余动词。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ToolCommand {
    Search {
        query: String,
        scope: SearchScope,
    },
    Info(String),
    Install {
        names: Vec<String>,
        allow_unverified: bool,
    },
    Uninstall {
        names: Vec<String>,
        purge_modified: bool,
    },
    Update {
        allow_unverified: bool,
        /// 只检查并报告，不下载不替换（给脚本 / systemd timer 用）。
        ///
        /// 有更新时退出码 10，没有更新是 0（和 checkupdates 一路）。
        check: bool,
    },
    List,
    Run {
        id: String,
        values: Vec<(String, String)>,
    },
}

// ── 解析 ────────────────────────────────────────────────────────────────────

/// 解析一个动词后面的参数（--dry-run 之类全局选项已经由 cli.rs 摘掉了）。
pub fn parse(verb: &str, rest: &[String]) -> Result<Command, String> {
    match verb {
        "repo" => parse_repo(rest).map(Command::Repo),
        "search" => {
            let (query, scope) = parse_search(rest)?;
            Ok(Command::Tool(ToolCommand::Search { query, scope }))
        }
        "info" => Ok(Command::Tool(ToolCommand::Info(one_name(rest, "info")?))),
        "install" => {
            let (names, flags) = split_names(rest, &["--allow-unverified"])?;
            if names.is_empty() {
                return Err(String::from("install 后面要跟至少一个包名"));
            }
            Ok(Command::Tool(ToolCommand::Install {
                names,
                allow_unverified: is_flagged(&flags, "--allow-unverified"),
            }))
        }
        "uninstall" => {
            let (names, flags) = split_names(rest, &["--purge"])?;
            if names.is_empty() {
                return Err(String::from("uninstall 后面要跟至少一个包名"));
            }
            Ok(Command::Tool(ToolCommand::Uninstall {
                names,
                purge_modified: is_flagged(&flags, "--purge"),
            }))
        }
        "update" => {
            let mut allow_unverified = false;
            let mut check = false;
            for arg in rest {
                match arg.as_str() {
                    "--allow-unverified" => allow_unverified = true,
                    "--check" => check = true,
                    other => return Err(format!("update 不认识的选项：{other}")),
                }
            }
            Ok(Command::Tool(ToolCommand::Update {
                allow_unverified,
                check,
            }))
        }
        "list" => {
            if let Some(extra) = rest.first() {
                return Err(format!("list 不接受参数：{extra}"));
            }
            Ok(Command::Tool(ToolCommand::List))
        }
        "run" => parse_run(rest).map(Command::Tool),
        "new" => parse_new(rest).map(Command::Author),
        "check" => Ok(Command::Author(AuthorCommand::Check {
            path: optional_path(rest, "check")?,
        })),
        "build" => {
            let mut stamp = false;
            let mut path: Option<PathBuf> = None;
            for arg in rest {
                match arg.as_str() {
                    "--stamp" => stamp = true,
                    other if other.starts_with('-') && other != "-" => {
                        return Err(format!("build 不认识的选项：{other}"));
                    }
                    other => {
                        if path.is_some() {
                            return Err(format!("build 只接一个目录，多出来的是：{other}"));
                        }
                        path = Some(PathBuf::from(other));
                    }
                }
            }
            Ok(Command::Author(AuthorCommand::Build { path, stamp }))
        }
        other => Err(format!("不认识的动词：{other}")),
    }
}

fn is_flagged(flags: &[String], name: &str) -> bool {
    flags.iter().any(|flag| flag == name)
}

fn parse_repo(rest: &[String]) -> Result<RepoCommand, String> {
    let Some(sub) = rest.first() else {
        return Err(String::from(
            "repo 后面要跟子命令：list / add / remove / enable / disable / update / search / path",
        ));
    };
    let tail = &rest[1..];
    match sub.as_str() {
        "list" => {
            if let Some(extra) = tail.first() {
                return Err(format!("repo list 不接受参数：{extra}"));
            }
            Ok(RepoCommand::List)
        }
        "path" => Ok(RepoCommand::Path),
        "add" => {
            let mut location: Option<String> = None;
            let mut name = None;
            let mut trust = None;
            let mut priority = None;
            let mut index = 0;
            while index < tail.len() {
                match tail[index].as_str() {
                    "--name" => name = Some(value_at(tail, &mut index, "--name")?),
                    "--trust" => trust = Some(value_at(tail, &mut index, "--trust")?),
                    "--priority" => {
                        let raw = value_at(tail, &mut index, "--priority")?;
                        priority = Some(
                            raw.parse::<i32>()
                                .map_err(|_| format!("--priority 要是个整数，收到：{raw}"))?,
                        );
                    }
                    other if other.starts_with('-') && other != "-" => {
                        return Err(format!("repo add 不认识的选项：{other}"));
                    }
                    other => {
                        if location.is_some() {
                            return Err(format!("repo add 只接一个地址，多出来的是：{other}"));
                        }
                        location = Some(other.to_string());
                    }
                }
                index += 1;
            }
            let location = location.ok_or_else(|| {
                String::from("repo add 要一个索引地址（URL、本地路径或 file://…）")
            })?;
            Ok(RepoCommand::Add {
                location,
                name,
                trust,
                priority,
            })
        }
        "remove" | "enable" | "disable" | "update" | "search" => {
            if sub == "search" {
                let (query, scope) = parse_search(tail)?;
                return Ok(RepoCommand::Search { query, scope });
            }
            let id = tail.first().cloned();
            if tail.len() > 1 && sub != "update" {
                return Err(format!("repo {sub} 只接一个仓库 id"));
            }
            match sub.as_str() {
                "remove" => Ok(RepoCommand::Remove(id.ok_or_else(missing_id)?)),
                "enable" => Ok(RepoCommand::Enable(id.ok_or_else(missing_id)?)),
                "disable" => Ok(RepoCommand::Disable(id.ok_or_else(missing_id)?)),
                _ => Ok(RepoCommand::Update(id)),
            }
        }
        other => Err(format!(
            "repo 不认识的子命令：{other}（有 list / add / remove / enable / disable / update / search / path）"
        )),
    }
}

fn missing_id() -> String {
    String::from("这里要一个仓库 id（toolbox-hub repo list 看看有哪些）")
}

fn value_at(args: &[String], index: &mut usize, flag: &str) -> Result<String, String> {
    *index += 1;
    args.get(*index)
        .filter(|value| !value.trim().is_empty())
        .cloned()
        .ok_or_else(|| format!("{flag} 后面要跟一个值"))
}

fn parse_search(rest: &[String]) -> Result<(String, SearchScope), String> {
    let mut query: Option<String> = None;
    let mut scope = SearchScope::All;
    let mut index = 0;
    while index < rest.len() {
        match rest[index].as_str() {
            "--scope" => {
                let raw = value_at(rest, &mut index, "--scope")?;
                scope = SearchScope::parse(&raw).ok_or_else(|| {
                    format!("--scope 只认 all / available / installed / upgradable，收到：{raw}")
                })?;
            }
            other if other.starts_with('-') && other != "-" => {
                return Err(format!("search 不认识的选项：{other}"));
            }
            other => {
                if query.is_some() {
                    return Err(format!(
                        "search 只接一个查询词（要多个词请加引号）：{other}"
                    ));
                }
                query = Some(other.to_string());
            }
        }
        index += 1;
    }
    Ok((query.unwrap_or_default(), scope))
}

fn one_name(rest: &[String], verb: &str) -> Result<String, String> {
    match rest {
        [] => Err(format!("{verb} 后面要跟一个名字")),
        [name] => Ok(name.clone()),
        [_, extra, ..] => Err(format!("{verb} 只接一个名字，多出来的是：{extra}")),
    }
}

/// 把参数分成「名字」与「认识的选项」两组。遇到不认识的选项直接报错。
fn split_names(rest: &[String], known: &[&str]) -> Result<(Vec<String>, Vec<String>), String> {
    let mut names = Vec::new();
    let mut flags = Vec::new();
    for arg in rest {
        if arg.starts_with('-') && arg.len() > 1 {
            if known.contains(&arg.as_str()) {
                flags.push(arg.clone());
                continue;
            }
            return Err(format!("不认识的选项：{arg}"));
        }
        names.push(arg.clone());
    }
    Ok((names, flags))
}

fn parse_run(rest: &[String]) -> Result<ToolCommand, String> {
    let mut id: Option<String> = None;
    let mut values: Vec<(String, String)> = Vec::new();
    let mut index = 0;
    while index < rest.len() {
        let arg = rest[index].clone();
        if let Some(key) = arg.strip_prefix("--") {
            if key.is_empty() {
                return Err(String::from("run 后面有个空的 --"));
            }
            // --key value 与 --key=value 两种写法都收。
            if let Some((key, value)) = key.split_once('=') {
                values.push((key.to_string(), value.to_string()));
            } else {
                let value = value_at(rest, &mut index, &arg)?;
                values.push((key.to_string(), value));
            }
            index += 1;
            continue;
        }
        if id.is_some() {
            return Err(format!("run 只接一个工具名，多出来的是：{arg}"));
        }
        id = Some(arg);
        index += 1;
    }
    Ok(ToolCommand::Run {
        id: id.ok_or_else(|| String::from("run 后面要跟一个工具名"))?,
        values,
    })
}

// ── 执行 ────────────────────────────────────────────────────────────────────

/// 跑一条命令。dry_run 只打印不动手。
pub fn run(
    command: &Command,
    service: &Service,
    bin_dir: &Path,
    dry_run: bool,
) -> Result<(), String> {
    if let Some(problem) = &service.config_problem {
        eprintln!("（仓库配置有问题，已按默认处理）：{problem}");
    }
    match command {
        Command::Repo(command) => run_repo(command, service, dry_run),
        Command::Tool(command) => run_tool(command, service, bin_dir, dry_run),
        Command::Author(command) => run_author(command, dry_run),
    }
}

/// new 的参数。
fn parse_new(rest: &[String]) -> Result<AuthorCommand, String> {
    let mut name: Option<String> = None;
    let mut dir: Option<PathBuf> = None;
    let mut kind = crate::repository::author::ScaffoldKind::Script;
    let mut description = None;
    let mut domain = None;
    let mut program = None;
    let mut index = 0;
    while index < rest.len() {
        match rest[index].as_str() {
            "--dir" => dir = Some(PathBuf::from(value_at(rest, &mut index, "--dir")?)),
            "--description" => description = Some(value_at(rest, &mut index, "--description")?),
            "--domain" => domain = Some(value_at(rest, &mut index, "--domain")?),
            "--program" => program = Some(value_at(rest, &mut index, "--program")?),
            "--kind" => {
                let raw = value_at(rest, &mut index, "--kind")?;
                kind = crate::repository::author::ScaffoldKind::parse(&raw)
                    .ok_or_else(|| format!("--kind 只认 script / recipe，收到：{raw}"))?;
            }
            other if other.starts_with('-') && other != "-" => {
                return Err(format!("new 不认识的选项：{other}"));
            }
            other => {
                if name.is_some() {
                    return Err(format!("new 只接一个名字，多出来的是：{other}"));
                }
                name = Some(other.to_string());
            }
        }
        index += 1;
    }
    let name = name.ok_or_else(|| String::from("new 后面要跟一个包名（小写字母、数字、- _ .）"))?;
    Ok(AuthorCommand::New {
        name,
        dir,
        kind,
        description,
        domain,
        program,
    })
}

fn optional_path(rest: &[String], verb: &str) -> Result<Option<PathBuf>, String> {
    match rest {
        [] => Ok(None),
        [path] if !path.starts_with('-') => Ok(Some(PathBuf::from(path))),
        [other, ..] => Err(format!("{verb} 只接一个目录，多出来的是：{other}")),
    }
}

/// 跑一条作者命令。
fn run_author(command: &AuthorCommand, dry_run: bool) -> Result<(), String> {
    match command {
        AuthorCommand::New {
            name,
            dir,
            kind,
            description,
            domain,
            program,
        } => {
            let root = dir.clone().unwrap_or_else(default_source_root);
            if dry_run {
                println!(
                    "（演练，没有写文件）会在 {} 下建一个「{name}」（{} 形状）",
                    root.display(),
                    kind.id()
                );
                return Ok(());
            }
            let written = crate::repository::author::scaffold(
                &root,
                name,
                *kind,
                description.as_deref(),
                domain.as_deref(),
                program.as_deref(),
            )?;
            let package_dir = root.join(name);
            println!("已生成 {} 个文件：", written.len());
            for path in &written {
                println!("  {}", path.strip_prefix(&root).unwrap_or(path).display());
            }
            println!(
                "\n下一步：\n  1. 改 {}（说明 / 依赖 / 域）\n  2. 改 {}/（动作与参数）\n  3. toolbox-hub check {}\n  4. toolbox-hub build .\n  5. toolbox-hub repo add <你的 index.json> 然后 toolbox-hub install {name}",
                package_dir
                    .join(crate::repository::author::TOOLBOX_TOML)
                    .display(),
                package_dir.display(),
                package_dir.display(),
            );
            Ok(())
        }
        AuthorCommand::Check { path } => {
            let target = path.clone().unwrap_or_else(|| PathBuf::from("."));
            let report = if target
                .join(crate::repository::author::TOOLBOX_TOML)
                .is_file()
            {
                println!("检查插件包：{}\n", target.display());
                crate::repository::author::check_package(&target)
            } else if target
                .join(crate::repository::author::PACKAGES_DIR)
                .is_dir()
                || target.join("index.json").is_file()
            {
                println!("检查整个仓库：{}\n", target.display());
                crate::repository::author::check_registry(&target)
            } else {
                return Err(format!(
                    "{} 里既没有 {} 也没有 {}/ —— 给个包目录或仓库根目录",
                    target.display(),
                    crate::repository::author::TOOLBOX_TOML,
                    crate::repository::author::PACKAGES_DIR
                ));
            };
            for line in &report.passed {
                println!("  ✓ {line}");
            }
            for line in &report.warnings {
                println!("  ! {line}");
            }
            for line in &report.errors {
                println!("  ✗ {line}");
            }
            println!("\n{}", report.summary());
            if report.ok() {
                Ok(())
            } else {
                Err(format!("{} 个错误要修", report.errors.len()))
            }
        }
        AuthorCommand::Build { path, stamp } => {
            let target = path.clone().unwrap_or_else(|| PathBuf::from("."));
            if dry_run {
                println!("（演练，没有写文件）会打包 {}", target.display());
                return Ok(());
            }
            if target
                .join(crate::repository::author::PACKAGES_DIR)
                .is_dir()
            {
                let result = crate::repository::author::build_registry(&target, *stamp)?;
                println!(
                    "已重建 {}（{} 个包）",
                    result.index_path.display(),
                    result.packages.len()
                );
                for id in &result.packages {
                    println!("  + {id}");
                }
                for warning in &result.warnings {
                    eprintln!("  ! {warning}");
                }
            } else {
                let artifacts = target.join(crate::repository::author::ARTIFACTS_DIR);
                let built = crate::repository::author::build_package(&target, &artifacts)?;
                println!("已打包 {} {}", built.id, built.version);
                println!("  产物    {}", built.artifact.display());
                println!("  sha256  {}", built.artifact_sha256);
                println!("  大小    {} 字节", built.artifact_size);
                for (file, digest) in &built.files {
                    println!(
                        "  {}  {}…",
                        file.relative.display(),
                        &digest[..12.min(digest.len())]
                    );
                }
                for warning in &built.warnings {
                    eprintln!("  ! {warning}");
                }
            }
            Ok(())
        }
    }
}

/// 没给 --dir 时往哪儿放：优先仓库里的 packages/，否则当前目录。
fn default_source_root() -> PathBuf {
    let packages = PathBuf::from(crate::repository::author::PACKAGES_DIR);
    if packages.is_dir() {
        packages
    } else {
        PathBuf::from(".")
    }
}

fn run_repo(command: &RepoCommand, service: &Service, dry_run: bool) -> Result<(), String> {
    match command {
        RepoCommand::Path => {
            println!("{}", service.cache_root.display());
            Ok(())
        }
        RepoCommand::List => {
            repo_list(service);
            Ok(())
        }
        RepoCommand::Add {
            location,
            name,
            trust,
            priority,
        } => {
            // 作者最自然的写法是「把当前目录加进去」（toolbox-hub repo add .）——
            // 那是目录，不是索引。这里替他把 index.json 补上，而不是报一句
            // 「Is a directory」让他猜。
            let location = &resolve_index_location(location);
            let display_name = name.clone().unwrap_or_else(|| default_name(location));
            let mut config = RepositoryConfig::user_added(&display_name, location)?;
            if let Some(raw) = trust {
                let parsed = Trust::parse(raw).ok_or_else(|| {
                    format!("--trust 只认 trusted / verified / community / unknown：{raw}")
                })?;
                config.trust = Some(parsed.id().to_string());
            }
            if let Some(priority) = priority {
                config.priority = *priority;
            }
            config.validate()?;

            let mut repositories: Repositories = service.repositories.clone();
            let id = repositories.add(config);
            let path = service.roots.config.join("repositories.toml");
            if dry_run {
                println!("（演练，没有写配置）会把仓库 {id} 写进 {}", path.display());
                return Ok(());
            }
            crate::repository::config::save_to(&path, &repositories)?;
            println!("已添加仓库「{id}」（{}）", path.display());

            // 加完立刻取一次，用户马上就能搜。取不到也只是提示，不算命令失败。
            let refreshed = Service::with_parts(
                repositories,
                service.roots.clone(),
                service.cache_root.clone(),
            )
            .refresh(&id)?;
            println!("{}", refreshed.message());
            for warning in &refreshed.warnings {
                eprintln!("  警告：{warning}");
            }
            Ok(())
        }
        RepoCommand::Remove(id) => {
            let mut repositories = service.repositories.clone();
            if !repositories.remove(id) {
                return Err(crate::repository::service::unknown_repository(
                    id,
                    &repositories,
                ));
            }
            save_or_preview(
                &repositories,
                service,
                dry_run,
                &format!("删除仓库「{id}」"),
            )
        }
        RepoCommand::Enable(id) | RepoCommand::Disable(id) => {
            let enabled = matches!(command, RepoCommand::Enable(_));
            let mut repositories = service.repositories.clone();
            if !repositories.set_enabled(id, enabled) {
                return Err(crate::repository::service::unknown_repository(
                    id,
                    &repositories,
                ));
            }
            let what = if enabled { "启用" } else { "停用" };
            save_or_preview(
                &repositories,
                service,
                dry_run,
                &format!("{what}仓库「{id}」"),
            )
        }
        RepoCommand::Search { query, scope } => {
            let hits = service.search(query, *scope);
            print_search_results(&hits, query, service);
            Ok(())
        }
        RepoCommand::Update(id) => {
            if dry_run {
                println!(
                    "（演练，没有联网）要刷新的是：{}",
                    id.clone().unwrap_or_else(|| String::from("全部已启用仓库"))
                );
                return Ok(());
            }
            let results = match id {
                Some(id) => vec![service.refresh(id)?],
                None => service.refresh_all(),
            };
            if results.is_empty() {
                println!("没有已启用的仓库（toolbox-hub repo list）");
            }
            for result in &results {
                println!("{}", result.message());
                for warning in &result.warnings {
                    eprintln!("  警告：{warning}");
                }
            }
            // 刷新失败**不算**命令失败：离线时本来就该能用缓存。
            Ok(())
        }
    }
}

fn save_or_preview(
    repositories: &Repositories,
    service: &Service,
    dry_run: bool,
    what: &str,
) -> Result<(), String> {
    let path = service.roots.config.join("repositories.toml");
    if dry_run {
        println!("（演练，没有写配置）会{what}，写进 {}", path.display());
        return Ok(());
    }
    crate::repository::config::save_to(&path, repositories)?;
    println!("已{what}（{}）", path.display());
    Ok(())
}

/// 从地址里猜一个像样的仓库名。
/// 把「指向目录」的地址补成它里面的 index.json。
///
/// 只做一件很窄的事：本地目录 + 目录里有 index.json → 用那个文件。
/// 其余（URL、file://、已经是 json 的路径）原样返回，绝不猜。
fn resolve_index_location(location: &str) -> String {
    let trimmed = location.trim();
    let without_scheme = trimmed.strip_prefix("file://").unwrap_or(trimmed);
    let path = Path::new(without_scheme);
    if !path.is_dir() {
        return trimmed.to_string();
    }
    let candidate = path.join("index.json");
    if !candidate.is_file() {
        return trimmed.to_string();
    }
    // file:// 前缀保留，免得把用户的写法改掉。
    match trimmed.strip_prefix("file://") {
        Some(_) => format!("file://{}", candidate.display()),
        None => candidate.to_string_lossy().to_string(),
    }
}

fn default_name(location: &str) -> String {
    let trimmed = location.trim_end_matches('/');
    let parts: Vec<&str> = trimmed
        .rsplit('/')
        .filter(|part| !part.is_empty())
        .collect();
    let first = parts.first().copied().unwrap_or(trimmed);
    let cleaned = strip_suffix(first);

    // index.json / toolbox.toml 是格式规定死的文件名，拿它当仓库名没有意义 ——
    // 退到上一层目录名（「/tmp/myrepo/index.json」该叫 myrepo）。
    if matches!(cleaned.as_str(), "index" | "toolbox")
        && let Some(parent) = parts.get(1)
    {
        let parent = strip_suffix(parent);
        if !parent.is_empty() {
            return parent;
        }
    }
    if cleaned.is_empty() {
        String::from("my-repository")
    } else {
        cleaned
    }
}

fn strip_suffix(segment: &str) -> String {
    segment
        .trim_end_matches(".json")
        .trim_end_matches(".toml")
        .trim_end_matches(".git")
        .to_string()
}

fn repo_list(service: &Service) {
    let statuses = service.statuses();
    if statuses.is_empty() {
        println!("还没有配置任何仓库。加一个：toolbox-hub repo add <索引地址>");
        return;
    }
    println!("ToolHub 工具仓库（和 pacman/AUR 的软件包仓库是两回事）\n");

    let id_width = statuses
        .iter()
        .map(|status| UnicodeWidthStr::width(status.config.id.as_str()))
        .max()
        .unwrap_or(2)
        .max(2);
    let name_width = statuses
        .iter()
        .map(|status| UnicodeWidthStr::width(status.config.name.as_str()))
        .max()
        .unwrap_or(4)
        .max(4);

    for status in &statuses {
        let flag = if status.config.enabled { " " } else { "×" };
        let count = if status.state.usable() {
            format!("{} 个包", status.package_count)
        } else {
            String::from("-")
        };
        println!(
            "{} {}  {}  {}  {}  {}",
            flag,
            status.state.marker(),
            pad(&status.config.id, id_width),
            pad(status.trust.label(), 4),
            pad(&status.config.name, name_width),
            count,
        );
        println!(
            "      {}  {}  {}",
            status.config.index,
            status.state.label(),
            status
                .fetched_at
                .map(|at| format!("上次刷新 {}", relative_time(at)))
                .unwrap_or_else(|| String::from("还没取过")),
        );
        if !status.config.enabled {
            println!("      （已停用：搜索与安装都不会看它）");
        }
    }
    println!("\n图例：● 在线  ◐ 缓存  ○ 未取过  ✕ 不可用  × 已停用");
}

fn run_tool(
    command: &ToolCommand,
    service: &Service,
    bin_dir: &Path,
    dry_run: bool,
) -> Result<(), String> {
    match command {
        ToolCommand::Search { query, scope } => {
            let (registry, _) = Registry::discover(bin_dir);
            let needle = query.trim().to_lowercase();
            let local: Vec<&ToolDefinition> = registry
                .tools()
                .iter()
                .filter(|tool| tool.matches(&needle))
                .collect();
            let hits = service.search(query, *scope);

            if local.is_empty() && hits.is_empty() {
                println!("没找到和「{query}」有关的工具或包");
                hint_if_no_index(service);
                return Ok(());
            }

            if !local.is_empty() {
                println!("本地工具（{} 个）", local.len());
                for tool in &local {
                    println!(
                        "  {}  {}  {}",
                        pad(&tool.name, 28),
                        pad(tool.domain.label(), 6),
                        tool.summary
                    );
                }
                println!();
            }
            print_search_results(&hits, query, service);
            Ok(())
        }
        ToolCommand::List => {
            tool_list(service);
            Ok(())
        }
        ToolCommand::Info(id) => tool_info(id, service, bin_dir),
        ToolCommand::Install {
            names,
            allow_unverified,
        } => {
            for name in names {
                tool_install(name, service, *allow_unverified, dry_run)?;
            }
            Ok(())
        }
        ToolCommand::Uninstall {
            names,
            purge_modified,
        } => {
            for name in names {
                if dry_run {
                    println!("（演练，没有卸载）{name}");
                    continue;
                }
                let report = service.uninstall(name, *purge_modified)?;
                println!("{}", report.summary());
                for (path, why) in &report.kept {
                    println!("  保留 {} —— {why}", path.display());
                }
                for warning in &report.warnings {
                    eprintln!("  警告：{warning}");
                }
            }
            Ok(())
        }
        ToolCommand::Update {
            allow_unverified,
            check,
        } => tool_update(service, *allow_unverified, *check, dry_run),
        ToolCommand::Run { id, values } => tool_run(id, values, bin_dir, dry_run),
    }
}

fn hint_if_no_index(service: &Service) {
    if service.indexes().is_empty() {
        println!("\n还没有可用的仓库索引 —— 先跑一次 toolbox-hub repo update");
    }
}

fn print_search_results(
    hits: &[crate::repository::service::PackageHit],
    query: &str,
    service: &Service,
) {
    if hits.is_empty() {
        println!("仓库里没有和「{query}」匹配的包");
        hint_if_no_index(service);
        return;
    }
    println!("仓库里的包（{} 个）", hits.len());
    let id_width = hits
        .iter()
        .map(|hit| UnicodeWidthStr::width(hit.id.as_str()))
        .max()
        .unwrap_or(2);
    for hit in hits {
        let integrity = if hit.has_hash {
            "有哈希"
        } else {
            "无哈希"
        };
        println!(
            "  {}  {}  {}  {}  {}  {}",
            pad(&hit.id, id_width),
            pad(&hit.version, 10),
            pad(hit.trust.label(), 6),
            pad(hit.status(), 10),
            pad(integrity, 6),
            hit.summary,
        );
        println!("      {} · {}", hit.repository_name, hit.state.label());
    }
}

fn tool_list(service: &Service) {
    let installed = service.installed();
    if installed.is_empty() {
        println!("还没装任何工具包（toolbox-hub search <关键词> 找找）");
        return;
    }
    println!("已安装的工具包（{} 个）\n", installed.len());
    let id_width = installed
        .iter()
        .map(|package| UnicodeWidthStr::width(package.id.as_str()))
        .max()
        .unwrap_or(2);
    for package in &installed {
        println!(
            "  {}  {}  {}  {} 个文件",
            pad(&package.id, id_width),
            pad(&package.version, 10),
            pad(package.trust().label(), 6),
            package.file_count(),
        );
        println!(
            "      来自 {} · 装于 {}",
            package.repository_name,
            relative_time(package.installed_at)
        );
    }
}

/// 找一个工具：认完整 id、名字、名字的小写形式，以及 id 的最后一段。
///
/// 最后那一条是为了好用：仓库包的 id 形如 `repository:demo-actions:demo-echo`，
/// 让人敲这么长一串没有道理，`toolbox-hub run demo-echo` 就该能跑。
fn find_tool<'a>(registry: &'a Registry, id: &str) -> Option<&'a ToolDefinition> {
    let needle = id.to_lowercase();
    let suffix = format!(":{needle}");
    let exact = |tool: &ToolDefinition| {
        tool.id == id
            || tool.name == id
            || tool.name.to_lowercase() == needle
            || tool.id.to_lowercase().ends_with(&suffix)
    };
    if let Some(tool) = registry.tools().iter().find(|tool| exact(tool)) {
        return Some(tool);
    }
    // 退一步按名字前缀找：仓库里的动作名字常常是「抽帧：按间隔导出图片」这种
    // 带后缀的写法，逼用户把整句抄一遍没有道理。有歧义就宁可找不到 ——
    // 猜错会跑错命令，那比报「找不到」糟得多。
    let unique = |mut matches: Vec<&'a ToolDefinition>| match matches.len() {
        1 => matches.pop(),
        _ => None,
    };
    let prefix: Vec<&ToolDefinition> = registry
        .tools()
        .iter()
        .filter(|tool| tool.name.to_lowercase().starts_with(&needle))
        .collect();
    if let Some(tool) = unique(prefix) {
        return Some(tool);
    }
    // 再退一步按「名字里包含」找：仓库里的名字常常是「视频抽帧成图片」这种写法，
    // 用户只会敲中间那两个关键字。有歧义就宁可找不到 —— 猜错会跑错命令。
    let anywhere: Vec<&ToolDefinition> = registry
        .tools()
        .iter()
        .filter(|tool| tool.name.to_lowercase().contains(&needle))
        .collect();
    if let Some(tool) = unique(anywhere) {
        return Some(tool);
    }
    // 最后按「包名」找：装上来的包 id 是 repository:<包>:<动作>，而用户装的时候
    // 敲的是包名，跑的时候自然会接着敲包名。包里只有一个动作就直接用它。
    match package_actions(registry, &needle).as_slice() {
        [only] => Some(only),
        _ => None,
    }
}

/// 一个包（id 形如 repository:<包>:<动作>）贡献的全部动作。
fn package_actions<'a>(registry: &'a Registry, package: &str) -> Vec<&'a ToolDefinition> {
    let needle = package.to_lowercase();
    registry
        .tools()
        .iter()
        .filter(|tool| {
            let mut parts = tool.id.split(':');
            parts.next() == Some("repository")
                && parts.next().map(str::to_lowercase).as_deref() == Some(needle.as_str())
        })
        .collect()
}

/// 找不到时给一句有用的。
///
/// 分三种情况：① 那是个包名，就把里面的动作列出来让用户挑；② 名字打错了，
/// 给「你是不是想找 X」；③ 真的没有，让他去搜。
fn tool_not_found(registry: &Registry, id: &str) -> String {
    let actions = package_actions(registry, id);
    if actions.len() > 1 {
        let lines: Vec<String> = actions
            .iter()
            .map(|tool| format!("  {}   {}", tool.id, tool.name))
            .collect();
        return format!(
            "「{id}」是一个包，里面有 {} 个动作 —— 选一个跑：\n{}",
            actions.len(),
            lines.join("\n")
        );
    }

    // 候选：工具名、动作 id 的尾段，以及标签（仓库装来的包会把包 id 放标签里）。
    let names = registry.tools().iter().flat_map(|tool| {
        std::iter::once(tool.name.as_str())
            .chain(tool.id.rsplit(':').next())
            .chain(tool.tags.iter().map(String::as_str))
    });
    match crate::repository::suggest::did_you_mean(id, names) {
        Some(hint) => format!("找不到工具「{id}」—— {hint}"),
        None => format!("找不到工具「{id}」（toolbox-hub search 找找）"),
    }
}

fn tool_info(id: &str, service: &Service, bin_dir: &Path) -> Result<(), String> {
    // 先看仓库里的包；没有就找本地工具。
    match service.find(id) {
        Some((config, meta, state)) => {
            let plan = install::plan(&meta, &config, &service.roots)?;
            print_plan(&plan, state);
            Ok(())
        }
        None => {
            let (registry, _) = Registry::discover(bin_dir);
            match find_tool(&registry, id) {
                Some(tool) => {
                    print_local_tool(tool);
                    Ok(())
                }
                None => Err(format!(
                    "{}\n（也确认一下：仓库里有没有这个包 —— toolbox-hub repo update）",
                    tool_not_found(&registry, id)
                )),
            }
        }
    }
}

fn print_local_tool(tool: &ToolDefinition) {
    println!("{}（本地工具，已经在你机器上）\n", tool.name);
    println!("  来源      {}", tool.provider);
    println!("  域        {}", tool.domain.label());
    println!("  说明      {}", tool.summary);
    println!("  路径      {}", tool.path.display());
    println!(
        "  执行方式  {}",
        match tool.mode {
            crate::model::RunMode::Interactive => "接管终端",
            crate::model::RunMode::Capture => "捕获输出",
            crate::model::RunMode::Native => "内置界面（命令行里不能跑）",
        }
    );
    println!("  危险等级  {}", tool.danger.label());
    println!("  依赖      {}", tool.deps_label());
    if let Some(action) = &tool.action {
        println!("  命令      {}", action.program);
        if !action.arguments.is_empty() {
            println!("\n  可填参数：");
            for argument in &action.arguments {
                let required = if argument.required {
                    "必填"
                } else {
                    "可选"
                };
                println!(
                    "    --{}  {}  [{}]{}",
                    pad(&argument.key, 16),
                    argument.label,
                    required,
                    argument
                        .default
                        .as_deref()
                        .map(|value| format!("  默认 {value}"))
                        .unwrap_or_default()
                );
            }
            println!("\n  例：toolbox-hub run {}", tool.name);
        }
    }
}

fn print_plan(plan: &InstallPlan, state: CacheState) {
    println!("{} {}", plan.name, plan.version);
    println!();
    match &plan.installed_version {
        Some(installed) => println!("  状态      已安装 {installed} → 这次装 {}", plan.version),
        None => println!("  状态      未安装"),
    }
    println!("  包 id     {}", plan.id);
    println!("  说明      {}", plan.summary);
    println!("  作者      {}", plan.author.as_deref().unwrap_or("-"));
    println!("  License   {}", plan.license.as_deref().unwrap_or("-"));
    println!("  来源      {}", plan.source.as_deref().unwrap_or("-"));
    println!(
        "  仓库      {}（{} · {}）",
        plan.repository_name,
        plan.trust.label(),
        plan.trust.meaning()
    );
    println!("  索引      {}（{}）", state.label(), state.meaning());
    println!(
        "  完整性    {}",
        match plan
            .artifact
            .as_ref()
            .and_then(|artifact| artifact.sha256())
        {
            Some(hash) => format!("声明了 SHA-256（{}…）", &hash[..8.min(hash.len())]),
            None => String::from("没有声明哈希 —— 安装前会要求你显式确认"),
        }
    );
    println!(
        "  危险等级  {}{}",
        plan.danger.label(),
        if plan.requires_root {
            "（需要写系统目录：工具箱不会替你提权，会拒绝安装）"
        } else {
            ""
        }
    );
    if plan.has_executables() {
        println!("  ⚠ 包含可执行脚本");
    }

    println!("\n  依赖：");
    if plan.dependencies.is_empty() {
        println!("    无");
    } else {
        for dependency in &plan.dependencies {
            let mark = if plan.missing_dependencies.contains(dependency) {
                "✗"
            } else {
                "✓"
            };
            let hint = plan
                .dep_hints
                .iter()
                .find(|(command, _)| command == dependency)
                .map(|(_, hint)| format!("   装它：{hint}"))
                .unwrap_or_default();
            println!("    {mark} {dependency}{hint}");
        }
    }

    println!("\n  文件：");
    if plan.files.is_empty() {
        println!("    （索引没有列出文件）");
    } else {
        for file in &plan.files {
            println!(
                "    {} → {}  [{}]",
                file.source,
                file.target.display(),
                file.kind.label()
            );
            if let Some(conflict) = &file.conflict {
                println!("      ⚠ {conflict}");
            }
        }
    }
    for warning in &plan.warnings {
        println!("  ⚠ {warning}");
    }
}

fn tool_install(
    id: &str,
    service: &Service,
    allow_unverified: bool,
    dry_run: bool,
) -> Result<(), String> {
    let plan = service.plan(id)?;
    let state = service
        .find(id)
        .map(|(_, _, state)| state)
        .unwrap_or(CacheState::Fresh);
    print_plan(&plan, state);

    plan.check(&service.roots, allow_unverified)?;
    if dry_run {
        println!("\n（演练，没有安装）");
        return Ok(());
    }
    println!();
    let report = service.install(&plan, allow_unverified)?;
    println!(
        "已安装 {} {}（{}）",
        report.id,
        report.version,
        report.integrity.label()
    );
    for file in &report.files {
        println!("  + {}", file.path.display());
    }
    for warning in &report.warnings {
        eprintln!("  警告：{warning}");
    }
    Ok(())
}

/// 有更新可用时的退出码（和 checkupdates 一路：0 = 无事可做，10 = 有更新）。
pub const UPDATE_AVAILABLE_CODE: i32 = 10;

fn tool_update(
    service: &Service,
    allow_unverified: bool,
    check: bool,
    dry_run: bool,
) -> Result<(), String> {
    let (candidates, warnings) = service.update_candidates();
    for warning in &warnings {
        eprintln!("警告：{warning}");
    }
    if candidates.is_empty() {
        if service.indexes().is_empty() {
            println!("还没有可用的仓库索引 —— 先跑一次 toolbox-hub repo update");
        } else {
            println!("已安装的包都是最新的");
        }
        return Ok(());
    }

    println!("{} 个包可升级：\n", candidates.len());
    for candidate in &candidates {
        println!(
            "  {}  {} → {}  （{}）",
            pad(&candidate.id, 24),
            candidate.current_version,
            candidate.available_version,
            candidate.repository_name
        );
    }
    if check {
        // 只报告、不动手：给脚本与 systemd timer 用（10 = 有更新可升）。
        // 这里就地退出是因为 0/1 已经分别表示「没事」和「失败」，脚本需要第三个答案。
        std::process::exit(UPDATE_AVAILABLE_CODE);
    }
    if dry_run {
        println!("\n（演练，没有升级）");
        return Ok(());
    }

    println!();
    let mut failures = 0;
    for candidate in &candidates {
        match service.install(&candidate.plan, allow_unverified) {
            Ok(report) => println!(
                "已升级 {} {} → {}",
                report.id, candidate.current_version, report.version
            ),
            Err(problem) => {
                failures += 1;
                eprintln!("升级 {} 失败：{problem}", candidate.id);
            }
        }
    }
    if failures > 0 {
        return Err(format!("{failures} 个包升级失败"));
    }
    Ok(())
}

fn tool_run(
    id: &str,
    overrides: &[(String, String)],
    bin_dir: &Path,
    dry_run: bool,
) -> Result<(), String> {
    let (registry, _) = Registry::discover(bin_dir);
    let tool = find_tool(&registry, id).ok_or_else(|| tool_not_found(&registry, id))?;

    let Some(action) = &tool.action else {
        // 没有动作 = 它本身就是个可执行文件，直接跑。
        if !overrides.is_empty() {
            return Err(format!(
                "{} 不接受参数（它没有声明任何可填字段）",
                tool.name
            ));
        }
        return execute(&tool.path.to_string_lossy(), &[], dry_run);
    };

    if matches!(tool.mode, crate::model::RunMode::Native) {
        return Err(format!("{} 是个内置界面，不能在命令行里跑", tool.name));
    }

    // 先按默认值铺一遍，再用用户给的覆盖。
    let mut values = action.default_values();
    for (key, value) in overrides {
        if !action.arguments.iter().any(|argument| argument.key == *key) {
            let valid: Vec<&str> = action
                .arguments
                .iter()
                .map(|argument| argument.key.as_str())
                .collect();
            return Err(format!(
                "「{key}」不是 {} 的参数；可用的是：{}",
                tool.name,
                valid.join(", ")
            ));
        }
        values.set(key, value.clone());
    }

    let argv = action.build_argv(&values)?;
    execute(&action.program, &argv, dry_run)
}

/// 执行：**永远传 argv，不经过 shell**。
fn execute(program: &str, argv: &[String], dry_run: bool) -> Result<(), String> {
    let preview = preview(program, argv);
    if dry_run {
        println!("（演练，没有执行）{preview}");
        return Ok(());
    }
    println!("$ {preview}");
    let _ = std::io::stdout().flush();

    let status = Process::new(program).args(argv).status().map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            format!("系统里没有 {program}")
        } else {
            format!("{program} 跑不起来：{error}")
        }
    })?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("{program} 退出码 {:?}", status.code()))
    }
}

/// 只用于**显示**的命令预览。
///
/// 带引号是给人看的；执行时仍然是一个个 argv 元素，永远不拼成 shell 字符串。
fn preview(program: &str, argv: &[String]) -> String {
    let mut parts = vec![quote(program)];
    parts.extend(argv.iter().map(|item| quote(item)));
    parts.join(" ")
}

fn quote(value: &str) -> String {
    if value.is_empty() {
        return String::from("''");
    }
    let risky = value.chars().any(|ch| {
        ch.is_whitespace()
            || matches!(
                ch,
                '\'' | '"' | '$' | ';' | '|' | '&' | '<' | '>' | '(' | ')'
            )
    });
    if !risky {
        return value.to_string();
    }
    // 含单引号时改用双引号包 —— 这只是一个给人看的预览，不参与执行。
    if value.contains('\'') {
        format!("\"{value}\"")
    } else {
        format!("'{value}'")
    }
}

/// 按显示宽度补空格（中文是两格宽）。
fn pad(text: &str, width: usize) -> String {
    let actual = UnicodeWidthStr::width(text);
    if actual >= width {
        return text.to_string();
    }
    format!("{text}{}", " ".repeat(width - actual))
}

/// 相对时间（不引日期库）。
pub fn relative_time(epoch: u64) -> String {
    let now = crate::repository::cache::now_secs();
    if epoch == 0 || epoch > now {
        return String::from("刚刚");
    }
    let delta = now - epoch;
    if delta < 60 {
        String::from("刚刚")
    } else if delta < 3600 {
        format!("{} 分钟前", delta / 60)
    } else if delta < 86_400 {
        format!("{} 小时前", delta / 3600)
    } else {
        format!("{} 天前", delta / 86_400)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|item| (*item).to_string()).collect()
    }

    #[test]
    fn only_the_known_words_are_verbs() {
        assert!(is_verb("repo"));
        assert!(is_verb("install"));
        assert!(!is_verb("/tmp/scripts"));
        assert!(!is_verb(""));
    }

    #[test]
    fn repo_subcommands_parse() {
        assert_eq!(
            parse("repo", &args(&["list"])),
            Ok(Command::Repo(RepoCommand::List))
        );
        assert_eq!(
            parse("repo", &args(&["add", "https://x/index.json"])),
            Ok(Command::Repo(RepoCommand::Add {
                location: String::from("https://x/index.json"),
                name: None,
                trust: None,
                priority: None
            }))
        );
        assert_eq!(
            parse(
                "repo",
                &args(&[
                    "add",
                    "/tmp/r",
                    "--name",
                    "我的",
                    "--trust",
                    "community",
                    "--priority",
                    "3"
                ])
            ),
            Ok(Command::Repo(RepoCommand::Add {
                location: String::from("/tmp/r"),
                name: Some(String::from("我的")),
                trust: Some(String::from("community")),
                priority: Some(3)
            }))
        );
        assert_eq!(
            parse("repo", &args(&["remove", "x"])),
            Ok(Command::Repo(RepoCommand::Remove(String::from("x"))))
        );
        assert_eq!(
            parse("repo", &args(&["update"])),
            Ok(Command::Repo(RepoCommand::Update(None)))
        );
        assert_eq!(
            parse("repo", &args(&["update", "official"])),
            Ok(Command::Repo(RepoCommand::Update(Some(String::from(
                "official"
            )))))
        );
    }

    #[test]
    fn bad_repo_arguments_are_explained() {
        assert!(parse("repo", &[]).unwrap_err().contains("子命令"));
        assert!(
            parse("repo", &args(&["nope"]))
                .unwrap_err()
                .contains("不认识的子命令")
        );
        assert!(
            parse("repo", &args(&["remove"]))
                .unwrap_err()
                .contains("仓库 id")
        );
        assert!(
            parse("repo", &args(&["add"]))
                .unwrap_err()
                .contains("索引地址")
        );
        assert!(
            parse("repo", &args(&["add", "x", "--priority", "abc"]))
                .unwrap_err()
                .contains("整数")
        );
        assert!(
            parse("repo", &args(&["add", "x", "--nope"]))
                .unwrap_err()
                .contains("不认识的选项")
        );
    }

    #[test]
    fn search_parses_a_query_and_a_scope() {
        assert_eq!(
            parse("search", &args(&["ffmpeg"])),
            Ok(Command::Tool(ToolCommand::Search {
                query: String::from("ffmpeg"),
                scope: SearchScope::All
            }))
        );
        assert_eq!(
            parse("search", &args(&["--scope", "upgradable", "x"])),
            Ok(Command::Tool(ToolCommand::Search {
                query: String::from("x"),
                scope: SearchScope::Upgradable
            }))
        );
        // 空查询 = 列全部
        assert_eq!(
            parse("search", &[]),
            Ok(Command::Tool(ToolCommand::Search {
                query: String::new(),
                scope: SearchScope::All
            }))
        );
        assert!(parse("search", &args(&["--scope", "乱写"])).is_err());
        assert!(parse("search", &args(&["a", "b"])).is_err());
    }

    #[test]
    fn install_and_uninstall_collect_names_and_flags() {
        assert_eq!(
            parse("install", &args(&["a", "b"])),
            Ok(Command::Tool(ToolCommand::Install {
                names: args(&["a", "b"]),
                allow_unverified: false
            }))
        );
        assert_eq!(
            parse("install", &args(&["a", "--allow-unverified"])),
            Ok(Command::Tool(ToolCommand::Install {
                names: args(&["a"]),
                allow_unverified: true
            }))
        );
        assert!(parse("install", &[]).unwrap_err().contains("包名"));
        assert!(parse("install", &args(&["a", "--nope"])).is_err());

        assert_eq!(
            parse("uninstall", &args(&["a", "--purge"])),
            Ok(Command::Tool(ToolCommand::Uninstall {
                names: args(&["a"]),
                purge_modified: true
            }))
        );
    }

    #[test]
    fn run_collects_argument_values_in_both_writings() {
        assert_eq!(
            parse(
                "run",
                &args(&["ffmpeg-compress", "--input", "a.mkv", "--crf", "20"])
            ),
            Ok(Command::Tool(ToolCommand::Run {
                id: String::from("ffmpeg-compress"),
                values: vec![
                    (String::from("input"), String::from("a.mkv")),
                    (String::from("crf"), String::from("20")),
                ]
            }))
        );
        assert_eq!(
            parse("run", &args(&["x", "--a=1"])),
            Ok(Command::Tool(ToolCommand::Run {
                id: String::from("x"),
                values: vec![(String::from("a"), String::from("1"))]
            }))
        );
        assert!(
            parse("run", &args(&["x", "--a"]))
                .unwrap_err()
                .contains("要跟一个值")
        );
        assert!(parse("run", &[]).unwrap_err().contains("工具名"));
        assert!(
            parse("run", &args(&["a", "b"]))
                .unwrap_err()
                .contains("只接一个")
        );
    }

    #[test]
    fn info_list_and_update_are_strict_about_extras() {
        assert_eq!(
            parse("info", &args(&["x"])),
            Ok(Command::Tool(ToolCommand::Info(String::from("x"))))
        );
        assert!(parse("info", &args(&["x", "y"])).is_err());
        assert!(parse("info", &[]).is_err());
        assert_eq!(parse("list", &[]), Ok(Command::Tool(ToolCommand::List)));
        assert!(parse("list", &args(&["x"])).is_err());
        assert_eq!(
            parse("update", &[]),
            Ok(Command::Tool(ToolCommand::Update {
                allow_unverified: false,
                check: false
            }))
        );
        assert!(parse("update", &args(&["--nope"])).is_err());
    }

    /// 命令预览只是给人看的：带空格/元字符的参数会被引起来，一眼看出是一个参数。
    #[test]
    fn the_preview_quotes_arguments_with_spaces() {
        let argv = args(&["--input", "两个 空格.mkv", "x;rm -rf /"]);
        let text = preview("ffmpeg", &argv);
        assert!(text.contains("'两个 空格.mkv'"), "{text}");
        assert!(text.contains("'x;rm -rf /'"), "{text}");
        assert_eq!(quote("plain"), "plain");
        assert_eq!(quote(""), "''");
        assert_eq!(quote("it's"), "\"it's\"");
    }

    #[test]
    fn padding_uses_display_width_not_byte_length() {
        // 中文是 4 个显示格、6 个字节
        assert_eq!(pad("中文", 6), "中文  ");
        assert_eq!(pad("ab", 6), "ab    ");
        assert_eq!(pad("abcdefgh", 6), "abcdefgh");
    }

    #[test]
    fn relative_time_reads_naturally_without_a_date_library() {
        let now = crate::repository::cache::now_secs();
        assert_eq!(relative_time(now), "刚刚");
        assert_eq!(relative_time(now - 120), "2 分钟前");
        assert_eq!(relative_time(now - 7200), "2 小时前");
        assert_eq!(relative_time(now - 3 * 86_400), "3 天前");
        assert_eq!(relative_time(0), "刚刚");
    }

    #[test]
    fn a_default_name_is_guessed_from_the_location() {
        assert_eq!(default_name("https://x/y/tools"), "tools");
        assert_eq!(default_name("https://x/y/tools/"), "tools");
        assert_eq!(default_name("/tmp/myrepo/index.json"), "myrepo");
        assert_eq!(default_name("https://github.com/u/r.git"), "r");
    }
}
