# Shell 补全

三份补全的子命令与选项都以 `toolbox-hub --help` 为准（`src/cli.rs` 的 `USAGE` 常量），
覆盖同一批命令，只是写法不同：

| 文件 | shell | 怎么装 | 生效方式 |
| --- | --- | --- | --- |
| `toolbox-hub.bash` | bash | `source` 本文件；或拷进 `/usr/share/bash-completion/completions/toolbox-hub`（用户级：`~/.local/share/bash-completion/completions/`） | `source` 立即生效；拷文件后新开 shell |
| `toolbox-hub.zsh` | zsh | 拷进 `$fpath` 里的目录并**命名为 `_toolbox_hub`**（如 `~/.zfunc/_toolbox_hub`）；系统级 `/usr/share/zsh/site-functions/_toolbox_hub` | `compinit` 之后新开 shell；已开 `compinit` 时可以 `source` |
| `toolbox-hub.fish` | fish | 拷进 `~/.config/fish/completions/toolbox-hub.fish` | fish 启动时自动加载 |

## 数据从哪来

补全只读本地缓存，**不联网**：

- 工具包 id：`$TOOLBOX_HUB_CACHE`（默认 `~/.cache/toolbox-hub`）下
  `repositories/*/index.json` 里的 `"id"`，加上安装账本
  `$TOOLBOX_HUB_DATA`（默认 `~/.local/share/toolbox-hub`）下 `packages/<id>/installed.toml`
  的目录名。
- 仓库 id：`repositories/` 下的目录名（如 `official`）。

缓存没有、文件读不出来，就静默返回空 —— 补全**不报错、不阻塞**。
只有 `-i` / `-r`（pacman 装 / 卸）要列包名，那里各有一条 1 秒的 `timeout` 兜底，
超时或 `pacman` 不在就当作没有候选。

## bash

```bash
source completions/toolbox-hub.bash                      # 当前 shell 立即生效
sudo cp completions/toolbox-hub.bash \
  /usr/share/bash-completion/completions/toolbox-hub     # 持久（需要 bash-completion）
```

用 `complete -F _toolbox_hub toolbox-hub` 注册；函数内部按「已经敲到哪个子命令」分派。

## zsh

```zsh
mkdir -p ~/.zfunc
cp completions/toolbox-hub.zsh ~/.zfunc/_toolbox_hub
# ~/.zshrc：fpath=(~/.zfunc $fpath) 之后 autoload -Uz compinit && compinit
```

文件是 `#compdef` + `_arguments` 风格，整个文件就是 autoload 的函数体（末尾会判断自己
是被 autoload 还是被 `source`）。**文件名必须是 `_toolbox_hub`**，zsh 按文件名决定补全函数名。

## fish 那份为什么不在这里「安装」

fish 那份就在本目录（`toolbox-hub.fish`），但 fish 没有 bash / zsh 那种注册步骤：
它启动时按文件名自动加载 `~/.config/fish/completions/*.fish`，不存在 `compinit` / `fpath` /
`source` 的差别，复制过去即可（文件头也写了）。另外 fish 用的是 `complete -c` 的动态条件，
某些选项只在特定子命令下出现；bash / zsh 两份是静态列表 + 少量位置判断，效果一致。
