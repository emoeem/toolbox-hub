//! libalpm 直连：搜索、包信息、已安装列表、可更新数、依赖索引。
//!
//! ## 为什么不再解析 `pacman` 的文本
//!
//! 同一台机器上实测（38,860 个同步包 / 2,271 个本地包）：
//!
//! | 要做的事 | 起进程 + 解析文本 | libalpm 直连 |
//! | --- | --- | --- |
//! | 打开库 + 注册 12 个仓库 | — | 1.4 ms |
//! | **第一次触碰同步库**（怎么问都一样） | — | **~365 ms** |
//! | 之后同一进程里再问 | — | ~3 ms |
//! | 全库找 `fzf` | `pacman -Ss` 450 ms | 365 ms（冷）+ 结构化数据 |
//! | 「有多少可更新」 | `checkupdates` **18.8 s** | 365 ms |
//! | 已安装列表（含外来/孤儿） | `-Q -Qe -Qm -Qtdq` 730 ms | ~400 ms |
//! | 版本比较 | 字符串比（**方向会错**） | pacman 自己的 vercmp |
//!
//! 关于那 365 ms：libalpm 第一次访问同步库时会把整个 `.db` 解析出来，之后全都在
//! 内存里。**别被「19 ms」那种数字骗了** —— 那是同一个进程里第二次问的结果
//! （我自己就先被自己骗过一次，所以把两种数都写在这儿）。
//!
//! 真正省下来的不是那几十毫秒，而是三件事：
//!
//! 1. `checkupdates` 的 18.8 秒（它每次都重新下载数据库）；
//! 2. 四个 `pacman -Q*` 进程加它们的文本解析；
//! 3. **正确性**：vercmp 是 pacman 自己的，字符串比会把「已装 0.74.4-1.1」
//!    和「仓库里的 0.74.4-1」的升级方向搞反。
//!
//! 还有一件：拿到的是**结构化**数据 —— 下载/安装体积、依赖、provides、构建日期……
//! 不用再从 `pacman -Sii` 的 `Key : Value` 文本里把它们抠出来。
//!
//! ## 线程约束（很重要）
//!
//! libalpm 句柄**不能跨线程共享**（crate 没给它 `Send`），所以每个后台线程各自
//! [`open`] 一个。打开 + 注册确实只要 1.4 ms，但**那 365 ms 的首次解析每个线程
//! 都要重付一次** —— 想摊薄它就得让句柄活得久一点（一个常驻线程反复用）。
//!
//! ## 孤儿包为什么自己算
//!
//! pacman 的判定是「装的时候是依赖 && 没人需要它」，而「没人需要」要连 provides
//! 一起算。libalpm 的 `Package::required_by()` 能算，但**逐包调用合计 245ms**
//! （每次都要扫一遍本地库）；自己走一遍 14,266 条依赖边只要 **195µs**，快 1200 倍。
//! 判定规则与 `pacman -Qtd` 对齐，另有 `#[ignore]` 冒烟测试拿真 pacman 对账。

use std::collections::{BTreeSet, HashMap, HashSet};

use ::alpm::{Alpm, Db, LogLevel, Package, PackageReason, SigLevel};

use super::{InstalledPackage, InstalledState, PackageHit};

/// pacman 的版本序：`a` 比 `b` 新吗（`Ver` 只实现了 `PartialOrd`，包一层好读）。
fn newer(a: &::alpm::Ver, b: &::alpm::Ver) -> bool {
    a.partial_cmp(b) == Some(std::cmp::Ordering::Greater)
}

/// pacman 的数据库目录（与默认 `pacman.conf` 的 `DBPath` 一致）。
const DB_PATH: &str = "/var/lib/pacman";
/// 仓库名与顺序（= 优先级）都从这里读。
const CONF_PATH: &str = "/etc/pacman.conf";

// ── 打开与仓库 ──────────────────────────────────────────────────────────────

/// 从 pacman.conf 里取仓库名，**顺序就是优先级**。
///
/// libalpm 的 `initialize` 不会替你读 pacman.conf：同步库要一个个
/// `register_syncdb` 注册，而顺序决定了「同一个包在多个仓库里出现时以谁为准」。
/// 只认 `[段名]`（`[options]` 不是仓库），不管后面的键值 —— 读库不需要它们。
pub fn repos_from_conf(text: &str) -> Vec<String> {
    text.lines()
        .filter_map(|line| {
            let line = line.trim();
            let name = line.strip_prefix('[')?.strip_suffix(']')?.trim();
            (!name.is_empty() && !name.eq_ignore_ascii_case("options")).then(|| name.to_string())
        })
        .collect()
}

/// 只注册**真的有 .db 文件**的仓库。
///
/// 两个原因：pacman.conf 里可能有本机没同步过的仓库（注册会报错刷日志），
/// 而没同步就意味着没有数据，注册了也没用。
fn register_repos(handle: &Alpm, sync_dir: &std::path::Path) -> usize {
    let conf = std::fs::read_to_string(CONF_PATH).unwrap_or_default();
    let mut registered = 0;

    // pacman.conf 读不到（不常见）就退回「同步目录里有什么用什么」，
    // 顺序按文件名 —— 至少还能用，只是仓库优先级不再权威。
    let mut names = repos_from_conf(&conf);
    if names.is_empty() {
        let mut from_dir: Vec<String> = std::fs::read_dir(sync_dir)
            .map(|entries| {
                entries
                    .flatten()
                    .filter_map(|entry| {
                        let name = entry.file_name().to_string_lossy().to_string();
                        name.strip_suffix(".db").map(str::to_string)
                    })
                    .collect()
            })
            .unwrap_or_default();
        from_dir.sort();
        names = from_dir;
    }

    for name in names {
        if !sync_dir.join(format!("{name}.db")).is_file() {
            continue;
        }
        if handle
            .register_syncdb(name.as_str(), SigLevel::NONE)
            .is_ok()
        {
            registered += 1;
        }
    }
    registered
}

/// 打开一个句柄（本地库 + 同步库）。
pub fn open() -> Result<Alpm, String> {
    let handle =
        Alpm::new("/", DB_PATH).map_err(|error| format!("打不开 pacman 数据库：{error}"))?;

    // 把 libalpm 的日志接走：TUI 里它往 stderr 写一行就是花屏。
    // （实测注册不存在的仓库不会写，但别的错误路径会 —— 不值得赌。）
    handle.set_log_cb((), |_level: LogLevel, _message: &str, _sink: &mut ()| {});

    register_repos(
        &handle,
        std::path::Path::new(DB_PATH).join("sync").as_path(),
    );
    Ok(handle)
}

// ── 「谁需要谁」索引 ────────────────────────────────────────────────────────

/// 一次扫出来的依赖索引：谁被谁需要（`depends`）与谁被谁可选依赖（`optdepends`）。
///
/// 这是替代 `pacman -Qi` 里 `Required By` / `Optional For` 两行的东西，
/// 也是孤儿判定的依据。解析 provides 的连字符版本（`libfoo.so=1-64`）只取名字段。
#[derive(Debug, Default)]
pub struct Demand {
    required_by: HashMap<String, Vec<String>>,
    optional_for: HashMap<String, Vec<String>>,
}

impl Demand {
    pub fn required_by(&self, name: &str) -> &[String] {
        self.required_by
            .get(name)
            .map(Vec::as_slice)
            .unwrap_or_default()
    }

    pub fn optional_for(&self, name: &str) -> &[String] {
        self.optional_for
            .get(name)
            .map(Vec::as_slice)
            .unwrap_or_default()
    }

    /// 一次扫完本地库里的全部依赖边（14k 条边 ≈ 0.2ms）。
    pub fn scan(local: &Db) -> Self {
        // 名字 → 包，以及「被 provides 出来的名字 → 提供它的包」
        let mut by_name: HashMap<&str, &Package> = HashMap::new();
        // key 必须 owned：provides 的条目是拼出来的字符串，借不得（原来这里悬垂过）
        let mut providers: HashMap<String, Vec<&str>> = HashMap::new();
        for pkg in local.pkgs() {
            by_name.insert(pkg.name(), pkg);
            for provided in pkg.provides() {
                providers
                    .entry(dep_name(provided.to_string().as_str()).to_string())
                    .or_default()
                    .push(pkg.name());
            }
        }

        let mut demand = Demand::default();
        for pkg in local.pkgs() {
            for dep in pkg.depends() {
                demand.add(
                    &by_name,
                    &providers,
                    dep_name(dep.to_string().as_str()),
                    pkg.name(),
                    false,
                );
            }
            for dep in pkg.optdepends() {
                demand.add(
                    &by_name,
                    &providers,
                    dep_name(dep.to_string().as_str()),
                    pkg.name(),
                    true,
                );
            }
        }
        demand
    }

    fn add(
        &mut self,
        by_name: &HashMap<&str, &Package>,
        providers: &HashMap<String, Vec<&str>>,
        dep: &str,
        who: &str,
        optional: bool,
    ) {
        let table = if optional {
            &mut self.optional_for
        } else {
            &mut self.required_by
        };
        let mut add_one = |target: &str| {
            let list = table.entry(target.to_string()).or_default();
            if !list.iter().any(|existing| existing == who) {
                list.push(who.to_string());
            }
        };

        if by_name.contains_key(dep) {
            add_one(dep);
        }
        // 依赖也可能靠 provides 满足（`sh`、`libreadline.so` 这类）
        if let Some(names) = providers.get(dep) {
            for name in names {
                add_one(name);
            }
        }
    }
}

/// 依赖串里的名字：
///
/// * `libreadline.so=8-64` → `libreadline.so`（版本约束）
/// * `foo>=1.2` → `foo`
/// * `python-mutagen: 扩展标签支持` → `python-mutagen`（**optdepends 带说明**）
///
/// 最后那条是实拍踩出来的：不切冒号的话，可选依赖的索引键会变成一整句
/// 「python-mutagen: 扩展标签支持」，查 `python-mutagen` 永远查不到 ——
/// 表现就是孤儿判定把「别人的可选依赖」误报成孤儿（pacman 说 0 个，这儿报 8 个）。
pub fn dep_name(dep: &str) -> &str {
    let end = dep
        .find(['=', '<', '>', ':', ' ', '\t'])
        .unwrap_or(dep.len());
    dep[..end].trim()
}

// ── 事实（结构化数据）与纯逻辑 ──────────────────────────────────────────────

/// 从 libalpm 抠出来的一个包的「事实」。
///
/// 有了它，字段拼装、状态标记、孤儿判定这些**全是纯函数**，可以在测试里钉住；
/// FFI 那一层就只剩下「怎么把事实取出来」，薄得不用测。
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Facts {
    pub repo: String,
    pub name: String,
    pub version: String,
    pub description: String,
    pub url: String,
    pub arch: String,
    pub licenses: Vec<String>,
    pub groups: Vec<String>,
    pub provides: Vec<String>,
    pub depends: Vec<String>,
    pub optional_deps: Vec<String>,
    pub required_by: Vec<String>,
    pub optional_for: Vec<String>,
    pub conflicts: Vec<String>,
    pub replaces: Vec<String>,
    pub download_size: Option<u64>,
    pub installed_size: Option<u64>,
    pub packager: String,
    pub build_date: Option<u64>,
    pub install_date: Option<u64>,
    /// 本地已装版本（没装就是 `None`）。
    pub installed_version: Option<String>,
    /// 已装版本与当前版本的比较结果（pacman 的 vercmp 说了算）。
    pub state: InstalledState,
    /// 装的时候是「自己点名」还是「被依赖拖进来」（没装就是 `None`）。
    pub explicit: Option<bool>,
}

/// `2030.16 KiB` 这种给人看的体积（pacman 也是这么写的）。
pub fn size_text(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KiB", "MiB", "GiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.2} {}", UNITS[unit])
    }
}

fn list_text(items: &[String]) -> Option<String> {
    (!items.is_empty()).then(|| items.join("  "))
}

/// 把事实拼成信息面板的 `Key : Value` 列表（顺序照着 paru / pacman -Sii 的习惯）。
///
/// 空字段直接不出现 —— 面板上不该有一堆 `None`。
pub fn info_fields(facts: &Facts) -> Vec<(String, String)> {
    let mut fields: Vec<(String, String)> = vec![
        ("Repository".into(), facts.repo.clone()),
        ("Name".into(), facts.name.clone()),
        ("Version".into(), facts.version.clone()),
        ("Description".into(), facts.description.clone()),
        ("Architecture".into(), facts.arch.clone()),
        ("URL".into(), facts.url.clone()),
    ];

    let optional = |key: &str, value: Option<String>| value.map(|value| (key.to_string(), value));

    fields.extend(optional("Licenses", list_text(&facts.licenses)));
    fields.extend(optional("Groups", list_text(&facts.groups)));
    fields.extend(optional("Provides", list_text(&facts.provides)));
    fields.extend(optional("Depends On", list_text(&facts.depends)));
    fields.extend(optional("Optional Deps", list_text(&facts.optional_deps)));
    // 卸载安全就靠这两行：谁需要它
    fields.extend(optional("Required By", list_text(&facts.required_by)));
    fields.extend(optional("Optional For", list_text(&facts.optional_for)));
    fields.extend(optional("Conflicts With", list_text(&facts.conflicts)));
    fields.extend(optional("Replaces", list_text(&facts.replaces)));
    fields.extend(
        facts
            .download_size
            .map(|size| ("Download Size".into(), size_text(size))),
    );
    fields.extend(
        facts
            .installed_size
            .map(|size| ("Installed Size".into(), size_text(size))),
    );
    fields.extend(optional(
        "Packager",
        (!facts.packager.is_empty()).then(|| facts.packager.clone()),
    ));
    fields.extend(
        facts
            .build_date
            .map(|stamp| ("Build Date".into(), super::format_epoch(stamp))),
    );
    fields.extend(
        facts
            .install_date
            .map(|stamp| ("Install Date".into(), super::format_epoch(stamp))),
    );
    fields.extend(
        facts
            .installed_version
            .clone()
            .map(|version| ("Installed".into(), version)),
    );
    fields.extend(facts.explicit.map(|explicit| {
        (
            "Install Reason".into(),
            String::from(if explicit {
                "Explicitly installed"
            } else {
                "Installed as a dependency"
            }),
        )
    }));
    fields.extend(facts.state.label().map(|label| ("Status".into(), label)));

    fields.retain(|(_, value)| !value.trim().is_empty());
    fields
}

/// 一个本地包是不是孤儿：装的时候是依赖，而且没人需要它（含可选依赖）。
///
/// 与 `pacman -Qtd` 一致 —— 差别只在「可选依赖也算有人要」这一条，
/// 实拍对账过：只看 `required_by` 会多报 8 个（都是别人的 optdepends）。
pub fn is_orphan(explicit: bool, required_by: usize, optional_for: usize) -> bool {
    !explicit && required_by == 0 && optional_for == 0
}

/// 组装「已安装浏览器」里的一行。
///
/// 只吃四个事实（名字、版本、安装原因、是否外来）+ 两个计数（有几个需要它、
/// 有几个可选依赖它），所以它既能被 [`installed`] 用，也能在测试里单独钉住 ——
/// 不用为了测「孤儿标记」而去造一整个 [`Facts`]。
pub fn installed_row(
    name: &str,
    version: &str,
    explicit: bool,
    foreign: bool,
    required_by: usize,
    optional_for: usize,
) -> InstalledPackage {
    InstalledPackage {
        name: name.to_string(),
        version: version.to_string(),
        explicit,
        foreign,
        orphan: is_orphan(explicit, required_by, optional_for),
    }
}

// ── 对外功能 ────────────────────────────────────────────────────────────────

/// 本地已装包的名字 → 包（一次扫完；搜索标状态、信息面板补本地字段都要它）。
fn local_packages(local: &Db) -> HashMap<&str, &Package> {
    local
        .pkgs()
        .iter()
        .map(|package| (package.name(), package))
        .collect()
}

/// 官方源搜索。
///
/// 语义与 `pacman -Ss` 接近但不完全一样：**子串匹配**（不分大小写），
/// 不认正则。用户顺手写的 `^fzf$` 会被当成「去掉锚点的词」处理 ——
/// 中心里的排序本来就把完全同名的排最前，锚点没必要。
///
/// 同一个包在多个仓库里出现时**只留优先级最高**的那个（pacman.conf 的顺序），
/// 这样 `cachyos-extra-v3/fzf` 与 `extra/fzf` 不会并排出现两条。
pub fn search(term: &str) -> Result<Vec<PackageHit>, String> {
    let handle = open()?;
    let local = local_packages(handle.localdb());
    let needle = term
        .trim()
        .trim_start_matches('^')
        .trim_end_matches('$')
        .to_lowercase();
    if needle.is_empty() {
        return Ok(Vec::new());
    }

    let mut hits = Vec::new();
    let mut claimed: HashSet<&str> = HashSet::new();

    for db in handle.syncdbs() {
        for package in db.pkgs() {
            // 仓库优先级：名字被前面的仓库占住就不再考虑
            if !claimed.insert(package.name()) {
                continue;
            }
            let name_hit = package.name().contains(needle.as_str());
            let desc_hit = package
                .desc()
                .is_some_and(|desc| desc.to_lowercase().contains(needle.as_str()));
            if !name_hit && !desc_hit {
                continue;
            }

            let installed = local.get(package.name()).copied();
            hits.push(PackageHit {
                repo: db.name().to_string(),
                name: package.name().to_string(),
                version: package.version().to_string(),
                description: package.desc().unwrap_or_default().to_string(),
                installed_state: state_of(
                    package.version(),
                    installed.map(|local| local.version()),
                ),
                votes: None,
                popularity: None,
                maintainer: None,
                out_of_date: false,
            });
        }
    }

    Ok(hits)
}

/// 官方源（或本地已装）包的信息面板。
///
/// 先找同步库（用户多半在看「能装的那个」），找不到再看本地库 ——
/// 外来包（AUR / 手工装的）在同步库里根本不存在，但它的信息仍然值得看。
pub fn info(name: &str) -> Result<Vec<(String, String)>, String> {
    let handle = open()?;
    let demand = Demand::scan(handle.localdb());
    let local = local_packages(handle.localdb());

    let found = handle.syncdbs().iter().find_map(|db| {
        db.pkg(name)
            .ok()
            .map(|package| (db.name().to_string(), package))
    });

    let facts = match found {
        Some((repo, package)) => {
            facts_from(package, &repo, local.get(package.name()).copied(), &demand)
        }
        None => {
            let package = handle
                .localdb()
                .pkg(name)
                .map_err(|error| format!("{name}：{error}"))?;
            facts_from(package, "local", Some(package), &demand)
        }
    };

    let fields = info_fields(&facts);
    if fields.is_empty() {
        return Err(format!("{name} 没有任何可显示的字段"));
    }
    Ok(fields)
}

/// 已安装列表（显式 / 依赖 / 外来 / 孤儿）。
pub fn installed() -> Result<Vec<InstalledPackage>, String> {
    let handle = open()?;
    let demand = Demand::scan(handle.localdb());
    let sync_names = sync_names(&handle);

    // 这一条路径**刻意不走 `facts_from`**：那张表会为一个包取十几个字段
    // （依赖、provides、许可证……），而列表只需要名字、版本、安装原因和孤儿标记。
    // 实测 2271 个包：走完整 Facts 是 0.56s，这样是几十毫秒 —— 区别全在
    // 「每个包都要 libalpm 现场解析一遍条目」上。
    let mut rows: Vec<InstalledPackage> = handle
        .localdb()
        .pkgs()
        .iter()
        .map(|package| {
            let name = package.name();
            installed_row(
                name,
                package.version().as_ref(),
                package.reason() == PackageReason::Explicit,
                !sync_names.contains(name),
                demand.required_by(name).len(),
                demand.optional_for(name).len(),
            )
        })
        .collect();
    rows.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(rows)
}

/// 有多少个包可以更新 —— `checkupdates` 的替代品。
///
/// `None` 表示**不知道**（数据库打不开），不是「0 个」：
/// 把「查不了」说成「没有更新」是实打实的谎报。
pub fn pending_updates() -> Option<usize> {
    let handle = open().ok()?;
    let local = local_packages(handle.localdb());
    let mut claimed: HashSet<&str> = HashSet::new();
    let mut count = 0;
    for db in handle.syncdbs() {
        for package in db.pkgs() {
            if !claimed.insert(package.name()) {
                continue;
            }
            if let Some(installed) = local.get(package.name())
                && newer(package.version(), installed.version())
            {
                count += 1;
            }
        }
    }
    Some(count)
}

/// 卸载这些包会连累谁（替代原来「一个包起一次 `pacman -Qi`」）。
pub fn removal_report(names: &[String]) -> Vec<String> {
    let Ok(handle) = open() else {
        return Vec::new();
    };
    let demand = Demand::scan(handle.localdb());
    let asked: HashSet<&str> = names.iter().map(String::as_str).collect();

    let mut dependents: BTreeSet<String> = BTreeSet::new();
    let mut optional: BTreeSet<String> = BTreeSet::new();
    for name in names {
        for who in demand.required_by(name) {
            if !asked.contains(who.as_str()) {
                dependents.insert(who.clone());
            }
        }
        for who in demand.optional_for(name) {
            if !asked.contains(who.as_str()) {
                optional.insert(who.clone());
            }
        }
    }

    let mut lines = Vec::new();
    if !dependents.is_empty() {
        lines.push(format!("依赖它们的有 {}", sample(&dependents)));
    }
    if !optional.is_empty() {
        lines.push(format!("可选依赖它们的有 {}", sample(&optional)));
    }
    lines
}

/// 队列里这些包一共要下载多少字节（同步库里查得到的才算）。
pub fn download_total(names: &[String]) -> Option<u64> {
    let handle = open().ok()?;
    let mut claimed: HashSet<&str> = HashSet::new();
    let mut wanted: HashMap<&str, u64> = HashMap::new();
    for db in handle.syncdbs() {
        for package in db.pkgs() {
            if !claimed.insert(package.name()) || !names.iter().any(|name| name == package.name()) {
                continue;
            }
            wanted.insert(package.name(), package.download_size().max(0) as u64);
        }
    }
    (!wanted.is_empty()).then(|| wanted.values().sum())
}

fn sample(items: &BTreeSet<String>) -> String {
    let preview: Vec<&str> = items.iter().take(10).map(String::as_str).collect();
    format!(
        "{} 个：{}{}",
        items.len(),
        preview.join("  "),
        if items.len() > preview.len() {
            " …"
        } else {
            ""
        }
    )
}

fn sync_names(handle: &Alpm) -> HashSet<&str> {
    let mut names = HashSet::new();
    for db in handle.syncdbs() {
        for package in db.pkgs() {
            names.insert(package.name());
        }
    }
    names
}

/// 「仓库里这个版本」与「本地那个版本」的关系（没装就是 `NotInstalled`）。
fn state_of(repo: &::alpm::Ver, installed: Option<&::alpm::Ver>) -> InstalledState {
    match installed {
        Some(installed) => InstalledState::from_ordering(
            repo.partial_cmp(installed)
                .unwrap_or(std::cmp::Ordering::Equal),
            installed.to_string(),
        ),
        None => InstalledState::NotInstalled,
    }
}

/// 同步库有多旧（最新的那个 `.db` 文件的 mtime 到现在）。
///
/// 为什么要暴露这个：**「有多少可更新」是按本地数据库算的**（和 `pacman -Qu`
/// 同一口径），数据库旧了这个数就偏小。`checkupdates` 之所以更「准」，是因为它
/// 每次都重新下载数据库 —— 代价就是那 18 秒。库旧了就该说出来，而不是给一个
/// 看着很确定的数字。
pub fn sync_db_age() -> Option<std::time::Duration> {
    let dir = std::path::Path::new(DB_PATH).join("sync");
    let newest = std::fs::read_dir(dir)
        .ok()?
        .flatten()
        .filter(|entry| entry.file_name().to_string_lossy().ends_with(".db"))
        .filter_map(|entry| entry.metadata().ok()?.modified().ok())
        .max()?;
    newest.elapsed().ok()
}

/// 孤儿包名单（给「清孤儿」用；`installed()` 里已经算好了，这里只是挑出来）。
pub fn orphan_names() -> Vec<String> {
    installed()
        .map(|packages| {
            packages
                .into_iter()
                .filter(|package| package.orphan)
                .map(|package| package.name)
                .collect()
        })
        .unwrap_or_default()
}

/// 把 libalpm 的包对象翻成 [`Facts`]（这一层之外就全是纯逻辑了）。
fn facts_from(package: &Package, repo: &str, local: Option<&Package>, demand: &Demand) -> Facts {
    let strings = |items: Vec<String>| items;

    let installed_version = local.map(|package| package.version().to_string());
    let state = state_of(package.version(), local.map(|local| local.version()));

    // 没装的时候，这两个索引查的是「本地库里谁需要它」—— 照样有意义：
    // 你要装的包，正是别人已经在用的那个。
    let required_by = strings(demand.required_by(package.name()).to_vec());
    let optional_for = strings(demand.optional_for(package.name()).to_vec());

    Facts {
        repo: repo.to_string(),
        name: package.name().to_string(),
        version: package.version().to_string(),
        description: package.desc().unwrap_or_default().to_string(),
        url: package.url().unwrap_or_default().to_string(),
        arch: package.arch().unwrap_or_default().to_string(),
        licenses: package
            .licenses()
            .iter()
            .map(|item| item.to_string())
            .collect(),
        groups: package
            .groups()
            .iter()
            .map(|item| item.to_string())
            .collect(),
        provides: package
            .provides()
            .iter()
            .map(|item| item.to_string())
            .collect(),
        depends: package
            .depends()
            .iter()
            .map(|item| item.to_string())
            .collect(),
        optional_deps: package
            .optdepends()
            .iter()
            .map(|item| item.to_string())
            .collect(),
        required_by,
        optional_for,
        conflicts: package
            .conflicts()
            .iter()
            .map(|item| item.to_string())
            .collect(),
        replaces: package
            .replaces()
            .iter()
            .map(|item| item.to_string())
            .collect(),
        download_size: (package.download_size() > 0).then(|| package.download_size() as u64),
        installed_size: (package.isize() > 0).then(|| package.isize() as u64),
        packager: package.packager().unwrap_or_default().to_string(),
        build_date: (package.build_date() > 0).then(|| package.build_date() as u64),
        install_date: package.install_date().map(|stamp| stamp as u64),
        installed_version,
        state,
        explicit: local.map(|package| package.reason() == PackageReason::Explicit),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 真实 pacman.conf 的形状（含 options 段、注释、Include）。
    const CONF: &str = "\
# /etc/pacman.conf
[options]
HoldPkg = pacman glibc

[cachyos-v3]
Include = /etc/pacman.d/cachyos-v3-mirrorlist

[core]
Include = /etc/pacman.d/mirrorlist

[extra]
[multilib]
[archlinuxcn]
";

    #[test]
    fn repos_come_from_the_conf_in_priority_order() {
        let repos = repos_from_conf(CONF);
        assert_eq!(
            repos,
            vec!["cachyos-v3", "core", "extra", "multilib", "archlinuxcn"],
            "顺序必须保持（它就是仓库优先级），options 段不算仓库"
        );
        assert!(repos_from_conf("").is_empty());
        assert!(repos_from_conf("[OPTIONS]").is_empty(), "大小写不敏感");
    }

    /// 依赖串里的名字：连字符版本号要切掉，否则 provides 永远匹配不上。
    #[test]
    fn dep_names_strip_version_constraints() {
        assert_eq!(dep_name("readline"), "readline");
        assert_eq!(dep_name("libreadline.so=8-64"), "libreadline.so");
        assert_eq!(dep_name("foo>=1.2"), "foo");
        assert_eq!(dep_name("bar<2.0"), "bar");
        // optdepends 带说明：冒号后面那一串不能进索引键
        assert_eq!(dep_name("python-mutagen: 扩展标签支持"), "python-mutagen");
        assert_eq!(dep_name("foo  "), "foo");
    }

    #[test]
    fn sizes_are_human_readable() {
        assert_eq!(size_text(512), "512 B");
        assert_eq!(size_text(1536), "1.50 KiB");
        assert_eq!(size_text(2030 * 1024), "1.98 MiB");
        assert_eq!(size_text(5 * 1024 * 1024 * 1024), "5.00 GiB");
    }

    /// 孤儿判定：可选依赖也算「有人要」—— 这条实拍对账过（差 8 个）。
    #[test]
    fn orphan_rule_matches_pacman() {
        assert!(is_orphan(false, 0, 0), "依赖装的、没人要 = 孤儿");
        assert!(!is_orphan(false, 3, 0), "有人依赖就不是孤儿");
        assert!(!is_orphan(false, 0, 1), "只是别人的可选依赖，也不算孤儿");
        assert!(!is_orphan(true, 0, 0), "自己点名装的不算孤儿");
    }

    fn facts() -> Facts {
        Facts {
            repo: "extra".into(),
            name: "fzf".into(),
            version: "0.74.4-1".into(),
            description: "Command-line fuzzy finder".into(),
            url: "https://github.com/junegunn/fzf".into(),
            arch: "x86_64".into(),
            licenses: vec!["MIT".into()],
            depends: vec!["glibc".into(), "bash".into()],
            required_by: vec!["toolbox-hub".into()],
            download_size: Some(2_030 * 1024),
            installed_size: Some(5_742_039),
            packager: "Someone <a@b.c>".into(),
            build_date: Some(1_760_000_000),
            installed_version: Some("0.74.4-1.1".into()),
            state: InstalledState::Newer("0.74.4-1.1".into()),
            explicit: Some(true),
            ..Facts::default()
        }
    }

    /// 空字段不该出现在面板上（否则满屏 `None`）。
    #[test]
    fn info_fields_skip_empty_values() {
        let fields = info_fields(&facts());
        let get = |key: &str| {
            fields
                .iter()
                .find(|(name, _)| name == key)
                .map(|(_, value)| value.clone())
        };

        assert_eq!(get("Repository").as_deref(), Some("extra"));
        assert_eq!(get("Depends On").as_deref(), Some("glibc  bash"));
        assert_eq!(get("Required By").as_deref(), Some("toolbox-hub"));
        assert_eq!(get("Download Size").as_deref(), Some("1.98 MiB"));
        assert_eq!(get("Installed Size").as_deref(), Some("5.48 MiB"));
        assert_eq!(
            get("Install Reason").as_deref(),
            Some("Explicitly installed")
        );
        assert_eq!(get("Groups"), None, "没有的字段不该出现");
        assert_eq!(get("Conflicts With"), None);

        // 面板顺序：体积和「谁需要它」都要在，且不能出现空值
        assert!(fields.iter().all(|(_, value)| !value.trim().is_empty()));
    }

    /// 已安装列表那一行的组装（显式/依赖/外来/孤儿）。
    #[test]
    fn installed_rows_carry_reason_and_orphan_flag() {
        let orphan = installed_row("stale-lib", "0.1-1", false, true, 0, 0);
        assert!(!orphan.explicit);
        assert!(orphan.foreign);
        assert!(orphan.orphan, "依赖装的且没人要 = 孤儿");
        assert_eq!(orphan.tag(), "孤儿");

        let needed = installed_row("readline", "8.2-1", false, false, 42, 0);
        assert!(!needed.orphan);
        assert_eq!(needed.tag(), "依赖");

        let optional = installed_row("python-mutagen", "1.0-1", false, false, 0, 1);
        assert!(!optional.orphan, "只是别人的可选依赖，不算孤儿");

        let explicit = installed_row("fzf", "0.74.4-1", true, false, 0, 0);
        assert_eq!(explicit.tag(), "显式");
    }
}
