# 网络诊断

排查「网不通」的标准四件套，加上一条只看本机的体检。**全部只读**，不改任何网络配置。

## 装了什么

| 文件 | 落到哪 |
| --- | --- |
| `scripts/net-diagnose` | `~/.local/bin/net-diagnose` |
| `manifests/net-diagnose.toml` | 包自己的目录 |
| `README.md` | 包自己的目录 |

## 动作一览

| 动作 | 实际跑的命令 | 依赖 |
| --- | --- | --- |
| 解析域名（DNS） | `dig +time=3 +tries=1 +short example.com A` | `sudo pacman -S bind` |
| ping 连通性 | `ping -c 4 1.1.1.1` | `sudo pacman -S iputils` |
| 看路由（traceroute） | `traceroute -m 15 -w 2 1.1.1.1` | `sudo pacman -S traceroute` |
| 端口通不通 | `nc -z -v -w 5 1.1.1.1 443` | `sudo pacman -S openbsd-netcat` |
| 本机出口 IP 与 DNS 配置 | `net-diagnose local` | `sudo pacman -S iproute2` |
| 网络诊断工具自检 | `net-diagnose tools` | 同上 |

前四条的 program 是裸命令名，所以依赖由 manifest 自动记入，界面缺什么提示什么。

## 实测输出

```console
$ dig +time=3 +tries=1 +short example.com A
172.66.147.243
104.20.23.154

$ ping -c 4 1.1.1.1
64 字节，来自 1.1.1.1: icmp_seq=1 ttl=52 时间=97.6 毫秒
--- 1.1.1.1 ping 统计 ---
已发送 4 个包， 已接收 4 个包, 0% packet loss, time 3014ms
rtt min/avg/max/mdev = 77.497/88.479/101.094/10.946 ms

$ nc -z -v -w 5 1.1.1.1 443
Connection to 1.1.1.1 443 port [tcp/https] succeeded!

$ nc -z -v -w 5 127.0.0.1 22
nc: connect to 127.0.0.1 port 22 (tcp) failed: Connection refused
```

## 端口那条为什么退出码 1 也算成功

manifest 里给 `net-port-check` 写了 `ok_exit_codes = [0, 1]`：

- 端口开着 → nc 返回 0；
- 端口关着 → nc 返回 1，但那是**诊断结论**，不是命令跑挂了。

两种都算成功，只有更严重的错误（比如域名解析不了、参数写错）才会被标成失败。

## 命令行直接用

```console
$ net-diagnose local    # 本机地址 / 默认路由 / 出口源地址 / DNS 配置
$ net-diagnose tools    # dig ping traceroute nc ip 都装了吗
$ net-diagnose -h
```

## 几点说明

- **出口 IP 不发包**。`net-diagnose local` 用的是 `ip route get 1.1.1.1`，
  只是问内核「这个包会从哪张网卡、用哪个源地址出去」，不产生任何流量。
- **DNS 配置优先看 resolvectl**。`/etc/resolv.conf` 里如果是 `127.0.0.53`，
  那说明 DNS 由 systemd-resolved 代管，真正的上游服务器要看 resolvectl 的输出 ——
  脚本会把这件事直接写在结果里，免得对着 127.0.0.53 发懵。
- **traceroute 全是 `* * *` 不一定是故障**。很多路由器不回 UDP 探测包，
  尤其在云网络里。脚本照常返回 0，结果就是「中间这几跳没吭声」。
- 四个探测动作都跑在普通用户权限下：ping 用 ICMP socket，traceroute 默认 UDP，
  都不需要 root。
