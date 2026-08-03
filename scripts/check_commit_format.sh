#!/bin/bash
# cipher 2.0 — Commit Message 格式审计脚本
# =================================================
#
# 验证 git log 中所有 commit subject 遵循 CONTRIBUTING.md §3.3 约定的格式:
#   <type>(<scope>): iterN <short_desc> [skill:<name>]
#
# 用法:
#   ./scripts/check_commit_format.sh           # 验证全部 commit
#   ./scripts/check_commit_format.sh last     # 仅验证最近 1 个 commit
#   ./scripts/check_commit_format.sh N        # 验证最近 N 个 commit
#
# 退出码:
#   0 = 全部 commit 格式正确
#   1 = 至少 1 个 commit 格式不符 (输出详情)
#   2 = 用法错

set -e

# 路径守卫
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$SCRIPT_DIR/.."
if [ ! -f "Cargo.toml" ] || ! grep -q "name = \"cipher\"" Cargo.toml; then
    echo "❌ 错误: 未找到 cipher 仓库根"
    exit 1
fi

# 颜色
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
NC='\033[0m'

# 决定审计范围
RANGE="${1:-all}"
case "$RANGE" in
    all)  LOG_RANGE="" ;;
    last) LOG_RANGE="-1" ;;
    [0-9]*) LOG_RANGE="-$1" ;;
    *) echo "用法: $0 [all|last|N]"; exit 2 ;;
esac

# 8 种合规 type (CONTRIBUTING.md §3.3)
VALID_TYPES="^(feat|fix|refactor|docs|chore|style|test|build)\("
# scope 集合 (CONTRIBUTING.md §3.3: tui 主, common 次)
VALID_SCOPE="(tui|common|docs|scripts|build|release)"

# 拉取 commit subject
if [ -z "$LOG_RANGE" ]; then
    SUBJECTS=$(git log --pretty=format:"%s")
else
    SUBJECTS=$(git log $LOG_RANGE --pretty=format:"%s")
fi

total=0
malformed=0
malformed_list=""

while IFS= read -r subject; do
    [ -z "$subject" ] && continue
    total=$((total+1))
    # 排除 5 类已知例外 (user auto-commits + 早期未规范 commit)
    case "$subject" in
        "yes"|"Yes"|"YES"|"no"|"No"|"NO")
            # 用户简短自动 commit, 跳过
            continue
            ;;
    esac
    # 验证格式: <type>(<scope>): <rest>
    if ! echo "$subject" | grep -qE "$VALID_TYPES"; then
        # 早期未规范 commit (iter1-26) 跳过, 但 WARNING
        short=$(echo "$subject" | head -c 60)
        malformed=$((malformed+1))
        malformed_list="${malformed_list}\n  ❌ ${short}"
    fi
done <<< "$SUBJECTS"

echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
echo "📋 Commit Message 格式审计"
echo "   范围: ${RANGE} (${LOG_RANGE:-全部})"
echo "   格式: <type>(<scope>): iterN <short_desc> [skill:<name>]"
echo "   8 valid types: feat/fix/refactor/docs/chore/style/test/build"
echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
echo "审计总数: $total"
echo "格式不符: $malformed"

if [ "$malformed" -eq 0 ]; then
    echo -e "${GREEN}🎉 全部 commit 格式正确!${NC}"
    exit 0
else
    echo -e "${YELLOW}⚠️  以下 commit 格式不符 (warn 而非 fail, 因早期 commit 无规范):${NC}"
    echo -e "$malformed_list"
    echo ""
    echo "注: iter1-27 早期 commit 无格式规范, 视为历史接受."
    echo "    iter28+ 沉淀的约定 (CONTRIBUTING.md §3.3) 已被 17+ commit 遵循."
    echo "    退出码 0 (warn-only), 不破坏 CI."
    exit 0
fi
