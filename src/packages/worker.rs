//! 常驻的取数线程：一个拿住 libalpm 上下文，一个拿住 HTTP 连接。
//!
//! ## 为什么要有这一层
//!
//! 两样东西**建起来很贵，但可以一直用**：
//!
//! * libalpm 第一次访问同步库要 ~365ms（把整个 `.db` 解析进内存），之后 ~3ms；
//!   本地依赖索引 26ms，`sync_names` 也要 365ms；
//! * HTTPS 的第一次握手实测 15.5s（到 aur.archlinux.org），之后同一条连接 0.7s。
//!
//! 之前的写法是「每次查询开一个线程、里面现场 open 一遍 / spawn 一个 curl」——
//! 那笔钱**每次都要重付**。现在改成常驻：进程起来时开两个线程，之后所有请求
//! 都是往 channel 里丢一条消息。
//!
//! ## 为什么是两个线程
//!
//! 网络请求动不动几秒（AUR 慢的时候十几秒），而数据库查询是毫秒级。合成一个
//! 线程的话，一次慢搜索会把「看一眼包信息」也堵住。分开之后各堵各的：搜索在
//! 慢慢跑，信息面板照样秒回。
//!
//! 两个线程都不能自己再开子线程去碰对方的资源（libalpm 句柄不 `Send`），
//! 所以数据库线程只做数据库的事，网络线程只做网络的事 —— 这条边界是硬的。

use std::{
    collections::HashMap,
    sync::{
        Arc, RwLock,
        mpsc::{self, Receiver, Sender},
    },
    thread,
};

use super::{
    InstalledPackage, NewsItem, PackageHit,
    health::{self, HealthItem},
    libalpm::Db,
    probe::{InfoOutcome, Net},
};

/// 发给数据库线程的请求。
pub enum DbRequest {
    Search {
        search_id: u64,
        term: String,
    },
    /// 整个同步库（paru 那个「一进来就有 38869 个包」的列表）。
    AllPackages,
    Info(String),
    Installed,
    /// 可更新数 + 同步库有多旧（一起问，省一次往返）。
    Updates,
    Removal(Vec<String>),
    DownloadTotal(Vec<String>),
    OrphanNames,
    /// 维护面板的一屏检查。
    Health,
    /// 文件完整性（`pacman -Qk`，几秒，按需跑）。
    FileIntegrity,
}

/// 发给网络线程的请求。
pub enum NetRequest {
    /// AUR 搜索（顺带缓存整包信息，之后看信息不用再联网）。
    AurSearch {
        search_id: u64,
        term: String,
    },
    AurInfo(String),
    News,
}

/// 从两个线程回来的东西（共用一个 channel）。
pub enum Response {
    /// 官方源搜索结果。
    Official {
        search_id: u64,
        hits: Vec<PackageHit>,
    },
    /// 全部包（浏览模式）。
    AllPackages(Vec<PackageHit>),
    /// AUR 搜索结果。
    Aur {
        search_id: u64,
        hits: Vec<PackageHit>,
    },
    /// 某一路失败了（`source` 是「官方源」或「AUR」）。
    Failed {
        source: &'static str,
        error: String,
        search_id: Option<u64>,
    },
    Info(InfoOutcome),
    Installed(Result<Vec<InstalledPackage>, String>),
    /// 可更新数 + 同步库多久没同步。
    Updates {
        count: Option<usize>,
        age: Option<u64>,
    },
    /// 卸载影响（补在确认面板上的那几行）。
    Removal(Vec<String>),
    DownloadTotal(Option<u64>),
    OrphanNames(Result<Vec<String>, String>),
    /// 维护面板的一屏检查。
    Health(Result<Vec<HealthItem>, String>),
    /// 文件完整性检查的输出（已经整理成行）。
    FileIntegrity(Result<Vec<String>, String>),
    News(Result<NewsChunk, String>),
}

/// 新闻线程回来的一整包东西。
pub struct NewsChunk {
    pub items: Vec<NewsItem>,
    pub after_upgrade: usize,
    pub mark: Option<u64>,
}

/// 常驻工作线程的句柄（界面只跟它打交道）。
pub struct Worker {
    db_tx: Sender<DbRequest>,
    net_tx: Sender<NetRequest>,
    rx: Receiver<Response>,
    /// 本地已装包的 名字 → 版本：数据库线程写，网络线程读（给 AUR 命中标状态）。
    /// 界面自己不需要它，所以这里只留一个名字占位，避免字段被当成没用的东西删掉。
    _local: Arc<RwLock<HashMap<String, String>>>,
}

impl Worker {
    /// 起两个常驻线程。
    ///
    /// 返回得很快：libalpm 的打开与 HTTP agent 的创建都在各自线程里，
    /// 而且都不会阻塞界面（真正的首次解析发生在第一个请求上）。
    pub fn start() -> Result<Self, String> {
        let (tx, rx) = mpsc::channel();
        let local: Arc<RwLock<HashMap<String, String>>> = Arc::new(RwLock::new(HashMap::new()));

        let db_tx = spawn_db(tx.clone(), Arc::clone(&local))?;
        let net_tx = spawn_net(tx, Arc::clone(&local))?;

        Ok(Self {
            db_tx,
            net_tx,
            rx,
            _local: local,
        })
    }

    fn db(&self, request: DbRequest) {
        let _ = self.db_tx.send(request);
    }

    fn net(&self, request: NetRequest) {
        let _ = self.net_tx.send(request);
    }

    /// 拉全部包（进界面时先把这个铺上，别让用户对着空屏发呆）。
    pub fn all_packages(&self) {
        self.db(DbRequest::AllPackages);
    }

    /// 搜两路：官方源走数据库线程，AUR 走网络线程，谁先回来谁先上屏。
    pub fn search(&self, term: &str, search_id: u64) {
        self.db(DbRequest::Search {
            search_id,
            term: term.to_string(),
        });
        self.net(NetRequest::AurSearch {
            search_id,
            term: term.to_string(),
        });
    }

    /// 查包信息：AUR 走网络（多半命中搜索缓存），其余走数据库。
    pub fn info(&self, hit: &PackageHit) {
        if hit.is_aur() {
            self.net(NetRequest::AurInfo(hit.name.clone()));
        } else {
            self.db(DbRequest::Info(hit.name.clone()));
        }
    }

    pub fn installed(&self) {
        self.db(DbRequest::Installed);
    }

    /// 可更新数 + 同步库年龄。
    pub fn updates(&self) {
        self.db(DbRequest::Updates);
    }

    pub fn removal(&self, names: Vec<String>) {
        self.db(DbRequest::Removal(names));
    }

    pub fn download_total(&self, names: Vec<String>) {
        self.db(DbRequest::DownloadTotal(names));
    }

    pub fn orphan_names(&self) {
        self.db(DbRequest::OrphanNames);
    }

    pub fn health(&self) {
        self.db(DbRequest::Health);
    }

    /// 文件完整性（慢，用户按了才跑）。
    pub fn file_integrity(&self) {
        self.db(DbRequest::FileIntegrity);
    }

    pub fn news(&self) {
        self.net(NetRequest::News);
    }

    /// 非阻塞取一条结果。
    pub fn try_recv(&self) -> Option<Response> {
        self.rx.try_recv().ok()
    }
}

fn spawn_db(
    tx: Sender<Response>,
    local: Arc<RwLock<HashMap<String, String>>>,
) -> Result<Sender<DbRequest>, String> {
    let (db_tx, db_rx) = mpsc::channel::<DbRequest>();
    thread::Builder::new()
        .name(String::from("pkg-db"))
        .spawn(move || {
            // 句柄在这里开：libalpm 不 Send，它这辈子就住在这个线程里。
            let db = match Db::open() {
                Ok(db) => db,
                Err(error) => {
                    // 让每个请求都拿到同一句错误，界面自己会说
                    while let Ok(request) = db_rx.recv() {
                        let _ = tx.send(failed_for(&request, &error));
                    }
                    return;
                }
            };
            while let Ok(request) = db_rx.recv() {
                let response = match request {
                    DbRequest::Search { search_id, term } => match db.search(&term) {
                        Ok(hits) => {
                            // 顺手把本地版本表刷新一份给网络线程用
                            if let Ok(mut shared) = local.write() {
                                *shared = db.local_versions().clone();
                            }
                            Response::Official { search_id, hits }
                        }
                        Err(error) => Response::Failed {
                            source: "官方源",
                            error,
                            search_id: Some(search_id),
                        },
                    },
                    DbRequest::Info(name) => Response::Info(match db.info(&name) {
                        Ok(fields) => InfoOutcome {
                            name,
                            fields,
                            error: None,
                        },
                        Err(error) => InfoOutcome {
                            name,
                            fields: Vec::new(),
                            error: Some(error),
                        },
                    }),
                    DbRequest::AllPackages => Response::AllPackages(db.all_packages()),
                    DbRequest::Installed => {
                        if let Ok(mut shared) = local.write() {
                            *shared = db.local_versions().clone();
                        }
                        Response::Installed(db.installed())
                    }
                    DbRequest::Updates => Response::Updates {
                        count: db.pending_updates(),
                        age: db.sync_age().map(|age| age.as_secs()),
                    },
                    DbRequest::Removal(names) => Response::Removal(db.removal_report(&names)),
                    DbRequest::DownloadTotal(names) => {
                        Response::DownloadTotal(db.download_total(&names))
                    }
                    DbRequest::OrphanNames => Response::OrphanNames(Ok(db.orphan_names())),
                    DbRequest::Health => Response::Health(Ok(health::scan(&db))),
                    DbRequest::FileIntegrity => Response::FileIntegrity(file_integrity_output()),
                };
                if tx.send(response).is_err() {
                    break;
                }
            }
        })
        .map_err(|error| format!("起不了包数据库线程：{error}"))?;
    Ok(db_tx)
}

fn spawn_net(
    tx: Sender<Response>,
    local: Arc<RwLock<HashMap<String, String>>>,
) -> Result<Sender<NetRequest>, String> {
    let (net_tx, net_rx) = mpsc::channel::<NetRequest>();
    thread::Builder::new()
        .name(String::from("pkg-net"))
        .spawn(move || {
            let mut net = Net::new();
            while let Ok(request) = net_rx.recv() {
                let response = match request {
                    NetRequest::AurSearch { search_id, term } => {
                        let installed = local.read().map(|map| map.clone()).unwrap_or_default();
                        match net.search_aur(&term, &installed) {
                            Ok(hits) => Response::Aur { search_id, hits },
                            Err(error) => Response::Failed {
                                source: "AUR",
                                error,
                                search_id: Some(search_id),
                            },
                        }
                    }
                    NetRequest::AurInfo(name) => Response::Info(match net.aur_info(&name) {
                        Ok(fields) => InfoOutcome {
                            name,
                            fields,
                            error: None,
                        },
                        Err(error) => InfoOutcome {
                            name,
                            fields: Vec::new(),
                            error: Some(error),
                        },
                    }),
                    NetRequest::News => Response::News(net.news().map(|items| {
                        // 「升级之后发布的」要读 pacman.log；只读一次
                        let mark = super::probe::last_upgrade();
                        NewsChunk {
                            after_upgrade: super::news_published_since(&items, mark),
                            mark,
                            items,
                        }
                    })),
                };
                if tx.send(response).is_err() {
                    break;
                }
            }
        })
        .map_err(|error| format!("起不了包网络线程：{error}"))?;
    Ok(net_tx)
}

/// `pacman -Qk` 的输出整理成行（几秒级，只在用户按了才跑）。
fn file_integrity_output() -> Result<Vec<String>, String> {
    let output = std::process::Command::new("pacman")
        .args(["-Qk"])
        .output()
        .map_err(|error| format!("pacman -Qk 跑不起来：{error}"))?;
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(health::parse_file_check(&text, 200))
}

/// 数据库打不开时，把错误翻译成「这次请求该回什么」。
fn failed_for(request: &DbRequest, error: &str) -> Response {
    match request {
        DbRequest::AllPackages => Response::Failed {
            source: "官方源",
            error: error.to_string(),
            search_id: None,
        },
        DbRequest::Search { search_id, .. } => Response::Failed {
            source: "官方源",
            error: error.to_string(),
            search_id: Some(*search_id),
        },
        DbRequest::Info(name) => Response::Info(InfoOutcome {
            name: name.clone(),
            fields: Vec::new(),
            error: Some(error.to_string()),
        }),
        DbRequest::Installed => Response::Installed(Err(error.to_string())),
        DbRequest::Updates => Response::Updates {
            count: None,
            age: None,
        },
        DbRequest::Removal(_) => Response::Removal(Vec::new()),
        DbRequest::DownloadTotal(_) => Response::DownloadTotal(None),
        DbRequest::OrphanNames => Response::OrphanNames(Err(error.to_string())),
        DbRequest::Health => Response::Health(Err(error.to_string())),
        DbRequest::FileIntegrity => Response::FileIntegrity(Err(error.to_string())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 两个线程都起得来，并且一条请求能得到一条回答。
    ///
    /// 这条会真的读本机 pacman 数据库（只读）—— 不算纯 hermetic，
    /// 但它是「常驻线程真的活着」的唯一证据，所以留着。
    #[test]
    fn workers_answer_requests() {
        let Ok(worker) = Worker::start() else {
            panic!("线程起不来");
        };
        worker.updates();

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            if let Some(response) = worker.try_recv() {
                match response {
                    Response::Updates { count, .. } => {
                        assert!(count.is_some(), "本机数据库应该读得出来");
                        break;
                    }
                    other => panic!("收到了意料之外的回答：{}", describe(&other)),
                }
            }
            assert!(std::time::Instant::now() < deadline, "等 10 秒还没等到回答");
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }

    #[test]
    fn database_failures_are_not_reported_as_clean_results() {
        assert!(matches!(
            failed_for(&DbRequest::OrphanNames, "database unavailable"),
            Response::OrphanNames(Err(error)) if error == "database unavailable"
        ));
        assert!(matches!(
            failed_for(&DbRequest::Health, "database unavailable"),
            Response::Health(Err(error)) if error == "database unavailable"
        ));
        assert!(matches!(
            failed_for(&DbRequest::FileIntegrity, "database unavailable"),
            Response::FileIntegrity(Err(error)) if error == "database unavailable"
        ));
    }

    fn describe(response: &Response) -> &'static str {
        match response {
            Response::Official { .. } => "官方源结果",
            Response::AllPackages(_) => "全部包",
            Response::Aur { .. } => "AUR 结果",
            Response::Failed { .. } => "失败",
            Response::Info(_) => "包信息",
            Response::Installed(_) => "已安装",
            Response::Updates { .. } => "可更新",
            Response::Removal(_) => "卸载影响",
            Response::DownloadTotal(_) => "下载体积",
            Response::OrphanNames(_) => "孤儿名单",
            Response::Health(_) => "维护检查",
            Response::FileIntegrity(_) => "文件完整性",
            Response::News(_) => "新闻",
        }
    }
}
