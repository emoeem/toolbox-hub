# 生态升级：设计依据与字段映射

这份文件留着**为什么这么设计**，而不是「做了什么」——做了什么看
[`../CHANGELOG-ECOSYSTEM.md`](../CHANGELOG-ECOSYSTEM.md)。上游资料、字段映射表、以及
shell 集成那套回填协议的数据流都在这里，改这些地方之前先读一遍。

## 一、Shell 集成的数据流

照着 navi 的 shell 插件那套学来的。关键点是**逐层都不碰用户的命令**：

```text
用户 shell rc 文件
  ├─ Bash/Zsh: eval "$(toolbox-hub widget bash|zsh)"
  └─ Fish:     toolbox-hub widget fish | source
          ↓ 生成并加载 shell 函数 / 原生按键绑定（Ctrl-G）
Ctrl-G（默认）
          ↓ shell widget 调用 toolbox-hub 的独立子进程
子进程的交互 TUI 使用 /dev/tty；stdout 仅承载最终命令
          ↓ 退出码 0=选择，1=取消，2=错误
Shell widget 捕获 stdout
          ↓
Bash: READLINE_LINE / READLINE_POINT
Zsh:  LBUFFER + zle reset-prompt
Fish: commandline -r + commandline -f repaint
          ↓
真实命令成为当前输入缓冲区，用户可编辑后再按 Enter
          ↓
Shell 自己执行并写入正常历史记录（不是 toolbox-hub 包装命令）
```

上游资料：

- <https://github.com/denisidoro/navi>
- <https://github.com/denisidoro/navi/blob/master/shell/navi.plugin.zsh>

**实现边界（已实现，改动别越过）**：

- 不能让主 TUI 的 stdout 混入界面内容 —— 选择器复用 `crate::components::Session`（只写 `/dev/tty`）。
- 默认只回填；直接执行是显式选项（`--run`），且即使执行也走 argv，不用 `sh -c`。
- 三种 shell 的生成脚本分别有用例；`bash -n` / `fish --no-execute` 进了 CI。
- PTY 集成测试不能靠普通管道冒充 —— 本轮的端到端验证用的是 `script -qec 'bash --norc -i'`。

**fish 的一个坑**：`bind \cg …` 只作用于默认 keymap。emacs 模式下默认 keymap 就是它，
但 vi 模式下按不出来，所以 insert 模式要再绑一条（同配置里的 atuin / fzf 也是两条都绑）。

## 二、外部速查格式 → Recipe 的字段映射

| 来源字段/语法 | toolbox-hub 目标 | 处理 | 说明 |
|---|---|---|---|
| navi `% tag` / 分类头 | recipe tags/domain | 适配 | 映射到现有域/标签；未知分类保留为标签 |
| navi `# 注释` | summary/说明 | 有损适配 | 注释可作说明，不能可靠区分标题与步骤 |
| navi 命令正文 | action.program + base argv / 参数 | 适配 | 仅在模板解析安全、语法明确时生成动作 |
| navi `<name>` 占位符 | dynamic/text 参数 | 适配 | 静态占位可生成 text 参数；有候选源时转 dynamic |
| navi `$ name: command` | dynamic 参数候选源 | **未实现** | 候选命令是外部代码；必须保留为显式执行的动态来源，绝不导入时运行 |
| navi `--column` / delimiter / 多列映射 | dynamic 元数据 | 部分支持 | 不匹配现有参数模型的列映射需警告并降级 |
| navi 多行/多命令脚本、shell 展开 | 单个 action | 放弃自动转换 | 不能安全地从字符串还原 shell 语义；保留原文件并报告 |
| cheat YAML front matter `syntax` | recipe metadata | 适配 | 记进说明；**不**因此允许 shell 拼接执行 |
| cheat YAML front matter `tags` | tags | 适配 | 合并进导入产物的标签 |
| cheat YAML `tags` | tags/domain | 适配 | tags 可保留；domain 不匹配时归入通用工具域 |
| cheat YAML front matter 整段 | 文件级元数据 | 适配 | `---` 围栏内**不产出条目**；只认 `syntax` / `tags` 两个键，不引 YAML 解析器 |
| cheat 正文注释与命令 | summary + action | 有损适配 | 多条独立示例默认拆分或只导入为文档 |
| cheat `<name>` | text 参数 | 适配 | 映射为明确的字段参数；不支持的默认值语法给警告 |
| cheat `$ name: command` 动态候选 | dynamic 参数 | **未实现** | 导入仅解析，不执行候选命令 |
| tldr 标题/说明/更多信息链接 | recipe metadata | 适配 | 标题、说明和上游 URL 可无损保存 |
| tldr `{{placeholder}}` | 参数字段 | 适配 | 纯占位符变成 text 参数（与 `<name>` 同等对待） |
| tldr 平台目录 | platform metadata / tags | 适配 | 只导入适用平台；保留 common/linux 等标记 |
| tldr 示例描述与多条命令 | 文档/多条 recipe | 适配 | 每条例子是独立用例；**默认仅展示**，不直接当可执行 manifest |
| 任一来源中的 shell 管道、重定向、命令替换 | 无直接 argv 映射 | 放弃自动执行转换 | 字符串拆分会改变语义且可能引入风险，一律「仅展示」 |

**授权**：tldr 是 CC BY 4.0、cheat 社区仓库多为 CC0 —— 来源与授权要随内容记录，
**授权不明的仓库内容不许打包进官方精选库**。这也是 `registry/featured/` 只放自己写的
Recipe、导入结果只落到用户本地目录的原因。

## 三、危险判定的形状

第一版是 14 个字符串 `contains`，几乎每个都能绕开（`rm -fr`、`dd of=`、`curl … | bash`…）。
现在的形状是：

```text
命令串
  ├─ 按空白与 | ; & 切词（分隔符本身保留）
  ├─ 按分隔符切成「段」，每段第一个词是程序名
  └─ 逐段判定
       ├─ 段内任意位置出现 sudo/doas            → 拦
       ├─ 非首段且段首是解释器（sh/bash/…）      → 拦
       ├─ basename ∈ 高危程序表 / mkfs* / swapon → 拦
       ├─ rm: 递归 && (force || 目标是 / 或 /*)   → 拦
       ├─ dd: of= 且目标不是 /dev/null            → 拦
       ├─ chmod/chown: 带 -r/-R                   → 拦
       └─ systemctl: poweroff/reboot/halt/rescue/emergency → 拦
```

「切段」而不是简单分词，是因为 `rm -rf x` 和 `echo rm -rf x` 完全是两回事：只有段首那个词
才决定跑的是什么程序。

**不要把它当沙箱**。它不展开变量、不解析别名、不知道通配符会匹配到什么、也不看脚本内部。
拦得住 / 拦不住各有测试清单，收紧了会先红。

## 三·补：取消任务为什么必须发整个进程组

任务是用户自己的命令，**常常是个脚本**，而脚本自己会拉起子进程（`&` 后台任务、ffmpeg 管道）。
只 `kill` 直接子进程的话，那些孙子进程会被 init 收养、继续跑、继续写文件 ——
用户看到的是「我取消了，但输出还在长」。

所以两处（交互式执行 [`crate::runtime::exec`]、后台队列 [`crate::ecosystem`]）都：

1. 启动时 `process_group(0)`，让任务自成一组（pgid = 自己的 pid）；
2. 取消时 `kill(-pgid, SIGTERM)` —— 负号表示「整组」，先请它自己收尾；
3. 宽限期（队列是 1 秒）之后还没走，`kill(-pgid, SIGKILL)`，再用 `Child::kill` 兜底。

「怎么叫停一个进程组」只该有一处实现：`signal_group` 在 `runtime::exec` 里，
队列是 `pub(crate)` 复用它，不是另抄一份。

**副作用要一起处理**：自成进程组之后，终端的 Ctrl-C 就不会再传给任务了（Ctrl-C 只发前台
进程组，而任务已经在自己的组里）。结果是按 Ctrl-C 只杀掉 `toolbox-hub`，任务变成孤儿继续写
文件 —— 比不改还糟。所以队列自己接住 SIGINT：处理器只写一个原子变量（异步信号安全），
主循环带超时收结果、发现标志就转成「取消」，退出码按惯例 130。

## 四、libalpm 的 feature 化顺序

按依赖方向拆，别反过来（先拆最底层的 `libalpm.rs`，最后才是 UI）：

1. `Cargo.toml`：`alpm = { version = "5", optional = true }` + `pacman = ["dep:alpm"]`（默认开）。
2. `src/packages/libalpm.rs`：仅 `pacman` 编译。
3. `src/packages/libalpm_stub.rs`：禁用时提供**同一组类型边界**，但数据库入口明确返回
   「此构建未启用包管理」—— 不把「没有数据」伪装成「空数据」。
4. `src/packages/mod.rs`、`probe.rs`、`worker.rs`：按 feature 隔离直接调用点。
5. `src/ui/*`、`src/app/*`：只读分支与提示。
6. `src/cli.rs`：`-s/-i/-r/-u/-n/-l` 在禁用时给明确中文错误；通用 `ui` 与 repository verbs 保持可用。

注意：非 pacman 构建里 AUR 状态比较退化为字符串排序，**只是显示层降级**，不是 Arch 版本比较。

## 五、发布流程的参照

参照 **lazygit**（Go 写的终端 TUI，GitHub Actions Release 流程成熟）：tag 触发、跨架构矩阵、
归档 + 校验和 + GitHub Release + AUR 更新这几步直接借鉴；navi 作为 Rust CLI/widget 项目的
参照（Widget 与 Cargo 安装方式）；k9s 只用来对照多平台打包，不复制它的复杂度。

与 lazygit 的差异（本项目特有）：

- 二进制**动态链接 libalpm**，所以发行产物是 `--no-default-features` 的**核心版**；
  Arch 用户走 AUR / PKGBUILD，那条路径才带包管理。
- 优先在**原生 runner** 上构建（`ubuntu-24.04-arm`），避免 aarch64 交叉链接系统库不匹配。
- 产物必须有 **minisign 签名**，没有密钥就拒绝发布（见 `packaging/release-signing.md`）。

## 六、本轮明确放弃的方案

| 方案 | 为什么不做 |
|---|---|
| 从字符串还原 shell 语义（把 `cat f \| grep x` 拆成两次调用） | 拆出来的东西不再是用户写的那条命令，语义会变 |
| 把 tldr/cheat 内容直接当可执行 Recipe | 它们是「带上下文的示例」，不是参数化模板 |
| 用 `sh -c` 执行回填结果 | 引号一拼错，命令含义就变了；argv 是唯一可控的路径 |
| 缩略图缓存用「路径」做键 | 同名文件换内容后画面不更新，是缩略图缓存最经典的 bug |
| 危险判定写成「更长的关键字列表」 | 长度不是能力，`rm -fr` 这种顺序变体靠列表永远补不完 |
| 给 featured Recipe 加一键执行入口 | 通用的「任意 Recipe 一键跑」需要先解决审计与确认流；本轮只做到 `recipe run` + 三层授权 |
