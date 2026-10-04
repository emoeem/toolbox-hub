#!/usr/bin/env bash
# 把 `tbx-pkgbuild` 装到能被 Toolbox Hub 找到的地方。
#
#   ./install.sh                 # 装到 ~/.local/bin（默认，已在 PATH 上就不用再改环境变量）
#   ./install.sh /usr/local/bin  # 装到别处（需要写权限）
#   ./install.sh --uninstall     # 从 ~/.local/bin 删掉
#
# 为什么装到 ~/.local/bin：
#   ① 它在 PATH 上 → manifest 里 program = "tbx-pkgbuild" 这个**裸命令名**能解析到；
#   ② 它也是本地脚本 Provider 的第一个扫描目录（这个脚本没有注解头，
#      所以不会在列表里重复出现，真正的入口是 manifests/pkgbuild-source.toml）。
#
# 装了 Arch 包的用户不需要本脚本：PKGBUILD 会把它放进 /usr/bin。
#
# 装完还要有仓库本身：默认找 ~/pkgbuild-source（或 ~/code/pkgbuild-source），
# 也可以用 TOOLBOX_HUB_PKGBUILD_SRC 指到别处。
set -Eeuo pipefail

here="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
name="tbx-pkgbuild"
dest="${HOME}/.local/bin"

if [[ ${1:-} == "--uninstall" ]]; then
    target="${dest}/${name}"
    if [[ -e $target ]]; then
        rm -f "$target"
        printf '已删除 %s\n' "$target"
    else
        printf '%s 本来就不在 %s\n' "$name" "$dest"
    fi
    exit 0
fi

[[ -n ${1:-} ]] && dest="$1"
command -v install >/dev/null || { printf '缺少 install 命令（coreutils）\n' >&2; exit 1; }

install -d "$dest"
install -m 755 "$here/$name" "$dest/$name"
printf '  %s → %s\n' "$name" "$dest/$name"

case ":$PATH:" in
    *":$dest:"*) printf '\n该目录已在 PATH 上 ✅\n' ;;
    *) printf '\n注意：%s 不在 PATH 上，manifest 会找不到它。\n' "$dest"
       printf '      export PATH="%s:$PATH"  # 加到你的 shell 配置里\n' "$dest" ;;
esac

printf '\n之后在 Toolbox Hub 里 Ctrl-R 刷新（或重开），「打包」域就有那 20 个动作了。\n'
printf '找不到仓库的话它会直说 —— 默认找 ~/pkgbuild-source，或用 TOOLBOX_HUB_PKGBUILD_SRC 指定。\n'
