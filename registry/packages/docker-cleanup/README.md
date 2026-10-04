# Docker 清理与体检

帮你在动手之前先看清楚：Docker 到底占了哪块盘、有哪些容器、哪些东西可以回收。
**所有清理动作默认只预览**，只有显式打开「真的删」才会执行 prune。

## 装了什么

| 文件 | 落到哪 |
| --- | --- |
| `scripts/docker-cleanup` | `~/.local/bin/docker-cleanup` |
| `manifests/docker-cleanup.toml` | 包自己的目录 |
| `README.md` | 包自己的目录 |

依赖：**docker**（`sudo pacman -S docker`）。没装的话界面会直接提示；
脚本自己也会给出安装与启动守护进程的步骤，而不是一句 `command not found`。

## 动作一览

| 动作 | 干什么 | 危险度 |
| --- | --- | --- |
| 看 Docker 占了多少磁盘 | `docker system df` | 只读 |
| 列出所有容器（含已停止） | `docker ps -a`，可按状态过滤 | 只读 |
| docker 环境自检 | 版本 / context / 存储驱动 / 数据根目录 | 只读 |
| 删掉指定的容器 | docker rm 一个容器，可选 -f | caution |
| 清理停掉的容器 | docker container prune -f | caution |
| 清理悬空镜像 | docker image prune -f | caution |
| 清理未使用的卷 | docker volume prune -f | caution |
| 清理全部可回收对象 | 上面四类一起，**不含卷** | caution |

## 清理动作怎么工作

以「清理悬空镜像」为例，动作默认拼出来的 argv 是：

```console
$ docker-cleanup images
docker-cleanup：预览模式（什么都没删；确认无误后加 --yes 再跑一次）

== 悬空镜像（dangling） ==
a1b2c3d4e5f6  <none>:<none>  412MB
0f1e2d3c4b5a  <none>:<none>  88.4MB
会执行的命令：docker image prune -f

== 当前占用（docker system df） ==
TYPE            TOTAL     ACTIVE    SIZE      RECLAIMABLE
Images          12        3         4.21GB    1.8GB (42%)
...
```

看清楚了，把表单里的「真的删」打开（argv 变成 docker-cleanup images --yes），才会真的删：

```console
$ docker-cleanup images --yes
docker-cleanup：真的要删了（--yes）
→ docker image prune -f
Deleted Images: untagged: sha256:a1b2...
Total reclaimed space: 500.4MB
```

## 命令行直接用

装完之后 `docker-cleanup` 就在 `~/.local/bin` 里，TUI 之外也能用：

```console
$ docker-cleanup -h
$ docker-cleanup all            # 预览：停掉的容器 / 悬空镜像 / 网络 / 构建缓存
$ docker-cleanup all --yes      # 真的清
$ docker-cleanup volumes        # 卷单独来，先看
$ docker-cleanup doctor         # 环境自检
```

## 几点说明

- **卷是最危险的**。docker volume prune 删掉的是数据，删了不会进回收站。
  所以「清理全部」刻意不包含卷，请单独确认后再跑。
- **容器名候选只列正在跑的容器**。`kind = "dynamic"` 的 `docker-containers` 候选来自
  docker ps；要删已经停掉的容器，直接在输入框里手打名字。
  「列出所有容器（含已停止）」那条动作就是用来看名字的。
- **不需要 root**。只要你在这个 docker 组里（或者用 rootless docker），
  这些命令都以普通用户跑。
- 用的是 docker <x> prune -f，而不是 docker system prune -a，
  所以**不会**顺手删掉你正在用的镜像。
