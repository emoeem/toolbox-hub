# 打包与分发

这个目录里是「怎么把它装进系统」的东西：

| 文件 | 干什么的 |
| --- | --- |
| `PKGBUILD` | Arch 的包定义（`makepkg` 用） |
| `make-source-tarball.sh` | 把仓库打成源码包（`git archive`，只含已提交的内容） |
| `toolbox-hub.1` | 手册页（roff 源），装到 `/usr/share/man/man1/` |

## 本机打包

> 实测过：`makepkg -f --nodeps --nocheck` 能一次跑通，产出
> `toolbox-hub-0.1.0-1-x86_64.pkg.tar.zst`（4 MB），里面有
> `/usr/bin/toolbox-hub`、`man1/toolbox-hub.1.gz`、fish 补全；解包试跑
> `--version` 正常。

```bash
cd ~/code/toolbox-hub
packaging/make-source-tarball.sh      # 生成 packaging/toolbox-hub-<版本>.tar.gz
cd packaging
makepkg -f                            # 依赖没装齐时它会提示；装齐了可以直接跑
sudo pacman -U toolbox-hub-*.pkg.tar.zst
```

`--nocheck` 跳过 `cargo test`（快一点），`--nodeps` 跳过依赖检查
（依赖已经装好的时候用）。

装在哪儿：

```
/usr/bin/toolbox-hub
/usr/share/man/man1/toolbox-hub.1        → man toolbox-hub
/usr/share/fish/vendor_completions.d/toolbox-hub.fish
```

跑起来 `toolbox-hub`，配置会落在 `~/.config/toolbox-hub/`，
第一次进界面还会自动写一份带注释的 `packages.toml`。

## 为什么 build() 里必须写 --locked

`Cargo.lock` 在这个仓库里是**提交进去**的。`--locked` 让它按锁定版本构建 ——
不写的话，某天某个依赖发新版就可能让打包突然失败，而失败原因跟你的改动毫无关系。

## 交到 AUR（两件事还没做）

1. **许可证**：仓库里还没有 `LICENSE`，`PKGBUILD` 里现在写的是 `license=('unknown')`。
   AUR 要求这个字段真实有效 —— 定了许可证之后：提交 `LICENSE`、改 `license=()`、
   把 `package()` 里那行 `install -Dm644 LICENSE …` 的注释打开。
2. **公开的源码地址**：`source=()` 现在指的是本地 tarball（`sha256sums=('SKIP')`），
   AUR 打包机拿不到。仓库推上去之后：

```bash
# 1. 换 source：把本地 tarball 换成远端地址（tag 也行）
#    source=("$pkgname-$pkgver.tar.gz::https://github.com/你/toolbox-hub/archive/refs/tags/v$pkgver.tar.gz")
# 2. 让 makepkg 算校验和
cd packaging && makepkg -g >> PKGBUILD

# 3. 生成 .SRCINFO（AUR 只认它里面的元数据）
makepkg --printsrcinfo > .SRCINFO

# 4. 推到 AUR（第一次要先在 aur.archlinux.org 建包）
git clone ssh://aur@aur.archlinux.org/toolbox-hub.git /tmp/aur-toolbox-hub
cp PKGBUILD .SRCINFO /tmp/aur-toolbox-hub/
cd /tmp/aur-toolbox-hub && git add -A && git commit -m "0.1.0-1" && git push
```

`.SRCINFO` 是**生成物**，别手改：改了 PKGBUILD 就重新生成一次。

## 手动装（不打包）

```bash
cargo build --release --locked
install -Dm755 target/release/toolbox-hub ~/.local/bin/toolbox-hub
install -Dm644 packaging/toolbox-hub.1 ~/.local/share/man/man1/toolbox-hub.1
install -Dm644 completions/toolbox-hub.fish ~/.config/fish/completions/toolbox-hub.fish
```

## 构建前提

二进制**动态链接 libalpm**（`pacman` 包提供 `libalpm.so` 与 `libalpm.pc`），
所以只有 Arch 系发行版能编译 —— 这是有意的，工具箱里一半的动作本来就是
pacman / paru。
