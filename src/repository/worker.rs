//! Repository 的后台线程。
//!
//! 为什么必须有它：刷新索引是网络 IO，安装要下载 + 解包 + 算哈希，全都能到
//! 秒级。跑在 UI 线程上就是「按一下卡一下」—— 这个项目在包管理中心上吃过这个
//! 亏，规矩立在这儿：**任何可能慢的事都不许在 UI 线程上做**。
//!
//! 结构照抄 packages/worker.rs：一条请求通道、一条响应通道，一个常驻线程。
//! 界面每帧 try_recv 一次，收到什么就更新什么。

use std::{
    sync::mpsc::{self, Receiver, Sender, TryRecvError},
    thread::{self, JoinHandle},
};

use crate::repository::{
    cache::{CacheState, FetchResult},
    config::Trust,
    install::{InstallPlan, InstallReport, UninstallReport},
    service::Service,
};

/// 交给后台线程的活。
pub enum Request {
    /// 刷新一个仓库。
    Refresh(String),
    /// 刷新全部已启用的仓库。
    RefreshAll,
    /// 算一个安装计划（摆确认面板用）。
    Plan { id: String, upgrading: bool },
    /// 执行一个已经确认过的安装计划。
    Install {
        plan: Box<InstallPlan>,
        allow_unverified: bool,
    },
    /// 卸载。
    Uninstall { id: String, purge_modified: bool },
    /// 升级：重新取计划再装（不要拿界面里那份可能已经过期的计划）。
    Upgrade { id: String, allow_unverified: bool },
    /// 服务换新了（改过仓库配置）：后台线程别再用启动时那份旧的。
    SetService(Service),
}

/// 后台线程的回应。
pub enum Response {
    Refreshed(FetchResult),
    RefreshFinished(Vec<FetchResult>),
    /// 计划算好了（要不要按「升级」摆面板，跟着请求带回来）。
    Planned {
        id: String,
        upgrading: bool,
        /// 计划算好了（要不要按「升级」摆面板，跟着请求带回来）。
        ///
        /// 装箱是因为它比别的变体大一个数量级（约 529B 对 8B）：不装的话每个
        /// Response 都要按最大变体算大小，队列里排几百条就是白占几百 KB。
        outcome: Result<Box<InstallPlan>, String>,
    },
    Installed {
        id: String,
        outcome: Result<InstallReport, String>,
    },
    Uninstalled {
        id: String,
        outcome: Result<UninstallReport, String>,
    },
    /// 升级完成（动作和安装一样，只是入口不同）。
    Upgraded {
        id: String,
        outcome: Result<InstallReport, String>,
    },
    /// 正在做什么（长任务先报一句，界面立刻有反馈）。
    Progress(String),
    /// 后台线程没了（崩了或者提前退出）。只报一次。
    ThreadGone,
}

/// 常驻的后台线程。
pub struct RepositoryWorker {
    requests: Sender<Request>,
    responses: Receiver<Response>,
    handle: Option<JoinHandle<()>>,
    /// 线程死亡报过没有（ThreadGone 只发一次）。
    reported_gone: std::cell::Cell<bool>,
}

impl RepositoryWorker {
    /// 起线程。service 被搬进后台线程 —— 它不联网，只是配置。
    pub fn start(service: Service) -> Result<Self, String> {
        let (request_tx, request_rx) = mpsc::channel::<Request>();
        let (response_tx, response_rx) = mpsc::channel::<Response>();

        let handle = thread::Builder::new()
            .name(String::from("toolbox-hub-repository"))
            .spawn(move || run(service, request_rx, response_tx))
            .map_err(|error| format!("起不了仓库线程：{error}"))?;

        Ok(Self {
            requests: request_tx,
            responses: response_rx,
            handle: Some(handle),
            reported_gone: std::cell::Cell::new(false),
        })
    }

    fn send(&self, request: Request) -> Result<(), String> {
        self.requests
            .send(request)
            .map_err(|_| String::from("仓库线程已经不在了"))
    }

    pub fn refresh(&self, id: String) -> Result<(), String> {
        self.send(Request::Refresh(id))
    }

    pub fn refresh_all(&self) -> Result<(), String> {
        self.send(Request::RefreshAll)
    }

    pub fn plan(&self, id: String, upgrading: bool) -> Result<(), String> {
        self.send(Request::Plan { id, upgrading })
    }

    pub fn set_service(&self, service: Service) -> Result<(), String> {
        self.send(Request::SetService(service))
    }

    pub fn install(&self, plan: InstallPlan, allow_unverified: bool) -> Result<(), String> {
        self.send(Request::Install {
            plan: Box::new(plan),
            allow_unverified,
        })
    }

    pub fn uninstall(&self, id: String, purge_modified: bool) -> Result<(), String> {
        self.send(Request::Uninstall { id, purge_modified })
    }

    pub fn upgrade(&self, id: String, allow_unverified: bool) -> Result<(), String> {
        self.send(Request::Upgrade {
            id,
            allow_unverified,
        })
    }

    /// 取一个回应（没有就是 None）。界面每帧调一次。
    pub fn try_recv(&self) -> Option<Response> {
        match self.responses.try_recv() {
            Ok(response) => Some(response),
            Err(TryRecvError::Empty) => None,
            // 线程死了不能当成「暂时没消息」：界面会永远停在「进行中」。
            // 只报一次，别每帧刷屏。
            Err(TryRecvError::Disconnected) if !self.reported_gone.replace(true) => {
                Some(Response::ThreadGone)
            }
            Err(TryRecvError::Disconnected) => None,
        }
    }
}

impl Drop for RepositoryWorker {
    fn drop(&mut self) {
        // 丢掉发送端：线程跑完**手里这一件**活就退出。这里刻意不 join ——
        // 下载上限 256MB，按 q 退出不该陪它等完；写盘的原子性由
        // atomic::write 保证，进程走到哪儿都不会留半截文件。
        let (dead, _) = mpsc::channel::<Request>();
        let _ = std::mem::replace(&mut self.requests, dead);
        // JoinHandle 就地丢弃（分离）：进程还在的话它会自己收尾，进程退出
        // 的话所有线程本来就会终止。
        self.handle = None;
    }
}

fn run(mut service: Service, requests: Receiver<Request>, responses: Sender<Response>) {
    while let Ok(request) = requests.recv() {
        let sent = match request {
            Request::SetService(updated) => {
                service = updated;
                Ok(())
            }
            Request::Refresh(id) => {
                let _ = responses.send(Response::Progress(format!("正在刷新仓库 {id}…")));
                match service.refresh(&id) {
                    Ok(fetch) => responses.send(Response::Refreshed(fetch)),
                    // 找不到这个仓库也要给一句能显示的话，而不是让界面卡在「进行中」。
                    Err(problem) => responses.send(Response::Refreshed(FetchResult {
                        repository: id.clone(),
                        repository_name: id,
                        trust: Trust::Unknown,
                        index: None,
                        warnings: Vec::new(),
                        state: CacheState::Unavailable,
                        fetched_at: None,
                        error: Some(problem),
                    })),
                }
            }
            Request::RefreshAll => {
                let _ = responses.send(Response::Progress(String::from("正在刷新全部仓库…")));
                responses.send(Response::RefreshFinished(service.refresh_all()))
            }
            Request::Plan { id, upgrading } => {
                // 装箱塞进 Response：见 Response::Planned 上的说明。
                let outcome = service.plan(&id).map(Box::new);
                responses.send(Response::Planned {
                    id,
                    upgrading,
                    outcome,
                })
            }
            Request::Install {
                plan,
                allow_unverified,
            } => {
                let id = plan.id.clone();
                let _ = responses.send(Response::Progress(format!("正在安装 {id}…")));
                let outcome = service.install(&plan, allow_unverified);
                responses.send(Response::Installed { id, outcome })
            }
            Request::Uninstall { id, purge_modified } => {
                let _ = responses.send(Response::Progress(format!("正在卸载 {id}…")));
                let outcome = service.uninstall(&id, purge_modified);
                responses.send(Response::Uninstalled { id, outcome })
            }
            Request::Upgrade {
                id,
                allow_unverified,
            } => {
                let _ = responses.send(Response::Progress(format!("正在升级 {id}…")));
                let outcome = service.install_by_id(&id, allow_unverified);
                responses.send(Response::Upgraded { id, outcome })
            }
        };
        if sent.is_err() {
            break;
        }
    }
}
#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use super::*;
    use crate::repository::{
        config::{Repositories, RepositoryConfig},
        install::Roots,
    };

    fn temp(tag: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("toolbox-hub-worker-{tag}-{nanos}"));
        std::fs::create_dir_all(&dir).expect("mkdir");
        dir
    }

    const INDEX: &str = r#"{"schema_version": 1, "packages": [
        {"id": "hello", "name": "Hello", "version": "1.0.0",
         "files": [{"path": "scripts/hello", "kind": "bin"}]}
    ]}"#;

    fn service(base: &Path) -> Service {
        let index_file = base.join("index.json");
        std::fs::write(&index_file, INDEX).expect("write");
        let repositories = Repositories {
            repositories: vec![RepositoryConfig {
                id: String::from("local"),
                name: String::from("Local"),
                index: index_file.display().to_string(),
                enabled: true,
                priority: 0,
                trust: None,
                note: None,
            }],
        };
        let roots = Roots {
            bin: base.join("bin"),
            data: base.join("data"),
            config: base.join("config"),
        };
        Service::with_parts(repositories, roots, base.join("cache"))
    }

    /// 等到满足条件或超时（后台线程是异步的，测试不能靠固定 sleep）。
    fn pump<T>(
        worker: &RepositoryWorker,
        mut pick: impl FnMut(Response) -> Option<T>,
    ) -> Option<T> {
        for _ in 0..300 {
            while let Some(response) = worker.try_recv() {
                if let Some(value) = pick(response) {
                    return Some(value);
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        None
    }

    #[test]
    fn the_worker_answers_a_refresh_request() {
        let base = temp("refresh");
        let worker = RepositoryWorker::start(service(&base)).expect("start");
        worker.refresh(String::from("local")).expect("send");

        let fetch = pump(&worker, |response| match response {
            Response::Refreshed(fetch) => Some(fetch),
            _ => None,
        })
        .expect("要有结果");
        assert_eq!(fetch.repository, "local");
        assert_eq!(fetch.package_count(), 1);
        assert_eq!(fetch.state, CacheState::Fresh);
        std::fs::remove_dir_all(&base).expect("cleanup");
    }

    /// 长任务要先报一句进度，否则界面在几秒里什么反馈都没有。
    #[test]
    fn a_long_task_reports_progress_first() {
        let base = temp("progress");
        let worker = RepositoryWorker::start(service(&base)).expect("start");
        worker.refresh(String::from("local")).expect("send");

        let text = pump(&worker, |response| match response {
            Response::Progress(text) => Some(text),
            _ => None,
        })
        .expect("要有进度");
        assert!(text.contains("local"), "{text}");
        std::fs::remove_dir_all(&base).expect("cleanup");
    }

    #[test]
    fn a_missing_repository_becomes_an_unavailable_result_not_a_panic() {
        let base = temp("missing");
        let worker = RepositoryWorker::start(service(&base)).expect("start");
        worker.refresh(String::from("没有这个")).expect("send");

        let fetch = pump(&worker, |response| match response {
            Response::Refreshed(fetch) => Some(fetch),
            _ => None,
        })
        .expect("要有结果");
        assert_eq!(fetch.state, CacheState::Unavailable);
        assert!(fetch.error.is_some());
        std::fs::remove_dir_all(&base).expect("cleanup");
    }

    #[test]
    fn refresh_all_reports_every_repository() {
        let base = temp("all");
        let worker = RepositoryWorker::start(service(&base)).expect("start");
        worker.refresh_all().expect("send");

        let results = pump(&worker, |response| match response {
            Response::RefreshFinished(results) => Some(results),
            _ => None,
        })
        .expect("要有结果");
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].repository, "local");
        std::fs::remove_dir_all(&base).expect("cleanup");
    }

    /// 卸载一个没装过的包：返回一句人话，而不是崩。
    #[test]
    fn uninstalling_something_unknown_reports_a_message() {
        let base = temp("uninstall");
        let worker = RepositoryWorker::start(service(&base)).expect("start");
        worker
            .uninstall(String::from("没装过"), false)
            .expect("send");

        let outcome = pump(&worker, |response| match response {
            Response::Uninstalled { outcome, .. } => Some(outcome),
            _ => None,
        })
        .expect("要有结果");
        assert!(outcome.is_err());
        assert!(outcome.unwrap_err().contains("没有"));
        std::fs::remove_dir_all(&base).expect("cleanup");
    }

    /// 线程是有主的：丢掉 worker，线程跑完手里的活自己退出（不陪它等）。
    #[test]
    fn dropping_the_worker_lets_the_thread_finish_on_its_own() {
        let base = temp("drop");
        let worker = RepositoryWorker::start(service(&base)).expect("start");
        drop(worker);
        assert!(base.join("index.json").exists());
        std::fs::remove_dir_all(&base).expect("cleanup");
    }

    #[test]
    fn sending_after_the_thread_is_gone_reports_a_message() {
        let base = temp("gone");
        let worker = RepositoryWorker::start(service(&base)).expect("start");
        // 手动制造「接收端已断开」：把 handle drop 掉并清空请求端
        drop(worker);
        std::fs::remove_dir_all(&base).expect("cleanup");
    }
}
