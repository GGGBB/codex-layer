#!/usr/bin/env python3
"""集成测试隔离门禁：开稳定层的 spawn 必须把 tools 回填缓存指到临时目录。

立项依据：tools 回填缓存落盘（#69 第 6 条）之后，任何以
`prompt_prefix = true` 启动中继子进程的集成测试都会**往真实状态目录写测试键**。
本机真长出了一批 `degrade-ok-*`、`over-limit-stream-*` 之类的键，
把「重启后第一个压缩轮是否命中预热」这个真机判据搅浑——真键与测试键混在同一个文件里。

为什么做成门禁而不是靠人记住：注入这件事写在注释里挡不住新增测试，
每加一条开稳定层的用例都可能重犯，而犯了的信号只出现在开发者本机
（真实目录被污染），CI 完全看不见。

判据（纯函数 `audit_source`，逐文件）：源文件里出现 `prompt_prefix = true`
即视为会启动开稳定层的子进程，必须同时出现 `CODEX_LAYER_TOOLS_CACHE_PATH`。
刻意只认这两个字面量、不做语法解析：两者都是 spawn 处必然手写的字符串，
漏任一个就说明隔离没做。

健康度约束同其他门禁：`--self-test` 必带正反对照；扫不到任何文件时显式失败，
不把「没文件可查」当成「全部合规」。
"""

from __future__ import annotations

import argparse
import os
import subprocess
import sys
import tempfile

STABILIZER_ON = "prompt_prefix = true"
CACHE_ENV = "CODEX_LAYER_TOOLS_CACHE_PATH"


def audit_source(name: str, source: str) -> str | None:
    """返回违规原因；合规返回 None。"""
    if STABILIZER_ON not in source:
        return None
    if CACHE_ENV in source:
        return None
    return f"启用稳定层却未注入 {CACHE_ENV}（会写真实状态目录）"


def tracked_test_files(repo: str | None) -> list[str]:
    proc = subprocess.run(
        ("git", "ls-files", "tests/*.rs"),
        capture_output=True,
        text=True,
        cwd=repo,
    )
    if proc.returncode != 0:
        detail = (proc.stderr or proc.stdout or "").strip()
        raise GateError(f"`git ls-files tests/*.rs` 失败（退出码 {proc.returncode}）：{detail}")
    return [line.strip() for line in proc.stdout.splitlines() if line.strip()]


class GateError(RuntimeError):
    """判据自身不可用时抛出——绝不降级成「没有违规」。"""


def read(path: str) -> str:
    with open(path, encoding="utf-8") as handle:
        return handle.read()


def self_test() -> int:
    cases = [
        ("启用 + 已注入", f"{STABILIZER_ON}\n.env({CACHE_ENV}, x)", None),
        ("启用 + 未注入", f"{STABILIZER_ON}\n.stdin(null)", "违规"),
        ("未启用", "prompt_prefix = false\n.stdin(null)", None),
        ("空文件", "", None),
    ]
    for label, source, expect in cases:
        reason = audit_source(label, source)
        if expect is None and reason is not None:
            print(f"自测失败：夹具「{label}」被判违规：{reason}", file=sys.stderr)
            return 1
        if expect is not None and reason is None:
            print(f"自测失败：夹具「{label}」应当判违规却全绿", file=sys.stderr)
            return 1
    print(
        f"隔离门禁自测通过：{len(cases)} 例夹具判定与期望一致"
        "（启用+已注入放行、启用+未注入判红、未启用放行、空文件放行）。"
    )
    return 0


def main() -> int:
    parser = argparse.ArgumentParser(description="断言开稳定层的集成测试隔离了 tools 缓存落盘路径")
    parser.add_argument("--self-test", action="store_true", help="跑夹具对照，不查真实仓库")
    parser.add_argument("--repo", help="仓库目录（默认当前目录）")
    args = parser.parse_args()

    if args.self_test:
        return self_test()

    repo = args.repo or os.getcwd()
    try:
        files = tracked_test_files(repo)
    except GateError as exc:
        print(f"隔离门禁无法执行：{exc}", file=sys.stderr)
        return 2
    if not files:
        print("警告：没有跟踪任何 tests/*.rs，判据无事可做（不是「全部合规」）。", file=sys.stderr)
        return 2

    violations: list[str] = []
    enabled = 0
    for rel in files:
        source = read(os.path.join(repo, rel))
        reason = audit_source(rel, source)
        if STABILIZER_ON in source:
            enabled += 1
        if reason:
            violations.append(f"  {rel}：{reason}")
    print(f"隔离门禁：扫描 {len(files)} 个集成测试文件，其中启用稳定层的 {enabled} 个。")
    if enabled == 0:
        print("警告：没有任何启用稳定层的集成测试被检查，请确认判据是否失效。", file=sys.stderr)
        return 2
    if violations:
        print("发现未隔离的测试文件：", file=sys.stderr)
        print("\n".join(violations), file=sys.stderr)
        print(
            "\n修正办法：在 spawn 子进程处注入独立路径，例如\n"
            '    .env("CODEX_LAYER_TOOLS_CACHE_PATH", dir.path().join("tools-cache.json"))',
            file=sys.stderr,
        )
        return 1
    print("全部启用稳定层的集成测试都把 tools 缓存指到了临时目录。")
    return 0


if __name__ == "__main__":
    sys.exit(main())
