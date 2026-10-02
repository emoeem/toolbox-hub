#!/usr/bin/env bash
# toolbox-hub 的脚本侧小库：`source` 之后用 tbx_confirm / tbx_pick / tbx_pager。
#
# 为什么要有它：这几个界面组件本身已经能直接调（`toolbox-hub ui …`），但脚本里
# 每次都要记「结果走 stdout、退出码 0/1/2」这件事很烦，包一层函数就顺手了。
#
# 用法：
#   source scripts/tbx.sh                       # 或 /usr/share/toolbox-hub/tbx.sh
#
#   if tbx_confirm "要覆盖 $out 吗？" --danger; then
#       ffmpeg -y ... "$out"
#   fi
#
#   src=$(tbx_pick --filter '*.mp4') || exit 1  # 取消时退出码非 0，直接退出
#   echo "选了 $src"
#
#   ls -l | tbx_pager --title "文件列表"
#
# 约定（和组件本身一致，所以这些函数可以放进 `$(...)`）：
#   * 结果只走 stdout，界面画在 /dev/tty —— 把输出重定向到文件也不会花屏；
#   * 退出码：0 = 选了/确认，1 = 取消（Esc/n），2 = 没终端或参数错。

# 用哪个二进制：环境变量 TBX 可以指到别处（比如 ./target/release/toolbox-hub）。
TBX="${TBX:-toolbox-hub}"

# 现在有终端可以弹界面吗。
#
# 判据是「能不能打开 /dev/tty」而不是「stdout 是不是 tty」——脚本经常把
# stdout/stderr 都重定向掉，但终端其实还在。
tbx_has_terminal() {
    { : < /dev/tty; } 2>/dev/null
}

# tbx_confirm <文案> [--yes 文字] [--no 文字] [--danger] [--default yes|no]
#
# 退出码即答案：0 = 确认，1 = 取消，2 = 没有终端且没给 --default。
# 用法：`if tbx_confirm "……"; then …; fi`
tbx_confirm() {
    "$TBX" ui confirm "$@"
}

# tbx_pick [--dir 目录] [--filter 词] [--multi] [--dir-only]
#
# 选中的路径打到 stdout，一行一个；取消时退出码 1（stdout 是空的）。
# 用法：`path=$(tbx_pick --dir ~/Videos) || exit 1`
tbx_pick() {
    "$TBX" ui pick "$@"
}

# tbx_pager [--title 文字] [文件]
#
# 从管道或文件读内容翻页。**没有终端时原样透传**，所以
# `cmd | tbx_pager | grep foo` 在脚本里照样能用。
tbx_pager() {
    "$TBX" ui pager "$@"
}
