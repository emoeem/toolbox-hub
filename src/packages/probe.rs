//! 包管理的**取数**层：pacman / paru / curl 的只读查询。
//!
//! 为什么在这里而不是 `runtime`：这些都是「给界面看的查询」，和「执行工具」
//! 是两回事；但**改系统的那一下**（`paru -S`）仍然走 `runtime` 的终端接管，
//! 不在这里偷偷执行。
//!
//! 每个函数都可以被后台线程直接调用（同步、返回结果）。

use std::{collections::BTreeSet, process::Command};

use super::{
    NewsItem, PackageHit, aur_info_url, aur_search_url, parse_aur_info, parse_aur_search,
    parse_info, parse_installed, parse_news_rss, parse_official_search,
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
///
/// 一律 `LC_ALL=C`：pacman 的标记是**本地化**的（中文系统上是 `[已安装]`），
/// 不锁 locale 就解析不了。
fn run(program: &str, args: &[&str]) -> Result<String, String> {
    run_ok_codes(program, args, &[0])
}

/// 同上，但可以声明「哪些退出码不算失败」。
///
/// 真实存在的例子：`pacman -Ss 没这个词` 退出码是 **1**（没有匹配），
/// 那不是错误 —— 实拍时它被当成「官方源失败」报了出来。
fn run_ok_codes(program: &str, args: &[&str], ok_codes: &[i32]) -> Result<String, String> {
    let output = Command::new(program)
        .args(args)
        .env("LC_ALL", "C")
        .output()
        .map_err(|error| format!("{program} 跑不起来：{error}"))?;

    let code = output.status.code();
    if !output.status.success() && !code.is_some_and(|code| ok_codes.contains(&code)) {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let first = stderr.lines().next().unwrap_or("（没有错误信息）");
        return Err(format!("{program} 退出码 {code:?}：{first}"));
    }
    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

/// `pacman -Qq`：一次拿全已安装的包名（搜索结果与 AUR 命中都靠它标「已安装」）。
pub fn installed_names() -> BTreeSet<String> {
    run("pacman", &["-Qq"])
        .map(|text| parse_installed(&text))
        .unwrap_or_default()
}

/// 官方源搜索（`pacman -Ss`）。
pub fn official_search(term: &str) -> Result<Vec<PackageHit>, String> {
    // 1 = 没有匹配（不是失败）
    run_ok_codes("pacman", &["-Ss", term], &[0, 1]).map(|text| parse_official_search(&text))
}

/// AUR 搜索（官方 RPC，走 curl；比解析 paru 的文本可靠得多）。
///
/// `curl` 缺失或超时都只是「这一路没有结果」，不影响官方源那一路。
pub fn aur_search(term: &str) -> Result<Vec<PackageHit>, String> {
    let url = aur_search_url(term);
    let text = run(
        "curl",
        &["-sS", "--max-time", "20", "-A", "toolbox-hub", &url],
    )?;
    parse_aur_search(&text)
}

/// 搜索：官方源 + AUR 各取一路，合并后按「已安装 → 相关性」排一下。
pub fn search(term: &str) -> SearchOutcome {
    let installed = installed_names();
    let mut outcome = SearchOutcome::default();

    match official_search(term) {
        Ok(hits) => outcome.hits.extend(hits),
        Err(error) => outcome.errors.push(format!("官方源：{error}")),
    }
    match aur_search(term) {
        Ok(hits) => outcome.hits.extend(hits),
        Err(error) => outcome.errors.push(format!("AUR：{error}")),
    }

    for hit in outcome.hits.iter_mut() {
        hit.installed = installed.contains(&hit.name);
    }

    // 排序：完全同名的排最前（搜索 `fzf` 时你多半就是要 fzf），然后已安装的，
    // 然后 AUR 按得票、官方源按仓库顺序。
    outcome.hits.sort_by(|a, b| {
        let exact = |hit: &PackageHit| hit.name.eq_ignore_ascii_case(term.trim()) as u8;
        b.installed
            .cmp(&a.installed)
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

/// 包信息：官方源用 `pacman -Sii`，AUR 用 RPC（字段整理成同一套面板）。
pub fn info(hit: &PackageHit) -> InfoOutcome {
    if hit.is_aur() {
        let url = aur_info_url(&hit.name);
        return match run(
            "curl",
            &["-sS", "--max-time", "20", "-A", "toolbox-hub", &url],
        ) {
            Ok(text) => match parse_aur_info(&text) {
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
            },
            Err(error) => InfoOutcome {
                name: hit.name.clone(),
                fields: Vec::new(),
                error: Some(error),
            },
        };
    }

    match run("pacman", &["-Sii", &hit.name]) {
        Ok(text) => InfoOutcome {
            name: hit.name.clone(),
            fields: parse_info(&text),
            error: None,
        },
        Err(error) => InfoOutcome {
            name: hit.name.clone(),
            fields: Vec::new(),
            error: Some(error),
        },
    }
}

/// 取 Arch 官方新闻（RSS）。
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

/// 有多少个包可以更新（`checkupdates`，只查不装）。
pub fn pending_updates() -> Option<usize> {
    // checkupdates 没有更新时退 2，`run` 会当成错误 —— 那种情况就是 0 个
    run("checkupdates", &[])
        .map(|text| text.lines().filter(|line| !line.trim().is_empty()).count())
        .ok()
        .or(Some(0))
}

/// 最后一次全系统更新的时间（读 pacman 日志；读不到就 `None`）。
pub fn last_upgrade() -> Option<u64> {
    let log = std::fs::read_to_string("/var/log/pacman.log").ok()?;
    super::last_upgrade_epoch(&log)
}

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

#[cfg(test)]
mod tests {
    use super::*;

    /// 真跑：官方源搜索 + AUR 搜索（只读，不碰系统）。
    #[test]
    #[ignore = "真的执行 pacman -Ss 与 curl（只读），默认跳过"]
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
                .any(|hit| hit.name == "fzf" && hit.installed),
            "fzf 装了，应该标上已安装"
        );
        // AUR 那一路可能因为网络失败，但错误要说清楚是哪一路
        for error in &outcome.errors {
            println!("（一路失败，可接受）：{error}");
        }
    }

    /// 真跑：搜一个不存在的词，两路都该是「0 个结果」而不是「失败」。
    ///
    /// `pacman -Ss 没有这个词` 退出码是 1 —— 这不是错误（实拍踩到过）。
    #[test]
    #[ignore = "真的执行 pacman -Ss 与 curl（只读），默认跳过"]
    fn smoke_no_match_is_not_an_error() {
        let outcome = search("toolbox-hub-definitely-nonexistent-xyz");

        assert!(
            !outcome
                .errors
                .iter()
                .any(|error| error.starts_with("官方源")),
            "没匹配不该报成失败：{:?}",
            outcome.errors
        );
        assert!(
            outcome.hits.len() < 5,
            "这个词应该几乎没有结果：{}",
            outcome.hits.len()
        );
    }

    /// 真跑：官方源包信息 + AUR 包信息。
    #[test]
    #[ignore = "真的执行 pacman -Sii 与 curl（只读），默认跳过"]
    fn smoke_info_both_sources() {
        let official = info(&PackageHit {
            repo: String::from("core"),
            name: String::from("bash"),
            version: String::from("0"),
            description: String::new(),
            installed: true,
            installed_version: None,
            votes: None,
            popularity: None,
            maintainer: None,
            out_of_date: false,
        });
        assert!(official.error.is_none(), "{:?}", official.error);
        assert!(
            official.fields.iter().any(|(key, _)| key == "Depends On"),
            "信息面板该有 Depends On: {:?}",
            official.fields
        );

        let aur = info(&PackageHit {
            repo: String::from("aur"),
            name: String::from("pacsea-bin"),
            version: String::from("0"),
            description: String::new(),
            installed: false,
            installed_version: None,
            votes: None,
            popularity: None,
            maintainer: None,
            out_of_date: false,
        });
        if let Some(error) = &aur.error {
            println!("AUR 那一路失败（网络问题可接受）：{error}");
        } else {
            assert!(aur.fields.iter().any(|(key, _)| key == "Votes"));
            assert!(aur.fields.iter().any(|(key, _)| key == "AUR URL"));
        }
    }

    /// 真跑：PKGBUILD 与 Arch 新闻。
    #[test]
    #[ignore = "真的执行 paru -Gp 与 curl（只读），默认跳过"]
    fn smoke_pkgbuild_and_news() {
        match run("paru", &["-Gp", "pacsea-bin"]) {
            Ok(text) => assert!(text.contains("pkgname"), "PKGBUILD 长这样？{text}"),
            Err(error) => println!("paru -Gp 失败（网络问题可接受）：{error}"),
        }

        match news() {
            Ok(items) => {
                assert!(!items.is_empty());
                println!("最新新闻：{}", items[0].title);
                println!(
                    "未读（升级之后发布的）：{:?}",
                    Some(super::super::unread_news(&items, last_upgrade()))
                );
            }
            Err(error) => println!("抓新闻失败（网络问题可接受）：{error}"),
        }
    }
}
