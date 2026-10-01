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

# ── 通用 ──
complete -c toolbox-hub -l dry-run -d "只打印将要执行的命令，不动系统"
complete -c toolbox-hub -s h -l help -d "显示帮助"
complete -c toolbox-hub -s V -l version -d "显示版本"
