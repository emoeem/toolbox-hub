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

## 一轮修补（2026-10-10）

### 一致性

- 包操作跑完后清队列落盘（`app/mod.rs` 的 `Plan::Queue` 收尾）改用和 `close_packages`
  同一个来源（`self.queue_path` 覆盖），不再直接写默认路径 —— 覆盖过队列路径时（测试、
  将来可能的自定义路径）不会清错文件。
- 去掉生产路径上唯一一个 `expect`（`poll_job` 收尾），改成 `let ... else`。

### 可维护性

- 软件包中心四个模式的「选中下标」读写收敛成 `selected_index` / `set_selected_index`
  两个访问器：`rows_selected` / `move_row` / `select_row_edge` 不再各写一遍四路 `match`
  （加一种模式曾要改三处，漏一处就是「按键不动」）。「选中那行的包名」也收敛成
  `selected_name`（`current_row_name` 与 `remove_from_queue` 共用）。

### 两个新功能

- **配置目录自动重载**：新增 `src/watch.rs`，用 `notify`（Linux 走 inotify）盯着配置目录，
  改了 `tools.d/*.toml` 或 `tools/` 就自动 `reload`，不用每次手按 `Ctrl-R`。起不来
  （没有 inotify）就退化成手动，不拦程序。三条边界都是实测踩出来的，写进了模块文档和测试：

  - **只认用户手写的两个子树**。配置目录里还躺着 `state.toml`（收藏、上次工作目录）、
    `repositories.toml`、`packages.toml` —— 那些是**程序自己**在正常操作里写的。第一版
    回调不看路径（`if event.is_ok()`），于是按 `f` 收藏 → `state.toml` 落盘 → 下一帧
    自己 `reload`：选中项跳回第一行、提示从「已收藏 x」被顶成「已刷新」。现在按**路径
    组件**比（`Path::starts_with`），只认 `tools.d/` 与 `tools/`；`tools.d` 不是 `tools`
    的子路径，`tools-extra/` 也不是。
  - **`Access` 类事件不算数**。inotify 把它自己的递归扫描（打开目录列内容）也报成
    `Access(Open)`：不看事件类型的话，启动时一眼 `tools.d/` 就会凭空重载一次。
  - **配置目录还不存在时**（第一次跑）挂不上，等它出现再补挂一次并立刻重扫 —— 挂载
    之前建的文件不会补发事件。否则这一整轮自动重载都是死的，而用户看不出来，只会以为
    新写的 manifest 没生效。
- **长任务桌面通知**：新增 `src/notifications.rs`，长的后台任务（> 10s）跑完发一条
  `notify-send`。刻意**没有**引 `notify-rust`：它在 Linux 上默认走 zbus，会把整条 async
  运行时（实测 80+ 个 crate）拖进来，对「跑完提醒一句」太重；`notify-send` 和剪贴板那套
  （`wl-copy` / `xclip`）一个思路 —— 尽力而为，缺了就退化成没有。

### CI

- clippy / test 加 `--all-features`。
- 新增 `msrv` job：固定用 1.90.0 编译一次，`Cargo.toml` 声明的 MSRV 破了就红。

### 未做（有意留手，见下）

- **CLI 上 `clap`**：`src/cli.rs` 的解析器**故意**手写（动词必须最先出现、全局选项先剥离、
  `ui` 子命令、TUI 模式的裸位置参数当脚本目录、`--all` 同时对新闻与列表生效）。换成 clap
  是一次**行为敏感的重写**，收益主要在自动补全，回归风险却压在「能不能被别的程序调用」这条
  命脉上 —— 值得单独一个 change 去做，不在本轮里顺手改。
- **`thiserror` 统一错误**：全仓库是 `Result<_, String>`，整体迁移是一次 4 万行级别的侵入式
  改动，且没有用户可见收益。建议按模块**增量**引入，不做一次性大迁移。
- **工具仓库 PGP 签名校验**（`sequoia-openpgp` 等）：真正的安全增强，但涉及依赖体量与密钥
  管理设计，需先定方案。

## 验证记录

### 2026-11-07（P4-01 + P4-03：MCP Tasks 与实时进度，上一批「未做」全部交付）

按上一条记录的实施要点执行，白名单不变（src/mcp、src/service、src/security，未触碰
app/、ui/、ecosystem.rs、registry/、scripts/）。协议事实来源
modelcontextprotocol.io/specification/2025-11-25（basic/utilities/tasks、/basic/lifecycle、
/server/tools），实现前对官方页面做了二次核对，两处比上批调研摘要更精确：`tasks/get`
是**非阻塞轮询**（阻塞到终态是 `tasks/result` 的规范要求）；get/cancel/list 的结果
**不带** related-task 元数据（taskId 已在响应结构里，只有 `tasks/result` 的响应必须带）。

交付：

- **P4-01 MCP Tasks**（新增 `mcp/tasks.rs`，server.rs 接入）：
  - TaskManager：BTreeMap 任务表 + Condvar（tasks/result 阻塞等待与退出清理共用）+
    出站通知队列 + 与同步调用共享的 limiter Arc；`begin` 带 purge 过期（ttl 自创建起算，
    只对终态生效，运行中的任务必须留住取消把手）与表上限 256（满则先逐出最老终态）；
    taskId 用 /dev/urandom 生成 128 位熵的 32 字符十六进制串（规范：无授权上下文必须
    加密安全随机，防猜测）。
  - `tools/call` params 带 task 字段：建任务 → 确认门槛**同步**走既有 elicitation
    （请求带 `io.modelcontextprotocol/related-task` 元数据，拒绝审计带 task_id）→
    全部拒绝路径在起子进程之前，拒绝即任务转 failed（结果就是同步路径那条 isError
    CallToolResult）→ 通过则起后台线程：`ToolCatalog::new(vec![工具副本])` + 共享
    limiter 走完整 `ToolGateway` 流水线（授权、名额、执行、限额、审计全复用），并立即回
    `CreateTaskResult{task:{taskId,status:"working",createdAt,lastUpdatedAt,ttl,pollInterval}}`。
  - `tasks/get`：非阻塞回当前状态，未知 / 已过期清理回 -32602；`tasks/result`：阻塞到
    终态，回底层请求本该返回的结果并必带 related-task 元数据；`tasks/cancel`：响应前转
    cancelled 终态 + 置共享 `Arc<AtomicBool>`，执行器 pump 循环见旗标即走既有
    terminate()（进程组 SIGTERM，2s 宽限后不肯退再 SIGKILL，与超时路径同一套先礼后兵），
    终态不可再变，终态再取消回 -32602。
  - 能力声明：`capabilities.tasks = {cancel:{}, requests:{tools:{call:{}}}}`；
    `tasks.list` 按规范安全建议不声明（stdio 无法识别请求方）；tools/list 每件工具带
    `execution.taskSupport: "optional"`。
  - 退出清理：stdio 断开后全部运行中任务置取消旗标并转 cancelled（逐条记审计），
    等终态上限 8 秒再退出，无孤儿进程（集成测试用脚本拉起的孙子进程 pid 验证）。
- **协商双版本**（ADR-001 修订，见该文档）：客户端请求 2025-11-25 / 2026-07-28 →
  回 2025-11-25 并开放 Tasks；请求 2025-06-18（或更老 / 未知）→ 维持 2025-06-18 现状，
  task 字段按规范忽略，tasks/* 回 -32601。集成测试
  `server_negotiates_to_the_version_it_implements` 同步更新。
- **P4-03 实时进度**：`service/execution.rs` 加进度钩子，pump 循环 1 秒节流回调
  `ExecutionProgress{elapsed_ms, message}`（ffmpeg 类 `JobEvent::Progress` 键值取最新值作
  阶段信息），取消旗标在循环顶部检查；`ToolGateway::call_with_hooks` 把钩子从服务端
  穿透到执行器，`call()` 原样委托，同步路径行为零变化。任务线程把同一份信息写进
  Task 的 `statusMessage`、审计 progress 条目（统一进度数据
  `TaskProgress{job_id,status,progress,message}`，current/total 无真实总量依据恒缺省）
  与 `notifications/progress`（客户端在 `_meta.progressToken` 索要过才发；progress 计数
  单调递增，计数递增与通知入队同锁保证顺序；无真实总量不给 total、不伪造百分比）。
- **审计**：任务生命周期链 created → progress → completed / cancelled 全部经
  `Auditor::with_task_tag` 副本自动携带 task_id（网关的 completed / denied 也在内，
  P4-05 的机制本批真正用上）；纪律不变，只记元数据与脱敏摘要。

验证：

- `cargo fmt --check` 通过；`clippy --all-targets`（`--all-features` 与
  `--no-default-features` 两套，`-D warnings`）通过。
- `cargo test --all-features` **693 passed / 16 ignored**（上批 670 + 新增 23：tasks 10、
  protocol 2、execution 2、gateway 2、server 3、集成 4）；
  `cargo test --locked --no-default-features` **687 passed / 16 ignored**（664 + 23）。
- `cargo +1.90.0 build --locked`（MSRV）通过。
- `check registry/` 85 项（featured 1 警告为存量）、`recipe check registry/featured` 35 条、
  `python3 registry/build-featured.py --check`（35 条 / SHA-256）通过。
- 集成测试（真实子进程对话）覆盖：创建 / 进度 / 完成 / 取消 / 终态再取消拒绝 /
  无孤儿（慢脚本 marker pid 文件验证进程组被收掉）/ 并发拒绝（max_concurrent=1 时
  后台任务占满后同步调用 busy）/ 进度形状（无 total、无百分比、单调递增）/
  协商双版本 / 2025-06-18 客户端下 task 字段被忽略。

未做（记在这里，下批接手）：

- 热更新的真实子进程集成测试（现有覆盖：指纹/信箱/watcher 真线程/服务端换入快照
  四组单元测试；集成版要给 `Server` 助手加读取线程才能收 `list_changed` 通知）。
- `mcp.toml` 模板补一段 max_concurrent 对后台任务同样生效的注释（模板已在 P2 有
  max_concurrent = 2，语义未变，不改文件）。
- `notifications/tasks/status` 状态变更通知（规范标可选；轮询模型已覆盖，本批未加）。

### 2026-11-07（P4：MCP 并发预算共享 / 工具目录热更新 / 任务生命周期审计 —— Tasks 调研完成、实现留待下批）

按计划书 P4 与 [`p0-baseline-audit.md`](p0-baseline-audit.md) §9 白名单执行（src/mcp、src/service、
src/security，未触碰 app/、ui/、ecosystem.rs、registry/、scripts/）。本批时限内完成 P4-02 / P4-04 /
P4-05 三项并全部验证通过；P4-01（MCP Tasks）与 P4-03（实时进度）在止损点前未落地，协议调研已
完成，实施要点全部记录在下面的「未做」里。

变更：

- **P4-02 并发预算共享**（`service/gateway.rs`）：`ConcurrencyLimiter` 改为可共享实例
  （`max` 用 `AtomicUsize`，新增 `set_max` / `max` / `active`），`ToolGateway::new` 接收
  调用方传入的 `Arc<ConcurrencyLimiter>`。这修掉了一个 P2 遗留的结构性问题：此前网关
  每次构造都新建一个 limiter（active 恒为 0），并发上限在跨请求维度上从未真正生效。
  现在整个服务端共用一份预算，超上限一律直接拒绝（不排队），语义与 P2 一致。
  `set_max` 让热更新改 `mcp.toml` 的 `max_concurrent` 时上限原地生效、预算不清零。
- **P4-04 工具目录热更新**（新增 `mcp/reload.rs`，接入 `mcp/server.rs` + `mcp/mod.rs`）：
  - watcher 线程每秒对 `tools.d/`、`tools/`、`mcp.toml` 做 mtime + 大小指纹轮询
    （与 `watch.rs` 同一条边界：`state.toml` 等程序自写文件不计入）；指纹变化即重跑
    发现与目录构建，产出**完整**的 `ReloadSnapshot` 放进一次性信箱。
  - 主循环在每条消息前 `apply_reload`：catalog / policy / auditor 整体换入（全部在
    `Arc` 或 Clone 之后，换入即生效，不存在半初始化状态），并发上限原地调新，
    名称表从新目录整体重建（不增量注册，重复 id 不可能出现）。
  - 授权清单的**内容签名**（id + summary + inputSchema）前后对比，真的变了才发
    `notifications/tools/list_changed`（协议事实：modelcontextprotocol.io/specification/
    2025-11-25/server/tools，`listChanged` 能力 + 该通知在 2025-06-18 与 2025-11-25
    同名同义）。能力声明相应改为 `tools.listChanged = true`。
  - 非法 Manifest 由既有发现层逐文件跳过（watcher 真线程测试覆盖：坏 TOML 后下一次
    变更照常重建，服务端不崩）；`ToolCatalog` 排序只由内容决定（P1 已验），相同目录
    内容产生相同顺序与相同名字。
  - 通知队列挂在 `McpServer::outgoing_handle()`，主循环写完当前响应后排空，stdout
    仍归协议独占（主线程独占写入）。
- **P4-05 任务生命周期审计字段**（`security/audit.rs`）：`AuditRecord` 新增
  `task_id`（可缺省，序列化时省略，旧记录天然兼容）；`Auditor` 新增 `with_task_tag`
  副本机制 —— 带标记的副本写下的每一条记录（含网关的 `completed` / `denied`）自动
  携带 `task_id`，新增 `created` / `progress` / `cancelled` 三个生命周期写入方法。
  纪律不变：只记元数据与脱敏摘要，原始请求、输出、敏感值一律不落盘。
  `progress` 条目的 `reason` 预留给统一进度数据的 JSON 串（见「未做」P4-03）。

P4-01 调研结论（协议事实已核实，2026-11-07，来源：modelcontextprotocol.io/specification/
2025-11-25/basic/utilities/tasks 与 /server/tools、/basic/lifecycle、/client/elicitation）：

- Tasks 是 2025-11-25 引入的**实验性**能力。创建 = `tools/call` params 带 `task: {ttl?}`
  字段，服务端立即回 `CreateTaskResult{task: {taskId, status:"working", createdAt,
  lastUpdatedAt, ttl, pollInterval, statusMessage?}}`；查询 = `tasks/get{taskId}`；
  结果 = `tasks/result{taskId}`（非终态必须阻塞到终态，响应 result 内必须带
  `_meta["io.modelcontextprotocol/related-task"]={taskId}`）；取消 = `tasks/cancel{taskId}`
  （终态再取消回 `-32602`；取消必须在响应前转成 `cancelled` 终态，且此后不得再变）。
- 服务端能力声明：`capabilities.tasks = {cancel:{}, requests:{tools:{call:{}}}}`；
  `tasks/list` 按规范安全建议**不声明**（stdio 无请求方识别："receivers that cannot
  identify requestors SHOULD NOT declare the tasks.list capability"）。
- 工具级协商：`tools/list` 每件工具带 `execution.taskSupport: "optional"`。
- 状态机：working → completed / failed / cancelled（isError=true 的工具结果 → failed）；
  taskId 必须为字符串且不可猜测（无授权上下文时要用加密安全随机源，可读 /dev/urandom，
  无需新依赖）。
- 任务相关消息（如确认用 elicitation）的 params 必须带 related-task 元数据；form 模式
  elicitation 的请求/响应形状在 2025-11-25 无破坏性变更（新增的是 url 模式，本服务端不用）。
- 进度（P4-03）：`notifications/progress{progressToken, progress, total?, message?}`，
  progress 必须单调递增、total 未知可省略；无真实总量依据时只给耗时与阶段信息，
  不得伪造百分比。Task 的 `statusMessage` 可承载同一份信息。
- 协商：客户端请求 2025-11-25 / 2026-07-28 时回 2025-11-25 并开放上述能力；请求
  2025-06-18 时维持现状（task 字段按普通请求处理）。集成测试
  `server_negotiates_to_the_version_it_implements` 届时要同步更新降级目标。

P4-01 + P4-03 实施要点（已设计，未写码）：

- 新增 `mcp/tasks.rs`：TaskManager（BTreeMap 任务表 + Condvar + 出站通知队列 +
  共享 limiter 的 Arc）；`begin`（purge 过期 + 表上限 256 防耗尽 + /dev/urandom 生成
  taskId）→ 建 working 记录 → 确认门槛同步走既有 elicitation（请求带 related-task）→
  拒绝路径全部在起子进程前：拒绝/无确认能力/用户拒绝 → 任务直接转 failed；
  通过则 spawn 后台线程，线程内 `ToolCatalog::new(vec![tool 副本])` + 共享 limiter
  走完整 `ToolGateway` 流水线（授权、名额、执行、限额、审计全复用）。
- 取消：`tasks/cancel` 置共享 `Arc<AtomicBool>`，执行器的 pump 循环（50ms 一轮）见到
  旗标即走既有 `terminate()`（进程组 SIGTERM → 2s 宽限 → SIGKILL），无孤儿子进程；
  任务状态在响应前即转 `cancelled` 并保持不变。
- `service/execution.rs` 加进度钩子：pump 循环 1s 节流回调 `ExecutionProgress{elapsed_ms,
  message}`（ffmpeg 类 `JobEvent::Progress` 的键值作阶段信息），取消旗标在循环顶部检查；
  `ToolGateway::call_with_hooks` 把钩子从服务端穿透到执行器。
- 退出清理：stdio 主循环断开后置全部运行中任务的取消旗标，等待终态（上限 8s）再退出。
- 测试面：创建/进度/完成/取消/无孤儿（真实子进程 + slow.sh 脚本 + marker 文件验证
  进程组被杀）、并发拒绝（max_concurrent=1 时后台任务占满后同步调用 busy）、
  进度形状断言（无 total 字段、无百分比）、协商双版本、task 字段在 2025-06-18 下被忽略。

未做（记在这里，下批接手）：

- P4-01 MCP Tasks 全部协议面、P4-03 实时进度通知：已在同日的下一批交付（见上一条
  记录），实施要点按本条原样落地；协议细节二次核对有两处按规范精确化（tasks/get
  是非阻塞轮询、其结果不带 related-task，阻塞与 related-task 元数据是 tasks/result
  的要求）。
- 热更新的真实子进程集成测试（现有覆盖：指纹/信箱/watcher 真线程/服务端换入快照
  四组单元测试；集成版要给 `Server` 助手加读取线程才能收 `list_changed` 通知）。
- `mcp.toml` 模板补一段 max_concurrent 对后台任务同样生效的注释（模板已在 P2 有
  max_concurrent = 2，语义未变，不改文件）。

验证：

- `cargo fmt --check` 通过；`clippy --all-targets`（`--all-features` 与
  `--no-default-features` 两套，`-D warnings`）通过。
- `cargo test --all-features` **670 passed / 16 ignored**（P3 后 663 + 新增 7）；
  `cargo test --locked --no-default-features` **664 passed / 16 ignored**（657 + 7）。
- `cargo +1.90.0 build --locked`（MSRV）通过。
- `check registry/` 85 项通过（featured 1 警告为存量）、`recipe check` 35 条、
  `python3 registry/build-featured.py --check`（35 条 / SHA-256）通过。

### 2026-11-07（P3：真正的 MCP stdio Server —— 入口 / 动态注册 / tools/call / 结构化输出 / 连接测试）

按计划书 P3 与 [`p0-baseline-audit.md`](p0-baseline-audit.md) §9 白名单执行；安全面全部走 P2 的
`security/` 与 `service/gateway.rs`，`mcp/` 自身不碰子进程。

交付（新增 `src/mcp/`，约 2000 行 + 集成测试）：

- **P3-01 入口**：`toolbox-hub mcp stdio`（main.rs 独立非 TUI 分支，先于 TUI 初始化判断）。
  stdout 只承载协议；日志与诊断统一 stderr；`--config-dir` / `--data-dir` 与其他子命令同源；
  客户端关 stdin 后干净退出（无孤儿子进程，集成测试覆盖）。
- **P3-02 动态注册**：启动时读 Registry 快照 + MCP Policy，`tools/list` 只返回通过策略的工具；
  `mcp/names.rs`（NameRegistry）把原始 id 规范化为 `toolbox_manifest_<action>` 等合法 MCP 名，
  冲突消解且映射稳定（单元测试 6 例）。
- **P3-03 tools/call 流水线**：名称映射 → 取 ToolDefinition → JSON 解析 → schema 检查
  （`mcp/schema.rs`，Manifest 参数声明到 JSON Schema 的映射）→ 确认门槛（Elicitation，
  客户端 initialize 时声明能力才可用）→ 网关执行。未知工具 `-32602`；缺参数走
  `isError + structuredContent.error_kind`；全部拒绝路径发生在启动子进程之前。
- **P3-04 结构化输出**：content[0].text + structuredContent（status / exit_code / duration_ms /
  truncated / stdout / stderr / 脱敏命令摘要），脱敏唯一出处仍是 P2 的 `TypedValues`。
- **审计**：每次 call 落 `mcp-audit.jsonl`（completed / denied 两种决策实测在场）。
- **P0-08 关闭（ADR-001）**：协议版本钉 2025-06-18，传输自研不引 rmcp，升级触发条件
  （P4 Tasks / HTTP 传输 / 客户端生态）写入 [`adr-001`](adr-001-mcp-protocol-version-and-transport.md)。
  顺手修掉一处真实规范偏差：initialize 原来盲目回显客户端请求的版本（等于宣称实现
  2026-07-28 语义），现按规范始终回自支持版本，新增协商降级集成测试
  `server_negotiates_to_the_version_it_implements`。
- **P3-05 连接测试**（tests/mcp_protocol.rs，真实子进程 + 行协议对话，非内部 dispatcher）：
  协议协商 / tools/list 形状（name/description/inputSchema/annotations.readOnlyHint）/
  tools/call 中文+特殊字符+空格+逗号原样进 argv（注入串 `$(touch ...)` 只当普通字节）/
  未知工具 / 缺参数 / 策略拒绝（审计 denied）/ 无孤儿退出 / 版本协商降级。
  超时与截断由 P1 服务层测试覆盖（`a_hanging_command_is_killed_at_the_deadline`、
  `truncation_never_splits_a_utf8_character`、`summary_tells_timeout_cancel_and_truncation_apart`）。

验证：

- `cargo fmt --check`；`clippy --all-targets`（两套 feature，`-D warnings`）通过。
- `cargo test --all-features` **663 passed / 16 ignored**（P2 后 630 + 新增 33，含协商修复 1 例）；
  `--locked --no-default-features` **657 passed / 0 failed**。
- `cargo +1.90.0 build --locked`（MSRV）通过。
- `check registry/` 85 项（featured 1 警告为存量）、`recipe check` 35 条、
  `build-featured.py --check`（35 条 / SHA-256）通过。

未做（记在这里，P4）：MCP Tasks（2025-11-25 起的规范能力，需按 ADR-001 升级协商）、
并发上限的策略化配置面、实时进度通知、Registry 热更新后的 listChanged、
审计与执行历史查询面、`ensure_template()` 已在 mcp 入口挂载但 TUI 侧入口未挂。


### 2026-11-06（P0：MCP 升级基线审计与架构冻结）

按 `Toolbox Hub 2.0` 计划书执行 P0，**只读审计 + 文档**，未改源码 / 脚本 / 配置，未提交。
完整报告见 [`p0-baseline-audit.md`](p0-baseline-audit.md)。

基线（HEAD `c45c4ca`，工作区干净）：

- `cargo fmt --check` 通过；`clippy --all-targets --all-features -- -D warnings` 通过。
- `cargo test --all-features` **567 passed / 16 ignored**；`--no-default-features`
  **561 passed / 16 ignored**；`cargo +1.90.0 build --locked` 通过（MSRV 实测）。
- `check registry/`（9 个插件包）、`recipe check registry/featured`（35 条）、
  `python3 registry/build-featured.py --check`（35 条 / SHA-256）全部通过。

审计结论（只列需要后续动手的）：

- **执行入口有 6 条**（TUI 捕获队列 / 交互单动作 / 交互多选 / 提权安装 / ecosystem
  Recipe 与队列 / CLI 包管理），没有统一执行服务；`foreach` 展开只存在于 `App`。
- **`ArgumentValues` 是字符串表**，多值靠分隔符拆，无法无损承接 MCP 数组输入。
- **`Danger` 只是 TUI 确认位**：`pkg-center`（能装能卸、走 sudo）标的是 `safe`，
  不能映射成 MCP 只读权限；需要独立 `ToolEffects`。
- **S-01（真实缺陷）**：历史只脱敏 `argv`，`values` 原样落盘（`app/mod.rs:2772`、
  `:2800`），`sensitive = true` 的值会泄漏 —— P2-08 必须修。
- **S-02（真实缺陷）**：Recipe 参数是纯字符串替换进 argv（`ecosystem.rs:1173`），
  没有 flag / 位置约束，存在参数注入（不是 shell 注入）—— P5 接入前必修。
- 计划书三处偏差：`Curated` 实为 `Manifest`；`check` 不校验精选 Recipe；
  MCP 规范 / `rmcp` 版本未核实（P0-08 留到 P3 前补 ADR）。

下一步（P1 门禁已通过）：新增 `src/service/`（Catalog / Typed Argument / Executor /
Result / Error），只改 `main.rs`、`model/argument.rs`、`runtime/exec.rs`，
不动 `app/`、`ui/`、`ecosystem.rs`、`scripts/sing-box/`。

### 2026-11-06（P2：MCP 安全地基 —— 副作用建模 / mcp.toml 策略 / 授权网关 / 审计 / S-01）

按计划书 P2 与 [`p0-baseline-audit.md`](p0-baseline-audit.md) §9 白名单执行。
本批**只建安全层**：没有接 MCP Server（P3），`security/` 目前只有测试调用，
`main.rs` 里 `#[allow(dead_code)]` 显式声明，不伪装使用。

**模型**（`model/tool.rs`、`model/argument.rs`）：

- `ToolEffects`：读 / 写 / 联网 / 改系统 / 提权 / 交互 / 破坏性 / 通用命令执行器，
  与 `Danger` 完全独立 —— `Danger::Safe` 是 TUI 的确认提示，不是只读证明（审计 §5）。
- `McpExposure`：「允许被 AI 调用」的**声明**，挂在 `ToolDefinition::mcp` 上；
  `None`＝没声明＝默认拒绝。
- `TypedValues::redacted / secrets / summary`：脱敏与审计摘要的唯一出处；
  `Action::redacted` 升级为支持 `flag_join` 形状（`-o/tmp/secret` 也能抹）与多值敏感字段。

**Provider 声明**：

- Manifest：`[action.mcp]`（`enabled` / `name` / `description` / `effects`），
  只接受 `capture`，发现阶段即报错；内置 `system.toml` 给 4 个只读巡检动作
  （disk-space / boot-errors / block-devices / pci-devices）加了声明。
- 本地脚本注解：`mcp` / `mcp-name` / `mcp-description` / `mcp-effects`（`read`、
  `write`、`network`、`system`、`privileged`、`interactive`、`destructive`、`shell`）。
  Curated / Repository 工具复用 Manifest 的构建路径，自动获得同一语义。

**安全层**（新增 `src/security/`）：

- `effects.rs`：从程序名 / base_argv / 参数声明**独立推断**副作用，再与作者声明取
  并集 —— 伪造 `danger = safe` + 「无副作用」声明抹不掉 `sudo` / `rm -rf`
  （`forged_safe_metadata_cannot_hide_privilege_or_destruction`）。
  `systemctl --failed` 这类只读巡检不误伤；`sh -c` 标为通用命令执行器。
- `policy.rs`：`mcp.toml`（配置目录）。判定顺序：启用 → 显式声明 → deny 名单 →
  allow 名单 → 依赖 → 执行模式 → 风险（默认全拒）→ 参数合法 → 路径范围 →
  确认能力 → 授权。**fail-closed**：文件缺失 / 字段拼错退回全拒绝并在
  `Loaded::problem` 说明；`ensure_template()` 写带注释模板。
  `[paths] allowed_dirs` 非空时工作目录与路径参数都必须在范围内（词法归一化，
  `../` 逃不掉）；路径值以 `-` 开头直接拒（选项注入）。
- `approval.rs`：需要确认（`Caution` 或非只读）而客户端无确认能力 → 拒绝，
  绝不「先跑再说」。stdio 客户端首期无确认能力。
- `audit.rs`：JSONL（数据目录 `mcp-audit.jsonl`），只记元数据 —— 时间、工具 id、
  客户端、授权结果、退出码、耗时、**脱敏后的参数摘要**；原始请求 / 输出 / 敏感值
  一律不落盘，也绝不写 stdout（MCP 协议流归协议）。
- `service/gateway.rs`：`ToolGateway` —— 唯一 AI 可调用入口。`list()` 只返回通过
  策略的工具；`call()` 在**启动子进程之前**完成全部拒绝路径，超时 / 输出字节
  上限 / 并发（上限直接拒绝，不排队）都由策略统一决定。

**S-01 修复**（`app/mod.rs`、`app/input.rs`）：历史文件的 `values` 列（回填再改用）
此前明文落盘，现与 argv 同一套 `redacted_values` 脱敏；回归测试
`history_never_persists_sensitive_form_values` 覆盖 argv 与取值两列。

未做（记在这里，P3）：MCP Server 接线、工具名规范化、Elicitation 确认流程、
`ensure_template()` 的首次运行调用点。

验证：

- `cargo fmt --check`；`clippy --all-targets`（两套 feature，`-D warnings`）通过。
- `cargo test --all-features` **630 passed / 16 ignored**（P1 后 592 + 新增 38）；
  `--no-default-features` **624 passed**（586 + 38）。
- `cargo +1.90.0 build --locked`（MSRV）通过。
- `recipe check` 35 条、`check registry/` 85 项通过（`featured` 警告为存量）。

### 2026-11-06（P1：统一 Tool Service —— 类型化参数 / ToolCatalog / 无终端执行器）

按计划书 P1 与 [`p0-baseline-audit.md`](p0-baseline-audit.md) §9 的白名单执行；
未触碰 TUI 行为、`ui/` 渲染、`ecosystem.rs` 与冻结的 `scripts/sing-box/`。

变更：

- **argv 构建收敛成一条路径**（`model/argument.rs`）：新增 `ArgumentValue`
  （Text / Bool / List）与 `TypedValues`；`Action::build_argv_typed` 是唯一的
  构建实现，`build_argv` 退化为「表单字符串 → 类型化」的兼容层。两条路不可能
  再各算各的（契约测试 `typed_and_form_values_build_the_same_argv`）。
- **多值不再被分隔符吃掉**：类型化 `List` 的每一项就是一个 argv 元素，
  路径里带逗号（`a,b.jpg`）也能原样保留 —— 字符串形式仍按分隔符理解（兼容）。
- **`Captured` 增加 `truncated` 字段**（`runtime/exec.rs`）：以前截断只能靠
  开头那行中文提示判断；现在 reader 直接返回标记，服务层不用解析文字。
- **新增 `src/service/`**：
  - `catalog`：Registry 的**稳定快照**（域 → 置顶 → 名字 → id 排序）；
    重复 id「先注册的胜出」—— 先按输入顺序去重再排序，顺序不能反过来决定身份；
  - `argument`：进入执行器前的校验（未声明的键 / 类型不对 / 未声明 repeatable
    却给了多个值），静默丢参数在服务层是不允许的；
  - `error`：稳定 `kind()`（`unknown_tool` / `not_ready` / `unsupported_mode` …），
    将来 CLI / MCP 直接映射状态码；
  - `execution`：`execute(&ExecutionRequest) -> Result<ExecutionResult>`，
    复用 `spawn_captured` 的进程组与输出上限；超时走 SIGTERM → 2s 宽限 →
    SIGKILL；成功按动作声明的 `ok_exit_codes` 判；缺省值自动叠加（与 TUI 打开
    表单时的初值一致）；`interactive` / `native` / 缺依赖在构造请求时就拒绝。
- `main.rs` 挂 `#[allow(dead_code)] mod service`：P3 MCP 服务端接线之前只有
  测试调用，显式声明而不是伪装使用。

偏差（已评估）：`Captured` 加字段导致 `app/mod.rs`、`ui/mod.rs` 各一处测试
构造点补 `truncated: false` —— 编译期同步，无行为变化。

验证：

- `cargo fmt --check` 通过；`clippy --all-targets`（两套 feature、`-D warnings`）通过。
- `cargo test --all-features` **592 passed / 16 ignored**（基线 567 + 新增 25）；
  `--no-default-features` **586 passed / 16 ignored**（基线 561 + 新增 25）。
- `cargo +1.90.0 build --locked` 通过（MSRV）。
- 超时测试用 `sh -c 'sleep 5'` 实测：300ms 截止即整组终止，耗时 < 4s。
- 服务层只 import `model` / `registry` / `runtime`，无 Ratatui 依赖。

P1 验收对照（计划书 §11.3 / 审计报告 §9.3）：

- TUI 行为、默认值、argv 顺序、历史脱敏（argv）不退化 —— 仅 `Captured` 构造点同步；
- 超时 / 取消 / 截断 / `ok_exit_codes` / 未知键均有测试；
- 基线 567 / 561 不减，反而 +25。

未做（记在这里）：

- `foreach` 的批量展开仍只在 `App::form_build_runs`；MCP 要支持批量执行前，
  先把它下沉到服务层（P2 的事，动了就要和 TUI 的 `{stem}` 替换语义对齐）。
- 敏感值脱敏仍只覆盖 argv，`values` 落盘未脱敏（P0 审计 S-01，P2-08 处理）。

### 2026-10-28（sing-box 开关收尾：tc 改名 / bypass 真正生效 / 表单对齐）

起点是工作区里改了一半的脚本：`--with-abf`、`--with-lyc-geosite`、`--bypass-cn`、
`--no-shared`、`--shared-plane` 只进了脚本，表单、测试和文档还停在旧口径。

- **`--bypass-cn` 在 TUN→eBPF 分支被静默丢掉**：参数只在「已是 eBPF」的分支被读取，
  从 TUN 切过去时勾了等于没勾。现在把挑选逻辑抽成 `bypass_tags()`（bash 函数 + 内嵌
  python），两条分支共用同一份结果；只从 `outbound=direct` 的规则反推 `geoip*` /
  `cidr*` 表，并拒绝同时被代理规则引用的表 —— 这条自检是防「`geoip/telegram` 被选进
  bypass，Telegram 反而变直连」。
- **`local.data_plane` 没有 `tc`**：本机 v1.14.4-reF1nd 实测 `sing-box check` 直接报
  `unknown local.data_plane: tc`，`tools ebpf status --local-data-plane tc` 也报
  invalid；合法值只有 `cgroup` / `socket_assign`。脚本把 `--data-plane tc` 当旧别名
  映射到 `socket_assign`（并打一行提示），TC 守卫改为按 `socket_assign` 或
  `shared=packet_rewrite` 触发，不再生成注定 check 不过的配置。
- **表单补齐**：manifest 增加 `--with-lyc-geosite`、`--bypass-cn`、`--no-shared`、
  `--shared-plane`，数据面的 `tc` 选项改成 `socket_assign`。`sing_box_actions_drive_the_ops_scripts`
  改成 8 个审计开关逐个对账，并新增 4 个 eBPF 开关的 argv 断言（以前表单加了、manifest
  忘写 `flag` 的话，勾了也静默无效）。
- **计数口径**：audit 头部还写「7 个开关」（漏了 abf），根 README 与 scripts README 写
  「6 个」—— 统一成 8。

验证（都是抽脚本里**同一份** heredoc 跑的，没有另写一套逻辑）：

- TUN→eBPF + `--bypass-cn`：生成配置 `bypass_rule_set=["geoip/cn"]`，代理表
  `geoip/telegram` 被排除，`sing-box check` 通过。
- 已是 eBPF 的四种组合（清除 bypass / 设置 bypass / 关闭 shared / `socket_assign` 接管）：
  `sing-box check` 全部通过。
- 审计 `--with-abf` + `--with-lyc-geosite` + `--nxdomain-ads`：用
  `/usr/share/sing-box-rule-sets` 里的真规则集生成配置，`sing-box check` 通过；广告规则
  同时吃到 adblockfilters 并改成 NXDOMAIN，`lyc/*` 只加在同类规则上。
- `cargo fmt --all -- --check`、`cargo clippy --all-targets --all-features -- -D warnings`
  通过；`cargo test --all-features` **559 passed / 16 ignored**，
  `cargo test --no-default-features` **553 passed / 16 ignored**。
- `bash -n` 四支脚本、两处脚本里的全部内嵌 Python 块 `py_compile` 通过。

未做：没有在 root 下跑完整切换/修复流程（会重启 `sing-box`、可能断网），只验证到
「生成的配置能过 `sing-box check`」这一层。

随后已执行 `./scripts/sing-box/install.sh`，`~/.local/bin` 下 6 个脚本与源码对齐。

### 2026-10-28（第二轮：UI 线程不卡 / 写盘不再静默 / 重载去抖 / 哈希去重）

上一条把脚本那层收尾了，这轮扫的是主程序本身（73 个 `.rs`、约 4.6 万行）。先说一个
**否证**：生产代码里的 `unwrap` / `expect` / `panic!` 是 **0** —— `app/mod.rs` 那 106 处
`.expect(` 全在 2928 行之后的测试模块里，manifest / `packages.toml` / `state.toml` 的解析
失败也各有退路（warning / 默认值 / `unwrap_or_default`）。所以「配置写错导致崩」这条路
已经堵住了。真正留下的是下面四条。

- **UI 线程上唯一的挂死点**：`probe_duration`（`src/runtime/exec.rs`）在按 Enter 执行单
  文件动作时同步拉起 `ffprobe` 并 `output()` 无限期等。ffprobe 会真的去**打开**目标文件，
  落在网络挂载或正在被写入的文件上就一直不返回 —— 界面卡死，连按键都收不到。改成
  `spawn` + `try_wait` 轮询 + 3 秒上限，超时 kill 并收尸（`src/runtime/exec.rs:339`）。
  等待逻辑抽成 `wait_with_timeout`，好用 `sleep 30` 直接测「不肯退的进程会被放弃」。
- **清空队列写盘失败被静默吞掉**（`src/app/mod.rs:1582`）：`let _ = save_queue_to(&path, &[])`。
  上一处（关界面时）有注释说明「写不进去也不该拦住关界面」，这一处没有 —— 磁盘满或数据
  目录被删时，用户清空队列后下次打开会看到**旧队列复活**，而界面上没有任何提示。改成
  把错误挂到 `self.message`（不中断流程，包已经装完了）。
- **配置热重载没有去抖**（`src/watch.rs`）：标志位只能把同一帧内的多条事件收敛成一次，
  而主循环每帧（100ms）都来问，于是编辑器的一次保存（写临时文件 → rename）或一次
  `git checkout`（几十个文件连发）会变成连续好几帧各全量重扫一次。加 300ms 冷却。
  **关键不变量**：冷却期里把标志位**放回去**而不是清掉 —— 清掉就等于把最后一次改动丢了，
  改了配置却永远不刷新，比多刷几次糟得多（`debounce_defers_the_flag_instead_of_dropping_it`
  这条测试钉的就是它）。主循环空闲时也在 100ms 一轮，不会没人来问。
- **`ecosystem::sha256` 是第二份哈希实现**（`src/ecosystem.rs:542`）：`fs::read` 整个文件
  再 digest，包有多大就吃多少内存；而 `repository::install::sha256_file` 是流式读（64KB
  缓冲）。删掉重复那份，改成转发。

验证：`cargo fmt --check` 通过；`clippy --all-targets --all-features -- -D warnings` 通过；
`cargo test --all-features` **563 passed / 16 ignored**，`--no-default-features`
**557 passed / 16 ignored**（两条都 +4，就是本轮新增的 4 条）。

### 2026-10-28（第三轮：三份重复实现收敛，顺手补上边界 bug）

上一轮的四条修完，接着清「同一段逻辑写了好几遍」这一堆 —— 这类重复不会立刻出错，
但改语义时漏掉一处就变成 bug（下面第 3 条就是这么暴露的）。

- **`mode_of` / `danger_of` 逐字节重复**：`providers/fftools.rs` 与 `providers/scripted.rs`
  各有一份完全相同的实现（连注释都一样）。挪进两个 Provider 共用的底座
  `providers/metadata.rs`（`metadata::mode_of` / `metadata::danger_of`），以后改
  「注解没写时的默认值」只动一处。
- **三份「按显示宽度截断」**：收进 `ui/mod.rs`，语义明确分开 ——
  - `clip(text, width)`：硬裁、不加标记，**表格列**用。`ui/repository.rs` 那张表的
    13 处调用原来用的是本文件里的私有同名函数（已删）。
  - `elide(text, width)`：**真截掉了**才补 `…`，用在「用户该知道被砍了」的地方
    （`widget.rs` 的 recipe 标题，原来叫 `truncate`，已删）。
  - 顺手修了旧 `truncate` 的边界：`width == 0` 时它仍然 `push('…')`，返回一个**比
    宽度还宽**的字符串；现在返回空串。
- **「等子进程，超时就杀」写了两遍**：`thumbnail.rs` 的 `run_ffmpeg` 早就有 deadline +
  `kill` + `wait`；上一轮我又给 `probe_duration` 写了一遍。现在收敛成
  `runtime::wait_with_timeout`，返回 `WaitOutcome::{Exited, TimedOut, Failed}` ——
  超时和「`try_wait` 自己失败」必须分开，两边调用方的措辞不一样。**这也是上一轮那个
  判断的旁证**：项目对「外部进程可能挂住」是有意识的，`probe_duration` 只是漏了。

验证（全绿）：
- `cargo fmt --check`、`clippy --all-targets --all-features -- -D warnings` 通过；
- `cargo test --all-features` **565 passed / 16 ignored**，`--no-default-features`
  **559 passed / 16 ignored**（各 +2：`clip` 的宽度不变量、`elide` 的「没截就不加省略号」）；
- 缩略图原有的 `a_hanging_ffmpeg_is_killed_at_the_budget` 在换成共用实现后照旧通过
  —— 说明收敛没改它的行为（300ms 预算、错误文案「没出图」都还在断言里）。

### 2026-10-29（第四轮：运维脚本里三个真 bug + 临时文件名 / CI 门禁）

前三轮都在 Rust 侧，这轮把 `scripts/sing-box/` 六个脚本逐个当代码读了一遍（它们
装在 `~/.local/bin`，不经过 cargo，之前也没有任何 CI 覆盖）。挖出三个真 bug，
其中第一个是**功能直接不可用**。

- **`sing-box-container-proxy` 少传一个参数，`--enable` 主路径必崩**：python 那段读
  `sys.argv[4]`（`data_plane`），调用处只传了 3 个 → `IndexError`，而脚本是
  `set -Eeuo pipefail`，于是「配置正要开始改」的时候直接死掉。补齐第四个参数。
  实测：旧写法 `rc=1 / IndexError: list index out of range`，新写法 `rc=0` 且生成的
  `shared.data_plane` 正确。
- **`sing-box-status --strict` 永远 exit 0**：`bad()` 里 `problems=$((problems + 1))`，
  但 `bad()` **总是**在 `$(bad)` 命令替换里调用 —— 那是子 shell，改的是副本。所以
  父进程恒为 0：末尾永远打「全部正常」，`--strict` 也永远成功。把计数落到临时文件
  （子 shell 也能追加），结尾数一次。实测拿一份只有 tun 入站、服务却是 active 的配置
  跑：**修前 rc=0 / 「全部正常」，修后「有 1 项异常」/ rc=1**。
- **`sing-box-audit` 在改配置之前就会静默退出**：`ls "$RULE_DIR"/*.srs | awk`，
  没有 `.srs` 时 `ls` 退非零，`pipefail` 让整条管道失败、`set -e` 直接把脚本带走 ——
  用户看到的是「脚本什么都没做」。补 `|| true`（实测旧写法 rc=2 且不再往下走）。
- 顺带一处：`sing-box-status` 把 `leak_default` 写成了 `leakhit_default`（变量根本
  不存在，`${leakhit_default:+…}` 永远为空）—— 走系统解析器命中的那半边从来不会显示。
- **`/tmp` 里可预测的临时文件名**：`components.rs`（`-pick.chosen` / `-pick.cwd`）
  与 `runtime/exec.rs`（`-browse.cwd` / `-browse.chosen`）都只用 pid 做名字。粘性
  目录里可预测 = 别人能先建同名符号链接，等外部文件管理器往里写。抽
  `util::temp_tag()`（pid + 纳秒），三处共用（`packages/worker.rs` 那份内联写法
  一并收敛 —— 它本来就是对的，只是没被抽出来）。
- **`$HOME` 缺失时配置写进当前目录**：`config.rs` 的 `xdg()` 用
  `var_os("HOME").unwrap_or_default()`，空路径 `join` 出来是**相对路径** —— 在哪个目录
  启动就写哪，还会随 `cd` 变，界面上一声不响。改成只在 `HOME` 是绝对路径时采用，
  否则退到 `/`（那种环境下写不进去会老实报错，而不是默默污染 cwd）。
- **后台输出没有上限**：`reader_thread` 的 `collected` 是全文累积，而界面只画尾部
  十几行。`ffmpeg -v debug` / 跑飞的 `rsync` 能把常驻内存推到几百 MB。改成保留最后
  4000 行，开头一行写清「前面 N 行没保留」。
- **CI 补上脚本门禁**：以前只对 `packaging/*.sh` 跑 `bash -n`，`scripts/sing-box/`
  六个完全在外。现在一起过；同时删掉 `core-no-pacman` job 里重复的
  `cargo fmt --check`（fmt 与 feature 无关，check job 已经跑过）。

验证：
- `cargo fmt --check`、`clippy --all-targets --all-features -- -D warnings` 通过；
  `cargo test --all-features` **566 passed / 16 ignored**，`--no-default-features`
  **560 passed / 16 ignored**（+1：`a_flood_of_output_keeps_only_the_tail`）。
- `bash -n` 覆盖 `packaging/*.sh` + `scripts/sing-box/*`（8 个文件，本地按 CI 同一段
  脚本跑过）；三个改过的脚本各做了实跑验证（见上）。
- 已重新执行 `install.sh`，`~/.local/bin` 六个副本与源码逐字节一致。

### 2026-10-29（第五轮：把上一轮「未做」里能安全做的做掉，顺手再挖两个 bug）

- **8 处把 shell 变量拼进 python 源码**（`open('$CONF')`）全部改成走 argv。
  实测旧写法：`CONF` 里带单引号 → `SyntaxError: unterminated string literal`；
  新写法同一路径 rc=0。附一条回归：配置目录名含 `'`、`$()`、空格也能正常读。
  顺带合并了重复判定 —— `switch-tun` / `switch-ebpf` 原来是两次 python 各读一遍
  配置来判「是 ebpf 还是 tun」，现在一次读全、交给 `case` 判定（三处提示文案各自保留）。
- **`sing-box-why` 报不出放行建议**：`route_idx` 在没有 reject 规则时是字符串 `?`，
  直接进 `$((route_idx + 1))` → bash 报「算术语法错误」，`set -u` 再补一刀
  「SUGGEST: 未绑定的变量」，整份报告一个字都不输出。改成数字才用，否则退化成
  「广告 reject 规则之前」。两种配置各实测一遍，都出建议。
- **`sing-box-why` 的 IPv6 目标是坏的，且探测命令经过 shell**：那段用
  `bash -c "(sleep 2) | timeout 6 openssl s_client -connect {dial}:{port} ..."`，
  IPv6 不带方括号 → openssl 直接判 `malformed or ambiguous`（实测对比过两种写法）；
  目标值来自命令行，含分号空格会被 shell 解释。改成 `Popen` + 参数列表 + 自动加
  方括号 + 关 stdin 后再 kill 兜底。验证方式：用假 `openssl` 记录真实 argv →
  `[2606:4700:4700::1111]:443`，且 `example.com; touch /tmp/...` 没有被执行。
- **四个脚本「重启失败」不回滚**：主路径 `systemctl restart` 是裸调用，失败时
  `set -e` 就地退出，而回滚逻辑排在**后面**的健康检查里 —— 结果配置已经换成新的、
  服务起不来、脚本一声不响退了。四个脚本统一就地兜住（还原备份 → 再启一次 → exit 1）。
  验证：假 systemctl 第一次失败第二次成功，四个脚本都是 rc=1、配置还原为旧内容、
  systemctl 被调用 2 次；旧写法同一场景下配置停在 `NEW`。
- **`install.rs` 装包不再把整个文件读进内存**：`fs::read` + `sha256_bytes` +
  `atomic::write` 换成流式 `sha256_file` + `atomic::copy` / `copy_executable`
  （`io::copy`，内存只有内核拷贝缓冲）。`atomic.rs` 顺手抽出 `write_atomic`
  填充闭包 —— 两种载荷只差「填内容」这一步，其余（父目录、临时名、权限位、
  失败清理）必须一致，不该写两遍。`write_executable` 因此没人用了，删掉。

验证：
- `cargo fmt --check`、`clippy --all-targets --all-features -- -D warnings` 通过；
  `cargo test --all-features` **567 passed / 16 ignored**，`--no-default-features`
  **561 passed / 16 ignored**（+1：`copy_lands_content_and_mode`）。
- `bash -n` 覆盖 `packaging/*.sh` + `scripts/sing-box/*`（8 个文件）；内嵌 python
  块（含 `PYEOF` 定界符）逐个 `py_compile`。
- 四个脚本按 CI 同一段本地跑过；改动处都有实跑对比（见上）。

未做（记在这里，别当成已修）：
- **四个脚本之间还有约 180 行同源代码**（提权重执行、check→备份→替换→重启、
  回滚、出口对比、DNS 探测）。抽公共文件的**真正难点在分发**：`install.sh` 与
  `PKGBUILD` 都是按 `sing-box-*` 通配装的，多一个 `lib.sh` 不会被装走 → 装完六个
  脚本全部 `source: No such file`，而 install.sh 依然报告成功。要么构建期内联展开，
  要么改两处打包清单，得单独决定。
- `/etc/sing-box/backups` 只增不减，没有轮转（本机目前是空目录，还没到疼的时候）。
- `src/app/mod.rs` 4907 行 / 8 个职责可再拆；`ui/` 其余 11 个文件仍无测试。

### 2026-10-13（本轮：生态升级 —— Recipe / Shell 集成 / 队列与安全 / 缩略图 / 签名）

完整变更与逐条证据见 [`../CHANGELOG-ECOSYSTEM.md`](../CHANGELOG-ECOSYSTEM.md)；
设计依据（字段映射表、回填协议、危险判定形状、放弃的方案）见
[`ecosystem-design-notes.md`](ecosystem-design-notes.md)。

- `cargo fmt --check` 通过（退出码 0）。
- `cargo clippy --all-targets --all-features -- -D warnings` 通过；
  `--all-targets --no-default-features -- -D warnings` 同样通过。
- `cargo test`：默认 feature **559 通过 / 0 失败 / 16 ignored**；
  `--no-default-features` **553 通过 / 0 失败 / 16 ignored**。
- `ldd` 检查 `--no-default-features` 产物：**没有 libalpm**（核心版不依赖它）。
- `python3 registry/build-featured.py --check` → 35 条 · SHA-256 全对；
  `toolbox-hub recipe check registry/featured` → 35 条通过。
- `toolbox-hub build registry/` 重建：只有 `registry/index.json` 多了 `featured` 指针，
  `registry/artifacts/` **逐字节未变** —— 可复现构建仍然成立。
- **shell 集成端到端（真 PTY，`script -qec 'bash --norc -i'`）**：`eval` 生成脚本后按
  `Ctrl-G` → 过滤 `kernel` → `Enter`，`uname -r` 被回填进输入行并执行成功，
  `history` 里记的是 `uname -r`；fish 用 `fish -i` 同样跑通。PTY 抓包显示 stdout
  只有 `uname -r\n`（9 字节），界面内容一个字节都没漏。
- **队列危险拦截**：任务 `rm -rf <canary>` 退出码 126 被拦，canary 目录里的文件仍在，
  同队列的 `touch` 任务正常完成 —— 拦截不是「整队罢工」。
- **minisign 真实签验**（minisign 0.11，与 Ubuntu 24.04 仓库同版本）：签名后自验通过；
  篡改产物 → 退出 1；未签名 → 退出 1；公钥缺失 → 退出 2 并提示「验不了不等于验过了」。
  把 `release.yml` 的签名步骤从 YAML 里抽出来本地执行：正常路径成功，
  「无 secret」「无 `packaging/minisign.pub`」两条路径都**拒绝发布**。
- **AUR 更新脚本**：临时 git checkout 里真跑 —— `sha256sums=('SKIP')` 被换成真实校验和，
  `.SRCINFO` 重新生成，`git log` 只有初始提交（**没有**自动提交或推送）。
- **视频缩略图**：桩 ffmpeg 验「命中缓存不重调 / 文件一变必重生成 / 超时被杀 / 失败不留垃圾」；
  再用真 ffmpeg 造测试视频、生成缩略图并解码成功。这条真跑抓到了
  「`-f image2` 按后缀挑编码器，`.tmp` 后缀会写成 JPEG」的 bug（桩程序看不见）。
- `bash -n` / `fish --no-execute` 校验三种 shell 生成脚本的语法，已进 CI。

### 2026-10-12（测试基线阻塞 + libalpm feature 化）

这两项在 2026-10-13 的验证记录之前完成，原文记录在执行记录里（已删），要点留在这里。

- **`cargo test` 卡住不是死锁**：`components::tests::pager_without_any_input_explains_itself`
  直接调了生产 `pager()`，而它会从**继承来的 stdin** 读到 EOF。远程终端会话保持 stdin 写端
  打开，测试于是停在 `read_to_string` 上 —— 表现是「测试跑不完」，很容易误判成分页器子进程
  死锁或 TTY 锁。
  修法：抽出 `read_pager_body(file, stdin_is_terminal, input)`，测试注入 `io::Cursor` 并覆盖
  「空管道」与「交互终端无输入」两条分支；生产管道行为仍然是读到 EOF
  （`cmd | toolbox-hub ui pager` 的语义没变）。
  复现命令：`timeout 60s cargo test components::tests::pager_without_any_input_explains_itself -- --exact --nocapture`
  → 修复前一直跑，修复后 1 passed / 0 failed；随后串行全量 504 passed / 0 failed / 16 ignored。
- **libalpm 变成 Cargo feature**：`default = ["pacman"]`、`pacman = ["dep:alpm"]`、
  `alpm = { version = "5", optional = true }`；新增 `src/packages/libalpm_stub.rs` 在禁用时
  保留类型边界并**明确拒绝**数据库动作（不把「没有数据」伪装成「空数据」）。
  `cargo check/test/clippy --no-default-features` 全部通过；CI 新增 `core-no-pacman` job。
  注意：非 pacman 构建里 AUR 状态比较退化为字符串排序，**只是显示层降级**，不是 Arch 版本比较。

### 2026-10-13（收尾补修：cheat front matter / 队列取消语义 / 预设改 TOML）

对着最初那份任务表逐条核对时发现的三处「与计划不符」，补在这里。

- **cheat 的 YAML front matter**：按行硬导时 `syntax: bash`、`tags: [networking, dns]` 会各变成
  一条「可导入」Recipe。现在 front matter 是文件级元数据：`syntax` 记进导入产物的说明、
  `tags` 并进标签，**不产出条目**；没有闭合的 `---` 会整份跳过并报出来。
  复现：`toolbox-hub import cheat <真 cheat 文件> --dry-run` —— 修前 3 条可导入（2 条是垃圾），
  修后 1 条（真的那条 `dig`）。
- **队列取消改走进程组**：原来 `child.kill()` 只杀直接子进程，任务是脚本时孙子进程会变孤儿
  继续写文件；默认并发也不符合计划（是 2，计划写 1）。现在启动 `process_group(0)`、
  取消时 `kill(-pgid, SIGTERM)` → 1 秒宽限 → 整组 SIGKILL，复用交互式执行层的同一个
  `signal_group`；默认并发回到 **1**（要并行显式 `--jobs N`）。
  顺带处理了进程组带来的副作用：任务自成组之后终端 Ctrl-C 碰不到它们（只会杀掉
  `toolbox-hub` 自己），所以队列自己接住 SIGINT 转成取消，退出码按惯例 130。
  实测：真发 SIGINT，任务与其孙子进程都消失，退出码 130。
- **预设改 TOML**（原计划就是 TOML，之前落成了 JSON）：`presets/<名字>.toml`，
  `preset show` 打的就是这份文本；碰到早期版本的 `.json` 预设会提示用 `preset save` 重存，
  而不是报「文件不存在」。
- 新增/更新用例 10 条（front matter 4、队列取消与并发 4、预设 TOML 2），
  测试数 549 → **559**（默认 feature）。fmt / clippy（两套 feature）/ ldd 复验通过，
  featured 35 条与 registry 可复现性未受影响。

### 2026-10-13（推送后修 CI：三条长期红着的门）

推上去才发现 CI 一直是红的 —— 三处都不是这次改出来的，其中两处已经红了很久。

- **`dependencies_come_from_both_declarations_and_actions` 看宿主装没装 jq**：本地装了
  jq 所以永远过，容器里没有所以永远红。改成两种如实报告都接受。
- **冒烟的 `/usr/bin/pac`** 是我这台机器上第三方包提供的路径，容器里没有 →
  换成 `/etc/pacman.conf`；另外冒烟 job 装上 `fzf`（那条断言要的就是「装了 fzf」）、
  把 `/usr/bin/vendor_perl` 放进 PATH（Arch 的 exiftool 装在那儿，非交互 shell 的
  PATH 不含它）。修完手动 dispatch 一次，冒烟从 3 红变成全绿。
- **容器 job 里 git 看不见仓库**：`ls -a` 有 `.github` / `.gitignore`，就是没有 `.git`，
  凡是用 git 的步骤必死。Registry 的重建比对改成 `cp -a` + `diff -r`（还更直白），
  tarball 那条挪到新的 ubuntu job `packaging`。随手在 check 里加了条诊断，
  把这件事记进日志。

现在推送触发的两个工作流（CI / Registry）都是 success，冒烟手动跑也全绿。

### 2026-10-10（自动重载 + 桌面通知 + CI 门）

- `cargo fmt --all -- --check` 通过。
- `cargo clippy --all-targets --all-features -- -D warnings` 通过。
- `cargo test --all-features`：**504 项通过，0 失败，16 项 ignored**。
- `cargo +1.90.0 build --locked` 通过 —— CI 新增的 `msrv` job 钉的就是这一条。
- **干净容器复核**：`archlinux:latest` + `pacman -S gcc pkgconf chafa` + rustup 1.90.0 +
  空的 `CARGO_HOME`，`cargo build --locked` 一次通过，产物 `--version` 正常。这条是照着
  `msrv` job 的步骤走的（那台机器上本机装了 1.90）；job 里 `base-devel` 那一整行的完整
  安装没能在这边复现 —— 本机到 Arch 镜像的 SSL 接连被重置（`unexpected eof` /
  `HTTP/2 PROTOCOL_ERROR`），连试三轮都卡在下载而不是安装或构建。那一行本身没改过，
  现有的 `check` job 一直在跑同一行。
- `cargo build --release` 通过；`ldd` 未发现 `libchafa`；`--version` 和 `--help` 正常。
- `cargo run --quiet -- check registry/`：84 项通过 · 1 个警告（本机没装 `docker`）· 0 个错误。
- `makepkg --printsrcinfo` 通过。
- **自动重载端到端**（真 pty，`script` 提供，全程不按 `Ctrl-R`）：
  - 启动后往 `tools.d/` 写一个新 manifest，界面自己出现了它 —— 不用手按 `Ctrl-R`。
  - 按 `f` 收藏（程序随即写自己的 `state.toml`）之后，状态栏最后一条仍是「已收藏 …」，
    不是「已刷新 …」，选中项也没跳回第一行。
  - 这条用**对旧实现是红的**：把路径过滤拿掉重跑，它会报「程序写自己的 state.toml
    触发了重载」。能红的用例才算钉住了回归，不是摆设。

### 更早一轮（`ui pick` 等）

- `cargo test` 在那一轮是 268 项通过、14 项 ignored（测试数后来涨到 500+，上面是本轮的实时值）。
- `toolbox-hub ui pick` 端到端验证（假文件管理器 + `script` 提供的真 pty，8 个用例）：外部程序写进自己 stdout/stderr 的画面噪音**一个字节都没有**进入调用方的 stdout；单选、多选、多行只取首条、取消（退出码 1）、`--dir-only` 取退出目录、`builtin` 强制内置、外部程序不存在时退回内置，均符合预期；完全没有终端时退出码 2 并给出可读提示。
- 验证过程踩到一个坑（记下来省下一次）：`script -c` 用的是 `$SHELL`（本机为 fish），而 fish 不认 `$?`，会让整条命令直接不执行、且失败得很安静。验证脚本里必须显式 `SHELL=/bin/sh`。

ignored 测试涉及本机 `/etc`、真实 `$HOME`、pacman/paru 状态、联网或会创建文件的外部命令，不属于当前默认 CI 的稳定测试集合。运行前应先阅读各测试的 `#[ignore]` 说明。

## 尚待维护者决定

- ~~确定真实许可证~~：已定为 **MIT**（`LICENSE` + `Cargo.toml` 的 `license` + `PKGBUILD` 的 `license=('MIT')` + `package()` 装到 `/usr/share/licenses/`）。
- 将 PKGBUILD 的占位 GitHub URL 和本地源码包来源替换为真实仓库/tag 地址，并生成校验和及 `.SRCINFO` 后再发布 AUR。
  （`packaging/update-aur.sh` 已能把 `SKIP` 换成真实校验和并重生成 `.SRCINFO`，但它只在本地
  checkout 里改；**交 AUR 前仍要人工审阅并推送**，且 `sha256sums=('SKIP')` 不该出现在 AUR 上。）
- **生成签名密钥并提交公钥**：`minisign -G -W -p packaging/minisign.pub -s ~/.minisign/toolbox-hub-ci.key`，
  私钥内容进 GitHub secret `MINISIGN_SECRET_KEY`。在公钥进仓库之前，release 流水线会**故意失败**
  （拒绝发布未签名产物），`verify-release.sh` 会以退出码 2 说明「验不了不等于验过了」。
  步骤见 `packaging/release-signing.md`。
- 当前 CLI 支持包管理操作，但还没有通用的 `toolbox-hub run <manifest-id>` 或 JSON 输出接口；这属于独立的 CLI 产品设计，不在本轮修改范围内。
- 默认 CI 继续运行格式、Clippy 和常规测试；定时真实环境冒烟测试需要先为每个 ignored 测试准备明确的依赖和隔离策略。