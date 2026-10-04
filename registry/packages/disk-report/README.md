# 磁盘占用排查（disk-report）

一个只读的磁盘占用排查包，三个动作都调用同一个脚本 `disk-report`，
底层只用 `du` / `find` / `sort` / `awk`。

## 动作

| 动作 | 命令 | 用来回答 |
| --- | --- | --- |
| 目录占用汇总 | `disk-report dirs` | 这个目录里，哪一层、哪个子目录最占地方？ |
| 找出最大的文件 | `disk-report files` | 到底是哪几个文件把磁盘吃掉了？ |
| 按扩展名统计 | `disk-report ext` | 硬盘上最多的 / 最大的是哪类文件？ |

## 表单参数

**目录占用汇总**：`目录`、`汇总深度`（1–4）、`显示前几项`（10–100）。

**找出最大的文件**：`目录`、`搜索深度`（1/2/3/5/全部）、`最小体积`（不限 / ≥1 MiB / ≥10 MiB / ≥100 MiB / ≥1 GiB）、`显示前几个`。

**按扩展名统计**：`目录`、`统计深度`、`显示前几种`。

表单值怎么变成命令行，可以直接对着看：

```console
$ disk-report dirs --path . --depth 1 --top 10
目录占用  .  （深度 1，前 10 项）

     1.2 GiB  ./target
   340.0 MiB  ./src
...
```

## 命令行直用

```console
$ disk-report dirs  --path ~/.cache --depth 1 --top 20
$ disk-report files --path ~/下载 --depth 2 --top 20 --min-size 10485760
$ disk-report ext   --path . --depth 3 --top 15
$ disk-report -h
```

三个子命令都支持 `-h`；`disk-report -h` 给出总览。

## 已知边界

* **不跨文件系统**：`du -x` / `find -xdev`。扫 `/` 时不会掉进 `/mnt` 上挂着的盘；
  起点自己就是挂载点时照常扫描。
* 大小按真实字节排序，只是**显示**成 KiB / MiB / GiB，所以 9 MiB 不会排在 10 MiB 前面。
* 没有权限读的目录：`du` 的报错走 stderr，统计继续（因此普通用户也能直接跑）。
* 路径里带换行符的文件名会被当成两行 —— 这是行式输出的固有限制。

## 依赖

| 命令 | 包 |
| --- | --- |
| `du` | coreutils |
| `find` | findutils |

两个都是 Arch 基础系统自带的；缺了会提示 `sudo pacman -S findutils coreutils`。
不需要 root，不联网。

## 卸载

```console
$ toolbox-hub uninstall disk-report
```
