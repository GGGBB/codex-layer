#!/usr/bin/env python3
"""提交作者门禁：断言待入库区间内每个 commit 的 author 都是角色身份。

立项依据见 issue #7：贡献流程要求每个 commit 带
`git -c user.name="codex-layer-<角色>" -c user.email="<角色>@users.noreply.github.com"`，
但漏带这两条时 commit 会静默回落到本机全局身份（真实姓名与真实邮箱），
既有的两个必需检查都看不出来，等发现时提交已进 main。

两条设计约束（都来自本仓库踩过的坑）：
1. **区间解析失败必须显式失败**。「没抓到就算通过」这类反向判据会把查询失败吞成
   判据通过，所以基线取不到、区间为空，一律非零退出并说明原因。
2. **逐条列证据**。失败输出必须带具体 commit 短码 + author 名 + author 邮箱，
   让人不必二次查询就能行动。
"""

from __future__ import annotations

import argparse
import os
import re
import sys
import tempfile

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))

import git_range  # 区间解析与 git 调用封装，与格式门禁共用同一份
from git_range import GateError, resolve

NAME_RE = re.compile(r"^codex-layer-([a-z0-9][a-z0-9-]*)$")
EMAIL_RE = re.compile(r"^([a-z0-9][a-z0-9-]*)@users\.noreply\.github\.com$")


def collect(rng: str) -> list[dict[str, object]]:
    raw = git_range.git("log", "--format=%H\x1f%an\x1f%ae\x1f%P\x1e", rng)
    items = []
    for record in raw.split("\x1e"):
        if not record.strip():
            continue
        sha, name, email, parents = record.strip("\n").split("\x1f")
        items.append(
            {
                "sha": sha,
                "name": name,
                "email": email,
                "is_merge": len(parents.split()) > 1,
            }
        )
    return items


def classify(item: dict[str, object]) -> str | None:
    """返回违规原因；合规返回 None。合并提交的 author 是合并执行者，不判。"""
    if item["is_merge"]:
        return None
    name, email = str(item["name"]), str(item["email"])
    name_m = NAME_RE.match(name)
    email_m = EMAIL_RE.match(email)
    if not name_m:
        return "author 名不是 codex-layer-<角色> 形态"
    if not email_m:
        return "author 邮箱不是 <角色>@users.noreply.github.com 形态"
    if name_m.group(1) != email_m.group(1):
        return f"author 名与邮箱的角色不一致（名 {name_m.group(1)}，邮箱 {email_m.group(1)}）"
    return None


def run_gate(rng: str) -> tuple[int, int, list[str]]:
    """返回 (退出码, 实际检查的提交数, 违规明细行)。"""
    items = collect(rng)
    lines: list[str] = []
    checked = 0
    for item in items:
        if item["is_merge"]:
            lines.append(f"  跳过合并提交 {str(item['sha'])[:7]}")
            continue
        checked += 1
        reason = classify(item)
        if reason:
            lines.append(
                f"  {str(item['sha'])[:7]} 违规：{reason}"
                f"\n      author name = {item['name']}"
                f"\n      author email = {item['email']}"
            )
    return (1 if any("违规" in line for line in lines) else 0), checked, lines


def self_test() -> int:
    """临时仓库正反对照，钉住判据本身的健康度。

    必须有正判据：合规样本全绿**且**检查计数恰等于提交数；违规样本必须被抓出
    并逐条列出短码。计数校验是防「区间解析失效导致 0 个提交被检查」的假绿。
    """
    with tempfile.TemporaryDirectory(prefix="codex-layer-author-gate-") as repo:
        git_range.REPO = repo
        git_range.git("init", "-q")
        git_range.git("config", "user.name", "本机真实身份")
        git_range.git("config", "user.email", "someone@example.com")

        def commit(message: str, role: str | None, name: str | None = None) -> str:
            with open(os.path.join(repo, f"{message}.txt"), "w", encoding="utf-8") as handle:
                handle.write(message + "\n")
            git_range.git("add", "-A")
            if role is None and name is None:
                # 模拟漏带 -c：静默回落本机全局身份
                git_range.git("commit", "-q", "-m", message)
            elif name is not None:
                # 模拟 author 名与邮箱的角色写岔
                git_range.git(
                    "-c",
                    f"user.name={name}",
                    "-c",
                    f"user.email={role}@users.noreply.github.com",
                    "commit",
                    "-q",
                    "-m",
                    message,
                )
            else:
                git_range.git(
                    "-c",
                    f"user.name=codex-layer-{role}",
                    "-c",
                    f"user.email={role}@users.noreply.github.com",
                    "commit",
                    "-q",
                    "-m",
                    message,
                )
            return git_range.git("rev-parse", "HEAD").strip()[:7]

        root = commit("root", "governance")
        good = commit("good", "stabilizer")
        bad_author = commit("bad-author", None)
        bad_mismatch = commit("bad-mismatch", "governance", name="codex-layer-stabilizer")

        cases = (
            (f"{root}..{root}", False, 0, []),
            (f"{root}..{good}", False, 1, []),
            (f"{root}..{bad_author}", True, 2, [bad_author]),
            (f"{root}..{bad_mismatch}", True, 3, [bad_mismatch]),
            (f"{root}..HEAD", True, 3, [bad_author, bad_mismatch]),
            # 不给区间边界时必须逐个查全历史，含根提交
            ("HEAD", True, 4, [bad_author, bad_mismatch]),
        )
        for rng, expect_fail, expect_checked, must_contain in cases:
            code, checked, lines = run_gate(rng)
            output = "\n".join(lines)
            if expect_fail and code == 0:
                print(f"自测失败：区间 {rng} 应当判违规，却全绿", file=sys.stderr)
                return 1
            if not expect_fail and code != 0:
                print(f"自测失败：区间 {rng} 本应合规，却报错：\n{output}", file=sys.stderr)
                return 1
            if checked != expect_checked:
                print(
                    f"自测失败：区间 {rng} 检查计数 {checked}，期望 {expect_checked}"
                    "（计数不符说明区间或解析失效）",
                    file=sys.stderr,
                )
                return 1
            for sha in must_contain:
                if sha not in output:
                    print(f"自测失败：区间 {rng} 的失败输出未列出 {sha}", file=sys.stderr)
                    return 1
        # 不可达基线必须显式报错，不许静默放行
        try:
            run_gate("deadbeef..HEAD")
        except GateError:
            pass
        else:
            print("自测失败：不可达基线未触发 GateError", file=sys.stderr)
            return 1
        git_range.REPO = None
    print(
        "提交作者门禁自测通过：合规样本全绿且计数正确，两类违规样本"
        "（回落全局身份、名与邮箱角色不一致）全部被抓出并逐条列出，不可达基线显式报错。"
    )
    return 0


def main() -> int:
    parser = argparse.ArgumentParser(description="断言待入库区间的 commit author 都是角色身份")
    parser.add_argument("--base", help="基线引用，例如 refs/remotes/origin/main")
    parser.add_argument("--range", dest="range", help="直接给定 rev-list 区间（优先于 --base）")
    parser.add_argument("--self-test", action="store_true", help="跑正反对照自测，不检查真实仓库")
    args = parser.parse_args()

    if args.self_test:
        return self_test()

    try:
        rng, how = resolve(args.range, args.base)
        code, checked, lines = run_gate(rng)
    except GateError as exc:
        print(f"提交作者门禁无法执行：{exc}", file=sys.stderr)
        return 2

    print(f"提交作者门禁：区间 {rng}（{how}），实际检查 {checked} 个提交。")
    if checked == 0:
        print("警告：区间内没有任何提交被检查，请确认基线是否正确。", file=sys.stderr)
        return 2
    if code:
        print("发现不合规 author：", file=sys.stderr)
        print("\n".join(lines), file=sys.stderr)
        print(
            "\n修正办法：给具体提交补上角色身份，例：\n"
            '  git -c user.name="codex-layer-<角色>" '
            '-c user.email="<角色>@users.noreply.github.com" '
            "commit --amend --reset-author --no-edit",
            file=sys.stderr,
        )
        return 1
    if lines:
        print("\n".join(lines))
    print("全部 author 符合角色身份约定。")
    return 0


if __name__ == "__main__":
    sys.exit(main())
