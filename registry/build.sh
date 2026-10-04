#!/usr/bin/env bash
# ToolHub 工具仓库的构建脚本。
#
# 这个脚本现在**只是 toolbox-hub build 的薄包装**：真正干活的是客户端里那段
# Rust（src/repository/author.rs）。这样做有两条好处：
#
#   1. 作者本地构建、CI 构建、别人复现构建，用的是**同一段代码**，不会漂；
#   2. 可复现的细节（条目排序、mtime/uid/gid 归零、路径不带 ./ 前缀、gzip 头无
#      时间戳）只在一个地方实现，改一次到处生效。
#
# 以前这里是一段手写的 tar+sha256 逻辑，结果撞上过「tar 带 ./ 条目被客户端拒绝」
# 这种坑 —— 那种坑不该由每个插件作者各踩一次。
set -euo pipefail

root="$(cd "$(dirname "$0")" && pwd)"

# 找一个 toolbox-hub：环境变量 > PATH > 源码目录下的 debug 构建（开发时方便）
if [ -n "${TOOLBOX_HUB_BIN:-}" ]; then
    bin="${TOOLBOX_HUB_BIN}"
elif command -v toolbox-hub >/dev/null 2>&1; then
    bin="toolbox-hub"
elif [ -x "$root/../target/debug/toolbox-hub" ]; then
    bin="$root/../target/debug/toolbox-hub"
else
    cat >&2 <<'EOF'
找不到 toolbox-hub。

  * 装一个：cargo install --path .   或   sudo pacman -S toolbox-hub
  * 或者指定：TOOLBOX_HUB_BIN=/path/to/toolbox-hub bash registry/build.sh
  * 或者在源码目录里先 cargo build，再重跑这个脚本（会用 target/debug/toolbox-hub）

写插件的说明见 docs/plugin-authoring.md。
EOF
    exit 1
fi

exec "$bin" build "$root" "$@"
