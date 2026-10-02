# Toolbox Hub：项目说明与实现记录

更新日期：2026-10-02

本文面向项目维护者，说明 Toolbox Hub 要解决的问题、内部结构、扩展方法，以及近期一轮依赖、正确性和 TUI 改进。日常安装和按键说明仍以 [README](../README.md) 为准。

## 项目是做什么的

Toolbox Hub 是一个面向 Linux 命令行工具的统一入口，提供 TUI 和 CLI 两种使用方式。它把分散在系统中的命令、用户脚本和软件包操作组织成可搜索、可填写参数、可确认、可回看的工作台。

它不是桌面应用启动器，也不重新实现 ffmpeg、pacman 等工具的功能。实际工作仍由这些程序完成；Toolbox Hub 负责发现它们、构建参数、运行进程、捕获结果和记录历史。

项目主要解决以下问题：

- 命令选项难记：通过 manifest 把常用参数变成表单，并在执行前展示最终 argv。
- 工具分散：把内置动作、用户脚本和包管理能力放进统一的域、分类和搜索视图。
- 批量操作麻烦：支持标记多个工具、重复路径参数和 `foreach` 批量任务。
- 结果难追溯：记录执行历史、状态和脱敏参数；捕获模式支持查看、复制和保存输出。
- Arch 包数据不方便查询：直接使用 libalpm 获取结构化包信息，而不是解析 pacman 的人类可读文本。

## 运行边界

这是一个 Arch Linux 优先的工具。Rust 二进制动态链接 `libalpm`，因此构建需要 `pacman` 提供的 `libalpm.so` 和 `libalpm.pc`，以及 `pkgconf`。包管理相关能力也依赖 pacman 数据库。

系统中的外部命令是可选能力：缺少依赖时动作仍可以被发现，但会显示缺失状态和安装提示。只有实际执行对应动作时才需要安装它们。

## 代码结构

| 目录或模块 | 职责 |
| --- | --- |
| `src/model/` | 域、工具、参数、危险等级和 argv 构造等基础模型 |
| `src/providers/` | 发现本地脚本、解析内置/用户 manifest、检查依赖 |
| `src/registry/` | 汇总 Provider 结果、去重、排序和重载 |
| `src/app/` | TUI 状态机、输入处理、表单、选择器和包管理状态 |
| `src/ui/` | Ratatui 绘制；主布局和相关鼠标命中共享布局计算 |
| `src/runtime/` | 子进程启动、输出捕获、进程组取消和终端交接 |
| `src/packages/` | libalpm 查询、AUR/新闻请求、维护检查和后台 worker |
| `manifests/` | 编译进程序的内置 CLI 动作定义 |
| `scripts/` | 随项目分发的运维脚本，例如 sing-box 管理动作 |
| `packaging/` | Arch `PKGBUILD`、man page 和本地打包说明 |

大致调用关系如下：

```text
启动参数
  ├─ CLI 模式 ──> 包管理命令 / 结果输出
  └─ TUI 模式 ──> Registry 发现工具 ──> App 状态机 ──> UI 绘制
                                      ├─ Worker: libalpm + HTTP
                                      └─ Runtime: 外部命令与输出
```

命令参数始终以 `argv` 数组传给子进程，不拼接 shell 命令字符串。修改系统的包操作会先展示将要执行的命令；危险动作按配置要求再次确认。

## 如何扩展

### 添加一个 CLI 动作

把 TOML 放进 `~/.config/toolbox-hub/tools.d/`；开发内置动作时放进 `manifests/`，并在 `src/providers/manifest.rs` 的 `BUNDLED` 列表注册。一个 manifest 可以声明 `program`、`base_argv`、域、分类、执行模式、危险等级和多个 `[[action.argument]]`。

参数的 `kind` 决定表单控件，`flag`、`placement`、`repeatable` 和 `separator` 决定 argv 构造方式。路径字段可使用文件选择器；`foreach` 可以把一组输入展开为多次独立运行。

在 TOML 中明确标注 `danger = "safe"` 或 `danger = "caution"`，并为可选外部程序提供 `install` 提示。改动内置 manifest 后至少运行 `cargo test providers::manifest::tests::bundled_manifests_all_parse_without_warnings`。

### 添加用户脚本

脚本放入 `~/.config/toolbox-hub/tools/`，在文件头部添加 `# <脚本名>:<键>=<值>` 元数据。常用键有 `summary`、`domain`、`tags`、`requires`、`mode` 和 `danger`。元数据扫描只读取文件头部，不应依赖脚本正文中的任意注释。

### 添加包管理能力

优先在 `src/packages/` 的数据/查询层定义行为，再通过 `worker.rs` 的请求和响应交给 `app/package_view.rs` 消费，最后在 `ui/packages.rs` 展示。数据库不可用、网络失败和“查询成功但结果为空”必须保持不同状态，不能把错误转换成空列表。

## 本轮实现记录

### 依赖与 release 产物

- 关闭 `ratatui-image` 默认 feature，只保留 `crossterm` 后端，避免构建环境是否安装 chafa 改变最终动态依赖。
- 将 `image` 改为显式启用 BMP、GIF、JPEG、PNG、TIFF 和 WebP 解码器；预览判断使用 `ImageFormat::reading_enabled()`，所以 AVIF 等未编入解码器的格式不会被误报为可预览。
- 为 release 配置 `strip = true`、`lto = "thin"` 和 `codegen-units = 1`。
- 本机 release 二进制约 5.82 MB；检查 `ldd` 未发现 `libchafa`。仍需注意它依赖 `libalpm`，所以 Arch 包必须声明 `pacman` 运行时依赖。

### 运行正确性和资源使用

- 后台任务运行时，搜索、表单、历史、文件选择器和工作目录输入态的 `q`/`Esc` 不再被误当作取消任务。
- libalpm 数据库打不开时，孤儿包、系统维护和文件完整性检查返回错误状态，不再伪装成空结果或“一切正常”。
- 脚本头部采用流式读取，并限制在 16 KiB、40 行以内；搜索和全库浏览使用已有的本地版本映射，避免对每个同步包重复查询本地数据库。
- 同步库年龄使用最旧同步库文件的时间；包下载总量用哈希集合匹配；批量 `foreach` 不在 UI 线程逐文件等待 `ffprobe`。
- 执行器检查文件的执行权限；路径选择器按参数定义的 separator 拆分/合并多值路径；自动新闻搜索会明确进入 loading 状态，并在失败后复位。
- ureq 3 的响应体读取默认已有 10 MiB 上限，因此没有再重复实现一层相同的全局上限。

### TUI 交互和显示

- 主界面窄屏布局、鼠标命中区域共用同一份 `MainLayout`；空间不足提示页不再响应列表点击。
- 修正主工具表表头偏移、软件包无表头结果的首行偏移、队列边框命中，并为输出视图启用滚轮。
- 窄屏域标签隐藏计数并压缩填充，保证最小支持宽度下七个域仍可见；中文帮助键名和包信息字段按终端显示列宽对齐。
- 底栏状态区根据内容宽度分配空间，秒级运行时间变化时触发重绘；包信息、新闻加载和窄屏行为有相应回归测试。

### 内置动作与包元数据

- `manifests/system.toml` 已在当前提交中提供六个系统只读动作：磁盘空间、目录占用、本次启动错误、失败的 systemd 单元、块设备和 PCI 设备。本轮为系统域增加了覆盖测试。
- 内置 manifest 的测试动作数和 native 动作列表改为从 TOML 推导，新增动作无需同步维护重复常量。
- `packaging/PKGBUILD` 增加 AUR、文件管理、媒体、文档、下载、PCI 和剪贴板等 `optdepends`；`makepkg --printsrcinfo` 已验证通过。

## 验证记录

本轮验证结果：

- `cargo fmt --all -- --check` 通过。
- `cargo test`：244 项通过，14 项 ignored。
- `cargo clippy --all-targets -- -D warnings` 通过。
- `cargo build --release` 通过；`ldd` 未发现 `libchafa`；`--version` 和 `--help` 正常。
- `makepkg --printsrcinfo` 通过。

ignored 测试涉及本机 `/etc`、真实 `$HOME`、pacman/paru 状态、联网或会创建文件的外部命令，不属于当前默认 CI 的稳定测试集合。运行前应先阅读各测试的 `#[ignore]` 说明。

## 尚待维护者决定

- 在 `packaging/PKGBUILD` 中确定真实许可证并加入许可证文件。目前仍是 `license=('unknown')`，不应由代码修改者替项目选择许可证。
- 将 PKGBUILD 的占位 GitHub URL 和本地源码包来源替换为真实仓库/tag 地址，并生成校验和及 `.SRCINFO` 后再发布 AUR。
- 当前 CLI 支持包管理操作，但还没有通用的 `toolbox-hub run <manifest-id>` 或 JSON 输出接口；这属于独立的 CLI 产品设计，不在本轮修改范围内。
- 默认 CI 继续运行格式、Clippy 和常规测试；定时真实环境冒烟测试需要先为每个 ignored 测试准备明确的依赖和隔离策略。