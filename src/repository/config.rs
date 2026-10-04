//! Repository 的配置：有哪些仓库、开没开、谁更优先、信任到什么程度。
//!
//! 存 \`<配置目录>/repositories.toml\`，和 \`packages.toml\` 一个待遇：第一次跑
//! 会写一份**带注释的模板**（不写的话没人知道有它），之后一个字节都不动。
//!
//! \`\`\`toml
//! [[repositories]]
//! id = "official"
//! name = "ToolHub Official"
//! index = "https://example.com/index.json"
//! enabled = true
//! priority = 0
//! trust = "trusted"
//! \`\`\`
//!
//! # 信任是谁给的
//!
//! 信任等级属于**仓库**，不属于包 —— 包会换版本，仓库不会换作者。
//! \`trust\` 只有三种来源：
//!
//! 1. 配置里显式写了 trust 字段；
//! 2. 否则，**随程序附带的官方仓库**（见 \`SHIPPED\`）算 trusted；
//! 3. 其余（用户自己 \`repo add\` 进来的）一律按 community 起步。
//!
//! 这一条很重要：community 不是「坏」，而是「客户端没有依据替它背书」。
//! 界面与 CLI 都必须把这句话说准，不能把「下载来源可信」说成「脚本安全」。

use std::{
    env, fs,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};

use crate::repository::atomic;
use crate::{config::Loaded, repository::paths};

/// 覆盖仓库配置文件位置的环境变量（测试与换机用）。
pub const ENV: &str = "TOOLBOX_HUB_REPOSITORIES";

/// 随程序附带的仓库 id：没写 trust 时它们算 trusted。
pub const SHIPPED: &[&str] = &["official"];

/// 官方索引的位置。它指向本项目的 \`registry/\` 目录（纯静态数据，不是服务端）。
pub const OFFICIAL_INDEX: &str =
    "https://raw.githubusercontent.com/emoeem/toolbox-hub/main/registry/index.json";

/// 一个仓库的信任等级。
///
/// 措辞刻意精确：这些都是**对来源的**判断，不是对脚本行为的判断。
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Trust {
    /// 官方仓库：客户端替它的索引背书。
    Trusted,
    /// 用户或第三方声明「已核对」的仓库。
    Verified,
    /// 社区仓库：能用，但客户端不替它背书。
    Community,
    /// 来源不明（本地路径、非 HTTP(S) 的奇怪 URL）。
    Unknown,
}

impl Trust {
    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_lowercase().as_str() {
            "trusted" | "official" => Some(Trust::Trusted),
            "verified" => Some(Trust::Verified),
            "community" => Some(Trust::Community),
            "unknown" => Some(Trust::Unknown),
            _ => None,
        }
    }

    pub fn id(self) -> &'static str {
        match self {
            Trust::Trusted => "trusted",
            Trust::Verified => "verified",
            Trust::Community => "community",
            Trust::Unknown => "unknown",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Trust::Trusted => "官方",
            Trust::Verified => "已核对",
            Trust::Community => "社区",
            Trust::Unknown => "未知来源",
        }
    }

    /// 界面上紧跟着标签的那句解释。**不能**简化成「安全」。
    pub fn meaning(self) -> &'static str {
        match self {
            Trust::Trusted => "随程序附带的官方索引，客户端替它的来源背书",
            Trust::Verified => "仓库声明已核对来源（客户端无法独立复核）",
            Trust::Community => "第三方仓库，来源可用但客户端不替它背书",
            Trust::Unknown => "来源不明确，只应在你完全清楚它是什么时使用",
        }
    }
}

fn default_true() -> bool {
    true
}

/// 一个仓库。
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RepositoryConfig {
    pub id: String,
    pub name: String,
    /// 索引位置：HTTP(S) URL，或本地路径 / file:// 形式。
    pub index: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// 越小越优先（同名的包取优先级最高的那个仓库）。
    #[serde(default)]
    pub priority: i32,
    #[serde(default)]
    pub trust: Option<String>,
    /// 一句话备注（用户自己加的仓库常写「这是我的」）。
    #[serde(default)]
    pub note: Option<String>,
}

impl RepositoryConfig {
    /// 造一个用户新加的仓库：默认按 community 起步。
    pub fn user_added(name: &str, index: &str) -> Result<Self, String> {
        let id = slug(name);
        Ok(Self {
            id,
            name: name.trim().to_string(),
            index: index.trim().to_string(),
            enabled: true,
            priority: 50,
            trust: None,
            note: None,
        })
    }

    pub fn trust(&self) -> Trust {
        if let Some(declared) = self.trust.as_deref().and_then(Trust::parse) {
            return declared;
        }
        if SHIPPED.contains(&self.id.as_str()) {
            return Trust::Trusted;
        }
        // 本地路径没法「来源可信」—— 它就在你机器上，信任来自你的文件系统权限。
        if self.is_local() {
            return Trust::Unknown;
        }
        Trust::Community
    }

    /// 索引走本地文件系统（不发网络请求）。
    pub fn is_local(&self) -> bool {
        self.index.starts_with("file://") || !self.index.contains("://")
    }

    /// 本地索引对应的路径。
    pub fn local_path(&self) -> Option<PathBuf> {
        if let Some(rest) = self.index.strip_prefix("file://") {
            return Some(PathBuf::from(rest));
        }
        if !self.index.contains("://") {
            return Some(PathBuf::from(&self.index));
        }
        None
    }

    /// 配置自身是否合格（id 能不能当目录名、index 是不是空的）。
    pub fn validate(&self) -> Result<(), String> {
        paths::safe_identifier(&self.id).map_err(|problem| format!("仓库 id 有问题：{problem}"))?;
        if self.name.trim().is_empty() {
            return Err(format!("{}: 缺少 name", self.id));
        }
        if self.index.trim().is_empty() {
            return Err(format!("{}: index 是空的", self.id));
        }
        Ok(())
    }
}

/// 从仓库名造一个 id（\`My Tools\` → my-tools）。
pub fn slug(name: &str) -> String {
    let mut out = String::new();
    let mut last_dash = false;
    for ch in name.trim().chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
            last_dash = false;
        } else if !last_dash && !out.is_empty() {
            out.push('-');
            last_dash = true;
        }
    }
    let trimmed = out.trim_matches('-').to_string();
    if trimmed.is_empty() {
        // 名字全是中文之类：给一个稳定的兜底 id，用户还能自己改文件。
        "repository".to_string()
    } else {
        trimmed
    }
}

/// 一份仓库清单。
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Repositories {
    #[serde(default)]
    pub repositories: Vec<RepositoryConfig>,
}

impl Repositories {
    /// 启用中的仓库，按优先级、再按 id 排（顺序即索引查找顺序）。
    pub fn enabled(&self) -> Vec<&RepositoryConfig> {
        let mut list: Vec<&RepositoryConfig> = self
            .repositories
            .iter()
            .filter(|repo| repo.enabled)
            .collect();
        list.sort_by(|a, b| a.priority.cmp(&b.priority).then_with(|| a.id.cmp(&b.id)));
        list
    }

    pub fn find(&self, id: &str) -> Option<&RepositoryConfig> {
        self.repositories.iter().find(|repo| repo.id == id)
    }

    pub fn find_mut(&mut self, id: &str) -> Option<&mut RepositoryConfig> {
        self.repositories.iter_mut().find(|repo| repo.id == id)
    }

    /// 加一个仓库。id 撞车时改成一个不撞的 id（而不是拒绝 —— 用户多半就是
    /// 想把同一个仓库再加一遍，报错没意义）。
    pub fn add(&mut self, mut config: RepositoryConfig) -> String {
        let base = config.id.clone();
        let mut suffix = 2;
        while self.find(&config.id).is_some() {
            config.id = format!("{base}-{suffix}");
            suffix += 1;
        }
        let id = config.id.clone();
        self.repositories.push(config);
        id
    }

    /// 删掉一个仓库，返回是否删掉了。
    pub fn remove(&mut self, id: &str) -> bool {
        let before = self.repositories.len();
        self.repositories.retain(|repo| repo.id != id);
        self.repositories.len() != before
    }

    /// 开关一个仓库，返回是否找到。
    pub fn set_enabled(&mut self, id: &str, enabled: bool) -> bool {
        match self.find_mut(id) {
            Some(repo) => {
                repo.enabled = enabled;
                true
            }
            None => false,
        }
    }

    /// 按优先级升序排列；同优先级按 id。
    pub fn sorted(&self) -> Vec<&RepositoryConfig> {
        let mut list: Vec<&RepositoryConfig> = self.repositories.iter().collect();
        list.sort_by(|a, b| a.priority.cmp(&b.priority).then_with(|| a.id.cmp(&b.id)));
        list
    }
}

/// 配置文件位置：环境变量优先，否则 \`<配置目录>/repositories.toml\`。
pub fn path() -> PathBuf {
    if let Some(raw) = env::var_os(ENV) {
        return PathBuf::from(raw);
    }
    crate::config::config_dir().join("repositories.toml")
}

pub(crate) fn load_from(file: &Path) -> Loaded<Repositories> {
    let Ok(text) = fs::read_to_string(file) else {
        return Loaded {
            value: Repositories {
                repositories: default_repositories(),
            },
            problem: None,
        };
    };
    match toml::from_str::<Repositories>(&text) {
        Ok(mut parsed) => {
            let mut problems: Vec<String> = Vec::new();
            let mut kept: Vec<RepositoryConfig> = Vec::new();
            for repo in parsed.repositories.drain(..) {
                match repo.validate() {
                    Ok(()) => {
                        if kept.iter().any(|existing| existing.id == repo.id) {
                            problems.push(format!("仓库 id 重复「{}」，只留第一个", repo.id));
                            continue;
                        }
                        kept.push(repo);
                    }
                    Err(problem) => problems.push(problem),
                }
            }
            parsed.repositories = kept;
            Loaded {
                value: parsed,
                problem: (!problems.is_empty()).then(|| problems.join("；")),
            }
        }
        Err(error) => Loaded {
            value: Repositories {
                repositories: default_repositories(),
            },
            problem: Some(format!(
                "{} 读不动（用默认值）：{}",
                file.display(),
                error.message()
            )),
        },
    }
}

pub(crate) fn save_to(file: &Path, repositories: &Repositories) -> Result<(), String> {
    let text = toml::to_string(repositories).map_err(|error| format!("写不了 TOML：{error}"))?;
    atomic::write(file, text.as_bytes())
        .map_err(|error| format!("写不了 {}：{error}", file.display()))
}

/// 第一次跑时写一份带注释的模板出来。已经存在就一个字节都不动。
pub fn ensure_template() -> bool {
    let file = path();
    if file.exists() {
        return false;
    }
    let Some(parent) = file.parent() else {
        return false;
    };
    if fs::create_dir_all(parent).is_err() {
        return false;
    }
    fs::write(&file, TEMPLATE).is_ok()
}

fn default_repositories() -> Vec<RepositoryConfig> {
    vec![RepositoryConfig {
        id: String::from("official"),
        name: String::from("ToolHub Official"),
        index: OFFICIAL_INDEX.to_string(),
        enabled: true,
        priority: 0,
        trust: Some(String::from("trusted")),
        note: None,
    }]
}

/// 模板内容：每一行都是可以改的默认值。
const TEMPLATE: &str = r#"# Toolbox Hub 的仓库清单（Discover / 仓库页）。
#
# 这是**工具仓库**，不是 Arch 的软件包仓库 —— 软件包中心（p 键）那套走
# pacman/AUR，和这里没有关系。这里管的是「工具 / 脚本 / manifest」这类
# 可以搜索、安装、更新、卸载的包。
#
# 删掉某一行 = 用它的默认值；文件整个删掉 = 回到默认（只有官方仓库），
# 下次启动会再写一份。

# id：目录名，只用小写字母、数字、- _ .
# index：索引地址。可以是 https://…，也可以是本地路径或 file://…
# trust：trusted / verified / community / unknown
#        不写的话：官方仓库算 trusted，你自己加的算 community。
# priority：越小越优先（同名包取优先级高的那个仓库）。

[[repositories]]
id = "official"
name = "ToolHub Official"
index = "https://raw.githubusercontent.com/emoeem/toolbox-hub/main/registry/index.json"
enabled = true
priority = 0
trust = "trusted"
"#;

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    fn temp_file(tag: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let dir = env::temp_dir().join(format!("toolbox-hub-repos-{tag}-{nanos}"));
        fs::create_dir_all(&dir).expect("mkdir");
        dir.join("repositories.toml")
    }

    fn repo(id: &str, enabled: bool) -> RepositoryConfig {
        RepositoryConfig {
            id: id.to_string(),
            name: id.to_string(),
            index: format!("https://example.com/{id}/index.json"),
            enabled,
            priority: 0,
            trust: None,
            note: None,
        }
    }

    #[test]
    fn a_missing_file_yields_the_shipped_official_repository() {
        let loaded = load_from(Path::new("/definitely/not/here/repositories.toml"));
        assert!(loaded.problem.is_none(), "缺文件不是问题");
        assert_eq!(loaded.value.repositories.len(), 1);
        let official = &loaded.value.repositories[0];
        assert_eq!(official.id, "official");
        assert_eq!(official.trust(), Trust::Trusted);
        assert!(official.enabled);
    }

    /// 信任的默认规则：官方仓库 trusted，用户自己加的一律 community。
    #[test]
    fn trust_defaults_by_origin_not_by_optimism() {
        assert_eq!(repo("official", true).trust(), Trust::Trusted);
        assert_eq!(repo("random", true).trust(), Trust::Community);

        let mut local = repo("local", true);
        local.index = "/home/emo/tools/index.json".to_string();
        assert_eq!(local.trust(), Trust::Unknown, "本地路径没有来源可信一说");

        let mut declared = repo("random", true);
        declared.trust = Some("verified".to_string());
        assert_eq!(declared.trust(), Trust::Verified);
        // 写错了就退回按来源判断，而不是崩
        declared.trust = Some("乱写".to_string());
        assert_eq!(declared.trust(), Trust::Community);
    }

    #[test]
    fn trust_meanings_never_claim_the_code_is_safe() {
        for trust in [
            Trust::Trusted,
            Trust::Verified,
            Trust::Community,
            Trust::Unknown,
        ] {
            let meaning = trust.meaning();
            assert!(!meaning.contains("安全"), "{meaning}");
            assert!(!meaning.is_empty());
        }
    }

    #[test]
    fn enabling_sorting_and_lookup_work() {
        let mut repositories = Repositories {
            repositories: vec![repo("b", true), repo("a", false), repo("c", true)],
        };
        assert_eq!(
            repositories
                .enabled()
                .iter()
                .map(|r| r.id.as_str())
                .collect::<Vec<_>>(),
            vec!["b", "c"]
        );
        assert!(repositories.find("a").is_some());
        assert!(repositories.find_mut("a").is_some());
        assert!(repositories.set_enabled("a", true));
        assert!(!repositories.set_enabled("不存在", true));
        assert_eq!(repositories.enabled().len(), 3);
    }

    #[test]
    fn priority_decides_the_order_and_ties_break_by_id() {
        let mut low = repo("z", true);
        low.priority = 5;
        let mut high = repo("y", true);
        high.priority = 1;
        let mut same = repo("x", true);
        same.priority = 5;
        let repositories = Repositories {
            repositories: vec![low, high, same],
        };
        assert_eq!(
            repositories
                .sorted()
                .iter()
                .map(|r| r.id.as_str())
                .collect::<Vec<_>>(),
            vec!["y", "x", "z"]
        );
    }

    #[test]
    fn adding_a_duplicate_id_renames_instead_of_failing() {
        let mut repositories = Repositories {
            repositories: vec![repo("mine", true)],
        };
        let id = repositories.add(repo("mine", true));
        assert_eq!(id, "mine-2");
        assert_eq!(repositories.repositories.len(), 2);
        assert_eq!(repositories.repositories[1].name, "mine", "名字不受影响");
    }

    #[test]
    fn removing_reports_whether_anything_went_away() {
        let mut repositories = Repositories {
            repositories: vec![repo("a", true)],
        };
        assert!(repositories.remove("a"));
        assert!(!repositories.remove("a"));
        assert!(repositories.repositories.is_empty());
    }

    #[test]
    fn a_round_trip_keeps_every_field() {
        let file = temp_file("round");
        let repositories = Repositories {
            repositories: vec![repo("mine", false)],
        };
        save_to(&file, &repositories).expect("应能保存");
        let loaded = load_from(&file);
        assert_eq!(loaded.value, repositories);
        assert!(loaded.problem.is_none());
        fs::remove_dir_all(file.parent().expect("parent")).expect("cleanup");
    }

    /// 坏文件不该让程序崩，也不该悄悄变成「没有任何仓库」。
    #[test]
    fn a_broken_file_degrades_to_defaults_with_a_complaint() {
        let file = temp_file("broken");
        fs::write(&file, "这不是 TOML {{{").expect("write");
        let loaded = load_from(&file);
        assert!(loaded.problem.is_some(), "要说出来");
        assert_eq!(loaded.value.repositories.len(), 1, "退回默认官方仓库");
        fs::remove_dir_all(file.parent().expect("parent")).expect("cleanup");
    }

    /// 手写配置里拼错字段名要报出来（deny_unknown_fields）。
    #[test]
    fn a_typo_in_the_field_name_is_reported() {
        let file = temp_file("typo");
        fs::write(
            &file,
            "[[repositories]]\nid = \"a\"\nname = \"A\"\nindex = \"https://x/i.json\"\nprio = 1\n",
        )
        .expect("write");
        let loaded = load_from(&file);
        assert!(loaded.problem.is_some(), "拼错的字段名要被报出来");
        fs::remove_dir_all(file.parent().expect("parent")).expect("cleanup");
    }

    /// id 不合格的仓库条目要被丢掉并说明，其余照常。
    #[test]
    fn an_invalid_repository_is_dropped_with_a_message() {
        let file = temp_file("invalid");
        fs::write(
            &file,
            "[[repositories]]\nid = \"../evil\"\nname = \"Evil\"\nindex = \"https://x/i.json\"\nenabled = true\n\n[[repositories]]\nid = \"good\"\nname = \"Good\"\nindex = \"https://x/g.json\"\nenabled = true\n",
        )
        .expect("write");
        let loaded = load_from(&file);
        assert_eq!(loaded.value.repositories.len(), 1);
        assert_eq!(loaded.value.repositories[0].id, "good");
        assert!(loaded.problem.unwrap().contains("id"));
        fs::remove_dir_all(file.parent().expect("parent")).expect("cleanup");
    }

    #[test]
    fn slug_makes_a_directory_safe_id_out_of_a_name() {
        assert_eq!(slug("My Tools"), "my-tools");
        assert_eq!(slug("  Foo/Bar  "), "foo-bar");
        assert_eq!(slug("A"), "a");
        assert_eq!(slug("x--y"), "x-y");
        // 全是非 ASCII 时给个兜底，不让 id 变成空串
        assert_eq!(slug("我的工具"), "repository");
    }

    #[test]
    fn a_repository_validates_its_own_shape() {
        assert!(repo("good", true).validate().is_ok());
        assert!(repo("../evil", true).validate().is_err());
        let mut empty_index = repo("a", true);
        empty_index.index = "   ".to_string();
        assert!(empty_index.validate().is_err());
        let mut no_name = repo("a", true);
        no_name.name = String::new();
        assert!(no_name.validate().is_err());
    }

    #[test]
    fn user_added_repositories_start_enabled_and_community() {
        let config = RepositoryConfig::user_added("My Tools", "https://x/i.json").expect("ok");
        assert_eq!(config.id, "my-tools");
        assert!(config.enabled);
        assert_eq!(config.trust(), Trust::Community);
        assert!(config.validate().is_ok());
    }
}
