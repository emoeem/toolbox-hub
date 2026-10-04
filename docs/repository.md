# 工具仓库（ToolHub Repository）

> 这一页讲的是**工具箱自己的工具仓库**：搜索 → 安装 → 更新 → 卸载远程的
> 「工具包」。它和 Arch 的 pacman / AUR 软件包**没有任何关系** —— 那套在
> 软件包中心（TUI 按 p，命令行是 -s / -i / -r / -u / -l）。
> 两件事分开，是因为后果完全不同：卸错一个 Arch 包会拆掉系统。

## 一分钟上手

\`\`\`bash
# 加一个仓库（URL、本地路径、file:// 都行）
toolbox-hub repo add https://raw.githubusercontent.com/emoeem/toolbox-hub/main/registry/index.json

# 看看有哪些仓库、什么状态
toolbox-hub repo list

# 搜索（本地工具 + 仓库里的包一起搜）
toolbox-hub search ffmpeg

# 看详情（会打印完整的安装计划：来源 / 依赖 / 文件 / 哈希）
toolbox-hub info ffmpeg-extra-recipes

# 安装
toolbox-hub install ffmpeg-extra-recipes

# 列已装的工具包 / 升级 / 卸载
toolbox-hub list
toolbox-hub update
toolbox-hub uninstall ffmpeg-extra-recipes
\`\`\`

TUI 里是「发现」域（数字键 9）的第一项，或者直接按 Enter 进入。
界面里有三个面板：**发现**（搜索与安装）/ **已安装** / **仓库**（启用、添加、删除、刷新）。

## 装到哪里

全部落在**你自己的目录**里，一次也不 sudo：

| 内容 | 落点 |
| --- | --- |
| 可执行脚本（kind = bin） | `~/.local/bin`（可用 `TOOLBOX_HUB_BIN_DIR` 改） |
| 动作定义 / 文档 / 数据 | `<数据目录>/packages/<包 id>/files/…` |
| 账本（装了哪些文件、哈希） | `<数据目录>/packages/<包 id>/installed.toml` |
| 索引缓存 | `<缓存目录>/repositories/<仓库 id>/` |

声明了 `requires_root = true` 的包**不会**被自动提权安装：计划里会标出来，
执行时拒绝并说明原因。系统目录的写入留给用户自己决定。

## 可视化操作（TUI）

命令行能做的一切，界面上都有；界面不做命令行没有的事 —— 两边跑的是同一段代码
（`src/repository/`）。

### 怎么进去

按 `9` 进「发现」域（域顺序：1 媒体 · 2 图像 · 3 系统 · 4 网络 · 5 开发 · 6 工具 ·
7 包管理 · 8 打包 · 9 发现）。该域里只有一项「工具仓库 · 发现」，按 `Enter` 打开。
或者在主列表按 `/` 搜「仓库」也能找到它。

打开后长这样（108 列的真实渲染）：

```text
 工具仓库    发现 3   已安装 1   仓库 1    （工具仓库，和 pacman/AUR 的软件包是两回事）
 ────────────────────────────────────────────────────────────────────────────────
 Enter 安装 · u 刷新索引 · / 搜索 · Tab 切面板
╭ 发现（全部） ───────────────────────────────────────────────────────────────────╮
│  disk-report 1.0.0  官方   ● 已安装   看看谁占了你的磁盘：目录汇总 / 最大的文件…    │
│  system-cleanup 1.0.0  官方      先看清楚能清什么、各有多大：pacman 缓存 / 日志…    │
│  net-diagnose 1.2.0  官方      DNS / 连通 / 路由 / 端口 / 本机出口 IP              │
╰────────────────────────────────────────────────────────────────────────────────╯
╭ 详情 ─────────────────────────────────────────────────────────────────────────╮
│  disk-report  1.0.0   ● 已安装                                                  │
│  仓库       ToolHub Official  官方   随程序附带的官方索引，客户端替它的来源背书      │
│  索引       ● 在线     完整性   有 SHA-256 可核对                                │
│  Enter 先看安装计划（来源 / 依赖 / 文件 / 哈希），确认之后才动手                     │
╰────────────────────────────────────────────────────────────────────────────────╯
```

### 三个面板

| 键 | 面板 | 在那里干什么 |
| --- | --- | --- |
| `Tab` / `BackTab` | 循环切换 | 也可以直接按 `1` `2` `3` |
| `1` | 发现 | 浏览仓库里的全部包；`Enter` 摆出安装计划，再 `Enter` 确认 |
| `2` | 已安装 | `Enter` 看详情、`Delete` 卸载、`U` 升级 |
| `3` | 仓库 | `a` 添加仓库、`Space` 启用停用、`Delete` 删除、`u` 刷新全部 |

通用：`j` `k` / `↑` `↓` 移动，`g` `G` 首尾，`Ctrl-u` `Ctrl-d` 翻页，`Esc` / `q` 退出。

### 筛选分三层

1. **域**（主列表）：`1`–`9`。工具包装好后，它的动作会出现在**它自己声明的域**里
   （比如 `disk-report` 的「目录占用」在「系统」域），所以日常使用根本不用进仓库界面。
2. **二级筛选**（主列表）：`h` / `[` 与 `l` / `]` 在当前域的分类标签之间切换，
   条上第一项是「全部」。空域或只有单一分类时不显示这一条。
3. **关键词**（两处都有）：按 `/` 输入。仓库界面里匹配 **id / 名字 / 简介 / 详细说明 /
   作者 / 标签 / 分类**；标题会变成 `发现 · 搜索「磁盘」` 提示正在过滤，`Esc` 清空。

### 安装确认面板

按 `Enter`（或 `i`）不直接装，先摆一屏计划：

```text
╭ 升级 磁盘占用报告 ──────────────────────────────╮
│ 版本       1.0.0 （本地是 1.0.0）                │
│ 来源       ToolHub Official  官方               │
│ SHA-256   a1b2c3d4e5f60718…（下载后会核对…）     │
│ 文件       1 个   ⚠ 包含可执行脚本               │
│   scripts/disk-report → ~/.local/bin/disk-report│
│ 依赖       ✓ du  ✓ find                        │
│ Enter 确认   Esc 取消                           │
╰────────────────────────────────────────────────╯
```

* 依赖前面是 `✓` 说明本机有，`✗` 会写清缺什么、怎么装；
* 来源没给哈希时，会变成「再按一次 Enter 表示你接受」；
* 包自标为 `danger = "caution"`（会改动系统）时，同样要多按一次 —— 理由见「安全模型」。

## 自动更新：检查自动，应用显式

* TUI 启动时若索引过期，会在**后台**刷新（不卡界面），查完在状态栏提示
  「有 N 个工具包可升级」；
* `toolbox-hub update --check` 只报告：**有更新退出码 10**，没有 0，出错 1 ——
  脚本与定时任务靠它分流；
* `toolbox-hub update` 才真的下载替换；
* 随包提供 `packaging/toolbox-hub-update.service` 与 `.timer`（systemd 用户单元），
  默认只做检查；要无人值守自动升级就把 ExecStart 换成 `toolbox-hub update`。

为什么默认不自动应用：升级 = 下载并执行别人新写的代码。静默替换本机脚本，和
「先给你看清楚要干什么，再动手」这条纪律是冲突的。

## 安全模型

Repository 的内容是**不可信输入**。客户端对它的处置分成三件互不相同的事 ——
把它们混成一句「安全」就是在骗人：

### 包自标为「注意」时，安装要多一次确认

`danger = "caution"` 是作者说「我的动作会改动系统」。它服务的是**动作**，不是安装本身
（安装只写用户自己的目录）；但按下 install 的人往往只是想「拿个工具」，所以：

* CLI：不加 `--allow-caution` 直接拒绝，并把原因和开关打在错误里；
* TUI：确认面板第一次 Enter 只把理由摆出来（`⚠ 这个包自标为「注意」，它的动作会改动系统 ——
  再按一次 Enter 表示你接受`），第二次才动手；
* `update` 同样对待 —— 升级就是把新代码再装一遍，不该比首次安装更宽松。

理由：不弹一次的「安全提示」等于没有提示。这条门和「没有哈希要再确认一次」共用同一套
`acknowledged` 机制，两种要多按一次的情况在同一个面板上收口。

### 升级不会静默覆盖你改过的文件

账本里记着每个文件**装好那一刻**的 SHA-256。磁盘上的和它对不上，就说明你动过它 ——
这时安装 / 升级会先把这些文件列出来并拒绝，直到你明确表态：

* CLI：不加 `--allow-modified` 就拒绝，错误里列出具体路径；
* TUI：确认面板第一次 Enter 只把理由摆出来（`⚠ 有 N 个文件你装完之后改过，这次会覆盖
  它们`），第二次才动手；
* `update` 同样对待。

为什么这条必须存在：卸载时这类文件是**保留**的，而升级原来会静默覆盖 —— 同一件事
两种待遇，用户会丢改动。发现它的方式是一次实测：给装下来的脚本加了一行，作者升版本
之后 `update` 把它抹了。

三处「要多按一次」现在收口在同一个机制上：**没哈希、会改系统、你改过文件**。


| 说法 | 含义 | 怎么来的 |
| --- | --- | --- |
| **来源可信** | 客户端替这个仓库的来源背书 | 仓库配置里的 `trust = "trusted"`（随程序附带的官方仓库默认如此） |
| **完整性已核对** | 下载到的东西和索引声明的 SHA-256 逐字节一致 | 安装时算出来的 |
| **已审核** | 有人看过这份脚本在干什么 | 客户端**无法**验证，只能由仓库作者声明 |

界面上因此显示的是「官方 / 已核对 / 社区 / 未知来源」+「有 SHA-256 可核对」，
而不是一个笼统的「安全」徽章。

安装时的硬门槛（任何一条不满足就不动手）：

1. **路径安全**：包内路径必须是干净的相对路径 —— 没有 `..`、不以 `/` 或 `~` 开头、
   没有反斜杠、没有 NUL。归档解包时同样逐条检查。
2. **归档内容**：只接受普通文件与目录。符号链接、硬链接、设备文件一律拒绝
   （这是最经典的越界写文件手法）。
3. **哈希**：索引声明了 SHA-256 就必须一致，不一致**拒绝安装**。
   网络来源没有声明哈希时，需要你显式接受（CLI 加 `--allow-unverified`，
   TUI 里会让你再按一次 Enter）。
4. **不覆盖别人的文件**：目标文件已经存在、且属于别的包 → 冲突，拒绝。
   内容完全相同的孤立文件当作同一个东西，放行。
5. **元数据一致**：包内 `toolbox.toml` 声明的 id / 版本必须和索引一致。
6. **只装声明的文件**：包里多出来的文件不会被安装。
7. **requires_root** → 拒绝（见上）。

卸载与升级都守着同一条规矩：**用户改过的文件不删、不覆盖**。
账本里记着每个文件装好时的哈希，删之前逐个数核对。

## 离线优先

* 本地工具、已装的工具包、缓存里的索引 —— **没有网络也全都可用**；
* 索引带 ETag / Last-Modified 条件请求，304 就只更新「上次刷新时间」；
* 刷新失败时退回缓存，并在界面上说清「这是缓存，刷新没成功」；
* 一个仓库不可用**不会**影响别的仓库，也不会让程序崩或者卡住 ——
  界面里所有联网与哈希活都在后台线程跑。

状态标记：● 在线 / ◐ 缓存（刷新没过） / ○ 未取过 / ✕ 不可用。

## 索引格式（schema v1）

> 这一节是**协议**（客户端怎么读）。你自己做仓库时不用手写这个文件：
> `toolbox-hub build` 从 `packages/*/toolbox.toml` 生成它。


索引是一个**静态 JSON 文件**，由仓库作者维护，客户端只读：

\`\`\`json
{
  "schema_version": 1,
  "name": "ToolHub Official",
  "packages": [
    {
      "id": "ffmpeg-extra-recipes",
      "name": "FFmpeg 补充配方",
      "version": "1.0.0-1",
      "summary": "抽帧 / 封面图 / 裁剪 / 响度 / 拼接",
      "description": "只放动作定义，不放脚本：直接调用系统 ffmpeg。",
      "categories": ["media"],
      "tags": ["ffmpeg", "video"],
      "author": "emo",
      "license": "MIT",
      "source": "https://github.com/emoeem/toolbox-hub",
      "dependencies": [{ "command": "ffmpeg", "hint": "sudo pacman -S ffmpeg" }],
      "requires_root": false,
      "danger": "safe",
      "artifact": {
        "url": "artifacts/ffmpeg-extra-recipes-1.0.0-1.tar.gz",
        "sha256": "…64 位小写十六进制…",
        "kind": "tar.gz"
      },
      "files": [
        { "path": "manifests/ffmpeg-extra.toml", "kind": "manifest", "sha256": "…" }
      ]
    }
  ]
}
\`\`\`

约定：

* `artifact.url` 可以是绝对 URL，也可以是**相对索引所在目录**的相对路径 ——
  所以「把整个仓库目录放在本地」也能直接装（`toolbox-hub repo add ./registry/index.json`）。
* `artifact.kind`：`file`（单文件，产物就是那个脚本）/ `tar` / `tar.gz`。
  不写就按 URL 后缀猜。
* `files[].kind`：`bin`（装到 ~/.local/bin）/ `manifest` / `doc` / `data`
  （后三种装到包自己的目录）。不写就按路径猜（`scripts/`、`bin/`、`.sh` → bin；
  `manifests/`、`.toml` → manifest；`docs/`、`.md` → doc）。
* `files[].sha256` 可选，但建议写：装的时候会逐个核对。
* `schema_version` 不认识时客户端会**明确拒绝**并告诉你它只认 v1，不会硬读。
* 未知字段会被当作警告报出来（多半是拼错了键名），但**不影响解析**——
  索引是远端格式，加字段不该让老客户端整份作废。

## 做一个自己的仓库

仓库就是一个静态目录：**你维护源码，索引是构建产物**。不需要服务器、不需要数据库、
不需要账号。

```bash
toolbox-hub new my-tool --description "一句话说明" --domain 工具
$EDITOR my-tool/manifests/my-tool.toml     # 动作与参数
toolbox-hub check my-tool                  # 校验：会把「装到别人机器上会出事」的写法拦下来
toolbox-hub build .                        # 打包 + 生成 index.json
```

目录长这样（`index.json` 与 `artifacts/` 都是 build 的产物，别手改）：

```text
my-tools/
├── index.json            ← build 生成
├── artifacts/            ← build 生成（可复现：两次构建逐字节相同）
│   └── my-tool-1.0.0.tar.gz
└── packages/             ← 你维护的源码（唯一权威）
    └── my-tool/
        ├── toolbox.toml
        ├── manifests/my-tool.toml
        └── scripts/my-tool
```

然后 `toolbox-hub repo add <你的 index.json 地址>` 就能用了。托管到 GitHub Pages、
任意静态 HTTP、或者干脆放在本地目录都行。

**写给作者的那部分**（字段参考、动作定义、动态参数、校验规则、常见错误速查）在
[plugin-authoring.md](plugin-authoring.md)。本项目的 [registry/](../registry/) 是可以
照抄的实例 —— 那里的 `build.sh` 只是 `toolbox-hub build` 的薄包装，
真正干活的是同一段 Rust 代码，所以作者本地构建和 CI 构建不会漂。

## 命令行一览

\`\`\`text
repo list                  看有哪些仓库与状态
repo add <地址>            加一个仓库（--name / --trust / --priority）
repo remove <id>           删掉一个仓库
repo enable|disable <id>   启用 / 停用
repo update [id]           刷新索引（不带 id = 全部已启用仓库）
repo search <词>           只在仓库里搜
repo path                  打印索引缓存目录

search <词>                本地工具 + 仓库里的包一起搜（--scope all/available/installed/upgradable）
info <名字>                看详情；仓库里的包会给完整安装计划
install <包...>            安装（--allow-unverified 只在来源没给哈希时需要）
uninstall <包...>          卸载（--purge 连你改过的文件一起删）
list                       列已安装的工具包
update                     升级已安装的工具包
run <工具> [--字段 值]…    直接跑；`--dry-run` 只打印最终 argv
\`\`\`

`run` 的例子：

\`\`\`bash
toolbox-hub run ffmpeg-preset --input movie.mkv --preset 1080p --out movie-1080p.mp4
\`\`\`

它做三件事：把 `--字段` 填进表单模型 → 用 `Action::build_argv` 拼出 argv
（**永远不拼 shell 字符串**）→ 打印预览再执行。参数名写错会告诉你这个工具支持哪些。

## 动态参数（借 navi 的思路）

manifest 的字段不必只能从写死的选项里选，也不该逼用户记住有哪些编码器 / 分支 /
容器。`kind = "dynamic"` 让候选来自命令输出：

\`\`\`toml
[[action.argument]]
key = "branch"
label = "分支"
kind = "dynamic"
source = "git-branches"          # 内置名字
# source = "command:docker ps --format {{.Names}}"   # 或者直接写一条命令
\`\`\`

内置源：`git-branches` / `git-tags` / `git-remotes` / `docker-containers` /
`docker-images` / `pacman-packages` / `pacman-explicit` / `ffmpeg-video-codecs` /
`ffmpeg-audio-codecs` / `ffmpeg-formats`。

**这里没有 shell**：命令串只在空白与引号处切开，然后作为 argv 交给进程。
所以 `source = "command:ls | wc -l"` 不会去数行数（`|` 会当成普通参数传给 ls）。
不引 shell 就没有注入面；要跑复杂逻辑，把它写成一个脚本，再让 source 指向那个脚本。

候选在**后台线程**里解析（`pacman -Qq` 要几百毫秒、`docker ps` 要连 daemon），
界面里会先显示「候选解析中…」，拿到了才变成可左右选的列表；解析失败会说明原因，
字段照样能当普通文本框用。

## 代码位置

| 模块 | 职责 |
| --- | --- |
| `src/repository/index.rs` | 索引格式（外部输入、版本化、向前兼容） |
| `src/repository/config.rs` | 有哪些仓库、开没开、信任等级 |
| `src/repository/cache.rs` | 索引缓存 + 条件抓取（离线优先） |
| `src/repository/installed.rs` | 已安装包的账本（卸载/升级靠它，不靠猜路径） |
| `src/repository/install.rs` | 计划 / 校验 / 安装 / 卸载 / 更新 |
| `src/repository/service.rs` | 服务层：CLI 与 TUI **共用**的唯一入口 |
| `src/repository/worker.rs` | 把网络与哈希搬到后台线程 |
| `src/providers/repository.rs` | 已安装包里的动作 → 工具（复用 manifest 解析） |
| `src/dynamic.rs` | 动态参数候选 |
| `src/app/repo_view.rs` + `src/ui/repository.rs` | 界面状态机与绘制 |
