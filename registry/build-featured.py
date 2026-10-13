#!/usr/bin/env python3
"""Build or verify the first-party featured Recipe index."""
import argparse
import hashlib
import json
import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent / "featured"
REQUIRED = {"id", "title", "description", "command", "params", "safety", "source", "tags", "args"}
# 粗粒度红灯：目的是拦住「把明显危险的命令标成 safe」。
# 真正的闸门在 src/ecosystem.rs 的 dangerous()（按段切词、看程序名与短选项），
# 比这张表严；这里宁可多报，不可漏到「已经发布」这一步。两边不一致时以 Rust 为准 ——
# CI 会同时跑本脚本和 `toolbox-hub recipe check`。
DANGEROUS = (
    "rm -rf", "rm -fr", "rm -r -f", "rm --recursive", "mkfs", "dd if=", "dd of=",
    "shutdown", "reboot", "poweroff", "halt", "wipefs", "fdisk", "parted", "sgdisk",
    "shred", "mkswap", "userdel", "groupdel", "sudo ", "doas ", "| sh", "| bash",
    "| python", ":(){", "chmod -r", "chmod -R", "chown -r", "chown -R",
)

def digest(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()

def entries():
    rows = []
    for path in sorted(ROOT.glob("*.json")):
        if path.name == "index.json":
            continue
        data = json.loads(path.read_text(encoding="utf-8"))
        missing = REQUIRED - data.keys()
        if missing:
            raise ValueError(f"{path.name}: 缺少字段 {sorted(missing)}")
        if data["id"] != path.stem:
            raise ValueError(f"{path.name}: id 必须与文件名一致")
        if data["safety"] not in {"safe", "caution", "destructive"}:
            raise ValueError(f"{path.name}: safety 不合法")
        command = " ".join([data["command"], *data.get("args", [])]).lower()
        if any(token in command for token in DANGEROUS) and data["safety"] != "destructive":
            raise ValueError(f"{path.name}: 危险命令不得标记为 {data['safety']}")
        params = {p["name"] for p in data["params"]}
        for key in re.findall(r"\{([a-zA-Z0-9_-]+)\}", command):
            if key.replace("-", "_") not in params:
                raise ValueError(f"{path.name}: 模板参数 {key} 未声明")
        rows.append({"id": data["id"], "file": path.name, "sha256": digest(path)})
    if len(rows) < 30:
        raise ValueError(f"featured Recipe 数量不足 30：当前 {len(rows)}")
    if len({r["id"] for r in rows}) != len(rows):
        raise ValueError("featured Recipe id 重复")
    return rows

def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--build", action="store_true")
    parser.add_argument("--check", action="store_true")
    args = parser.parse_args()
    index = ROOT / "index.json"
    want = {"schema_version": 1, "recipes": entries()}
    if args.build:
        index.write_text(json.dumps(want, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")
        print(f"featured index generated: {len(want['recipes'])} recipes")
        return 0
    got = json.loads(index.read_text(encoding="utf-8"))
    if got != want:
        print("featured/index.json 与 Recipe 文件不一致；运行 python3 registry/build-featured.py --build", file=sys.stderr)
        return 1
    print(f"featured registry OK: {len(want['recipes'])} recipes; SHA-256 verified")
    return 0

if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except (OSError, ValueError, json.JSONDecodeError) as error:
        print(f"featured registry error: {error}", file=sys.stderr)
        raise SystemExit(1)
