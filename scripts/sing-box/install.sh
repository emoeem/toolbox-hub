#!/usr/bin/env bash
# 把 scripts/sing-box/ 下的运维脚本装到能被 Toolbox Hub 找到的地方。
#
#   ./install.sh                 # 装到 ~/.local/bin（默认，已在 PATH 上就不用再改环境变量）
#   ./install.sh /usr/local/bin  # 装到别处（需要写权限）
#   ./install.sh --uninstall     # 从 ~/.local/bin 删掉
#
# 为什么装到 ~/.local/bin：
#   ① 它在 PATH 上 → manifest 里 program = "sing-box-audit" 这类**裸命令名**能解析到；
#   ② 它也是本地脚本 Provider 的**第一个**扫描目录（只是这些脚本标了 internal=true，
#      不会在列表里重复出现，真正的入口是 manifests/sing-box.toml 里的动作）。
#
# 装了 Arch 包的用户不需要本脚本：PKGBUILD 会把它们放进 /usr/bin。
set -Eeuo pipefail

here="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
dest="${HOME}/.local/bin"

if [[ ${1:-} == "--uninstall" ]]; then
    removed=0
    for f in "$here"/sing-box-*; do
        [[ -f $f ]] || continue
        target="${dest}/$(basename "$f")"
        [[ -e $target ]] && { rm -f "$target"; printf '  已删除 %s\n' "$target"; removed=$((removed + 1)); }
    done
    printf '共删除 %d 个\n' "$removed"
    exit 0
fi

[[ -n ${1:-} ]] && dest="$1"
command -v install >/dev/null || { printf '缺少 install 命令（coreutils）\n' >&2; exit 1; }

install -d "$dest"
count=0
for f in "$here"/sing-box-*; do
    [[ -f $f ]] || continue
    install -m 755 "$f" "$dest/"
    printf '  %s → %s\n' "$(basename "$f")" "$dest/$(basename "$f")"
    count=$((count + 1))
done

printf '\n装了 %d 个脚本到 %s\n' "$count" "$dest"
case ":$PATH:" in
    *":$dest:"*) printf '该目录已在 PATH 上 ✅\n' ;;
    *) printf '注意：%s 不在 PATH 上，manifest 会找不到这些命令。\n' "$dest"
       printf '      export PATH="%s:$PATH"  # 加到你的 shell 配置里\n' "$dest" ;;
esac
printf '之后在 Toolbox Hub 里 Ctrl-R 刷新（或重开），「网络」域就有 sing-box 动作了。\n'
