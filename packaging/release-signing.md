# 发布签名（minisign）

发行产物必须带签名。**没有签名的产物一律不发布** —— 流水线里签名那一步拿不到密钥
就直接红，而不是悄悄发一个裸二进制出去（那种失败不会有人发现）。

## 为什么是 minisign 而不是 SHA-256 就够了

`.sha256` 只能证明「下载没坏」。它自己没有任何权威性：谁能替换产物，就能顺手把
校验和一起替换掉，两边对得上，看起来一切正常。签名把「这是维护者签的」这件事钉死，
所以校验和是**方便**，签名是**必需**。

minisign 的选择理由：密钥小、签名短（一行）、验证命令一句话、Arch 与 Debian/Ubuntu
仓库里都有（Ubuntu 24.04 是 0.11），不需要 GPG 那套钥匙串。

## 一次性准备（维护者）

```bash
# 1) 生成密钥对。CI 用**无口令**密钥：minisign 的密码从 tty 读，
#    流水线里没有 tty，带口令的密钥在 CI 里根本签不出来。
minisign -G -W \
  -p packaging/minisign.pub \
  -s ~/.minisign/toolbox-hub-ci.key

# 2) 公钥提交进仓库（verify-release.sh 与流水线的自检都用它）
git add packaging/minisign.pub && git commit -m "chore(release): 加上签名公钥"

# 3) 私钥内容进 GitHub secret：仓库 Settings → Secrets → Actions →
#    新增 MINISIGN_SECRET_KEY，值就是 ~/.minisign/toolbox-hub-ci.key 的全部内容。
#    别把它存进仓库、别贴进 issue、别丢进日志。

# 4) 私钥文件本身留在工作站加密处（或删掉——secret 里那份才是流水线用的）。
```

## 发布时发生了什么

`release.yml` 的构建 job 在打包之后、上传之前：

1. 装 minisign；
2. 没有 `MINISIGN_SECRET_KEY` 或仓库里没有 `packaging/minisign.pub` → **直接失败**；
3. 把 secret 写进 `$RUNNER_TEMP` 的临时文件（`umask 077`，退出时 `trap` 删除）；
4. 对每个 tarball 签名，签名里带一行 trusted comment：`toolbox-hub <版本> <仓库> <tag>`；
5. **立刻用仓库里的公钥自验一遍** —— 签错了当场红，而不是等用户来报。

产物一共三个：`*.tar.gz`、`*.tar.gz.sha256`、`*.tar.gz.minisig`。

## 用户怎么验

```bash
bash packaging/verify-release.sh toolbox-hub-<版本>-<架构>.tar.gz
# 或者两步手动来：
sha256sum -c toolbox-hub-<版本>-<架构>.tar.gz.sha256
minisign -V -p packaging/minisign.pub -m toolbox-hub-<版本>-<架构>.tar.gz
```

`verify-release.sh` 的三种结局要分清：

| 退出码 | 含义 | 该做什么 |
|---|---|---|
| `0` | 校验和与签名都过 | 可以装 |
| `1` | 产物被改过 / 没签名 / 签名不对 | **不要装，也不要转发给别人** |
| `2` | 用法或环境问题（没装 minisign、找不到公钥） | 先修环境；「验不了」不等于「验过了」 |

## 轮换与撤销

私钥泄露（贴进聊天记录、误提交、工作站失守）时：

1. 立刻在仓库里生成新密钥、更新 `packaging/minisign.pub`，并用新私钥替换
   `MINISIGN_SECRET_KEY`；
2. 用新密钥**重新签**仍然支持的那些版本（重跑 tag 对应 workflow，或手动签）；
3. 在 release notes 里写清楚：哪个 tag 之前用旧密钥、旧公钥指纹是什么。

minisign 没有在线撤销列表，所以「哪个版本的签名由哪把密钥签的」这件事只能靠
release notes 记着。公钥换掉之后，旧版本的签名就再也验不过 —— 这是设计使然，
不是 bug。

## 这套机制**不**保证什么

- **不保证产物能构建**。签名只说明「这串字节是维护者签的」，不说明它没有漏洞。
- **不保证可复现**。要自己核对字节，得用 `cargo build --locked` 在同样工具链上重建。
- **不保护私钥**。密钥的防护只有 GitHub secret + 只在 tag 上触发 + 可轮换这三条；
  谁拿到 secret，谁就能签出「合法」的产物。

## 在本地手动签（例如热修复）

带口令的密钥在这里没问题（有 tty 可以输密码）：

```bash
minisign -S -s ~/.minisign/toolbox-hub.key -m dist/toolbox-hub-<版本>-<架构>.tar.gz \
  -t "toolbox-hub <版本> $(git remote get-url origin) <tag>"
bash packaging/verify-release.sh --pub packaging/minisign.pub dist/*.tar.gz
```

**只在受控工作站上手工签**。手工签出来的产物和流水线产物在验证上是等价的，
所以别在没有把握的机器上做。
