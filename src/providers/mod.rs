//! 工具来源（Provider）层。
//!
//! 每个 Provider 负责把一类**已经存在**的命令行工具（脚本目录、TOML manifest、
//! 某个工具自己的配置……）翻译成 Provider 无关的 [`ToolDefinition`]。它不碰终端、
//! 不碰 UI、不关心选中状态，因此接一类新工具只需要写一个新 Provider 并在这里注册。
//!
//! 现在接入三条线：
//!
//! | Provider | 来源 | 工具主要落在 |
//! | --- | --- | --- |
//! | [`fftools`] | `~/.local/bin/fzf-*` 脚本头部元数据 | 媒体 |
//! | [`scripted`] | 任意带注解的可执行脚本 | 由注解决定 |
//! | [`manifest`] | TOML 写的「动作 + 参数」，包装已装 CLI | 由声明决定 |
//! | [`repository`] | 从 ToolHub 仓库安装的包（事实来源是账本） | 由包里的 manifest 决定 |
//!
//! 这里刻意**不**接桌面应用条目（`.desktop`）：工具箱服务的是命令行工具，
//! 不是应用启动器。

pub mod fftools;
pub mod manifest;
pub mod metadata;
pub mod repository;
pub mod scripted;

use std::path::Path;

use crate::model::{Domain, ToolDefinition};

/// 一次发现的产物：工具 + 不致命的问题。
///
/// 有了它，某个 manifest 写错了不会让整个 Provider 失败：其余工具照常出现，
/// 问题汇总到状态栏，用户才知道自己那份 TOML 哪一行写错了。
#[derive(Debug, Default)]
pub struct Discovery {
    pub tools: Vec<ToolDefinition>,
    /// 不影响其它工具的局部问题（例如某个 manifest 解析失败）。
    pub warnings: Vec<String>,
}

impl Discovery {
    /// 只有工具、没有问题 —— 文件扫描类 Provider 用这个。
    pub fn clean(tools: Vec<ToolDefinition>) -> Self {
        Self {
            tools,
            warnings: Vec::new(),
        }
    }
}

/// 一类工具来源。
pub trait Provider {
    /// 稳定短 id，用作工具 id 前缀，例如 `fftools`。
    fn id(&self) -> &'static str;

    /// 展示名，例如 `FFTools`。
    fn label(&self) -> &'static str;

    /// 该 Provider 在某个域下希望二级分类按什么顺序显示；默认不声明（按工具出现顺序）。
    ///
    /// 这只是**显示顺序提示**，不是「这个 Provider 属于哪个域」——
    /// 域只属于每个工具自己（[`ToolDefinition::domain`]），Provider 只回答
    /// 「工具从哪里来」，两者不要混在一起。
    fn tag_order(&self, _domain: Domain) -> &'static [&'static str] {
        &[]
    }

    /// 扫描并返回当前可用的工具。整体失败视为「该 Provider 暂时不可用」，不致命。
    fn discover(&self) -> std::io::Result<Discovery>;
}

/// 当前注册的 Provider 集合。
///
/// **顺序即优先级**：同一份脚本被多个 Provider 认领时，先注册的胜出（去重逻辑见
/// [`crate::registry::Registry::reload`]）。所以 FFTools 必须排在通用脚本 Provider
/// 前面，否则 `fzf-*` 会以「本地脚本」的身份在表格里出现第二次。
///
/// 通用脚本 Provider 会扫 `<bin_dir>`、`~/bin`、`~/.config/toolbox-hub/tools`、
/// `/usr/local/bin`（或被 `TOOLBOX_HUB_PATH` 覆盖），收录其中带注解的脚本。
///
/// Manifest Provider 会读 `~/.config/toolbox-hub/tools.d/*.toml` 以及编译进
/// 二进制的默认 manifest（或被 `TOOLBOX_HUB_MANIFEST_PATH` 覆盖）。
pub fn all(bin_dir: &Path) -> Vec<Box<dyn Provider>> {
    vec![
        Box::new(fftools::FftoolsProvider::new(bin_dir.to_path_buf())),
        Box::new(scripted::ScriptedProvider::with_defaults(bin_dir)),
        Box::new(manifest::ManifestProvider::with_defaults()),
        // 仓库排在最后：用户手写的 manifest 与内置动作优先，装来的包不能把
        // 同 id 的动作抢走（去重时先注册的胜出）。
        Box::new(repository::RepositoryProvider::with_defaults(bin_dir)),
    ]
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::all;

    #[test]
    fn providers_are_registered_in_priority_order() {
        let providers = all(Path::new("/nonexistent-bin-dir"));
        let ids: Vec<_> = providers.iter().map(|provider| provider.id()).collect();
        assert_eq!(
            ids,
            vec!["fftools", "scripted", "manifest", "repository"],
            "注册顺序即去重优先级"
        );

        let labels: Vec<_> = providers.iter().map(|provider| provider.label()).collect();
        assert_eq!(labels, vec!["FFTools", "本地脚本", "Manifest", "仓库"]);
    }
}
