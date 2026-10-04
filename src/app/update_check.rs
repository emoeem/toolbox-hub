//! 启动时的「有没有新版」后台检查。
//!
//! # 为什么只自动化「检查」
//!
//! 升级一个工具包 = 下载并执行别人新写的代码。把它做成静默自动应用，等于让一个
//! 远程仓库在你不知情的时候换掉本机脚本 —— 这和这个项目「先看清要干什么，再动手」
//! 的纪律冲突。所以这里的分工是：
//!
//! * **自动**：索引过期就在后台刷新，然后告诉你「有几个包能升」；
//! * **显式**：真正下载与替换，要你按下去（发现页的 U，或 `toolbox-hub update`）。
//!
//! 想要无人值守的机器可以自己挂 systemd timer 跑 `toolbox-hub update` ——
//! 那是你显式选择的，不是我们替你决定的。
//!
//! 全程在后台线程里做（见 repository::worker），界面一帧都不会卡。

use crate::repository::{
    service::Service,
    worker::{RepositoryWorker, Response},
};

/// 一次启动检查的全部状态。
pub struct UpdateCheck {
    worker: RepositoryWorker,
    service: Service,
    /// `None` = 还在查；`Some(n)` = 查到的可升级数。
    upgradable: Option<usize>,
    /// 刷新失败之类的问题（不致命，界面上一句话带过）。
    problem: Option<String>,
}

impl UpdateCheck {
    /// 按当前配置起一次检查；没有启用任何仓库时返回 `None`（不需要检查）。
    pub fn start() -> Option<Self> {
        Self::with_service(Service::from_config())
    }

    /// 用给定服务起一次检查（测试用；也让将来能换数据源）。
    pub fn with_service(service: Service) -> Option<Self> {
        let statuses = service.statuses();
        let enabled: Vec<&crate::repository::service::RepositoryStatus> = statuses
            .iter()
            .filter(|status| status.config.enabled)
            .collect();
        if enabled.is_empty() {
            return None;
        }
        // 索引过期（或从没取过）才联网。全是新的就纯本地算，一次网络都不发。
        let stale = enabled.iter().any(|status| !status.state.usable());

        let worker = RepositoryWorker::start(service.clone()).ok()?;
        if stale && worker.refresh_all().is_err() {
            return None;
        }

        let mut check = Self {
            worker,
            service,
            upgradable: None,
            problem: None,
        };
        if !stale {
            // 缓存是新的，答案现在就有。
            check.recount();
        }
        Some(check)
    }

    /// 收后台回应。返回「有没有变化」。
    pub fn poll(&mut self) -> bool {
        let mut changed = false;
        while let Some(response) = self.worker.try_recv() {
            changed = true;
            match response {
                Response::Progress(_) => {}
                Response::Refreshed(fetch) => {
                    if !fetch.state.usable() {
                        self.problem = fetch.error.clone();
                    }
                }
                Response::RefreshFinished(results) => {
                    // 一个仓库失败不影响别的：这里只记一句，接着照常算升级数。
                    self.problem = results.iter().find_map(|result| {
                        (!result.state.usable())
                            .then(|| result.error.clone())
                            .flatten()
                    });
                    self.recount();
                }
                _ => {}
            }
        }
        changed
    }

    /// 现在知道的可升级数（还没查出来就是 `None`）。
    pub fn upgradable(&self) -> Option<usize> {
        self.upgradable
    }

    pub fn problem(&self) -> Option<&str> {
        self.problem.as_deref()
    }

    /// 重新算一遍可升级数（只读本地账本与索引缓存，不联网）。
    fn recount(&mut self) {
        self.upgradable = Some(self.service.update_candidates().0.len());
    }
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use super::*;
    use crate::repository::{
        cache,
        config::{Repositories, RepositoryConfig},
        install::Roots,
        installed::{self, SCHEMA_VERSION},
        service::Service,
    };

    fn temp(tag: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("toolbox-hub-updatecheck-{tag}-{nanos}"));
        std::fs::create_dir_all(&dir).expect("mkdir");
        dir
    }

    const INDEX: &str = r#"{"schema_version": 1, "packages": [
        {"id": "hello-tool", "name": "Hello", "version": "2.0.0", "summary": "新版本",
         "files": [{"path": "scripts/hello-tool", "kind": "bin"}]}
    ]}"#;

    /// 造一个服务：索引已缓存 + 本地装着一个旧版本。
    fn service(base: &Path, cached: bool, installed_version: Option<&str>) -> Service {
        let cache_root = base.join("cache");
        if cached {
            let meta = cache::IndexMeta {
                fetched_at: Some(cache::now_secs()),
                ..cache::IndexMeta::default()
            };
            cache::store(&cache_root, "official", INDEX, &meta).expect("store");
        }
        let repositories = Repositories {
            repositories: vec![RepositoryConfig {
                id: String::from("official"),
                name: String::from("ToolHub Official"),
                index: base.join("index.json").display().to_string(),
                enabled: true,
                priority: 0,
                trust: Some(String::from("trusted")),
                note: None,
            }],
        };
        let roots = Roots {
            bin: base.join("bin"),
            data: base.join("data"),
            config: base.join("config"),
        };
        if let Some(version) = installed_version {
            let package = installed::InstalledPackage {
                schema_version: SCHEMA_VERSION,
                id: String::from("hello-tool"),
                name: String::from("Hello"),
                version: version.to_string(),
                repository: String::from("official"),
                repository_name: String::from("ToolHub Official"),
                trust: String::from("trusted"),
                installed_at: 0,
                source: None,
                license: None,
                requires_root: false,
                danger: String::from("safe"),
                artifact_sha256: None,
                dependencies: Vec::new(),
                files: Vec::new(),
                allow_unverified: false,
            };
            installed::save(&roots.data, &package).expect("save");
        }
        Service::with_parts(repositories, roots, cache_root)
    }

    #[test]
    fn no_enabled_repository_means_no_check() {
        let base = temp("norepo");
        let mut service = service(&base, true, None);
        service.repositories.repositories.clear();
        assert!(UpdateCheck::with_service(service).is_none());
        std::fs::remove_dir_all(&base).expect("cleanup");
    }

    /// 缓存是新的：立刻就能给答案，而且不发网络请求。
    #[test]
    fn a_fresh_cache_answers_immediately() {
        let base = temp("fresh");
        let check =
            UpdateCheck::with_service(service(&base, true, Some("1.0.0"))).expect("应当检查");
        assert_eq!(check.upgradable(), Some(1), "1.0.0 对 2.0.0 是可升级");
        std::fs::remove_dir_all(&base).expect("cleanup");
    }

    #[test]
    fn an_up_to_date_install_reports_nothing_to_do() {
        let base = temp("uptodate");
        let check =
            UpdateCheck::with_service(service(&base, true, Some("2.0.0"))).expect("应当检查");
        assert_eq!(check.upgradable(), Some(0));
        std::fs::remove_dir_all(&base).expect("cleanup");
    }

    /// 没装过任何包：可升级数是 0，不是「未知」。
    #[test]
    fn nothing_installed_means_nothing_to_upgrade() {
        let base = temp("nothing");
        let check = UpdateCheck::with_service(service(&base, true, None)).expect("应当检查");
        assert_eq!(check.upgradable(), Some(0));
        std::fs::remove_dir_all(&base).expect("cleanup");
    }

    /// 索引过期：先给「还在查」，后台刷新完了才给数。
    #[test]
    fn a_stale_cache_refreshes_then_answers() {
        let base = temp("stale");
        // 本地仓库（file://）指向一个真实存在的索引，刷新一定成功。
        let index_file = base.join("index.json");
        std::fs::write(&index_file, INDEX).expect("write");
        let mut service = service(&base, false, Some("1.0.0"));
        service.repositories.repositories[0].index = index_file.display().to_string();

        let mut check = UpdateCheck::with_service(service).expect("应当检查");
        assert_eq!(check.upgradable(), None, "刚起来时还在查");

        for _ in 0..300 {
            if check.poll() && check.upgradable().is_some() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert_eq!(check.upgradable(), Some(1), "刷新完就该算出可升级数");
        assert!(check.problem().is_none(), "{:?}", check.problem());
        std::fs::remove_dir_all(&base).expect("cleanup");
    }

    /// 索引取不到时不能卡在「未知」：照样把本地能算的算出来，并记一句问题。
    #[test]
    fn an_unreachable_index_still_reports_and_records_the_problem() {
        let base = temp("unreachable");
        let mut service = service(&base, false, Some("1.0.0"));
        service.repositories.repositories[0].index = base.join("不存在.json").display().to_string();

        let mut check = UpdateCheck::with_service(service).expect("应当检查");
        for _ in 0..300 {
            if check.poll() && check.problem().is_some() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(check.problem().is_some(), "取不到索引要留一句说明");
        std::fs::remove_dir_all(&base).expect("cleanup");
    }
}
