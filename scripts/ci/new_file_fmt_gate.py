#!/usr/bin/env python3
"""新增 Rust 文件的格式门禁：只对本次新增的 `*.rs` 跑 `rustfmt --check`。

立项依据见 issue #8：全仓 `cargo fmt --check` 因既有代码的上百处差异不可用
（一次性重排会淹没真实改动，仓库纪律明令禁止），CI 因此完全不跑 fmt，
连带**新写文件**也没有任何格式约束，「新写文件按默认风格」只是口头约定，
随时间会再攒出一批不可格式化的存量。

边界（同样来自 issue #8，不许越界）：
- 只判 `git diff --diff-filter=A` 得到的**新增** `.rs` 文件；
- 「修改但非新增」的文件不判——既有代码的合法修改不能被误伤；
- 门禁只读不写，不改任何文件，也不建 rustfmt.toml。

判据健康度约束：`rustfmt` 不存在时必须显式失败，不许把「找不到工具」
当成「没有不合规文件」放行；新增文件数为 0 时照常通过，但要在输出里说明
是「无新增文件」而不是「全部合规」；输出里的检查文件数必须等于 diff 的新增文件数，
对不上就是判据自己失效了。
"""

from __future__ import annotations

import argparse
import os
import shutil
import subprocess
import sys
import tempfile

sys.path.insert(0, os.path.abspath(os.path.dirname(__file__)))

import git_range
from git_range import GateError, resolve

RUST_EDITION = "2024"


def added_rust_files(rng: str) -> list[str]:
    return git_range.added_files(rng, "*.rs")


def check_text(path: str, content: str, rustfmt: str) -> tuple[bool, str]:
    """判定单个文件的**指定内容**，全程只读，不触碰工作区文件。

    两个实测踩坑：
    1. `--check` 走标准输入时，本机稳定版 rustfmt 即使打印出差异也**退出码 0**，
       而 `--stdin-filename` 属 unstable 选项、稳定版不认——只能落成临时文件用文件形态判。
    2. 临时文件必须带 `.rs` 后缀，否则 rustfmt 直接拒绝。
    本仓库刻意没有 rustfmt.toml（见仓库纪律），因此把内容搬到临时目录判不会丢配置口径；
    将来若真要引入配置文件，这里要同步改成显式传 `--config-path`。
    """
    with tempfile.TemporaryDirectory(prefix="codex-layer-fmt-") as tmp:
        probe = os.path.join(tmp, os.path.basename(path) or "probe.rs")
        with open(probe, "w", encoding="utf-8") as handle:
            handle.write(content)
        proc = subprocess.run(
            (rustfmt, "--check", "--edition", RUST_EDITION, probe),
            capture_output=True,
            text=True,
        )
    if proc.returncode == 0:
        return True, ""
    return False, (proc.stdout or proc.stderr or "").strip()


def run_gate(rng: str) -> tuple[int, int, list[str]]:
    """返回 (退出码, 检查文件数, 违规明细)。

    判定内容取自**区间终点**的 blob，而不是工作区当前内容：同一个文件可能在区间之后
    又被改过，拿工作区内容判会让「区间内新增的形态」与「最终形态」混淆，
    本地复跑历史区间时尤其容易得出假结论。
    """
    rustfmt = shutil.which("rustfmt")
    if not rustfmt:
        raise GateError("PATH 里找不到 rustfmt，无法执行格式判据")
    endpoint = git_range.range_endpoint(rng)
    files = added_rust_files(rng)
    bad: list[str] = []
    for path in files:
        try:
            content = git_range.git("show", f"{endpoint}:{path}")
        except GateError as exc:
            raise GateError(f"区间显示 {endpoint} 新增了 {path}，但取不到内容：{exc}") from exc
        ok, detail = check_text(path, content, rustfmt)
        if not ok:
            bad.append(f"  {path}\n{indent(detail)}")
    if bad:
        return 1, len(files), bad
    return 0, len(files), []


def indent(text: str, prefix: str = "      ") -> str:
    return "\n".join(prefix + line for line in text.splitlines()) or prefix + "(无输出)"


def self_test() -> int:
    """临时仓库正反对照，钉住判据本身。

    用例刻意覆盖三个失败模式：把「查询失败」当通过、把既有文件的格式改动误伤、
    以及拿工作区当前内容冒充区间终点内容。
    """
    ok_src = "pub fn good() -> u32 {\n    1\n}\n"
    bad_src = "pub   fn bad()->u32 {\nreturn 2 }\n"
    with tempfile.TemporaryDirectory(prefix="codex-layer-fmt-gate-") as repo:
        git_range.REPO = repo
        git_range.git("init", "-q")
        git_range.git("config", "user.name", "codex-layer-selftest")
        git_range.git("config", "user.email", "selftest@users.noreply.github.com")

        def write(name: str, content: str) -> None:
            target = os.path.join(repo, name)
            os.makedirs(os.path.dirname(target), exist_ok=True)
            with open(target, "w", encoding="utf-8") as handle:
                handle.write(content)

        def commit(message: str) -> str:
            git_range.git("add", "-A")
            if not git_range.git("diff", "--cached", "--name-only").strip():
                raise GateError(f"自测 commit {message} 没有任何暂存内容")
            git_range.git("commit", "-q", "-m", message)
            return git_range.git("rev-parse", "HEAD").strip()[:7]

        write("README.md", "# 自测仓库\n")
        root = commit("根提交：README（非 Rust）")
        write("src/good.rs", ok_src)
        good = commit("新增规范文件 good.rs")
        write("src/legacy.rs", ok_src.replace("good", "legacy"))
        legacy = commit("新增规范文件 legacy.rs")
        write("src/legacy.rs", ok_src.replace("good", "legacy").replace("    1", "        1"))
        modified = commit("改坏既有文件 legacy.rs 的格式")
        write("src/bad.rs", bad_src)        # 新增即不合格式
        commit("新增不合格式的 bad.rs")

        cases = (
            # 无新增 Rust 文件：放行，计数 0
            (f"{root}..{root}", 0, 0, [], []),
            # 新增规范文件：放行，计数恰为 1（计数是防解析失效的正判据）
            (f"{root}..{good}", 0, 1, [], []),
            (f"{root}..{legacy}", 0, 2, [], []),
            # 只改坏既有文件的格式：不该误伤。legacy..modified 无新增文件；
            # 而 good..modified 里 legacy.rs 属**新增**，其在终点已被改坏，必须判违规
            (f"{legacy}..{modified}", 0, 0, [], []),
            (f"{good}..{modified}", 1, 1, ["src/legacy.rs"], []),
            # 新增不合格式文件：必须被抓并列名
            (f"{modified}..HEAD", 1, 1, ["src/bad.rs"], []),
            # 按区间终点的内容判定：good.rs 是本区间新增、终点形态规范 → 不判它；
            # 它后来在区间外被改坏也不该被牵连（判定只看区间终点）
            (f"{root}..{modified}", 1, 2, ["src/legacy.rs"], []),
            # 不给区间边界时按整棵树判定，README 不该被算进 Rust 文件数
            ("HEAD", 1, 3, ["src/bad.rs", "src/legacy.rs"], ["src/good.rs"]),
        )
        for rng, expect_code, expect_count, must_contain, must_absent in cases:
            code, count, lines = run_gate(rng)
            output = "\n".join(lines)
            if code != expect_code:
                print(
                    f"自测失败：区间 {rng} 退出码 {code}，期望 {expect_code}\n{output}",
                    file=sys.stderr,
                )
                return 1
            if count != expect_count:
                print(
                    f"自测失败：区间 {rng} 检查文件数 {count}，期望 {expect_count}\n{output}",
                    file=sys.stderr,
                )
                return 1
            for needle in must_contain:
                if needle not in output:
                    print(f"自测失败：区间 {rng} 的输出未列出 {needle}", file=sys.stderr)
                    return 1
            for needle in must_absent:
                if needle in output:
                    print(f"自测失败：区间 {rng} 的输出不该列出 {needle}", file=sys.stderr)
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
        "格式门禁自测通过：规范新文件放行且计数正确，错格式新文件被抓并列名，"
        "既有文件的格式改动不误伤，判定取自区间终点而非工作区，不可达基线显式报错。"
    )
    return 0


def main() -> int:
    parser = argparse.ArgumentParser(description="只对本次新增的 *.rs 文件跑 rustfmt --check")
    parser.add_argument("--base", help="基线引用，例如 refs/remotes/origin/main")
    parser.add_argument("--range", dest="range", help="直接给定 rev-list 区间（优先于 --base）")
    parser.add_argument("--self-test", action="store_true", help="跑正反对照自测，不检查真实仓库")
    args = parser.parse_args()

    if args.self_test:
        return self_test()

    try:
        rng, how = resolve(args.range, args.base)
        code, count, lines = run_gate(rng)
    except GateError as exc:
        print(f"格式门禁无法执行：{exc}", file=sys.stderr)
        return 2

    print(f"格式门禁：区间 {rng}（{how}），本次新增 Rust 文件 {count} 个。")
    if code:
        print("以下新增文件不符合 rustfmt 默认风格：", file=sys.stderr)
        print("\n".join(lines), file=sys.stderr)
        print(
            f"\n修正办法：只对新增文件跑 `rustfmt --edition {RUST_EDITION} <文件>`，"
            "不要全仓格式化。",
            file=sys.stderr,
        )
        return 1
    if count == 0:
        print("本次无新增 Rust 文件，无需格式检查。")
    else:
        print("新增文件全部符合 rustfmt 默认风格。")
    return 0


if __name__ == "__main__":
    sys.exit(main())
