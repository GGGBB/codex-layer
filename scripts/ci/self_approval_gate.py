#!/usr/bin/env python3
"""作者不得自审的事后检测：交叉 review 事件、issue 评论与实际合并事实。

条款侧（作者不得下结论、不得合并）已由贡献指南写死，但单账号仓库拿不到
GitHub 语义上的「他人批准」，条款只能约束读得到它的人。本脚本是那条**事后检测**：
它不判「谁批准」（同一登录名下恒真，毫无区分力），只判三个结构化事实——

R1 合并时无生效批准：全部批准口径条目的时间戳都晚于合并时间，即合并那一刻
   不存在任何可能影响决策的批准（事后补的记档即使含批准措辞也不算）。
R2 批准未覆盖最终内容：合并前最后一条批准之后，分支上又出现了新提交，
   即「批准的是 A，合并的是 A+新增」；作者改了东西却没有重新取得批准。
   判据取「批准之后是否仍有新提交」（按 commit 的 author date），而不是比对 SHA：
   本仓库只允许 rebase 合并，GitHub 会重写提交 SHA，比对 SHA 会把正常的
   「落后于 main 时合并」误判成绕过。author date 在 rebase 后保持不变，可用。
R3 无批准口径合并：禁令生效后合并、却没有任何批准口径的 review / 评论，也没有
   APPROVED 事件——即绕过 review 直接合并。

两条来自本仓库踩坑的硬约束：
1. **判据必须自带正反对照**（`--self-test`）：合规样本判绿**且**扫描计数等于条目数，
   违规样本必须被抓出。否则就是恒绿假判据。
2. **措辞匹配要保守**：裸的「通过」大量出现在技术事实句里（「判据自测那行刚打印通过」
   「cargo test 通过」），一律误报。批准口径必须带结论句式或合并口径。宁可漏判，
   但漏判侧必须有清单输出，让人看到扫过了哪些条目。

只读：不调用任何写接口，不改任何 PR 状态。
"""

from __future__ import annotations

import argparse
import json
import os
import re
import sys
import urllib.error
import urllib.request

API = "https://api.github.com"

# 作者侧禁令（贡献指南的动作级禁止）落地时间：此后合并的 PR 才受本判据约束，
# 此前的历史 PR 缺留痕属既成事实，只记档不判红。
BAN_EFFECTIVE_AT = "2026-10-14T23:06:20Z"

# 批准口径。刻意要求「结论句式 + 合并口径」或既有的固定措辞，不接受裸「通过」。
APPROVE_RE = re.compile(
    r"复核通过|审读通过|审阅通过|审核通过|整体通过|内容通过|实现通过|条款[^。\n]{0,4}通过"
    r"|结论[：:]\s*[^。\n]{0,8}通过"
    r"|通过[，,]\s*(可|可以|即将|随后|已|rebase)[^。\n]{0,8}合并"
    r"|(可|可以|实现)[^。\n]{0,4}合并|批准合并|同意合并|准予合并|LGTM|approve[d]?\b",
    re.I,
)
# 「先修再合」形态：批准口径里带条件。它本身不判红，但配合 R2 能钉住
# 「结论是先修再合、随后照样合并」这类绕过。
CONDITIONAL_RE = re.compile(
    r"(改掉|修完|修好|改完|修复|表态|处理|解决)[^。\n]{0,10}再合|后再合|再\s*rebase\s*合并|先修"
)


def conclusion_hit(body: str) -> bool:
    """是否为「review 结论」条目：无条件批准，或带条件的批准（先修再合）。

    条件式结论同样必须纳入交叉——历史事故正是「结论写明先修再合，随后照样合并」，
    只认无条件批准的判据对这种形态完全失明。
    """
    return bool(APPROVE_RE.search(body or "")) or bool(CONDITIONAL_RE.search(body or ""))


def classify(pr: dict) -> tuple[bool, list[str]]:
    """判定单个 PR。返回 (是否判红, 证据行)；只接受已合并的 PR。"""
    merged_at = pr.get("merged_at")
    if not merged_at or merged_at <= BAN_EFFECTIVE_AT:
        return False, ["禁令生效前合并，不判"]
    entries: list[tuple[str, str, str | None, str]] = []
    for r in pr.get("reviews", []):
        if r.get("state") == "COMMENTED" and not conclusion_hit(r.get("body", "")):
            continue
        entries.append(
            (
                f"review:{r.get('state')}",
                r.get("submitted_at") or "",
                r.get("commit_id"),
                r.get("body") or "",
            )
        )
    for c in pr.get("comments", []):
        if conclusion_hit(c.get("body", "")):
            entries.append(("comment", c.get("created_at") or "", None, c.get("body") or ""))

    approvals = [
        e for e in entries if e[0] == "review:APPROVED" or conclusion_hit(e[3])
    ]
    if not approvals:
        return True, [f"R3 无批准口径合并：禁令生效后合并于 {merged_at}，无批准 review 也无批准评论"]

    reds: list[str] = []
    pre_merge = [a for a in approvals if a[1] and a[1] <= merged_at]
    if not pre_merge:
        latest = approvals[-1]
        reds.append(
            f"R1 合并时无生效批准：唯一的批准口径条目写于 {latest[1]}，"
            f"晚于合并时间 {merged_at}，不可能影响合并决策"
        )
        # 已经没有合并前的批准，R2 无从谈起，直接交卷。
        return True, reds
    latest_approval = max(a[1] for a in pre_merge)
    # 批准之后新推的提交：author date 不受 rebase 重写影响，是「批准未覆盖最终内容」的可靠信号。
    pushed_after = [
        (sha, date)
        for sha, date in (pr.get("commit_dates") or [])
        if latest_approval and date > latest_approval
    ]
    if pushed_after:
        last_sha, last_date = pushed_after[-1]
        reds.append(
            f"R2 批准未覆盖最终内容：合并前最后一条批准（{latest_approval}）之后仍有 "
            f"{len(pushed_after)} 个提交，最新 {last_sha[:7]} 写于 {last_date}，合并于 {merged_at}"
        )
    elif not any(a[2] for a in approvals):
        # 批准只出现在评论里、且期间没有新提交：缺 commit 锚点但不构成绕过。
        reds.append("提示：批准只出现在评论里，无 commit 锚点可验覆盖（不判红，请人工确认）")
    return bool([r for r in reds if r.startswith(("R1", "R2", "R3"))]), reds


def fetch_merged(token: str, owner: str, repo: str, per_page: int = 100) -> list[dict]:
    prs: list[dict] = []
    # 逐页往前翻，直到某一页全部早于禁令生效时间：已合并 PR 会随时间累积，
    # 只取第一页会在第 101 个 PR 之后静默漏扫，那正是本仓库最反对的假绿形态。
    for page in range(1, 11):
        batch = _get(
            f"{API}/repos/{owner}/{repo}/pulls?state=all&per_page={per_page}&page={page}", token
        )
        if not batch:
            break
        prs.extend(batch)
        if all(
            (p.get("merged_at") or "") <= BAN_EFFECTIVE_AT for p in batch
        ):
            break
    merged = [p for p in prs if p.get("merged_at") and p["merged_at"] > BAN_EFFECTIVE_AT]
    for p in merged:
        n = p["number"]
        p["reviews"] = _get(f"{API}/repos/{owner}/{repo}/pulls/{n}/reviews?per_page=100", token)
        p["comments"] = _get(
            f"{API}/repos/{owner}/{repo}/issues/{n}/comments?per_page=100", token
        )
        # author date 在 rebase 合并后保持不变，用它判断「批准之后是否又推了提交」。
        p["commit_dates"] = [
            (c["sha"], c["commit"]["author"]["date"])
            for c in _get(f"{API}/repos/{owner}/{repo}/pulls/{n}/commits?per_page=100", token)
        ]
    return merged


def _get(url: str, token: str) -> list[dict]:
    req = urllib.request.Request(
        url,
        headers={
            "Authorization": f"Bearer {token}",
            "Accept": "application/vnd.github+json",
            "User-Agent": "codex-layer-self-approval-gate",
        },
    )
    try:
        with urllib.request.urlopen(req, timeout=30) as resp:
            return json.load(resp)
    except (urllib.error.URLError, urllib.error.HTTPError, TimeoutError) as exc:
        raise GateError(f"查询失败 {url}：{exc}") from exc


class GateError(RuntimeError):
    """判据自身不可用时抛出——绝不降级成「没有违规」。"""


def review_body_fixtures() -> tuple[list[str], list[str]]:
    """措辞表正反对照：批准口径必须命中，技术事实句必须不命中。"""
    must_hit = [
        "review（维护者）通过，rebase 合并。",
        "review（维护者）通过，已 rebase 合并为 main `0d974d2`",
        "## review 结论：通过，即将 rebase 合并",
        "复核通过。条款按构建目标区分",
        "审读通过。条款措辞准确",
        "**维护者记档：内容通过，但本 PR 的合并流程违规**",
        "整体通过，判据链写得扎实",
        "实现可以合并，但有一处需要你明确表态后再合",
        "结论：本 PR 内改掉再合",
    ]
    must_miss = [
        "更正我上面那条的一处数字：判据自测那行刚打印通过，紧接着 tar: stdout: write error",
        "本地实测监看 8 全过，测试通过，可以删掉临时调试日志",
        "cargo test 通过",
        "gates 判据自测通过（示例输出）",
    ]
    return must_hit, must_miss


def pr_fixture(number: int, head: str, merged_at: str, reviews, comments=(), commits=()) -> dict:
    return {
        "number": number,
        "merged_at": merged_at,
        "head": {"sha": head},
        "commit_dates": list(commits),
        "reviews": [
            {
                "state": st,
                "submitted_at": ts,
                "commit_id": cid,
                "body": body,
            }
            for st, ts, cid, body in reviews
        ],
        "comments": [{"created_at": ts, "body": body} for ts, body in comments],
    }


def self_test() -> int:
    must_hit, must_miss = review_body_fixtures()
    for body in must_hit:
        if not conclusion_hit(body):
            print(f"自测失败：批准口径未命中 :: {body[:40]}", file=sys.stderr)
            return 1
    for body in must_miss:
        if conclusion_hit(body):
            print(f"自测失败：技术事实句被误判成批准 :: {body[:40]}", file=sys.stderr)
            return 1

    base = "2026-10-15T08:00:00Z"
    head_a = "a" * 40
    commit_a = ("a1" + "0" * 38, "2026-10-15T07:10:00Z")
    cases = [
        # 合规：批准覆盖最终 head，且早于合并
        (
            pr_fixture(
                1, head_a, base,
                [("COMMENTED", "2026-10-15T07:59:00Z", head_a, "复核通过，即将合并")],
                commits=[commit_a],
            ),
            False,
            "合规",
        ),
        # R1：唯一批准晚于合并（历史真实形态）
        (
            pr_fixture(
                2, head_a, base,
                [("COMMENTED", "2026-10-15T08:02:00Z", head_a, "审读通过。")],
                commits=[commit_a],
            ),
            True,
            "R1",
        ),
        # R2：批准之后作者又推了提交，且没有重新批准（「先修再合、随后照样合并」形态）
        (
            pr_fixture(
                3, head_a, base,
                [("COMMENTED", "2026-10-15T07:00:00Z", "b" * 40, "整体通过")],
                commits=[commit_a, ("c9" + "0" * 38, "2026-10-15T07:30:00Z")],
            ),
            True,
            "R2",
        ),
        # R3：无任何批准口径条目
        (pr_fixture(4, head_a, base, []), True, "R3"),
        # 禁令生效前合并：不判
        (pr_fixture(5, head_a, "2026-10-13T02:57:34Z", []), False, "禁令前"),
        # 批准只在评论里、期间无新提交：不判红，但要给出人工确认提示
        (
            pr_fixture(40, head_a, base, [], [("2026-10-15T07:30:00Z", "## review 结论：通过，即将 rebase 合并")]),
            False,
            "评论锚点提示",
        ),
    ]
    for pr, expect_red, label in cases:
        red, lines = classify(pr)
        if red != expect_red:
            print(
                f"自测失败：夹具「{label}」判成 {'红' if red else '绿'}，期望 {'红' if expect_red else '绿'}",
                file=sys.stderr,
            )
            return 1
        if expect_red and not any(line.startswith("R") for line in lines):
            print(f"自测失败：夹具「{label}」判红却没列证据", file=sys.stderr)
            return 1
    if len(cases) != 6:
        print("自测失败：夹具数量与声明不符", file=sys.stderr)
        return 1

    # 两起真实发生过的事故（脱敏后入夹具）：判据若抓不出它们，就是恒绿假判据。
    incidents = [
        # 事故甲：结论写明「先修再合」，随后作者未重跑批准直接合并，
        # 合并进的是批准之后新推的提交。
        (
            pr_fixture(
                901,
                "5f4b1fb" + "0" * 33,
                "2026-10-15T04:41:30Z",
                [
                    ("COMMENTED", "2026-10-15T04:15:00Z", "9e6e808" + "0" * 33,
                     "整体通过，三处待改，请在本 PR 内改掉再合"),
                    ("COMMENTED", "2026-10-15T04:29:10Z", "9e6e808" + "0" * 33,
                     "过程说明：P1 仍未修复"),
                ],
                commits=[
                    ("9e6e808" + "0" * 33, "2026-10-15T04:10:00Z"),
                    ("5f4b1fb" + "0" * 33, "2026-10-15T04:40:00Z"),
                ],
            ),
            "R2",
        ),
        # 事故乙：批准口径的时间戳晚于合并时间，它不可能影响合并决策。
        (
            pr_fixture(
                902,
                "c78a113" + "0" * 33,
                "2026-10-15T03:59:58Z",
                [("COMMENTED", "2026-10-15T04:02:25Z", "c78a113" + "0" * 33, "审读通过。条款措辞准确")],
                commits=[("c78a113" + "0" * 33, "2026-10-15T03:50:00Z")],
            ),
            "R1",
        ),
    ]
    for pr, expect_rule in incidents:
        red, lines = classify(pr)
        if not red:
            print(f"自测失败：历史事故夹具 {pr['number']} 未判红——判据失明", file=sys.stderr)
            return 1
        if not any(line.startswith(expect_rule) for line in lines):
            print(
                f"自测失败：历史事故夹具 {pr['number']} 未命中 {expect_rule}，"
                f"实际证据：{lines}",
                file=sys.stderr,
            )
            return 1
    print(
        f"事后检测自测通过：措辞表 {len(must_hit)} 条批准口径全部命中、"
        f"{len(must_miss)} 条技术事实句零误报；PR 夹具 {len(cases)} 例判定与期望一致"
        "（合规、R1 事后、R2 未覆盖、R3 无批准、禁令前不判、评论锚点只提示）；"
        f"另 {len(incidents)} 起真实事故夹具分别命中 R2 与 R1。"
    )
    return 0


def main() -> int:
    parser = argparse.ArgumentParser(description="作者不得自审的事后检测（只读）")
    parser.add_argument("--self-test", action="store_true", help="跑夹具对照，不访问网络")
    parser.add_argument("--repo", default="GGGBB/codex-layer", help="owner/repo")
    args = parser.parse_args()

    if args.self_test:
        return self_test()

    token = os.environ.get("GITHUB_TOKEN") or os.environ.get("GH_TOKEN")
    if not token:
        print("事后检测无法执行：缺少 GITHUB_TOKEN（不许把「查不到」当成「没有违规」）", file=sys.stderr)
        return 2
    owner, _, repo = args.repo.partition("/")
    try:
        merged = fetch_merged(token, owner, repo)
    except GateError as exc:
        print(f"事后检测无法执行：{exc}", file=sys.stderr)
        return 2
    if not merged:
        print("警告：禁令生效后没有任何已合并 PR 被扫描，判据无事可做。", file=sys.stderr)
        return 2

    reds = 0
    print(f"事后检测：扫描禁令生效后合并的 {len(merged)} 个 PR。")
    for pr in sorted(merged, key=lambda p: p["merged_at"]):
        red, lines = classify(pr)
        reds += int(red)
        print(f"  {'红' if red else '绿'} PR#{pr['number']}（合并于 {pr['merged_at']}）")
        for line in lines:
            print(f"      {line}")
    if reds:
        print(f"\n发现 {reds} 个 PR 违反「先 review 后合并」：见上方逐条证据。", file=sys.stderr)
        return 1
    print("全部通过：每个 PR 合并前都存在批准口径，且批准之后没有未再批准的新提交。")
    return 0


if __name__ == "__main__":
    sys.exit(main())
