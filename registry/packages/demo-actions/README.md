# Demo Actions

ToolHub Repository 里的「完整包裹」示例：一个包里同时带**命令**和**动作定义**。

装完之后你得到：

| 文件 | 落到哪 | 干什么 |
| --- | --- | --- |
| `scripts/demo-echo` | `~/.local/bin/demo-echo` | 把拿到的参数原样回显出来 |
| `manifests/demo.toml` | 包自己的目录 | 两条动作：打招呼 / 列成一串 |
| `toolbox.toml` | 包自己的目录 | 包元数据，id 与 version 必须和索引一致 |
| `README.md` | 包自己的目录 | 就是这份说明 |

## 它演示了什么

* **一个包可以既有命令又有动作**：`manifests/demo.toml` 里的 `program = "demo-echo"`
  写的是命令名，而这个命令正是同一个包装出来的。
* **表单 → argv 是可预测的**：在 TUI 里把「名字」填成 `alice`、打开「转成大写」，
  拼出来的就是 `demo-echo -u alice`。想看结果，直接跑一次 `demo-echo -u alice` 也一样。
* **多值参数**：`demo-echo-list` 的「词」可以填 `一,二,三`，每个词各占一行。

## 手动试一下

```console
$ demo-echo -b "ToolHub demo" -u alice
TOOLHUB DEMO
ALICE

$ demo-echo -n 苹果 banana 中文
 1. 苹果
 2. banana
 3. 中文
```

## 卸载

```console
$ toolbox-hub uninstall demo-actions
```

装出来的文件如果是**你自己改过**的，卸载时会保留并报告；原样没动的才删。
