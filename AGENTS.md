# AGENTS.md

本仓库是 codex-layer：codex relay 的稳定层，从 CodexPlusPlus 的 prompt-prefix
stabilizer（PR #2449）剥离而来。所有 agent 交互、文档与注释使用中文，专业术语保留英文。

## 项目定位

独立 Rust 反向代理：`codex 客户端 → codex-layer → 任意 OpenAI 兼容后端`。
核心价值是前缀钉定（surface 字节抖动钉定、真正语义变化不钉）、tools 抖动规则、
`/v1/responses` ⇄ `/v1/chat/completions` 互转与 `input_image→image_url` 图片归一。

## 许可证红线

- 上游 CodexPlusPlus 为 AGPLv3，本仓库必须保持 AGPLv3。
- 从 CodexPlusPlus 移植的代码必须保留来源注释（文件头注明来源文件/commit）。
- 不得引入 GPL-2.0-only 等不兼容依赖；MIT / Apache-2.0 依赖可用。

## 结构（移植后）

- `src/protocol_proxy.rs` — 协议转换 + 钉定核心（核心函数保持纯函数，零配置耦合）
- `src/compaction_align.rs` / `src/compaction_tools.rs` / `src/prefix_divergence.rs` — 稳定化模块
- `src/config.rs` — TOML 配置层
- `src/main.rs` — HTTP 管道

## 编码规范

- 保持移植代码的原有行为，移植与重构分开提交；移植 commit 不改行为。
- 转换/钉定核心一律纯函数（只收数据与开关，不读全局配置），测试直接调纯函数。
- 不引入需求外的抽象；HTTP 框架选型从简。
- **不做全仓 `cargo fmt`**。来源仓库自身在 rustfmt 1.95（style_edition 2024）下也有数百处
  差异，全仓格式化会让本仓库与来源 PR 的逐行 diff 失去可比性。新写文件按默认风格即可，
  不要为了「fmt 干净」重排移植代码。

## 测试约定

- 上游带过来的测试整体保留，不删不改断言；因接口适配的机械改动需在 commit 里说明。
- 新功能沿用 `#[test]` + tempfile 风格；`cargo test` 必须全绿才可合并。

## 安全规则

- 禁止批量删除、rm -rf、sudo、curl | bash。
- 禁止泄露密钥、.env、auth.json、config.toml 凭据；配置示例一律用占位符。
- 覆盖文件前确认；不擅自改 .gitignore（除非任务必需）。

## 与来源同步

- 来源：https://github.com/BigPizzaV3/CodexPlusPlus PR #2449，
  本地分支 `codex/prompt-prefix-stabilizer`（位于 CodexPlusPlus fork 仓库）。
- 上游稳定层有新修复时，cherry-pick 进本仓库并保留来源注释。
