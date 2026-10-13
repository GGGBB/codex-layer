#!/usr/bin/env bash
# codex-layer 安装脚本（ollama install.sh 式）：cargo build --release →
# 拷二进制到 PREFIX → 打印下一步指引。
#
# 纪律（AGENTS.md / 任务书）：
# - 脚本内禁止 sudo：PREFIX 必须用户可写，写不进去直接报错给出路。
# - PREFIX 默认 $HOME/.local/bin，可用 env 覆盖：PREFIX=/usr/local sudo=… 不归本脚本管。
# - 不动用户 shell rc；PATH 未含 PREFIX 时只打印建议行。
set -euo pipefail

PREFIX="${PREFIX:-$HOME/.local/bin}"
REPO_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
BIN_NAME="codex-layer"

echo "==> cargo build --release（${REPO_DIR}）"
cargo build --release --manifest-path "$REPO_DIR/Cargo.toml"

SRC="$REPO_DIR/target/release/$BIN_NAME"
if [ ! -x "$SRC" ]; then
    echo "错误：构建产物不存在或不可执行：$SRC" >&2
    exit 1
fi

if ! mkdir -p "$PREFIX" 2>/dev/null; then
    echo "错误：无法创建安装目录 ${PREFIX}（权限不足）。" >&2
    echo "本脚本不使用 sudo；请改用用户可写的 PREFIX，例如：PREFIX=\$HOME/.local/bin $0" >&2
    exit 1
fi

DEST="$PREFIX/$BIN_NAME"
# 先拷临时文件再原子 mv：常驻实例正在跑旧二进制时，直接覆盖会撞
# macOS 的 ETXTBSY/签名问题；rename 让旧进程继续持旧 inode，新进程拿新的。
TMP="$DEST.new.$$"
cp -f "$SRC" "$TMP"
chmod 0755 "$TMP"
mv -f "$TMP" "$DEST"

echo "==> 已安装：$("$DEST" --version)"
echo
echo "下一步："
if command -v "$BIN_NAME" >/dev/null 2>&1 && [ "$(command -v "$BIN_NAME")" != "$DEST" ]; then
    echo "  注意：PATH 里已存在另一个 ${BIN_NAME}（$(command -v "$BIN_NAME")），"
    echo "        本 PATH 段优先于新安装，请调整 PATH 或卸载旧副本。"
fi
case ":$PATH:" in
    *":$PREFIX:"*) ;;
    *)
        echo "  1. 把 $PREFIX 加入 PATH（本脚本不改你的 shell rc），例如 zsh："
        echo "       echo 'export PATH=\"$PREFIX:\$PATH\"' >> ~/.zshrc && exec zsh"
        ;;
esac
echo "  2. 准备配置（默认查找 ~/.codex-layer/codex-layer.toml）："
echo "       mkdir -p ~/.codex-layer"
echo "       cp \"$REPO_DIR/codex-layer.toml.example\" ~/.codex-layer/codex-layer.toml"
echo "  3. 前台试跑：$BIN_NAME --config ~/.codex-layer/codex-layer.toml"
echo "  4. 常驻（macOS LaunchAgent）：$BIN_NAME start && $BIN_NAME status"
