# Toolbox Hub

把「命令复杂、参数众多、难以记忆」的 CLI 工具，以及我自己写的脚本，统一包装成一个
**可发现、可填表、可回看**的 TUI 工具箱。

**它不是桌面应用启动器**：GUI/TUI 只是 CLI 的交互层，真正干活的一直是那些命令行工具本身。
同一个二进制也是**命令行工具**：`toolbox-hub -s fzf` / `-i fzf` / `-u` / `-l --exp` 都能直接用。

维护者文档：[项目说明与实现记录](docs/project-overview-and-work-log.md)，包含项目边界、模块分工、本轮改动和验证方式。

```text
媒体 44 │ 图像 3 │ 系统 5 │ 网络 2 │ 开发 4 │ 工具 2 │ 包管理 13
──────────────────────────────────────────────────────
 全部 · 转码 · 编辑 · 媒体 · 字幕 · 分析 · 工具 · 下载
──────────────────────────────────────────────────────
  工具                        域    Provider   分类  状态      说明
▸ ffmpeg -progress …         媒体  Manifest   转码  ● 就绪   H.264 + CRF 重编码…
  fzf-trim-video             媒体  本地脚本   编辑  ● 就绪   剪掉视频的一段…
```

## 跑起来

```bash
cd ~/code/toolbox-hub
cargo run                    # 开发版
cargo build --release && ./target/release/toolbox-hub    # 更快
```

**构建前提**：`pacman`（提供 `libalpm.so` 与 `libalpm.pc`，Arch 上本来就有）+ `pkgconf`。
二进制**动态链接 libalpm** —— 包数据层是直连 pacman 的库，不是解析它的输出（见下）。
非 Arch 系统上编译过不去，这也是有意的：这个工具箱里一半的动作本来就是 pacman / paru。

可选：`toolbox-hub [脚本目录]`（默认 `$FZF_FFTOOLS_BIN_DIR`，再默认 `~/.local/bin`）。

## 按键（进界面按 `?` 看全部）

| 场景 | 键 |
| --- | --- |
| 列表 | `↑↓`/`jk` 选择 · `g`/`Home` 首项 · `G`/`End` 末项 · `PgUp/PgDn` 或 `Ctrl+U/D` 翻页 · **鼠标左键选择 / 双击执行 / 滚轮滚动** · `Enter` 执行 · `/` 搜索（跨域） · `←→`/`1-7` 域 · `h`/`l` 分类 · `Tab` 标记 · `f` 收藏 · `v` 视图 · `H` 历史 · **`F` 看文件** · **`y` 文件管理器** · **`p` 包管理** · **鼠标点击包结果 / 双击加入队列** · `d` 改工作目录 · `Ctrl-R` 重扫 · `?`/`F1` 帮助 · `q` 退出 |
| 软件包中心（`p`） | `1-4` 切模式 · `←→`/鼠标切模式：搜索 · 已安装 · 新闻 · 维护 · 打字即本地模糊筛 · `Enter` 上网搜 / 读本地 / 抓新闻 · `↑↓` 选择 · `Tab` 换面板（结果→安装清单→包信息） · `Alt+1-9`/`Alt+0` 标签开关 · `s` 排序菜单 · `Space` 排队 · **`Enter` 先看命令、再按一次才执行** · `m` 安装/卸载/仅下载 · `U` 更新 · `c` 清缓存 · `O` 清孤儿 · `D` 演练模式 · `Ctrl+X` PKGBUILD · `Ctrl+K` 检查 · `Ctrl+E`/`Ctrl+I` 导出/导入 · 结果面板 `Ctrl+D` 翻页，其他面板 `Ctrl+D` 清空队列 |
| 表单 | `↑↓` 换字段 · `←→` 改选项 · `Enter` 编辑 · **`Ctrl-F` 挑文件** · `Ctrl-E` 执行（危险动作按两次） · `Esc` 返回 |
| 输出视图 | `↑↓` 滚动 · `←→` 横向滚动（长行不换行） · `g`/`G` 顶底 · `s` 保存 · `c` 复制 · `q` 关闭 |
| 文件视图（`F`） | `↑↓` 选文件 · 右半边**预览图片** · `Enter` 切到该文件所在目录 · 打字过滤 |
| 执行中 | `q` 取消（先 SIGTERM 让工具收尾） · 其余按键照常可用 |

## 两条加东西的路

**① 一个 CLI 动作（推荐）** —— 写个 TOML 丢进 `~/.config/toolbox-hub/tools.d/`，`Ctrl-R` 刷新，**不用重编译**：

```toml
[[action]]
id = "my-ocr"
name = "tesseract OCR"
summary = "把图片里的文字认出来"
domain = "图像"
program = "tesseract"
install = "sudo pacman -S tesseract"

[[action.argument]]
key = "image"
label = "图片"
kind = "path"
required = true
placement = "leading"          # 有些工具要求输入排在选项之前（实测 ImageMagick 就是）
repeatable = true              # 多值：填多条各占一个 argv 元素
help = "要识别的图片"
```

**批量（一个输入一个输出）**：动作上写 `foreach = "input"`，那个字段的每个取值各跑一次，
输出可以写成 `{stem}_small.mp4` —— 选 3 个文件就是 3 条命令、3 个输出：

```toml
foreach = "input"                          # 每个输入各跑一次
# 输出字段：default = "{stem}_small.mp4"   # {name}/{stem}/{ext}/{dir} 都认
```

字段全表见 `~/.config/toolbox-hub/tools.d/README.md`。同 id 会**覆盖**内置动作。

## 图片预览（`F` 视图）

`F` 视图的右半边会画选中图片的缩略图。用的是终端的图形协议：

| 终端 | 画出来是什么 |
| --- | --- |
| kitty / Konsole / WezTerm 等 | kitty 图形协议（真像素） |
| foot / xterm(sixel) / mlterm | sixel |
| iTerm2 | iTerm2 协议 |
| 都不支持 | 半块字符（`▀`），只能看个大概，但至少不是空白 |

探测**懒执行**：第一次真要画图时才问终端（最多 1 秒），所以不用这个功能的人
启动一点不慢。解码在后台线程做（一张 1200 万像素的 JPEG 要一两百毫秒，在界面
线程里就是「按一下 ↓ 卡一下」），结果只缓存当前这一张 —— 往上往下翻不重复解，
翻过去就放掉。

视频**不预览**：抓首帧要起 ffmpeg、写临时文件，几百毫秒起，不值得放在翻列表的
路径上。面板上会直说「这个类型不预览」，不留空白。

**② 自己的脚本** —— 丢进 `~/.config/toolbox-hub/tools/`，头部加注解即可被发现：

```bash
#!/usr/bin/env bash
# my-disk:summary=看磁盘谁最占地方
# my-disk:domain=系统
# my-disk:tags=磁盘
# my-disk:requires=du
```

约定见 `~/.config/toolbox-hub/tools/README.md`。

## 磁盘上有什么

| 位置 | 内容 |
| --- | --- |
| `~/.config/toolbox-hub/tools/` | 你的脚本（注解契约） |
| `~/.config/toolbox-hub/tools.d/` | 你的 manifest（**同 id 覆盖内置**） |
| `~/.config/toolbox-hub/state.toml` | 收藏 · 工作目录 · 最近目录 |
| `~/.local/share/toolbox-hub/history.log` | 执行历史（纯文本，可直接看/改） |
| `~/.local/share/toolbox-hub/install-queue.txt` | 安装清单（一行一个 `仓库/包名 版本`） |
| `~/.local/share/toolbox-hub/news-read.log` | 已读新闻（一行一个链接，删一行就等于标回未读） |
| `~/.local/share/toolbox-hub/output/` | 输出视图里按 `s` 保存的文件 |
| `manifests/*.toml`（项目内） | 内置动作，编译进二进制 |

环境变量（都可选）：`TOOLBOX_HUB_PATH`（脚本目录）、`TOOLBOX_HUB_MANIFEST_PATH`（manifest 目录）、
`TOOLBOX_HUB_WORKDIR`（启动工作目录）、`TOOLBOX_HUB_FILE_MANAGER`（默认 `yazi`）、
`TOOLBOX_HUB_CONFIG`（配置目录）、`TOOLBOX_HUB_DATA`（数据目录）、`TOOLBOX_HUB_STATE`（状态文件）。

## 配置：两个目录，别混

| 目录 | 放什么 | 默认 | 怎么覆盖 |
| --- | --- | --- | --- |
| **配置** | `state.toml`（收藏）、`tools.d/`（你的 manifest）、`tools/`（你的脚本）、`packages.toml` | `~/.config/toolbox-hub` | `--config-dir DIR` > `TOOLBOX_HUB_CONFIG` |
| **数据** | 执行历史、安装队列、搜索历史、已读新闻、输出留存 | `~/.local/share/toolbox-hub` | `--data-dir DIR` > `TOOLBOX_HUB_DATA` |

分开是因为：配置是换机器要**带走**的，数据是可再生的。想让整套都进一个目录
（比如塞进 U 盘随身带），两个参数都给：

```bash
toolbox-hub --config-dir /run/media/u/tbh/config --data-dir /run/media/u/tbh/data
```

`packages.toml` 管软件包中心的默认值，**第一次进 TUI 时会自动写一份带注释的模板**：

```toml
repos = []          # 默认只看这些仓库（空 = 全看），名字就是 pacman.conf 的段名
dry_run = false     # 打开就进「演练模式」
cache_keep = 1      # 清缓存时每个包留几个版本（界面里 [ ] 还能当场改）
sort = "相关度"      # 相关度 / 名字 / 仓库 / 得票 / 版本
mode = "搜索"        # 打开时停在哪个模式：搜索 / 已安装 / 新闻 / 维护
```

写错了不会崩：不认识的排序/模式回落到默认，拼错的字段名会在状态行里报出来
（那是最坏的情况——你以为配上了，其实没有）。

## 软件包中心（`p`，或直接进「包管理」域）

Hub 自己的统一包入口。**进「包管理」域（`7` 或 `←→`）会自动打开它** —— Esc 回到
那一域的 14 个 CLI 动作（单条查询之类）。列表里按 `p` 也是同一个界面。

版式照 `paru` 来：**整行列表在上、整行包信息在下**，状态行是
`结果 30172/30172 (n 已选)` + 右端键提示。一进来**全库（三万个包）就铺好了**，
打字即时本地过滤，`Enter` 才上网搜 AUR —— 不用对着空屏想关键词。

```text
┌ 软件包中心 ─────────────────────────────────────────────────────────────────┐
│ 结果 30172/30172  (2 已选)   筛选 fzf▏      全部 30172 个包 · 打字即时过滤   Tab:队列 · Enter:执行 · Space:多选 · s:排序 · Esc:退出 │
│ [ 搜索 ][ 已安装 ][ 新闻 ][ 维护 ] │ [extra 7299✓][aur 4600✓][blackarch 4907✓] │
│ ➤ cachyos-extra-v3  0ad       0.28.0-3.1   Cross-platform, 3D and historically-based …  │
│   extra             0ad-data  0.28.0-1     Cross-platform, 3D and historically-based …  │
│ ╭ 包信息 · 0ad ────────────────────────────────────────────────────────────╮ │
│ │ 软件库    cachyos-extra-v3      提供        无                            │ │
│ │ 名字      0ad                   依赖于      glibc  gcc-libs  …            │ │
│ │ 版本      0.28.0-3.1            与它冲突    无                            │ │
│ │ 描述      Cross-platform, …     取代        无                            │ │
│ │ 架构      x86_64_v3             下载大小    8.31 MiB                      │ │
│ │ URL       http://play0ad.com/   安装后大小  38.15 MiB                     │ │
│ │ 软件许可  GPL-2.0-or-later      打包者      …                             │ │
│ ╰──────────────────────────────────────────────────────────────────────────╯ │
└──────────────────────────────────────────────────────────────────────────────┘
```

字段名与顺序照着 `paru -Si` 的中文输出（`组`/`与它冲突`/`取代` 空着也写「无」，
少一行会让人以为没查到）。

三块屏共用同一个界面：

| 模式 | 看什么 | 主要来源 |
| --- | --- | --- |
| **搜索** | 官方源 + AUR 的搜索结果、仓库标签带条数、排序菜单 | **libalpm 直连** · AUR RPC（ureq） |
| **已安装** | 全部 / 显式 / 依赖 / 外来 / 孤儿，可直接排队卸载 | libalpm（本地库 + 一次扫完的依赖索引） |
| **新闻** | 未读 / 已读 / 全部，`★` 标出「上次升级之后发布的」 | `archlinux.org/feeds/news/` + `pacman.log` |
| **维护** | 孤儿包 / 依赖完整性 / `.pacnew` / 缓存占用 / 可更新 / 上次升级 / 文件完整性 | libalpm · `pacman -Dk` · 扫 `/etc` 与缓存目录 |

`←→` 切模式（**输入态下用 `[` `]`** —— 那时光标归 ←→ 管）、鼠标点标签也行。

**打字就是过滤**，不用先按键进输入态：一进来全库（三万个包）已经铺好了，
打 `vlc` 光标就落在 `vlc` 上。要换一个搜就 `Esc` 清空（或 `Ctrl+U`），接着打。
停手 400ms 而本地一个都没匹配上时，会自动去 AUR 问一次 —— 官方源的包本地全库
就有，AUR 的包自动补上，不用记得按什么键（`Ctrl+R` 是手动重搜）。

**多选与取消**：`Space` 加进队列（行首会出现标记），**再按一次就取消**；
`Del` 把选中的那行移出；结果面板按 `Ctrl+D` 向下翻页，切到队列或包信息面板后
`Ctrl+D` 清空整个队列。`Tab` 切到队列面板能一眼看全，
`Enter` 则是「装」——它先把要跑的命令摆出来，**再按一次才真的动系统**。

**维护模式**（`←→` 或 `[` `]` 切到第四个标签）把「该看一眼」的东西凑成一屏，
每项都能直接按 Enter 处理：

```text
✓ 正常  孤儿包          没有：装了但没人依赖的包一个都没有
✓ 正常  依赖完整性      pacman -Dk 没发现问题
! 注意  配置文件        10 个 .pacnew / .pacsave 等你合          ← Enter 看完整名单
! 注意  包缓存          651 个文件 · 8.54 GiB                    ← Enter 清旧版本
! 注意  系统更新        27 个包可以更新（库是新的）               ← Enter 系统更新
✓ 正常  上次全系统升级  2026-10-01（0 天前）
! 注意  文件完整性      要按 Enter 才查（pacman -Qk 要几秒）      ← Enter 开始检查
```

最后一项单独放出来是因为它**真的慢**（要 stat 每个包的每个文件），
其余六项都是几十到几百毫秒。

**「待更新」是按本地同步库算的**（和 `pacman -Qu` 同一口径），所以它可能比
`checkupdates` 少 —— 后者每次都重新下载数据库，代价是 18 秒。库超过一天没同步，
状态行会直接标出来（`待更新 27（库 3 天没同步）`）。

它不再启动 `pac` / `pacsea` 这类独立 TUI；真正改系统时才交给 `pacman` / `paru`。

**任何会改系统的动作都先摆到确认面板上**：那条命令就是马上要跑的那条（同一个
`command_preview()` 算出来的），要看清楚了再按第二次 `Enter`。`D` 打开演练模式后，
确认也只会把命令写进状态行，不动系统（等于 pacsea 的 `--dry-run`）。

**提权**：`pacman` / `paccache` 必须以 root 跑，非 root 时命令前面会自动补 `sudo`
（`paru` 自己会调 sudo，所以不加）—— 确认面板上看到的就是带 `sudo` 的那条。

```text
┌ 软件包中心 ─────────────────────────────────────────────────────────────────┐
│ 结果 128   排序 相关度▾   操作 安装   队列 2   待更新 12   新闻 3 未读        │
│ [ 搜索 ][ 已安装 ][ 新闻 ] │ [core 42✓] [extra 61✓] [aur 14✓]               │
│ ➤ extra  fzf     0.74.4-1  Command-line fuzzy finder   ✓ 已安装 │ 包信息 fzf  │
│   aur    ● sysz  1.4.3-1   fzf terminal UI …  票 23·1.50        │ Depends On  │
│ 搜索 fzf▏                    Enter 上网搜 · Esc 退出输入         │ glibc       │
│ ┌ 安装清单 (2) ───────────────────────────────────────────────┐ │ Download    │
│ │ aur   sysz   1.4.3-1                                        │ │ 0.5 MiB     │
│ └─────────────────────────────────────────────────────────────┘ │             │
└──────────────────────────────────────────────────────────────────────────────┘
```

队列（安装清单）会落盘：关掉界面再打开还在。搜索结果里排过队的包名字前有 `●`。

## 命令行模式（不进 TUI）

带动作参数就干完即退，能写进脚本、绑到快捷键、丢进管道：

```bash
toolbox-hub -s fzf                 # 搜官方源 + AUR
toolbox-hub -i fzf ripgrep         # 装（队列里出现 AUR 就自动走 paru）
toolbox-hub --dry-run -i fzf       # 只打印将要执行的命令，不动系统
toolbox-hub -r fzf                 # 卸（-Rns，非 root 自动补 sudo）
toolbox-hub -u                     # 系统更新
toolbox-hub -n --unread            # 只看没读过的 Arch 新闻
toolbox-hub -l --exp               # 自己点名装的包
toolbox-hub -l --imp | wc -l       # 被依赖拖进来的有多少个
toolbox-hub --orphans              # 孤儿包
toolbox-hub --clear-cache          # 清包缓存（paccache -rk1）
```

`toolbox-hub --help` 是权威列表。CLI 与 TUI **共用同一份命令翻译**
（`packages::PackageOperation` + `escalate`），不会出现两边算出来的命令不一样。

## 给脚本用的界面组件（`ui …`）

写脚本时缺的往往不是功能而是**界面**：挑一个输入文件、确认一次、翻一页长输出。
这三个组件把主界面已经在用的部件单独暴露出来，脚本拿 `$(…)` 就能消费：

```bash
src=$(toolbox-hub ui pick --dir ~/Videos --filter '*.mp4') || exit 1   # 选文件
if toolbox-hub ui confirm "要覆盖 $out 吗？" --danger; then …; fi        # 确认
ls -l | toolbox-hub ui pager --title "文件列表"                          # 翻页
```

也可以 `source scripts/tbx.sh`，用 `tbx_pick` / `tbx_confirm` / `tbx_pager`
（装上包之后在 `/usr/share/toolbox-hub/tbx.sh`），省得每次记退出码。

| 组件 | 干什么 | 主要选项 |
| --- | --- | --- |
| `ui confirm <文案>` | 弹一个圆角确认框 | `--yes/--no <文字>` · `--danger`（红框 + 默认停在「取消」）· `--default yes\|no`（没有终端时用这个答案） |
| `ui pager [文件]` | 翻页看长输出（复用输出视图：不换行、`←→` 横移、按最长行夹紧） | `--title <文字>`；不接文件就读管道 |
| `ui pick` | 在终端里挑文件，选中的路径打到 stdout（一行一个） | `--dir <目录>` · `--filter <词>` · `--multi`（`Tab` 标记）· `--dir-only` |

**三条约定**（破坏任何一条，这东西在脚本里就没法用）：

1. **stdout 只放结果，界面画在 `/dev/tty`。** 所以
   `toolbox-hub ui pick > list.txt 2>/dev/null` 照样能用 —— 判据是「能不能打开
   `/dev/tty`」，不是「stdout/stderr 是不是 tty」；
2. **退出码固定**：`0` 选了/确认，`1` 用户取消（`Esc`/`n`），`2` 没终端或参数错。
   于是 `if p=$(tbx_pick); then` 直接可用；
3. **键位与主界面一致**：`↑↓`/`jk`、`g`/`G`、`PgUp/PgDn`、`Esc` 取消、`Tab` 多选、
   打字即过滤。学一次就够。

没有终端时**不猜**：`ui confirm` 不给 `--default` 就报错退出（`2`），
`ui pick` 直接说「选文件要有人点」，只有 `ui pager` 会原样透传 ——
因为 `cmd | toolbox-hub ui pager | grep foo` 里本来就没人看界面。

## 工作目录：脚本找不到文件的根因

FFTools 那批脚本是在**工作目录**里扫文件的（`fd … .`）。从项目目录启动时那里没有媒体文件，
所以脚本「什么都找不到」。三条路解决：

1. **看** —— 头部直接写 `12 个媒体文件` / `⚠ 没有媒体文件`（规则与脚本的 `fd` 调用**完全一致**）；
2. **查** —— `F` 列出这些文件（名字/体积/相对路径），`Enter` 把工作目录切到该文件所在目录；
3. **逛** —— `y` 把终端交给 [yazi](https://yazi-rs.github.io/)，退出时**工作目录跟着你走**。

## 架构（依赖方向自上而下，没有环）

```text
main ─► cli ──► packages ──► libalpm       （命令行模式：不进 TUI，干完即退）
  │                          probe（Net）  （AUR / 新闻：常驻 HTTP 连接）
  └──► app ─► registry ─► providers ─► model
        │                    │
        ├─► ui（只读 App）    └─► metadata（依赖探测 / 路径解析）
        ├─► packages::worker               （两个常驻线程：数据库 + 网络）
        └─► runtime ────────► model        （执行：只传 argv，永不经 shell）
            media                          （工作目录里的媒体文件）
            history / state                （历史与配置，纯文本落盘）
```

| 模块 | 职责 |
| --- | --- |
| `model` | 域、工具定义、参数模型与 `build_argv` |
| `providers` | 工具从哪来：FFTools 脚本 / 本地注解脚本 / TOML manifest |
| `registry` | Provider 聚合、发现、重载、跨域搜索、收藏与最近 |
| `app` | 界面状态与按键（列表 / 表单 / 选择器 / 文件 / 历史 / 输出 / 帮助） |
| `ui` | 纯渲染 |
| `packages::libalpm` | 包数据的权威来源：搜索 / 信息 / 已安装 / 孤儿 / 可更新数 / 依赖索引（纯逻辑都有单测） |
| `packages::probe` | 网络那半边：AUR RPC 与 Arch 新闻，持有一个常驻 `ureq` agent（连接复用） |
| `packages::worker` | 两个常驻线程（数据库 + 网络），界面只跟 channel 打交道 |
| `packages` | 与取数方式无关的东西：筛选、队列、已读新闻、命令翻译（`argv` 而不是 shell 串） |
| `app::text_input` | 带光标的单行输入（字符下标，中文/emoji 不会切半） |
| `cli` | 命令行模式：`-s/-i/-r/-u/-n/-l/--clear-cache`，与 TUI 共用同一份命令翻译 |
| `runtime` | 交互式接管终端；捕获式后台任务（实时输出 / 进度 / 取消） |
| `media` | 工作目录里的媒体文件扫描 |

## 已知局限（诚实清单）

- 一次只跑**一个**后台任务（队列串行）；
- `F` 扫描有上限 500 个 / 250ms，超了显示 `≥500`；软链接目录不往里钻（脚本的 `fd --follow` 会）；
- **不是**原生调用 libav：执行层始终是 CLI，好处是稳定、可组合、可预览，代价是没有帧级精度；
- 交互式脚本（fzf 菜单那类）会接管终端、无法后台化 —— 它们本身就是 UI；
- 危险度只有 `safe` / `caution` 两级；
- 取消先 SIGTERM（发给**整个进程组**）：ffmpeg 会把已写部分收尾成一个可播的文件，
  但**不是完整结果**；
- 官方源搜索是**子串匹配**，不是正则。`pacman -Ss` 那种 `^fzf$` 写进来会被当成
  「去掉锚点的词」（排序本来就把完全同名的排最前）；要正则就用「包管理」域里那条
  `pacman -Ss` 动作；
- 二进制**动态链接 libalpm**：只支持 Arch（这个工具箱本来就一半是 pacman / paru）；
- 「可更新数」按本地同步库算，库旧了会偏小（状态行会说出来）。

## sing-box：一套内置的本机代理运维动作

`manifests/sing-box.toml` 里带了 5 个「网络」域的动作，真正干活的是 `scripts/sing-box/`
下的脚本 —— 动作只负责把开关填成表单，**改配置的安全管线全在脚本里**：

```text
改配置（内存里）→ sing-box check（不过就什么都不做）→ 时间戳备份 → 原子替换
  → 重启 → 健康检查 → 任一环节失败就自动回滚 + 重启
```

| 动作 | 干什么 |
| --- | --- |
| **sing-box 体检** | 只读：模式 / 入站 / 出口 / 分流 / DNS 拦截 / 规则集构成 / 备份，一屏看完（`--fast` 跳过联网检查） |
| **sing-box 审计修复** | 补 anti-AD 广告表、刷新 geoip/cn、清理冗余规则；6 个开关（国内 IP 表 / 直连清单 / 广告补漏 / DNS 故障转移组 / 规则集走包路径 / 广告改 NXDOMAIN） |
| **sing-box 切到 eBPF** | TUN → eBPF：本机流量在内核 socket 层接管；可选数据面 `cgroup`/`tc`、可一并接管下游接口 |
| **sing-box 切回 TUN** | eBPF → TUN：**容器 / 虚拟机也能被代理**（TUN 用 `auto_route` 覆盖转发流量） |
| **sing-box 容器代理** | 在 podman 网桥上开 eBPF `shared` 数据面，让 rootful 容器也走代理 |

源码用户先 `./scripts/sing-box/install.sh`（装到 `~/.local/bin`）；装了 Arch 包的不用管，
PKGBUILD 已经把它们放进 `/usr/bin`。细节与踩过的坑见 [`scripts/sing-box/README.md`](scripts/sing-box/README.md)。

**危险度是认真的**：四个会改配置的动作都是 `caution` + `interactive`（要 sudo 密码、
也要能看见回滚日志），只有体检是 `safe` + `capture`。有测试盯着这条策略
（`sing_box_actions_drive_the_ops_scripts`），以后谁把开关名写错、或者忘了标危险度，CI 会红。

**诚实提醒**：`tc` 数据面与 `shared` 都依赖内核的 TC eBPF 支持，而**本机内核不支持**
（实跑报 `register TC eBPF TCP listener: operation not supported`）；注意预检
`--mode local` 全绿**并不代表** TC 可用（那 28 项要 `--mode all` 才涉及，结果是 inconclusive）。
所以脚本会在动配置**之前**先拦下这两条路，并提示容器改用 `--network=host`。

## 打包与分发

`packaging/` 里有 PKGBUILD、手册页（roff）和源码包脚本：

```bash
packaging/make-source-tarball.sh     # git archive 出一个只含已提交内容的 tar.gz
cd packaging && makepkg -f           # 打 Arch 包
sudo pacman -U toolbox-hub-*.pkg.tar.zst
```

装上之后有 `toolbox-hub(1)` 手册页和 fish 补全；细节（含**交 AUR 的两步**）见
[`packaging/README.md`](packaging/README.md)。

一个实拍踩到的坑记在这儿：makepkg 默认给 `CFLAGS` 带 `-flto=auto -ffat-lto-objects`，
而 `ureq` 的 TLS 后端 `ring` 里有一段汇编，在这两个选项下链接会缺
`ring_core_0_17_14_*` 符号。PKGBUILD 的 `build()` 里把这两个选项摘掉了。

## 开发

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test                       # 快，基本 hermetic
cargo test -- --ignored          # 冒烟：真跑 ffmpeg / 7z / jq / pandoc / exiftool / magick
cargo test smoke -- --ignored    # 冒烟：真读 pacman 数据库、真联网（AUR / 新闻）
cargo build
```

这三条已经进了 CI（`.github/workflows/ci.yml`，跑在 Arch 容器里 —— 链接 libalpm）。

fish 补全在 `completions/toolbox-hub.fish`：

```bash
ln -s ~/code/toolbox-hub/completions/toolbox-hub.fish \
      ~/.config/fish/completions/toolbox-hub.fish
```

`-i` 补仓库里的包名，`-r` 补本地已装的 —— 都是现查 `pacman`，不联网。

改 `manifests/*.toml` 也要重新编译（它们是 `include_str!` 编进去的）。

测试的约定：**行为用测试钉住，工具链用冒烟钉住**。真跑那几条 (`--ignored`) 已经抓出过
「7z 只认 `-mx9`」「ImageMagick 要求输入在前」「ffmpeg 漏 `-i` 会把输入当输出」「进度行会
刷屏输出视图」这些只有真跑才看得见的问题。
