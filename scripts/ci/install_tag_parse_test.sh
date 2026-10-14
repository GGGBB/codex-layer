#!/usr/bin/env bash
# latest tag 解析的正反对照测试（issue #16 的缺陷复现）。
#
# 立项依据：解析一度写成 `tr ',{}" ' '\n\n\n\n\n' | sed -n 's/^tag_name://p'`，
# tr 的字符集含冒号，把 "tag_name":"vX.Y.Z" 拆成两个孤立行，判据恒空——
# 默认路径静默回落源码构建，特性形同虚设。四场景手测全带 VERSION，撞不到这条分支。
# 因此这里必须带**正判据**（真能取出值）与**旧写法恒空的对照**，防止再退回去。
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/../.." && pwd)"

# 只取函数定义段，不执行脚本主体（主体会真下载/构建）。
extractor="$(sed -n '/^latest_tag_from_json() {/,/^}/p' "$REPO_ROOT/install.sh")"
if [ -z "$extractor" ]; then
    echo "FAIL：install.sh 里找不到 latest_tag_from_json 函数" >&2
    exit 1
fi
eval "$extractor"

fail=0
check() {
    local name="$1" want="$2" got="$3"
    if [ "$want" = "$got" ]; then
        echo "  ok   ${name}"
    else
        echo "  FAIL ${name}：期望 [${want}]，实得 [${got}]" >&2
        fail=1
    fi
}

echo "正判据（必须真取到值，防恒空）"
check "典型响应" "v9.9.9" \
    "$(printf '%s' '{"url":"https://x/releases/1","tag_name":"v9.9.9","name":"n"}' | latest_tag_from_json)"
check "键序不同" "v0.3.1" \
    "$(printf '%s' '{"name":"x","tag_name":"v0.3.1","assets":[]}' | latest_tag_from_json)"
check "带空白与换行" "v1.2.3" \
    "$(printf '{\n  "tag_name" : "v1.2.3",\n  "draft": false\n}\n' | latest_tag_from_json)"
check "多键只取首个" "v1.0.0" \
    "$(printf '%s' '{"tag_name":"v1.0.0","target_commitish":"main"}' | latest_tag_from_json)"

echo "负判据（不该产出的东西）"
check "无 tag_name 键" "" \
    "$(printf '%s' '{"message":"Not Found"}' | latest_tag_from_json)"
check "空输入" "" \
    "$(printf '' | latest_tag_from_json || true)"

echo "旧写法对照（同一条输入，旧管道必须恒空——证明测试有鉴别力）"
old="$(printf '%s' '{"url":"https://x","tag_name":"v9.9.9","name":"n"}' \
    | tr ',{}" ' '\n\n\n\n\n' | sed -n 's/^tag_name://p' | head -1 || true)"
check "旧管道恒空" "" "$old"

# 真实 latest 路径（不带 VERSION 的默认分支就是这个解析）：只断言取到的 tag
# 非空且形如 vX.Y.Z，不断言具体值——否则下次发版会把测试改红。
# 用 --network 显式开启，离线环境不会假红。
if [ "${1:-}" = "--network" ]; then
    echo "真实 latest（只断言形态）"
    if ! command -v curl >/dev/null 2>&1; then
        echo "  FAIL 指定了 --network 但本机没有 curl" >&2
        fail=1
    else
        real="$(curl -fsSL --max-time 20 \
            "https://api.github.com/repos/GGGBB/codex-layer/releases/latest" 2>/dev/null \
            | latest_tag_from_json || true)"
        if printf '%s' "$real" | grep -qE '^v[0-9]+\.[0-9]+\.[0-9]+$'; then
            echo "  ok   latest tag 形态合规：${real}"
        else
            echo "  FAIL latest tag 取不到或形态不合规：[${real}]" >&2
            fail=1
        fi
    fi
fi

if [ "$fail" -ne 0 ]; then
    echo "latest tag 解析测试失败" >&2
    exit 1
fi
echo "latest tag 解析测试全绿"
