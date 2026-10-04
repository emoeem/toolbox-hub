# toolbox-hub 的 bash 补全。
#
# 装上（二选一）：
#   1) 临时启用：source completions/toolbox-hub.bash
#   2) 持久安装：sudo cp completions/toolbox-hub.bash \
#        /usr/share/bash-completion/completions/toolbox-hub
#      用户级（需要 bash-completion 包）：~/.local/share/bash-completion/completions/toolbox-hub
#
# 子命令与选项以 `toolbox-hub --help` 为准（src/cli.rs 的 USAGE 常量）。
# 工具包名字只从本地索引缓存与安装账本里读，不联网；读不到就静默返回空 ——
# 补全不该报错，也不该把 shell 卡住（pacman 那两处有 timeout 兜底）。

# 缓存目录 / 数据目录：和二进制认的 TOOLBOX_HUB_CACHE / TOOLBOX_HUB_DATA 一致。
_toolbox_hub_cache_dir() {
    printf '%s\n' "${TOOLBOX_HUB_CACHE:-${XDG_CACHE_HOME:-$HOME/.cache}/toolbox-hub}"
}

_toolbox_hub_data_dir() {
    printf '%s\n' "${TOOLBOX_HUB_DATA:-${XDG_DATA_HOME:-$HOME/.local/share}/toolbox-hub}"
}

# 工具包 id：索引缓存 <缓存>/repositories/*/index.json
#            + 安装账本 <数据目录>/packages/<id>/installed.toml
# 两个目录都不存在、或文件读不出来，就什么也不输出（静默）。
_toolbox_hub_ids() {
    local cache data f
    cache="$(_toolbox_hub_cache_dir)"
    data="$(_toolbox_hub_data_dir)"
    for f in "$cache"/repositories/*/index.json; do
        [ -f "$f" ] || continue
        grep -o '"id"[[:space:]]*:[[:space:]]*"[^"]*"' "$f" 2>/dev/null |
            sed 's/.*"\([^"]*\)"$/\1/'
    done
    for f in "$data"/packages/*/installed.toml; do
        [ -f "$f" ] || continue
        basename -- "$(dirname -- "$f")"
    done
}

# 仓库 id：缓存里的目录名就是仓库 id（official / 自己加的）。
_toolbox_hub_repo_ids() {
    local cache d
    cache="$(_toolbox_hub_cache_dir)"
    for d in "$cache"/repositories/*/; do
        [ -d "$d" ] || continue
        basename -- "${d%/}"
    done
}

# pacman 的包名（-i 装的是仓库里的，-r 卸的是本地已装的）。
# 只在真的按 Tab 时才跑，且用 timeout 兜底：读不出来（或太慢）就静默放弃。
_toolbox_hub_pacman() {
    command -v pacman >/dev/null 2>&1 || return 0
    if command -v timeout >/dev/null 2>&1; then
        timeout 1 pacman "$@" 2>/dev/null || true
    else
        pacman "$@" 2>/dev/null || true
    fi
}

# 把候选词按前缀筛进 COMPREPLY；顺手去重（同一个 id 可能出现在多个仓库里，
# -i/-r 的 pacman 包名也会「仓库一个、本地一个」各来一次）。
_toolbox_hub_values() {
    local words="$1" cur="$2" w
    local -A seen=()
    COMPREPLY=()
    while IFS= read -r w; do
        [ -n "$w" ] || continue
        [ -n "${seen[$w]:-}" ] && continue
        seen[$w]=1
        COMPREPLY+=("$w")
    done < <(compgen -W "$words" -- "$cur")
}

# 目录候选（位置参数可以是「脚本目录」，check/build 也要目录）。
_toolbox_hub_dirs() {
    local cur="$1"
    COMPREPLY=()
    mapfile -t COMPREPLY < <(compgen -d -- "$cur")
    compopt -o filenames 2>/dev/null || true
}

# 工具包 id 候选（同样经过去重）。
_toolbox_hub_tool_ids() {
    _toolbox_hub_values "$(_toolbox_hub_ids)" "$1"
}

_toolbox_hub() {
    local cur prev i w group="" gsub="" sub="" seen_group=0
    cur="${COMP_WORDS[COMP_CWORD]}"
    prev="${COMP_WORDS[COMP_CWORD-1]}"
    COMPREPLY=()

    local subcmds="search info install uninstall list update run new check build repo ui"
    local actions="-s -i -r -u -n -l --search --install --remove --update --news --list --unread --read --exp --imp --all --orphans --remove-orphans --clear-cache"
    local common="--dry-run --config-dir --data-dir -h --help -V --version"

    # 已经用到哪个子命令（组）。注意 repo / ui 之后的位置参数属于这个组，
    # 不能再当成顶层子命令（比如 `repo update` 里的 update）。
    for ((i = 1; i < COMP_CWORD; i++)); do
        w="${COMP_WORDS[i]}"
        if [ "$seen_group" = 1 ]; then
            [ -z "$gsub" ] && gsub="$w"
            continue
        fi
        case "$w" in
            repo | ui)
                group="$w"
                seen_group=1
                ;;
            search | info | install | uninstall | list | update | run | new | check | build)
                sub="$w"
                ;;
        esac
    done

    # 选项的取值：按前一个词判断。
    case "$prev" in
        --scope) _toolbox_hub_values "all available installed upgradable" "$cur"; return ;;
        --trust) _toolbox_hub_values "trusted verified community unknown" "$cur"; return ;;
        --kind) _toolbox_hub_values "script recipe" "$cur"; return ;;
        --domain) _toolbox_hub_values "媒体 图像 系统 网络 开发 工具 包管理 打包 发现" "$cur"; return ;;
        --default) _toolbox_hub_values "yes no" "$cur"; return ;;
        --dir | --config-dir | --data-dir) _toolbox_hub_dirs "$cur"; return ;;
        -i | --install) _toolbox_hub_values "$(_toolbox_hub_pacman -Slq)" "$cur"; return ;;
        -r | --remove) _toolbox_hub_values "$(_toolbox_hub_pacman -Qq)" "$cur"; return ;;
        --yes | --no | --title | --filter | --name | --priority | --description | --program | -s | --search) return ;;
    esac

    # 顶层：动作选项 + 通用选项（第一个词还多一份子命令）。
    if [ "$seen_group" = 0 ] && [ -z "$sub" ]; then
        if [ "$COMP_CWORD" = 1 ]; then
            if [[ "$cur" == .* || "$cur" == /* || "$cur" == "~"* ]]; then
                _toolbox_hub_dirs "$cur"
                return
            fi
            _toolbox_hub_values "$subcmds $actions $common" "$cur"
            return
        fi
        _toolbox_hub_values "$actions $common" "$cur"
        return
    fi

    # repo 组：list / add / remove / enable / disable / update / search / path
    if [ "$group" = "repo" ]; then
        if [ -z "$gsub" ]; then
            if [[ "$cur" == -* ]]; then
                _toolbox_hub_values "$common" "$cur"
            else
                _toolbox_hub_values "list add remove enable disable update search path" "$cur"
            fi
            return
        fi
        case "$gsub" in
            add) _toolbox_hub_values "$common --name --trust --priority" "$cur" ;;
            remove | enable | disable | update) _toolbox_hub_repo_ids "$cur" ;;
            *) _toolbox_hub_values "$common" "$cur" ;;
        esac
        return
    fi

    # ui 组：confirm / pager / pick
    if [ "$group" = "ui" ]; then
        if [ -z "$gsub" ]; then
            if [[ "$cur" == -* ]]; then
                _toolbox_hub_values "$common" "$cur"
            else
                _toolbox_hub_values "confirm pager pick" "$cur"
            fi
            return
        fi
        case "$gsub" in
            confirm) _toolbox_hub_values "$common --yes --no --danger --default" "$cur" ;;
            pager) _toolbox_hub_values "$common --title" "$cur" ;;
            pick) _toolbox_hub_values "$common --dir --filter --multi --dir-only" "$cur" ;;
            *) _toolbox_hub_values "$common" "$cur" ;;
        esac
        return
    fi

    # 顶层子命令。
    case "$sub" in
        install)
            if [[ "$cur" == -* ]]; then
                _toolbox_hub_values "$common --allow-caution --allow-modified --allow-unverified" "$cur"
            else
                _toolbox_hub_tool_ids "$cur"
            fi
            ;;
        uninstall)
            if [[ "$cur" == -* ]]; then
                _toolbox_hub_values "$common --purge" "$cur"
            else
                _toolbox_hub_tool_ids "$cur"
            fi
            ;;
        info | run) _toolbox_hub_tool_ids "$cur" ;;
        new) _toolbox_hub_values "$common --kind --dir --description --domain --program" "$cur" ;;
        check) _toolbox_hub_dirs "$cur" ;;
        build)
            if [[ "$cur" == -* ]]; then
                _toolbox_hub_values "$common --stamp" "$cur"
            else
                _toolbox_hub_dirs "$cur"
            fi
            ;;
        search) _toolbox_hub_values "$common --scope" "$cur" ;;
        update) _toolbox_hub_values "$common --check --allow-caution --allow-modified" "$cur" ;;
        *) _toolbox_hub_values "$common" "$cur" ;;
    esac
}

complete -F _toolbox_hub toolbox-hub
