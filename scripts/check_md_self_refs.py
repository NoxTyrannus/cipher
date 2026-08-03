#!/usr/bin/env python3
"""
cipher 2.0 — Self-Reference 链接完整性脚本
================================================

iter49 (ADR-089) 沉淀: Claude 写 doc 时反复犯同 typo (e.g., check_doc-links vs check_doc_links.sh).
本脚本: 扫仓库内所有 .md 文件的链接, 包括到仓库根文件 (e.g., CONTRIBUTING.md) 的链接, 防止路径 off-by-N.

vs scripts/check_doc_links.sh (iter42):
- check_doc_links.sh: 默认 4 主 doc (main 模式), 验证 doc/ 内部链接
- check_md_self_refs.py (本脚本): 扫**所有** .md (含 ADR scope), 重点是**仓库根 + 跨域**链接 (CONTRIBUTING.md / CLAUDE.md / HANDOFF.md)

用法:
    python3 scripts/check_md_self_refs.py            # 扫 cipher 仓库所有 .md
    python3 scripts/check_md_self_refs.py docs/     # 扫指定子目录

退出码:
    0 = 全部链接 resolve
    1 = 至少 1 个 broken
"""
import re
import os
import sys
from pathlib import Path

# 仓库根级文件白名单 (允许 .md 链接到这些, 路径为 ../../)
ROOT_FILES = ["CONTRIBUTING.md", "CLAUDE.md", "Cargo.toml", "Cargo.lock", "rust-toolchain.toml", "README.md", "LICENSE"]

# Historical archives are retained for auditability but are outside active docs.
INACTIVE_DIRS = {".git", "target", "node_modules", "archive"}
ROOT_RELATIVE_PREFIXES = ("docs/", "archive/")
EXTERNAL_PREFIXES = ("http://", "https://", "file://", "~/")

# 仓库根目录绝对路径 (脚本运行时确定)
SCRIPT_DIR = Path(__file__).resolve().parent
REPO_ROOT = SCRIPT_DIR.parent  # cipher/


def check_file(md_path: str, base_dir: str = ".") -> list:
    """检查单个 .md 文件中的所有 .md 链接, 返回 broken 列表"""
    broken = []
    if not os.path.isfile(md_path):
        return [(md_path, "FILE_NOT_FOUND", "")]
    with open(md_path, encoding="utf-8") as f:
        text = f.read()
    # 匹配 markdown 链接 [text](path.md) — 排除 http(s):// 和纯锚点
    for m in re.finditer(r'\[([^\]]+)\]\(([^)]+\.md(?:[^)]*))\)', text):
        link = m.group(2)
        link_path = link.split('#')[0]
        if not link_path or link_path.startswith(EXTERNAL_PREFIXES):
            continue
        # iter52 同步 iter51 过滤: 纯文件名 (无 / . ../) 视为 code example
        if '/' not in link_path and not link_path.startswith('.'):
            continue
        # Links beginning with docs/ or archive/ are repository-root relative.
        if link_path.startswith(ROOT_RELATIVE_PREFIXES) or link_path in ROOT_FILES:
            target = REPO_ROOT / link_path
        else:
            target = Path(md_path).parent / link_path
        if not target.is_file():
            broken.append((md_path, link, target))
    return broken


def main():
    scope = sys.argv[1] if len(sys.argv) > 1 else "all"
    if scope != "all" and not os.path.isdir(scope):
        print(f"用法: {sys.argv[0]} [scope_dir|all]")
        sys.exit(2)

    # 收集 .md 文件
    if scope == "all":
        files = []
        for root, dirs, fnames in os.walk("."):
            dirs[:] = [d for d in dirs if d not in INACTIVE_DIRS]
            for fn in fnames:
                if fn.endswith(".md"):
                    files.append(os.path.join(root, fn))
        scope_label = f"仓库全部 .md ({len(files)} files)"
    else:
        files = []
        for root, _, fnames in os.walk(scope):
            for fn in fnames:
                if fn.endswith(".md"):
                    files.append(os.path.join(root, fn))
        scope_label = f"{scope} ({len(files)} files)"

    total_links = 0
    total_broken = 0
    broken_list = []
    for f in files:
        broken = check_file(f)
        for src, link, target in broken:
            total_broken += 1
            broken_list.append((src, link, target))
        with open(f, encoding="utf-8") as fh:
            total_links += len(list(re.finditer(r'\[[^\]]+\]\([^)]+\.md(?:[^)]*)\)', fh.read())))

    print("=" * 60)
    print(f"Self-Reference 链接完整性检查")
    print(f"Scope: {scope_label}")
    print(f"=" * 60)
    print(f"Total links: {total_links}, Broken: {total_broken}")
    if total_broken == 0:
        print("🎉 All .md links resolve correctly!")
        sys.exit(0)
    else:
        print(f"\n❌ {total_broken} broken links:")
        for src, link, target in broken_list:
            # 显示 short path
            short_src = src.replace("./", "", 1) if src.startswith("./") else src
            print(f"   {short_src}: ({link}) → {target}")
        sys.exit(1)


if __name__ == "__main__":
    main()
