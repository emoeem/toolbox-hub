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
    path::{Path, PathBuf},
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
use crate::runtime;

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
    /// PKGBUILD 抓取（Ctrl+X）或抓取 + 检查（Ctrl+K）的结果。
    Pkgbuild(Result<PkgbuildReport, String>),
    News(Result<NewsChunk, String>),
}

/// paru -Gp 之后的产物：已经整理成能直接丢进输出视图的行。
pub struct PkgbuildReport {
    pub name: String,
    pub lines: Vec<String>,
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
    /// 一次性任务（PKGBUILD 这类）用它起**独立**线程，见 Worker::pkgbuild。
    tx: Sender<Response>,
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
        let net_tx = spawn_net(tx.clone(), Arc::clone(&local))?;

        Ok(Self {
            db_tx,
            net_tx,
            rx,
            tx,
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

    /// 取 PKGBUILD：只取原文，或再顺带过一遍 shellcheck 与 namcap。
    ///
    /// **起一条独立线程**跑，这是有意为之的两头不靠：
    /// * 扔在 UI 线程上跑（原来的做法）会把整个界面冻住 —— 没有输出、不能滚动、
    ///   按 q 也不响应；而它偏偏是网络操作，冷连接要几秒；
    /// * 扔进常驻的网络线程则会让同时进行的 AUR 搜索排在它后面。
    pub fn pkgbuild(&self, name: String, check: bool, cwd: PathBuf) {
        let tx = self.tx.clone();
        let _ = thread::Builder::new()
            .name(String::from("pkg-pkgbuild"))
            .spawn(move || {
                let report = pkgbuild_report(&name, check, &cwd);
                let _ = tx.send(Response::Pkgbuild(report));
            });
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
/// 取一份 PKGBUILD，可选顺带检查（shellcheck / namcap）。
///
/// 这里是**唯一**真跑命令的地方；返回的是给人看的行，形状和文件完整性检查
/// 一致，输出视图那边不需要认识 Captured。
///
/// 检查工具没装不算失败：那一段会写明「这一截跳过了」。**检查没做**和
/// **检查没问题**是两件事，不能让前者看起来像后者。
fn pkgbuild_report(name: &str, check: bool, cwd: &Path) -> Result<PkgbuildReport, String> {
    let paru = PathBuf::from("paru");
    let argv = [String::from("-Gp"), name.to_string()];
    let fetched = runtime::run_captured(&paru, &argv, cwd, &format!("PKGBUILD {name}"))
        .map_err(|error| format!("跑不了 paru -Gp：{error}"))?;

    if !fetched.success || fetched.stdout.trim().is_empty() {
        return Err(format!(
            "没拿到 {name} 的 PKGBUILD（它是 AUR 包吗？网络通吗？）"
        ));
    }

    let mut lines = section(&fetched.command, &fetched.stdout);
    if !check {
        return Ok(PkgbuildReport {
            name: name.to_string(),
            lines,
        });
    }

    // 写到临时文件：检查工具要的是文件，不是管道。名字带 pid + 纳秒时间戳，
    // 不给 sticky /tmp 里的符号链接攻击留可预测路径。
    let unique = format!(
        "toolbox-hub-{}-{}-PKGBUILD",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos())
            .unwrap_or_default()
    );
    let path = std::env::temp_dir().join(unique);
    std::fs::write(&path, &fetched.stdout)
        .map_err(|error| format!("写不了临时文件 {}：{error}", path.display()))?;

    for program in ["shellcheck", "namcap"] {
        let argv = [path.display().to_string()];
        match runtime::run_captured(&PathBuf::from(program), &argv, cwd, program) {
            Ok(captured) => {
                // shellcheck 报「有问题」用的是**退出码 1** —— 那是检查成功、有告警，
                // 不是命令失败（不然标题会写成「失败」，误导）。
                let title = match captured.status {
                    Some(1) if program == "shellcheck" => format!("{program}（有告警）"),
                    Some(code) if code != 0 => format!("{program}（退出码 {code}）"),
                    _ => program.to_string(),
                };
                let body = format!("{}{}", captured.stdout, captured.stderr);
                let body = if body.trim().is_empty() {
                    String::from("（没有发现问题）")
                } else {
                    body
                };
                lines.extend(section(&title, &body));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                lines.extend(section(
                    program,
                    &format!("没装 {program}，这一截跳过了。\n装上它：pacman -S {program}"),
                ));
            }
            Err(error) => lines.extend(section(program, &format!("跑不起来：{error}"))),
        }
    }
    // 用完就删：这循环里没有提前返回的路径，删一次就够了。
    let _ = std::fs::remove_file(&path);

    Ok(PkgbuildReport {
        name: name.to_string(),
        lines,
    })
}

/// 输出视图里的一段：一行 `── 标题 ──` 加正文。
fn section(title: &str, body: &str) -> Vec<String> {
    let mut out = vec![format!("── {title} ──")];
    let trimmed = body.trim();
    if trimmed.is_empty() {
        out.push(String::from("（没有输出）"));
    } else {
        out.extend(trimmed.lines().map(str::to_string));
    }
    out
}

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

    /// 输出视图里的一段：标题行 + 正文。空正文必须说「没有输出」，
    /// 不能留一段空白让人以为那一截根本没跑。
    #[test]
    fn section_labels_the_title_and_never_emits_silence() {
        let lines = section("shellcheck", "a\nb\n");
        assert_eq!(lines[0], "── shellcheck ──");
        assert_eq!(&lines[1..], &["a".to_string(), "b".to_string()]);

        let empty = section("namcap", "   \n");
        assert_eq!(empty.len(), 2, "{empty:?}");
        assert!(empty[1].contains("没有输出"), "{empty:?}");
    }

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
            Response::Pkgbuild(_) => "PKGBUILD",
            Response::News(_) => "新闻",
        }
    }
}
