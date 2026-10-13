#!/usr/bin/env bash
# 校验一个发行产物：SHA-256 对得上 + minisign 签名有效。
#
#   bash packaging/verify-release.sh toolbox-hub-1.2.0-x86_64-unknown-linux-gnu.tar.gz
#   bash packaging/verify-release.sh --pub /path/to/minisign.pub <产物…>
#
# 为什么两件事都要做：SHA-256 只能证明「传输没坏」，它自己没有任何权威性 ——
# 谁能换掉产物，就能顺手换掉校验和。签名才把「这是维护者签的」这件事钉死，
# 所以一个产物**没有签名就是不合格**，不是「可选的额外步骤」。
#
# 退出码：0 全部通过；1 校验失败（**不要**安装这个产物）；2 用法/环境问题。
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
pub="$here/minisign.pub"

args=()
while [ $# -gt 0 ]; do
    case "$1" in
        --pub)
            [ $# -ge 2 ] || { echo "verify-release: --pub 后面要跟公钥文件" >&2; exit 2; }
            pub="$2"
            shift 2
            ;;
        -h | --help)
            sed -n '2,12p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'
            exit 0
            ;;
        --)
            shift
            while [ $# -gt 0 ]; do args+=("$1"); shift; done
            ;;
        -*)
            echo "verify-release: 不认识的选项：$1" >&2
            exit 2
            ;;
        *)
            args+=("$1")
            shift
            ;;
    esac
done

if [ "${#args[@]}" -eq 0 ]; then
    echo "verify-release: 至少给一个产物路径" >&2
    exit 2
fi

if ! command -v minisign >/dev/null 2>&1; then
    echo "verify-release: 没装 minisign。Arch: pacman -S minisign；Debian/Ubuntu: apt install minisign" >&2
    exit 2
fi

if [ ! -f "$pub" ]; then
    cat >&2 <<EOF
verify-release: 找不到公钥：$pub

维护者还没把签名公钥提交到仓库里（见 packaging/release-signing.md）。
在有公钥之前，任何产物都**无法**被验证 —— 请不要把「验不了」当成「验过了」。
EOF
    exit 2
fi

failed=0
for file in "${args[@]}"; do
    [ -f "$file" ] || { echo "verify-release: 没有这个文件：$file" >&2; failed=1; continue; }

    # 1) 校验和：有 .sha256 就核对，没有就只提示（签名才是必需的那一半）。
    #
    # 只比对摘要本身，不照着文件里记的路径去找文件：库里发出去的校验和写的是纯文件名，
    # 但历史版本（以及别人手工生成的）可能带 `dist/` 前缀 —— 那种情况下
    # `sha256sum -c` 会报「文件不存在」，看起来像产物坏了，其实只是路径记错了。
    sha_file="${file}.sha256"
    if [ -f "$sha_file" ]; then
        expected="$(awk 'NF { print $1; exit }' "$sha_file")"
        actual="$(sha256sum "$file" | awk '{ print $1 }')"
        if [ -n "$expected" ] && [ "$expected" = "$actual" ]; then
            echo "✓ SHA-256 对得上：$file"
        else
            echo "✗ SHA-256 不匹配：$file" >&2
            echo "  期望 $expected" >&2
            echo "  实际 $actual" >&2
            failed=1
            continue
        fi
    else
        echo "· 没有 $sha_file，跳过校验和（签名仍然要验）"
    fi

    # 2) 签名：缺签名 = 不合格，不是「跳过」。
    sig="${file}.minisig"
    if [ ! -f "$sig" ]; then
        echo "✗ 没有签名文件：$sig —— 未签名的产物一律不装" >&2
        failed=1
        continue
    fi
    if minisign -V -p "$pub" -m "$file" -x "$sig" >/dev/null; then
        echo "✓ minisign 签名有效：$file"
    else
        echo "✗ minisign 签名无效：$file" >&2
        failed=1
    fi
done

[ "$failed" -eq 0 ] || {
    echo "verify-release: 有产物没通过校验 —— 不要安装它，也不要把它转发给别人。" >&2
    exit 1
}
echo "verify-release: 全部通过"
