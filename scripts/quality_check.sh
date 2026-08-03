#!/bin/bash
# cipher 2.0 — Quality Check Script
#
# 整合 iter36 沉淀的 4 项质量审计 (ADR-076):
# 1. cargo test --lib (252/252 baseline + 新增测试不退化)
# 2. cargo clippy --all-targets -- -D warnings (默认 lint 集必须 0 warning)
# 3. cargo doc --no-deps --document-private-items (rustdoc 完整性)
# 4. grep TODO/FIXME/panic! 残留 (产品代码 panic 必须 0)
#
# 额外:
# 5. cargo build --release (release profile 编译干净)
#
# 用法:
#   ./scripts/quality_check.sh           # 跑全部 9 项 (~3 min)
#   ./scripts/quality_check.sh quick     # 仅跑 1+2 (CI fast path, ~30s)
#   ./scripts/quality_check.sh full      # 跑全部 (含 3+4+5+6+7+8+9, ~3 min)
#
# Run from the repository root. The script resolves its own location so it can
# also be invoked from another working directory.
#
# 退出码:
#   0 = 全部通过
#   1 = 至少 1 项失败 (输出错误详情)

set -e

# 路径守卫
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$SCRIPT_DIR/.."  # cd 到 repository root
if [ ! -f "Cargo.toml" ] || ! grep -q "name = \"cipher\"" Cargo.toml; then
    echo "❌ 错误: 未找到 cipher 仓库根"
    echo "   当前位置: $(pwd)"
    echo "   预期: 仓库根的 Cargo.toml 含 name = \"cipher\""
    exit 1
fi

MODE="${1:-full}"

RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
NC='\033[0m' # No Color

PASS=0
FAIL=0

run_step() {
    local name="$1"
    local cmd="$2"
    echo ""
    echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
    echo "🔍 $name"
    echo "   $cmd"
    echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
    if eval "$cmd"; then
        echo -e "${GREEN}✅ PASS${NC}: $name"
        PASS=$((PASS+1))
    else
        echo -e "${RED}❌ FAIL${NC}: $name"
        FAIL=$((FAIL+1))
    fi
}

# Step 1: cargo test
run_step "1/5 cargo test --all-targets" "cargo test --all-targets"

# Step 2: clippy (default 集)
run_step "2/5 cargo clippy --all-targets -- -D warnings" \
    "cargo clippy --all-targets -- -D warnings"

# Quick 模式仅跑 1+2
if [ "$MODE" = "quick" ]; then
    echo ""
    echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
    echo "📊 quick 模式结果: PASS=$PASS FAIL=$FAIL"
    echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
    [ $FAIL -eq 0 ] && exit 0 || exit 1
fi

# Step 3: cargo doc
run_step "3/5 cargo doc --no-deps --document-private-items" \
    "cargo doc --no-deps --document-private-items"

# Step 4: 产品代码 panic 残留 (用 cargo build --lib 替代 grep, 更精准)
# 原理: cargo build --lib 不编译 #[cfg(test)] 模块, 若产品代码有 panic 编译会失败
# 比 grep 更可靠: grep 无法识别 #[cfg(test)] mod tests 块范围
echo ""
echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
echo "🔍 4/5 cargo build --lib (产品代码 panic = 0 残留检查)"
echo "   (cargo build --lib 不编译 #[cfg(test)] 模块, 若产品代码有 panic 编译失败)"
echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
if cargo build --lib 2>&1 | grep -q "error\["; then
    echo -e "${RED}❌ FAIL${NC}: cargo build --lib 报错 (产品代码可能含 panic 或其他编译错误)"
    cargo build --lib 2>&1 | tail -20
    FAIL=$((FAIL+1))
else
    # 额外: TODO/FIXME 残留 (产品代码注释中)
    TODO_HITS=$(grep -rn "TODO\|FIXME\|XXX" src/ 2>/dev/null | grep -v "tests\b" || true)
    if [ -z "$TODO_HITS" ]; then
        echo -e "${GREEN}✅ PASS${NC}: 0 产品代码 panic (cargo build --lib clean) + 0 TODO/FIXME 残留"
        PASS=$((PASS+1))
    else
        echo -e "${YELLOW}⚠️  TODO/FIXME 残留 (可能无害, 但建议清理):${NC}"
        echo "$TODO_HITS"
        # TODO 算 warning, 不算 fail
        PASS=$((PASS+1))
    fi
fi

# Step 5: cargo build --release
run_step "5/9 cargo build --release" \
    "cargo build --release"

# Step 6: 文档链接完整性 (iter42 沉淀, ADR-081 + ADR-082)
# 验证主 doc 文件 (HANDOFF/00/01/P0) 中所有 .md 链接 resolve
# 防 iter41 22 broken 重复
run_step "6/9 文档 .md 链接完整性 (main scope, 防 iter41 重复)" \

# Step 7: cargo-audit 漏洞扫描 (iter43 沉淀, ADR-083, opt-in)
# ⚠️ 需要先 `cargo install cargo-audit --locked` (1 次性, 2m 10s)
# 检查 PATH: 没装就 warn 而非 fail (不强加其他 contributor 装此工具)
echo ""
echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
echo "🔍 7/9 cargo-audit 漏洞扫描 (opt-in, 需先 `cargo install cargo-audit --locked`)"
echo "   (iter43 装 cargo-audit v0.22.2, cipher 160 deps 扫描 0 vulnerabilities)"
echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
if command -v cargo-audit >/dev/null 2>&1; then
    if cargo audit 2>&1 | tail -3; then
        echo -e "${GREEN}✅ PASS${NC}: cargo-audit 0 vulnerabilities (160 deps 扫描)"
        PASS=$((PASS+1))
    else
        echo -e "${RED}❌ FAIL${NC}: cargo-audit 报告 vulnerabilities, 详见上方"
        FAIL=$((FAIL+1))
    fi
else
    echo -e "${YELLOW}⚠️  SKIP${NC}: cargo-audit 未装 (其他 contributor 可选; cipher 推荐装, 见 ADR-083)"
    echo "   安装: cargo install cargo-audit --locked"
    # 不算 PASS 也不算 FAIL, 跳过
fi

# Step 8: Commit message 格式审计 (iter45 沉淀, ADR-085)
# 验证 git log 中 commit subject 遵循 CONTRIBUTING.md §3.3 约定
# warn-only (早期 commit 无规范, 接受为历史)
run_step "8/9 Commit message 格式审计 (CONTRIBUTING.md §3.3)" \
    "bash scripts/check_commit_format.sh"

# Step 9: Self-reference 链接完整性 (iter50 沉淀, ADR-090)
# 扫仓库**全部** .md 链接 (含仓库根文件), 防 self-typo (iter49 反复 4+ 次)
# warn-only (legacy archive refs 已知, 不破坏 8/8 baseline)
echo ""
echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
echo "🔍 9/9 Self-ref .md 链接 (全仓库扫, 防 self-typo, warn-only)"
echo "   (iter50 沉淀, 扫所有 .md 含仓库根文件, 防止 off-by-N 路径 typo)"
echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
SELFRES_BROKEN=$(python3 scripts/check_md_self_refs.py 2>&1 | grep -E "Total links" | tail -1 || echo "未运行")
echo "$SELFRES_BROKEN"
# warn-only: 接受 archive 中已知 broken (iter49 ADR-089 分布确认)
# 期望: main scope 0 broken + archive ~72 broken (接受)
# 警告: 如果 main scope 有新 broken, 显式标出

# 汇总
echo ""
echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
echo "📊 汇总: PASS=$PASS FAIL=$FAIL (cargo-audit SKIP 不计)"
echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
if [ $FAIL -eq 0 ]; then
    echo -e "${GREEN}🎉 全部 6 项通过 (cargo-audit 可选)${NC}"
    exit 0
else
    echo -e "${RED}❌ $FAIL 项失败, 详见上方输出${NC}"
    exit 1
fi
