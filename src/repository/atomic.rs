//! 原子写盘：先写临时文件再 rename，断电 / 崩溃也不会留半截文件。
//!
//! 账本、仓库配置、索引缓存这些文件一旦写坏，代价都是「这个包 / 这个仓库
//! 再也管不了，只能手工清理」，所以一律走这里，不许直接 `fs::write` 覆写。

use std::fs;
use std::io;
use std::path::Path;

/// 原子写入（父目录不存在会自动建）。
pub fn write(target: &Path, bytes: &[u8]) -> Result<(), String> {
    write_atomic(target, None, |handle| {
        use std::io::Write;
        handle
            .write_all(bytes)
            .map_err(|error| format!("写不了 {}：{error}", target.display()))
    })
}

/// 把 `source` 复制到 `target`（同样先写临时文件再 rename）。
///
/// 装包时载荷可能是几十 MB 的二进制：先 `fs::read` 进内存再写一遍，峰值内存
/// 是文件的两倍。这里走 `io::copy`，内存只有内核的拷贝缓冲。
pub fn copy(source: &Path, target: &Path) -> Result<(), String> {
    copy_with_mode(source, target, None)
}

/// 同 [`copy`]，但落地为 0o755。
pub fn copy_executable(source: &Path, target: &Path) -> Result<(), String> {
    copy_with_mode(source, target, Some(0o755))
}

fn copy_with_mode(source: &Path, target: &Path, mode: Option<u32>) -> Result<(), String> {
    write_atomic(target, mode, |handle| {
        let mut input = fs::File::open(source)
            .map_err(|error| format!("读不了 {}：{error}", source.display()))?;
        io::copy(&mut input, handle)
            .map_err(|error| format!("写不了 {}：{error}", target.display()))?;
        Ok(())
    })
}

/// 写盘骨架：临时文件 → 填内容 → 设权限 → rename。
///
/// 两种载荷（内存里的字节、磁盘上另一个文件）只有「填内容」这一步不同，
/// 其余（父目录、临时名、权限位、失败清理）必须完全一致 —— 所以收成一个
/// 带填充闭包的函数，别让两条路径各写一份。
fn write_atomic(
    target: &Path,
    mode: Option<u32>,
    fill: impl FnOnce(&mut fs::File) -> Result<(), String>,
) -> Result<(), String> {
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

    let run = || -> Result<(), String> {
        use std::io::Write;
        let mut handle = fs::File::create(&temp)
            .map_err(|error| format!("写不了 {}：{error}", temp.display()))?;
        fill(&mut handle)?;
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

    match run() {
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

    #[test]
    fn copy_lands_content_and_mode() {
        let base = std::env::temp_dir().join(format!("toolbox-hub-atomic3-{}", std::process::id()));
        let source = base.join("payload.bin");
        fs::create_dir_all(&base).expect("base");
        fs::write(&source, b"payload").expect("source");
        let target = base.join("deep/tool");

        copy_executable(&source, &target).expect("copy");
        assert_eq!(fs::read(&target).expect("read"), b"payload");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&target).expect("meta").permissions().mode() & 0o777;
            assert_eq!(mode, 0o755, "可执行载荷必须落地为 0755");
        }

        // 源文件没了要如实报错，而不是留一个空目标或半截文件。
        fs::remove_file(&target).expect("rm");
        assert!(copy(&base.join("nope"), &target).is_err());
        assert!(!target.exists(), "失败时不该留下目标文件");
        fs::remove_dir_all(&base).expect("cleanup");
    }
}
