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
维护者（maintainer）负责 review 与合并。

**作者侧硬性禁令（动作级，不是职责描述）**：作者角色的会话不得做下面两件事，
任何情况下都不许，包括「CI 已全绿」「改动很小」「我已经自己复核过了」这几种情形：

1. **不得下 review 结论**：不替维护者写批准口径（「通过」「可以合并」「LGTM」一类），
   也不给自己 `gh pr review` 加任何审批状态。作者可以说明自己核过什么、什么仍是空的，
   但那属于交付说明，不是结论；
2. **不得执行合并**：`gh pr merge`、`gh pr merge --admin`、网页上的 Merge 按钮，
   以及任何等价形式。

交付**止于**「PR 链接 + HEAD SHA」两条信息，写完就停。review 与合并权只在维护者手里。
这条不是分工建议而是禁止动作：单账号仓库拿不到 GitHub 语义上的他人批准，
「先 review 后合并」这道闸门没有技术兜底，全靠这里写死（见流程第 7 条）。

贡献者认领 issue 时**建分支即建 worktree**，二者同时发生（见下节「worktree 隔离」）。

## worktree 隔离（必读）

分支约定在并发下形同虚设：多个贡献者共享同一个 checkout 时，任何一方的
`git switch` / `git checkout` 都会改掉其他人的 HEAD，提交会静默落在别人的
分支上（issue #10 的事故教训）。为此规定：

1. **开工前必须建立自己的工作目录，不得复用主 checkout**。一个角色一个
   worktree，与分支一次性创建：

   ```bash
   git -C /Users/zifengwang/Documents/Codex/codex-layer worktree add \
       /Users/zifengwang/Documents/Codex/codex-layer-workspace/worktrees/<角色> \
       -b agent/<issue号>-<slug> origin/main
   ```

2. **全程只在自己的 worktree 内执行 git 与构建命令**；主 checkout 由维护者
   独占，仅用于 review、合并与 ruleset 运维。对主 checkout 也不要隔空操作
   （`git -C 主checkout ...`、改它的分支指针等）。
3. **构建产物隔离**：worktree 天然拥有独立 `target/`；若显式设置
   `CARGO_TARGET_DIR`，必须用按角色命名的独立目录（如
   `/tmp/codex-layer-target-<角色>`），不得互踩。
4. **开工前自检**：

   ```bash
   git -C <自己的 worktree> rev-parse --abbrev-ref HEAD
   ```

   输出必须等于自己的分支名；不等则**停下并回报**，不允许「先干活再说」。

## 流程

1. **issue 先行**：每个变更先有对应 issue。已开的 issues 优先认领，
   不重复立项。
2. **分支**：从 main 切出，命名 `agent/<issue号>-<slug>`，
   例如 `agent/3-web-search-guard`；在自己的 worktree 内创建
   （见「worktree 隔离」）。
3. **commit 纪律**：不改行为的搬运 / 接入 commit 与行为演进 commit 严格分开。前者
   保持既有内容逐字节原样、不改行为；后者的 commit 正文说明动机、行为影响面
   与测试证据。
4. **PR**：push 分支后开 PR，标题中文，正文按
   [`.github/PULL_REQUEST_TEMPLATE.md`](.github/PULL_REQUEST_TEMPLATE.md) 填写，
   用 `Closes #<issue号>` 关联 issue，打上 `type/*` 与 `P1`/`P2` 标签。
5. **CI**：[`.github/workflows/ci.yml`](.github/workflows/ci.yml) 的 `test` 与
   `smoke` 两个 job 全绿才进 review。
   两个 job 名即 ruleset 要求的状态检查上下文（`cargo test` 与
   `smoke (pinning / streaming)`），任一不绿则 PR 无法合并；且为 strict 模式，
   main 前进后需 rebase 重跑。若出现 "job was not started" 类红，
   先辨别是否为账号计费锁造成的假红，是则重跑（`gh run rerun`）。
   另有 `gates` job 跑两条流程门禁（提交作者角色身份、新增 Rust 文件格式），
   目前**不是** ruleset 必需检查，红了先按下面「流程门禁」一节自查修正。
   另有 `self-approval` job 做事后检测（批准口径 × 合并时间 × 批准后的新提交），
   同样**不是**必需检查，且刻意永不列必需——见下节末条。
6. **review 返工**：在源分支上追加增量 commit，不 force-push（合并前的最终 rebase
   除外）。
7. **merge**：由维护者执行 rebase 合并（ruleset 只允许 rebase 方式，squash 与
   merge commit 均被禁用）。本仓库为单账号仓库，拿不到 GitHub 语义上的
   「他人批准」，因此 **review 结论以 PR 评论形式留痕**（inline 评论 + 明确的
   结论文字；ruleset 要求批准数为 0，但所有 review 讨论线程必须 resolve），
   merge 由维护者在确认无未决线程后执行。
   留痕必须**先于**合并：一条时间戳晚于合并的 review 不可能影响合并决策，
   它只是事后说明，不能当作闸门用过。作者侧的禁止动作见开头「作者侧硬性禁令」。

## 自测纪律（必读）

- 认领 issue 后**尽早、频繁 commit**：本地每跑绿一轮就 commit，具备条件即
  push 分支（WIP 也可），不要等全部做完才第一次提交。本仓库的贡献者是
  agent 会话，已有两次在长任务尾声交出空回报、工作全部留在未提交工作区
  的事故（一次 CLI 修复、一次可观测性数据面），维护者只能靠读 diff 反推
  状态——落盘即安全。回报里若包含「未完成」，也必须写清当前 commit 与
  卡点，**禁止交空回报**。
- 所有 git 与 cargo 命令只在自己的专属 worktree 内执行（主 checkout 由维护者
  独占）；开工先 `git rev-parse --abbrev-ref HEAD` 自检 HEAD 是自己的分支名，
  不等即停手回报；构建产物用独立 `target/` 或按角色命名的
  `CARGO_TARGET_DIR`（详见「worktree 隔离」）。
- 端到端自测**必须**使用临时 Label + 临时端口 + 临时配置，例如
  `codex-layer --label com.codex-layer.test.<任务名> -c <临时配置>` 监听 57341
  之类的非默认端口。
- **严禁** stop / restart / kill 本机真实运行的 `com.codex-layer.proxy`（维护者
  正在使用的推理链路；其监听口以 `codex-layer status` 为准，不要靠记端口号定位），
  严禁改动 `com.user.mlx-ssm-env` LaunchAgent，严禁把 codex 客户端配置临时指向别处。
- `cargo test` 与 `scripts/smoke/*.py` 可直接在仓库内运行（冒烟脚本自带临时目录
  与独立假上游）。
- 一次性 LaunchAgent 用完即 `launchctl bootout`，不留残余注册。

## 安全红线

- **禁**全仓 `cargo fmt`：既有代码保持现状——全仓格式化会一次性产生上百处无关差异，
  淹没真实改动，CI 因此不跑 `fmt --check`，也不跑 `clippy -D warnings`。
  新写文件按 rustfmt 默认风格即可，不要为「fmt 干净」重排既有代码。
- **禁**批量删除文件；删除必须单文件逐个进行并在 PR 中说明理由。
- **禁** sudo、**禁** `curl | bash` 类管道安装。
- **禁**提交任何真实密钥：API key、`.env`、`auth.json`、config.toml 凭据；
  配置示例一律用占位符（见 `codex-layer.toml.example`）。
- `AGENTS.md` 是 agent 本地协作文件，**不入库**（已在 `.gitignore`），
  其中纪律已固化进本文档与 [docs/CODING_STANDARDS.md](docs/CODING_STANDARDS.md)。

## 流程门禁（本地可先跑）

CI 的 `gates` job 里有两条判据，都可以本地先跑，不必等 CI：

```bash
python3 scripts/ci/commit_author_gate.py --base origin/main   # 提交作者角色身份
python3 scripts/ci/new_file_fmt_gate.py --base origin/main    # 本次新增 *.rs 的格式
python3 scripts/ci/tools_cache_isolation_gate.py              # 开稳定层的测试是否隔离了缓存落盘
```

- **提交作者**：区间内每个 commit 的 author 名必须是 `codex-layer-<角色>`，
  邮箱必须是 `<角色>@users.noreply.github.com`，且两侧角色一致。漏带 `-c`
  会静默回落本机全局身份（真实姓名与真实邮箱），这条判据就是为此而设。
  已提交但写岔的，就地补身份：

  ```bash
  git -c user.name="codex-layer-<角色>" \
      -c user.email="<角色>@users.noreply.github.com" \
      commit --amend --reset-author --no-edit
  ```

  合并提交的 author 是合并执行者，不判。
- **新增文件格式**：只判区间内**新增**的 `.rs`，既有文件的格式改动不误伤。
  红了只格式化被点名的那一个文件（`rustfmt --edition 2024 <文件>`），
  **不许**全仓 `cargo fmt`。
- **集成测试缓存隔离**：源文件里出现 `prompt_prefix = true` 即视为会启动开稳定层的
  子进程，必须同时出现 `CODEX_LAYER_TOOLS_CACHE_PATH`。落盘上线后，未注入的测试会往
  **真实状态目录**写 e2e 键（`degrade-ok-*` 这类），真键与测试键混在一个文件里，
  「重启后第一个压缩轮是否命中预热」这个真机判据就废了。污染只出现在开发者本机、
  CI 看不见，所以这条必须在 CI 拦。新增用例时照既有三个文件的 spawn 处注入：

  ```rust
  .env("CODEX_LAYER_HOME", dir.path())
  .env("CODEX_LAYER_TOOLS_CACHE_PATH", dir.path().join("tools-cache.json"))
  ```

两条判据都带 `--self-test`，先自证判据可用再判真实区间。若本地报「无法执行」
（取不到基线、找不到 rustfmt），那是判据自身不可用，会以非零退出，
**不要**把它当成「通过」。

第三条判据交叉的是 GitHub 事件而不是 git 区间，需要 token，本地这样跑：

```bash
python3 scripts/ci/self_approval_gate.py --self-test          # 离线夹具对照
GITHUB_TOKEN=$(gh auth token) python3 scripts/ci/self_approval_gate.py
```

- **它判什么**：禁令生效后合并的每个 PR，必须①合并前存在批准口径的留痕，
  ②批准之后没有未再批准的新提交。批准口径要求结论句式或合并口径，
  裸的「通过」（「cargo test 通过」「判据自测通过」一类技术事实）不算批准。
- **它不判什么**：不判「谁批准」。单账号仓库里批准者与作者永远是同一个登录名，
  那条判据恒真、毫无区分力；能被机器辨认的只有时间顺序与提交顺序。
- **为什么永不列必需检查**：它判的是「合并已经发生、留痕不合规」。列必需会让一次
  历史违规永久卡死后续所有合并，等于让闸门反过来掐住主干。它的定位是信号与记档：
  红了必须当场记档并说明，但不阻塞合并。

## 测试要求

- **外部命令判据必须配正判据**：判据逻辑依赖外部命令（lsof、launchctl 等）的输出集合时，
  「查询失败」与「判据通过」在反向判据下不可区分——空集合会同时表示两者，缺陷因此长期
  隐身。必须有正判据或恒真对照钉住查询本身的健康度（例如已知监听者必须被查得出来），
  并在自测里同时覆盖正反两例与「检查计数 > 0」的假绿防护。
- 合并前基线：lib 单元测试 + `tests/protocol_proxy.rs` 集成测试 + 两条冒烟脚本
  全绿；测试数量**只增不减**（除非对应行为被本 PR 移除，需在 PR 中说明）。
- 既有测试整体保留，不删不改断言；因接口适配的机械改动须在 commit 正文说明。
- 转换 / 钉定核心的新逻辑必须有直接调用纯函数的单元测试，不允许只靠 HTTP 层
  集成测试兜底。

## warning 基线（按构建目标分别记录）

**「零新增」是相对同一条命令的基线，不是绝对数字为零。** main 上本来就带着一批
dead-code warning，而不同命令编译的目标不同，看到的条数天然不一样：拿 `cargo test`
看到的条数去对 `cargo build` 的基线，一定会误判成「自己引入了 warning」，
继而返工去改与本任务无关的死代码。

| 命令 | 编译哪些目标 | 基线条数 |
| --- | --- | --- |
| `cargo build` | 仅 lib（不含 `#[cfg(test)]`、不含 example 与集成测试） | **3** |
| `cargo test`、`cargo check --all-targets` | lib 及其 test cfg + bin + example + 集成测试 | **7** |

基线明细（在 `982f660` 上 `cargo clean -p codex-layer` 后实测；行号会随改动漂移，
按符号名定位更可靠）。两个命令都有的 3 条（lib 本体）：

- `content_starts_signature` never used —— `src/compaction_align.rs:176`
- `first_text_of` never used —— `src/compaction_align.rs:207`
- `message_has_image` never used —— `src/protocol_proxy.rs:4696`

只有带测试 / example 编译才多出的 4 条：

- `OPEN_PAGE_BLOCK` never used —— `src/protocol_proxy.rs:8258`（属于 lib 的
  `#[cfg(test)]` 测试模块，所以 `cargo build` 看不到它）
- `fnv` never used —— `examples/prefix_replay.rs:12`
- `fam_off` unused variable —— `examples/prefix_replay.rs:137`
- `respond_once` never used —— `tests/protocol_proxy.rs:4130`（集成测试）

自查姿势：先固定一条命令（建议 `cargo test`，与 CI 一致），再与本次改动**之前**
同一条命令的输出对比。**不要把上面两个数字当永久常量照抄**：它们会随 main 演进
而变化，当门禁用之前必须在当前 main 上重测一遍。

红线：

- **禁止**为了「消数字」给这些符号加 `#[allow(dead_code)]`。那只是把问题藏起来，
  还会让下一次真正新增的 warning 混在里面看不见。
- 要清理就另开 issue 逐个论证：先 `git grep` 确认确实无人引用（有过先例：某常量
  被断言「上线可见、删了会破坏鉴权」，实测全仓零引用，其实是死代码），再单独
  一笔删除，与行为改动分开提交。
- CI 不跑 `clippy -D warnings`（理由见「安全红线」），所以 warning 只能靠自查，
  基线就得按构建目标写得能对上号。
