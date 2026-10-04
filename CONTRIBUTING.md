# 贡献插件

Toolbox Hub 的插件（仓库包）是**纯静态数据**：一份 `toolbox.toml` + 若干 `manifests/*.toml`，
可选自带脚本。没有服务端、没有账号，别人装上它，就能在界面里看到你写的那张表单。

字段参考、动态参数、校验规则这些**细节**在 [docs/plugin-authoring.md](docs/plugin-authoring.md)。
这份文件只讲**流程**：怎么起步、提交到哪、会被怎么验收。

## 两条路，先想清楚走哪条

| | 官方仓库（本仓库） | 自建仓库（你自己的仓库） |
| --- | --- | --- |
| 包放哪 | `registry/packages/<id>/` | 你随便 |
| 索引谁生成 | `toolbox-hub build registry/`，产物要一起提交 | `toolbox-hub build <你的仓库根>`，产物随你放 |
| 谁能装 | 所有加了官方源的人 | 只要知道地址的人 |
| 要不要评审 | 要过 PR 评审（安全边界为主） | 不用，你自己把关 |
| 适合 | 通用、别人也用得上的工具 | 内部工具、实验品、个人脚本 |

两条路用的是同一套格式、同一个 `toolbox-hub`。自建的东西以后想搬进官方，把目录复制过来、
重跑一遍 `build` 就行。

## 选形状：script 还是 recipe

| `--kind` | 包里有什么 | 什么时候用 |
| --- | --- | --- |
| `script`（默认） | `scripts/<名字>` + 动作调用它 | 逻辑是你自己写的 |
| `recipe` | 只有 `manifests/*.toml` | 包装一个**已经装好的** CLI（jq / ffmpeg / docker / git …） |

判断标准一句话：**要跑的命令是不是你这个包提供的**。是 → `script`；不是 → `recipe`，
并且把那个命令填进 `program`（它会自动变成这个包的依赖）。

## 五分钟写一个

在仓库根目录：

```bash
toolbox-hub new my-tool --dir registry/packages \
    --description "把文件里的大写换成小写" --domain 工具
```

生成出来的是**能直接跑通**的骨架（脚本带 `-h`、动作有必填字段）：

```text
registry/packages/my-tool/
├── toolbox.toml                # 元数据（唯一权威）
├── manifests/my-tool.toml      # 动作与参数
├── scripts/my-tool             # 只有 script 形状有；已经是 755
├── README.md
└── docs/usage.md
```

（`--dir` 也可以不写：在 `registry/` 里执行时会自动放进 `registry/packages/`；
仓库里没有 `packages/` 时则放进当前目录。）

然后就是「改 → 校验 → 打包」：

```bash
$EDITOR registry/packages/my-tool/toolbox.toml          # 说明 / 域 / 依赖
$EDITOR registry/packages/my-tool/manifests/my-tool.toml # 动作与参数
$EDITOR registry/packages/my-tool/scripts/my-tool        # script 形状：真正要做的事

toolbox-hub check registry/packages/my-tool   # 单个包：有错返回 1，没错返回 0
toolbox-hub build registry/                   # 打包 + 重建 index.json（可复现）
toolbox-hub check registry/                   # 索引 ↔ 产物 ↔ 源码，三方一致
```

顺序有讲究：**先 `build` 再 `check registry/`**。新包的产物和索引条目还没生成时，
整仓校验会报一句「`packages/my-tool/` 存在但索引里没有它 —— 跑一次 build」——这正是它该说的话。

自己先装一遍（本地目录也能当仓库）：

```bash
toolbox-hub repo add "$PWD/registry/index.json" --name local
toolbox-hub info my-tool
toolbox-hub install my-tool
toolbox-hub run my-tool --input 某个文件
toolbox-hub uninstall my-tool
```

## 提交到官方仓库

1. **Fork** [emoeem/toolbox-hub](https://github.com/emoeem/toolbox-hub)，clone 你的 fork，开一个分支：

   ```bash
   git clone https://github.com/<你>/toolbox-hub
   cd toolbox-hub
   git checkout -b add-my-tool
   ```

2. 在 `registry/packages/<id>/` 下建你的包（`id` 必须等于目录名）：

   ```bash
   toolbox-hub new my-tool --dir registry/packages \
       --description "把文件里的大写换成小写" --domain 工具
   # 改完源码后：
   toolbox-hub check registry/packages/my-tool
   ```

3. 重新构建，再校验整仓：

   ```bash
   toolbox-hub build registry/
   toolbox-hub check registry/
   ```

4. **把 `registry/index.json` 与 `registry/artifacts/` 跟源码一起提交**：

   ```bash
   git add registry/packages/my-tool registry/index.json registry/artifacts
   git commit -m "registry: 新增 my-tool"
   git push origin add-my-tool
   ```

5. 对这个分支开 **PR 到 `emoeem/toolbox-hub` 的 `main`**。

> **索引是构建产物，但必须一起提交。**
>
> 官方源的地址就是仓库 `main` 分支上的
> `https://raw.githubusercontent.com/emoeem/toolbox-hub/main/registry/index.json` ——
> 别人 `toolbox-hub repo add` 之后，客户端 GET 的就是**提交进仓库的那一份**。
> 只提交源码、不提交 `index.json` / `artifacts/`，线上仓库和源码就对不上：
> 别人搜不到你的包，或者装到的还是旧版本。
>
> 反过来，也**不要手改** `index.json`：版本、哈希、文件清单全是 `build` 算出来的，
> 手改的那一份下次构建就会被打回原形。

CI 里那一步「仓库里的索引与产物必须就是源码构建出来的那一份」如果红了，
把它当成一句提示就好：在本地跑一次 `toolbox-hub build registry/`，把
`index.json` 与 `artifacts/` 一起提交。

## 或者：不改本仓库，自己托管一个仓库

插件不一定要进官方仓库。`build` 出来的目录本身就是一份可以发布的静态站点：

```text
你的仓库/
├── index.json          ← build 生成
├── artifacts/*.tar.gz  ← build 生成
└── packages/*/         ← 你维护的源码
```

推到 **GitHub Pages**、任意静态 HTTP、对象存储，甚至一个本地目录都行，然后：

```bash
toolbox-hub repo add https://<你>.github.io/<仓库>/index.json --name 我的工具
toolbox-hub search 关键词
toolbox-hub install my-tool
toolbox-hub update          # 你发新版本后，用户这样升
```

这条路**不需要任何人评审**，你什么时候发新版本由你自己决定。要注意的是：

* 产物要跟索引一起发布，路径按 `index.json` 里 `artifact.url` 摆（默认是相对路径，
  所以整个目录搬到哪都行）；
* 走 HTTP 的仓库必须有正确的 `artifact.sha256` —— `build` 总会写，别手改索引，
  远程产物没有哈希客户端默认拒绝安装。

细节见 [registry/README.md](registry/README.md) 与 [docs/repository.md](docs/repository.md)。

## 评审会看什么

主要是**别人装上去会不会被坑**，不是你的代码写得漂不漂亮。按重要性排：

1. **安全边界**。不偷偷 `sudo`、不提权、不往系统目录写东西。官方仓库里的包
   `requires_root` 一律是 `false`：客户端不会替你提权，需要 root 的事要么让用户
   自己在终端里做，要么换个不需要 root 的做法。
2. **不联网下载不校验的东西**。绝大多数插件根本不需要联网。如果你的脚本要从网上
   取文件，必须校验哈希并说清楚来源 —— 「下载就执行」不会被接受。
3. **破坏性动作标出来**。会删东西、会改状态的动作写 `danger = "caution"`（执行前
   工具箱会再确认一次），并且**默认行为是「什么都不做 / 只预览」**，把「真的删」做成
   一个显式开关（`registry/packages/docker-cleanup/` 就是这么写的）。
4. **依赖要声明**。动作里的 `program` 会自动记成依赖；脚本里自己调用的命令，
   在 `[[dependencies]]` 里补上（写在 `toolbox.toml` 或 `manifests/*.toml` 都收，
   `command` + 可选 `hint`）。漏了的话，用户那边会显示「就绪」，然后一跑就 command not found。
5. **脚本有 `-h`，退出码有意义**。结果走 stdout、诊断走 stderr；「参数不合法」
   用非 0 退出码（骨架里用的是 2）。
6. **没有占位**。`TODO`、「改掉这句话」、空 `summary`、写了但没动作的目录，
   评审会直接打回。
7. **一个动作只做一件事**，字段的 `help` 写成人话 —— 界面里那张表单就是你的产品。

## 会被直接拒绝的做法

| 做法 | 为什么 |
| --- | --- |
| 往 `~/.local/bin` 和包自己的目录以外写文件 | 安装落点只有这两个地方（`bin` 进 `~/.local/bin`，`manifest` / `doc` / `data` 进包目录）。脚本别自己去写 `/etc`、`$HOME` 根目录这些地方 |
| 要求 root 却不声明 `requires_root` | 反过来也一样：不声明就不能假定自己有 root。官方仓库里 `requires_root` 一律 `false` |
| 包里放符号链接 | `check` 直接报错（错误，不是警告）。这是最经典的越界写文件手法 |
| 路径里带 `..`、绝对路径、`~` | `check` 直接报错 |
| 把别人的包原样抄来 | 版权和信任问题。要包装别人的命令就自己写，并说明差异 |
| 包装一个**已经存在**的命令却不说明差异 | 评审看不出它和系统里那个命令有什么不同，就会拒（例如再包一个 `ls` 得说清楚多了什么） |

## 版本与更新

* 用语义化版本：`1.0.0`，补丁 `1.0.1`；`1.0.0-1` 这种带 pkgrel 的也认。
* **改了任何文件内容就升版本**。产物哈希会变，而用户侧的 `list` / `update` 是按版本号
  工作的 —— 版本不动，别人 `toolbox-hub update` 就看不到你的新版本。
* 因为你提交了新的产物，旧版本的产物索引不再引用它；留着能过 `check`，
  顺手 `git rm registry/artifacts/<旧文件名>` 也可以。
* 用户侧两条命令别搞混：`toolbox-hub repo update` 是刷新索引，
  `toolbox-hub update` 才是把已装的包升级到最新。
* 官方索引默认**不带** `updated` 字段，别用 `toolbox-hub build registry/ --stamp`
  生成要提交的索引 —— 那会把时间戳写进去，可复现就没了。

## 提交前自测清单

- [ ] `toolbox-hub check registry/packages/<id>/` → **0 错误**（警告也尽量清零）
- [ ] `toolbox-hub build registry/` 跑两遍，`sha256sum registry/artifacts/*` 两次结果一样
- [ ] `toolbox-hub check registry/` → 0 错误
- [ ] 真的装了一遍：`repo add` → `install` → `run`；顺手 `my-tool -h` 看得到用法
- [ ] `toolbox-hub uninstall <id>` 卸得掉（你改过的文件它会**留着并报告**，这是故意的）
- [ ] `git status` 里没有漏掉的 `index.json` / `artifacts/` 改动

## 命令速查

| 命令 | 干什么 |
| --- | --- |
| `toolbox-hub new <包名> [--kind script\|recipe] [--dir 目录]` | 生成骨架（`--description` / `--domain` / `--program` 可选） |
| `toolbox-hub check [目录]` | 校验包目录或仓库根；有错**返回 1**，没错 0 |
| `toolbox-hub build [目录] [--stamp]` | 打包 + 重建 `index.json`；可复现，默认不写 `updated` |
| `toolbox-hub repo add <地址>` | 加一个仓库（URL / 本地路径 / `file://…`） |
| `toolbox-hub install <包名>` | 装（先核对哈希） |
| `toolbox-hub run <包名> --字段 值` | 按动作跑一次 |
| `toolbox-hub uninstall <包名>` | 卸（改过的文件保留并报告） |
| `toolbox-hub update` | 把已装的包升到最新 |

## 更细的文档

* 字段参考、目录约定、动态参数、校验规则：[docs/plugin-authoring.md](docs/plugin-authoring.md)
* 索引 schema、安全模型、离线策略：[docs/repository.md](docs/repository.md)
* 官方仓库怎么组织、怎么本地加进来：[registry/README.md](registry/README.md)
