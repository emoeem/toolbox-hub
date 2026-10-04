# ToolHub Official —— 工具箱自己的工具仓库

这是 [ToolHub](../README.md)（`toolbox-hub`）的仓库数据：**一个静态 JSON 索引 + 若干产物文件**。
客户端把它加进来之后，就能搜索、看详情、安装、卸载里面的工具包。

它和 pacman / AUR **没有任何关系**：那边管的是 Arch 软件包（`-s` / `-i` / `-r` 这些老命令），
这边管的是 ToolHub 自己的工具包（`search` / `info` / `install` / `uninstall` 这些动词）。

> **这是纯静态数据，不是服务端。** 没有后台进程、没有数据库、没有账号，也没有任何需要运行的代码：
> 客户端只是 GET 一个 `index.json`，再 GET 它里面点到的那几个产物文件。所以它可以放在任何
> 能发文件的地方 —— GitHub Pages、对象存储、nginx、公司内网、甚至一个 U 盘。

## 目录结构

```text
registry/
├── index.json                 ← 索引：客户端唯一入口（schema_version = 1）
├── build.sh                   ← 只是 `toolbox-hub build` 的薄包装（真正干活在客户端里）
├── artifacts/                 ← 产物（索引里 artifact.url 指向的就是这些）
│   ├── hello-tool.sh
│   ├── demo-actions-1.0.0.tar.gz
│   └── ffmpeg-extra-recipes-1.0.0-1.tar.gz
└── packages/                  ← **唯一权威**：每个插件一个目录（见 docs/plugin-authoring.md）
    ├── hello-tool/scripts/hello-tool
    ├── demo-actions/{toolbox.toml,manifests/demo.toml,scripts/demo-echo,README.md}
    └── ffmpeg-extra-recipes/manifests/ffmpeg-extra.toml
```

`index.json` 与 `artifacts/` 都是**生成物**：不要手改，改 `packages/` 再跑
`toolbox-hub build registry/`（或等价的 `bash registry/build.sh`）。
**索引是从源码生成的** —— 手工维护的索引迟早会和源码对不上（版本、哈希）。

插件怎么写：**[../docs/plugin-authoring.md](../docs/plugin-authoring.md)**。

## 这个仓库里有哪三个包

| 包 | 版本 | 产物 | 装了什么 |
| --- | --- | --- | --- |
| `hello-tool` | 1.0.0 | 单文件（`kind = "file"`） | 一个可执行脚本 `~/.local/bin/hello-tool` |
| `demo-actions` | 1.0.0 | tar.gz | `demo-echo` 命令 + `manifests/demo.toml` 两条动作 + `toolbox.toml` + `README.md` |
| `ffmpeg-extra-recipes` | 1.0.0-1 | tar.gz | **只有** `manifests/ffmpeg-extra.toml`（没有脚本，动作直接调 `ffmpeg` / `ffprobe`） |

后两个演示了两件事：一个包可以**同时带命令和动作**；一个包也可以**只是一个 recipe**。

## 本地加这个仓库

在仓库根目录（也就是 `registry/` 的上一层）：

```console
$ toolbox-hub repo add ./registry/index.json
$ toolbox-hub repo list                 # 看看加进去了没有
$ toolbox-hub repo update               # 重新拉一遍索引
```

索引里的 `artifact.url` 全是**相对路径**，客户端会拿它相对 `index.json` 所在目录去解析，
所以「整个 registry 目录搬到哪，产物就跟到哪」。也可以写绝对路径或 `file://` 形式：

```console
$ toolbox-hub repo add /home/me/toolbox-hub/registry/index.json --name official
$ toolbox-hub repo add file:///srv/tool-hub/index.json --name intranet
```

加完之后：

```console
$ toolbox-hub search hello          # 搜索引（id / 名字 / 简介 / 标签 / 分类 / 作者）
$ toolbox-hub info hello-tool       # 看详情与安装计划（会列出依赖、要落哪些文件）
$ toolbox-hub install hello-tool    # 装（默认要哈希对得上）
$ toolbox-hub uninstall hello-tool  # 卸（你改过的文件会留着，只报告）
```

## 托管到静态站点

索引里的路径全是相对路径，所以**把整个 `registry/` 目录原样发布出去**就行，不需要任何服务端逻辑。

**GitHub Pages**：把 `registry/` 的内容推到 Pages 分支（或 `docs/` 目录），然后

```console
$ toolbox-hub repo add https://<用户名>.github.io/<仓库>/index.json --name official
```

**GitHub raw / 对象存储 / 任何静态 HTTP**：

```console
$ toolbox-hub repo add https://raw.githubusercontent.com/<用户名>/<仓库>/main/registry/index.json
$ toolbox-hub repo add https://example.com/tools/index.json
```

本地起一个看一眼：

```console
$ python3 -m http.server 8080 --directory registry
# 然后 repo add http://127.0.0.1:8080/index.json
```

几个注意点：

* 产物要**跟着索引一起发布**，路径按 `index.json` 里的 `artifact.url` 摆；
* 走 HTTP 的仓库，`artifact.sha256` **必须**写对 —— 远程产物没有哈希，客户端默认拒绝安装
  （除非用户显式 `--allow-unverified`）；
* 更新索引后记得让客户端刷新：`toolbox-hub repo update`（仓库客户端有缓存，不会每次重下）。

## 索引格式（schema_version = 1）

顶层只有这三样，外加可选的 `name` / `updated`：

```json
{
  "schema_version": 1,
  "name": "ToolHub Official",
  "updated": "2026-10-03",
  "packages": [ /* … */ ]
}
```

一个包：

| 字段 | 必填 | 说明 |
| --- | --- | --- |
| `id` | ✅ | 小写字母 / 数字 / `-` `_` `.`；也是安装目录名 |
| `name` | ✅ | 展示名 |
| `version` | ✅ | 要能解析：`1.0.0`、`1.0.0-1`（pkgrel）、`1.0.0-alpha.1` 都行 |
| `summary` | | 一句话；列表里显示 |
| `description` | | 长一点的说明 |
| `categories` / `tags` | | 搜索用 |
| `author` / `license` / `source` | | 展示「这东西从哪来」 |
| `dependencies` | | `[{"command": "ffmpeg", "hint": "sudo pacman -S ffmpeg"}]`，按**命令名**判断缺没缺 |
| `requires_root` | | 本仓库一律 `false`：客户端不会替你提权 |
| `danger` | | `safe` / `caution` |
| `artifact` | ✅ | `{url, sha256, kind, size}`，见下 |
| `files` | ✅ | 装哪些文件、落到哪，见下 |

产物 `artifact`：

* `url`：相对 `index.json` 的路径（推荐），也可以是绝对 URL / 绝对路径 / `file://`；
* `sha256`：**产物文件本身**的 64 位小写十六进制；远程仓库必须有；
* `kind`：只能是 `file`（单文件）/ `tar` / `tar.gz`；
* `size`：字节数（可选，给人看的）。

文件清单 `files[]`：`path` 是**包内相对路径**（tar 包里的条目路径），`kind` 决定落到哪：

| `kind` | 落到哪 |
| --- | --- |
| `bin` | `~/.local/bin/<文件名>`（可执行） |
| `manifest` | 包自己的目录（工具定义，由 Provider 读） |
| `doc` / `data` | 包自己的目录 |

`sha256`（可选但**强烈建议**）是**该文件内容**的哈希：tar 包就是解出来的那一份文件的哈希，
单文件产物就是产物本身的哈希。它在安装前逐文件核对，对不上直接拒绝。

## 加一个新包

以 `my-tool` 为例，两种写法：

**A. 单文件包（就是一个脚本）**

1. 放源码：`packages/my-tool/scripts/my-tool`（记得 `chmod +x`）；
2. 在 `index.json` 的 `packages` 里加一条：

   ```json
   {
     "id": "my-tool",
     "name": "My Tool",
     "version": "1.0.0",
     "summary": "一句话说明",
     "categories": ["tools"],
     "tags": ["示例"],
     "author": "emo",
     "license": "MIT",
     "source": "https://github.com/…",
     "dependencies": [],
     "requires_root": false,
     "danger": "safe",
     "artifact": { "url": "artifacts/my-tool.sh", "sha256": "全 0 占位", "kind": "file" },
     "files": [{ "path": "scripts/my-tool", "kind": "bin", "sha256": "全 0 占位" }]
   }
   ```

   （`sha256` 先写 64 个 `0` 占位，下一步会重算；别漏了这一项，否则 JSON 形状不对）

3. `toolbox-hub build registry/` —— 产物、哈希、索引一起生成。

**B. tar.gz 包（命令 + 动作 + 文档）**

1. 建 `packages/my-tool/`，里面按包内路径摆文件：

   ```text
   packages/my-tool/
   ├── toolbox.toml          ← [package] id / version 必须与索引完全一致
   ├── manifests/my-tool.toml
   └── scripts/my-tool-cmd
   ```

2. 索引里 `artifact.kind` 写 `"tar.gz"`、`url` 写 `artifacts/my-tool-1.0.0.tar.gz`，
   `files` 逐个列出包内路径（`manifests/*.toml` → `manifest`，`scripts/*` → `bin`，`README.md` → `doc`，
   `toolbox.toml` → `data`）；
3. `toolbox-hub build registry/`。

**动作（manifest）怎么写**：照抄 `packages/demo-actions/manifests/demo.toml` 或
`packages/ffmpeg-extra-recipes/manifests/ffmpeg-extra.toml`。字段是**严格**的
（解析器开了 `deny_unknown_fields`，拼错一个键名就会报错），常用的有：

```toml
[[action]]
id = "my-tool-hello"        # 必填
name = "打个招呼"            # 必填
summary = "一句话"           # 必填
domain = "工具"              # 必填：工具/媒体/图像/系统/网络/开发/包管理/打包（或 tools/media/…）
program = "my-tool-cmd"     # 必填：命令名
base_argv = ["--verbose"]   # 可选：固定放在最前面的参数
mode = "capture"            # 可选：capture（默认）/ interactive / native

[[action.argument]]
key = "name"                # 必填
label = "名字"               # 必填
kind = "text"               # 必填：text / path / choice / toggle
required = true
help = "显示在表单里的一句话"
```

拼 argv 的规则很直白：`base_argv` 打头，然后 `placement = "leading"` 的参数，
再是有 flag 的参数，最后是位置参数（输出文件之类）。所以
「`-i 输入 … 输出`」这种顺序靠 `placement` 就能摆对。

## 重新生成

```console
$ bash registry/build.sh            # 重算 artifact.sha256 / size 与 files[].sha256
$ bash registry/build.sh --stamp    # 顺带把 updated 改成今天
```

它是**幂等**的：同一份源码跑两次，产物与索引逐字节相同。为此在打包时显式固定了
`--sort=name --owner=0 --group=0 --numeric-owner --mtime='@0'`，并用 `gzip -n`
（不写文件名与时间戳）；打包前还会把权限位统一 `chmod`。所以 mac / Linux、不同用户、
不同 umask 上打出来的包，哈希都是同一个。

两个刻意的细节：

* 归档里的条目**落在根部**（`toolbox.toml`、`manifests/demo.toml`…），不套顶层目录 ——
  客户端是按「包内相对路径」取文件的；
* 不写 `tar -cf - .`，而是把**文件**清单交给 tar。`.` 会多带一个 `./` 目录条目，
  客户端的路径校验会把它当空路径拒掉；而把「目录 + 目录里的文件」一起交给 tar，
  同一个文件会被收两遍（第二遍是硬链接条目）。

`toolbox-hub check registry/` 会校验：索引声明的每个 `files[].path` 都要在产物里找得到，
产物里也不许有索引没声明的文件。对不上就报错退出，并且**不写** `index.json`。

## 自己核对一遍

```console
$ python3 -c "import json;json.load(open('registry/index.json'))"   # 1. JSON 合法
$ bash registry/build.sh && sha256sum registry/artifacts/*          # 2. 重算一遍
$ python3 - <<'PY'                                                  # 3. 索引里的哈希 vs 磁盘
import json, hashlib, os
os.chdir("registry")
index = json.load(open("index.json"))
for package in index["packages"]:
    artifact = package["artifact"]
    got = hashlib.sha256(open(artifact["url"], "rb").read()).hexdigest()
    print(package["id"], "OK" if got == artifact["sha256"] else "MISMATCH")
PY
```

## License

索引与这里的脚本自身按仓库的 MIT 许可发布；各个包对上游工具的调用不改变上游的许可。
