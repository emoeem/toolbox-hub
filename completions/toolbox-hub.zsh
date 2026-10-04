#compdef toolbox-hub
#
# toolbox-hub 的 zsh 补全（`#compdef` 风格：整个文件既是 autoload 的函数体，
# 也能直接 source —— 末尾那段会判断当前是哪种加载方式）。
#
# 装上：把本文件放进 $fpath 里的目录并命名为 _toolbox_hub，例如
#     mkdir -p ~/.zfunc
#     cp completions/toolbox-hub.zsh ~/.zfunc/_toolbox_hub
#     # ~/.zshrc 里：fpath=(~/.zfunc $fpath) 之后有 autoload -Uz compinit && compinit
# 系统级：sudo cp completions/toolbox-hub.zsh /usr/share/zsh/site-functions/_toolbox_hub
# 已开 compinit 的当前 shell 也可以直接生效：source completions/toolbox-hub.zsh
#
# 子命令与选项以 `toolbox-hub --help` 为准（src/cli.rs 的 USAGE 常量）。
# 工具包名字只从本地索引缓存与安装账本里读，不联网；读不到就静默返回空 ——
# 补全不该报错，也不该把 shell 卡住（pacman 那两处有 timeout 兜底）。

# 缓存目录 / 数据目录：和二进制认的 TOOLBOX_HUB_CACHE / TOOLBOX_HUB_DATA 一致。
_toolbox_hub_cache_dir() {
    print -r -- "${TOOLBOX_HUB_CACHE:-${XDG_CACHE_HOME:-$HOME/.cache}/toolbox-hub}"
}

_toolbox_hub_data_dir() {
    print -r -- "${TOOLBOX_HUB_DATA:-${XDG_DATA_HOME:-$HOME/.local/share}/toolbox-hub}"
}

# 工具包 id：索引缓存 <缓存>/repositories/*/index.json
#            + 安装账本 <数据目录>/packages/<id>/installed.toml（目录名就是 id）。
# 读不出来就什么也不输出（(N) 让不匹配的 glob 变成空）。
_toolbox_hub_ids() {
    local cache data f
    cache="$(_toolbox_hub_cache_dir)"
    data="$(_toolbox_hub_data_dir)"
    for f in "$cache"/repositories/*/index.json(N); do
        grep -o '"id"[[:space:]]*:[[:space:]]*"[^"]*"' "$f" 2>/dev/null | sed 's/.*"\([^"]*\)"$/\1/'
    done
    for f in "$data"/packages/*/installed.toml(N); do
        print -r -- "${f:h:t}"
    done
}

# 仓库 id：缓存里的目录名就是仓库 id（official / 自己加的）。
_toolbox_hub_repo_ids() {
    local cache d
    cache="$(_toolbox_hub_cache_dir)"
    for d in "$cache"/repositories/*(N/); do
        print -r -- "${d:t}"
    done
}

# pacman 的包名（-i 装的是仓库里的，-r 卸的是本地已装的）。
# 只在真的按 Tab 时才跑，且用 timeout 兜底：读不出来（或太慢）就静默放弃。
_toolbox_hub_pacman() {
    (( $+commands[pacman] )) || return 0
    if (( $+commands[timeout] )); then
        timeout 1 pacman "$@" 2>/dev/null
    else
        pacman "$@" 2>/dev/null
    fi
}

# 所有地方都认的通用选项。
typeset -ga _toolbox_hub_common
_toolbox_hub_common=(
    '--dry-run[只打印将要执行的命令，不动系统]'
    '--config-dir[配置目录（state.toml / tools.d / packages.toml）]:目录:_directories'
    '--data-dir[数据目录（历史 / 队列 / 已读新闻）]:目录:_directories'
    '(-h --help)'{-h,--help}'[显示这份帮助]'
    '(-V --version)'{-V,--version}'[显示版本]'
)

# repo 子命令。
_toolbox_hub_repo() {
    local -a subs
    subs=(
        'list:看有哪些仓库'
        'add:加一个仓库（URL / 本地路径 / file://…）'
        'remove:删掉一个仓库'
        'enable:启用'
        'disable:停用（搜索与安装都不再看它）'
        'update:刷新索引'
        'search:只在仓库里搜'
        'path:打印索引缓存目录'
    )
    if (( CURRENT == 2 )); then
        _describe -t commands 'repo 子命令' subs
        return
    fi
    case $words[2] in
        add)
            _arguments "${_toolbox_hub_common[@]}" \
                '--name[显示名]:名字:' \
                '--trust[信任等级]:等级:(trusted verified community unknown)' \
                '--priority[越小越优先]:整数:' \
                '1:地址:'
            ;;
        remove|enable|disable|update)
            _arguments "${_toolbox_hub_common[@]}" '1:仓库 id:($(_toolbox_hub_repo_ids))'
            ;;
        search)
            _arguments "${_toolbox_hub_common[@]}" '1:搜索词:'
            ;;
        *)
            _arguments "${_toolbox_hub_common[@]}"
            ;;
    esac
}

# ui 子命令。
_toolbox_hub_ui() {
    local -a subs
    subs=(
        'confirm:弹一个确认框。退出码 0=确认 1=取消'
        'pager:翻页器；不是终端就原样透传'
        'pick:挑文件，选中的路径打到 stdout'
    )
    if (( CURRENT == 2 )); then
        _describe -t commands 'ui 子命令' subs
        return
    fi
    case $words[2] in
        confirm)
            _arguments "${_toolbox_hub_common[@]}" \
                '--yes[确认按钮的文字]:文字:' \
                '--no[取消按钮的文字]:文字:' \
                '--danger[红框 + 默认停在「取消」]' \
                '--default[没有终端时用这个答案]:答案:(yes no)' \
                '1:文案:'
            ;;
        pager)
            _arguments "${_toolbox_hub_common[@]}" '--title[标题]:文字:' '1:文件:_files'
            ;;
        pick)
            _arguments "${_toolbox_hub_common[@]}" \
                '--dir[从哪个目录开始]:目录:_directories' \
                '--filter[预先填进过滤框]:词:' \
                '--multi[多选]' \
                '--dir-only[只让选目录]'
            ;;
        *)
            _arguments "${_toolbox_hub_common[@]}"
            ;;
    esac
}

_toolbox_hub() {
    local curcontext="$curcontext" state line
    typeset -A opt_args
    local -a subcmds
    subcmds=(
        'search:本地工具 + 仓库里的包一起搜'
        'info:看详情'
        'install:安装工具包'
        'uninstall:卸载工具包'
        'list:列已安装的工具包'
        'update:把已安装的工具包升到最新'
        'run:填好参数直接跑'
        'new:造一个新插件骨架'
        'check:校验插件包或整个仓库'
        'build:打包 + 重建 index.json'
        'repo:工具仓库（list/add/remove/enable/disable/update/search/path）'
        'ui:界面组件（confirm/pager/pick）'
    )

    _arguments -C \
        "${_toolbox_hub_common[@]}" \
        '(-s --search)'{-s,--search}'[搜官方源 + AUR]:搜索词:' \
        '(-i --install)'{-i,--install}'[安装（含 AUR 时自动走 paru）]:包:($(_toolbox_hub_pacman -Slq))' \
        '(-r --remove)'{-r,--remove}'[卸载（-Rns，会清掉只被它们依赖的依赖）]:包:($(_toolbox_hub_pacman -Qq))' \
        '(-u --update)'{-u,--update}'[系统更新（有 paru 用 paru，否则 sudo pacman -Syu）]' \
        '(-n --news)'{-n,--news}'[看 Arch 新闻]' \
        '--unread[只看未读（与 -n 搭配）]' \
        '--read[只看已读（与 -n 搭配）]' \
        '(-l --list)'{-l,--list}'[列已安装包]' \
        '--exp[只看自己点名装的（与 -l 搭配）]' \
        '--imp[只看被依赖拖进来的（与 -l 搭配）]' \
        '--all[全部（-n 与 -l 都认，也是它们的默认值）]' \
        '--orphans[列出孤儿包（没人依赖、你也没点名装）]' \
        '--remove-orphans[卸载孤儿包]' \
        '--clear-cache[清包缓存（paccache -rk1）]' \
        '1:子命令:->cmds' \
        '*::参数:->args' && return 0

    case $state in
        cmds)
            _describe -t commands 'toolbox-hub 子命令' subcmds
            ;;
        args)
            case $words[1] in
                repo)
                    _toolbox_hub_repo
                    ;;
                ui)
                    _toolbox_hub_ui
                    ;;
                search)
                    _arguments "${_toolbox_hub_common[@]}" \
                        '--scope[搜索范围]:范围:(all available installed upgradable)' \
                        '1:搜索词:'
                    ;;
                info)
                    _arguments "${_toolbox_hub_common[@]}" '1:名字:($(_toolbox_hub_ids))'
                    ;;
                install)
                    _arguments "${_toolbox_hub_common[@]}" \
                        '--allow-caution[允许装自标为「注意」的包（它的动作会改动系统）]' \
                        '--allow-modified[允许覆盖你装完之后改过的文件]' \
                        '--allow-unverified[来源没提供 SHA-256 时才需要]' \
                        '*:工具包:($(_toolbox_hub_ids))'
                    ;;
                uninstall)
                    _arguments "${_toolbox_hub_common[@]}" \
                        '--purge[连你改过的那些也一起删]' \
                        '*:工具包:($(_toolbox_hub_ids))'
                    ;;
                list)
                    _arguments "${_toolbox_hub_common[@]}"
                    ;;
                update)
                    _arguments "${_toolbox_hub_common[@]}" \
                        '--check[只检查不升级；有更新时退出码 10]' \
                        '--allow-caution[允许升级自标为「注意」的包]' \
                        '--allow-modified[允许覆盖你改过的文件]'
                    ;;
                run)
                    _arguments "${_toolbox_hub_common[@]}" '*:工具:($(_toolbox_hub_ids))'
                    ;;
                new)
                    _arguments "${_toolbox_hub_common[@]}" \
                        '--kind[形状]:形状:(script recipe)' \
                        '--dir[放哪儿]:目录:_directories' \
                        '--description[一句话说明]:说明:' \
                        '--domain[域]:域:(媒体 图像 系统 网络 开发 工具 包管理 打包 发现)' \
                        '--program[动作要跑的命令]:命令:' \
                        '1:包名:'
                    ;;
                check)
                    _arguments "${_toolbox_hub_common[@]}" '1:包目录或仓库根目录:_directories'
                    ;;
                build)
                    _arguments "${_toolbox_hub_common[@]}" \
                        '--stamp[给索引盖时间戳（默认不盖，以保持可复现）]' \
                        '1:包目录或仓库根目录:_directories'
                    ;;
                *)
                    _default
                    ;;
            esac
            ;;
    esac
}

# 放进 fpath 时（autoload：文件体就是函数体）把参数交给真正的实现；
# 直接 source 时只注册 compdef。
if [[ "$funcstack[1]" == "_toolbox_hub" || "$funcstack[1]" == "_toolbox-hub" ]]; then
    _toolbox_hub "$@"
elif (( $+functions[compdef] )); then
    compdef _toolbox_hub toolbox-hub
fi
