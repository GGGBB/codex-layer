# codex-layer

codex relay 的稳定层。The stability layer for codex relays.

`codex-layer` 夹在 [codex](https://github.com/openai/codex) 客户端与**任意 OpenAI 兼容后端**
（mlx-serve、llama.cpp、vLLM，或云端 API）之间，唯一职责是：让后端的 prompt prefix cache
在多轮对话之间保持命中，而不是每一轮都把几万 token 重新预填充一遍。

## 问题在哪

codex 客户端每一轮都重放整段历史。重放出来的 prompt 带有**表层字节抖动**：重新生成的
时间戳、被改写的尾部消息、`tools` 序列化顺序不稳定。这些改动不改变语义，却改变了字节；
而前缀缓存是按字节命中的——一个抖动的字节就能让它后面的全部内容失效。会话越长，代价越
接近每轮几十秒。

## 做什么

- **带语义边界的前缀钉定。** 只有「表层字节抖动」会被钉回第一份稳定字节序列：system 区的
  块集合按块名规范序重排、同名块折叠，到达顺序和拆分边界不再影响产物。真正的语义变化
  一律原样透传——钉定从不掩盖一次真实的编辑。
- **`tools` 抖动规则。** 工具定义只在表层序列化噪声（键序、空白）上钉定；schema 真的变了
  就不钉，避免把该变的东西钉死。
- **协议互转。** `/v1/responses` ⇄ `/v1/chat/completions`，含 SSE 流式回流与压缩轮
  （`/responses/compact`）的 `compaction` 输出项重组，因此上游说哪种方言都能接。
- **媒体归一。** `input_image` 部件归一成 `image_url`，图片输入对任意 OpenAI 兼容的视觉
  后端都可用。
- **前缀分叉探针。** 按 `prompt_cache_key` 逐轮记录「转换后 system 块序列 + tools」的指纹，
  字节突变即 dump 分叉点的新旧对比，用来取证「这一轮为什么冷算」。

对具体推理服务器没有硬编码依赖，对 CodexPlusPlus 本身也没有依赖。

## 构建

需要 Rust 1.85 以上（edition 2024）。

```bash
cargo build --release
./target/release/codex-layer --version
```

## 配置

配置查找顺序：`$CODEX_LAYER_CONFIG` -> `./codex-layer.toml` -> `~/.codex-layer/codex-layer.toml`。

一份能跑的配置只需要 `[upstream]` 三项：

```toml
[upstream]
base_url = "http://127.0.0.1:11234/v1"   # 写到 /v1 即可，协议路径由本层拼接
api_key  = "YOUR_UPSTREAM_KEY"
protocol = "chat_completions"            # 自建推理服务器一般是这个

[stabilizer]
prompt_prefix = true
```

完整字段见 [`codex-layer.toml.example`](codex-layer.toml.example)。未知字段会**直接报错**——
配置写错要立刻炸，不要静默按默认值跑。

默认只监听 `127.0.0.1:57321`：本进程持有上游密钥，不该默认暴露到局域网。本层不校验入站
`Authorization`（回环地址上不必加这一层）；若 codex 要求 `env_key`，填任意值即可。

## 接入 codex

把 codex 的 provider 指到本层，`wire_api` 保持 `responses`：

```toml
# ~/.codex/config.toml
model_provider = "layer"

[model_providers.layer]
name = "codex-layer"
base_url = "http://127.0.0.1:57321/v1"
wire_api = "responses"
env_key = "CODEX_LAYER_KEY"
```

然后 `codex-layer --config codex-layer.toml`，正常用 codex 即可。要同时接多个上游时用
`[[model_route]]` 按模型改路由；本项目**不做**多渠道轮换与限流，那属于 CodexPlusPlus。

## 诊断

诊断日志是 JSONL，默认 `~/.codex-layer/codex-layer.log`，可由 `[log].path` 改写。稳定层的事件：

| 事件 | 含义 |
| --- | --- |
| `protocol_proxy.prefix_stabilizer_prefix_baseline` | 某会话的前缀基线已登记 |
| `protocol_proxy.prefix_stabilizer_prefix_divergence` | 该会话前缀字节突变，附分叉点新旧对比 |
| `protocol_proxy.prefix_stabilizer_dedup_last_folded` | 同名 system 块被折叠 |
| `protocol_proxy.prefix_stabilizer_compaction_appended` | 压缩轮 tools 回填 |
| `protocol_proxy.prefix_stabilizer_unpaired_notice_stripped` | 剥离了孤立的图片占位文本 |

「这一轮为什么重新预填充」先看有没有 `prefix_divergence`：有就说明前缀字节真的变了，
再看 snip 定位是哪一个块变的。主会话与子 Agent 共用 `prompt_cache_key`（线程 id），
多个 system 形态在同键上交替是常态，探针按「已见指纹集」判定，不误报。

## 测试

```bash
cargo test                        # 61 单元 + 163 集成
python3 scripts/smoke/pinning.py  # 钉定行为，进程级
python3 scripts/smoke/streaming.py
```

冒烟脚本会拉起二进制与一个把请求体逐条落盘的假上游，断言：

- 「集合相同、到达序与拆分边界不同」的两轮请求在上游侧**逐字节相同**；
- 块内容真的变了，上游字节必须跟着变（不钉死语义）；
- 关掉 `prompt_prefix` 后同一对请求必须变（反证钉定确实来自本层，而非碰巧）；
- 流式回包的终结事件闭合（`response.output_text.done` / `content_part.done` /
  `output_item.done` / `response.completed` + `[DONE]`），`tool_calls` 轮重组为
  `function_call` 项，`usage` 透传，上游密钥正确注入；
- 前缀探针登记基线，且同键多轮重复内容不误报分叉。

脚本默认在临时目录工作，可用 `CODEX_LAYER_BIN` 指定二进制、`SMOKE_WORK` 指定工作目录。

## 由来

从 [CodexPlusPlus](https://github.com/BigPizzaV3/CodexPlusPlus) 的 prompt 前缀稳定层中继
（[PR #2449](https://github.com/BigPizzaV3/CodexPlusPlus/pull/2449)）中抽出。移植提交与
重构提交严格分开：`chore(port)` 保留逐字节原样与来源注释，`feat` 才做解耦，方便逐 commit
审查。多渠道轮换（relay_rotation）、渠道限流（channel_protection）、聚合模式、VLM 三态
等 CodexPlusPlus 专有功能不进入本项目，已剥离。

## License

AGPLv3，与来源项目一致。见 [LICENSE](LICENSE)。
