# 系统清理预检

清理系统的第一步不是删，是**先称重**。这个包把 Arch 机器上最常见的几处垃圾
摆出来 —— 各占多少、能省多少 —— 再让你决定动不动手。

除了最后一条，**所有动作都是只读的**：只跑 du / find / journalctl --disk-usage / pacman -Qdtq。

## 装了什么

| 文件 | 落到哪 |
| --- | --- |
| `scripts/sysclean` | `~/.local/bin/sysclean` |
| `manifests/system-cleanup.toml` | 包自己的目录 |
| `README.md` | 包自己的目录 |

依赖：**paccache**（`sudo pacman -S pacman-contrib`）—— 只有预览那一步用它。
缺了会给出安装命令，以及官方的替代方案。

## 动作一览

| 动作 | 读什么 | 危险度 |
| --- | --- | --- |
| 系统清理预检（一次看完） | 下面五节全跑一遍 | 只读 |
| pacman 缓存占了多少 | `/var/cache/pacman/pkg` 的 du + find | 只读 |
| systemd 日志占了多少 | journalctl --disk-usage | 只读 |
| ~/.cache 里谁最占地方 | `du` 排行榜，条数可调 | 只读 |
| 缩略图缓存占了多少 | du + find | 只读 |
| 孤儿包有哪些 | pacman -Qdtq | 只读 |
| 清理 pacman 缓存 | paccache --dryrun 预览 / -r 真删 | caution |

## 实测输出

在作者的机器上，`sysclean report` 跑出来是这样：

```console
── pacman 包缓存（/var/cache/pacman/pkg） ──
大小：9.2G
缓存包数：596
可以清：paccache -d -k 1 先预览，paccache -r -k 1 真删（需要 root）

── systemd 日志 ──
Archived and active journals take up 46.9M in the file system.
可以清：sudo journalctl --vacuum-size=200M（或 --vacuum-time=2weeks）

── 用户缓存（/home/emo/.cache） ──
总大小：15G
3.4G	/home/emo/.cache/uv
2.2G	/home/emo/.cache/mozilla
...

── 缩略图缓存 ──
大小：16M
文件数：435

── 孤儿包 ──
gnome-desktop
共 1 个。可以清：sudo pacman -Rns $(pacman -Qdtq)
```

## 真清理那一条怎么工作

动作默认拼出的 argv 是 sysclean pacman-prune -k 1，它只跑 paccache 的 **dryrun**：

```console
$ sysclean pacman-prune -k 1
预览模式（paccache --dryrun）：下面是会被删掉的包。

$ paccache -d -k 1
==> finished dry run: 86 candidates (disk space saved: 2.14 GiB)

确认没问题，就把「真的删」打开（或加 --yes）再跑一次。
```

打开「真的删」之后 argv 变成 `sysclean pacman-prune -k 1 --yes`。
这一步要 root，而脚本**不会偷偷 sudo**：它是 root 就直接删；sudo 免密可用就用 sudo -n；
两条都不行就退出码 1，并把该跑的命令原样打给你：

```console
$ sysclean pacman-prune -k 1 --yes
sysclean: 删 /var/cache/pacman/pkg 里的包需要 root，这里没法替你提权。

  上面预览列出的就是会被删掉的包。要真删，请自己跑一次：
      sudo sysclean pacman-prune -k 1 --yes
```

## 几点说明

- **~/.cache 不是垃圾桶**。里面有浏览器、包管理器、构建工具自己的缓存，
  删掉通常不会坏，但会明显变慢。所以这里只列出来，不提供一键清空的动作。
- **孤儿包只列不删**。pacman -Rns 是有副作用的（可能连带卸掉别的东西），
  该由你自己确认列表后再跑。
- 日志清理走的是 systemd 自己的 journalctl --vacuum-size，这里只告诉你占用。
