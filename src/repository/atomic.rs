//! 原子写盘：先写临时文件再 rename，断电 / 崩溃也不会留半截文件。
//!
//! 账本、仓库配置、索引缓存这些文件一旦写坏，代价都是「这个包 / 这个仓库
//! 再也管不了，只能手工清理」，所以一律走这里，不许直接 `fs::write` 覆写。

use std::fs;
use std::path::Path;

/// 原子写入（父目录不存在会自动建）。
pub fn write(target: &Path, bytes: &[u8]) -> Result<(), String> {
    write_with_mode(target, bytes, None)
}

/// 原子写入并设为 0o755（可执行载荷）。权限在 rename **之前**设好，
/// 最终文件不会以错误权限存在哪怕一瞬间。
pub fn write_executable(target: &Path, bytes: &[u8]) -> Result<(), String> {
    write_with_mode(target, bytes, Some(0o755))
}

fn write_with_mode(target: &Path, bytes: &[u8], mode: Option<u32>) -> Result<(), String> {
    let parent = target
        .parent()
        .ok_or_else(|| format!("{} 没有父目录", target.display()))?;
    fs::create_dir_all(parent).map_err(|error| format!("建不了 {}：{error}", parent.display()))?;
    let name = target
        .file_name()
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_else(|| String::from("file"));
    // 临时文件名带 pid + 线程 id：两个线程同时写同一个目标也不会互踩。
    let temp = parent.join(format!(
        ".{name}.toolbox-tmp-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));

    let fill = || -> Result<(), String> {
        use std::io::Write;
        let mut handle = fs::File::create(&temp)
            .map_err(|error| format!("写不了 {}：{error}", temp.display()))?;
        handle
            .write_all(bytes)
            .map_err(|error| format!("写不了 {}：{error}", temp.display()))?;
        handle
            .flush()
            .map_err(|error| format!("写不了 {}：{error}", temp.display()))?;
        if let Some(mode) = mode {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(&temp, fs::Permissions::from_mode(mode))
                    .map_err(|error| format!("设不了权限位 {}：{error}", temp.display()))?;
            }
        }
        Ok(())
    };

    match fill() {
        Ok(()) => fs::rename(&temp, target).map_err(|error| {
            let _ = fs::remove_file(&temp);
            format!("写不了 {}：{error}", target.display())
        }),
        Err(problem) => {
            let _ = fs::remove_file(&temp);
            Err(problem)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_creates_parents_and_lands_content() {
        let base = std::env::temp_dir().join(format!("toolbox-hub-atomic-{}", std::process::id()));
        let target = base.join("a/b/c.toml");
        write(&target, b"hello").expect("write");
        assert_eq!(fs::read(&target).expect("read"), b"hello");
        fs::remove_dir_all(&base).expect("cleanup");
    }

    #[test]
    fn overwritten_target_never_disappears() {
        let base = std::env::temp_dir().join(format!("toolbox-hub-atomic2-{}", std::process::id()));
        let target = base.join("f.toml");
        write(&target, b"old").expect("first");
        write(&target, b"new").expect("second");
        assert_eq!(fs::read(&target).expect("read"), b"new");
        // 目录里不该留下临时文件。
        let leftovers: Vec<_> = fs::read_dir(base.join(".")).expect("dir").collect();
        assert_eq!(leftovers.len(), 1, "不该有临时文件残留");
        fs::remove_dir_all(&base).expect("cleanup");
    }
}
