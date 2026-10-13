//! codex-layer —— codex relay 的稳定层。
//!
//! 核心能力来自 CodexPlusPlus 的 prompt-prefix stabilizer（PR #2449），
//! 各移植文件头部注明来源文件与 commit。整体分层：
//!
//! 1. 纯函数转换层：`protocol_proxy`（协议互转 + 前缀钉定）、`compaction_align`、
//!    `compaction_tools`、`prefix_divergence`。只收数据与开关，不读任何全局配置。
//! 2. 配置层：`config`（TOML 入口）与 `settings`（内部数据模型，字段名与
//!    CodexPlusPlus 的 settings.json 兼容）。
//! 3. HTTP 管道：`main.rs`，codex 客户端 → 本代理 → 任意 OpenAI 兼容后端。

pub mod compaction_align;
pub mod compaction_tools;
pub mod cli;
pub mod config;
pub mod diagnostic_log;
pub mod http_client;
pub mod paths;
pub mod prefix_divergence;
pub mod protocol_proxy;
pub mod relay_config;
pub mod relay_headers;
pub mod settings;
pub mod vision;
