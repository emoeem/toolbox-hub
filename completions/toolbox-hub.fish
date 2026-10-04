# toolbox-hub 的 fish 补全。
#
# 装上：把本文件复制（或软链）到 ~/.config/fish/completions/toolbox-hub.fish
#
# 包名补全是现查 pacman 数据库的：装的时候列**仓库里**的（你要装的多半是没装的），
# 卸的时候列**本地已装**的（你只能卸装过的东西）。两份都只取名字，不解析版本。

function __toolbox_hub_repo_packages -d "官方源 + 已装 + AUR 里的包名（补全用）"
    # -Slq 是「仓库里的名字」，-Qq 是「本地已装的名字」；AUR 的名字本地库没有，
    # 就只能靠手打了（问 AUR 一次要联网，补全里不该干这个）。
    pacman -Slq 2>/dev/null
    pacman -Qq 2>/dev/null
end

function __toolbox_hub_local_packages -d "本地已装的包名（补全用）"
    pacman -Qq 2>/dev/null
end

# 没有子命令时进 TUI，位置参数是脚本目录
complete -c toolbox-hub -f
complete -c toolbox-hub -n "not __fish_seen_subcommand_from -s -i -r -u -n -l --exp --imp --all --orphans --remove-orphans --clear-cache" \
    -a "(__fish_complete_directories)" -d "脚本目录（TUI 用）"

# ── 动作 ──
complete -c toolbox-hub -s s -l search -x -d "搜官方源 + AUR"
complete -c toolbox-hub -s i -l install -x -a "(__toolbox_hub_repo_packages)" -d "安装（含 AUR 时自动走 paru）"
complete -c toolbox-hub -s r -l remove -x -a "(__toolbox_hub_local_packages)" -d "卸载（-Rns）"
complete -c toolbox-hub -s u -l update -d "系统更新（paru -Syu，没有 paru 就 sudo pacman -Syu）"
complete -c toolbox-hub -s n -l news -d "看 Arch 新闻"
complete -c toolbox-hub -s l -l list -d "列已安装包"

# ── 范围 ──
complete -c toolbox-hub -l unread -d "只看未读（与 -n 搭配）"
complete -c toolbox-hub -l read -d "只看已读（与 -n 搭配）"
complete -c toolbox-hub -s a -l all-news -d "全部新闻（与 -n 搭配）"
complete -c toolbox-hub -l exp -d "只看自己点名装的（与 -l 搭配）"
complete -c toolbox-hub -l imp -d "只看被依赖拖进来的（与 -l 搭配）"
complete -c toolbox-hub -l all -d "全部（-n 与 -l 都认）"

# ── 维护 ──
complete -c toolbox-hub -l orphans -d "列出孤儿包"
complete -c toolbox-hub -l remove-orphans -d "卸载孤儿包"
complete -c toolbox-hub -l clear-cache -d "清包缓存（paccache -rk1）"

# ── 工具仓库（ToolHub 自己的工具包；和上面 pacman 那批是两回事）──
#
# 装/卸/看详情的名字从本地索引缓存里取，所以补全不联网。
function __toolbox_hub_tool_ids -d "索引缓存里的包 id（补全用）"
    set -l cache (test -n "$TOOLBOX_HUB_CACHE"; and echo $TOOLBOX_HUB_CACHE; or echo $HOME/.cache/toolbox-hub)
    for f in $cache/repositories/*/index.json
        test -f $f; or continue
        grep -o '"id"[[:space:]]*:[[:space:]]*"[^"]*"' $f 2>/dev/null | sed 's/.*"\([^"]*\)"$/\1/'
    end
end

complete -c toolbox-hub -n "__fish_seen_subcommand_from repo" -a "list" -d "看有哪些仓库与状态"
complete -c toolbox-hub -n "__fish_seen_subcommand_from repo" -a "add" -d "加一个仓库（URL / 本地路径 / file://…）"
complete -c toolbox-hub -n "__fish_seen_subcommand_from repo" -a "remove" -d "删掉一个仓库"
complete -c toolbox-hub -n "__fish_seen_subcommand_from repo" -a "enable" -d "启用"
complete -c toolbox-hub -n "__fish_seen_subcommand_from repo" -a "disable" -d "停用（搜索与安装都不再看它）"
complete -c toolbox-hub -n "__fish_seen_subcommand_from repo" -a "update" -d "刷新索引"
complete -c toolbox-hub -n "__fish_seen_subcommand_from repo" -a "search" -d "只在仓库里搜"
complete -c toolbox-hub -n "__fish_seen_subcommand_from repo" -a "path" -d "打印索引缓存目录"
complete -c toolbox-hub -n "__fish_seen_subcommand_from repo; and __fish_seen_subcommand_from add" \
    -l name -x -d "显示名" -l trust -x -a "trusted verified community unknown" -d "信任等级" \
    -l priority -x -d "越小越优先"

complete -c toolbox-hub -n "__fish_seen_subcommand_from install uninstall info" \
    -a "(__toolbox_hub_tool_ids)" -d "工具包"
complete -c toolbox-hub -n "__fish_seen_subcommand_from install" -l allow-unverified -d "来源没给哈希时表示你接受"
complete -c toolbox-hub -n "__fish_seen_subcommand_from uninstall" -l purge -d "连你改过的文件一起删"
complete -c toolbox-hub -n "__fish_seen_subcommand_from search" -l scope -x -a "all available installed upgradable" -d "范围"

# ── 写插件（作者工具）──
complete -c toolbox-hub -n "__fish_seen_subcommand_from new" -l kind -x -a "script recipe" -d "形状"
complete -c toolbox-hub -n "__fish_seen_subcommand_from new" -l dir -x -a "(__fish_complete_directories)" -d "放哪儿"
complete -c toolbox-hub -n "__fish_seen_subcommand_from new" -l description -x -d "一句话说明"
complete -c toolbox-hub -n "__fish_seen_subcommand_from new" -l domain -x \
    -a "媒体 图像 系统 网络 开发 工具 包管理 打包 发现" -d "域"
complete -c toolbox-hub -n "__fish_seen_subcommand_from new" -l program -x -d "动作要跑的命令"
complete -c toolbox-hub -n "__fish_seen_subcommand_from check build" \
    -a "(__fish_complete_directories)" -d "包目录或仓库根目录"
complete -c toolbox-hub -n "__fish_seen_subcommand_from build" -l stamp -d "给索引盖时间戳（默认不盖，以保持可复现）"

# ── 通用 ──
complete -c toolbox-hub -l dry-run -d "只打印将要执行的命令，不动系统"
complete -c toolbox-hub -s h -l help -d "显示帮助"
complete -c toolbox-hub -s V -l version -d "显示版本"
