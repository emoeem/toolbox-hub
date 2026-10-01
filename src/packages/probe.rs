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
        self.agent
            .get(url)
            .call()
            .map_err(|error| format!("请求失败：{error}"))?
            .body_mut()
            .read_to_string()
            .map_err(|error| format!("读取响应失败：{error}"))
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

/// 扫 `PATH` 判断命令在不在。
///
/// 不走 `sh -c 'command -v …'`：后者每次要 spawn 一个 shell（约 20ms），而这里
/// 只是给「更新 / 清缓存」挑条路，不值得多花那个钱。
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
