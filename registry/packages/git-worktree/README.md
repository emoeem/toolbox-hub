# Worktree 与分支管理（git-worktree）

围绕 `git worktree` 和分支的五件日常事，一个脚本五个子命令。

## 动作

| 动作 | 命令 | 干什么 |
| --- | --- | --- |
| 列出 worktree | `git-worktree list` | 一行一个 worktree：目录、HEAD、分支 |
| 新建 worktree | `git-worktree add` | 给分支开独立工作目录，或顺手开新分支 |
| 列出已合并的分支 | `git-worktree merged` | 哪些分支已经并进基准（只读） |
| 清理已合并的分支 | `git-worktree cleanup` | 删掉它们；**默认只列出来** |
| 清理失效的 worktree 记录 | `git-worktree prune` | 目录手动删了之后，清掉 `.git` 里的残留 |

## 表单参数

* **列出 worktree**：`仓库目录`、`机器可读格式`。
* **新建 worktree**：`仓库目录`、`分支`（候选 = 当前仓库的分支，也可以手打新名字）、
  `worktree 目录`、`新建分支（-b）`、`新分支起点`、`目标已存在也硬上`。
* **列出已合并的分支**：`仓库目录`、`基准`（默认 HEAD）、`连远程分支一起列`。
* **清理已合并的分支**：`仓库目录`、`基准`、`额外保住的分支`、`真的删`。
* **清理失效的 worktree 记录**：`仓库目录`、`只预演`。

拼出来的命令行一看就懂，比如「新建 worktree」填好之后就是：

```console
$ git-worktree add --repo . --branch feature/login --path ../wt-login
签出已有分支 feature/login，worktree 落到 ../wt-login
正在检出文件...
```

## 安全约定

* `cleanup` **默认不动手**，只打印待删清单；确认之后再打开「真的删」。
* 删除只用 `git branch -d`（只会删确实合并过的），不用 `-D`。
* 下面这些分支**永远**在保护名单里：
  当前分支、`--base` 指定的基准、`main`、`master`、
  以及**任何已经在某个 worktree 里签出的分支**（git 自己也不让删）。
  需要额外保护就往 `额外保住的分支` 里写，逗号分隔。
* `prune` 只清 `.git/worktrees` 里的管理文件，**不会**动你的工作目录。
  不确定就先 `--dry-run`。

## 命令行直用

```console
$ git-worktree list
$ git-worktree list --repo ~/code/proj --porcelain
$ git-worktree add --branch feature/login --path ../wt-login
$ git-worktree add --branch feature/new --path ../wt-new --new --start main
$ git-worktree merged --base main
$ git-worktree cleanup --base main --keep develop
$ git-worktree cleanup --base main --keep develop --delete
$ git-worktree prune --dry-run
$ git-worktree -h
```

`--path` 的相对路径按 `--repo` 解析（git 的 `-C` 语义），不是按你当前 shell 的目录。

## 依赖

| 命令 | 包 |
| --- | --- |
| `git` | git |

不需要 root，不联网（除非你的仓库配置让 git 自己去连远程）。

## 卸载

```console
$ toolbox-hub uninstall git-worktree
```
