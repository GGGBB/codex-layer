# SYNC_BASELINE：与参照实现的同步基线

本文件登记 codex-layer 移植所依据的**参照基线**，配合
[`scripts/sync_check.sh`](../scripts/sync_check.sh) 使用，
用于回答「参照侧这些文件自基线以来又改了什么，本仓库要不要跟进」。

## 基线信息

- 参照仓库：<https://github.com/BigPizzaV3/CodexPlusPlus>（AGPLv3，移植代码须保留来源注释）
- 参照分支：`personal/stabilizer-agent-payload`
- 基线 commit：`11eb685`（fix(proxy): forward plaintext encrypted agent payload to chat upstreams，
  即 cherry-pick PR #2474 后的形态）
- 移植来源：本仓库 `a72520f` 切自 PR #2449 较早形态（来源注释标注 commit `f9be7cf`）；
  基线 `11eb685` 是其后的演进形态，逐项差距见 [GAP_ANALYSIS.md](GAP_ANALYSIS.md)。

## 涉及文件对照表

| 参照仓库路径 | 本仓库路径 |
| --- | --- |
| `crates/codex-plus-core/src/protocol_proxy.rs` | `src/protocol_proxy.rs` |
| `crates/codex-plus-core/src/compaction_align.rs` | `src/compaction_align.rs` |
| `crates/codex-plus-core/src/compaction_tools.rs` | `src/compaction_tools.rs` |
| `crates/codex-plus-core/src/prefix_divergence.rs` | `src/prefix_divergence.rs` |

## 用法

在本地 clone 了 CodexPlusPlus（含参照分支）的机器上：

```bash
./scripts/sync_check.sh /path/to/codexplusplus
```

脚本对每个涉及文件输出 `git log --oneline <基线>..<参照分支> -- <文件>`，
即「自基线以来该文件在参照侧的新提交」；为空表示该文件无需同步。
若本地看不到新提交，先在参照仓库执行 fetch：

```bash
git -C /path/to/codexplusplus fetch --all
```

可选第二个参数覆盖基线 commit（例如回查更早的差分窗口）：

```bash
./scripts/sync_check.sh /path/to/codexplusplus 3d10622
```

## 同步流程约定

1. 跑 `sync_check.sh`，逐文件确认参照侧新提交；
2. 对需要跟进的提交，在参照仓库 `git show <commit>` 审阅改动，
   cherry-pick / 手工移植进本仓库对应文件，**保留来源注释**（注明来源 commit）；
3. 移植与重构分开提交，移植 commit 不改行为（见 AGENTS.md）；
4. 全量跟进后更新本文件的基线 commit，并同步更新 GAP_ANALYSIS.md。
