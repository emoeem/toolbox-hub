# 写一个 Toolbox Hub 插件

> 插件 = **仓库包**：一份元数据（`toolbox.toml`）+ 若干动作定义（`manifests/*.toml`）
> + 可选的自带脚本（`scripts/*`）。
> 作者只维护源码，**索引是构建产物** —— 你不用手算哈希，也不用自己写 tar。

## 五分钟走一遍

```bash
toolbox-hub new my-tool --description "把文件里的大写换成小写" --domain 工具
cd my-tool
$EDITOR manifests/my-tool.toml      # 调动作与参数
$EDITOR scripts/my-tool             # 写真正要做的事
toolbox-hub check .                 # 校验（错了会告诉你怎么改）
toolbox-hub build .                 # 打包，算出哈希与产物
toolbox-hub repo add "$PWD"         # 把当前目录当一个仓库加上去，自己先装一遍
toolbox-hub install my-tool
toolbox-hub run my-tool --input 某个文件
```

`new` 会生成一套**能直接跑通**的骨架（脚本有 `-h`、动作有必填字段），所以你不用
从空文件开始。两种形状：

| `--kind` | 包里有什么 | 适合 |
| --- | --- | --- |
| `script`（默认） | `scripts/<名字>` + 动作调用它 | 你自己的逻辑 |
| `recipe` | 只有 `manifests/*.toml` | 给**已经装好的** CLI 补一条好用的表单（例如 ffmpeg / jq / docker） |

## 目录约定

```text
registry/packages/my-tool/
├── toolbox.toml              # 元数据（唯一权威）
├── README.md                 # → doc
├── manifests/my-tool.toml    # → manifest（动作定义）
├── scripts/my-tool           # → bin（会装到 ~/.local/bin，记得 chmod +x）
├── docs/usage.md             # → doc
└── data/…                    # → data（随包分发的大文件放这里）
```

`build` 按**顶层目录名**决定每个文件怎么装：

| 放在 | 类别 | 装到 |
| --- | --- | --- |
| `scripts/` `bin/` | bin | `~/.local/bin`（自动 +x） |
| `manifests/` | manifest | 包自己的目录（被界面读成动作） |
| `docs/`、`README.md`、`LICENSE` | doc | 包自己的目录 |
| `data/` | data | 包自己的目录 |
| 其它顶层文件 | — | **只进产物，不安装**（`check` 会提醒你） |

`toolbox.toml` 是特例：它**进产物但不安装**，客户端拿它核对「索引说的版本」和
「包里写的版本」是否一致。

## toolbox.toml

```toml
[package]
id = "my-tool"              # 必填，必须等于目录名
name = "把大写换小写"        # 必填，界面上显示的名字
version = "1.0.0"           # 必填，1.0.0 / 1.0.0-1 都行
summary = "一句话说明"       # 必填，搜索结果里显示它
description = """            # 可选，长说明
支持多行。
"""
categories = ["tools"]      # 可选
tags = ["text", "batch"]    # 可选，页内筛选条用它
author = "你的名字"          # 可选
license = "MIT"             # 可选
source = "https://github.com/you/my-tool"   # 可选
homepage = "https://…"      # 可选
requires_root = false       # 可选，默认 false；见「不要 root」
danger = "safe"             # 可选：safe | caution，默认 safe
install = "sudo pacman -S coreutils"        # 可选：依赖缺失时给用户的提示
```

显式声明（一般不需要）：

```toml
# 想手工指定装哪些文件（不写就按上面的目录约定自动推导）
[[files]]
path = "scripts/my-tool"
kind = "bin"

# 想手工声明外部命令依赖（动作里的 program 会自动补进来，这里只用于补充）
[[dependencies]]
command = "coreutils"
hint = "sudo pacman -S coreutils"
```

**依赖是推出来的**：某个动作写了 `program = "jq"`，而 `jq` 不是本包 `scripts/`
提供的，那 `jq` 就自动成为这个包的依赖 —— 装它的用户会看到「✓ jq」或「✗ jq +
怎么装」。所以你不必手抄一遍，也不会漏。

## 动作定义（`manifests/*.toml`）

一个动作 = 界面上一条可填表单 + 一条命令。**命令永远是 argv 数组，不经过 shell**，
所以用户填的值不会被解释成命令（`; rm -rf /` 只是一个普通参数）。

### 动作的字段

| 字段 | 必填 | 说明 |
| --- | --- | --- |
| `id` | ✓ | 动作 id，同一文件内唯一 |
| `name` | ✓ | 界面上显示的名字 |
| `summary` | ✓ | 一句话说明 |
| `domain` | ✓ | 媒体 / 图像 / 系统 / 网络 / 开发 / 工具 / 包管理 / 打包 / 发现 |
| `program` | ✓ | 要跑的命令。`script` 形状写脚本名；`recipe` 形状写已装的 CLI |
| `base_argv` | | 无条件加在最前面的参数，例如 `["--no-mtime"]` |
| `tags` | | 二级筛选条 |
| `mode` | | `capture`（默认，收输出进内置阅读器）/ `interactive`（接管终端）/ `native`（内置界面） |
| `danger` | | `safe`（默认）/ `caution`（执行前再确认一次） |
| `install` | | 依赖缺失时给用户的提示 |
| `input` `output` `features` | | 详情区里那三行说明 |
| `allow_empty` | | 允许不带任何参数运行（默认不允许，防止忘了写 `base_argv`） |
| `ok_exit_codes` | | 哪些退出码也算成功，例如 `[0, 1]` |
| `pin` | | 在本域里置顶（越小越靠前） |
| `foreach` | | 批量：指定一个字段，**每个取值各跑一次** |
| `duration_from` `limit_from` | | 进度条的总时长从哪个参数（文件）探 |

`interactive` 什么时候用：这个命令自己就是个全屏界面（fzf、btop、`sudo` 要密码、
编辑器）。工具箱会把终端整个交给它，退出后再切回来。

### 参数的字段

| 字段 | 说明 |
| --- | --- |
| `key` | 取值表的键，也是 `run --key value` 的名字 |
| `label` | 界面上显示的中文标签 |
| `kind` | `text` / `path` / `choice` / `toggle` / **`dynamic`** |
| `required` | 必填：为空时不允许执行（不会替你猜） |
| `default` | 默认值。`toggle` 用 `"true"`/`"false"`；`choice` 用选项的 `value` |
| `help` | 字段下面那句话 |
| `flag` | 进 argv 的形式：`"--quality"` → `--quality <值>`；不写就是位置参数 |
| `placement` | `leading` / `middle` / `trailing`（默认：带 flag → middle，否则 trailing） |
| `flag_join` | `true` → 贴成一个元素（`-mx9`、`-o/tmp`） |
| `repeatable` | 这一项能填多条；带 flag 时默认「flag 一次、值平铺」（`-S a b c`） |
| `repeat_flag` | `true` → 每个值配一个 flag（`-i a -i b`） |
| `separator` | 多值分隔符，默认逗号 |
| `choices` | `choice` 的选项：`{ label = "1080p", value = "…" }`（显示与实参是两回事） |
| `sensitive` | 敏感值：写进执行历史前会被换成 `***` |
| `dir_only` | 这个路径字段要的是目录 |
| `source` | `dynamic` 的候选来源 |

**`placement` 为什么存在**：不是所有工具都接受「选项在前、对象在后」。
实测 ImageMagick 要求输入排在操作之前（`magick in.png -resize 50% out.webp`），
ffmpeg 则要求编码选项放在 `-i` 之后 —— 位置必须能显式说清楚。

## 动态参数（借 navi 的思路）

不要逼用户去背「有哪些编码器 / 分支 / 容器」—— 机器知道：

```toml
[[action.argument]]
key = "branch"
label = "分支"
kind = "dynamic"
source = "git-branches"
```

内置 source：`git-branches` `git-tags` `git-remotes` `docker-containers`
`docker-images` `pacman-packages` `pacman-explicit` `ffmpeg-video-codecs`
`ffmpeg-audio-codecs` `ffmpeg-formats`。

也可以自己给一条命令：`source = "command:docker ps --format {{.Names}}"`。

**这里没有 shell**：命令串只在空白与引号处切开，然后作为 argv 交给进程。
所以 `command:ls | wc -l` 不会去数行数。要跑复杂逻辑，把它写进 `scripts/` 里的
一个脚本，再让 `source` 指向那个脚本 —— 那也是更可测的做法。

候选在后台线程解析，界面先显示「候选解析中…」，失败会说明原因，字段照样能当普通
文本框用（候选不是限制）。

## 脚本怎么写

```bash
#!/bin/sh
set -eu
# 1. 参数是 argv，不是 shell 串：带空格的名字不用你转义
# 2. 支持 -h / --help —— 用户和你未来的自己都会需要
# 3. 正常输出走 stdout，诊断/进度走 stderr（工具箱分开显示）
# 4. 退出码：0 成功；非 0 会被报成失败（除非在 ok_exit_codes 里声明）
# 5. 批量动作会被调用多次，每次一个输入 —— 别自己写循环去处理目录
```

几条实践：

* **一个动作只做一件事**。三个动作比一个「带五种模式」的动作好用得多。
* 需要确认的破坏性操作，用 `danger = "caution"` 标明，别让用户靠猜。
* 大文件放 `data/`，别塞进 `scripts/`。
* 脚本不要依赖当前工作目录：工具箱会在**用户的工作目录**里执行它。

## 校验会拒绝什么

`toolbox-hub check` 不只是「格式对不对」，它盯的是**装到别人机器上会出事**的东西：

| 检查 | 结果 |
| --- | --- |
| 包里的符号链接 | **错误**（越界写文件最经典的手法，客户端也会拒绝） |
| 路径里的 `..`、绝对路径、`~` | **错误** |
| `id` 与目录名不一致 | **错误**（build 按目录找包） |
| 版本号读不懂 | **错误** |
| `summary` 为空 | **错误** |
| 包里一个可安装文件都没有 | **错误** |
| 改了源码但没重新 `build` | **错误**（索引与源码对不上） |
| 索引里有、源码里没有（或反之） | **错误** |
| 产物哈希与索引对不上 | **错误** |
| `scripts/` 里的文件没有 +x | 警告 |
| 顶层放了不认识的目录/文件 | 警告（它会进产物但不会被安装） |
| 动作里用到的命令本机没有 | 警告（用户那边会显示「依赖缺失」） |
| `manifests/` 里一个可用动作都没有 | 警告（装了也没入口） |

校验整个仓库（`check <仓库根>`）还会额外核对：索引 ↔ 产物 ↔ 源码三方一致、
每个包的产物哈希、`files[]` 内容哈希、有没有包没进索引。

## 产物与发布

```bash
toolbox-hub build .          # 打这一个包
toolbox-hub build registry/  # 扫 packages/* 全部重打，并重建 index.json
```

`build` 的产物**可复现**：同样的源码，两次构建逐字节相同（条目排序、mtime/uid/gid
归零、路径不带 `./` 前缀、gzip 头不带时间戳）。这不是洁癖 —— 哈希对不上就不可能
谈「这个包是谁发的」。

`index.json` 是**生成物**，别手改。默认不写 `updated`（写了每次构建索引都会变，
可复现就没了）；要盖时间戳用 `build --stamp`。

发布 = 把这个目录推到任何能放静态文件的地方：

```text
你的仓库/
├── index.json          ← build 生成
├── artifacts/*.tar.gz  ← build 生成
└── packages/*/         ← 你维护的源码
```

GitHub Pages、任意静态 HTTP、甚至一个本地目录都行。别人这样装：

```bash
toolbox-hub repo add https://你的域名/index.json
toolbox-hub search 关键词
toolbox-hub install 你的包
toolbox-hub update            # 你发新版本后，用户这样升
```

版本号用语义化版本；改了文件内容就是新版本（哈希会变），`list` / `update` 靠它工作。

## 常见错误速查

| 症状 | 原因 |
| --- | --- |
| `check` 说「目录名与 id 不一致」 | 改目录名或改 `id`，两者必须一样 |
| `check` 说「改了没重新 build」 | 跑一次 `toolbox-hub build <仓库根>` |
| 装上了但界面里看不到入口 | `manifests/` 没放对地方，或 `domain` 写错（`check` 会警告） |
| 一跑就「依赖缺失」 | 动作的 `program` 不在 PATH 上，把 `install` 提示补上 |
| 命令跑起来但参数顺序不对 | 用 `placement` 显式指定（ImageMagick 那种输入要在前） |
| 选项被拆成两半（`-mx9` 变成 `-mx 9`） | 加 `flag_join = true` |
| 用户填的值里带空格结果出错 | 不该出错 —— 值永远是单个 argv 元素；检查脚本自己有没有二次拼接 shell |
| `build` 报「没有任何可安装的文件」 | 文件放错目录了，看上面的目录约定表 |

## 一个完整例子

```text
packages/json-peek/
├── toolbox.toml
├── manifests/json-peek.toml
└── docs/usage.md
```

```toml
# toolbox.toml
[package]
id = "json-peek"
name = "看一眼 JSON"
version = "1.0.0"
summary = "从 JSON 里挑出你要的那一段，不用记 jq 的语法"
tags = ["json"]
license = "MIT"
install = "sudo pacman -S jq"
```

```toml
# manifests/json-peek.toml
[[action]]
id = "json-peek-field"
name = "取一个字段"
summary = "把一个 JSON 文件里的某个字段取出来"
domain = "开发"
program = "jq"
mode = "capture"

[[action.argument]]
key = "filter"
label = "表达式"
kind = "text"
default = "."
required = true
help = "例如 .name / .items[0].id"

[[action.argument]]
key = "file"
label = "文件"
kind = "path"
required = true
placement = "trailing"
```

`toolbox-hub check .` → 通过；`toolbox-hub build .` → 产出 `artifacts/json-peek-1.0.0.tar.gz`
与哈希；用户 `repo add` 之后就能搜到、装上、在「开发」域里看到这条表单。

## 下一步

* 协议细节（索引 schema、安全模型、离线策略）：[repository.md](repository.md)
* 官方样例仓库：[../registry/](../registry/) —— `registry/build.sh` 与
  `toolbox-hub build registry/` 等价，你可以照抄目录结构。
* 想贡献到官方仓库：把插件放进 `registry/packages/<id>/`，跑
  `toolbox-hub check registry/` 与 `toolbox-hub build registry/` 都干净，
  提 PR。
