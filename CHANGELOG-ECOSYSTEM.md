# 生态升级变更记录（Recipe 生态 / Shell 集成 / 队列与安全 / 发布）

这一轮把「一个 TUI 工具箱」补成了「一个可以被脚本和别人复用的工具箱生态」。下面按**用户能感知到的变化**写，
每条都附**实际跑过的验证**；没验证的一律写在最后一节，不混在「已完成」里。

验证基线：`cargo fmt --check`、`cargo clippy --all-targets --all-features -- -D warnings`、
`cargo clippy --all-targets --no-default-features -- -D warnings` 全部退出 0；
`cargo test` 默认 feature **559 passed / 0 failed / 16 ignored**，`--no-default-features` **553 / 0 / 16**。

---

## 1. Featured Recipe 精选库（35 条，带 SHA-256 索引）

- 新增 `registry/featured/`：35 条通用 Recipe JSON（`id/title/description/command/params/args/safety/source/tags`）。
- 新增 `registry/featured/index.json`：每条 Recipe 的 SHA-256，和包索引是**分开的完整性与 schema 域**。
- 根 `registry/index.json` 只多一个**相对指针** `featured: { index, schema_version }`，老客户端读它时
  `serde(flatten)` 收进 `extra`，不会报错（客户端 `Index` 没有 `deny_unknown_fields`）。
- 新增 `registry/build-featured.py`：`--build` 生成索引、`--check` 校验数量（≥30）、字段、`safety` 合法、
  模板参数已声明、SHA-256 一致。
- 新增 CLI：`recipe list` / `recipe check` / `recipe run`。
- `registry/build.sh`、`ci.yml`、`registry.yml` 接上这两条校验。

**验证**：`python3 registry/build-featured.py --check` → `35 recipes; SHA-256 verified`；
`toolbox-hub recipe check registry/featured` → `featured 校验通过：35 条`；
`toolbox-hub build registry/` 重建后**只有 `index.json` 多了 featured 指针**，
`registry/artifacts/` 逐字节未变 —— 可复现构建这条特性没有被这次改动破坏。

## 2. 外部速查导入（navi / cheat / tldr）

`toolbox-hub import <navi|cheat|tldr> <文件> [--dry-run] [--out 目录]`。

每一条来源行都有明确归宿，而不是「能用就用」：

| 处置 | 什么情况 | 例子 |
| --- | --- | --- |
| **可导入** | 能安全拆成「程序 + argv 模板」 | `ls -la <path>` → `ls` + `["-la", "{path}"]` |
| **仅展示** | shell 语义拆不掉（管道/重定向/命令替换），或引号需要人工确认 | `cat f \| grep x`、`echo $(whoami)` |
| **跳过** | 命中危险命令闸门，或占位符没法安全映射 | `rm -rf /tmp/foo`、`sudo …` |

- **cheat 的 YAML front matter 当元数据**：`---` 包起来那段里的 `syntax:` 与 `tags:`
  会被记下来（syntax 进说明、tags 进标签），**不产出条目**。之前按行硬导的结果是真跑出来的：
  `syntax: bash` 和 `tags: [networking, dns]` 各变成一条「可导入」Recipe。
  没有闭合的 `---` 会整份跳过并报出来，而不是把剩下的 YAML 当命令。
- `--dry-run` 只报告不写文件；不带 `--dry-run` 才落到 `--out`（默认 `imported-recipes/`）。
- 导入产物一律 `safety = safe` 并走 `validate_recipe`，**不会**因为「导入的」就放宽标准。
- tldr 的每条示例默认**仅展示**：它是带上下文的速查，脱离上下文直接当可执行模板是错的。
- 候选命令（navi 的 `$ name: command`）不会被导入时执行。

**验证**：8 条导入相关单测 + 真实文件跑 `--dry-run`（见第 7 节记录）。

**真页面抓到的 bug**：tldr 页面是**两行一条**示例（说明一行，命令一行且包着反引号）：

```text
- Show the last N number of commits:
`git log -n {{number_of_commits}}`
```

最早只处理了单行 `- 说明: 命令` 的写法（用例也是照那个写的），拿**真的 `git-log` 页面**一试，
整页输出全是「跳过：空命令」—— 一条都取不出来。现在两种写法都认，反引号会剥掉
（它不剥掉就会被判成 shell 命令替换、整条降级）。`tldr_command_extraction_handles_both_layouts`
与 `a_real_tldr_page_yields_the_commands_from_the_backticked_lines` 把这份真实形状钉住了。

占位符名里带 `/`（如 `{{path/to/file}}`）**不自动改名**，只降级为「仅展示」并说明要人工改名 ——
参数名会跟着进模板与预设，改错了是静默的。

## 3. 并发任务队列（argv，不经 shell）

`toolbox-hub queue run tasks.json [--jobs N]`，任务文件是 JSON 数组：

```json
[{ "id": "ffmpeg-a", "program": "ffmpeg", "args": ["-i", "a.mp4", "-c", "copy", "a.mkv"] }]
```

- 执行只用 `Command::new(program).args(args)`，**永远不拼 shell 字符串**。
- **默认并发 1**：一次跑一个，要并行得显式 `--jobs N`。
- 取消走**整个进程组**：先 SIGTERM（给工具收尾的机会，比如 ffmpeg 把已写部分封成可播文件），
  1 秒宽限期后整组 SIGKILL。任务是脚本时它自己拉起的子进程也会一起收到 —— 只杀直接子进程
  会留下继续写文件的孤儿。用的是交互式执行层同一个 `signal_group`，没有第二份实现。
- **Ctrl-C 也被接住**：任务自成进程组之后，终端的 Ctrl-C 只会发给前台进程组，
  也就是说它杀得掉 `toolbox-hub` 却碰不到任务 —— 会让任务变孤儿继续跑。所以队列自己
  处理 SIGINT（处理器只写一个原子变量），转成取消，退出码按惯例给 `130`。
- 每个任务独立报告退出码与耗时；`127` = 起不来（程序不存在），`126` = 被危险闸门拦下。
- 一个任务失败不影响其他任务继续推进；整队退出码取「最坏结果」。
- 支持取消令牌（`CancellationToken`）：取消后不再起新任务，正在跑的会被 kill 并标记 `cancelled`。
- 每完成一个任务往 stderr 打一行进度。

**验证**：4 个任务并发跑（含 `false`、不存在的程序、`rm -rf <canary>`），
canary 目录里的文件**还在**（`rm -rf` 被拦，退出码 126），同队列的 `touch` 任务正常完成。

## 4. 参数预设与历史回放

- `preset save <名字> <RecipeID> [key=value …]` / `list` / `show <名字>` / `delete <名字>`，
  落在 `<配置目录>/presets/<名字>.toml`（**TOML**，和 `state.toml`、工具 manifest 一致；
  能直接打开改，改坏了报错也看得懂）。`show` 打的就是这份 TOML 文本。
- 碰到早期版本的 `.json` 预设会直说「这是旧格式，用 `preset save` 重存一次」，
  而不是报一句「文件不存在」让人以为是名字打错了。
- 预设名会拒绝 `/`、`\`、`..`、空名 —— 名字直接当文件名用，不校验就是路径穿越。
- 读到坏掉的预设文件会**跳过它**，而不是让整张列表消失。
- `history list` / `history replay <序号>` 记录 Recipe 执行（id、参数、退出码、时间）。
- Recipe 已失效时**降级而不是报错**：原样保留参数，提示「可手动选择替代动作」——历史里
  最有价值的是那串参数，不是那条 Recipe 还在不在。

**验证**：真跑一遍 `preset save`（文件确实落在预置目录里）→ `list` → `show` → `delete`；
`recipe run` 一条只读 Recipe 后 `history list` 有记录、`history replay 1` 回填参数。
另有落盘往返、覆盖写、坏文件跳过、路径穿越拒绝等 4 条单测。

## 5. 安全分级与危险命令闸门

三级：`safe`（直接跑）/ `caution`（要 `--confirm`）/ `destructive`（**默认拦截**）。

`destructive` 要三条同时满足才跑：`--confirm-destructive` + 非空 `--audit <说明>` +
环境变量 `TOOLBOX_HUB_CONFIRM_DESTRUCTIVE=1`；同时把 id/source/command/args 打到 stderr 留痕。
`validate_recipe` 会**拒绝**把已知危险命令标成 `safe` 或 `caution` —— 改字段绕不过去。

闸门本身这轮重写过。旧版是 14 个字符串 `contains`，漏洞很明显：

| 旧版漏掉 | 现在 |
| --- | --- |
| `rm -fr`、`rm -r -f`、`rm --recursive --force` | 按段切词 + 看短/长选项，全拦 |
| `dd of=/dev/sda`（旧版只看 `dd if=`） | 拦 `dd of=`，放行 `of=/dev/null` |
| `curl … \| bash`（旧版只认 `curl \| sh`） | 管道右侧是解释器就拦（`sh/bash/zsh/fish/dash/ksh/csh/python/python3`） |
| `mkfs.ext4`、`shred`、`sgdisk`、`mkswap`、`userdel`、`groupdel` | 都进了高危程序表 |
| `chmod -R` 之外的 `chown -R`、`/bin/rm -rf` | 看 basename，递归改权限一律拦 |
| `systemctl poweroff/reboot/halt` | 拦 |
| `env | sudo tee …` 这种非段首提权 | 任意位置出现 `sudo`/`doas` 就拦 |

**它拦不住什么（明写出来，别当成沙箱）**：`xargs rm -rf`、`find . -delete`、别名（`alias d='rm -rf'; d /`）、
脚本**内部**的动作、以及会在 shell 里展开的变量目标（`rm -rf $DIR` 不知道 `$DIR` 是什么）。
这份「拦不住的清单」和「拦得住的清单」在 `src/ecosystem.rs` 里各有一条测试钉着，
哪天真收紧了，测试会先红，提醒同步这份文档。

**验证**：19 条「应当拦下」+ 11 条「不该误拦」+ 5 条「拦不住」的用例全过；
官方 35 条 featured Recipe 逐条过闸门 + 校验（`every_shipped_featured_recipe_passes_the_guard_and_validation`）。

## 6. Shell 集成（`widget`）：把命令回填到输入行

```bash
eval "$(toolbox-hub widget bash)"     # zsh 换成 widget zsh；fish 用 | source
```

按 `Ctrl-G` 打开选择器 → 打字过滤（标题/id/命令行都参与匹配）→ `Enter` 选中 →
**命令变成你正在输入的这一行**，回车之前还能改。执行与写历史都由 shell 自己做，
所以历史里是 `uname -r`，不是 `toolbox-hub widget …`。

- 只在 `/dev/tty` 上画界面，**stdout 只放最终命令**（复用 `ui` 组件那套约定）。
- 退出码固定：`0` 选中 / `1` 取消 / `2` 参数错或没有终端。
- 参数逐个问；**回车留空保留 `<占位符>`**，回去手填 —— 空字符串会让命令「语法对、语义错」。
- 填进去的值按 shell 字面量引用（`'my file.txt'`、`'$HOME'`、`'*.mp4'`、`it's` → `'it'\''s'`）。
- `--run` 才直接执行（默认只回填），而且即便执行也走 argv；破坏性 Recipe 还额外要
  `TOOLBOX_HUB_CONFIRM_DESTRUCTIVE=1`。
- `widget render <id> k=v` 是非交互入口，脚本和测试都用它。

**验证**：真的在 PTY 里起了 `bash --norc -i`，`eval` 生成脚本，按 `Ctrl-G` → 输 `kernel` → `Enter`，
得到 `uname -r` 被回填并执行（输出 `7.2.9-2-cachyos-bore-lto`），`history` 里记的是 `uname -r`；
fish 同样跑通。PTY 抓包确认 stdout 只有 9 字节（`uname -r\n`），界面全在 `/dev/tty`。

**一个只有真 shell 才暴露的 bug**：fish 的绑定原本只写在默认 keymap 上，
vi 模式里按 `Ctrl-G` 是没反应的（emacs 模式下两种写法等价）。现在默认与 insert 两条都绑。

## 7. 视频缩略图缓存

选中 mp4/mkv 时先让 ffmpeg 抓第一帧（`-ss 1`，取不到退到 `0`），再走和图片同一条解码路径。
面板标题写明「视频缩略图 · …」——第一帧不是这个视频的样子。

- 缓存目录 `$XDG_CACHE_HOME/toolbox-hub/thumbs`（`TOOLBOX_HUB_CACHE` 可改），**随时可整删**。
- 缓存键 = **绝对路径 + 体积 + mtime**。只按路径做键就是那个经典 bug：同名文件换了内容，
  屏幕上还是旧画面，怎么刷新都不变。
- 最多留 512 张，超了从最旧的删；只认 `.png`，用户自己丢进缓存目录的文件不会被删。
- 先写临时文件再 `rename`：半张图不会被当成成品缓存下来。
- 8 秒上限，到点 kill —— 界面不会被一个坏文件拖住。少装 ffmpeg 只会显示「取不到缩略图：没找到 ffmpeg…」。

**验证**：桩 ffmpeg 证明「命中缓存不重调、文件一变必重生成、超时被杀、失败不留垃圾」；
再用**真 ffmpeg** 造一段测试视频、生成缩略图并解码成功（480×360 PNG）。
这条真跑抓到了一个桩程序永远发现不了的 bug：`-f image2` 按输出后缀挑编码器，
临时文件名是 `.tmp` 时它退回 **mjpeg** —— 结果每个缩略图都是「叫 .png 的 JPEG」，
预览全部解码失败。现在临时名以 `.png` 结尾并且显式 `-c:v png`。

## 8. 归档发行与签名（minisign）

`.github/workflows/release.yml`：打 `v*` tag → 在 x86_64 与 aarch64 的**原生 runner** 上
构建 `--no-default-features` 核心二进制 → 归档 + `.sha256` + `.minisig` 签名 → 建 GitHub Release。

- **没有签名密钥就直接失败**：不会悄悄发一个未签名的二进制出去（那种失败没人会发现）。
- 签完**立刻用仓库里的公钥自验一遍**：签错了当场红。
- 校验和文件里只写文件名（先 `cd dist`）—— 写成 `dist/xxx.tar.gz` 会让用户下载后
  `sha256sum -c` 找不到文件，白白以为产物坏了。
- 新增 `packaging/verify-release.sh`：一次验校验和 + 签名，三种结局分明
  （`0` 通过 / `1` 别装别转发 / `2` 环境问题）。
- 新增 `packaging/release-signing.md`：密钥生成、进 GitHub secret、轮换、以及**这套机制不保证什么**。
- `packaging/update-aur.sh`：更新 `pkgver`、真校验和与 `.SRCINFO`，**默认只展示差异**，不提交不推送。

**验证**：用 minisign 0.11（和 Ubuntu 24.04 仓库里同一个版本）真签真验；
篡改产物 → `1`，未签名 → `1`，公钥不在 → `2`（且提示「验不了不等于验过了」）；
把 CI 的签名步骤从 YAML 里抽出来在本地跑通，并验证「无 secret」「无公钥」两条路径都**拒绝发布**；
AUR 脚本在临时 git checkout 里真跑：`SKIP` → 真实 sha256，`.SRCINFO` 重生成，没有产生任何提交。

## 9. 非 Arch 构建（不链接 libalpm）

`Cargo.toml` 里 `alpm` 变成 optional，新增 `pacman` feature（默认开）。
`cargo build --no-default-features` 出来的二进制**完全不链接 libalpm**（`ldd` 里没有它），
包管理域明确回「此构建未启用包管理」，不把「没数据」伪装成「空数据」。
CI 新增 `core-no-pacman` job（ubuntu）+ `msrv` job（1.90 编译一次）。

**验证**：`ldd target/debug/toolbox-hub`（no-default-features）→ `libalpm 出现次数: 0`；
两条 feature 组合的 clippy 与测试全绿。

## 10. CI 门禁补齐

| 门 | 防的是什么 |
| --- | --- |
| `cargo fmt --check` / `clippy --all-features -D warnings` | 常规 |
| `cargo test --all-features` | 常规（547 条） |
| `core-no-pacman` job | 非 Arch 路径被改坏 |
| `msrv` job（1.90.0） | 用了新工具链才有的 API |
| `python3 registry/build-featured.py --check` | featured 索引与文件对不上 |
| `toolbox-hub check registry/` + 重建后 `git diff` | 索引/产物不是源码该产出的那一份 |
| **widget 冒烟**：`bash -n` / `fish --no-execute` + 渲染断言 | 生成的脚本语法错、回填结果被污染 |
| **打包脚本语法**：`bash -n` 三个脚本 | 发版路径的脚本平时没人跑，语法错要等到打 tag 才暴露 |
| `smoke`（每周）：`cargo test -- --ignored` | 真跑 ffmpeg / 7z / jq / pandoc / exiftool / magick |

---

## 已知局限与剩余风险（本轮**没有**做到的）

1. **没有真的发过 Release**。`release.yml` 只在本地被逐段抽出来验过（含两条拒绝发布的路径），
   没打过 tag、没有真实 runner 上的双架构产物。「多架构发布可用」这句话目前**没有证据**。
2. **zsh 没有真跑过**。开发机上没有 zsh：生成脚本的内容与回调 API 有用例覆盖，
   但没人在真 zsh 里按过 `Ctrl-G`。bash 与 fish 是真跑过的。
3. **危险判定是 denylist，不是沙箱**。第 5 节列了明确的盲点；它拦的是踩过的坑。
4. **队列不能取消「其中某一个」任务**。Ctrl-C 与 API 取消都是**整队**语义：先 SIGTERM
   再整组 SIGKILL。想只要其中几个跑，得自己拆任务文件。
5. **缩略图缓存键不是内容指纹**：同秒、同大小的替换看不出来。
6. **AUR 的 PKGBUILD 仍是「本地 source + `sha256sums=('SKIP')`」**。交 AUR 前必须换成
   URL + 真实校验和（`packaging/README.md` 里写着这两步）；`update-aur.sh` 会把 `SKIP`
   换成真校验和，但那只是在本地 checkout 里，AUR 上的那份还没换。
7. **minisign 公钥还没有进仓库**。维护者要先生成密钥对、把公钥提交为 `packaging/minisign.pub`、
   私钥进 `MINISIGN_SECRET_KEY`；在那之前 release 流水线**会故意失败**（这是设计，不是 bug），
   而 `verify-release.sh` 会以 `2` 退出并说明「验不了不等于验过了」。
8. **非 Arch 的 CI 只在本地等价验证过**：`core-no-pacman` job 是这一轮新加的，本轮没在
   GitHub 上真跑过它（本地用同一组命令验过）。
9. **35 条 featured Recipe 没有逐条在真实终端手工执行过**。过的是「字段合法 + 安全标记 +
   模板参数 + 不触发闸门 + 渲染稳定」这些可自动化的性质；`git-status` 与 `os-release` 真跑过，
   其余没有。它们的命令本身都是常见只读命令，但「没跑过」就是没跑过。
10. **`import` 的 navi/cheat 解析是行级的**：不解析 navi 的 `$ name: command` 候选源、
    不解析多列映射，也没有把「注释当标题」做得更聪明。不认识的语法一律降级为「仅展示」。
    tldr 按真实页面的两行格式（`- 说明` + 反引号命令）解析；cheat 的 YAML front matter
    当元数据处理（只认 `syntax` 与 `tags`，不引 YAML 解析器）；占位符名里带 `/` 的
    （`{{path/to/file}}`）仍然只降级、不自动改名。
