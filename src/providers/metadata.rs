//! 脚本元数据层：`# <脚本名>:<键>=<值>` 的解析与依赖检测。
//!
//! 这是所有「读脚本头部注解」的 Provider 共用的底座，新 Provider 不必再实现一遍解析。
//!
//! # 注解契约
//!
//! 注解写在脚本头部注释里，必须落在文件前 [`HEAD_LINES`] 行内（避免整个大脚本进内存）。
//!
//! | 键 | 必填 | 含义 |
//! | --- | --- | --- |
//! | `summary` | 通用脚本 Provider 必填 | 一句话说明；没有它的脚本不会被收录 |
//! | `domain` | 否 | `媒体` / `图像` / `系统` / `网络` / `开发` / `工具`，缺省按关键词猜 |
//! | `tags` | 否 | 逗号分隔的二级分类，决定页内筛选条 |
//! | `input` / `output` / `features` | 否 | 详情区展示 |
//! | `requires` | 否 | 逗号分隔的外部命令，全部存在才算「就绪」 |
//! | `install` | 否 | 依赖缺失时给用户看的安装办法，例如 `sudo pacman -S yt-dlp` |
//! | `mode` | 否 | `interactive`（接管终端，默认）或 `capture`（收进内置输出视图） |
//! | `danger` | 否 | `caution` 表示会改动用户没点名的文件，执行前要再确认一次 |
//! | `internal` | 否 | `true` 表示内部件（主菜单、共享库、预览器），不收录 |
//!
//! 例：
//!
//! ```text
//! #!/usr/bin/env bash
//! # my-tool:summary=把图片批量转成 WebP
//! # my-tool:domain=图像
//! # my-tool:tags=转换,批处理
//! # my-tool:requires=cwebp
//! # my-tool:install=sudo pacman -S libwebp
//! ```

use std::{
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
    sync::{LazyLock, Mutex},
};

/// 只读文件头部这么多行解析元数据。
pub const HEAD_LINES: usize = 40;

/// 元数据前缀固定为 `# <脚本名>:`。
const META_PREFIX: &str = "# ";

/// 读取 `# <name>:<key>=<value>`。
///
/// 前缀必须与脚本名完全匹配，避免读到别的脚本的元数据；
/// 同一键出现多次时取第一条（与 FFTools 原有行为一致）。
pub fn meta(head: &str, name: &str, key: &str) -> Option<String> {
    let prefix = format!("{META_PREFIX}{name}:{key}=");
    head.lines().find_map(|line| {
        line.strip_prefix(&prefix)
            .map(str::trim)
            .map(str::to_string)
    })
}

/// 逗号分隔列表：去空白、丢空项。
pub fn split_list(raw: &str) -> Vec<String> {
    raw.split(',')
        .map(|item| item.trim().to_string())
        .filter(|item| !item.is_empty())
        .collect()
}

/// 读取脚本头部（最多 [`HEAD_LINES`] 行）。读不出来就当空字符串，
/// 调用方会自然退化成「无注解」，不会把不可读文件当成工具。
pub fn read_head(path: &Path) -> String {
    fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .take(HEAD_LINES)
        .collect::<Vec<_>>()
        .join("\n")
}

/// `internal=true` 的内部件：主菜单、共享库、预览器之类，不该出现在工具列表里。
pub fn is_internal(head: &str, name: &str) -> bool {
    meta(head, name, "internal").as_deref() == Some("true")
}

/// `requires=` 的解析结果。
///
/// 不仅回答「能不能用」，还保留**到底是哪一个缺了** —— 详情区要把缺失项逐个标出来，
/// 光有一个 `bool` 说不出「缺什么」。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Dependencies {
    /// 注解里声明的全部依赖，顺序与注解一致。
    pub required: Vec<String>,
    /// 其中当前 `PATH` 上找不到的。
    pub missing: Vec<String>,
}

impl Dependencies {
    /// 依赖齐全（含「没声明依赖」）。
    pub fn is_ready(&self) -> bool {
        self.missing.is_empty()
    }
}

/// 解析 `requires` 并把每一项分成「满足 / 缺失」。
pub fn dependencies(head: &str, name: &str) -> Dependencies {
    let required = meta(head, name, "requires")
        .map(|raw| split_list(&raw))
        .unwrap_or_default();
    dependencies_of(&required)
}

/// 直接对一组命令做依赖检测。
///
/// 非脚本来源的 Provider（例如 [`crate::providers::manifest`] 包装已装 CLI）用这个，
/// 不必假装自己有一份带注解的脚本头。
pub fn dependencies_of(required: &[String]) -> Dependencies {
    let missing = required
        .iter()
        .filter(|dependency| !command_exists(dependency))
        .cloned()
        .collect();
    Dependencies {
        required: required.to_vec(),
        missing,
    }
}

pub fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    path.metadata()
        .map(|meta| meta.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

/// 把命令名在 `PATH` 里解析成绝对路径；带 `/` 的按路径直接判断。
///
/// Manifest / Curated 这类「包装已装 CLI」的 Provider 用它把 `program` 落成
/// 具体文件，详情区就能说清「到底会跑哪个二进制」。找不到返回 `None`，
/// 由调用方保留命令名并如实显示「依赖缺失」。
pub fn resolve_program(program: &str) -> Option<PathBuf> {
    if program.contains('/') {
        let path = PathBuf::from(program);
        return is_executable(&path).then_some(path);
    }
    let paths = std::env::var_os("PATH")?;
    std::env::split_paths(&paths)
        .map(|dir| dir.join(program))
        .find(|candidate| is_executable(candidate))
}

/// 依赖探测结果缓存。
///
/// 一轮发现里同一个命令可能被几十个工具依赖（`ffmpeg` 就是），没必要重复查。
static COMMAND_CACHE: LazyLock<Mutex<HashMap<String, bool>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// 依赖检测：命令在 `PATH` 上、或给定的路径确实存在且可执行。
///
/// 这里**刻意不走 `sh -lc 'command -v …'`**：实测每查一次要 spawn 一个 shell（约 20ms），
/// 12 个依赖就是 250ms，而直接扫 `PATH` 是 1ms 级。100~300 个工具时这个差别会变成
/// 以秒计的启动时间。
fn command_exists(name: &str) -> bool {
    if let Some(hit) = COMMAND_CACHE
        .lock()
        .ok()
        .and_then(|cache| cache.get(name).copied())
    {
        return hit;
    }

    let found = lookup_command(name);
    if let Ok(mut cache) = COMMAND_CACHE.lock() {
        cache.insert(name.to_string(), found);
    }
    found
}

fn lookup_command(name: &str) -> bool {
    // 空名字必须单独挡掉：`dir.join("")` 就是 `dir` 本身，而目录通常带执行位，
    // 不挡的话空依赖会被误判成「已满足」。
    if name.is_empty() {
        return false;
    }
    if name.contains('/') {
        return is_executable(Path::new(name));
    }
    std::env::var_os("PATH")
        .map(|paths| std::env::split_paths(&paths).any(|dir| is_executable(&dir.join(name))))
        .unwrap_or(false)
}

/// 清空依赖探测缓存。
///
/// [`crate::registry::Registry::reload`] 开头会调用：缓存只活在「一轮发现」之内，
/// 用户装了新工具之后按 Ctrl-R 应该立刻看到变化，而不是拿到进程启动时的旧结论。
pub fn clear_command_cache() {
    if let Ok(mut cache) = COMMAND_CACHE.lock() {
        cache.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::{
        clear_command_cache, command_exists, dependencies, is_internal, lookup_command, meta,
        split_list,
    };

    #[test]
    fn meta_requires_exact_script_name_prefix_and_trims() {
        let head = "# fzf-a:summary=  你好  \n# fzf-b:summary=别人\n# fzf-a:input=视频\n";
        assert_eq!(meta(head, "fzf-a", "summary").as_deref(), Some("你好"));
        assert_eq!(meta(head, "fzf-a", "input").as_deref(), Some("视频"));
        assert_eq!(meta(head, "fzf-a", "output"), None);
        // 前缀必须完全匹配：`fzf-a` 不该读到 `fzf-ab` 的注解。
        assert_eq!(meta("# fzf-ab:summary=x\n", "fzf-a", "summary"), None);
    }

    #[test]
    fn split_list_drops_blanks_and_trims() {
        assert_eq!(
            split_list(" sh , ffmpeg ,,  "),
            vec!["sh".to_string(), "ffmpeg".to_string()]
        );
        assert!(split_list("   ").is_empty());
    }

    #[test]
    fn is_internal_only_accepts_exact_true() {
        assert!(is_internal("# t:internal=true\n", "t"));
        assert!(!is_internal("# t:internal=false\n", "t"));
        assert!(!is_internal("# t:internal=1\n", "t"));
        assert!(!is_internal("# t:summary=x\n", "t"));
    }

    #[test]
    fn dependencies_split_into_required_and_missing() {
        let deps = dependencies("# t:requires=sh\n", "t");
        assert_eq!(deps.required, vec!["sh".to_string()]);
        assert!(deps.missing.is_empty());
        assert!(deps.is_ready());

        let deps = dependencies("# t:requires=sh,toolbox-hub-definitely-missing\n", "t");
        assert_eq!(deps.required.len(), 2);
        assert_eq!(
            deps.missing,
            vec!["toolbox-hub-definitely-missing".to_string()],
            "缺失项要能指名道姓，详情区靠它告诉用户缺什么"
        );
        assert!(!deps.is_ready());

        let deps = dependencies("# t:summary=x\n", "t");
        assert!(deps.required.is_empty());
        assert!(deps.missing.is_empty());
        assert!(deps.is_ready(), "没有声明依赖就算就绪");
    }

    #[test]
    fn dependency_lookup_handles_names_paths_and_empty_strings() {
        // 命令名走 PATH
        assert!(command_exists("sh"));
        assert!(!command_exists("toolbox-hub-definitely-missing"));
        // 带 `/` 的按路径判文件，而不是截成 basename 再拿去 PATH 里找
        assert!(command_exists("/bin/sh"));
        assert!(!command_exists("/nonexistent/dir/app"));
        // 空名字不能因为 `dir.join("") == dir` 且目录带执行位就被误判成可用
        assert!(!lookup_command(""));
    }

    #[test]
    fn clearing_the_cache_keeps_the_same_answers() {
        assert!(command_exists("sh"));
        clear_command_cache();
        assert!(command_exists("sh"), "清缓存只影响速度，不影响结论");
        assert!(!command_exists("toolbox-hub-definitely-missing"));
    }
}
