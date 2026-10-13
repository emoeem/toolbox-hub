#!/usr/bin/env bash
# 在独立的 AUR checkout 中更新版本、校验和与 .SRCINFO；默认只展示差异，不推送。
set -euo pipefail

if [[ $# -ne 2 ]]; then
  printf '用法：%s <AUR checkout 目录> <版本号>\n' "$0" >&2
  exit 2
fi

aur_dir=$1
version=$2
[[ -d $aur_dir && -f $aur_dir/PKGBUILD ]] || {
  printf '错误：%s 不是含 PKGBUILD 的 AUR checkout\n' "$aur_dir" >&2
  exit 2
}
[[ $version =~ ^[0-9]+\.[0-9]+\.[0-9]+([.-][A-Za-z0-9.-]+)?$ ]] || {
  printf '错误：版本号格式不合法：%s\n' "$version" >&2
  exit 2
}

cd "$aur_dir"
sed -i -E "s/^pkgver=.*/pkgver=$version/" PKGBUILD
updpkgsums
makepkg --printsrcinfo > .SRCINFO
git diff --check
printf '版本与校验和已更新。请人工审阅以下差异，再决定是否提交到 AUR：\n'
git diff -- PKGBUILD .SRCINFO
