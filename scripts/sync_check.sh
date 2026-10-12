#!/usr/bin/env bash
# sync_check.sh：输出参照实现（BigPizzaV3/CodexPlusPlus 分支
# personal/stabilizer-agent-payload）中移植涉及文件自同步基线以来的提交列表，
# 供人工判断是否需要向本仓库同步。基线登记见 docs/SYNC_BASELINE.md。
#
# 用法：
#   scripts/sync_check.sh <本地 codexplusplus 仓库路径> [基线 commit]
set -euo pipefail

BASELINE_DEFAULT="11eb685"
BRANCH="personal/stabilizer-agent-payload"

# 参照仓库文件 -> 本仓库文件
FILES=(
  "crates/codex-plus-core/src/protocol_proxy.rs|src/protocol_proxy.rs"
  "crates/codex-plus-core/src/compaction_align.rs|src/compaction_align.rs"
  "crates/codex-plus-core/src/compaction_tools.rs|src/compaction_tools.rs"
  "crates/codex-plus-core/src/prefix_divergence.rs|src/prefix_divergence.rs"
)

usage() {
  echo "用法: $0 <本地 codexplusplus 仓库路径> [基线 commit，默认 ${BASELINE_DEFAULT}]" >&2
  exit 2
}

REPO="${1:-}"
BASELINE="${2:-$BASELINE_DEFAULT}"
[ -n "$REPO" ] || usage
[ -d "$REPO" ] || { echo "错误: 目录不存在: $REPO" >&2; exit 2; }
git -C "$REPO" rev-parse --git-dir >/dev/null 2>&1 \
  || { echo "错误: 不是 git 仓库: $REPO" >&2; exit 2; }
git -C "$REPO" cat-file -e "${BASELINE}^{commit}" 2>/dev/null \
  || { echo "错误: 参照仓库中找不到基线 commit: ${BASELINE}（请先在该仓库 git fetch --all）" >&2; exit 2; }

# 依次尝试本地分支与各远端的同名分支
REF=""
for cand in "$BRANCH" "origin/$BRANCH" "upstream/$BRANCH"; do
  if git -C "$REPO" rev-parse --verify --quiet "$cand" >/dev/null; then
    REF="$cand"
    break
  fi
done
if [ -z "$REF" ]; then
  echo "错误: 参照仓库中找不到分支 ${BRANCH}（或其 origin/upstream 远端副本），" >&2
  echo "      请先在 $REPO 执行: git fetch --all" >&2
  exit 2
fi

TIP="$(git -C "$REPO" rev-parse --short "$REF")"
AHEAD="$(git -C "$REPO" rev-list --count "${BASELINE}..${REF}")"

echo "参照仓库: $REPO"
echo "参照分支: $REF (tip ${TIP})"
echo "同步基线: ${BASELINE}（分支侧领先 ${AHEAD} 个提交）"
if [ "$AHEAD" -eq 0 ]; then
  echo
  echo "基线即分支最新提交，涉及文件无需同步检查。"
  exit 0
fi

TOTAL=0
for entry in "${FILES[@]}"; do
  up="${entry%%|*}"
  local_file="${entry##*|}"
  echo
  echo "== ${up} （本仓库: ${local_file}）=="
  log="$(git -C "$REPO" log --oneline "${BASELINE}..${REF}" -- "$up")"
  if [ -z "$log" ]; then
    echo "  （自基线以来无新提交，无需同步）"
  else
    echo "$log" | sed 's/^/  /'
    n="$(printf '%s\n' "$log" | wc -l | tr -d ' ')"
    TOTAL=$((TOTAL + n))
  fi
done

echo
echo "汇总: 涉及文件自基线以来共有 ${TOTAL} 个提交触及（同一提交触及多个文件会重复计数）。"
echo "逐提交审阅: git -C $REPO show <commit>；移植时保留来源注释（见 AGENTS.md）。"
