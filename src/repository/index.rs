//! Repository 索引（index）的格式与解析。
//!
//! 索引是一个**静态 JSON 文件**，由仓库作者维护，客户端只读。它必须在
//! 「展示安装计划之前」就带够信息：来源、作者、License、依赖、要装哪些文件、
//! 以及可选的 SHA-256。这样用户在**下载任何东西之前**就能看到自己要装什么。
//!
//! \`\`\`json
//! {
//!   "schema_version": 1,
//!   "packages": [
//!     {
//!       "id": "sing-box-tools",
//!       "name": "sing-box 运维工具",
//!       "version": "1.4.0",
//!       "summary": "本机代理的体检 / 审计 / 切换",
//!       "categories": ["network"],
//!       "tags": ["sing-box", "proxy"],
//!       "license": "MIT",
//!       "source": "https://github.com/example/sing-box-tools",
//!       "dependencies": [{ "command": "sing-box" }, { "command": "jq" }],
//!       "requires_root": false,
//!       "danger": "safe",
//!       "artifact": {
//!         "url": "https://example.com/sing-box-tools-1.4.0.tar.gz",
//!         "sha256": "…64 位十六进制…",
//!         "kind": "tar.gz"
//!       },
//!       "files": [
//!         { "path": "scripts/sing-box-audit", "kind": "bin" },
//!         { "path": "manifests/sing-box.toml", "kind": "manifest" }
//!       ]
//!     }
//!   ]
//! }
//! \`\`\`
//!
//! # 两条刻意的取舍
//!
//! 1. **不拒绝未知字段**（和项目里手写配置的做法相反）。索引是**远端**格式，
//!    作者加一个新字段不该让老客户端整份索引作废。未知字段会被收集成警告
//!    （方便发现自己拼错了字段名），但**不影响解析**。
//! 2. \`schema_version\` 不认识时**明确报错**，不 panic、不猜、不半读半不读。
//!    用户会看到「这个仓库的 schema v2，本版本只认 v1」。

use std::{
    collections::{BTreeMap, HashSet},
    path::PathBuf,
};

use serde::{Deserialize, Serialize};

use crate::{model::Danger, repository::paths, repository::version::Version};

/// 本客户端支持的索引 schema 版本。
pub const SCHEMA_VERSION: u32 = 1;

/// 一份解析好的索引 + 解析时顺手记下的问题。
#[derive(Clone, Debug)]
pub struct ParsedIndex {
    pub index: Index,
    pub warnings: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Index {
    pub schema_version: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// 索引自己声明的更新时间（只用于展示，不参与判断新鲜度）。
    ///
    /// 构建时刻意**不写**它：写进去等于每次构建都产出一个不同的索引，
    /// 可复现就没了。要盖时间戳用 build --stamp。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated: Option<String>,
    #[serde(default)]
    pub packages: Vec<PackageMeta>,
    /// 未知的顶层字段：留个记录，不报错。
    #[serde(flatten)]
    pub extra: BTreeMap<String, serde_json::Value>,
}

/// 索引里的一个包。
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct PackageMeta {
    pub id: String,
    pub name: String,
    pub version: String,
    #[serde(default)]
    pub summary: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub categories: Vec<String>,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub author: Option<String>,
    #[serde(default)]
    pub license: Option<String>,
    /// 包的来源（仓库地址、作者主页…）。展示给用户看「这东西从哪来」。
    #[serde(default)]
    pub source: Option<String>,
    #[serde(default)]
    pub homepage: Option<String>,
    #[serde(default)]
    pub dependencies: Vec<Dependency>,
    /// 声明「装了它要动系统目录」。当前版本**不会**自动提权，见 install 模块。
    #[serde(default)]
    pub requires_root: bool,
    #[serde(default)]
    pub danger: Option<String>,
    #[serde(default)]
    pub artifact: Option<Artifact>,
    #[serde(default)]
    pub files: Vec<PackageFile>,
    #[serde(flatten)]
    pub extra: BTreeMap<String, serde_json::Value>,
}

/// 一个外部命令依赖。
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct Dependency {
    pub command: String,
    /// 缺了怎么装（作者写的话）。没写就退回包里 install 提示或通用说法。
    #[serde(default)]
    pub hint: Option<String>,
}

/// 可下载的产物。
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct Artifact {
    pub url: String,
    /// 产物整包的 SHA-256（小写十六进制）。网络来源**必须**有。
    #[serde(default)]
    pub sha256: Option<String>,
    /// \`tar.gz\` / \`tar\` / \`file\`；缺省按 url 后缀猜。
    #[serde(default)]
    pub kind: Option<String>,
    #[serde(default)]
    pub size: Option<u64>,
}

/// 索引里声明的一个文件（同时是「安装哪些文件」的清单）。
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct PackageFile {
    pub path: String,
    #[serde(default)]
    pub kind: Option<String>,
    #[serde(default)]
    pub sha256: Option<String>,
    #[serde(default)]
    pub executable: Option<bool>,
    /// 未知字段：留个记录（多半是拼错了字段名），不影响解析。
    #[serde(flatten)]
    pub extra: BTreeMap<String, serde_json::Value>,
}

/// 文件的落地类别：决定它装到哪。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FileKind {
    /// 可执行脚本 → \`~/.local/bin/<文件名>\`（并进 \`~/.local/share/toolbox-hub/bin\` 的记录）。
    Bin,
    /// manifest（\`*.toml\`）→ 包自己的目录，由 Repository Provider 读。
    Manifest,
    /// 文档 → 包自己的目录。
    Doc,
    /// 其它数据 → 包自己的目录。
    Data,
}

impl FileKind {
    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_lowercase().as_str() {
            "bin" | "binary" | "script" | "executable" => Some(FileKind::Bin),
            "manifest" | "action" | "recipe" | "config" => Some(FileKind::Manifest),
            "doc" | "docs" | "documentation" => Some(FileKind::Doc),
            "data" | "other" => Some(FileKind::Data),
            _ => None,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            FileKind::Bin => "可执行脚本",
            FileKind::Manifest => "动作定义",
            FileKind::Doc => "文档",
            FileKind::Data => "数据",
        }
    }
}

/// 产物的承载形式。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ArtifactKind {
    TarGz,
    Tar,
    /// 单个文件，直接落到它自己的位置。
    File,
}

impl ArtifactKind {
    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_lowercase().as_str() {
            "tar.gz" | "tgz" | "targz" => Some(ArtifactKind::TarGz),
            "tar" => Some(ArtifactKind::Tar),
            "file" | "raw" => Some(ArtifactKind::File),
            _ => None,
        }
    }

    /// 没有显式声明时按 URL 猜。
    pub fn guess(url: &str) -> Self {
        let lowered = url.to_lowercase();
        if lowered.ends_with(".tar.gz") || lowered.ends_with(".tgz") {
            ArtifactKind::TarGz
        } else if lowered.ends_with(".tar") {
            ArtifactKind::Tar
        } else {
            ArtifactKind::File
        }
    }
}

impl Artifact {
    pub fn kind(&self) -> ArtifactKind {
        self.kind
            .as_deref()
            .and_then(ArtifactKind::parse)
            .unwrap_or_else(|| ArtifactKind::guess(&self.url))
    }

    /// 这个 URL 走的是本地文件系统（不发网络请求）。
    pub fn is_local(&self) -> bool {
        self.url.starts_with("file://") || !self.url.contains("://")
    }

    /// 本地文件对应的路径。
    pub fn local_path(&self) -> Option<PathBuf> {
        if let Some(rest) = self.url.strip_prefix("file://") {
            return Some(PathBuf::from(rest));
        }
        if !self.url.contains("://") {
            return Some(PathBuf::from(&self.url));
        }
        None
    }

    /// 规范化后的 SHA-256（小写）；格式不对返回 \`None\`。
    pub fn sha256(&self) -> Option<String> {
        let raw = self.sha256.as_deref()?.trim().to_lowercase();
        if raw.len() == 64 && raw.chars().all(|ch| ch.is_ascii_hexdigit()) {
            Some(raw)
        } else {
            None
        }
    }
}

impl PackageFile {
    /// 落地类别：显式写了用写的，否则按路径猜。
    ///
    /// 猜的规则刻意保守：\`scripts/\`、\`bin/\`、\`.sh\` 当可执行；
    /// \`manifests/\`、\`.toml\` 当动作定义；\`docs/\`、\`.md\` 当文档。
    pub fn kind(&self) -> FileKind {
        if let Some(explicit) = self.kind.as_deref().and_then(FileKind::parse) {
            return explicit;
        }
        if self.executable == Some(true) {
            return FileKind::Bin;
        }
        let lowered = self.path.to_lowercase();
        let top = lowered.split('/').next().unwrap_or("");
        if top == "scripts" || top == "bin" {
            return FileKind::Bin;
        }
        if top == "manifests" {
            return FileKind::Manifest;
        }
        if top == "docs" {
            return FileKind::Doc;
        }
        if lowered.ends_with(".sh") {
            return FileKind::Bin;
        }
        if lowered.ends_with(".toml") {
            return FileKind::Manifest;
        }
        if lowered.ends_with(".md") {
            return FileKind::Doc;
        }
        FileKind::Data
    }

    /// 规范化后的 SHA-256；格式不对返回 \`None\`（当作没声明）。
    pub fn sha256(&self) -> Option<String> {
        let raw = self.sha256.as_deref()?.trim().to_lowercase();
        if raw.len() == 64 && raw.chars().all(|ch| ch.is_ascii_hexdigit()) {
            Some(raw)
        } else {
            None
        }
    }
}

impl PackageMeta {
    /// 解析成结构化的版本；版本号写坏时返回 \`None\`。
    pub fn parsed_version(&self) -> Option<Version> {
        Version::parse(&self.version)
    }

    pub fn danger(&self) -> Danger {
        self.danger
            .as_deref()
            .and_then(Danger::parse)
            .unwrap_or(Danger::Safe)
    }

    pub fn summary_or_description(&self) -> &str {
        self.summary
            .as_deref()
            .or(self.description.as_deref())
            .map(str::trim)
            .filter(|text| !text.is_empty())
            .unwrap_or("-")
    }

    /// 这句话给用户看：作者 / License / 来源。
    #[cfg(test)]
    pub fn provenance_line(&self) -> String {
        let mut parts = Vec::new();
        if let Some(author) = self
            .author
            .as_deref()
            .filter(|text| !text.trim().is_empty())
        {
            parts.push(author.trim().to_string());
        }
        if let Some(license) = self
            .license
            .as_deref()
            .filter(|text| !text.trim().is_empty())
        {
            parts.push(format!("License {}", license.trim()));
        }
        if parts.is_empty() {
            String::from("未声明作者与许可证")
        } else {
            parts.join(" · ")
        }
    }

    /// 索引里的包搜索：id / 名字 / 简介 / 描述 / 标签 / 分类 / 作者。
    ///
    /// \`needle\` 必须已经小写化；空串视为命中。
    pub fn matches(&self, needle: &str) -> bool {
        if needle.is_empty() {
            return true;
        }
        let contains = |text: &str| text.to_lowercase().contains(needle);
        contains(&self.id)
            || contains(&self.name)
            || self.summary.as_deref().is_some_and(contains)
            || self.description.as_deref().is_some_and(contains)
            || self.author.as_deref().is_some_and(contains)
            || self.tags.iter().any(|tag| contains(tag))
            || self.categories.iter().any(|category| contains(category))
    }

    /// 校验一个包能不能被安装。
    ///
    /// 这里拒绝的东西都是**不可能安全安装**的：坏 id、坏版本号、越界的文件路径、
    /// 重复文件、以及「既没有产物也没有文件清单」。
    pub fn validate(&self) -> Result<(), String> {
        let id = paths::safe_identifier(&self.id)
            .map_err(|problem| format!("包 id 有问题：{problem}"))?;
        if self.name.trim().is_empty() {
            return Err(format!("{id}: 缺少 name"));
        }
        if self.parsed_version().is_none() {
            return Err(format!(
                "{id}: 版本号「{}」读不懂（要像 1.2.0 或 1.2.0-1）",
                self.version
            ));
        }
        if let Some(artifact) = &self.artifact {
            if artifact.url.trim().is_empty() {
                return Err(format!("{id}: artifact.url 是空的"));
            }
            if let Some(raw) = artifact.sha256.as_deref()
                && artifact.sha256().is_none()
            {
                return Err(format!("{id}: artifact.sha256 不是 64 位十六进制：{raw}"));
            }
        }
        if self.artifact.is_none() && self.files.is_empty() {
            return Err(format!("{id}: 既没有 artifact 也没有 files，装不了"));
        }
        let mut seen: Vec<PathBuf> = Vec::new();
        for file in &self.files {
            let relative = paths::safe_relative(&file.path)
                .map_err(|problem| format!("{id}: 文件路径有问题：{problem}"))?;
            if seen.contains(&relative) {
                return Err(format!("{id}: 文件重复声明：{}", file.path));
            }
            seen.push(relative);
            if let Some(raw) = file.sha256.as_deref()
                && file.sha256().is_none()
            {
                return Err(format!("{id}: {} 的 sha256 格式不对：{raw}", file.path));
            }
        }
        Ok(())
    }
}

impl Index {
    /// 解析一份索引文本。
    ///
    /// 失败只有三种：JSON 坏、schema 版本不认识、某个包自己不合格（这一条会
    /// 降级成警告并丢弃该包，其余包照常可用 —— 一个坏包不该毁掉整个仓库）。
    pub fn parse(text: &str) -> Result<ParsedIndex, String> {
        let mut index: Index =
            serde_json::from_str(text).map_err(|error| format!("索引不是合法的 JSON：{error}"))?;

        if index.schema_version != SCHEMA_VERSION {
            return Err(format!(
                "这个仓库的索引 schema 是 v{}，本版本只认 v{SCHEMA_VERSION}（请升级 toolbox-hub）",
                index.schema_version
            ));
        }

        let mut warnings = Vec::new();
        if !index.extra.is_empty() {
            let keys: Vec<&str> = index.extra.keys().map(String::as_str).collect();
            warnings.push(format!("索引里有本版本不认识的字段：{}", keys.join(", ")));
        }

        let mut kept = Vec::with_capacity(index.packages.len());
        let mut claimed: HashSet<String> = HashSet::new();
        for package in index.packages {
            if !package.extra.is_empty() {
                let keys: Vec<&str> = package.extra.keys().map(String::as_str).collect();
                warnings.push(format!(
                    "包「{}」有本版本不认识的字段：{}",
                    package.id,
                    keys.join(", ")
                ));
            }
            // 文件条目里的未知字段同样要看得见：那是本地作者最容易拼错的地方。
            for file in &package.files {
                if !file.extra.is_empty() {
                    let keys: Vec<&str> = file.extra.keys().map(String::as_str).collect();
                    warnings.push(format!(
                        "包「{}」的文件 {} 有本版本不认识的字段：{}",
                        package.id,
                        file.path,
                        keys.join(", ")
                    ));
                }
            }
            match package.validate() {
                Ok(()) => {
                    if claimed.contains(&package.id) {
                        warnings.push(format!("包 id 重复「{}」，只保留第一个", package.id));
                        continue;
                    }
                    claimed.insert(package.id.clone());
                    kept.push(package);
                }
                Err(problem) => warnings.push(format!("丢弃一个包：{problem}")),
            }
        }
        index.packages = kept;
        Ok(ParsedIndex { index, warnings })
    }

    pub fn find(&self, id: &str) -> Option<&PackageMeta> {
        self.packages.iter().find(|package| package.id == id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"{
      "schema_version": 1,
      "name": "ToolHub Official",
      "packages": [
        {
          "id": "sing-box-tools",
          "name": "sing-box 运维工具",
          "version": "1.4.0",
          "summary": "本机代理体检",
          "categories": ["network"],
          "tags": ["sing-box", "proxy"],
          "license": "MIT",
          "author": "emo",
          "dependencies": [{"command": "sing-box"}, {"command": "jq", "hint": "sudo pacman -S jq"}],
          "artifact": {"url": "https://example.com/a.tar.gz", "sha256": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa", "kind": "tar.gz"},
          "files": [
            {"path": "scripts/sing-box-audit", "kind": "bin"},
            {"path": "manifests/sing-box.toml"}
          ]
        }
      ]
    }"#;

    fn sample() -> PackageMeta {
        Index::parse(SAMPLE)
            .expect("应能解析")
            .index
            .packages
            .remove(0)
    }

    #[test]
    fn a_well_formed_index_parses() {
        let parsed = Index::parse(SAMPLE).expect("应能解析");
        assert_eq!(parsed.index.schema_version, 1);
        assert_eq!(parsed.index.packages.len(), 1);
        assert_eq!(parsed.index.name.as_deref(), Some("ToolHub Official"));
        assert!(parsed.warnings.is_empty(), "{:?}", parsed.warnings);
    }

    /// schema 不认识时必须明确报错 —— 不能当成 v1 硬读。
    #[test]
    fn an_unknown_schema_version_is_an_explicit_error() {
        let problem = Index::parse(r#"{"schema_version": 99, "packages": []}"#).unwrap_err();
        assert!(problem.contains("v99"), "{problem}");
        assert!(
            problem.contains("v1"),
            "要告诉用户本版本认哪个版本: {problem}"
        );
    }

    /// 远端格式加字段不该让老客户端整份索引作废，但要说出来（多半是拼错了）。
    #[test]
    fn unknown_fields_warn_but_do_not_invalidate() {
        let parsed = Index::parse(
            r#"{"schema_version": 1, "future_field": 1, "packages": [
                 {"id": "a", "name": "A", "version": "1.0.0",
                  "files": [{"path": "x.sh", "also_new": true}]}
               ]}"#,
        )
        .expect("应能解析");
        assert_eq!(parsed.index.packages.len(), 1, "包还是要留下");
        assert!(
            parsed
                .warnings
                .iter()
                .any(|warning| warning.contains("future_field")),
            "{:?}",
            parsed.warnings
        );
        assert!(
            parsed
                .warnings
                .iter()
                .any(|warning| warning.contains("also_new")),
            "包内的未知字段也要报: {:?}",
            parsed.warnings
        );
    }

    #[test]
    fn malformed_json_is_reported_with_a_message_not_a_panic() {
        for bad in ["", "{", "not json", "[]"] {
            assert!(Index::parse(bad).is_err(), "{bad:?} 应该报错");
        }
    }

    /// 一个坏包不能毁掉整个仓库。
    #[test]
    fn one_bad_package_is_dropped_with_a_warning() {
        let parsed = Index::parse(
            r#"{"schema_version": 1, "packages": [
                 {"id": "good", "name": "Good", "version": "1.0.0",
                  "files": [{"path": "a.sh"}]},
                 {"id": "bad", "name": "Bad", "version": "..",
                  "files": [{"path": "b.sh"}]}
               ]}"#,
        )
        .expect("整体应能解析");
        assert_eq!(parsed.index.packages.len(), 1);
        assert_eq!(parsed.index.packages[0].id, "good");
        assert_eq!(parsed.warnings.len(), 1);
        assert!(parsed.warnings[0].contains("bad"), "{:?}", parsed.warnings);
    }

    #[test]
    fn duplicate_package_ids_keep_the_first() {
        let parsed = Index::parse(
            r#"{"schema_version": 1, "packages": [
                 {"id": "a", "name": "一", "version": "1.0.0", "files": [{"path": "a.sh"}]},
                 {"id": "a", "name": "二", "version": "2.0.0", "files": [{"path": "b.sh"}]}
               ]}"#,
        )
        .expect("应能解析");
        assert_eq!(parsed.index.packages.len(), 1);
        assert_eq!(parsed.index.packages[0].name, "一");
        assert!(parsed.warnings.iter().any(|w| w.contains("重复")));
    }

    /// 安全：索引里的文件路径是**不可信输入**。
    #[test]
    fn unsafe_file_paths_are_rejected_at_parse_time() {
        for bad in ["../escape.sh", "/etc/passwd", "~/x.sh", "a\\\\b.sh"] {
            let text = format!(
                r#"{{"schema_version": 1, "packages": [
                     {{"id": "a", "name": "A", "version": "1.0.0",
                       "files": [{{"path": "{bad}"}}]}}
                   ]}}"#
            );
            let parsed = Index::parse(&text).expect("整体应能解析");
            assert!(parsed.index.packages.is_empty(), "{bad} 必须被拒绝");
            assert!(
                parsed.warnings.iter().any(|w| w.contains("路径")),
                "{:?}",
                parsed.warnings
            );
        }
    }

    #[test]
    fn a_package_with_neither_artifact_nor_files_is_rejected() {
        let text = r#"{"schema_version": 1, "packages": [
            {"id": "empty", "name": "空", "version": "1.0.0"}
        ]}"#;
        let parsed = Index::parse(text).expect("整体应能解析");
        assert!(parsed.index.packages.is_empty());
        assert!(parsed.warnings[0].contains("装不了"));
    }

    #[test]
    fn a_bad_hash_is_rejected_not_silently_ignored() {
        let text = r#"{"schema_version": 1, "packages": [
            {"id": "a", "name": "A", "version": "1.0.0",
             "artifact": {"url": "https://x/y.tar.gz", "sha256": "nothex"},
             "files": [{"path": "a.sh"}]}
        ]}"#;
        let parsed = Index::parse(text).expect("整体应能解析");
        assert!(parsed.index.packages.is_empty(), "坏 hash 必须拒绝");
        assert!(parsed.warnings[0].contains("sha256"));
    }

    #[test]
    fn a_valid_hash_is_normalised_to_lowercase() {
        let mut package = sample();
        package.artifact.as_mut().unwrap().sha256 = Some("A".repeat(64));
        assert_eq!(
            package.artifact.as_ref().unwrap().sha256().unwrap(),
            "a".repeat(64)
        );
        // 格式不对当作没声明，而不是崩
        package.artifact.as_mut().unwrap().sha256 = Some("短".to_string());
        assert!(package.artifact.as_ref().unwrap().sha256().is_none());
    }

    #[test]
    fn artifact_kind_is_declared_or_guessed_from_the_url() {
        assert_eq!(
            ArtifactKind::guess("https://x/a.tar.gz"),
            ArtifactKind::TarGz
        );
        assert_eq!(ArtifactKind::guess("https://x/a.tgz"), ArtifactKind::TarGz);
        assert_eq!(ArtifactKind::guess("https://x/a.tar"), ArtifactKind::Tar);
        assert_eq!(ArtifactKind::guess("https://x/a.sh"), ArtifactKind::File);
        assert_eq!(ArtifactKind::parse("TAR.GZ"), Some(ArtifactKind::TarGz));

        let mut artifact = sample().artifact.expect("有 artifact");
        artifact.kind = None;
        assert_eq!(artifact.kind(), ArtifactKind::TarGz, "没写就按 url 猜");
    }

    #[test]
    fn local_artifacts_are_recognised() {
        let mut artifact = sample().artifact.expect("有 artifact");
        artifact.url = "file:///tmp/pkg.tar.gz".to_string();
        assert!(artifact.is_local());
        assert_eq!(
            artifact.local_path().unwrap(),
            PathBuf::from("/tmp/pkg.tar.gz")
        );
        artifact.url = "/tmp/pkg.tar.gz".to_string();
        assert!(artifact.is_local(), "没有 scheme 的也当本地路径");
        artifact.url = "https://x/y".to_string();
        assert!(!artifact.is_local());
        assert_eq!(artifact.local_path(), None);
    }

    #[test]
    fn file_kinds_are_declared_or_inferred_from_the_path() {
        let cases = [
            ("scripts/a.sh", FileKind::Bin),
            ("bin/tool", FileKind::Bin),
            ("manifests/a.toml", FileKind::Manifest),
            ("docs/readme.md", FileKind::Doc),
            ("assets/data.bin", FileKind::Data),
            ("loose.sh", FileKind::Bin),
            ("loose.toml", FileKind::Manifest),
            ("loose.md", FileKind::Doc),
        ];
        for (path, expected) in cases {
            let file = PackageFile {
                path: path.to_string(),
                ..PackageFile::default()
            };
            assert_eq!(file.kind(), expected, "{path}");
        }

        // 显式声明压过推断
        let explicit = PackageFile {
            path: "scripts/a.sh".to_string(),
            kind: Some("doc".to_string()),
            ..PackageFile::default()
        };
        assert_eq!(explicit.kind(), FileKind::Doc);

        // executable = true 也算 bin
        let exec = PackageFile {
            path: "data/x".to_string(),
            executable: Some(true),
            ..PackageFile::default()
        };
        assert_eq!(exec.kind(), FileKind::Bin);
    }

    #[test]
    fn search_covers_identity_summary_tags_and_author() {
        let package = sample();
        assert!(package.matches(""));
        assert!(package.matches("sing-box"));
        assert!(package.matches("运维"));
        assert!(package.matches("proxy"));
        assert!(package.matches("network"));
        assert!(package.matches("emo"));
        assert!(!package.matches("ffmpeg"));
    }

    #[test]
    fn danger_defaults_to_safe_and_parses_the_known_words() {
        let mut package = sample();
        package.danger = None;
        assert_eq!(package.danger(), Danger::Safe);
        package.danger = Some("caution".to_string());
        assert_eq!(package.danger(), Danger::Caution);
        package.danger = Some("乱写".to_string());
        assert_eq!(package.danger(), Danger::Safe, "不认识就退回安全");
    }

    #[test]
    fn provenance_line_says_something_even_when_metadata_is_missing() {
        let mut package = sample();
        assert_eq!(package.provenance_line(), "emo · License MIT");
        package.author = None;
        package.license = None;
        assert_eq!(package.provenance_line(), "未声明作者与许可证");
    }

    #[test]
    fn find_locates_a_package_by_id() {
        let index = Index::parse(SAMPLE).expect("应能解析").index;
        assert!(index.find("sing-box-tools").is_some());
        assert!(index.find("不存在").is_none());
    }

    #[test]
    fn versions_go_through_the_shared_comparator() {
        let package = sample();
        assert_eq!(package.parsed_version().unwrap().as_str(), "1.4.0");
    }
}
