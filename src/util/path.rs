//! 在 `PATH` 里找可执行文件 —— **全项目唯一一份**。
//!
//! 以前这段逻辑散在好几处：执行层（runtime）、Provider 元数据层（metadata）、
//! 仓库作者校验层（author），各自都有一份 `.map(|dir| dir.join(program))`。
//! 散着的代价不是多打几个字，而是**边界各写各的**：有的不排除目录（目录通常带
//! 执行位）、有的不要求可执行位、有的不挡空名字。于是同一个命令会出现
//! 「发现页说就绪、作者校验说缺失」这种自相矛盾。
//!
//! 这里给出三种粒度，调用点按需要挑：
//!
//! * [`is_executable`] —— 一个具体路径是不是可执行文件（目录不算）；
//! * [`find_on_path`] —— 只在 `PATH` 里找，返回第一个命中的路径；
//! * [`command_available`] —— 命令名能不能用：带 `/` 的按路径判，其余走 `PATH`。
//!
//! 只跑 Linux，没有 `.exe` / `PATHEXT` 之类的分支 —— 用不上的分支就是以后
//! 没人敢删的假代码。

use std::path::{Path, PathBuf};

/// 这个路径是不是「可执行文件」。
///
/// 三条一起成立才算：存在、是普通文件、带执行位。
///
/// 中间的「是普通文件」不能省：目录几乎总是带执行位（`drwxr-xr-x`），
/// 少这一条的话，`PATH` 上随便一个与命令同名的目录都会被当成命令存在。
pub fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    path.metadata()
        .map(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

/// 在 `PATH` 的各个目录里找 `program`，返回第一个命中的路径。
///
/// 只做 `PATH` 扫描。带 `/` 的写法（`/bin/sh`、`./run.sh`）是另一回事，
/// 由调用方按路径判断 —— 分不清这两种，就会把「用户指定的路径」也拿去每个
/// `PATH` 目录里拼一遍。要一步到位的判断用 [`command_available`]。
///
/// `PATH` 没设置（或整个 `PATH` 里都没有）都返回 `None`：找不到不是错误，
/// 是一句要展示给用户的结论。
pub fn find_on_path(program: &str) -> Option<PathBuf> {
    let paths = std::env::var_os("PATH")?;
    std::env::split_paths(&paths)
        .map(|dir| dir.join(program))
        .find(|candidate| is_executable(candidate))
}

/// 这个命令现在能用吗。
///
/// * 空名字一律 `false` —— `dir.join("")` 就是 `dir` 本身，不挡的话空依赖
///   会因为「目录存在」被判成已满足；
/// * 带 `/` 的按路径判（`/bin/sh` 行不行，只看那个文件自己）；
/// * 其余按命令名扫 `PATH`。
pub fn command_available(program: &str) -> bool {
    if program.is_empty() {
        return false;
    }
    if program.contains('/') {
        return is_executable(Path::new(program));
    }
    find_on_path(program).is_some()
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    fn temp(tag: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("toolbox-hub-util-path-{tag}-{nanos}"));
        fs::create_dir_all(&dir).expect("mkdir");
        dir
    }

    fn chmod(path: &Path, mode: u32) {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(mode)).expect("chmod");
    }

    /// 目录几乎总是带执行位，但目录不是可执行文件。
    #[test]
    fn a_directory_is_not_an_executable_file() {
        let dir = temp("dir");
        assert!(!is_executable(&dir), "目录不该算可执行文件");
        // /usr/bin 是 drwxr-xr-x，同样不能算
        assert!(!is_executable(Path::new("/usr/bin")));
        fs::remove_dir_all(&dir).expect("cleanup");
    }

    #[test]
    fn the_executable_bit_is_required() {
        let dir = temp("bit");
        let plain = dir.join("plain");
        fs::write(&plain, "#!/bin/sh\n").expect("write");
        chmod(&plain, 0o644);
        assert!(!is_executable(&plain), "没有执行位就不是可执行文件");

        chmod(&plain, 0o755);
        assert!(is_executable(&plain));
        fs::remove_dir_all(&dir).expect("cleanup");
    }

    #[test]
    fn command_available_handles_names_paths_and_empty_strings() {
        // 命令名走 PATH
        assert!(command_available("sh"));
        assert!(!command_available("toolbox-hub-definitely-missing-xyz"));
        // 带 `/` 的按路径判，而不是截成 basename 再拿去 PATH 里找
        assert!(command_available("/bin/sh"));
        assert!(!command_available("/nonexistent/dir/app"));
        // 空名字不能因为 `dir.join("") == dir` 且目录带执行位就被误判成可用
        assert!(!command_available(""));
    }

    #[test]
    fn find_on_path_returns_the_first_hit_or_nothing() {
        let found = find_on_path("sh").expect("本机一定有 sh");
        assert!(found.is_file(), "{found:?}");
        assert!(find_on_path("toolbox-hub-definitely-missing-xyz").is_none());
    }
}
