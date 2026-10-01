#!/usr/bin/env bash
# 把当前仓库打成 makepkg 要的源码包（toolbox-hub-<版本>.tar.gz）。
#
# 为什么用 `git archive` 而不是 `tar czf`：它只装**已提交**的文件（不会把
# target/、临时文件、编辑器备份卷进去），而且同样内容打出来是同一个包 ——
# 校验和稳定，重跑不会因为时间戳变来变去。
#
# 用法（在仓库根目录）：
#   packaging/make-source-tarball.sh
#   cd packaging && makepkg -f --nodeps        # --nodeps：依赖已经装好了就别再 pacman -S
set -euo pipefail

cd "$(dirname "$0")/.."

version=$(sed -n 's/^version *= *"\(.*\)"/\1/p' Cargo.toml | head -1)
[ -n "$version" ] || { echo "读不出 Cargo.toml 里的版本号" >&2; exit 1; }

name="toolbox-hub-$version.tar.gz"
out="packaging/$name"

# 有未提交改动就提醒：打出来的包不含它们，容易「明明改了却没用上」
if [ -n "$(git status --porcelain)" ]; then
  echo "注意：工作区有未提交的改动，打出来的包只含最后一次提交的内容。" >&2
fi

git archive --format=tar.gz --prefix="toolbox-hub-$version/" -o "$out" HEAD
echo "已生成 $out（$(du -h "$out" | cut -f1)）"
echo "接着：cd packaging && makepkg -f"
