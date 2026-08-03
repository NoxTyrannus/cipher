#!/usr/bin/env bash
set -euo pipefail

REPO_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
BIN_NAME="cipher"
RELEASE_BIN="$REPO_DIR/target/release/$BIN_NAME"
MIN_RUST="1.96.0"

echo "== Cipher 安装脚本 =="

check_rust() {
    if ! command -v rustc >/dev/null 2>&1; then
        echo "错误: 未找到 Rust 工具链。请先安装: https://rustup.rs"
        exit 1
    fi
    local ver
    ver=$(rustc --version | awk '{print $2}')
    if ! command -v rustup >/dev/null 2>&1; then
        echo "警告: 未找到 rustup。当前 rustc 版本: $ver"
        return
    fi
    # 按 rust-toolchain.toml 自动安装/切换
    if [ -f "$REPO_DIR/rust-toolchain.toml" ]; then
        rustup show >/dev/null 2>&1 || true
    fi
    echo "Rust 工具链: $(rustc --version)"
}

build() {
    echo "== 构建 release 版本 (可能需要几分钟) =="
    (cd "$REPO_DIR" && cargo build --release)
    if [ ! -x "$RELEASE_BIN" ]; then
        echo "错误: 构建失败，未找到 $RELEASE_BIN"
        exit 1
    fi
    echo "构建完成: $RELEASE_BIN"
}

install_to_path() {
    local dest
    if [ -d "$HOME/.local/bin" ] && [ -w "$HOME/.local/bin" ]; then
        dest="$HOME/.local/bin"
    else
        dest="/usr/local/bin"
        if [ ! -w "$dest" ]; then
            echo "== 将二进制复制到 $dest (需要 sudo) =="
            sudo cp "$RELEASE_BIN" "$dest/$BIN_NAME"
            echo "已安装到 $dest/$BIN_NAME"
            return
        fi
    fi
    cp "$RELEASE_BIN" "$dest/$BIN_NAME"
    echo "已安装到 $dest/$BIN_NAME"
    case ":$PATH:" in
        *":$dest:"*) ;;
        *) echo "提示: 将 $dest 加入 PATH 后可直接运行 $BIN_NAME" ;;
    esac
}

main() {
    check_rust
    build
    if [ "${1:-}" = "--no-install" ]; then
        echo "== 跳过安装 (--no-install)。运行: $RELEASE_BIN =="
        exit 0
    fi
    install_to_path
    echo
    echo "== 安装完成 =="
    echo "首次使用请运行: $BIN_NAME setup"
    echo "然后启动: $BIN_NAME"
}

main "$@"
