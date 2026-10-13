//! Ecosystem helpers: Recipe validation/import, presets, replay and argv queue.
use crate::runtime::signal_group;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    process::{Child, Command, ExitStatus, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum SafetyLevel {
    Safe,
    Caution,
    Destructive,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct RecipeParam {
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub required: bool,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Recipe {
    pub id: String,
    pub title: String,
    pub description: String,
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub params: Vec<RecipeParam>,
    pub safety: SafetyLevel,
    pub source: String,
    #[serde(default)]
    pub tags: Vec<String>,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ImportDisposition {
    Actionable(Recipe),
    DisplayOnly(String),
    Skipped(String),
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImportItem {
    pub line: usize,
    pub text: String,
    pub disposition: ImportDisposition,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImportReport {
    pub items: Vec<ImportItem>,
}
/// 已知高危程序（按 basename 比对；`mkfs` 这一族按前缀）。
const DANGEROUS_PROGRAMS: [&str; 12] = [
    "shutdown", "reboot", "poweroff", "halt", "wipefs", "fdisk", "parted", "sgdisk", "shred",
    "mkswap", "userdel", "groupdel",
];

/// 出现在管道 / 分号右边的解释器：`curl … | sh` 就是把外部内容当代码跑。
const SHELLS: [&str; 9] = [
    "sh", "bash", "zsh", "fish", "dash", "ksh", "csh", "python", "python3",
];

/// 提权词。**只要出现就算高危**，不看位置 —— 宁可多拦，不可漏拦。
const PRIVILEGE: [&str; 2] = ["sudo", "doas"];

fn basename(word: &str) -> &str {
    word.rsplit('/').next().unwrap_or(word)
}

/// 把命令行切成「段」：`|`、`;`、`&` 是段边界，每段第一个词是程序名。
///
/// 切段而不是简单按空格分词，是因为 `rm -rf x` 和 `echo rm -rf x` 完全是两回事：
/// 只有段首那个词才决定跑的是什么程序。
fn guard_segments(tokens: &[String]) -> Vec<Vec<&str>> {
    let mut segments: Vec<Vec<&str>> = vec![];
    let mut current: Vec<&str> = vec![];
    for token in tokens {
        let word = token.trim_matches(|c| c == '"' || c == '\'');
        if word.is_empty() {
            continue;
        }
        if matches!(word, "|" | ";" | "&" | "&&" | "||") {
            if !current.is_empty() {
                segments.push(std::mem::take(&mut current));
            }
            continue;
        }
        current.push(word);
    }
    if !current.is_empty() {
        segments.push(current);
    }
    segments
}

/// 参数里有没有这个短选项字母（`-rf` / `-r -f` / `--recursive` 都算）。
fn flag_set(args: &[&str], letter: char) -> bool {
    args.iter().any(|arg| {
        if let Some(long) = arg.strip_prefix("--") {
            // 长选项只认这几个我们真正关心的：别把 `--follow` 当成 `-f`。
            matches!((letter, long), ('r' | 'R', "recursive") | ('f', "force"))
        } else {
            arg.starts_with('-') && arg.len() > 1 && arg[1..].contains(letter)
        }
    })
}

/// 一条命令里有没有「已知高危」的写法。
///
/// 这是**规则式 denylist，不是沙箱**：它能拦住踩过坑的写法，拦不住没写到的写法，
/// 也不理解通配符展开后的真实目标（`rm -rf $VAR` 拦不住，因为展开发生在它之后）。
/// 所以三个调用点（导入器、队列、Recipe 校验）都只把它当第一道闸，不当保证。
fn dangerous(s: &str) -> bool {
    let lowered = s.to_ascii_lowercase();
    // fork bomb 没有「程序 + 参数」的结构，只能整串看。
    if lowered.contains(":(){") {
        return true;
    }

    let tokens = guard_tokens(&lowered);
    let segments = guard_segments(&tokens);

    for (index, segment) in segments.iter().enumerate() {
        let Some(program) = segment.first() else {
            continue;
        };
        let name = basename(program);

        // 提权词出现在任何位置都算：`… | sudo tee …` 同样是提权。
        if segment
            .iter()
            .any(|word| PRIVILEGE.contains(&basename(word)))
        {
            return true;
        }
        // 段首就是解释器，且它不在第一段 —— 说明有东西被管道喂给它。
        if index > 0 && SHELLS.contains(&name) {
            return true;
        }
        if DANGEROUS_PROGRAMS.contains(&name) || name.starts_with("mkfs") || name == "swapon" {
            return true;
        }

        let args = &segment[1..];
        match name {
            // `dd of=` 会写块设备：`dd if=` 只是读，写出去的那半边才危险。
            // 写 `/dev/null` 是常见的吞吐量测试，放行。
            "dd" => {
                if args
                    .iter()
                    .filter_map(|arg| arg.strip_prefix("of="))
                    .any(|target| target != "/dev/null")
                {
                    return true;
                }
            }
            "rm" => {
                let recursive = flag_set(args, 'r') || flag_set(args, 'R');
                let force = flag_set(args, 'f');
                let root = args.iter().any(|arg| *arg == "/" || *arg == "/*");
                if recursive && (force || root) {
                    return true;
                }
            }
            // 递归改权限是经典踩坑：`-R` 一加，作用范围就从「这个文件」变成「整棵树」。
            "chmod" | "chown" => {
                if flag_set(args, 'r') || flag_set(args, 'R') {
                    return true;
                }
            }
            "systemctl" => {
                let power = args.iter().any(|arg| {
                    matches!(
                        *arg,
                        "poweroff" | "reboot" | "halt" | "rescue" | "emergency"
                    )
                });
                if power {
                    return true;
                }
            }
            _ => {}
        }
    }
    false
}

/// 按空白与 shell 分隔符切词，**保留分隔符本身**（[`guard_segments`] 靠它们断句）。
fn guard_tokens(s: &str) -> Vec<String> {
    let mut out = vec![];
    let mut current = String::new();
    for ch in s.chars() {
        if ch.is_whitespace() {
            if !current.is_empty() {
                out.push(std::mem::take(&mut current));
            }
        } else if matches!(ch, '|' | ';' | '&') {
            if !current.is_empty() {
                out.push(std::mem::take(&mut current));
            }
            out.push(ch.to_string());
        } else {
            current.push(ch);
        }
    }
    if !current.is_empty() {
        out.push(current);
    }
    out
}
fn shell_syntax(s: &str) -> bool {
    ["|", ";", "&&", "||", ">", "$(", "`"]
        .iter()
        .any(|p| s.contains(p))
}
fn words(s: &str) -> Vec<String> {
    if s.contains('"') || s.contains('\'') {
        vec![]
    } else {
        s.split_whitespace().map(str::to_string).collect()
    }
}
fn parse_command(fmt: &str, line: usize, raw: &str) -> ImportItem {
    let text = raw.trim().trim_start_matches("$ ").trim().to_string();
    let skip = |w: &str| ImportItem {
        line,
        text: text.clone(),
        disposition: ImportDisposition::Skipped(w.into()),
    };
    let display = |w: &str| ImportItem {
        line,
        text: text.clone(),
        disposition: ImportDisposition::DisplayOnly(w.into()),
    };
    if text.is_empty() {
        return skip("空命令");
    }
    if dangerous(&text) {
        return skip("危险命令，需要人工审计");
    }
    let syntax_probe = text
        .split_whitespace()
        .map(|word| {
            if word.starts_with('<') && word.ends_with('>') {
                "{parameter}"
            } else {
                word
            }
        })
        .collect::<Vec<_>>()
        .join(" ");
    if shell_syntax(&syntax_probe) {
        return display("包含 shell 复合语法，不能安全转换为 argv");
    }
    let mut w = words(&text);
    if w.is_empty() {
        return display("引号或语法需要人工转换");
    }
    let command = w.remove(0);
    if command.starts_with('-') {
        return skip("缺少命令名");
    }
    let mut args = vec![];
    let mut params: Vec<RecipeParam> = vec![];
    for word in w {
        if (word.starts_with('<') && word.ends_with('>'))
            || (word.starts_with("{{") && word.ends_with("}}"))
        {
            let key = word
                .trim_matches(|c: char| matches!(c, '<' | '>' | '{' | '}'))
                .replace('-', "_");
            if key.is_empty() || !key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
                // 不做「把 / 自动换成 _」这种猜测：参数名会跟着进模板和预设，
                // 改错了是静默的，不如让人自己看一眼再改。
                return display(&format!(
                    "占位符「{key}」不能当参数名（只收字母数字下划线），需人工改名"
                ));
            }
            if !params.iter().any(|p| p.name == key) {
                params.push(RecipeParam {
                    name: key.clone(),
                    description: key.clone(),
                    required: true,
                })
            }
            args.push(format!("{{{key}}}"))
        } else {
            args.push(word)
        }
    }
    let recipe = Recipe {
        id: format!(
            "import-{fmt}-{}-{line}",
            command.replace(|c: char| !c.is_ascii_alphanumeric(), "-")
        ),
        title: format!("导入示例：{command}"),
        description: format!("从 {fmt} 导入；执行前请检查参数。"),
        command,
        args,
        params,
        safety: SafetyLevel::Safe,
        source: fmt.into(),
        tags: vec![fmt.into(), "imported".into()],
    };
    if let Err(e) = validate_recipe(&recipe) {
        return skip(&e);
    }
    ImportItem {
        line,
        text,
        disposition: ImportDisposition::Actionable(recipe),
    }
}
/// cheat 文件头部的 YAML front matter（文件级元数据，**不是命令**）。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct FrontMatter {
    /// `syntax:` 的值：这份速查是按哪种解释器写的。
    syntax: Option<String>,
    /// `tags:` 列表。
    tags: Vec<String>,
}

/// 从 cheat 文件里剥出 front matter。
///
/// cheat 的速查表可以在最前面放一段 YAML：
///
/// ```text
/// ---
/// syntax: bash
/// tags: [networking, dns]
/// ---
/// # 查 DNS
/// dig +short <domain>
/// ```
///
/// 这段是**文件级元数据**。早先按行硬导的后果是真跑出来的：`syntax: bash` 和
/// `tags: [networking, dns]` 各变成一条「可导入」Recipe（程序名分别是 `syntax:` 和
/// `tags:`），而真正的 `dig` 那条混在中间 —— 整份文件的导入结果不可用。
///
/// 返回 `(元数据, 正文起始行)`；`起始行` 为 `None` 表示「没有闭合的 `---`」，
/// 调用方应当整份按元数据吞掉（而不是把剩下的 YAML 当命令）。
///
/// 只认这两个键，而且只认 `key: value` 这一种写法：值里的引号会剥掉、`tags` 的
/// `[a, b]` 与 `a, b` 都收。**不引 YAML 解析器** —— 为了两个键引一棵依赖树不划算，
/// 认不出来的部分一律当元数据忽略，不会跑到命令那一侧去。
fn cheat_front_matter(text: &str) -> (FrontMatter, Option<usize>) {
    let mut meta = FrontMatter::default();

    // 开头的空行允许（有些编辑器会留），但 `---` 之前出现别的内容就不是 front matter。
    let mut opening = None;
    for (index, raw) in text.lines().enumerate() {
        let line = raw.trim();
        if line.is_empty() {
            continue;
        }
        if line == "---" {
            opening = Some(index);
        }
        break;
    }
    let Some(opening) = opening else {
        return (meta, Some(0));
    };

    for (index, raw) in text.lines().enumerate().skip(opening + 1) {
        let line = raw.trim();
        if line == "---" {
            return (meta, Some(index + 1));
        }
        if let Some(value) = line.strip_prefix("syntax:") {
            let value = value.trim().trim_matches(['"', '\'']);
            if !value.is_empty() {
                meta.syntax = Some(value.to_string());
            }
        } else if let Some(value) = line.strip_prefix("tags:") {
            meta.tags = parse_tag_list(value);
        }
    }
    // 没有闭合的 `---`：后面全是元数据（宁可什么都不导，也不要导出一堆 YAML 垃圾）。
    (meta, None)
}

/// `[networking, dns]` 与 `networking, dns` 两种写法都收。
fn parse_tag_list(raw: &str) -> Vec<String> {
    raw.trim()
        .trim_start_matches('[')
        .trim_end_matches(']')
        .split(',')
        .map(|tag| tag.trim().trim_matches(['"', '\'']).to_string())
        .filter(|tag| !tag.is_empty())
        .collect()
}

pub fn import_text(fmt: &str, text: &str) -> Result<ImportReport, String> {
    let fmt = fmt.to_ascii_lowercase();
    if !["navi", "cheat", "tldr"].contains(&fmt.as_str()) {
        return Err("格式必须是 navi、cheat 或 tldr".into());
    }

    // front matter 是 cheat 的语法（navi 用 `% 标签`、tldr 用标题），只对它剥。
    let (meta, body_start) = if fmt == "cheat" {
        cheat_front_matter(text)
    } else {
        (FrontMatter::default(), Some(0))
    };

    let mut items = vec![];
    if body_start.is_none() {
        let head = text.lines().next().unwrap_or_default().trim();
        items.push(ImportItem {
            line: 1,
            text: head.to_string(),
            disposition: ImportDisposition::Skipped(
                "front matter 的 `---` 没有闭合，整份文件按元数据跳过（检查文件结构）".into(),
            ),
        });
        return Ok(ImportReport { items });
    }

    for (i, raw) in text.lines().enumerate().skip(body_start.unwrap_or(0)) {
        let s = raw.trim();
        if s.is_empty()
            || s.starts_with('#')
            || s.starts_with('%')
            || s.starts_with("~~~")
            || s.starts_with('>')
        {
            continue;
        }
        let command = if fmt == "tldr" {
            let Some(command) = tldr_command(s) else {
                continue;
            };
            command
        } else {
            s.to_string()
        };
        let mut item = parse_command(&fmt, i + 1, &command);
        if fmt == "tldr" && matches!(item.disposition, ImportDisposition::Actionable(_)) {
            item.disposition = ImportDisposition::DisplayOnly(
                "tldr 是带上下文的速查示例，默认仅展示，需人工确认后转换".into(),
            )
        }
        // front matter 里的信息**只补元数据，不碰 argv**：`syntax: bash` 是「这份速查
        // 按 bash 写的」这一事实的备注，不是「可以用 bash 拼命令执行」的许可。
        if let ImportDisposition::Actionable(recipe) = &mut item.disposition {
            if let Some(syntax) = &meta.syntax {
                recipe.description =
                    format!("从 cheat 导入（syntax: {syntax}）；执行前请检查参数。");
            }
            for tag in &meta.tags {
                if !recipe.tags.contains(tag) {
                    recipe.tags.push(tag.clone());
                }
            }
        }
        items.push(item)
    }
    Ok(ImportReport { items })
}

/// 从一行 tldr 里取出命令。取不到就返回 `None`（说明这一行不是命令行）。
///
/// 真实的 tldr 页面是**两行一条**示例：
///
/// ```text
/// - Show the last N number of commits:
/// `git log -n {{number_of_commits}}`
/// ```
///
/// 命令在下一行、且**包着反引号**。反引号必须剥掉：它在 shell 里是命令替换，
/// 留着就会被判成「含 shell 复合语法」而降级 —— 真实页面会一条都取不出来
/// （这是拿真的 `git-log` 页面跑出来的：整页全是「跳过：空命令」）。
///
/// 也兼容把命令写在说明同一行的写法（`- 说明: 命令`）；那种行里冒号后面为空
/// 说明命令在下一行，这一行按「不是命令行」跳过。
fn tldr_command(line: &str) -> Option<String> {
    let s = line.trim();
    if let Some(inner) = s.strip_prefix('`').and_then(|rest| rest.strip_suffix('`')) {
        let inner = inner.trim();
        return (!inner.is_empty()).then(|| inner.to_string());
    }
    let rest = s.strip_prefix("- ")?;
    let after = rest.split_once(':')?.1.trim();
    (!after.is_empty()).then(|| after.to_string())
}
pub fn validate_recipe(r: &Recipe) -> Result<(), String> {
    if r.id.trim().is_empty()
        || r.title.trim().is_empty()
        || r.description.trim().is_empty()
        || r.command.trim().is_empty()
        || r.source.trim().is_empty()
    {
        return Err("缺少 id/title/description/command/source".into());
    }
    if r.id
        .chars()
        .any(|c| !(c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.')))
    {
        return Err("id 格式不合法".into());
    }
    let full = std::iter::once(r.command.as_str())
        .chain(r.args.iter().map(String::as_str))
        .collect::<Vec<_>>()
        .join(" ");
    if dangerous(&full) && r.safety != SafetyLevel::Destructive {
        return Err("危险命令不能标记为 safe/caution，必须标记 destructive".into());
    }
    if r.safety == SafetyLevel::Safe
        && (full.contains("--delete") || r.args.iter().any(|a| a.contains("{output}")))
    {
        return Err("可能修改数据的命令不能标记为 safe".into());
    }
    for a in &r.args {
        let mut tail = a.as_str();
        while let Some(i) = tail.find('{') {
            let after = &tail[i + 1..];
            let Some(end) = after.find('}') else {
                return Err("参数模板花括号不匹配".into());
            };
            let key = &after[..end];
            if !r.params.iter().any(|p| p.name == key) {
                return Err(format!("模板参数 {key} 未声明"));
            }
            tail = &after[end + 1..]
        }
    }
    Ok(())
}
pub fn sha256(p: &Path) -> Result<String, String> {
    Ok(format!(
        "{:x}",
        Sha256::digest(fs::read(p).map_err(|e| e.to_string())?)
    ))
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Preset {
    pub name: String,
    pub recipe_id: String,
    #[serde(default)]
    pub values: BTreeMap<String, String>,
}
/// 预设目录：`TOOLBOX_HUB_CONFIG` > `$HOME/.config/toolbox-hub` > 当前目录。
///
/// 环境值当参数传进来（而不是在这里读 `std::env`）是为了能测：edition 2024 里改
/// 环境变量是 `unsafe`，并行跑的测试还会互相踩。
fn config_dir_from(config: Option<&std::ffi::OsStr>, home: Option<&std::ffi::OsStr>) -> PathBuf {
    config
        .map(PathBuf::from)
        .or_else(|| {
            home.map(PathBuf::from)
                .map(|p| p.join(".config/toolbox-hub"))
        })
        .unwrap_or_else(|| PathBuf::from(".toolbox-hub"))
}

fn config_dir() -> PathBuf {
    config_dir_from(
        std::env::var_os("TOOLBOX_HUB_CONFIG").as_deref(),
        std::env::var_os("HOME").as_deref(),
    )
}

/// 预设名直接当文件名用，所以这里同时承担「路径穿越」的防线。
fn preset_name_ok(name: &str) -> bool {
    !name.is_empty()
        && !name.contains('/')
        && !name.contains('\\')
        && !name.contains("..")
        && name != "."
}

pub fn save_preset(p: &Preset) -> Result<(), String> {
    save_preset_in(&config_dir(), p)
}

/// 写入一份预设。`base` 是配置根目录（`TOOLBOX_HUB_CONFIG`），测试直接传给临时目录。
///
/// 格式是 **TOML**：配置目录里的东西（`state.toml`、`packages.toml`、工具 manifest）
/// 一律 TOML，预设没理由例外 —— 让人能直接打开看、直接改，改坏了报错也看得懂。
pub fn save_preset_in(base: &Path, p: &Preset) -> Result<(), String> {
    if !preset_name_ok(&p.name) {
        return Err("预设名无效".into());
    }
    let d = base.join("presets");
    fs::create_dir_all(&d).map_err(|e| e.to_string())?;
    let text = toml::to_string_pretty(p).map_err(|e| format!("写不出 TOML：{e}"))?;
    fs::write(d.join(format!("{}.toml", p.name)), text).map_err(|e| e.to_string())
}

pub fn load_preset(n: &str) -> Result<Preset, String> {
    load_preset_in(&config_dir(), n)
}

pub fn load_preset_in(base: &Path, n: &str) -> Result<Preset, String> {
    if !preset_name_ok(n) {
        return Err("预设名无效".into());
    }
    let path = base.join("presets").join(format!("{n}.toml"));
    let text = match fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) => {
            // 早期版本存的是 JSON（那个版本没有发布过）。碰上了就直说怎么办，
            // 而不是报一句「没有那个文件」让人以为是名字打错了。
            let legacy = base.join("presets").join(format!("{n}.json"));
            if legacy.is_file() {
                return Err(format!(
                    "{n} 是早期版本的 JSON 预设（{}）；现在只读 TOML —— 用 `preset save` 重存一次即可",
                    legacy.display()
                ));
            }
            return Err(format!("读不到预设 {}：{error}", path.display()));
        }
    };
    toml::from_str(&text).map_err(|error| format!("{} 不是合法 TOML：{error}", path.display()))
}

pub fn list_presets() -> Result<Vec<Preset>, String> {
    list_presets_in(&config_dir())
}

pub fn list_presets_in(base: &Path) -> Result<Vec<Preset>, String> {
    let Ok(es) = fs::read_dir(base.join("presets")) else {
        return Ok(vec![]);
    };
    let mut out = vec![];
    for e in es.flatten() {
        if e.path().extension().and_then(|s| s.to_str()) != Some("toml") {
            continue;
        }
        // 坏掉的预设文件不该让「列出来」整体失败：跳过它，其余照常显示。
        // （只有 `.toml` 算预设；早期版本的 `.json` 会被当成无关文件忽略。）
        let Ok(bytes) = fs::read(e.path()) else {
            continue;
        };
        let Ok(text) = String::from_utf8(bytes) else {
            continue;
        };
        if let Ok(preset) = toml::from_str(&text) {
            out.push(preset);
        }
    }
    out.sort_by(|a: &Preset, b| a.name.cmp(&b.name));
    Ok(out)
}
pub fn delete_preset(n: &str) -> Result<(), String> {
    delete_preset_in(&config_dir(), n)
}

pub fn delete_preset_in(base: &Path, n: &str) -> Result<(), String> {
    if !preset_name_ok(n) {
        return Err("预设名无效".into());
    }
    fs::remove_file(base.join("presets").join(format!("{n}.toml"))).map_err(|e| e.to_string())
}
pub fn replay_values(
    id: &str,
    v: &BTreeMap<String, String>,
    known: &[String],
) -> (String, BTreeMap<String, String>) {
    if known.iter().any(|x| x == id) {
        (id.into(), v.clone())
    } else {
        (String::new(), v.clone())
    }
}
/// 终端里按下 Ctrl-C 之后置位。
///
/// 为什么要管这个：任务是**自成进程组**启动的（取消时才好整组带走），而 Ctrl-C
/// 只会发给终端的**前台进程组** —— 也就是说加了进程组之后，Ctrl-C 只杀得掉
/// toolbox-hub 自己，任务会变成孤儿继续跑。所以自己接住 SIGINT，把它转成
/// 「取消」：走同一条 SIGTERM → 宽限 → SIGKILL 的路，任务跟我们一起停。
static INTERRUPTED: AtomicBool = AtomicBool::new(false);

/// SIGINT 的处理器。**只写一个原子变量** —— 信号处理器里只能做异步信号安全的事。
extern "C" fn note_interrupt(_signal: libc::c_int) {
    INTERRUPTED.store(true, Ordering::SeqCst);
}

/// 装上 Ctrl-C 处理（只对队列生效，不碰其它子命令的信号处置）。
fn install_interrupt_handler() {
    // SAFETY: 处理器只做一次原子写；`signal()` 只改这一个信号的处置。
    unsafe {
        // 两步转换是刻意的：函数项直接转整数会被 lint（function_casts_as_integer），
        // 先取成裸指针再转成 sighandler_t，语义也没变。
        libc::signal(
            libc::SIGINT,
            note_interrupt as *const () as libc::sighandler_t,
        );
    }
}

/// `queue run` 不带 `--jobs` 时的并发数。
///
/// **默认 1**，而不是「有几个核就开几个」：任务是用户自己的命令，
/// 一次跑一个才符合「我按顺序让它干活」的预期，也不会把机器打满。
pub const DEFAULT_JOBS: usize = 1;

/// 取消任务时的宽限期：先 SIGTERM 请它收尾，过了这个时间还没走就 SIGKILL。
///
/// 给宽限期是因为工具需要时间把已经写出去的东西收尾（ffmpeg 会把已写部分
/// 封成一个能播的文件）。不要太长：取消是用户按下去的，等太久就不叫取消了。
const CANCEL_GRACE: Duration = Duration::from_secs(1);

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct QueueTask {
    pub id: String,
    pub program: String,
    #[serde(default)]
    pub args: Vec<String>,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TaskResult {
    pub id: String,
    pub exit_code: i32,
    pub elapsed_ms: u128,
    pub cancelled: bool,
    pub error: Option<String>,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QueueSummary {
    pub results: Vec<TaskResult>,
    pub exit_code: i32,
}
#[derive(Clone, Default)]
pub struct CancellationToken(Arc<AtomicBool>);
impl CancellationToken {
    pub fn cancel(&self) {
        self.0.store(true, Ordering::SeqCst)
    }
    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
}
pub fn run_queue(tasks: Vec<QueueTask>, jobs: usize, cancel: CancellationToken) -> QueueSummary {
    run_queue_with(tasks, jobs, cancel, &INTERRUPTED)
}

/// 同上，但中断标志由调用方给（测试用：不发真信号也能验「中断 = 取消」）。
fn run_queue_with(
    tasks: Vec<QueueTask>,
    jobs: usize,
    cancel: CancellationToken,
    interrupt: &AtomicBool,
) -> QueueSummary {
    let tasks = Arc::new(tasks);
    let next = Arc::new(AtomicUsize::new(0));
    let (tx, rx) = mpsc::channel();
    let mut handles = vec![];
    for _ in 0..jobs.max(1).min(tasks.len().max(1)) {
        let tasks = tasks.clone();
        let next = next.clone();
        let tx = tx.clone();
        let cancel = cancel.clone();
        handles.push(thread::spawn(move || {
            loop {
                if cancel.is_cancelled() {
                    break;
                }
                let i = next.fetch_add(1, Ordering::SeqCst);
                let Some(t) = tasks.get(i) else { break };
                if cancel.is_cancelled() {
                    break;
                }
                let start = Instant::now();
                let joined = std::iter::once(t.program.as_str())
                    .chain(t.args.iter().map(String::as_str))
                    .collect::<Vec<_>>()
                    .join(" ");
                if dangerous(&joined) {
                    let _ = tx.send(TaskResult {
                        id: t.id.clone(),
                        exit_code: 126,
                        elapsed_ms: 0,
                        cancelled: false,
                        error: Some(
                            "已拦截疑似 destructive 命令；必须经 Recipe 审计和显式确认入口".into(),
                        ),
                    });
                    continue;
                }
                let mut command = Command::new(&t.program);
                command
                    .args(&t.args)
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null());
                // 让它自成进程组（pgid = 自己的 pid）。任务常常是个脚本，脚本自己
                // 会拉起子进程；不整组带走的话，取消之后那些孙子进程会变成孤儿
                // 接着跑、接着写文件。与交互式执行层（runtime::exec）同一套做法。
                #[cfg(unix)]
                {
                    use std::os::unix::process::CommandExt;
                    command.process_group(0);
                }
                let mut child = match command.spawn() {
                    Ok(c) => c,
                    Err(e) => {
                        let _ = tx.send(TaskResult {
                            id: t.id.clone(),
                            exit_code: 127,
                            elapsed_ms: start.elapsed().as_millis(),
                            cancelled: false,
                            error: Some(e.to_string()),
                        });
                        continue;
                    }
                };
                let status = loop {
                    if cancel.is_cancelled() {
                        break cancel_and_wait(&mut child, CANCEL_GRACE);
                    }
                    match child.try_wait() {
                        Ok(Some(s)) => break Some(s),
                        Ok(None) => thread::sleep(Duration::from_millis(10)),
                        Err(_) => {
                            let _ = child.kill();
                            break child.wait().ok();
                        }
                    }
                };
                let c = cancel.is_cancelled();
                let code = status
                    .and_then(|s| s.code())
                    .unwrap_or(if c { 130 } else { 1 });
                let _ = tx.send(TaskResult {
                    id: t.id.clone(),
                    exit_code: code,
                    elapsed_ms: start.elapsed().as_millis(),
                    cancelled: c,
                    error: None,
                });
                if c {
                    break;
                }
            }
        }))
    }
    drop(tx);
    let mut results = Vec::new();
    let mut told_about_interrupt = false;
    loop {
        // 带超时地收：既要收结果，也要能及时发现「用户按了 Ctrl-C」。
        let result = match rx.recv_timeout(Duration::from_millis(100)) {
            Ok(result) => result,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if interrupt.load(Ordering::SeqCst) && !cancel.is_cancelled() {
                    if !told_about_interrupt {
                        told_about_interrupt = true;
                        eprintln!("收到 Ctrl-C：正在取消还没跑完的任务（先 SIGTERM 让它们收尾）…");
                    }
                    // 走和 API 取消同一条路：worker 会把进程组整组带走。
                    cancel.cancel();
                }
                continue;
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        };
        eprintln!(
            "队列进度：{}/{} 完成，任务 {} 退出码 {}",
            results.len() + 1,
            tasks.len(),
            result.id,
            result.exit_code
        );
        results.push(result);
    }
    for h in handles {
        let _ = h.join();
    }
    results.sort_by(|a, b| a.id.cmp(&b.id));
    let exit_code = if cancel.is_cancelled() {
        130
    } else if results.iter().any(|r| r.exit_code != 0) {
        1
    } else {
        0
    };
    QueueSummary { results, exit_code }
}
/// 取消一个任务：先 **SIGTERM 整个进程组**请它收尾，宽限期过了再整组 SIGKILL。
///
/// 发整组而不是发单个进程，是因为任务自己拉起来的子进程（脚本里的 `&`、
/// ffmpeg 管道那类）也该一起收到 —— 只杀直接子进程会留下继续写文件的孤儿。
fn cancel_and_wait(child: &mut Child, grace: Duration) -> Option<ExitStatus> {
    #[cfg(unix)]
    {
        let _ = signal_group(child.id(), libc::SIGTERM);
    }
    let deadline = Instant::now() + grace;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Some(status),
            Ok(None) => {
                if Instant::now() >= deadline {
                    // 请不动就动手：整组 SIGKILL，再用 Child::kill 兜底
                    //（进程组信号失败时至少把直接子进程带走）。
                    #[cfg(unix)]
                    {
                        let _ = signal_group(child.id(), libc::SIGKILL);
                    }
                    let _ = child.kill();
                    return child.wait().ok();
                }
                thread::sleep(Duration::from_millis(20));
            }
            Err(_) => {
                let _ = child.kill();
                return child.wait().ok();
            }
        }
    }
}

/// `--jobs N` 的解析：没给、给了非数字、给了 0 都退回 [`DEFAULT_JOBS`]。
fn parse_jobs(args: &[String]) -> usize {
    args.windows(2)
        .find(|pair| pair[0] == "--jobs")
        .and_then(|pair| pair[1].parse::<usize>().ok())
        .filter(|jobs| *jobs > 0)
        .unwrap_or(DEFAULT_JOBS)
}

pub fn run_cli(a: &[String]) -> Result<(), String> {
    match a.first().map(String::as_str) {
        Some("recipe") => {
            if a.get(1).map(String::as_str) == Some("run") {
                return run_recipe_cli(&a[2..]);
            }
            let d = a
                .get(2)
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("registry/featured"));
            let idx: serde_json::Value =
                serde_json::from_slice(&fs::read(d.join("index.json")).map_err(|e| e.to_string())?)
                    .map_err(|e| e.to_string())?;
            let rows = idx["recipes"].as_array().ok_or("索引缺少 recipes")?;
            if a.get(1).map(String::as_str) == Some("check") {
                let mut ids = std::collections::BTreeSet::new();
                for row in rows {
                    let id = row["id"].as_str().ok_or("缺 id")?;
                    let file = row["file"].as_str().ok_or("缺 file")?;
                    let hash = row["sha256"].as_str().ok_or("缺 sha256")?;
                    if !ids.insert(id.to_string()) {
                        return Err(format!("重复 id：{id}"));
                    }
                    if sha256(&d.join(file))? != hash {
                        return Err(format!("SHA-256 不匹配：{file}"));
                    }
                    let r: Recipe =
                        serde_json::from_slice(&fs::read(d.join(file)).map_err(|e| e.to_string())?)
                            .map_err(|e| e.to_string())?;
                    validate_recipe(&r)?
                }
                println!("featured 校验通过：{} 条", rows.len())
            } else {
                for r in rows {
                    println!(
                        "{}\t{}",
                        r["id"].as_str().unwrap_or("?"),
                        r["file"].as_str().unwrap_or("?")
                    )
                }
            }
            Ok(())
        }
        Some("import") => {
            if a.len() < 3 {
                return Err(
                    "用法：import <navi|cheat|tldr> <文件> [--dry-run] [--out 目录]".into(),
                );
            }
            let fmt = &a[1];
            let text = fs::read_to_string(&a[2]).map_err(|e| e.to_string())?;
            let report = import_text(fmt, &text)?;
            let dry = a.iter().any(|x| x == "--dry-run");
            let out = a
                .windows(2)
                .find(|w| w[0] == "--out")
                .map(|w| PathBuf::from(&w[1]))
                .unwrap_or_else(|| PathBuf::from("imported-recipes"));
            let mut n = 0;
            for i in report.items {
                match i.disposition {
                    ImportDisposition::Actionable(r) => {
                        println!("导入\t{}\t{}", r.id, i.text);
                        if !dry {
                            fs::create_dir_all(&out).map_err(|e| e.to_string())?;
                            fs::write(
                                out.join(format!("{}.json", r.id)),
                                serde_json::to_vec_pretty(&r).map_err(|e| e.to_string())?,
                            )
                            .map_err(|e| e.to_string())?
                        }
                        n += 1
                    }
                    ImportDisposition::DisplayOnly(w) => {
                        println!("仅展示\t{}\t原因：{}", i.text, w)
                    }
                    ImportDisposition::Skipped(w) => println!("跳过\t{}\t原因：{}", i.text, w),
                }
            }
            println!(
                "{} 条可导入{}",
                n,
                if dry {
                    "（dry-run，未写入）"
                } else {
                    "，已写入"
                }
            );
            Ok(())
        }
        Some("history") => history_cli(&a[1..]),
        Some("preset") => match a.get(1).map(String::as_str) {
            Some("list") => {
                for p in list_presets()? {
                    println!("{}\t{}", p.name, p.recipe_id)
                }
                Ok(())
            }
            Some("show") => {
                // 打的就是**落盘的 TOML 文本**：既能人看，也能直接
                // `preset show x > presets/x.toml` 拷走，不用在两套格式之间换算。
                let base = config_dir();
                let name = a.get(2).ok_or("缺少名称")?;
                let preset = load_preset_in(&base, name)?;
                let text = toml::to_string_pretty(&preset).map_err(|e| e.to_string())?;
                print!("{text}");
                Ok(())
            }
            Some("delete") => delete_preset(a.get(2).ok_or("缺少名称")?),
            Some("save") => {
                let name = a.get(2).ok_or("缺少名称")?;
                let id = a.get(3).ok_or("缺少 Recipe id")?;
                let mut v = BTreeMap::new();
                for p in a.iter().skip(4) {
                    let (k, x) = p.split_once('=').ok_or("参数必须是 key=value")?;
                    v.insert(k.into(), x.into());
                }
                save_preset(&Preset {
                    name: name.clone(),
                    recipe_id: id.clone(),
                    values: v,
                })
            }
            _ => Err(
                "用法：preset list|show 名称|delete 名称|save 名称 RecipeID [key=value…]".into(),
            ),
        },
        Some("queue") => {
            if a.get(1).map(String::as_str) != Some("run") {
                return Err(format!(
                    "用法：queue run tasks.json [--jobs N]（默认 {DEFAULT_JOBS}）"
                ));
            }
            let f = a.get(2).ok_or("缺少任务文件")?;
            let jobs = parse_jobs(a);
            // 任务是自成进程组的，Ctrl-C 不会自动传到它们那里；接过来自己处理。
            install_interrupt_handler();
            let tasks: Vec<QueueTask> =
                serde_json::from_slice(&fs::read(f).map_err(|e| e.to_string())?)
                    .map_err(|e| e.to_string())?;
            let r = run_queue(tasks, jobs, CancellationToken::default());
            for x in &r.results {
                println!("{}\t退出码={}\t耗时={}ms", x.id, x.exit_code, x.elapsed_ms);
                if let Some(e) = &x.error {
                    eprintln!("{}：{}", x.id, e)
                }
            }
            println!("队列退出码：{}", r.exit_code);
            if r.exit_code == 0 {
                return Ok(());
            }
            // 被取消就按 Ctrl-C 的惯例退出（130）：脚本里 `cmd || ...` 能分清
            // 「是用户叫停的」还是「任务自己失败了（1）」。
            if r.exit_code == 130 {
                eprintln!("队列已取消");
                std::process::exit(130);
            }
            Err("队列中有任务失败".into())
        }
        _ => Err("支持 recipe、import、preset、queue".into()),
    }
}
fn recipe_history_path() -> PathBuf {
    std::env::var_os("TOOLBOX_HUB_DATA")
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME")
                .map(PathBuf::from)
                .map(|p| p.join(".local/share/toolbox-hub"))
        })
        .unwrap_or_else(|| PathBuf::from(".toolbox-hub"))
        .join("recipe-history.jsonl")
}

fn append_recipe_history(
    id: &str,
    values: &BTreeMap<String, String>,
    exit_code: i32,
) -> Result<(), String> {
    let path = recipe_history_path();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let row = serde_json::json!({"time": std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_secs(), "recipe_id": id, "values": values, "exit_code": exit_code});
    use std::io::Write;
    let mut f = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|e| e.to_string())?;
    writeln!(f, "{}", row).map_err(|e| e.to_string())
}

fn run_recipe_cli(args: &[String]) -> Result<(), String> {
    let file = args.first().ok_or("用法：recipe run <recipe.json> [key=value…] [--confirm] [--confirm-destructive --audit 说明]")?;
    let recipe: Recipe = serde_json::from_slice(&fs::read(file).map_err(|e| e.to_string())?)
        .map_err(|e| e.to_string())?;
    validate_recipe(&recipe)?;
    let confirm = args.iter().any(|a| a == "--confirm");
    let confirm_destructive = args.iter().any(|a| a == "--confirm-destructive");
    let audit = args
        .windows(2)
        .find(|w| w[0] == "--audit")
        .map(|w| w[1].as_str())
        .unwrap_or("")
        .trim();
    if recipe.safety == SafetyLevel::Caution && !confirm {
        return Err("此 Recipe 标记为 caution；先检查 manifest，再显式添加 --confirm".into());
    }
    if recipe.safety == SafetyLevel::Destructive {
        eprintln!(
            "DESTRUCTIVE 审计：id={} source={} command={} args={:?}",
            recipe.id, recipe.source, recipe.command, recipe.args
        );
        if !confirm_destructive
            || audit.is_empty()
            || std::env::var("TOOLBOX_HUB_CONFIRM_DESTRUCTIVE").as_deref() != Ok("1")
        {
            return Err("默认拦截 destructive。需要 --confirm-destructive、--audit <审计说明>，并设置 TOOLBOX_HUB_CONFIRM_DESTRUCTIVE=1".into());
        }
        eprintln!("审计说明：{audit}");
    }
    let mut values = BTreeMap::new();
    for pair in args.iter().skip(1) {
        if pair.starts_with("--") || !pair.contains('=') {
            continue;
        }
        let (key, value) = pair.split_once('=').ok_or("参数必须为 key=value")?;
        if !recipe.params.iter().any(|p| p.name == key) {
            return Err(format!("未声明的参数：{key}"));
        }
        values.insert(key.to_string(), value.to_string());
    }
    for p in &recipe.params {
        if p.required && !values.contains_key(&p.name) {
            return Err(format!("缺少必填参数：{}", p.name));
        }
    }
    let argv = recipe
        .args
        .iter()
        .map(|arg| {
            let mut value = arg.clone();
            for (key, replacement) in &values {
                value = value.replace(&format!("{{{key}}}"), replacement);
            }
            value
        })
        .collect::<Vec<_>>();
    eprintln!("执行 argv：{} {:?}", recipe.command, argv);
    let status = Command::new(&recipe.command)
        .args(&argv)
        .status()
        .map_err(|e| format!("启动命令失败：{e}"))?;
    let code = status.code().unwrap_or(1);
    append_recipe_history(&recipe.id, &values, code)?;
    if code == 0 {
        Ok(())
    } else {
        Err(format!("Recipe 执行失败，退出码 {code}"))
    }
}

fn history_cli(args: &[String]) -> Result<(), String> {
    let path = recipe_history_path();
    match args.first().map(String::as_str) {
        Some("list") => {
            let Ok(text) = fs::read_to_string(path) else {
                println!("暂无 Recipe 历史");
                return Ok(());
            };
            for (i, line) in text.lines().enumerate() {
                if let Ok(row) = serde_json::from_str::<serde_json::Value>(line) {
                    println!(
                        "{}\t{}\t退出码={}",
                        i + 1,
                        row["recipe_id"].as_str().unwrap_or("?"),
                        row["exit_code"]
                    );
                }
            }
            Ok(())
        }
        Some("replay") => {
            let number: usize = args
                .get(1)
                .ok_or("用法：history replay <序号> [featured 目录]")?
                .parse()
                .map_err(|_| "历史序号无效")?;
            let text = fs::read_to_string(path).map_err(|e| e.to_string())?;
            let row = text
                .lines()
                .nth(number.saturating_sub(1))
                .ok_or("历史记录不存在")?;
            let row: serde_json::Value = serde_json::from_str(row).map_err(|e| e.to_string())?;
            let id = row["recipe_id"].as_str().unwrap_or("");
            let featured = args
                .get(2)
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("registry/featured"));
            let index: serde_json::Value = serde_json::from_slice(
                &fs::read(featured.join("index.json")).map_err(|e| e.to_string())?,
            )
            .map_err(|e| e.to_string())?;
            let known = index["recipes"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|v| v["id"].as_str().map(str::to_string))
                .collect::<Vec<_>>();
            let values = serde_json::from_value::<BTreeMap<String, String>>(row["values"].clone())
                .unwrap_or_default();
            let (resolved, values) = replay_values(id, &values, &known);
            if resolved.is_empty() {
                println!("原 Recipe「{id}」已失效；以下参数已保留，可手动选择替代动作：");
            } else {
                println!("Recipe：{resolved}；参数已回填，执行前仍需重新确认。");
            }
            println!(
                "{}",
                serde_json::to_string_pretty(&values).map_err(|e| e.to_string())?
            );
            Ok(())
        }
        _ => Err("用法：history list | history replay <序号>".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn navi_import_normal_missing_dangerous() {
        let r = import_text("navi", "% files\n# list\nls -lah <path>\nrm -rf <path>\n").unwrap();
        assert!(matches!(
            r.items[0].disposition,
            ImportDisposition::Actionable(_)
        ));
        assert!(matches!(
            r.items[1].disposition,
            ImportDisposition::Skipped(_)
        ));
        assert!(import_text("navi", "").unwrap().items.is_empty())
    }
    #[test]
    fn cheat_import_normal_missing_dangerous() {
        let r = import_text(
            "cheat",
            "# network\ncurl -I https://example.com\nrm -rf /tmp/foo\n",
        )
        .unwrap();
        assert!(matches!(
            r.items[0].disposition,
            ImportDisposition::Actionable(_)
        ));
        assert!(matches!(
            r.items[1].disposition,
            ImportDisposition::Skipped(_)
        ));
        assert!(import_text("cheat", "").unwrap().items.is_empty())
    }
    #[test]
    fn tldr_import_normal_missing_dangerous() {
        let r = import_text(
            "tldr",
            "# ls\n- List files: ls -la\n- Remove: rm -rf /tmp/foo\n",
        )
        .unwrap();
        assert!(matches!(
            r.items[0].disposition,
            ImportDisposition::DisplayOnly(_)
        ));
        assert!(matches!(
            r.items[1].disposition,
            ImportDisposition::Skipped(_)
        ));
        assert!(import_text("tldr", "").unwrap().items.is_empty())
    }

    /// 真实的 tldr 页面（照抄 `git-log` 的形状）：标题、`> 说明`、以及**两行一条**
    /// 的示例 —— 说明一行，命令一行且包着反引号。
    ///
    /// 这份 fixture 存在的理由：早先的用例只写了单行 `- 说明: 命令`，于是「真页面
    /// 一条都导不出来」这个 bug 一路绿着溜过去了。跑真页面才发现。
    const REAL_TLDR_PAGE: &str = "\
# git-log

> Show a history of commits.
> More information: https://git-scm.com/docs/git-log.

- Show the sequence of commits starting from the current one:

`git log`

- Show the history of a particular file:

`git log {{path/to/file}}`

- Show the last N number of commits:

`git log -n {{number_of_commits}}`

- Remove everything:

`rm -rf /tmp/foo`
";

    #[test]
    fn a_real_tldr_page_yields_the_commands_from_the_backticked_lines() {
        let report = import_text("tldr", REAL_TLDR_PAGE).expect("import");

        // 反引号里的命令要被取出来；说明行、`>` 行、标题都不算命令。
        let shown: Vec<&str> = report
            .items
            .iter()
            .filter(|item| matches!(item.disposition, ImportDisposition::DisplayOnly(_)))
            .map(|item| item.text.as_str())
            .collect();
        assert_eq!(
            shown,
            vec![
                "git log",
                "git log {{path/to/file}}",
                "git log -n {{number_of_commits}}",
            ],
            "真页面的命令在反引号里，必须全部取出来"
        );

        // 危险那条仍然要被拦下（它也是反引号包着的，所以这条同时证明：
        // 「剥反引号」没有把安全闸门一起绕过去）。
        assert!(matches!(
            report.items.last().map(|item| &item.disposition),
            Some(ImportDisposition::Skipped(_))
        ));

        // tldr 的示例一律只展示，不落盘成可执行 Recipe —— 这是设计，不是遗漏。
        assert!(
            !report
                .items
                .iter()
                .any(|item| matches!(item.disposition, ImportDisposition::Actionable(_))),
            "tldr 示例不该直接变成可执行 Recipe"
        );
    }

    #[test]
    fn tldr_command_extraction_handles_both_layouts() {
        assert_eq!(tldr_command("`git log`").as_deref(), Some("git log"));
        assert_eq!(
            tldr_command("`tar -xzf {{archive}}`").as_deref(),
            Some("tar -xzf {{archive}}")
        );
        assert_eq!(
            tldr_command("- List files: ls -la").as_deref(),
            Some("ls -la"),
            "单行写法也要认"
        );
        // 说明行：命令在下一行，这一行不是命令。
        assert_eq!(tldr_command("- Show the last N commits:"), None);
        assert_eq!(
            tldr_command("- Show more information about this command"),
            None
        );
        assert_eq!(tldr_command("``"), None, "空反引号不是命令");
        assert_eq!(tldr_command("> More information: https://x"), None);
    }

    // ── cheat 的 YAML front matter ──────────────────────────────────────────

    /// 真的 cheat 文件长这样：`---` 包起来的一段 YAML，然后才是正文。
    const CHEAT_WITH_FRONT_MATTER: &str = "\
---
syntax: bash
tags: [networking, dns]
---
# 查 DNS
dig +short <domain>
";

    #[test]
    fn cheat_front_matter_is_metadata_not_commands() {
        let report = import_text("cheat", CHEAT_WITH_FRONT_MATTER).expect("import");

        // 之前这里会多出两条垃圾 Recipe：程序名是 `syntax:` 和 `tags:`。
        assert_eq!(
            report.items.len(),
            1,
            "front matter 不该产出任何条目：{:?}",
            report.items
        );
        let ImportDisposition::Actionable(recipe) = &report.items[0].disposition else {
            panic!("正文那条应当可导入：{:?}", report.items[0].disposition);
        };
        assert_eq!(recipe.command, "dig");
        assert_eq!(recipe.args, vec!["+short", "{domain}"]);
        // 元数据要被**记录**下来（但不参与 argv）：
        assert!(
            recipe.description.contains("syntax: bash"),
            "解释器提示该记进说明：{}",
            recipe.description
        );
        assert!(recipe.tags.contains(&"networking".to_string()));
        assert!(recipe.tags.contains(&"dns".to_string()));
        assert!(recipe.tags.contains(&"cheat".to_string()));
    }

    #[test]
    fn cheat_without_front_matter_still_imports_normally() {
        let report = import_text("cheat", "# 网络\ncurl -I https://example.com\n").expect("import");
        assert_eq!(report.items.len(), 1);
        let ImportDisposition::Actionable(recipe) = &report.items[0].disposition else {
            panic!("应当可导入");
        };
        assert_eq!(recipe.command, "curl");
        // 没有 front matter 就不该硬塞标签。
        assert!(!recipe.tags.contains(&"networking".to_string()));
    }

    #[test]
    fn an_unterminated_front_matter_is_reported_instead_of_imported_as_commands() {
        let report = import_text("cheat", "---\nsyntax: bash\necho hello\n").expect("import");
        assert_eq!(report.items.len(), 1);
        let ImportDisposition::Skipped(reason) = &report.items[0].disposition else {
            panic!(
                "没有闭合的 `---` 时剩下的 YAML 不该被当成命令：{:?}",
                report.items[0].disposition
            );
        };
        assert!(
            reason.contains("闭合"),
            "原因要说清楚是 front matter 没闭合：{reason}"
        );
        // 报的是**整份文件**的问题，所以只出一条，而不是把每行 YAML 都报一遍。
        assert_eq!(report.items.len(), 1);
    }

    #[test]
    fn cheat_front_matter_only_understands_the_two_documented_keys() {
        // 认不出来的键一律当元数据忽略 —— 绝不能漏到「命令」那一侧去。
        let report = import_text(
            "cheat",
            "---\nfoo: bar\nsyntax: \"python3\"\ntags: a, b\n---\nls -l\n",
        )
        .expect("import");
        assert_eq!(report.items.len(), 1);
        let ImportDisposition::Actionable(recipe) = &report.items[0].disposition else {
            panic!("只剩正文那条");
        };
        assert_eq!(recipe.command, "ls");
        assert!(recipe.description.contains("python3"), "引号要剥掉");
        assert!(
            recipe.tags.contains(&"a".to_string()),
            "无方括号的写法也要认"
        );
        assert!(recipe.tags.contains(&"b".to_string()));
    }

    #[test]
    fn front_matter_parsing_is_line_based_and_predictable() {
        let (meta, body) = cheat_front_matter("---\nsyntax: bash\ntags: [x]\n---\nls\n");
        assert_eq!(meta.syntax.as_deref(), Some("bash"));
        assert_eq!(meta.tags, vec!["x".to_string()]);
        assert_eq!(body, Some(4), "正文从第 5 行（下标 4）开始");

        // 没有 front matter：正文从第一行开始，不吞任何东西。
        let (meta, body) = cheat_front_matter("ls -l\n");
        assert_eq!(meta, super::FrontMatter::default());
        assert_eq!(body, Some(0));

        // 开头有空行也认：0/1 是空行，2 是 `---`，4 是闭合的 `---`，正文从 5 开始。
        let (meta, body) = cheat_front_matter("\n\n---\nsyntax: zsh\n---\nls\n");
        assert_eq!(meta.syntax.as_deref(), Some("zsh"));
        assert_eq!(body, Some(5));

        assert_eq!(parse_tag_list("[a, \"b\", 'c']"), vec!["a", "b", "c"]);
        assert!(parse_tag_list("[]").is_empty());
    }
    #[test]
    fn destructive_cannot_claim_safe() {
        let r = Recipe {
            id: "wipe".into(),
            title: "wipe".into(),
            description: "danger".into(),
            command: "rm".into(),
            args: vec!["-rf".into(), "/".into()],
            params: vec![],
            safety: SafetyLevel::Safe,
            source: "test".into(),
            tags: vec![],
        };
        assert!(validate_recipe(&r).is_err())
    }
    #[test]
    fn missing_replay_degrades_and_keeps_values() {
        let v = BTreeMap::from([("path".into(), "/tmp".into())]);
        let (id, g) = replay_values("removed", &v, &[]);
        assert!(id.is_empty());
        assert_eq!(v, g)
    }
    #[test]
    fn concurrent_queue_isolates_failures() {
        let t = vec![
            QueueTask {
                id: "ok".into(),
                program: "true".into(),
                args: vec![],
            },
            QueueTask {
                id: "bad".into(),
                program: "false".into(),
                args: vec![],
            },
            QueueTask {
                id: "missing".into(),
                program: "/no/such/program".into(),
                args: vec![],
            },
        ];
        let r = run_queue(t, 3, CancellationToken::default());
        assert_eq!(r.results.len(), 3);
        assert_ne!(r.exit_code, 0);
        assert!(
            r.results
                .iter()
                .any(|x| x.id == "missing" && x.exit_code == 127)
        )
    }
    #[test]
    fn cancelled_queue_does_not_start_tasks() {
        let t = CancellationToken::default();
        t.cancel();
        let r = run_queue(
            vec![QueueTask {
                id: "slow".into(),
                program: "sleep".into(),
                args: vec!["5".into()],
            }],
            1,
            t,
        );
        assert_eq!(r.exit_code, 130);
        assert!(r.results.is_empty())
    }
    // ── 队列：取消与并发 ────────────────────────────────────────────────────

    /// 拼一串命令行参数（测试里写得短一点）。
    fn argv(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|part| part.to_string()).collect()
    }

    /// 进程还活着吗（僵尸不算活着 —— 容器里 PID 1 常常不回收孤儿）。
    fn alive(pid: i32) -> bool {
        if let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat"))
            && let Some(rest) = stat.rsplit_once(')').map(|(_, rest)| rest)
            && rest.split_whitespace().next() == Some("Z")
        {
            return false;
        }
        // SAFETY: `kill(pid, 0)` 只做存在性与权限检查，不发信号。
        unsafe { libc::kill(pid, 0) == 0 }
    }

    /// 取消必须把**整个进程组**带走。
    ///
    /// 任务是个脚本，脚本自己拉起的后台子进程也是这个组的一员；只杀直接子进程的话
    /// 它会变成孤儿接着跑 —— 「取消了但还有东西在写文件」就是这么来的。
    /// 和交互式执行层的 `terminate_takes_the_whole_process_group` 是同一条规矩。
    #[test]
    fn cancelling_the_queue_takes_the_whole_process_group() {
        let dir = temp_dir("queue-pgid");
        let pidfile = dir.join("grandchild.pid");

        // `sh -c 'sleep 300 & echo $! > pidfile; wait'`：直接子进程是 sh，
        // 孙子进程是 sleep —— 只杀 sh 的话 sleep 会被 init 收养并继续活着。
        let script = format!("sleep 300 & echo $! > {}; wait", pidfile.display());
        let task = QueueTask {
            id: "script".into(),
            program: "/bin/sh".into(),
            args: vec!["-c".into(), script],
        };

        let cancel = CancellationToken::default();
        let watcher = cancel.clone();
        // 等孙子进程的 pid 落盘之后再取消（否则取消可能发生在脚本还没跑起来时，
        // 那样测的就不是「带走孙子进程」了）。
        let wait_for_pid = pidfile.clone();
        let killer = std::thread::spawn(move || {
            for _ in 0..200 {
                if let Ok(text) = std::fs::read_to_string(&wait_for_pid)
                    && text.trim().parse::<i32>().is_ok()
                {
                    break;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            watcher.cancel();
        });

        let summary = run_queue(vec![task], 1, cancel);
        killer.join().expect("取消线程");

        let grandchild: i32 = std::fs::read_to_string(&pidfile)
            .expect("孙子进程的 pid 应当写进文件")
            .trim()
            .parse()
            .expect("pid");
        let mut gone = false;
        for _ in 0..250 {
            if !alive(grandchild) {
                gone = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(gone, "孙子进程 {grandchild} 还活着 —— 进程组没一起带走");
        assert!(
            summary.results.iter().any(|result| result.cancelled),
            "被取消的任务要如实标成 cancelled：{:?}",
            summary.results
        );

        std::fs::remove_dir_all(&dir).expect("cleanup");
    }

    #[test]
    fn queue_concurrency_defaults_to_one_and_ignores_junk() {
        assert_eq!(DEFAULT_JOBS, 1, "默认一次只跑一个任务");
        assert_eq!(parse_jobs(&argv(&["queue", "run", "t.json"])), 1);
        assert_eq!(parse_jobs(&argv(&["queue", "run", "t.json", "--jobs"])), 1);
        assert_eq!(
            parse_jobs(&argv(&["queue", "run", "t.json", "--jobs", "abc"])),
            1,
            "给的不是数字时退回默认值，而不是 panic"
        );
        assert_eq!(
            parse_jobs(&argv(&["queue", "run", "t.json", "--jobs", "0"])),
            1,
            "0 个 worker 等于永远跑不完，退回默认值"
        );
        assert_eq!(
            parse_jobs(&argv(&["queue", "run", "t.json", "--jobs", "4"])),
            4
        );
    }

    /// Ctrl-C（中断标志）必须变成「取消」，而不是让任务变孤儿继续跑。
    ///
    /// 用**注入的标志**而不是发真信号：真 SIGINT 会打到整个测试进程，
    /// 并行跑的其他用例会被连累。发信号那一段（`install_interrupt_handler`）
    /// 本身只有一行 `libc::signal`，真正的逻辑在这里。
    #[test]
    fn an_interrupt_cancels_the_running_tasks() {
        let interrupt = Arc::new(AtomicBool::new(false));
        let cancel = CancellationToken::default();

        // 任务跑起来之后再「按 Ctrl-C」：一个短命线程在 200ms 后置位。
        let flag = interrupt.clone();
        let flipper = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(200));
            flag.store(true, Ordering::SeqCst);
        });

        let started = Instant::now();
        let summary = run_queue_with(
            vec![QueueTask {
                id: "long".into(),
                program: "sleep".into(),
                args: vec!["30".into()],
            }],
            1,
            cancel.clone(),
            &interrupt,
        );
        flipper.join().expect("中断线程");

        assert!(
            started.elapsed() < Duration::from_secs(10),
            "中断之后必须很快收工，实际等了 {:?}",
            started.elapsed()
        );
        assert!(
            cancel.is_cancelled(),
            "中断应当转成取消令牌，worker 才知道要收尾"
        );
        assert_eq!(summary.exit_code, 130, "被中断的队列退出码是 130");
        assert!(
            summary.results.iter().any(|result| result.cancelled),
            "被取消的任务要如实标出来：{:?}",
            summary.results
        );
    }

    /// 默认并发数真的会体现在并行度上：两个任务必须**一个跑完再跑下一个**。
    ///
    /// 用「锁文件」判重：任务开始时如果锁还在，说明上一个还没跑完 —— 那就是重叠。
    /// 默认（1 个 worker）下这条必须**一个重叠都没有**，这是硬保证，不是时间上的
    /// 大概齐。为了证明这个探针真的能测出重叠，再用 2 个 worker 跑一次，
    /// 必须观察到重叠（最多试几次，避免极端调度下的偶发）。
    #[test]
    fn the_default_concurrency_never_overlaps_tasks() {
        let overlap_probe = |jobs: usize| -> (usize, usize) {
            let dir = temp_dir("queue-serial");
            let lock = dir.join("lock");
            let overlaps = dir.join("overlaps");
            // `mkdir` 是原子的：抢到的持有 200ms，抢不到的说明有人正在跑 ——
            // 用 `[ -e ]` + `touch` 那种「先看后动」的写法测不出重叠（两个任务会
            // 在对方 touch 之前就把检查做完了，实测正是如此）。
            let script = format!(
                "if mkdir {lock} 2>/dev/null; then sleep 0.2; rmdir {lock}; \
                 else echo x >> {overlaps}; fi",
                lock = lock.display(),
                overlaps = overlaps.display()
            );
            let tasks = (0..2)
                .map(|index| QueueTask {
                    id: format!("t{index}"),
                    program: "/bin/sh".into(),
                    args: vec!["-c".into(), script.clone()],
                })
                .collect::<Vec<_>>();

            let summary = run_queue(tasks, jobs, CancellationToken::default());
            let overlapping = std::fs::read_to_string(&overlaps)
                .map(|text| text.lines().count())
                .unwrap_or(0);
            std::fs::remove_dir_all(&dir).expect("cleanup");
            (overlapping, summary.results.len())
        };

        let (overlapping, ran) = overlap_probe(DEFAULT_JOBS);
        assert_eq!(ran, 2, "两个任务都该跑完");
        assert_eq!(
            overlapping, 0,
            "默认并发下两个任务重叠了 {overlapping} 次 —— 说明默认不是串行"
        );

        // 探针自检：并行跑时它**必须**能看出重叠，否则上面那个 0 什么也证明不了。
        let mut observed = 0;
        for _ in 0..5 {
            let (overlapping, _) = overlap_probe(2);
            if overlapping > 0 {
                observed = overlapping;
                break;
            }
        }
        assert!(
            observed > 0,
            "两个 worker 时都没观察到重叠 —— 这个探针测不出重叠，上面的结论不可信"
        );
    }

    #[test]
    fn preset_toml_roundtrip() {
        let p = Preset {
            name: "x".into(),
            recipe_id: "git-status".into(),
            values: BTreeMap::from([("path".into(), "/tmp".into())]),
        };
        let text = toml::to_string_pretty(&p).expect("序列化");
        assert_eq!(toml::from_str::<Preset>(&text).expect("反序列化"), p);
    }

    /// 碰到早期版本的 JSON 预设，要**说清楚怎么回事**，而不是「文件不存在」。
    #[test]
    fn a_legacy_json_preset_gets_an_actionable_error() {
        let base = temp_dir("preset-legacy");
        let dir = base.join("presets");
        std::fs::create_dir_all(&dir).expect("mkdir");
        std::fs::write(
            dir.join("old.json"),
            "{\"name\":\"old\",\"recipe_id\":\"git-status\",\"values\":{}}",
        )
        .expect("write");

        let error = load_preset_in(&base, "old").expect_err("该报错");
        assert!(error.contains("JSON"), "要认出这是旧格式：{error}");
        assert!(error.contains("preset save"), "要给出怎么办：{error}");
        // 而且它不该被当成一份可用的预设列出来。
        assert!(list_presets_in(&base).expect("list").is_empty());

        std::fs::remove_dir_all(&base).expect("cleanup");
    }

    // ── 预设落盘 ────────────────────────────────────────────────────────────

    fn temp_dir(tag: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("toolbox-hub-ecosystem-{tag}-{nanos}"));
        std::fs::create_dir_all(&dir).expect("mkdir");
        dir
    }

    #[test]
    fn preset_survives_a_real_round_trip_through_disk() {
        let base = temp_dir("preset");
        let preset = Preset {
            name: "daily-log".into(),
            recipe_id: "git-log".into(),
            values: BTreeMap::from([("count".into(), "20".into())]),
        };

        save_preset_in(&base, &preset).expect("save");
        let written = base.join("presets/daily-log.toml");
        assert!(
            written.is_file(),
            "预设必须真的写到 <配置目录>/presets/<名字>.toml"
        );
        // 落盘的是 TOML 文本（配置目录里的东西一律 TOML，人能直接打开改）。
        let text = std::fs::read_to_string(&written).expect("读回来");
        assert!(
            text.contains("recipe_id = \"git-log\"") && text.contains("[values]"),
            "预设文件该是 TOML 且人看得懂：\n{text}"
        );
        assert!(
            !base.join("presets/daily-log.json").exists(),
            "不该再写 JSON 那一份"
        );

        // 读回来要和写进去的一模一样 —— 不只是「能解析」。
        assert_eq!(load_preset_in(&base, "daily-log").expect("load"), preset);

        // 覆盖写：同名再存一次应当替换，而不是追加出第二份。
        let updated = Preset {
            values: BTreeMap::from([("count".into(), "50".into())]),
            ..preset.clone()
        };
        save_preset_in(&base, &updated).expect("overwrite");
        assert_eq!(load_preset_in(&base, "daily-log").expect("reload"), updated);
        assert_eq!(list_presets_in(&base).expect("list").len(), 1);

        delete_preset_in(&base, "daily-log").expect("delete");
        assert!(load_preset_in(&base, "daily-log").is_err());
        assert!(list_presets_in(&base).expect("list").is_empty());

        std::fs::remove_dir_all(&base).expect("cleanup");
    }

    #[test]
    fn listing_presets_skips_corrupt_files_instead_of_failing() {
        let base = temp_dir("preset-corrupt");
        save_preset_in(
            &base,
            &Preset {
                name: "good".into(),
                recipe_id: "git-status".into(),
                values: BTreeMap::new(),
            },
        )
        .expect("save");
        // 坏文件（手工编辑坏了 / 磁盘写了一半）：不该让整张列表消失。
        std::fs::write(base.join("presets/broken.toml"), "这不是 TOML {{{").expect("write");
        std::fs::write(base.join("presets/notes.txt"), "hello").expect("write");
        // 早期版本的 JSON 预设（没发布过）：当无关文件忽略，不要试图解析它。
        std::fs::write(base.join("presets/legacy.json"), "{\"name\":\"legacy\"}").expect("write");

        let listed = list_presets_in(&base).expect("list");
        assert_eq!(listed.len(), 1, "只应列出能解析的那一份");
        assert_eq!(listed[0].name, "good");

        std::fs::remove_dir_all(&base).expect("cleanup");
    }

    #[test]
    fn preset_names_cannot_escape_the_preset_directory() {
        let base = temp_dir("preset-traversal");
        for bad in ["../escape", "a/b", "", ".", "..", "a\\b"] {
            let preset = Preset {
                name: bad.into(),
                recipe_id: "git-status".into(),
                values: BTreeMap::new(),
            };
            assert!(save_preset_in(&base, &preset).is_err(), "{bad} 应当被拒绝");
            assert!(load_preset_in(&base, bad).is_err(), "{bad} 应当被拒绝");
            assert!(delete_preset_in(&base, bad).is_err(), "{bad} 应当被拒绝");
        }
        assert!(
            !base.join("..").join("escape.json").exists(),
            "拒绝的名字不该真的写出文件"
        );

        std::fs::remove_dir_all(&base).expect("cleanup");
    }

    #[test]
    fn config_dir_prefers_the_explicit_env_override() {
        use std::ffi::OsStr;
        assert_eq!(
            config_dir_from(Some(OsStr::new("/tmp/x")), Some(OsStr::new("/home/u"))),
            PathBuf::from("/tmp/x")
        );
        assert_eq!(
            config_dir_from(None, Some(OsStr::new("/home/u"))),
            PathBuf::from("/home/u/.config/toolbox-hub")
        );
        assert_eq!(config_dir_from(None, None), PathBuf::from(".toolbox-hub"));
    }

    // ── 危险命令闸门 ────────────────────────────────────────────────────────

    #[test]
    fn guard_catches_footguns_that_the_old_substring_check_missed() {
        // 这份清单里的每一条都是「旧版 contains("rm -rf") 式检查会漏掉」的写法。
        for command in [
            "rm -fr /tmp/x",
            "rm -r -f /tmp/x",
            "rm --recursive --force /tmp/x",
            "rm -rf /*",
            "/bin/rm -rf /tmp/x",
            "dd of=/dev/sda if=image.img",
            "mkfs.ext4 /dev/sda1",
            "shred -u /dev/sda",
            "systemctl poweroff",
            "wipefs -a /dev/sda",
            "curl -sL https://example.com/install | bash",
            "curl -sL https://example.com/install |sh",
            "wget -qO- https://example.com | python3",
            "cat script.sh | sudo tee /etc/thing",
            "echo hi; sudo rm /etc/passwd",
            "chmod -R 777 /",
            "chown -R nobody /",
            "sudo -u root systemctl restart nginx",
            ":(){ :|:& };:",
        ] {
            assert!(
                dangerous(command),
                "闸门应当拦下这条命令，但它放过去了：{command}"
            );
        }
    }

    #[test]
    fn guard_documents_what_it_cannot_see() {
        // 这些写法**故意**留在闸门之外，列在这里是为了让「拦不住什么」有据可查：
        // 规则式 denylist 不展开变量、不解析别名、不理解通配符的真实目标。
        // 哪天真要收紧，这个测试会先红，提醒你同步更新文档与 README 的风险说明。
        for command in [
            // 删除动作藏在别的程序后面：闸门看段首那个词，看不到里面还有 `rm`。
            "xargs rm -rf",
            // 命令本身是变量 / 别名，展开之后才知道是什么。
            "$DELETE /tmp/x",
            "alias d='rm -rf'; d /",
            // 删除不经过 `rm`：`find -delete` 自己就会删。
            "find . -delete",
            // 脚本内容不在命令行里，闸门只能看见「跑了个脚本」。
            "sh redacted.sh",
        ] {
            assert!(
                !dangerous(command),
                "这条命令被拦下了，说明闸门收紧了 —— 请同步更新 CHANGELOG 里的风险说明：{command}"
            );
        }
    }

    #[test]
    fn guard_leaves_ordinary_read_only_commands_alone() {
        for command in [
            "ls -lah /tmp",
            "grep -RIn pattern .",
            "git status --short",
            "ps -eo pid,comm,%cpu --sort=-%cpu",
            "tar -tzf archive.tar.gz",
            "curl -I https://example.com",
            "sed -n 1,20p file.txt",
            "rm file.txt",
            "chmod 644 file.txt",
            "systemctl status nginx",
            "dd if=/dev/zero of=/dev/null count=1",
        ] {
            assert!(!dangerous(command), "只读 / 常规命令被误拦：{command}");
        }
    }

    #[test]
    fn every_shipped_featured_recipe_passes_the_guard_and_validation() {
        // 官方精选库不能出现「自己的闸门会拦下自己」的矛盾：CI 里 build / check 跑
        // 的就是这些文件，一条不合格就得当场发现。
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("registry/featured");
        let mut checked = 0;
        for entry in std::fs::read_dir(&dir).expect("featured 目录应当存在") {
            let path = entry.expect("entry").path();
            if path.extension().and_then(|e| e.to_str()) != Some("json")
                || path.file_name().and_then(|n| n.to_str()) == Some("index.json")
            {
                continue;
            }
            let recipe: Recipe =
                serde_json::from_slice(&std::fs::read(&path).expect("read")).expect("parse");
            let full = std::iter::once(recipe.command.as_str())
                .chain(recipe.args.iter().map(String::as_str))
                .collect::<Vec<_>>()
                .join(" ");
            assert!(
                !dangerous(&full),
                "{} 被闸门判为危险，但它标成 {:?}",
                path.display(),
                recipe.safety
            );
            validate_recipe(&recipe).unwrap_or_else(|e| panic!("{} 校验失败：{e}", path.display()));
            checked += 1;
        }
        assert!(checked >= 30, "精选库至少要有 30 条，实际 {checked}");
    }

    #[test]
    fn a_destructive_recipe_needs_an_explicit_opt_in() {
        let recipe = Recipe {
            id: "wipe".into(),
            title: "wipe".into(),
            description: "danger".into(),
            command: "rm".into(),
            args: vec!["-rf".into(), "/tmp/x".into()],
            params: vec![],
            safety: SafetyLevel::Destructive,
            source: "test".into(),
            tags: vec![],
        };
        // 标了 destructive 就能通过校验 —— 但执行侧仍然默认拦截（见 run_recipe_cli）。
        assert!(validate_recipe(&recipe).is_ok());

        // 同一份 Recipe 标成 safe / caution 必须被拒：不能靠「改个字段」绕过闸门。
        for safety in [SafetyLevel::Safe, SafetyLevel::Caution] {
            let mut risky = recipe.clone();
            risky.safety = safety;
            assert!(validate_recipe(&risky).is_err());
        }
    }

    // ── 导入器边界 ──────────────────────────────────────────────────────────

    #[test]
    fn importer_rejects_unknown_formats_instead_of_guessing() {
        assert!(import_text("bash-history", "ls\n").is_err());
        assert!(import_text("", "ls\n").is_err());
    }

    #[test]
    fn importer_keeps_placeholders_but_never_shell_syntax() {
        let report = import_text(
            "navi",
            "ls <path>\ntar -xzf {{archive}}\ncat a > b\necho $(whoami)\n",
        )
        .expect("import");

        let actionable = report
            .items
            .iter()
            .filter_map(|item| match &item.disposition {
                ImportDisposition::Actionable(recipe) => Some(recipe),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(actionable.len(), 2, "只有前两条能变成 argv 模板");
        assert_eq!(actionable[0].params[0].name, "path");
        assert_eq!(
            actionable[1].params[0].name, "archive",
            "{{}} 与 <> 同等对待"
        );
        assert_eq!(actionable[1].args, vec!["-xzf", "{archive}"]);

        // 重定向与命令替换必须落进「仅展示」：字符串拆分会改变语义。
        assert!(matches!(
            report.items[2].disposition,
            ImportDisposition::DisplayOnly(_)
        ));
        assert!(matches!(
            report.items[3].disposition,
            ImportDisposition::DisplayOnly(_)
        ));
    }

    #[test]
    fn imported_recipes_validate_against_the_real_schema() {
        let report = import_text("cheat", "git log --oneline -n <count>\n").expect("import");
        let ImportDisposition::Actionable(recipe) = &report.items[0].disposition else {
            panic!("应当能导入：{:?}", report.items[0].disposition);
        };
        assert_eq!(recipe.command, "git");
        assert_eq!(recipe.args, vec!["log", "--oneline", "-n", "{count}"]);
        assert_eq!(recipe.source, "cheat");
        validate_recipe(recipe).expect("导入产物必须过校验");
        // 导入的东西一律是「先检查再执行」，不能自己封自己 safe 之外还要更宽松。
        assert_eq!(recipe.safety, SafetyLevel::Safe);
    }
}
