//! 取数层里**需要网络**的那部分：AUR RPC 与 Arch 新闻。
//!
//! 官方源那边（搜索、包信息、已安装、可更新、依赖索引）全在
//! [`crate::packages::libalpm`]；这里只管 libalpm 够不着的东西。
//!
//! ## 为什么不再是「每次 spawn 一个 curl」
//!
//! 进程之间**不可能复用连接**，于是每一次 AUR 请求都要重新做一遍 TLS 握手。
//! 实测（这台机器到 aur.archlinux.org）：冷握手 15.5s、热连接 0.73s —— 一次
//! 搜索加上一次包信息就是三十秒。所以这里持有一个常驻的 [`ureq::Agent`]：
//! 第一次之后所有请求都走已经握好的连接（实测 1.03s → 0.28s）。
//!
//! ## AUR 包信息不联网
//!
//! AUR 的搜索响应里**已经带了**信息面板要的全部字段（依赖、许可证、provides、
//! 关键词、得票、维护者、提交/修改时间……）。所以搜索时把整个 [`AurPackage`]
//! 留下来，选中它时直接拼面板 —— 不再发第二次请求。

use std::{collections::HashMap, time::Duration};

const MAX_RESPONSE_BYTES: u64 = 4 * 1024 * 1024;

use super::{
    AurPackage, InstalledState, NewsItem, PackageHit, aur_info_url, aur_search_url, parse_aur_one,
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

/// 联网取数的常驻上下文：一个 agent（连接池）+ 搜索过的 AUR 包。
pub struct Net {
    agent: ureq::Agent,
    /// 搜到过的 AUR 包：名字 → 整个 RPC 对象（信息面板直接吃它，不再联网）。
    aur: HashMap<String, AurPackage>,
}

impl Default for Net {
    fn default() -> Self {
        Self::new()
    }
}

impl Net {
    pub fn new() -> Self {
        let agent = ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(20)))
            .user_agent("toolbox-hub")
            .build()
            .new_agent();
        Self {
            agent,
            aur: HashMap::new(),
        }
    }

    fn get(&self, url: &str) -> Result<String, String> {
        for attempt in 0..=2 {
            match self.agent.get(url).call() {
                Ok(mut response) => {
                    return response
                        .body_mut()
                        .with_config()
                        .limit(MAX_RESPONSE_BYTES)
                        .read_to_string()
                        .map_err(|error| format!("读取响应失败：{error}"));
                }
                Err(ureq::Error::StatusCode(429)) if attempt < 2 => {
                    std::thread::sleep(Duration::from_millis(250 << attempt));
                }
                Err(error) => return Err(format!("请求失败：{error}")),
            }
        }
        Err(String::from("请求失败：服务器持续限流（HTTP 429）"))
    }

    /// AUR 搜索；顺手把结果存起来，后面看信息就不用再问了。
    ///
    /// `installed` 是「本地已装的名字 → 版本」，用来标「已安装 / 可升级」——
    /// AUR 自己不知道你装没装（那是本地库的事）。
    pub fn search_aur(
        &mut self,
        term: &str,
        installed: &HashMap<String, String>,
    ) -> Result<Vec<PackageHit>, String> {
        let text = self.get(&aur_search_url(term))?;
        let packages = parse_aur_search(&text)?;
        let mut hits = Vec::with_capacity(packages.len());
        for package in packages {
            let mut hit = package.hit();
            hit.installed_state = state_for(installed, &package.name, &package.version);
            self.aur.insert(package.name.clone(), package);
            hits.push(hit);
        }
        Ok(hits)
    }

    /// AUR 包信息：**先查搜索缓存**，没有再问 RPC。
    pub fn aur_info(&mut self, name: &str) -> Result<Vec<(String, String)>, String> {
        if let Some(package) = self.aur.get(name) {
            return Ok(package.info_fields());
        }
        let text = self.get(&aur_info_url(name))?;
        let package = parse_aur_one(&text)?;
        let fields = package.info_fields();
        self.aur.insert(name.to_string(), package);
        Ok(fields)
    }

    /// 抓 Arch 官方新闻（RSS）。
    pub fn news(&self) -> Result<Vec<NewsItem>, String> {
        let text = self.get("https://archlinux.org/feeds/news/")?;
        let items = parse_news_rss(&text);
        if items.is_empty() {
            return Err(String::from("新闻源里没有条目（格式变了吗）"));
        }
        Ok(items)
    }
}

/// 拿本地版本和「正在看的版本」比一下（比较交给 libalpm 的 vercmp）。
pub fn state_for(installed: &HashMap<String, String>, name: &str, version: &str) -> InstalledState {
    match installed.get(name) {
        Some(local) => {
            InstalledState::from_ordering(::alpm::vercmp(version, local.as_str()), local.clone())
        }
        None => InstalledState::NotInstalled,
    }
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
    std::process::Command::new(opener)
        .arg(url)
        .spawn()
        .map(|_| ())
        .map_err(|error| format!("打不开浏览器（{opener}）：{error}"))
}

/// 命令在不在 `PATH` 上。
///
/// 走共享实现（`src/util/path.rs`）：它查的是「存在 **且可执行**」，
/// 而不是「有这个文件」—— 一个存在但没有可执行位的文件不该被当成可用命令。
fn on_path(name: &str) -> bool {
    crate::util::path::command_available(name)
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
    use std::process::Command;

    use super::*;

    /// 真跑：AUR 搜索 + 用搜索结果回答包信息（第二次不发请求）。
    #[test]
    #[ignore = "真的联网（只读），默认跳过"]
    fn smoke_aur_search_then_cached_info() {
        let mut net = Net::new();
        let hits = match net.search_aur("fzf", &HashMap::new()) {
            Ok(hits) => hits,
            Err(error) => {
                println!("AUR 那一路失败（网络问题可接受）：{error}");
                return;
            }
        };
        assert!(
            !hits.is_empty(),
            "fzf 在 AUR 里应该有东西（fzf-extras、fzf-tab-git…）"
        );
        assert!(
            hits.iter().any(|hit| hit.name.contains("fzf")),
            "结果里应该有名字带 fzf 的包：{:?}",
            hits.iter().map(|hit| &hit.name).collect::<Vec<_>>()
        );

        // 搜索过的包，信息面板不该再联网（缓存里就有）
        let name = hits[0].name.clone();
        let fields = net.aur_info(&name).expect("缓存里应该有");
        assert!(fields.iter().any(|(key, _)| key == "Votes"));
        assert!(fields.iter().any(|(key, _)| key == "AUR URL"));
        assert!(net.aur.contains_key(&name), "信息请求不该把它从缓存里踢掉");
    }

    /// 真跑：libalpm 算出来的两个数字，必须与真 pacman 一致。
    ///
    /// 对账用 `pacman -Qu` 而**不是** `checkupdates`：后者每次都重新下载数据库
    /// （就是那 18 秒），于是能看到「本地库还不知道的更新」。实测差距就是这么来的：
    /// checkupdates 30 / `pacman -Qu` 27 / libalpm 27。
    #[test]
    #[ignore = "真的跑 pacman -Qu 与 pacman -Qtdq（只读），默认跳过"]
    fn smoke_libalpm_agrees_with_pacman() {
        use crate::packages::libalpm::Db;

        let db = Db::open().expect("打开数据库");

        let expected = count_lines("pacman", &["-Qu"]);
        let ours = db.pending_updates().expect("应该算得出来");
        assert_eq!(
            ours,
            expected,
            "可更新数与 pacman -Qu 对不上（差 {}）",
            ours as i64 - expected as i64
        );

        let expected = count_lines("pacman", &["-Qtdq"]);
        let ours = db.orphan_names().len();
        assert_eq!(ours, expected, "孤儿判定与 pacman -Qtd 对不上");
    }

    /// 跑一条命令数它输出了几行（空行不算）。
    fn count_lines(program: &str, args: &[&str]) -> usize {
        let output = Command::new(program).args(args).output().expect("跑得起来");
        String::from_utf8_lossy(&output.stdout)
            .lines()
            .filter(|line| !line.trim().is_empty())
            .count()
    }

    /// 真跑：两路搜索都能出结果，同一个包不会因为出现在多个仓库里而重复。
    #[test]
    #[ignore = "真的读数据库并联网（只读），默认跳过"]
    fn smoke_search_both_sources() {
        use crate::packages::libalpm::Db;

        let db = Db::open().expect("打开数据库");
        let official = db.search("fzf").expect("官方源搜索");
        assert!(
            official.iter().any(|hit| hit.name == "fzf"),
            "官方源该有 fzf：{:?}",
            official.iter().map(|hit| &hit.name).collect::<Vec<_>>()
        );
        assert!(
            official
                .iter()
                .any(|hit| hit.name == "fzf" && hit.is_installed()),
            "fzf 装了，应该标上已安装"
        );

        // 仓库优先级去重：同名包只留优先级最高的那个仓库
        let mut names: Vec<&str> = official.iter().map(|hit| hit.name.as_str()).collect();
        let before = names.len();
        names.sort_unstable();
        names.dedup();
        assert_eq!(
            before,
            names.len(),
            "同一个包不该出现两次（cachyos 与 extra）"
        );

        // AUR 那一路可能因为网络失败，但失败要说明白是哪一路
        match Net::new().search_aur("fzf", db.local_versions()) {
            Ok(hits) => assert!(!hits.is_empty(), "AUR 里应该有名字带 fzf 的包"),
            Err(error) => println!("AUR 那一路失败（网络问题可接受）：{error}"),
        }
    }

    /// 真跑：官方源与 AUR 的信息面板（字段名是中文，照 `paru -Si` 排）。
    #[test]
    #[ignore = "真的读数据库并联网（只读），默认跳过"]
    fn smoke_info_both_sources() {
        use crate::packages::libalpm::Db;

        let db = Db::open().expect("打开数据库");
        let official = db.info("bash").expect("bash 的信息");
        for wanted in [
            "软件库",
            "名字",
            "版本",
            "描述",
            "依赖于",
            "下载大小",
            "安装后大小",
            "验证者",
        ] {
            assert!(
                official.iter().any(|(key, _)| key == wanted),
                "信息面板少了「{wanted}」：{official:?}"
            );
        }

        // 外来包（AUR 装的）也要能查：同步库里没有，只能落到本地库
        if let Some(foreign) = db
            .installed()
            .ok()
            .and_then(|packages| packages.into_iter().find(|package| package.foreign))
        {
            let fields = db.info(&foreign.name).expect("外来包也该有信息");
            assert!(
                fields.iter().any(|(key, _)| key == "名字"),
                "外来包的信息面板：{fields:?}"
            );
        }
    }

    /// 真跑：Arch 新闻。
    #[test]
    #[ignore = "真的联网（只读），默认跳过"]
    fn smoke_news() {
        match Net::new().news() {
            Ok(items) => {
                assert!(!items.is_empty());
                println!("最新新闻：{}", items[0].title);
            }
            Err(error) => println!("抓新闻失败（网络问题可接受）：{error}"),
        }
    }
}
