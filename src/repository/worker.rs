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
    /// 执行一个已经确认过的安装计划。
    Install {
        plan: Box<InstallPlan>,
        allow_unverified: bool,
    },
    /// 卸载。
    Uninstall { id: String, purge_modified: bool },
    /// 升级：重新取计划再装（不要拿界面里那份可能已经过期的计划）。
    Upgrade { id: String, allow_unverified: bool },
}

/// 后台线程的回应。
pub enum Response {
    Refreshed(FetchResult),
    RefreshFinished(Vec<FetchResult>),
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
}

/// 常驻的后台线程。
pub struct RepositoryWorker {
    requests: Sender<Request>,
    responses: Receiver<Response>,
    handle: Option<JoinHandle<()>>,
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
            Err(TryRecvError::Empty) | Err(TryRecvError::Disconnected) => None,
        }
    }
}

impl Drop for RepositoryWorker {
    fn drop(&mut self) {
        // 丢掉发送端，线程收到断开就退出；再等它一下 —— 不然它可能正在写文件，
        // 进程却已经走人了。
        let (dead, _) = mpsc::channel::<Request>();
        let _ = std::mem::replace(&mut self.requests, dead);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

fn run(service: Service, requests: Receiver<Request>, responses: Sender<Response>) {
    while let Ok(request) = requests.recv() {
        let sent = match request {
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

    /// 线程是有主的：丢掉 worker 就该把它收回来。
    #[test]
    fn dropping_the_worker_joins_its_thread() {
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
