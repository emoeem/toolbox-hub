# Toolbox Hub：项目说明与实现记录

更新日期：2026-10-03

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

这是一个 Arch Linux 优先的工具。Rust 二进制动态链接 `libalpm`，因此构建需要 `pacman` 提供的 `libalpm.so` 和 `libalpm.pc`，以及 `pkgconf`。`ratatui-image` 的 `build.rs` 还会用 pkg-config 探测 `chafa >= 1.8.0`（pkg-config 名就叫 `chafa`）：本仓库关了它的默认 feature（只留 `crossterm`），这个探测目前不生效，但 Arch 上仍按 `sudo pacman -S --needed base-devel git rust pkgconf chafa` 装齐（见 README 的「构建前提」）。包管理相关能力也依赖 pacman 数据库。

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

### 文件选择：外部文件管理器优先

- `toolbox-hub ui pick` 改为**优先交给外部文件管理器**（默认 yazi）：它自带预览、书签、多选和批量操作，浏览体验比内置选择器好得多，也免去继续维护一套浏览逻辑。内置选择器降级为替补，只在外部程序没装、没有控制终端或起不来时顶上，保证脚本不会因为少一个外部程序就跑不动。
- 外部程序的 stdin/stdout/stderr 三条标准流全部接到 `/dev/tty`：脚本这边通常是 `path=$(toolbox-hub ui pick)`，stdout 是管道，只允许有结果。
- 两类答案分开取：文件走 `--chooser-file`（在 yazi 里 Enter 触发），目录走 `--cwd-file`（退出时所在的目录）—— 在 yazi 里「打开」一个目录是*进去*，不会触发 chooser，这和 ranger 的 `--choosedir` 是同一套路。
- 新增 `ExternalPick::{Unavailable, Cancelled}` 区分「用不了」和「用户取消」：前者要退回替补继续服务，后者是最终答案。混为一谈会让「没装 yazi」变成「用户取消了」，脚本静悄悄地什么也拿不到。
- 环境变量 `TOOLBOX_HUB_FILE_MANAGER` 与主界面按 `y` 浏览目录共用，设成 `builtin` / `none` 则明确用内置的。`--filter` 只有内置选择器认，走外部程序时会往 stderr 说明一句，而不是悄悄丢掉它。

### 长耗时操作的实时反馈

- 四处仍在 UI 线程上同步执行的地方全部改走既有的「线程 + mpsc + 每帧 poll」通道。此前 `runtime::run_captured` 是同步阻塞的，命中它的调用会让整个 TUI 冻住：没有实时输出、没有已用时、按 `q` 也不响应。
- 历史记录里的「重跑」改走 `App::replay_from_history`，排进后台队列。这条路上的 argv 什么都有（转码、打包 —— 恰恰是最久的那批），是四处里最危险的一处。
- AUR 的 PKGBUILD 抓取（`Ctrl+X` / `Ctrl+K`）改走 `Worker::pkgbuild` 的独立线程：它是网络操作，扔在 UI 线程上要冻几秒，扔进常驻网络线程又会让并行的 AUR 搜索排在它后面。
- 健康视图（系统维护）本身不需要改：`ensure_health()` 每次进入该模式都会重扫，不存在「数据陈旧」的问题。本轮确认后未作改动。

### 「打包」域：接自己的 Arch 软件仓库

- 新增第八个域 `Domain::Packaging`（标签「打包」）：它管的是**自己的软件包仓库**（一堆 PKGBUILD + CI 自动构建 + repo 分支发布）的日常 —— 状态总览、环境自检、审计、构建计划与 DAG、并行构建、构建时序、修复中心、同步 AUR 源、跟踪/排查 CI、从仓库安装。追加在 `Domain::ALL` 最后，所以你记住的 `1`-`7` 一个都没动，新域是 `8`。
- 域本身只是分类：**空着也是合法的**（UI 显示「Provider 待接入」），所以加一个域不影响没有这类仓库的人。
- 接法和 sing-box 那套完全一致，只有三件东西在 toolbox 这边：`manifests/pkgbuild-source.toml`（20 个动作，`program` 写**裸命令名** `tbx-pkgbuild`）、`scripts/pkgbuild-source/tbx-pkgbuild`（几行的转发脚本，只负责**找仓库**）、PKGBUILD 里把它装到 `/usr/bin`。真正的逻辑全在那个仓库自己的 `manage.sh` 里，所以那边随便改，工具箱不用重编译。
- 仓库位置不能写死（那是用户的私有仓库）：`$TOOLBOX_HUB_PKGBUILD_SRC` > `~/pkgbuild-source` > `~/code/pkgbuild-source`。找不到时脚本给一句明确的指引并退 `2`，工具箱里则显示「依赖缺失」+ 安装办法；源码用户跑 `./scripts/pkgbuild-source/install.sh`。
- `mode` 是**实测**分出来的，不是按感觉标的：逐个函数扫过对 `fzf`/`prompt_value`/`select_one`/`sudo` 的依赖之后，只有 5 个纯只读动作走 `capture`（`dashboard`/`audit`/`timing`/`doctor`/`pull`，输出留在输出视图里），其余 15 个要选包或提权，走 `interactive` 把终端交给它。第一遍扫描漏了间接调用（`check_local_updates` 末尾其实有个 fzf 浏览器），按函数体判定比按印象判定可靠。
- 这个域暴露了一个**真 bug**：`runtime::execute_tools`（interactive 那条路）把 `argv` 写死成空 `Vec`，于是「本体命令写在 `base_argv` 里」的动作全变成光跑程序名 —— 实测 `manage.sh plan` 变成了光跑 `manage.sh`（弹出它自己的主菜单）。capture 那条路此前踩过同一个坑。现在两条路共用 `runtime::default_argv()`，并有一条测试钉住。

## 验证记录

本轮验证结果：

- `cargo fmt --all -- --check` 通过。
- `cargo test`：268 项通过，14 项 ignored。
- `cargo clippy --all-targets -- -D warnings` 通过。
- `cargo build --release` 通过；`ldd` 未发现 `libchafa`；`--version` 和 `--help` 正常。
- `makepkg --printsrcinfo` 通过。
- `toolbox-hub ui pick` 端到端验证（假文件管理器 + `script` 提供的真 pty，8 个用例）：外部程序写进自己 stdout/stderr 的画面噪音**一个字节都没有**进入调用方的 stdout；单选、多选、多行只取首条、取消（退出码 1）、`--dir-only` 取退出目录、`builtin` 强制内置、外部程序不存在时退回内置，均符合预期；完全没有终端时退出码 2 并给出可读提示。
- 验证过程踩到一个坑（记下来省下一次）：`script -c` 用的是 `$SHELL`（本机为 fish），而 fish 不认 `$?`，会让整条命令直接不执行、且失败得很安静。验证脚本里必须显式 `SHELL=/bin/sh`。

ignored 测试涉及本机 `/etc`、真实 `$HOME`、pacman/paru 状态、联网或会创建文件的外部命令，不属于当前默认 CI 的稳定测试集合。运行前应先阅读各测试的 `#[ignore]` 说明。

## 尚待维护者决定

- ~~确定真实许可证~~：已定为 **MIT**（`LICENSE` + `Cargo.toml` 的 `license` + `PKGBUILD` 的 `license=('MIT')` + `package()` 装到 `/usr/share/licenses/`）。
- 将 PKGBUILD 的占位 GitHub URL 和本地源码包来源替换为真实仓库/tag 地址，并生成校验和及 `.SRCINFO` 后再发布 AUR。
- 当前 CLI 支持包管理操作，但还没有通用的 `toolbox-hub run <manifest-id>` 或 JSON 输出接口；这属于独立的 CLI 产品设计，不在本轮修改范围内。
- 默认 CI 继续运行格式、Clippy 和常规测试；定时真实环境冒烟测试需要先为每个 ignored 测试准备明确的依赖和隔离策略。