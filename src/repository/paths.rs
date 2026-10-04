//! 安装路径的安全规则。
//!
//! Repository 的内容是**不可信输入**（别人写的 index、别人打的包），
//! 所以「包里的相对路径」在变成「磁盘上的绝对路径」之前必须过一道闸：
//!
//! * 拒绝绝对路径（\`/etc/passwd\`）；
//! * 拒绝任何 \`..\` 段（\`../../.ssh/authorized_keys\`）；
//! * 拒绝空段、\`NUL\`、反斜杠（在别的平台上是一次目录穿越）；
//! * 拒绝以 \`~\` 开头（我们不做 shell 展开，那是另一条注入面）。
//!
//! 通过校验的路径再由 \`resolve_under\` 拼到安装根目录下面，并**再次**确认
//! 结果仍在根目录之内 —— 纵深防御，代价是几行代码。

use std::path::{Component, Path, PathBuf};

/// 校验一个包内相对路径，返回规范化后的形式（永远是相对路径）。
pub fn safe_relative(raw: &str) -> Result<PathBuf, String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(String::from("文件路径是空的"));
    }
    if trimmed.contains('\0') {
        return Err(format!("文件路径里有 NUL 字符：{trimmed:?}"));
    }
    if trimmed.contains('\\') {
        return Err(format!("文件路径里有反斜杠（不做跨平台展开）：{trimmed}"));
    }
    if trimmed.starts_with('~') {
        return Err(format!(
            "文件路径不该以 ~ 开头（不做 shell 展开）：{trimmed}"
        ));
    }
    if trimmed.starts_with('/') {
        return Err(format!("文件路径必须是相对的，不能是 {trimmed}"));
    }

    let mut out = PathBuf::new();
    for component in Path::new(trimmed).components() {
        match component {
            Component::Normal(part) => {
                let text = part.to_string_lossy();
                if text.trim().is_empty() {
                    return Err(format!("文件路径里有空目录名：{trimmed}"));
                }
                out.push(part);
            }
            Component::CurDir => {}
            Component::ParentDir => {
                return Err(format!("文件路径里有 ..（拒绝目录穿越）：{trimmed}"));
            }
            Component::RootDir | Component::Prefix(_) => {
                return Err(format!("文件路径必须是相对的：{trimmed}"));
            }
        }
    }

    if out.as_os_str().is_empty() {
        return Err(format!("文件路径没有有效内容：{trimmed}"));
    }
    Ok(out)
}

/// 把包内相对路径拼到根目录下，并确认结果没跑出根目录。
///
/// 注意 `Path::starts_with` **不解析** `..`：`/a/b/../evil` 在它看来仍然
/// 「以 /a/b 开头」。所以这里逐段确认，只接受纯正的相对路径。
pub fn resolve_under(root: &Path, relative: &Path) -> Result<PathBuf, String> {
    for component in relative.components() {
        if !matches!(component, Component::Normal(_)) {
            return Err(format!(
                "{} 不是干净的相对路径（只接受普通目录名）",
                relative.display()
            ));
        }
    }
    let joined = root.join(relative);
    if !joined.starts_with(root) {
        return Err(format!(
            "{} 落在了安装根目录之外（{}）",
            relative.display(),
            root.display()
        ));
    }
    Ok(joined)
}

/// 顶层组件（用来把 \`scripts/foo.sh\` 归类成 \`bin\`）。
#[allow(dead_code)] // 归类辅助
pub fn top_component(relative: &Path) -> Option<String> {
    relative
        .components()
        .next()
        .and_then(|component| match component {
            Component::Normal(part) => Some(part.to_string_lossy().to_string()),
            _ => None,
        })
}

/// 包 id / 仓库 id 这类标识符：只允许小写字母、数字、\`-\`、\`_\`、\`.\`。
///
/// 这些 id 会被拼进目录名，所以不能放任它带 \`/\` 或者 \`..\`。
pub fn safe_identifier(raw: &str) -> Result<String, String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(String::from("id 是空的"));
    }
    if trimmed.len() > 128 {
        return Err(String::from("id 太长（上限 128 字符）"));
    }
    let valid = trimmed
        .chars()
        .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || matches!(ch, '-' | '_' | '.'));
    if !valid {
        return Err(format!("id 只能用小写字母、数字、- _ . ：{trimmed}"));
    }
    if trimmed.starts_with('.') || trimmed.ends_with('.') {
        return Err(format!("id 不能以 . 开头或结尾：{trimmed}"));
    }
    Ok(trimmed.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ordinary_paths_pass_and_are_normalised() {
        assert_eq!(
            safe_relative("scripts/a.sh").unwrap(),
            PathBuf::from("scripts/a.sh")
        );
        assert_eq!(
            safe_relative("a/b/c.toml").unwrap(),
            PathBuf::from("a/b/c.toml")
        );
        // 冗余的 ./ 与重复斜杠被吃掉，不会变成第二份文件
        assert_eq!(
            safe_relative("./a//b.toml").unwrap(),
            PathBuf::from("a/b.toml")
        );
        assert_eq!(safe_relative("  a.sh  ").unwrap(), PathBuf::from("a.sh"));
    }

    #[test]
    fn traversal_and_absolute_paths_are_refused() {
        for bad in [
            "../x",
            "a/../../x",
            "..",
            "/etc/passwd",
            "~/x",
            "a\\b",
            "",
            "   ",
            "a/./../b",
        ] {
            assert!(safe_relative(bad).is_err(), "{bad:?} 应该被拒绝");
        }
    }

    #[test]
    fn null_bytes_and_empty_segments_are_refused() {
        assert!(safe_relative("a\0b").is_err());
        // 只有 . 的路径等于没有内容
        assert!(safe_relative(".").is_err());
    }

    #[test]
    fn resolve_under_keeps_children_inside() {
        let root = Path::new("/tmp/root");
        assert_eq!(
            resolve_under(root, Path::new("a/b")).unwrap(),
            PathBuf::from("/tmp/root/a/b")
        );
        // 纵深防御：即使 safe_relative 被绕过，这里也要拦住
        assert!(resolve_under(root, Path::new("../evil")).is_err());
        assert!(resolve_under(root, Path::new("/etc/x")).is_err());
    }

    #[test]
    fn identifiers_are_restricted_to_directory_safe_characters() {
        assert_eq!(safe_identifier("my-tools_1.2").unwrap(), "my-tools_1.2");
        for bad in ["", "..", "a/b", "A", "a b", ".hidden", "trailing."] {
            assert!(safe_identifier(bad).is_err(), "{bad:?} 应该被拒绝");
        }
        assert!(safe_identifier(&"x".repeat(200)).is_err());
    }

    #[test]
    fn top_component_names_the_first_directory() {
        assert_eq!(
            top_component(Path::new("scripts/a.sh")).as_deref(),
            Some("scripts")
        );
        assert_eq!(
            top_component(Path::new("a.toml")).as_deref(),
            Some("a.toml")
        );
    }
}
