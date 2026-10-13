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
- **原生搜索谎报守卫。** Chat 路由不执行服务端原生搜索：请求里顶层 `web_search`
  声明一律剥除（客户端执行的同名工具保留），`namespace` 子项里的原生声明同样剥除，
  命名空间只在「被本层剥空」时才整项移除；`tool_choice` 强制到被剥掉的项时给结构化 400
  `unsupported_native_web_search`；上游伪造的搜索调用只产 `status:"failed"` 的
  `web_search_call`，并使整响应 `response.failed` + `invalid_upstream_tool_call`，
  流式与非流式同口径：本层不把模型幻觉包装成「已搜索完成」。
  同名冲突按上行工具名判定，客户端声明优先（回程归 `function_call` /
  `custom_tool_call` 正常形态）；`response.failed` 就是流式终态，本层不再补
  `data: [DONE]`，HTTP body 随后收尾。
- **媒体归一。** `input_image` 部件归一成 `image_url`，图片输入对任意 OpenAI 兼容的视觉
  后端都可用。
- **前缀分叉探针。** 按 `prompt_cache_key` 逐轮记录「转换后 system 块序列 + tools」的指纹，
  字节突变即 dump 分叉点的新旧对比，用来取证「这一轮为什么冷算」。

对具体推理服务器与任何特定上游实现都没有硬编码依赖。

## 构建

需要 Rust 1.85 以上（edition 2024）。

```bash
cargo build --release
./target/release/codex-layer --version
```

## 快速开始（ollama 式包命令）

仓库根目录提供 `install.sh`：`cargo build --release` 后把二进制装到
`$HOME/.local/bin`（`PREFIX` 环境变量可覆盖；全程不用 sudo，不动 shell rc），
结束时打印下一步指引。装完后终端直接敲：

| 命令 | 行为 |
| --- | --- |
| `codex-layer` | 前台运行（默认子命令，等价 `codex-layer serve`） |
| `codex-layer start` | macOS 注册 LaunchAgent 常驻：程序生成 plist（KeepAlive + RunAtLoad，标准输出/错误
重定向到日志目录，Environment 带 `CODEX_LAYER_CONFIG`）后 bootstrap |
| `codex-layer stop` | `launchctl bootout` 注销 |
| `codex-layer restart` | 重写 plist + bootout + bootstrap 整包重载（新配置/新 plist 直接生效） |
| `codex-layer status` | 打印注册状态与 PID（解析 `launchctl print`）、监听口、配置与日志路径 |
| `codex-layer version` | 打印版本 |

`--config/-c` 全子命令透传；`--label`（默认 `com.codex-layer.proxy`）只对常驻四命令
开放，用于多实例并跑与测试实例隔离。start / stop / restart / status 仅 macOS；
非 macOS 上它们给明确可读的不支持错误，serve / version 平台不受限。
本机部署与 launchctl 运维命令见 [docs/DEPLOY_LOCAL.md](docs/DEPLOY_LOCAL.md)。

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
`[[model_route]]` 按模型改路由；本项目**不做**多渠道轮换与限流，属多渠道产品
形态，不在本项目范围。

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

### 请求关联 id 与 `/stability` 数据面

`/v1/responses` 的每一轮会在 `codex_layer.stream_open` 与收口事件
（`stream_completed` / `stream_failed`，含压缩轮）的 `detail.requestId` 里带
同一个进程内 UUID，并发下可把开流与收口精确配对、算出单轮耗时。
**requestId 只进诊断日志**：发往上游的请求体与请求头逐字节不含它
（出站体每轮多一个随机字段等于前缀每轮都变，深缓存全废），集成测试
`tests/stability_endpoint.rs` 钉住了这条红线。

只读端点 `GET /stability`（路由始终注册在 `fallback` 之前；`[stability]`
的 `enabled = true` 才返回快照）：惰性读诊断
日志尾部窗口（默认最近 8MB），返回聚合快照——轮次计数、baseline/divergence
及比率、钉定器生效次数、按 requestId 配对的单轮耗时 p50/p95/max（配不上的
显式计入 `unpaired`，不静默丢）。隐私：`oldSnip`/`newSnip` 可能含用户提示词，
默认只给长度与位置；`show_prompt_snippets = true` 才输出截断到 120 字符的
片段并在响应里带警示字段。响应不含 API key、配置全文。默认关时该端点
返回 404「未启用」。

### `/stability/ui` 监看界面

`GET /stability/ui` 返回全自包含 HTML 页（零前端依赖、零 CDN，随二进制
打包，CSS/JS 全内联）：页内轮询 `/stability`，指标卡展示轮次计数与耗时
分位，SVG 时间轴按时间铺开轮次与分叉事件（`outcome=null` 的行渲染成
「开着未收口」，终态-only 行照实展示），下方两张表列分叉明细与最近 20 条
轮次。控件只有两个：窗口大小（以 `?window_bytes=` 覆盖读取窗口，钳进
1KB–64MB，非法值明确 400）与自动刷新。数据面未启用时页面渲染成可读的
禁用引导而不是报错。

快照含提示词长度/位置等元数据，**只应通过回环地址访问**（`[server]` 的
`host` 保持 `127.0.0.1`）；若监听非回环地址，请自行在反向代理或防火墙层
加访问控制。

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

## 项目定位

这一层夹在 codex 客户端与任意 OpenAI 兼容后端之间，只解决一件事：多轮重放里
的表层字节抖动反复打断前缀缓存，让会话越深、每轮冷预填充越贵。问题机制与
能力清单见上文，这里只说过程纪律与边界：

- 不改行为的搬运 / 接入 commit 与行为演进 commit 严格分开：前者保持既有代码
  逐字节原样（连格式化也不做），`feat` / `fix` 才做解耦与行为演进，
  方便逐 commit 审查。
- 多渠道产品形态的能力（多渠道轮换、渠道限流、聚合供应商、VLM 三态等）
  不进入本项目。

## License

AGPLv3，见 [LICENSE](https://github.com/GGGBB/codex-layer/blob/main/LICENSE)。
