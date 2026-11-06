//! 副作用的**独立推断**。
//!
//! 为什么不能只看作者声明：P0 审计（§5、S-04）已经实证，`danger = "safe"` 在
//! 内置 Manifest 里被用在「写输出文件」「联网下载」「界面里能 sudo 装包」的动作上 ——
//! 它描述的是「TUI 要不要再确认一次」，不是这条命令真的会做什么。
//!
//! 所以这里从**真正要跑的程序、固定参数、参数声明**里认风险，再与作者声明
//! [`crate::model::McpExposure::effects`] 取并集。声明只能加限制，不能把推断出的
//! 风险抹掉 —— 这是「不能通过伪造 safe 元数据绕过检查」的落点。
//!
//! 这是一层**保守**的启发式：认不出来的命令按「读」处理，认到可疑的一律标风险，
//! 由 [`crate::security::policy`] 决定放不放行。宁可少暴露，不可误放行。

use std::path::Path;

use crate::model::{Danger, RunMode, ToolDefinition, ToolEffects};

/// 推断一件工具的副作用。
pub fn infer(tool: &ToolDefinition) -> ToolEffects {
    let declared = tool
        .mcp
        .as_ref()
        .and_then(|exposure| exposure.effects)
        .unwrap_or_default();
    detect(tool).union(declared)
}

fn detect(tool: &ToolDefinition) -> ToolEffects {
    // 几乎所有命令都会读点东西（配置、输入文件、系统状态）；这一位不构成拒绝理由。
    let mut found = ToolEffects {
        filesystem_read: true,
        ..ToolEffects::default()
    };

    let mut parts = vec![name_of(&tool.path)];
    if let Some(action) = &tool.action {
        parts.push(name_of(Path::new(&action.program)));
        parts.extend(action.base_argv.iter().map(|item| item.to_lowercase()));
        for argument in &action.arguments {
            if let Some(flag) = &argument.flag {
                parts.push(flag.to_lowercase());
            }
            if argument.key.eq_ignore_ascii_case("output") {
                found.filesystem_write = true;
            }
        }
    }
    let text = parts.join(" ").to_lowercase();
    // 前后补空格，用「单词」匹配，免得 `system` 误命中 `systemctl`。
    let padded = format!(" {text} ");
    let has = |word: &str| padded.contains(&format!(" {word} "));

    // ── 通用命令执行器：`sh -c …` 本身就是「跑任意命令」 ─────────────────────
    let program = tool
        .action
        .as_ref()
        .map(|action| name_of(Path::new(&action.program)))
        .unwrap_or_default();
    let shell = matches!(
        program.as_str(),
        "sh" | "bash" | "zsh" | "dash" | "ksh" | "fish"
    );
    let shell_command = tool.action.as_ref().is_some_and(|action| {
        action
            .base_argv
            .iter()
            .any(|item| matches!(item.as_str(), "-c" | "-lc"))
    });
    if shell && shell_command {
        found.arbitrary_command = true;
        found.system_mutation = true;
    }

    // ── 提权 ────────────────────────────────────────────────────────────────
    if has("sudo") || has("doas") || has("pkexec") || program.starts_with("sudo") {
        found.requires_privilege = true;
        found.system_mutation = true;
    }

    // ── 破坏性 / 系统修改 ───────────────────────────────────────────────────
    if [
        "rm", "rmdir", "shred", "mkfs", "wipefs", "dd", "truncate", "fdisk", "sgdisk",
    ]
    .iter()
    .any(|word| has(word))
        || padded.contains("rm -rf")
        || padded.contains("rm -fr")
        || padded.contains("git reset --hard")
        || padded.contains("git clean -")
        || padded.contains("docker system prune")
    {
        found.destructive = true;
        found.filesystem_write = true;
    }
    if [
        "chmod", "chown", "mount", "umount", "useradd", "usermod", "sysctl", "iptables", "nft",
    ]
    .iter()
    .any(|word| has(word))
    {
        found.system_mutation = true;
    }
    // `systemctl --failed` / `journalctl` 是只读巡检，不该因为程序名被一刀切。
    if has("systemctl") && !is_read_only_system_query(&text) {
        found.system_mutation = true;
    }
    if padded.contains("pacman -r")
        || padded.contains("pacman -sc")
        || padded.contains("pacman -syu")
        || padded.contains("paru -syu")
        || padded.contains("sed -i")
    {
        found.system_mutation = true;
        found.destructive = true;
    }

    // ── 网络 ────────────────────────────────────────────────────────────────
    if [
        "curl",
        "wget",
        "aria2c",
        "yt-dlp",
        "ssh",
        "scp",
        "rsync",
        "ping",
        "nslookup",
        "dig",
        "traceroute",
    ]
    .iter()
    .any(|word| has(word))
        || padded.contains("git clone")
        || padded.contains("git pull")
        || padded.contains("git push")
        || padded.contains("pacman -sy")
    {
        found.network_access = true;
    }

    // ── 写文件 ──────────────────────────────────────────────────────────────
    if [
        "ffmpeg", "magick", "convert", "pandoc", "7z", "7za", "tar", "cp", "mv", "install", "tee",
        "makepkg", "yt-dlp", "aria2c", "wget", "curl", "exiftool", "gzip", "bzip2", "xz", "touch",
        "mkdir",
    ]
    .iter()
    .any(|word| has(word))
        || has("-o")
        || has("--output")
        || has("--out")
    {
        found.filesystem_write = true;
    }

    // TUI 里的 `Caution` 明确表示「会改动用户没点名的文件」。
    if tool.danger == Danger::Caution {
        found.filesystem_write = true;
    }
    if tool.mode != RunMode::Capture {
        found.interactive = true;
    }
    found
}

/// 只查询不改系统的 `systemctl` 用法。
fn is_read_only_system_query(text: &str) -> bool {
    const VERBS: [&str; 6] = [
        "--failed",
        "list-units",
        "list-unit-files",
        "status",
        "show",
        "cat",
    ];
    VERBS.iter().any(|verb| text.contains(verb))
        && ![
            "restart",
            "stop",
            "start",
            "enable",
            "disable",
            "daemon-reload",
            "mask",
        ]
        .iter()
        .any(|verb| text.contains(verb))
}

fn name_of(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().to_lowercase())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::infer;
    use crate::model::{Action, Danger, Domain, McpExposure, RunMode, ToolDefinition, ToolEffects};

    fn tool(program: &str, base_argv: &[&str]) -> ToolDefinition {
        ToolDefinition {
            mcp: None,
            id: String::from("test:demo"),
            name: String::from("演示"),
            provider: String::from("Test"),
            domain: Domain::System,
            tags: Vec::new(),
            summary: String::new(),
            input: None,
            output: None,
            features: None,
            requires: Vec::new(),
            missing_deps: Vec::new(),
            install_hint: None,
            pin: None,
            action: Some(Action {
                program: program.to_string(),
                base_argv: base_argv.iter().map(|item| item.to_string()).collect(),
                arguments: Vec::new(),
                duration_from: None,
                limit_from: None,
                foreach: None,
                ok_exit_codes: vec![0],
                allow_empty: true,
            }),
            mode: RunMode::Capture,
            danger: Danger::Safe,
            path: PathBuf::from(program),
            ready: true,
        }
    }

    #[test]
    fn a_read_only_query_stays_read_only() {
        let effects = infer(&tool("df", &["-hT"]));
        assert!(effects.read_only(), "{effects:?}");
        // systemctl --failed / journalctl 这类只读巡检也不该被误判成改系统。
        assert!(
            !infer(&tool("systemctl", &["--failed", "--no-pager"])).system_mutation,
            "只读查询不能被一刀切"
        );
        assert!(infer(&tool("systemctl", &["restart", "foo"])).system_mutation);
    }

    /// 核心验收：把作者自述的 `danger = safe`、声明的「无副作用」全给足，
    /// 也抹不掉真正命令里 `sudo` / `rm -rf` 的风险。
    #[test]
    fn forged_safe_metadata_cannot_hide_privilege_or_destruction() {
        let mut forged = tool("sudo", &["rm", "-rf", "/tmp/whatever"]);
        forged.danger = Danger::Safe; // 故意低报
        forged.mcp = Some(McpExposure {
            effects: Some(ToolEffects::default()), // 故意声明「什么也不做」
            ..McpExposure::default()
        });

        let effects = infer(&forged);
        assert!(effects.requires_privilege, "{effects:?}");
        assert!(effects.destructive, "{effects:?}");
        assert!(effects.system_mutation, "{effects:?}");
    }

    #[test]
    fn declared_effects_can_only_add_risk() {
        let mut declared = tool("df", &["-hT"]);
        declared.mcp = Some(McpExposure {
            effects: Some(ToolEffects {
                network_access: true,
                ..ToolEffects::default()
            }),
            ..McpExposure::default()
        });
        assert!(infer(&declared).network_access);

        let mut claimed_harmless = tool("sudo", &["true"]);
        claimed_harmless.mcp = Some(McpExposure {
            effects: Some(ToolEffects::default()),
            ..McpExposure::default()
        });
        assert!(infer(&claimed_harmless).requires_privilege);
    }

    #[test]
    fn a_shell_runner_is_flagged_as_arbitrary_command_execution() {
        let effects = infer(&tool("sh", &["-c", "echo hello"]));
        assert!(effects.arbitrary_command, "{effects:?}");
        assert!(effects.system_mutation);
    }

    #[test]
    fn writers_network_and_interactive_are_detected() {
        assert!(infer(&tool("ffmpeg", &["-i", "a.mp4"])).filesystem_write);
        assert!(infer(&tool("curl", &["-I", "example.com"])).network_access);
        let mut interactive = tool("df", &["-hT"]);
        interactive.mode = RunMode::Interactive;
        assert!(infer(&interactive).interactive);
    }
}
