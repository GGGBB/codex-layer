#!/usr/bin/env python3
"""锁文件版本一致性门禁：`Cargo.lock` 里的包版本必须等于 `Cargo.toml` 的版本。

立项依据（本机真实踩到的两类代价）：
1. 只改 `Cargo.toml` 而不同步 `Cargo.lock` 时，任何一次 `cargo` 调用都会**悄悄重写
   锁文件**，工作区凭空多出未提交改动，`git pull` 被挡（本仓库已两次因此先 stash 再
   drop 才能继续）；
2. 一旦有人（或 CI）用 `cargo build --locked`，会直接报
   「cannot update the lock file because --locked was passed」——发版链路上就是红。

根因是发版 bump 只改一处。判据刻意做得极窄：只比对本包自己的版本行，
不校验依赖解析结果（那属于 `--locked` 的职责，且会把正常的依赖升级拦在门外）。

判据本体是纯函数 `decide`，`--self-test` 带正反对照；两个文件任一读不到都显式失败，
绝不把「读不到」当成「一致」。
"""

from __future__ import annotations

import argparse
import os
import re
import sys

PACKAGE = "codex-layer"


class GateError(RuntimeError):
    """判据自身不可用时抛出——绝不降级成「没有违规」。"""


def manifest_version(text: str) -> str:
    """取 `[package]` 段里第一条 `version = "x.y.z"`。"""
    m = re.search(r"^\[package\][^\[]*?^version\s*=\s*\"([^\"]+)\"", text, re.M | re.S)
    if not m:
        raise GateError("Cargo.toml 里找不到 [package] 段的 version 行")
    return m.group(1)


def lock_version(text: str, package: str = PACKAGE) -> str:
    """取锁文件里该包那条 `[[package]]` 的 version。"""
    pattern = re.compile(
        r"^\[\[package\]\]\nname = \"" + re.escape(package) + r"\"\nversion = \"([^\"]+)\"",
        re.M,
    )
    found = pattern.findall(text)
    if len(found) != 1:
        raise GateError(f"Cargo.lock 里本包条目数异常（{len(found)} 条），判据无法定论")
    return found[0]


def decide(manifest: str, locked: str) -> str | None:
    """一致返回 None；不一致返回人读的差异。"""
    return None if manifest == locked else f"Cargo.toml={manifest} 而 Cargo.lock={locked}"


def read(path: str) -> str:
    try:
        with open(path, encoding="utf-8") as handle:
            return handle.read()
    except OSError as exc:
        raise GateError(f"读不到 {path}：{exc}") from exc


def self_test() -> int:
    manifest = '[package]\nname = "codex-layer"\nversion = "0.3.3"\nedition = "2024"\n'
    ok_lock = 'version = 4\n\n[[package]]\nname = "codex-layer"\nversion = "0.3.3"\n'
    bad_lock = 'version = 4\n\n[[package]]\nname = "codex-layer"\nversion = "0.3.2"\n'
    other_lock = 'version = 4\n\n[[package]]\nname = "axum"\nversion = "0.3.2"\n'

    if manifest_version(manifest) != "0.3.3":
        print("自测失败：manifest 版本解析错误", file=sys.stderr)
        return 1
    if lock_version(ok_lock) != "0.3.3":
        print("自测失败：lock 版本解析错误", file=sys.stderr)
        return 1
    if decide("0.3.3", "0.3.3") is not None:
        print("自测失败：一致版本被判违规", file=sys.stderr)
        return 1
    if decide("0.3.3", "0.3.2") is None:
        print("自测失败：漂移版本未被抓出", file=sys.stderr)
        return 1
    # 锁文件里根本没有本包条目：判据自身失效，必须显式报错而不是放行
    try:
        lock_version(other_lock)
    except GateError:
        pass
    else:
        print("自测失败：锁文件缺本包条目却未报错", file=sys.stderr)
        return 1
    print(
        "锁文件版本门禁自测通过：一致放行、漂移判红、"
        "manifest 与 lock 的解析各自正确、锁文件缺本包条目显式报错。"
    )
    return 0


def main() -> int:
    parser = argparse.ArgumentParser(description="断言 Cargo.lock 的包版本等于 Cargo.toml")
    parser.add_argument("--self-test", action="store_true", help="跑夹具对照，不查真实仓库")
    parser.add_argument("--repo", help="仓库目录（默认当前目录）")
    args = parser.parse_args()

    if args.self_test:
        return self_test()

    repo = args.repo or os.getcwd()
    try:
        manifest = manifest_version(read(os.path.join(repo, "Cargo.toml")))
        locked = lock_version(read(os.path.join(repo, "Cargo.lock")))
    except GateError as exc:
        print(f"锁文件版本门禁无法执行：{exc}", file=sys.stderr)
        return 2

    reason = decide(manifest, locked)
    if reason:
        print(f"锁文件版本漂移：{reason}", file=sys.stderr)
        print(
            "\n修正办法（离线即可，不访问网络）：\n"
            "  cargo metadata --format-version 1 --offline > /dev/null\n"
            "然后把 Cargo.lock 与被改的 Cargo.toml 一起提交——发版 bump 必须两处在同一笔。",
            file=sys.stderr,
        )
        return 1
    print(f"锁文件版本与清单一致（{manifest}）。")
    return 0


if __name__ == "__main__":
    sys.exit(main())
