#!/usr/bin/env python3
"""待入库区间的解析与 git 调用封装，供 CI 门禁脚本共用。

单独成模块的理由：区间解析里有「取不到基线怎么办」这条易错分支。两个门禁各写一遍
迟早写岔，而写岔的后果是门禁静默放行——比没有门禁更糟，因为它给出假的绿色信号。
"""

from __future__ import annotations

import os
import subprocess

# 自测用临时仓库路径；生产路径为 None，表示用当前工作目录。
REPO: str | None = None

# GitHub 在首次推送（没有历史）时把 before 填成全零 SHA
ZERO_SHA = "0" * 40


class GateError(RuntimeError):
    """判据自身不可用时抛出——绝不降级成「没有违规」。"""


def git(*args: str) -> str:
    proc = subprocess.run(
        ("git", *args),
        capture_output=True,
        text=True,
        cwd=REPO,
    )
    if proc.returncode != 0:
        detail = (proc.stderr or proc.stdout or "").strip()
        raise GateError(f"`git {' '.join(args)}` 失败（退出码 {proc.returncode}）：{detail}")
    return proc.stdout


def verify_commit(rev: str) -> str:
    return git("rev-parse", "--verify", f"{rev}^{{commit}}").strip()


def has_parent(sha: str) -> bool:
    return bool(git("show", "-s", "--format=%P", sha).split())


def single_head_range() -> tuple[str, str]:
    """兜底区间：只查 HEAD 单笔。

    取不到基线时宁可少查，也不许把整仓历史一次性放行——后者会让任何一次基线
    解析故障都变成「门禁全绿」的假信号。单笔是确定能查的最小单元。
    """
    revs = git("rev-list", "--max-count=1", "HEAD").split()
    if not revs:
        raise GateError("HEAD 解析不出任何提交，仓库状态可疑")
    head = revs[0]
    if has_parent(head):
        return f"{head}~1..{head}", "无基线信息，退回只查 HEAD 单笔"
    return head, "HEAD 是根提交（无父），只查这一笔"


def resolve(explicit_range: str | None, base: str | None) -> tuple[str, str]:
    """返回 (rev-list 区间, 该区间是怎么定下来的)，供门禁与人工核对复用。"""
    if explicit_range:
        return explicit_range, f"显式指定 --range {explicit_range}"
    if base:
        sha = verify_commit(base)
        merge_base = git("merge-base", sha, "HEAD").strip()
        return f"{merge_base}..HEAD", f"基线 {base}（{sha[:7]}）与 HEAD 的 merge-base {merge_base[:7]}"

    # push 事件：GitHub 给出推送前的 SHA，正好是「本次新增提交」的自然边界
    before = os.environ.get("GATE_PUSH_BEFORE", "").strip()
    if before and before != ZERO_SHA:
        try:
            verify_commit(before)
        except GateError:
            # 强推或历史被裁剪时推送前 SHA 不可达，退回单笔而不是整仓
            return single_head_range()
        return f"{before}..HEAD", f"推送前 SHA {before[:7]}"

    # pull_request 事件：actions/checkout 已把 base 分支取到本地
    base_ref = os.environ.get("GITHUB_BASE_REF", "").strip()
    if base_ref:
        return resolve(None, f"refs/remotes/origin/{base_ref}")

    return single_head_range()


def range_endpoint(rng: str) -> str:
    """取出区间右端点的修订，用于「按区间终点的内容」判定，而不是工作区当前内容。

    `a..b` 的右端点是 b；`a...b` 同理；裸 `HEAD` 或单个 SHA 就是它自己；
    右端为空（`a..`）按 git 语义即 HEAD。解析不出右端点时显式失败。
    """
    if ".." in rng:
        left, _, right = rng.rpartition("..")
        right = right.strip()
        return right or "HEAD"
    token = rng.strip()
    if not token:
        raise GateError("区间为空串，解析不出判定终点")
    return token


def empty_tree() -> str:
    """git 的空树对象哈希，用于把「单个修订」当作「相对空树的全部新增」。"""
    return git("hash-object", "-t", "tree", "/dev/null").strip()


def added_files(rng: str, pathspec: str) -> list[str]:
    """列出区间内新增的文件名。

    单个修订（不含 `..`）时相对空树比较：`git diff <rev>` 比的是工作区，
    会给出「工作区相对该提交少了什么」，拿它当「新增了哪些文件」是错的。
    """
    if ".." in rng:
        raw = git("diff", "--name-only", "--diff-filter=A", rng, "--", pathspec)
    else:
        rev = rng.strip()
        if not rev:
            raise GateError("区间为空串，无法判定新增文件")
        raw = git("diff", "--name-only", "--diff-filter=A", empty_tree(), rev, "--", pathspec)
    return [line.strip() for line in raw.splitlines() if line.strip()]
