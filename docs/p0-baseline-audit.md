# P0 基线审计与架构冻结报告

> 依据：`Toolbox Hub 2.0：项目优化与 MCP 能力建设计划书`
> 仓库：`emoeem/toolbox-hub`（本地 `/home/emo/code/toolbox-hub`）
> 审计日期：2026-11-06
> 基线提交：`c45c4ca`（2026-10-29，分支 `main`，工作区干净）
> 本批次性质：**只读审计 + 文档**。未修改任何源码、脚本、配置，未执行 commit / push。

---

## 0. 结论摘要

1. **基线健康**：格式、Clippy、两套 feature 的测试、MSRV 1.90 构建、Registry / Recipe 自检全部通过；测试基线为 **567 passed / 16 ignored**（默认 feature）与 **561 passed / 16 ignored**（`--no-default-features`）。
2. **不需要推倒重来**：工具发现、参数到 argv 的构造、进程组取消、输出捕获、历史脱敏（argv 部分）都已存在且质量不错，MCP 所需底座约完成 60%~70%。
3. **三个必须补的架构缺口**：
   - 执行入口分散在 TUI 与 `ecosystem` 两套代码里，没有统一的 `ToolExecutor`；
   - `ArgumentValues` 是 `BTreeMap<String, String>`，多值靠分隔符拆分，无法无损承载 MCP 的数组输入；
   - `Danger` 只服务 TUI 二次确认，不等价于「只读 / 写文件 / 联网 / 提权」，不能直接映射为 MCP 权限。
4. **两个已确认的安全缺陷**（不是理论风险）：
   - 执行历史只对 `argv` 脱敏，**`values` 原样落盘**（`src/app/mod.rs:2772`、`:2800`），`sensitive = true` 的参数值会泄漏；
   - Recipe 参数是纯字符串替换进 argv，没有 flag/位置语义（`src/ecosystem.rs:1173`），存在**参数注入**（值以 `--` 开头会被当选项），接入 MCP 前必须补约束。
5. **与计划书有 3 处偏差**，见 §1.3；其中 MCP 协议版本与 RMCP API 必须在 P3 开工前重新核实。

---

## 1. 基线验证

### 1.1 环境

| 项目 | 实测值 |
| --- | --- |
| 工具链 | `rustc 1.99.1` / `cargo 1.99.1` |
| MSRV | `1.90.0`（rustup 中确实存在，已用于验证） |
| Cargo feature | 默认 `pacman`；`--no-default-features` 可用 |
| 平台 | Linux（Arch 优先，`libalpm` 动态链接） |

### 1.2 检查结果

| 命令 | 结果 | 备注 |
| --- | --- | --- |
| `git status --short --branch` | `## main...origin/main`，无改动 | HEAD `c45c4ca` |
| `cargo fmt --check` | 通过 | |
| `cargo clippy --all-targets --all-features -- -D warnings` | 通过 | 34.5s，无警告 |
| `cargo test --all-features` | **567 passed / 16 ignored** | 与计划书引用的历史数字一致 |
| `cargo test --locked --no-default-features` | **561 passed / 16 ignored** | 少 6 条，全部是 `pacman` 相关用例 |
| `cargo +1.90.0 build --locked` | 通过 | 22.0s |
| `cargo run --quiet -- check registry/` | 通过 | 9 个插件包、索引 / 哈希 / 载荷一致 |
| `cargo run --quiet -- recipe check registry/featured` | 通过 | 35 条 Recipe |
| `python3 registry/build-featured.py --check` | 通过 | 35 条、SHA-256 全部一致 |

**结论：可以进入实现阶段。基线数字以本表为准，不再引用计划书中的历史记录。**

### 1.3 与计划书的三处偏差（需要在后续批次修订计划）

1. **Provider 名称**：计划书 §2.8 / §6 多次提到 `Curated` Provider。实际代码里没有这个类型；对应的能力由 `manifest` Provider 承担（`src/providers/manifest.rs:36`），`Registry` 实际装配的是 `fftools` / `scripted` / `manifest` / `repository` 四个（`src/providers/mod.rs:82`）。
2. **`check` 命令语义**：`toolbox-hub check registry/` 校验的是**插件包仓库**（`toolbox.toml` + 载荷 + 索引），**不校验**精选 Recipe。Recipe 要单独跑 `recipe check` 与 `registry/build-featured.py --check`。P0 检查清单应把这两类分开列。
3. **MCP 版本**：计划书写 MCP `2026-07-28` 与 `rmcp` 但未锁版本。P3 开工前必须重新确认规范版本、`rmcp` 稳定版本、其 MSRV 与 feature 组合；`rmcp` 会引入 Tokio，需先做 ADR（P0-08）。

---

## 2. 现有架构实况

### 2.1 启动入口（`src/main.rs`）

```text
main()
├── 恢复 SIGPIPE 默认处置、安装 panic 钩子                    :62-67
├── 首参 ∈ {recipe, import, preset, queue, history}
│      → ecosystem::run_cli()，不进 TUI                        :70-82
├── 首参 == widget → widget::run_cli()                         :86-88
├── cli::parse() → Invocation
│      ├── Command(options) → cli::run()，不进 TUI             :108-115
│      └── Tui { bin_dir, dirs } → 配置目录、Registry::discover :99-119
└── App::new → 终端初始化 → 事件循环                            :127-137
```

**要点**：`mcp` 应当作为与 `recipe` / `queue` 平级的非 TUI 分支，在 `main.rs:70` 那个分支里加入；不得进入 `setup_terminal()`。

### 2.2 模块规模与职责

| 模块 | 行数 | 现有职责 | MCP 视角 |
| --- | ---: | --- | --- |
| `src/app/mod.rs` | 4913 | 状态机、表单、任务队列、历史、包中心状态 | 需拆分；执行逻辑必须外移 |
| `src/ecosystem.rs` | 2123 | Recipe / 队列 / 历史 / 预设 / 导入 CLI | 独立执行通道，首期不复用给 MCP |
| `src/cli.rs` | 883 | 包管理 CLI 与参数解析 | 无工具执行 JSON 接口 |
| `src/runtime/exec.rs` | 1298 | 子进程、捕获、超时、进程组、终端交接 | 需拆出无终端路径 |
| `src/providers/manifest.rs` | 1816 | TOML 动作解析与校验 | MCP Schema 的事实来源 |
| `src/registry/mod.rs` | 583 | 聚合、去重、排序、筛选 | ToolCatalog 的底座 |
| `src/ui/*` | 17 个文件 | 纯渲染 | 13 个文件没有 `mod tests` |

### 2.3 现有执行链路（4 条，互不统一）

| # | 入口 | 代码 | 执行函数 | 终端 | 输出 |
| --- | --- | --- | --- | --- | --- |
| 1 | TUI 捕获队列 | `src/app/mod.rs:1946` / `:2011` / `:2016` | `spawn_captured` | 不占用 | 实时事件 + 最终 `Captured` |
| 2 | TUI 交互单动作 | `src/app/input.rs:868` | `execute_action` → `run_batch` | 交接终端 | 直接继承 stdout |
| 3 | TUI 交互多选 | `src/app/input.rs:1501` | `execute_tools` | 交接终端 | 直接继承 stdout |
| 4 | TUI 提权 / 安装 | `src/app/mod.rs:1623` | `run_in_terminal` | 交接终端 | 直接继承 stdout |
| 5 | CLI Recipe / 队列 | `src/ecosystem.rs:795` / `:1187` | 自己的 `Command::new` + 进程组 / 危险词拦截 | 非 TUI | 丢弃或直接继承 |
| 6 | CLI 包管理 | `src/cli.rs:406` | `packages::*` + `escalate` | 可能交互 | 人读文本 |

另外 `runtime::run_captured`（同步阻塞）仍被 `src/packages/worker.rs:435`、`:461` 使用，不是死代码。

**结论**：`foreach` 展开逻辑也只存在于 `App::form_build_runs`（`src/app/mod.rs:2434`），MCP 若要支持批量，必须先把它下沉到服务层，否则会写出第二套语义。

### 2.4 Provider 与 Registry 契约

| Provider | 来源 | ID 规则 | 关键事实 |
| --- | --- | --- | --- |
| `fftools` | `bin_dir` 下 `fzf-*` | `fftools:<name>` | 头部注解驱动 |
| `scripted` | `bin_dir`、`~/bin`、配置 `tools/`、`/usr/local/bin`（可被 `TOOLBOX_HUB_PATH` 覆盖） | `scripted:<name>` | **会把 `/usr/local/bin` 全扫一遍**，不能直接全量暴露给 AI |
| `manifest` | 用户 `tools.d/*.toml` + 13 份内置 Manifest | `manifest:<action id>` | 默认 `mode = "capture"`（`manifest.rs:457-459`） |
| `repository` | 已安装的 ToolHub 插件包（账本驱动） | `repository:<package>:<action>` | 当前索引 9 个包 |

现有 `Provider` trait 只有 `id()` / `label()` / `tag_order()` / `discover()`（`src/providers/mod.rs:51-69`），没有计划书 §7.3 要求的 `metadata()` / `validate()` / `source_identity()`。

`Registry` 的两个细节会直接影响 MCP：

- **去重键是 `(canonical path, name)`，不是 `id`**（`src/registry/mod.rs:101-115`）。跨 Provider 的 ID 冲突目前不会被检测。
- **排序键是 `domain → pin → name`**（`src/registry/mod.rs:124-131`），没有 `id` 作为最终 tie-breaker。`scripted` 内部对目录项做了排序，但 `manifest` 的多目录合并依赖注册顺序；同名工具在极端情况下顺序不稳定，与计划书 §P4-04「相同目录内容产生相同排序」有差距。

---

## 3. 工具目录盘点（P0-01）

### 3.1 静态可数部分

| 来源 | 数量 | 明细 |
| --- | ---: | --- |
| 内置 Manifest | **59 个动作 / 13 份文件** | 见下表 |
| 精选 Recipe | 35 条 | 33 条 `safe`、2 条 `caution`（`archive-create`、`http-download`） |
| 插件包仓库 | 9 个包 | `demo-actions`、`disk-report`、`docker-cleanup`、`ffmpeg-extra-recipes`、`git-worktree`、`hello-tool`、`image-batch`、`net-diagnose`、`system-cleanup` |
| 随仓库脚本 | 8 个 | `scripts/tbx.sh` + 冻结的 `scripts/sing-box/*`（6 个 + `install.sh`） |

| Manifest | 动作数 | 主要域 | 首期 MCP 可用性 |
| --- | ---: | --- | --- |
| `pkgbuild-source.toml` | 20 | 打包 | 多为 `capture`，但会改动仓库状态，需逐项审查 |
| `ffmpeg.toml` | 9 | 媒体 | 只读 `ffmpeg-probe` 可先用；其余写文件 |
| `packages.toml` | 7 | 包管理 | 3 个只读 `capture` 可评估；4 个 `native` 不暴露 |
| `sing-box.toml` | 6 | 网络 | **冻结**，一律不暴露 |
| `system.toml` | 6 | 系统 | 只读动作可作为首批候选 |
| `7zip.toml` / `exiftool.toml` / `yt-dlp.toml` | 2 / 2 / 2 | 工具 / 图像 / 媒体 | 写文件 |
| `aria2c` / `discover` / `imagemagick` / `jq` / `pandoc` | 1 各 | 混合 | `jq`、`imagemagick` 可评估 |

**运行时实际发现数量当前无法在不进 TUI 的情况下取得**（没有 `tools list` 命令）——这本身就是 P5-04 CLI JSON 接口的价值证明，也是 P1 验收时需要的观测手段。

### 3.2 首批 MCP 候选（建议，不是授权）

满足「`capture` + 明确只读 + 依赖为常见 coreutils」的候选：

- `manifest:ffmpeg-probe`（ffprobe 只读）
- `manifest:pkg-search`、`manifest:pkg-owner`、`manifest:pkg-files`（pacman 只读查询，需确认不带 `-S`/`-R`）
- `system.toml` 中的只读查询动作（需逐条核对 `base_argv`）
- 精选 Recipe 中的只读项（**P5 才接入**，首期不走 Recipe）

---

## 4. 执行模式矩阵（P0-04）

| 模式 | 默认产生者 | 是否依赖终端 | 现有无终端路径 | MCP 1.0 |
| --- | --- | --- | --- | --- |
| `Capture` | 所有 `manifest` 动作（未显式声明时） | 否 | `spawn_captured`（后台线程） | **支持** |
| `Interactive` | 脚本注解缺省、`aria2c`、包管理 | 是（`suspend_terminal`） | 无 | **不暴露** |
| `Native` | `discover`、`packages` 的 4 个入口 | 是（TUI 内部界面） | 无 | **不暴露** |

风险点：`RunMode::default()` 是 `Interactive`（`src/model/tool.rs:6-11`），这是正确的安全默认；但 `scripted` / `fftools` 的脚本只要写 `mode=capture` 就能进入可暴露集合，**注解是自述，不是审计**。

---

## 5. `Danger` 与真实副作用的差距（P0-05）

现有 `Danger` 只有 `Safe` / `Caution`（`src/model/tool.rs:36-44`），语义是「执行前要不要再确认一次」，不是副作用分类。实证：

| 动作 | 现有 Danger | 真实副作用 | 结论 |
| --- | --- | --- | --- |
| `manifest:ffmpeg-compress` | `Safe`（未声明） | 写输出文件、长时间 CPU | 不能映射为只读 |
| `manifest:exif-strip` | `Safe`（未声明） | 写新文件（刻意不原地改） | 仍属写文件 |
| `manifest:aria2c-download` | `Safe` | 联网 + 写文件 | 不能映射为只读 |
| `manifest:7z-extract` | `Caution` | 可能覆盖同名文件 | 正确但要独立建模 |
| `manifest:pkg-center` | `Safe` + `Native` | 界面内可安装 / 卸载（`sudo`） | **`Safe` 严重失真** |
| `sing-box-*` 脚本 | 多数 `Caution` / 未声明 | `sudo`、改系统配置、重启服务 | 冻结，不暴露 |

`packages.toml:13-14` 的注释明确写着「会改系统的动作统一标 `danger = "caution"`……需要 root 的动作把 `program` 写成 `sudo`」，但同一文件里 `pkg-center*` 仍标 `safe` —— 说明 `Danger` 描述的是「界面确认」，不是「实际权限」。

**结论**：P2-01 的 `ToolEffects` 必须独立于 `Danger` 建模，且 `readOnlyHint` 只能由 `ToolEffects` 推导，不能由 `Danger` 推导。

---

## 6. 安全审计（P0-05）

### 6.1 做对的部分（可以复用）

- **全链路 argv，不拼 shell**：`Action::build_argv`（`src/model/argument.rs:254`）只做字符串拼接，用户值始终是单个 argv 元素；有专门测试覆盖 `;`、`|`、`$()`、反引号、中文空格路径（同文件 `:553-584`）。
- **动态候选不经过 shell**：`crate::dynamic::split_command` 按空白/引号切分。
- **进程组取消**：`spawn_captured` 以 `process_group(0)` 启动（`src/runtime/exec.rs:551-555`），取消发 `-pid`，`Drop` 兜底（`:626-637`），并有孙子进程测试。
- **输出上限**：单流最多 4000 行，丢弃时在开头标注（`src/runtime/exec.rs:645`、`:703-710`）。
- **argv 脱敏**：`Action::redacted` 按值精确替换为 `***`（`src/model/argument.rs:337`）。

### 6.2 已确认缺陷

| 编号 | 缺陷 | 证据 | 影响 | 归属 |
| --- | --- | --- | --- | --- |
| S-01 | 历史 `values` 未脱敏 | `src/app/mod.rs:2772-2776` 取原始表单值；`:2800` 原样写入；`history.rs:11` 的注释只承诺 argv | `sensitive = true` 的密码 / token 会以明文留在历史文件 | **P2-08 必修** |
| S-02 | Recipe 参数无 flag/位置约束 | `src/ecosystem.rs:1173-1190` 纯 `replace` 后进 argv | 参数注入：`path="--help"` 会变成选项；不是 shell 注入，但能改变命令语义 | **P5 前必修** |
| S-03 | 队列危险拦截是词表启发式 | `src/ecosystem.rs:780-792` `dangerous()` | 只能降低误操作，不能作为授权机制 | MCP 不得复用为策略 |
| S-04 | 注解可自述 `danger=safe` | `src/providers/metadata.rs:101-105` | 脚本作者可以低报风险 | P2 需独立 `mcp.toml` 白名单 |
| S-05 | 无 ID 冲突检测 | `src/registry/mod.rs:101-115` | 新增 Provider / 包更新可能改变已授权工具身份 | P2/P3 需在注册时校验 |

### 6.3 提权路径清单

| 路径 | 位置 | MCP 首期策略 |
| --- | --- | --- |
| `packages::escalate` 自动加 `sudo` | `src/packages/mod.rs:615-620` | 不暴露 `packages` 域 |
| Manifest 中 `program = "sudo"` 的动作 | `manifests/packages.toml` 注释与动作 | 不暴露 |
| `scripts/sing-box/*` 内部 `sudo` / `systemctl` | `scripts/sing-box/` | **冻结**，不暴露 |
| `pkgbuild-source` 系列（`makepkg` / git 操作） | `manifests/pkgbuild-source.toml`（20 个动作） | 首期不暴露，后续逐项审查 |

---

## 7. MCP 就绪度差距（P0-02 / P0-03 / P0-06）

| 计划要求 | 现状 | 差距 |
| --- | --- | --- |
| `ToolCatalog` | `Registry::tools()` / `tool_count_in()` / `tags_for()` | 缺 `get_tool(id)` / `search` / 稳定排序的显式契约；Registry 与 Provider 生命周期绑定 |
| 类型化参数 | `ArgumentValues` 是 `BTreeMap<String, String>`（`argument.rs:175`） | 多值只能靠 `separator` 拆，数组内容含分隔符时失真 |
| 统一执行服务 | TUI 队列 + `execute_*` + `ecosystem` 三套 | 无 `ExecutionRequest/Result`，无超时参数 |
| Headless Runtime | `spawn_captured` 可用 | `exec.rs` 直接 `use ratatui::crossterm`（`:16-20`）；超时只在 `wait_with_timeout`（用于探测），未用于执行 |
| 结构化结果 | `Captured`（label/command/stdout/stderr/status/success/elapsed/cancelled） | 无 `truncated` 字段（截断只是文本前缀）、无 `timed_out`、无 `tool_id` |
| Schema 生成 | 无 | 需要 Manifest → JSON Schema 映射（含 `repeatable` / `foreach`） |
| MCP Server | 无 `src/mcp`，Cargo 无 `rmcp` / `tokio` | P3 |
| 策略配置 | `config.rs` 只有 `state.toml` / `packages.toml` / `tools.d/` / `tools/` | 无 `mcp.toml` |
| 审计 | 有 `history`（500 条上限） | 无独立审计流；无授权结果；`values` 泄漏（S-01） |
| CLI JSON | `cli.rs` 无 `tools` 动词 | P5 |

---

## 8. 计划书修订建议

1. §2.8 / §6 的 `Curated` 改为 `Manifest`；把「Recipe 接入」明确标注为**独立执行通道**，不要假设它已经在 Registry 里。
2. §9 的检查清单拆成三条：`check registry/`（插件包）、`recipe check`（精选 Recipe）、`build-featured.py --check`（索引与哈希）。
3. §P0-08 的 ADR 必须同时记录：`rmcp` 版本、其 MSRV、启用的 feature、Tokio 版本、以及**是否接受把 Rust 版本下限抬高**。
4. §P2-08 增加一条明确的验收：「历史文件里不得出现 `sensitive` 参数的原值」，当前代码不满足。
5. §P3-02 补充「MCP 工具名冲突检测」：现有 Registry 不做 ID 唯一性校验，命名映射必须自己去重并保持稳定。
6. §P4-04 的「相同目录内容产生相同排序」需要给 `Registry` 的排序补 `id` tie-breaker。

---

## 9. P1 批次实施方案（下一批）

### 9.1 允许修改的文件（白名单）

新增：

```text
src/service/mod.rs
src/service/catalog.rs
src/service/argument.rs
src/service/execution.rs
src/service/result.rs
src/service/error.rs
```

修改（最小侵入）：

```text
src/main.rs            // 仅新增 `mod service;`
src/model/argument.rs  // 新增 ArgumentValue / ArgumentInputValues，保留 ArgumentValues 兼容层
src/model/mod.rs       // 导出新类型
src/runtime/exec.rs    // 抽出「可选超时 + 结构化捕获」的无终端函数，不改动现有终端路径
```

禁止触碰：

```text
scripts/sing-box/**        // 冻结
src/ui/**                  // P1 不改渲染
src/app/**                 // 除编译必要的类型适配外不改行为
src/ecosystem.rs           // Recipe / 队列 不在 P1 范围
```

### 9.2 P1 任务

1. `ToolCatalog`：从 `Registry` 构建只读索引；`list / get / search / by_domain`；按 `id` 最终排序。
2. 类型化参数：`ArgumentValue::{String, Boolean, Strings}`；`ArgumentInputValues` 支持数组无损输入；`build_argv` 接受统一输入。
3. 兼容层：`ArgumentValues`（字符串表单值）→ `ArgumentInputValues` 的转换，保证 TUI 现有行为不变。
4. `ToolExecutor`：`execute(request) -> Result<ExecutionResult, ExecutionError>`；内部复用 `spawn_captured` 的进程组 / 取消 / 输出上限；支持 `timeout`；返回 `truncated` / `timed_out` / `exit_code` / `elapsed_ms`。
5. 一致性测试：同一组参数分别走 `App::form_build` 与 `ToolExecutor`，argv 必须逐元素相等；`foreach` 展开结果一致。
6. 回归：`cargo fmt --check`、Clippy、两套 feature 测试、Recipe / Registry 自检全绿。

### 9.3 P1 验收

- 现有 TUI 行为、默认值、argv 顺序、foreach、历史脱敏（argv）不退化。
- 新服务不依赖 Ratatui，不写父进程 stdout。
- 超时 / 取消 / 输出截断都有测试。
- 基线 567 / 561 不减少。

---

## 10. P0 验收自检

| 计划书 P0 要求 | 状态 | 证据 / 备注 |
| --- | --- | --- |
| P0-01 工具目录清单 | 完成（静态） | §3；运行时数量需 P5 CLI |
| P0-02 模型生命周期 | 完成 | §2.1–§2.3 |
| P0-03 执行入口梳理 | 完成 | §2.3，共 6 条 |
| P0-04 执行模式矩阵 | 完成 | §4 |
| P0-05 危险操作与提权路径 | 完成 | §5、§6.3，含 5 项缺陷 |
| P0-06 服务拆分边界 | 完成 | §7、§9 |
| P0-07 测试基线 | 完成 | §1.2 |
| P0-08 RMCP ADR | **未完成** | 需联网确认版本后再定；不阻塞 P1 |

**门禁决定**：P0 通过，可以进入 P1。P0-08 与 MCP 版本相关，留到 P3 前完成，避免用未核实的 API 提前设计。
