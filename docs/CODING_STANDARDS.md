# 编码规范（CODING_STANDARDS）

从仓库既有纪律（原 AGENTS.md，本地协作文件，不入库）与现行代码提炼的**有依据、
可执行**条目。与 [CONTRIBUTING.md](CONTRIBUTING.md) 冲突时以 CONTRIBUTING.md 为准。

## 模块边界与纯函数纪律

- 转换 / 钉定核心保持**纯函数**：只接收数据与显式开关（options），不读全局配置、
  不摸环境变量；配置由 `src/config.rs` 的 TOML 层解析，在 `src/main.rs` 装配时注入。
  先例：`src/protocol_proxy.rs` 的转换与稳定化入口均携带 options 结构体。
- 稳定化能力按职责独立成模块，从转换层入口调用：
  `src/compaction_align.rs`（压缩轮前缀对齐）、`src/compaction_tools.rs`
  （压缩轮 tools 回填）、`src/prefix_divergence.rs`（前缀分叉探针）。
  新能力优先开新模块，不继续堆进 `protocol_proxy.rs`。

## 来源注释与许可证

- 移植文件必须保留文件头 `移植来源：…` 注释，写明来源仓库/PR/文件/commit；
  该注释是移植区与行为改动的对齐锚点、也是审计依据，不得删改。
- 上游参照实现为 AGPLv3，本仓库必须保持 AGPLv3；不得引入 GPL-2.0-only
  等不兼容依赖（MIT / Apache-2.0 可用）。

## 诊断日志

- 统一 JSONL，经 `diagnostic_log::append_diagnostic_log(event, detail)` 写入，
  路径由 `[log].path` 配置（默认 `~/.codex-layer/codex-layer.log`）。
- 事件名为点分命名空间 `<模块>.<主题>_<动作>`：进程级用 `codex_layer.*`
  （如 `codex_layer.stream_open` / `codex_layer.stream_completed` /
  `codex_layer.conversion_rejected`），转换与稳定层用 `protocol_proxy.*`
  （如 `protocol_proxy.prefix_stabilizer_prefix_baseline` /
  `protocol_proxy.prefix_stabilizer_prefix_divergence`）。
- 新增事件须沿用该命名，并登记进 README 的诊断事件表。

## 错误处理

- 对客户端的拒绝必须走**结构化错误体**：错误类型实现
  `response_body() -> serde_json::Value`，`error.code` 用 snake_case 机器码，
  message 用可行动的中文文案（先例：`unsupported_encrypted_agent_content`、
  `native_compaction_unsupported`）。
- fail-loud 原则：无法解析、opaque、被抽空语义的内容一律显式 400 拒绝或显式
  `response.failed`，**不得静默跳过或空播**（P0-3 的教训：静默丢弃
  `encrypted_content` item 等于发出抽空任务内容的请求）。
- 配置未知字段直接报错启动失败，不静默按默认值跑。

## 测试组织

- 三层结构：
  1. 单元对拍——各源文件 `#[cfg(test)]` 模块，直接调用纯函数；
  2. 跨模块请求/响应回放——`tests/protocol_proxy.rs`（HTTP 层与转换链集成）；
  3. 进程级字节断言——`scripts/smoke/*.py`（拉起真实二进制 + 落盘请求体的
     假上游，断言上游侧逐字节一致 / 必变）。
- 触及文件系统的用例用 `tempfile`（已是 dev-dependency），不落固定公共路径。
- 探针基线等含用户数据的诊断落盘物必须 `0o600` 且位于私有日志目录，
  禁写 world-writable 的 `/tmp` 裸文件（P0-4 的先例）。

## 风格与格式化

- 注释与文档用中文，专业术语保留英文。
- **不做全仓 `cargo fmt`**（移植代码与来源 PR 有 115 处 fmt 差异，重排会让
  逐行 diff 失去可比性）；新写文件按 rustfmt 默认风格即可。
- 移植 commit 不改行为；接口适配的机械改动（含测试断言改写）必须在 commit
  正文说明。

## Git 并发

- 多贡献者环境下 `git switch` / `git checkout` 等改 HEAD 的命令属**危险操作**：
  共享同一 checkout 时会连带改掉其他人的工作目录（#10 事故）。必须在专属
  worktree 内执行，隔离规范见 CONTRIBUTING.md「worktree 隔离」。
