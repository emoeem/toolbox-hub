# sing-box 运维脚本

这套脚本把「本机 sing-box 代理」的日常运维（体检 / 修复 / 模式切换 / 容器覆盖）做成
**可回滚的一键动作**。它们是 `manifests/sing-box.toml` 里那 5 个内置动作真正执行的命令 ——
动作只负责把开关填成表单，**改配置的安全管线全在脚本里**：

```text
改配置（内存里）→ sing-box check（不过就什么都不做）→ 时间戳备份 → 原子替换
  → 重启 → 健康检查 → 任一环节失败就自动回滚 + 重启
```

## 装法

| 你是 | 怎么做 |
| --- | --- |
| 源码用户 | `./scripts/sing-box/install.sh`（装到 `~/.local/bin`，它已在 PATH 上） |
| 装了 Arch 包 | 不用做：PKGBUILD 已经把它们放进 `/usr/bin` |

装完在 Toolbox Hub 里 `Ctrl-R` 刷新，「**网络**」域就会出现这些动作。
脚本标了 `internal=true`，所以不会在列表里再重复出现一次（入口只有 manifest 那一份）。

## 动作一览

| 动作 | 脚本 | 干什么 |
| --- | --- | --- |
| sing-box 体检 | `sing-box-status` | **只读**：模式 / 入站 / 出口 / 分流 / DNS 拦截 / 规则集构成 / 备份，一屏看完 |
| sing-box 审计修复 | `sing-box-audit` | 补 anti-AD 广告表、刷新 geoip/cn、清理冗余规则；6 个可选开关 |
| sing-box 切到 eBPF | `sing-box-switch-ebpf` | TUN → eBPF（本机流量在内核 socket 层接管）；可选数据面与下游接口 |
| sing-box 切回 TUN | `sing-box-switch-tun` | eBPF → TUN（**容器 / 虚拟机也能被代理**：TUN 用 `auto_route` 覆盖转发流量） |
| sing-box 容器代理 | `sing-box-container-proxy` | 在 podman 网桥上开 eBPF `shared` 数据面，让 rootful 容器也走代理 |

`--fast`（体检）、6 个 `--with-*`（审计）、`--data-plane`、`--shared`、`--from`、`--iface`、`--disable`
这些开关在动作表单里就是一个个勾选框/输入框，不用记。

## 几个踩过的坑（都写进脚本了，这里备查）

1. **DNS 广告拦截用 REFUSED 会让应用卡 5 秒**：sing-box 的 `action: reject` 回的是 REFUSED，
   而 `systemd-resolved` 不把 REFUSED 转告客户端（它当成"上游坏了"去重试其他上游），于是
   应用每个广告域名都要等到超时。改成 `predefined` + `rcode: NXDOMAIN` 后是正常应答，
   应用**秒失败**并做负缓存。
2. **预检 `--mode local` 全绿 ≠ TC 可用**：`local` 只覆盖 cgroup 数据面；`tc` / `shared`
   （`packet_rewrite`）走的是 TC 路径，只有 `--mode all` 才涉及，而它在这台机器上是
   `inconclusive`（36 项 unknown）。实跑会失败于
   `register TC eBPF TCP listener: operation not supported` ——
   所以脚本会在动配置**之前**先拦下来，别白重启一轮。
3. **rootless 容器（pasta）天然抓不到**：pasta 把容器数据包 splice 进宿主网络栈、
   **不创建宿主 socket**，cgroup 钩子看不见它（容器 DNS 与国内直连通、境外直连失败）。
   要么切回 TUN，要么给容器 `--network=host`。
4. **切换瞬间有 2–3 秒 DNS 空窗**：旧模式拆掉、新模式还没挂上的那段，查询可能外泄并被
   缓存。脚本会在切换后 `resolvectl flush-caches`，健康检查也用**随机子域**问广告域名
   （任何缓存都命不中），免得被缓存骗成"没拦住"。
5. **健康判据要跟着模式走**：TUN 看 `tun0` 是否出现；eBPF 没有 `tun0`，改用
   **"不设任何代理的请求是否等于代理出口"** —— 这才是"内核接管生效"的铁证。

## 和 pkgbuild 仓库里那份的关系

同一套脚本在两个仓库都有：那边供打包 / CI 使用，这边供 Toolbox Hub 的内置动作使用
（这边多了注解头与"自提权"前导：不是 root 就自己 `exec sudo`，这样 manifest 的 `program`
可以直接写脚本名，缺脚本时能如实显示「依赖缺失」）。改的时候**两边一起改**，
或者把这边当作源头、往那边复制。
