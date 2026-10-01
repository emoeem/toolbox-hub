# Toolbox Hub

把「命令复杂、参数众多、难以记忆」的 CLI 工具，以及我自己写的脚本，统一包装成一个
**可发现、可填表、可回看**的 TUI 工具箱。

**它不是桌面应用启动器**：GUI/TUI 只是 CLI 的交互层，真正干活的一直是那些命令行工具本身。

```text
媒体 44 │ 图像 3 │ 系统 5 │ 网络 2 │ 开发 4 │ 工具 2 │ 包管理 15
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

可选：`toolbox-hub [脚本目录]`（默认 `$FZF_FFTOOLS_BIN_DIR`，再默认 `~/.local/bin`）。

## 按键（进界面按 `?` 看全部）

| 场景 | 键 |
| --- | --- |
| 列表 | `↑↓`/`jk` 选择 · `Enter` 执行 · `/` 搜索（跨域） · `←→`/`1-7` 域 · `h`/`l` 分类 · `Tab` 标记 · `f` 收藏 · `v` 视图 · `H` 历史 · **`F` 看文件** · **`y` 文件管理器** · **`p` 包管理** · **`d` 改工作目录** · `Ctrl-R` 重扫 · `?` 帮助 · `q` 退出 |
| 包管理（`p`） | 打字+`Enter` 搜官方源+AUR · `1-9`/`0` 仓库标签 · `Space` 排队 · `Tab` 结果⇄队列 · `Enter` 安装（按两次） · `Ctrl+X` PKGBUILD · `Ctrl+N` 新闻 · `Ctrl+E`/`Ctrl+I` 导出/导入队列 · `o` 浏览器看 AUR 页 |
| 表单 | `↑↓` 换字段 · `←→` 改选项 · `Enter` 编辑 · **`Ctrl-F` 挑文件** · `Ctrl-E` 执行（危险动作按两次） · `Esc` 返回 |
| 输出视图 | `↑↓` 滚动 · `g`/`G` 顶底 · `s` 保存 · `c` 复制 · `q` 关闭 |
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
| `~/.local/share/toolbox-hub/output/` | 输出视图里按 `s` 保存的文件 |
| `manifests/*.toml`（项目内） | 内置动作，编译进二进制 |

环境变量（都可选）：`TOOLBOX_HUB_PATH`（脚本目录）、`TOOLBOX_HUB_MANIFEST_PATH`（manifest 目录）、
`TOOLBOX_HUB_WORKDIR`（启动工作目录）、`TOOLBOX_HUB_FILE_MANAGER`（默认 `yazi`）、
`TOOLBOX_HUB_DATA`、`TOOLBOX_HUB_STATE`。

## 原生包管理（`p`）

照 [pacsea](https://github.com/Firstp1ck/Pacsea) 的布局自己实现的一个包管理界面 ——
**不是**去启动 `pac`/`pacsea`，搜索、解析、筛选、队列、状态全在这个 Rust 程序里；
只有「真正改系统」的那一下交给 `paru -S`（pacsea 也是这么做的）。

| 数据 | 来源 |
| --- | --- |
| 官方源搜索 / 信息 | `LC_ALL=C pacman -Ss` / `-Sii`（锁 locale 是因为标记会本地化成 `[已安装]`） |
| AUR 搜索 / 信息 | AUR 官方 RPC + `serde_json`：得票、热度、维护者、是否过期、依赖 |
| 已安装集合 | `pacman -Qq` 一次拿全 |
| PKGBUILD | `paru -Gp`，拿输出视图看 |
| Arch 新闻 | `archlinux.org/feeds/news/`，并对比 `pacman.log` 里最后一次全系统更新，提醒未读 |

```text
┌ 结果 128 条 · [core✓][extra✓][aur✓] · 队列 2 · 「fzf」128 个结果 ──────────┐
│ > extra  fzf        0.74.4-1  Command-line fuzzy finder    ✓ 已安装       │
│   aur    fzf-git    0.74.4…   fzf from git                 票 12·0.31    │
├ 搜索 fzf▏                     Enter 搜索 · Esc 退出输入                  │
├ 包信息 fzf                    Depends On  glibc  …                       │
└ Space 排队 · Enter 安装 · Tab 队列 · Ctrl+X PKGBUILD · Ctrl+N 新闻 …     ┘
```

## 工作目录：脚本找不到文件的根因

FFTools 那批脚本是在**工作目录**里扫文件的（`fd … .`）。从项目目录启动时那里没有媒体文件，
所以脚本「什么都找不到」。三条路解决：

1. **看** —— 头部直接写 `12 个媒体文件` / `⚠ 没有媒体文件`（规则与脚本的 `fd` 调用**完全一致**）；
2. **查** —— `F` 列出这些文件（名字/体积/相对路径），`Enter` 把工作目录切到该文件所在目录；
3. **逛** —— `y` 把终端交给 [yazi](https://yazi-rs.github.io/)，退出时**工作目录跟着你走**。

## 架构（依赖方向自上而下，没有环）

```text
main ─► app ─► registry ─► providers ─► model
        │                    │
        ├─► ui（只读 App）    └─► metadata（依赖探测 / 路径解析）
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
| `runtime` | 交互式接管终端；捕获式后台任务（实时输出 / 进度 / 取消） |
| `media` | 工作目录里的媒体文件扫描 |

## 已知局限（诚实清单）

- 一次只跑**一个**后台任务（队列串行）；
- `F` 扫描有上限 500 个 / 250ms，超了显示 `≥500`；软链接目录不往里钻（脚本的 `fd --follow` 会）；
- **不是**原生调用 libav：执行层始终是 CLI，好处是稳定、可组合、可预览，代价是没有帧级精度；
- 交互式脚本（fzf 菜单那类）会接管终端、无法后台化 —— 它们本身就是 UI；
- 危险度只有 `safe` / `caution` 两级；
- 取消先 SIGTERM：ffmpeg 会把已写部分收尾成一个可播的文件，但**不是完整结果**。

## 开发

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test                       # 快，全 hermetic
cargo test -- --ignored          # 冒烟：真跑 ffmpeg / 7z / jq / pandoc / exiftool / magick
cargo build
```

改 `manifests/*.toml` 也要重新编译（它们是 `include_str!` 编进去的）。

测试的约定：**行为用测试钉住，工具链用冒烟钉住**。真跑那几条 (`--ignored`) 已经抓出过
「7z 只认 `-mx9`」「ImageMagick 要求输入在前」「ffmpeg 漏 `-i` 会把输入当输出」「进度行会
刷屏输出视图」这些只有真跑才看得见的问题。
