## 我改了什么

- [ ] 新插件：`registry/packages/<id>/`
- [ ] 修改已有插件：`<id>`（如果改了内容，版本号升了吗？）
- [ ] 其它（文档 / CI / 工具）

## 自测清单

- [ ] `toolbox-hub check registry/` 干净（0 错误；有警告的话我看过并处理了）
- [ ] `toolbox-hub build registry/` 跑过，`registry/index.json` 与 `registry/artifacts/` **一起提交**了
- [ ] 真装了一遍：`repo add` → `install` → `run` 能用，`uninstall` 卸得掉
- [ ] 破坏性 / 不可逆的动作标了 `danger = "caution"`，默认行为是「只预览」
- [ ] 没有偷偷 `sudo`；没有要求 root 却不声明（官方仓库 `requires_root` 一律 `false`）
- [ ] 包里没有符号链接，没有 `..` / 绝对路径 / `~`
- [ ] 依赖声明全了（动作的 `program`，以及脚本里调用的其它命令）
- [ ] 脚本有 `-h`，退出码有意义；没有 `TODO` / 占位文案

## 说明

<!-- 这个包做什么、和已有的包有什么不同、希望评审重点看哪里。没有就删掉这段。 -->
