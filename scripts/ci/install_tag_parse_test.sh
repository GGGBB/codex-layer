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
        if printf '%s\n' "$real" | grep -Eq '^v[0-9]+\.[0-9]+\.[0-9]+$' >/dev/null; then
            echo "  ok   latest tag 形态合规：${real}"
        else
            echo "  FAIL latest tag 取不到或形态不合规：[${real}]" >&2
            fail=1
        fi
    fi
fi

# 管道判据写法检查：install.sh 里不得出现「产生者 | grep -q…」。
# grep -q 命中即退出并关闭管道，产生者后续写入拿到 EPIPE 后非零退出，
# 在 set -o pipefail 下「已经匹配」会被判成整条管道失败。发布链路实测踩过：
# 打包步骤的 `tar -tzf | grep -qx` 把成员齐全的正确产物判死
# （tar: stdout: write error）。判据先自测再判真值，防它退化成恒零。
echo "管道判据写法（不得出现 产生者 | grep -q）"
anti='[[:space:]][|][[:space:]]*grep[[:space:]]+-[A-Za-z]*q'
if printf 'x | grep -qx pat\n' | grep -Eq "$anti"; then
    if printf 'printf "%s" "$m" | grep -x pat >/dev/null\n' | grep -Eq "$anti"; then
        echo "  FAIL 反模式正则写宽了（合规写法也被判中），检查无鉴别力" >&2
        fail=1
    else
        echo "  ok   反模式正则判据自测（正/负各一条）"
    fi
else
    echo "  FAIL 反模式正则判据自测失败（恒零，检查形同虚设）" >&2
    fail=1
fi
# 只判可执行行：注释里会点名这个反模式（解释为什么不能用它），命中注释不是缺陷。
# 这条豁免自己也要有鉴别力，所以先拿一行真代码确认反模式抓得住。
# 扫描范围：install.sh 与发布链路 release.yml 都判——那条判据两半在两侧，
# 只守一侧迟早单边漂移。
if printf '    if tar -tzf f | grep -qx "$m"; then\n' | grep -Eq "$anti"; then
    for target in install.sh .github/workflows/release.yml; do
        hits="$(sed 's/^[[:space:]]*//' "$REPO_ROOT/$target" | grep -v '^#' | grep -En "$anti" || true)"
        if [ -n "$hits" ]; then
            echo "  FAIL $target 的可执行行里有「产生者 | grep -q」写法：" >&2
            printf '%s\n' "$hits" | sed 's/^/      /' >&2
            fail=1
        else
            echo "  ok   $target 可执行行无该写法"
        fi
    done
else
    echo "  FAIL 反模式正则抓不住真实代码写法，这条检查不可信" >&2
    fail=1
fi

# 语义对照（有 python3 才跑）：证明「产生者 | grep -q」在 pipefail 下会把已匹配
# 判成失败。产生者用 python3 而不是 tar——BSD tar 静默忽略 EPIPE（macOS 本机因此
# 复现不出来），Python 与 GNU tar 一样把写失败当错误抛出，CI 上炸的正是后者。
if command -v python3 >/dev/null 2>&1; then
    probe_dir="$(mktemp -d "${TMPDIR:-/tmp}/codex-layer-pipeprobe-XXXXXX")"
    cat > "$probe_dir/unsafe.sh" <<'PROBE'
set -euo pipefail
if python3 -c 'for i in range(200000): print("line%d" % i)' | grep -qx line0; then
    echo MATCHED
else
    echo JUDGED_ABSENT
fi
PROBE
    cat > "$probe_dir/safe.sh" <<'PROBE'
set -euo pipefail
members="$(python3 -c 'for i in range(200000): print("line%d" % i)')"
if printf '%s\n' "$members" | grep -x line0 >/dev/null; then
    echo MATCHED
else
    echo JUDGED_ABSENT
fi
PROBE
    check "反模式在 pipefail 下确实误判" "JUDGED_ABSENT" "$(bash "$probe_dir/unsafe.sh" 2>/dev/null || true)"
    check "安全写法判为匹配" "MATCHED" "$(bash "$probe_dir/safe.sh" 2>/dev/null || true)"
    rm -f "$probe_dir/unsafe.sh" "$probe_dir/safe.sh"
    rmdir "$probe_dir" 2>/dev/null || true
else
    echo "  skip 需要 python3 的语义对照"
fi

if [ "$fail" -ne 0 ]; then
    echo "latest tag 解析测试失败" >&2
    exit 1
fi
echo "latest tag 解析测试全绿"
