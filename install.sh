#!/usr/bin/env bash
# codex-layer 安装脚本（ollama install.sh 式）：**先试下载预构建产物**，
# 命中则校验 sha256 后落盘；未命中 / 校验失败 / 无网络时**回落到源码构建**，
# 绝不因下载失败而半途而废（issue #16 验收）。
#
# 纪律：
# - 脚本内禁止 sudo：PREFIX 必须用户可写，写不进去直接报错给出路。
# - PREFIX 默认 $HOME/.local/bin，可用 env 覆盖。
# - 不动用户 shell rc；PATH 未含 PREFIX 时只打印建议行。
# - 下载一律 curl 到临时文件再校验，**绝不** curl | sh 之类管道执行。
# - 校验和不匹配 = 拒绝该产物并回落源码构建，不装未验证的字节。
#
# env：
#   PREFIX                  安装目录（默认 $HOME/.local/bin）
#   VERSION                 要安装的版本（默认取 latest release 的 tag）
#   REPO                    产物所在仓库（默认 GGGBB/codex-layer）
#   CODEX_LAYER_DOWNLOAD_BASE  产物下载基址（默认 github 的 release 下载页；
#                           可指向镜像或本地目录，便于离线验证校验逻辑）
#   CODEX_LAYER_NO_DOWNLOAD=1  跳过下载分支，直接源码构建（离线 / 复现用）
set -euo pipefail

PREFIX="${PREFIX:-$HOME/.local/bin}"
REPO_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
BIN_NAME="codex-layer"
REPO="${REPO:-GGGBB/codex-layer}"
DOWNLOADED=0

# 与 .github/workflows/release.yml 的产物命名严格对齐：
#   codex-layer-<tag>-<target 三元组>.tar.gz
target_triple() {
    local os arch
    case "$(uname -s)" in
        Darwin) os="apple-darwin" ;;
        Linux)  os="unknown-linux-gnu" ;;
        *) return 1 ;;
    esac
    case "$(uname -m)" in
        arm64 | aarch64) arch="aarch64" ;;
        x86_64 | amd64)  arch="x86_64" ;;
        *) return 1 ;;
    esac
    # 产物只发这两个组合：macOS 只 arm64、Linux 只 x86_64。
    # 对不上的形态（如 Intel mac）判「无产物」，回落源码构建。
    if [ "$os" = "apple-darwin" ] && [ "$arch" != "aarch64" ]; then return 1; fi
    printf '%s-%s\n' "$arch" "$os"
}

expected_sum() {
    local manifest="$1" name="$2" line
    line="$(awk -v n="$name" '$2 == n {print $1; exit}' "$manifest" 2>/dev/null || true)"
    [ -n "$line" ] || return 1
    printf '%s\n' "$line"
}

actual_sum() {
    local file="$1"
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$file" | awk '{print $1}'
    elif command -v shasum >/dev/null 2>&1; then
        shasum -a 256 "$file" | awk '{print $1}'
    else
        return 1
    fi
}

# 原子落盘：常驻实例正跑旧二进制时直接覆盖会撞 macOS 的 ETXTBSY/签名问题，
# 先拷临时文件再 mv，让旧进程继续持旧 inode、新进程拿新的。
install_binary() {
    local src="$1" dest="$2"
    if ! mkdir -p "$(dirname "$dest")" 2>/dev/null; then
        echo "错误：无法创建安装目录 $(dirname "$dest")（权限不足）。" >&2
        echo "本脚本不使用 sudo；请改用用户可写的 PREFIX，例如：PREFIX=\$HOME/.local/bin $0" >&2
        return 1
    fi
    local tmpfile="$dest.new.$$"
    cp -f "$src" "$tmpfile" || { rm -f "$tmpfile"; return 1; }
    chmod 0755 "$tmpfile"
    mv -f "$tmpfile" "$dest"
}

# 返回 0 = 已装好一个**已验证**的产物；非零 = 调用方回落源码构建。
try_download() {
    if [ "${CODEX_LAYER_NO_DOWNLOAD:-0}" = "1" ]; then
        echo "==> 按 CODEX_LAYER_NO_DOWNLOAD=1 跳过下载分支"
        return 1
    fi
    if ! command -v curl >/dev/null 2>&1 || ! command -v tar >/dev/null 2>&1; then
        echo "==> 缺少 curl 或 tar，回落源码构建"
        return 1
    fi
    local triple tag url manifest tmpdir want got member
    if ! triple="$(target_triple)"; then
        echo "==> 本机形态没有对应产物（只发 macOS arm64 与 Linux x86_64），回落源码构建"
        return 1
    fi
    tmpdir="$(mktemp -d "${TMPDIR:-/tmp}/codex-layer-install-XXXXXX")" || return 1
    # 本仓库纪律禁 rm -rf，因此这里**不解包**：用 tar -xzO 只取那一个成员，
    # 临时目录里最多三个已知文件，逐个删净后 rmdir 收尾。
    cleanup() {
        rm -f "$tmpdir/sha256sums.txt" "$tmpdir/$pkg" "$tmpdir/$BIN_NAME"
        rmdir "$tmpdir" 2>/dev/null || true
    }

    if [ -n "${VERSION:-}" ]; then
        tag="${VERSION#v}"
    else
        # 只读 tag 名；取不到就当未命中，交给源码构建
        tag="$(curl -fsSL --max-time 20 \
            "https://api.github.com/repos/${REPO}/releases/latest" 2>/dev/null \
            | tr ',{}" ' '\n\n\n\n\n' | sed -n 's/^tag_name://p' | head -1 || true)"
    fi
    [ -n "$tag" ] || { echo "==> 取不到版本号，回落源码构建"; return 1; }
    [ "${tag#v}" = "$tag" ] && tag="v$tag"

    pkg="${BIN_NAME}-${tag}-${triple}.tar.gz"
    url="${CODEX_LAYER_DOWNLOAD_BASE:-https://github.com/${REPO}/releases/download/${tag}}"
    url="${url%/}"
    manifest="$tmpdir/sha256sums.txt"

    echo "==> 尝试下载预构建产物：${tag} / ${triple}"
    if ! curl -fsSL --max-time 30 -o "$manifest" "$url/sha256sums.txt" 2>/dev/null; then
        echo "==> 取不到 sha256sums.txt（该 release 不带产物），回落源码构建"
        cleanup
        return 1
    fi
    if ! curl -fL --max-time 300 -o "$tmpdir/$pkg" "$url/$pkg" 2>/dev/null; then
        echo "==> 产物下载失败：$pkg，回落源码构建"
        cleanup
        return 1
    fi
    if ! want="$(expected_sum "$manifest" "$pkg")"; then
        echo "==> 校验清单里没有 $pkg 这一条，回落源码构建"
        cleanup
        return 1
    fi
    if ! got="$(actual_sum "$tmpdir/$pkg")"; then
        echo "==> 本机没有 sha256sum/shasum，无法验证产物，回落源码构建"
        cleanup
        return 1
    fi
    if [ "$want" != "$got" ]; then
        echo "==> 校验和不匹配，拒绝安装该产物（期望 ${want}，实得 ${got}）" >&2
        echo "    回落源码构建。若你没改动过本脚本与网络路径，这值得当作安全问题上报。" >&2
        cleanup
        return 1
    fi
    echo "==> sha256 校验通过：${got}"
    member="${BIN_NAME}-${tag}-${triple}/${BIN_NAME}"
    if ! tar -tzf "$tmpdir/$pkg" | grep -qx "$member"; then
        echo "==> 产物里没有预期成员 $member，回落源码构建"
        cleanup
        return 1
    fi
    if ! tar -xzOf "$tmpdir/$pkg" "$member" > "$tmpdir/$BIN_NAME"; then
        echo "==> 取成员失败，回落源码构建"
        cleanup
        return 1
    fi
    chmod 0755 "$tmpdir/$BIN_NAME"
    DEST="$PREFIX/$BIN_NAME"
    if ! install_binary "$tmpdir/$BIN_NAME" "$DEST"; then
        cleanup
        return 1
    fi
    cleanup
    echo "==> 已安装预构建产物：$("$DEST" --version)"
    DOWNLOADED=1
    return 0
}

if try_download; then
    :
else
    if ! command -v cargo >/dev/null 2>&1; then
        echo "错误：既没有可用的预构建产物，本机也没有 cargo 可来源码构建。" >&2
        echo "     先装 Rust 工具链（https://rustup.rs），或用 VERSION 指向一个带产物的 release。" >&2
        exit 1
    fi
    echo "==> cargo build --release（${REPO_DIR}）"
    cargo build --release --manifest-path "$REPO_DIR/Cargo.toml"

    SRC="$REPO_DIR/target/release/$BIN_NAME"
    if [ ! -x "$SRC" ]; then
        echo "错误：构建产物不存在或不可执行：$SRC" >&2
        exit 1
    fi
    DEST="$PREFIX/$BIN_NAME"
    install_binary "$SRC" "$DEST" || exit 1
    echo "==> 已安装（源码构建）：$("$DEST" --version)"
fi
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

if [ "${DOWNLOADED:-0}" = "1" ]; then
    echo "提示：预构建产物未做代码签名与公证。macOS 首次运行可能提示「无法验证"
    echo "      开发者」，按需在「系统设置 → 隐私与安全性」放行；"
    echo "      或直接改用源码构建：CODEX_LAYER_NO_DOWNLOAD=1 $0"
fi
