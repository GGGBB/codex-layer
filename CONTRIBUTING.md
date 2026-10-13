# 贡献指南（CONTRIBUTING）

一句话流程：**issue 先行 → 分支 → PR → CI 全绿 → review → rebase-merge**。
main 保护**已生效**（GitHub ruleset `main-protection`，无 bypass 名单，实测直推被拒）：
必须走 PR、要求的两个状态检查全绿（上下文名 `cargo test` 与
`smoke (pinning / streaming)`）、强制线性历史且只允许 rebase 合并。
一切变更（包括文档与治理）都走 PR。

## 角色与身份

仓库由单一 GitHub 账号提交、多角色协作，角色用 commit author 区分：

```bash
git -c user.name="codex-layer-<角色>" \
    -c user.email="<角色>@users.noreply.github.com" commit -m "..."
```

角色名用英文小写（如 `governance`、`stabilizer`、`ci-maintainer`）。
维护者（maintainer）负责 review 与合并；**贡献者不自 merge 自己的 PR**。

## 流程

1. **issue 先行**：每个变更先有对应 issue。已有 backlog（见 `docs/GAP_ANALYSIS.md`
   与已开的 issues）优先认领，不重复立项。
2. **分支**：从 main 切出，命名 `agent/<issue号>-<slug>`，
   例如 `agent/3-web-search-guard`。
3. **commit 纪律**：移植 commit 与改行为 commit 严格分开。移植 commit 保留来源
   逐字节内容与文件头来源注释（来源仓库/PR/文件/commit），不改行为；
   行为改动 commit 正文说明动机、行为影响面与测试证据。
4. **PR**：push 分支后开 PR，标题中文，正文按
   [`.github/PULL_REQUEST_TEMPLATE.md`](.github/PULL_REQUEST_TEMPLATE.md) 填写，
   用 `Closes #<issue号>` 关联 issue，打上 `type/*` 与 `P1`/`P2` 标签。
5. **CI**：[`.github/workflows/ci.yml`](.github/workflows/ci.yml) 的 `test` 与
   `smoke` 两个 job 全绿才进 review。
   两个 job 名即 ruleset 要求的状态检查上下文（`cargo test` 与
   `smoke (pinning / streaming)`），任一不绿则 PR 无法合并；且为 strict 模式，
   main 前进后需 rebase 重跑。若出现 "job was not started" 类红，
   先辨别是否为账号计费锁造成的假红，是则重跑（`gh run rerun`）。
6. **review 返工**：在源分支上追加增量 commit，不 force-push（合并前的最终 rebase
   除外）。
7. **merge**：由维护者执行 rebase 合并（ruleset 只允许 rebase 方式，squash 与
   merge commit 均被禁用）。本仓库为单账号仓库，拿不到 GitHub 语义上的
   「他人批准」，因此 **review 结论以 PR 评论形式留痕**（inline 评论 + 明确的
   结论文字；ruleset 要求批准数为 0，但所有 review 讨论线程必须 resolve），
   merge 由维护者在确认无未决线程后执行。

## 自测纪律（必读）

- 端到端自测**必须**使用临时 Label + 临时端口 + 临时配置，例如
  `codex-layer --label com.codex-layer.test.<任务名> -c <临时配置>` 监听 57341
  之类的非默认端口。
- **严禁** stop / restart / kill 本机真实运行的 `com.codex-layer.proxy`（57331，
  维护者正在使用的推理链路），严禁改动 `com.user.mlx-ssm-env` LaunchAgent，
  严禁把 codex 客户端配置临时指向别处。
- `cargo test` 与 `scripts/smoke/*.py` 可直接在仓库内运行（冒烟脚本自带临时目录
  与独立假上游）。
- 一次性 LaunchAgent 用完即 `launchctl bootout`，不留残余注册。

## 安全红线

- **禁**全仓 `cargo fmt`：移植代码须与来源 PR 保持逐行 diff 可比性
  （现状有 115 处 fmt 差异，CI 因此不跑 `fmt --check`，也不跑 `clippy -D warnings`）。
  新写文件按 rustfmt 默认风格即可，不要为「fmt 干净」重排移植代码。
- **禁**批量删除文件；删除必须单文件逐个进行并在 PR 中说明理由。
- **禁** sudo、**禁** `curl | bash` 类管道安装。
- **禁**提交任何真实密钥：API key、`.env`、`auth.json`、config.toml 凭据；
  配置示例一律用占位符（见 `codex-layer.toml.example`）。
- `AGENTS.md` 是 agent 本地协作文件，**不入库**（已在 `.gitignore`），
  其中纪律已固化进本文档与 [docs/CODING_STANDARDS.md](docs/CODING_STANDARDS.md)。

## 测试要求

- 合并前基线：lib 单元测试 + `tests/protocol_proxy.rs` 集成测试 + 两条冒烟脚本
  全绿；测试数量**只增不减**（除非对应行为被本 PR 移除，需在 PR 中说明）。
- 上游移植过来的测试整体保留，不删不改断言；因接口适配的机械改动须在 commit
  正文说明。
- 转换 / 钉定核心的新逻辑必须有直接调用纯函数的单元测试，不允许只靠 HTTP 层
  集成测试兜底。
