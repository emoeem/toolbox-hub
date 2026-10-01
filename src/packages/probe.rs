//! 取数层里**需要外部进程 / 网络**的那部分：AUR RPC、Arch 新闻、浏览器。
//!
//! 官方源那边（搜索、包信息、已安装、可更新）已经全部交给 [`crate::packages::libalpm`]
//! —— 那是同一份数据的权威来源，还不用起进程。这里剩下的都是 libalpm 管不到的：
//!
//! * AUR 没有本地数据库，只能问官方 RPC；
//! * Arch 新闻是 RSS；
//! * 「浏览器打开 AUR 页面」要外部程序。
//!
//! 每个函数都可以被后台线程直接调用（同步、返回结果）。

use std::{collections::HashMap, process::Command};

use super::{
    InstalledState, NewsItem, PackageHit, aur_info_url, aur_search_url, libalpm, parse_aur_info,
    parse_aur_search, parse_news_rss,
};

/// 一次搜索的结果（官方源 + AUR 合并）。
#[derive(Clone, Debug, Default)]
pub struct SearchOutcome {
    pub hits: Vec<PackageHit>,
    /// 哪一路失败了（官方源 / AUR 各自独立，坏了一路不影响另一路）。
    pub errors: Vec<String>,
}

/// 包信息的结果。
#[derive(Clone, Debug)]
pub struct InfoOutcome {
    pub name: String,
    pub fields: Vec<(String, String)>,
    /// 出错时给一句人话。
    pub error: Option<String>,
}

/// 跑一条命令，拿 stdout（失败时把 stderr 的第一行带回来当错误）。
fn run(program: &str, args: &[&str]) -> Result<String, String> {
    let output = Command::new(program)
        .args(args)
        .output()
        .map_err(|error| format!("{program} 跑不起来：{error}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let first = stderr.lines().next().unwrap_or("（没有错误信息）");
        return Err(format!(
            "{program} 退出码 {:?}：{first}",
            output.status.code()
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

// ── AUR ─────────────────────────────────────────────────────────────────────

/// AUR 搜索（官方 RPC）。
pub fn aur_search(term: &str) -> Result<Vec<PackageHit>, String> {
    let url = aur_search_url(term);
    let text = run(
        "curl",
        &["-sS", "--max-time", "20", "-A", "toolbox-hub", &url],
    )?;
    parse_aur_search(&text)
}

/// 从 AUR RPC 的 `info` 里拿包信息（字段整理成和官方源同一套面板）。
pub fn aur_info(name: &str) -> Result<Vec<(String, String)>, String> {
    let url = aur_info_url(name);
    let text = run(
        "curl",
        &["-sS", "--max-time", "20", "-A", "toolbox-hub", &url],
    )?;
    parse_aur_info(&text)
}

// ── 搜索：两路合流 ──────────────────────────────────────────────────────────

/// 搜索：官方源（libalpm）+ AUR（RPC），合并后排序。
pub fn search(term: &str) -> SearchOutcome {
    let mut outcome = SearchOutcome::default();

    // 本地已装的名字 → 版本：AUR 的命中也要能标「已安装 / 可升级」。
    let installed = installed_versions();

    match libalpm::search(term) {
        Ok(hits) => outcome.hits.extend(hits),
        Err(error) => outcome.errors.push(format!("官方源：{error}")),
    }
    match aur_search(term) {
        Ok(hits) => outcome.hits.extend(hits.into_iter().map(|mut hit| {
            hit.installed_state = state_for(&installed, &hit.name, &hit.version);
            hit
        })),
        Err(error) => outcome.errors.push(format!("AUR：{error}")),
    }

    // 排序：完全同名的排最前（搜 `fzf` 时你多半就是要 fzf），然后已安装的，
    // 然后 AUR 按得票、官方源按仓库顺序。
    let needle = term.trim().trim_start_matches('^').trim_end_matches('$');
    outcome.hits.sort_by(|a, b| {
        let exact = |hit: &PackageHit| hit.name.eq_ignore_ascii_case(needle) as u8;
        b.is_installed()
            .cmp(&a.is_installed())
            .then(exact(b).cmp(&exact(a)))
            .then(b.votes.unwrap_or(0).cmp(&a.votes.unwrap_or(0)))
            .then(a.repo.cmp(&b.repo))
            .then(a.name.cmp(&b.name))
    });
    outcome
        .hits
        .dedup_by(|a, b| a.name == b.name && a.version == b.version);
    outcome
}

/// 本地已装包的 名字 → 版本（一次读全，给 AUR 命中标状态用）。
fn installed_versions() -> HashMap<String, String> {
    libalpm::installed()
        .map(|packages| {
            packages
                .into_iter()
                .map(|package| (package.name, package.version))
                .collect()
        })
        .unwrap_or_default()
}

/// 拿本地版本和「正在看的版本」比一下（比较交给 libalpm 的 vercmp）。
fn state_for(installed: &HashMap<String, String>, name: &str, version: &str) -> InstalledState {
    match installed.get(name) {
        Some(local) => {
            InstalledState::from_ordering(::alpm::vercmp(version, local.as_str()), local.clone())
        }
        None => InstalledState::NotInstalled,
    }
}

/// 包信息：AUR 走 RPC，其余（官方源 / 本地外来包）走 libalpm。
pub fn info(hit: &PackageHit) -> InfoOutcome {
    let result = if hit.is_aur() {
        aur_info(&hit.name)
    } else {
        libalpm::info(&hit.name)
    };
    match result {
        Ok(fields) => InfoOutcome {
            name: hit.name.clone(),
            fields,
            error: None,
        },
        Err(error) => InfoOutcome {
            name: hit.name.clone(),
            fields: Vec::new(),
            error: Some(error),
        },
    }
}

/// 已安装浏览器的数据（libalpm 一次算完，含外来与孤儿）。
pub fn installed_packages() -> Result<Vec<super::InstalledPackage>, String> {
    libalpm::installed()
}

/// 孤儿包名单（没人依赖、你也没点名装的）。
pub fn orphan_names() -> Vec<String> {
    libalpm::orphan_names()
}

/// 有多少个包可以更新（libalpm 算，`checkupdates` 那个 18 秒的进程不需要了）。
pub fn pending_updates() -> Option<usize> {
    libalpm::pending_updates()
}

/// 本地同步库有多旧（「待更新」是按它算的，旧了要说明）。
pub fn sync_db_age() -> Option<std::time::Duration> {
    libalpm::sync_db_age()
}

/// 卸载影响：谁依赖这些包（libalpm 的依赖索引，秒回）。
pub fn removal_report(names: &[String]) -> Vec<String> {
    libalpm::removal_report(names)
}

/// 队列里这些包一共要下载多少字节（同步库里查得到的才算）。
pub fn download_total(names: &[String]) -> Option<u64> {
    libalpm::download_total(names)
}

// ── Arch 新闻 ───────────────────────────────────────────────────────────────

/// 抓 Arch 官方新闻（RSS）。
pub fn news() -> Result<Vec<NewsItem>, String> {
    let text = run(
        "curl",
        &[
            "-sS",
            "--max-time",
            "20",
            "-A",
            "toolbox-hub",
            "https://archlinux.org/feeds/news/",
        ],
    )?;
    let items = parse_news_rss(&text);
    if items.is_empty() {
        return Err(String::from("新闻源里没有条目（格式变了吗）"));
    }
    Ok(items)
}

/// 最后一次全系统更新的时间（读 pacman 日志；读不到就 `None`）。
pub fn last_upgrade() -> Option<u64> {
    let log = std::fs::read_to_string("/var/log/pacman.log").ok()?;
    super::last_upgrade_epoch(&log)
}

// ── 杂项 ────────────────────────────────────────────────────────────────────

/// 在浏览器里打开一个地址（评论、投票都在 AUR 页面上，那里需要你的登录）。
pub fn open_in_browser(url: &str) -> Result<(), String> {
    let opener = if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    Command::new(opener)
        .arg(url)
        .spawn()
        .map(|_| ())
        .map_err(|error| format!("打不开浏览器（{opener}）：{error}"))
}

/// 扫 `PATH` 判断命令在不在。
///
/// 不走 `sh -c 'command -v …'`：后者每次要 spawn 一个 shell（约 20ms），而这里
/// 只是给「更新 / 清缓存」挑条路，不值得多花那个钱
/// （`providers::metadata` 里对这个坑有更详细的记录）。
fn on_path(name: &str) -> bool {
    std::env::var_os("PATH")
        .is_some_and(|paths| std::env::split_paths(&paths).any(|dir| dir.join(name).is_file()))
}

/// `paru` 在不在（系统更新走 `paru -Syu` 才能把 AUR 包一起升上去）。
pub fn has_paru() -> bool {
    on_path("paru")
}

/// `paccache` 在不在（`pacman-contrib` 带的缓存清理工具）。
pub fn has_paccache() -> bool {
    on_path("paccache")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 真跑：官方源（libalpm）+ AUR（RPC）两路搜索。
    #[test]
    #[ignore = "真的读 pacman 数据库并联网（只读），默认跳过"]
    fn smoke_search_both_sources() {
        let outcome = search("fzf");

        if let Some(error) = outcome
            .errors
            .iter()
            .find(|error| error.starts_with("官方源"))
        {
            panic!("官方源搜索失败：{error}");
        }
        assert!(
            outcome.hits.iter().any(|hit| hit.name == "fzf"),
            "应该能搜到 fzf：{:?}",
            outcome.hits.iter().map(|hit| &hit.name).collect::<Vec<_>>()
        );
        assert!(
            outcome
                .hits
                .iter()
                .any(|hit| hit.name == "fzf" && hit.is_installed()),
            "fzf 装了，应该标上已安装"
        );

        // 同一个包不该因为出现在多个仓库里而重复（仓库优先级去重）
        let mut names: Vec<&str> = outcome.hits.iter().map(|hit| hit.name.as_str()).collect();
        let before = names.len();
        names.sort_unstable();
        names.dedup();
        assert_eq!(before, names.len(), "多仓库同名的包要去重");
    }

    /// 真跑：libalpm 与真 pacman 的两个数字必须一致。
    ///
    /// 对账用的是 `pacman -Qu` 而**不是** `checkupdates`：后者每次会重新下载数据库
    /// （就是那 18 秒），于是它能看到「本地库还不知道的更新」。实测差距就是这么来的：
    /// checkupdates 30 / `pacman -Qu` 27 / libalpm 27。想看新数字就先同步库，
    /// 或者干脆让 `-Syu` 自己去同步 —— 界面上也会标出「库多久没同步了」。
    #[test]
    #[ignore = "真的跑 pacman -Qu 与 pacman -Qtdq（只读），默认跳过"]
    fn smoke_libalpm_agrees_with_pacman() {
        let output = Command::new("pacman")
            .args(["-Qu"])
            .output()
            .expect("跑 pacman -Qu");
        let expected = String::from_utf8_lossy(&output.stdout)
            .lines()
            .filter(|line| !line.trim().is_empty())
            .count();
        let ours = pending_updates().expect("libalpm 应该算得出来");
        assert_eq!(
            ours,
            expected,
            "libalpm 与 pacman -Qu 对不上（差 {}）",
            ours as i64 - expected as i64
        );

        // 孤儿：`pacman -Qtdq` 是权威，自己算的必须一致
        let output = Command::new("pacman")
            .args(["-Qtdq"])
            .output()
            .expect("跑 pacman -Qtdq");
        let expected = String::from_utf8_lossy(&output.stdout)
            .lines()
            .filter(|line| !line.trim().is_empty())
            .count();
        let ours = installed_packages()
            .expect("libalpm 应该读得出来")
            .iter()
            .filter(|package| package.orphan)
            .count();
        assert_eq!(ours, expected, "孤儿判定与 pacman -Qtd 对不上");
    }

    /// 真跑：官方源包信息（libalpm）与 AUR 包信息（RPC）。
    #[test]
    #[ignore = "真的读 pacman 数据库并联网（只读），默认跳过"]
    fn smoke_info_both_sources() {
        let official = libalpm::info("bash").expect("bash 的信息");
        assert!(
            official.iter().any(|(key, _)| key == "Depends On"),
            "信息面板该有 Depends On: {official:?}"
        );
        assert!(
            official.iter().any(|(key, _)| key == "Download Size"),
            "体积也该有（这是 paru 那套面板的重点）"
        );

        if let Err(error) = aur_info("pacsea-bin") {
            println!("AUR 那一路失败（网络问题可接受）：{error}");
        }
    }

    /// 真跑：Arch 新闻。
    #[test]
    #[ignore = "真的执行 curl（只读），默认跳过"]
    fn smoke_news() {
        match news() {
            Ok(items) => {
                assert!(!items.is_empty());
                println!("最新新闻：{}", items[0].title);
            }
            Err(error) => println!("抓新闻失败（网络问题可接受）：{error}"),
        }
    }
}
