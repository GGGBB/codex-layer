# GAP_ANALYSIS：codex-layer 对照参照实现的差距分析

- 本文仓库：`codex-layer` main @ dde1dbb。
- 参照实现：CodexPlusPlus fork 分支 `personal/stabilizer-agent-payload` @ 11eb685
  （= 上游 main v1.7.7 + PR #2449 前缀稳定层演进形态 + cherry-pick PR #2474 明文 encrypted_content 透传）。
- 移植基线：本仓库 a72520f 切自 PR #2449 较早形态（来源注释标注 commit f9be7cf）；
  参照分支在其后又演进了多项能力，本文逐项核对。
- 实测测试基线（2026-10-13，`cargo test`）：**61 单元 + 163 集成，0 失败**（与 README 声称一致，本数为实跑记录）。
- 模块级 diff 结论：`compaction_align.rs`、`compaction_tools.rs` 与参照除来源注释外**逐字节同步**；
  `prefix_divergence.rs` 仅基线落盘一处差异（见 P0-4）；全部其余差距集中在
  `protocol_proxy.rs`（7692 行 vs 参照 9052 行）与 `main.rs` 装配流。
- 三态结论定义：**建议照搬** = 语义独立、可直接移植；**需适配** = 能力要跟进但须剥离
  CodexPlusPlus 专有耦合（多渠道轮换 / 官方登录 / 聚合供应商 / profile 体系）或改动命名与配置面；
  **不适用** = 依赖 CodexPlusPlus 产品形态，本独立代理不携带。

## P0（正确性缺陷 / 必须跟进）

### P0-1 轮次标记 `<current_user_turn>` 在非 user 收尾轮钉错位置
- 本仓库：`src/protocol_proxy.rs` `mark_current_user_turn`（约 4540 行）——全量解包后用
  `rposition(role=="user")` 找「最后一条 user」钉标记，不检查请求是否以 user 收尾。
- 参照：同函数已改为**仅当 `messages.last()` 为 user 时才钉**，否则解包后直接返回；
  行为被测试 `current_turn_mark_skipped_when_request_ends_with_tool_result` 钉死。
- 后果：工具续轮（末尾是 tool/assistant，日常占大头）把标记钉在历史旧问题上，
  模型把旧问题当「当前指令」，出现回答几天前问题的串线——恰是该机制声称要根治的症状。
- 结论：**建议照搬**（修复是小改动，不依赖任何 CodexPlusPlus 专有结构；同步移植参照测试）。

### P0-2 本地压缩摘要在 `encrypted_content` 无命名空间，原生 opaque 状态被当明文回放
- 本仓库：wrap 路径 `src/protocol_proxy.rs:897` 把明文摘要**裸放**进
  `compaction.encrypted_content`；回放路径 `expand_compaction_item`（约 1022 行）与
  input item 循环的 `Some(COMPACTION_OUTPUT_TYPE)` 分支（约 3502 行）对**任意** compaction
  item 的 `encrypted_content` 一律按明文展开成 user 文本。
- 参照：本地摘要写为 `LOCAL_COMPACTION_PREFIX`（`codexplusplus:summary:v1:` + JSON string，
  `protocol_proxy.rs` 约 1170、870 行），回放只展开带前缀且可解码的项
  （`expand_local_compaction_history`），原生状态一律经 `input_has_native_compaction`
  闸门拒绝（`NativeCompactionUnsupported`，400 `native_compaction_unsupported`）；
  测试 `chat_conversion_rejects_unmarked_or_invalid_compaction_history`、
  `summary_converter_does_not_relabel_native_compaction_as_local_text`。
- 后果：codex 历史里来自其它 Responses 服务器的真加密压缩状态会被本层把 base64 密文
  整段展开进 user 文本喂给上游（静默污染语义与前缀），且无法区分「自家摘要」与「原生状态」。
- 结论：**需适配**。命名空间前缀改用本项目的标识（如 `codexlayer:summary:v1:`）；
  注意切换点对**已持久化的旧裸明文摘要**历史：旧项无前置标记会被新闸门拒绝，
  需保留一次性兼容展开（仅当 input 不含新格式项时回退旧展开）或在 README 声明该边界。
  本仓库既有测试 `tests/protocol_proxy.rs:77`（断言 `PRIOR_SUMMARY` 裸展开）编码的是旧行为，
  移植时要随参照改写并在 commit 说明。

### P0-3 Chat 转换缺 native-compaction / 顶层 `encrypted_content` item 的 fail-loud 闸门
- 本仓库：`responses_to_chat_completions` 的 item 循环对顶层
  `{"type":"encrypted_content", ...}` item 落入兜底 `_ =>` 分支（`src/protocol_proxy.rs` 约 3514 行），
  `content` 为 null 时静默丢弃——等于静默发出一个被抽空任务内容的请求。
- 参照：`responses_to_chat_completions_with_options` 入口先
  `expand_local_compaction_history`，随后 `input_has_native_compaction(&body)` 命中即
  `NativeCompactionUnsupported::history()` 显式报错（`protocol_proxy.rs` 约 1540 行），
  错误体携带可行动文案（换 Responses 上游或新开会话）；内容片段层面空值/opaque 也走
  `UnsupportedEncryptedAgentContent` 结构化 400（不再静默跳过空片段，#2474 语义）。
- 结论：**建议照搬**（`UnsupportedEncryptedAgentContent` / `NativeCompactionUnsupported`
  两个类型与闸门均零外部依赖；结构化错误体 `response_body()` 一并移植）。

### P0-4 探针基线全文落 world-writable `/tmp`，含用户提示词
- 本仓库：`src/prefix_divergence.rs` `dump_baseline_file`——`std::fs::write("/tmp/prefix_baseline_*.txt")`，
  默认权限、任意本机进程可读可预占。基线内容含**用户提示词、工具定义与工作区信息**。
- 参照：同函数已迁到诊断日志私有目录（`diagnostic_log_path().parent()`），
  `OpenOptions::mode(0o600)` + 已存在同名文件覆写前 `set_permissions(0o600)`；
  测试断言权限必须 0600。
- 结论：**建议照搬**（本仓库已有 `crate::diagnostic_log::diagnostic_log_path()`，直接可用）。

### P0-5 `compaction_trigger` 检出存在盲区：非末尾 trigger 整轮漏检
- 本仓库：`src/protocol_proxy.rs` `request_has_compaction_trigger` 只检查「从尾部数第一个
  带 type 的 item」；`strip_compaction_trigger` 只弹尾部。
- 参照：两处均已放宽——检出用 `items.iter().any(type==compaction_trigger)`（注释：
  「当前 Codex 把 trigger 追加在 input 末尾；这里仍接受任意位置，避免漏检」），
  剥离用 `retain` 清全部（且仅在确认上游无原生项、需要改写为本地摘要时调用）。
- 后果：若客户端（含版本演进后）把 trigger 放在非末尾，本层把压缩轮当普通轮：
  不重组 compaction 包装、不回填 tools、不做前缀对齐，codex 端拿不到
  `compaction` 输出项直接报错。
- 结论：**建议照搬**（放宽判定与 `retain` 剥离都是纯函数局部改动；
  `strip_compaction_trigger` 的调用时机要与 P1-1 探针链的「先原生后降级」顺序一起对齐，
  参照中它只在降级路径里执行）。

### P0-6 compaction 请求 `stream=true` 而上游返回 JSON 时，摘要被送进 SSE 解析路径
- 本仓库：`open_responses_proxy_request` 返回
  `is_stream: is_stream || content_type.contains("text/event-stream")`（`src/protocol_proxy.rs:1433`）——请求侧 stream=true
  时无论上游返回什么都标 stream；`src/main.rs` 约 497/579/615 行据此走流式重组，
  把 `application/json` 摘要按 SSE 解析 → 提出 0 段文本 →
  `wrap_empty_upstream_yields_failed_compaction_response` 的 failed 响应。
  对「任意 OpenAI 兼容后端」（部分网关无视 stream 直接回 JSON）是静默错答。
- 参照：同返回点已改为
  `is_stream: (is_stream && !json_compaction) || content_type.contains("text/event-stream")`
  （`protocol_proxy.rs` 约 2367 行），JSON 摘要交非流式包装器。
- 结论：**建议照搬**（判定条件不依赖 rotation；`main.rs` 的包装分路已按此语义存在）。

## P1（能力缺口 / 语义已漂移）

### P1-1 Responses 上游的原生压缩探针链整体缺失（含与本地摘要的合并顺序）
- 本仓库：`upstream_request_parts` 对 `protocol=responses` 的上游在压缩轮**原样透传带 trigger
  的请求**（`synthetic_compaction` 仅 ChatCompletions 分支）：服务端支持原生 v2 时没有任何校验
  （不检查恰好一个 `compaction` 项、不观察流终止事件、不限响应体大小）；
  服务端 400 拒绝 trigger 时直接错到客户端，不会降级本地摘要。
- 参照：完整探针链——`preserve_compaction_trigger` 让原生上游先看到 trigger →
  `read_limited_upstream_body`（32MB 上限）→ `upstream_compaction_item_count`（SSE 感知的
  「恰好一项」计数）/ `upstream_has_terminal_failure` / `compaction_input_rejected`（只读错误体、
  防回显误判）→ 成功即 `native_compaction_passthrough` 透传、流被截断时
  `incomplete_compaction_stream` 显式 `response.failed`、非流式 JSON 原生项
  `render_native_compaction_as_sse` 重组、仅显式拒绝才 `strip + rewrite` 降级本地摘要。
- 结论：**需适配**（剥离 rotation/official_login/aggregate 与多候选重试环后移植纯函数与
  单上游线性流程；`tests/protocol_proxy.rs` 补 `upstream_compaction_item_count`、
  `compaction_input_rejected` 的输入输出对拍与 wiremock 超限测试）。

### P1-2 `NativeResponsesSseObserver`：原生 Responses 透传流的终止事件守卫缺失
- 本仓库：`src/main.rs` 对 Responses 上游流式透传不做任何终止观察，上游在
  `response.completed` 前断流时客户端只见连接关闭，无从判真伪。
- 参照：`NativeResponsesSseObserver`（`protocol_proxy.rs` 约 702-830 行）旁路观察、不改字节、
  事件前缀 64KB 有界；failed/incomplete 优先于 completed，`[DONE]`/EOF 不证明成功；
  装配在参照 `launcher.rs:2552`。
- 结论：**需适配**（观察者本体是纯状态机可照搬；装配点从 launcher.rs 换到本仓库 `main.rs`
  流式透传支路，与 P1-1 一起做）。

### P1-3 `web_search_call` 回放与「Chat 路由不执行服务端原生搜索」守卫链落后
- 本仓库（移植版）：上游把原生 `web_search` 工具当 function tool 调用时，中转层重组为
  `web_search_call` 且**恒 `status:"completed"`**（`src/protocol_proxy.rs` 约 6116-6127、6275 行），
  action 取不到字段时退化为空 `{"type":"search"}`——搜索实际没发生，却对客户端谎报完成，
  模型幻觉被包装成「已搜索」事实；`action` 字段名不认各家网关常用的 `input`。
- 参照（同文件约 7445、7496-7560、3907、311、2945 行）：请求侧
  `remove_unsupported_native_search` 从 Chat 请求剥掉顶层 `web_search` 工具声明（同名
  function/custom/namespace/tool_search 属客户端执行，必须保留；`tool_choice` 强制原生搜索时
  返回结构化 400 `unsupported_native_web_search`）；响应侧伪造调用 → item
  `status:"failed"`，item 含空 action（`web_search_action_has_target`）或任何原生搜索项 →
  整响应 `response.failed` + `invalid_upstream_tool_call`（流式/非流式都覆盖）；
  action 映射接受 `input` 键。
- 结论：**需适配**（守卫逻辑照搬；「Chat 路由是搜索的唯一归宿」这条策略要按本仓库定位复核：
  `remove_unsupported_native_search` 与既有 `is_builtin_web_search_proxy` 代理链的取舍需明确——
  参照的选择是「声明剥离 + 调用标记 failed」，即不假装代理执行搜索）。

### P1-4 规范序产物对空白一刀切 trim，块体首尾空白被改写
- 本仓库：`order_system_chunks_stably`（`src/protocol_proxy.rs` 约 4416-4460 行）对每段
  `text.trim()` 后拼接。
- 参照：引入 `has_named_blocks` 语义分桶——切分结果含具名块时，文本段首尾空白属
  「块间分隔」，归一保幂等；整段只有纯文本时，空白就是块体本身，逐字节透传
  （契约测试 `stabilizer_keeps_chunk_body_whitespace_verbatim`）。
- 后果：纯文本块的首尾空白字节被改写，违背稳定层「非易变内容逐字节不变」的 opt-in 契约
  （参照侧维护者定级 P2，属语义漂移而非稳定性回归——trim 确定性不伤前缀命中）。
- 结论：**建议照搬**（同文件同函数；本仓库移植测试 `prefix_stabilizer_tests` 中的旧断言
  需按参照更新——该改动属「接口/断言适配」，commit 里说明）。

### P1-5 Gemini 系 Chat 上游的 `reasoning_effort` 转发缺失
- 本仓库：`apply_reasoning_effort` / `supports_reasoning_effort`（`src/protocol_proxy.rs`
  约 7331、7449 行）不含 Gemini。
- 参照：`standard_openai_protocol` 开启且模型名含 `gemini` 时映射
  `reasoning_effort`；`gemini-3.8-flash` 进入已验证白名单（`#2464`）。
- 结论：**建议照搬**（本仓库已有 `[relay].standard_openai_protocol` 等价开关，逐行可移植）。

### 已核对为同步 / 不适用项（免二次排查）
- `compaction_align.rs`、`compaction_tools.rs` 全部逻辑与参照同步（含压缩轮 tools 防毒化：
  检出压缩轮即跳过 `remember_tools`，杜绝 `tools:[]` 进回填缓存）；差异仅来源注释与
  `json!` import 位置。
- 探针「已见指纹集合模式」（同 `prompt_cache_key` 多 system 形态交替不误报）：两边一致，
  `prefix_divergence.rs` 的 `observe` 与 `sub_agent_alternates_without_false_divergence` 同形。
- #2474 的**内容片段级**明文 encrypted_content 透传（agent_message content part，
  本仓库 `protocol_proxy.rs:4875-4928`）已在移植中携带，`encrypted_content_value_is_opaque`
  实证判定同步；缺的只是 P0-3 的 item 级闸门与空片段 fail-loud。
- 多渠道轮换（relay_rotation）、官方登录成员（official_login）、聚合供应商
  （aggregate_catalog）、`/timeouts` 与 `[timeouts]` 覆盖、VLM 三态图片策略、
  模型列表/语音/图片旁路探活：CodexPlusPlus 专有产品形态，**不适用**
  （本仓库 README 已声明剥离；`[timeouts]` 反而比参照多出配置面，属本仓库增量）。

## P2（环境与工程改进）

### P2-1 无 CI workflow
- 本仓库：无 `.github/`。参照仓库有 `pr-build.yml` 等 4 条 workflow，但均为 CodexPlusPlus
  构建面（Tauri 产物、多平台矩阵），**不适用**照搬。
- 建议：新增最小 `ci.yml`：`cargo fmt --check`（仅新写文件策略，见 AGENTS.md 的
  不做全仓 fmt 例外说明需落成 CI 口径）、`cargo clippy -- -D warnings`、`cargo test`、
  两条冒烟脚本 `scripts/smoke/{pinning,streaming}.py`。结论：**需适配**（自建极简版）。

### P2-2 缺「与来源 PR 同步」的可重放脚本
- 本仓库：AGENTS.md 声明 cherry-pick 工作流但无可执行入口；参照的 `scripts/` 全部是
  CPP 前端组装脚本，不适用。
- 建议：新增 `scripts/sync_from_upstream.sh`：登记来源 fork 为远端、按
  `chore(port) 基线 f9be7cf..目标 commit` 列出差分文件清单（限定
  `protocol_proxy.rs`/`compaction_*.rs`/`prefix_divergence.rs`/对应测试），输出逐文件 diff
  供人工移植决策，避免下次差距分析全靠手 diff。结论：**需适配**（本仓库专属新写）。

### P2-3 冒烟脚本未接入自动化与版本断言
- 本仓库：`scripts/smoke/*.py` 已可复现（假上游落盘请求体 + 断言逐字节一致），
  但无 `--version` 快速自检、无固定 `CODEX_LAYER_BIN` 构建步骤串联；与 P2-1 的 CI 合流即可。
- 结论：**需适配**（小改：CI job 里 `cargo build --release` 后 export 二进制路径再跑两条脚本）。

## 汇总

| 级别 | 条数 | 建议照搬 | 需适配 | 不适用（记录在案） |
| --- | --- | --- | --- | --- |
| P0 | 6 | P0-1/3/4/5/6 | P0-2 | — |
| P1 | 5 | P1-4/5 | P1-1/2/3 | 轮换/官方登录/聚合/VLM 等（见 P1 末节） |
| P2 | 3 | — | P2-1/2/3 | 参照 CI 构建面 |

落地顺序建议：P0-4（安全，独立最小）→ P0-1/P0-5/P0-6（小而独立）→ P0-2+P0-3（同属
compaction 命名空间与闸门，一次改齐含测试改写）→ P1-4/P1-5 → P1-1+P1-2（探针链一体）→
P1-3 → P2。
