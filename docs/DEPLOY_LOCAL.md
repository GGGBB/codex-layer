# 本机部署与运维（macOS LaunchAgent）

本文收录本机的安装、launchctl 那套运维命令与日志排查。示例默认 Label
`com.codex-layer.proxy`、配置 `~/.codex-layer/codex-layer.toml`。

## 安装

```bash
./install.sh                      # → $HOME/.local/bin/codex-layer
PREFIX=$HOME/bin ./install.sh     # 自定义安装目录（须用户可写，脚本不用 sudo）
```

脚本行为：`cargo build --release` → 临时文件 + 原子 mv 落位（常驻实例跑着
旧二进制也可安全重装）→ 打印下一步指引。PATH 未含 PREFIX 时只打印建议行，
不改任何 shell rc。

## 日常运维

```bash
codex-layer status                # 注册状态 / PID / 监听口 / 配置与日志路径
codex-layer start                 # 注册并拉起常驻（坏配置当场拒绝注册）
codex-layer restart               # 改完配置后整包重载（重写 plist 再 bootstrap）
codex-layer stop                  # 注销（进程随 bootout 退出）
codex-layer --config /path/to/x.toml status   # 配置路径全子命令透传
```

多实例并跑（如灰度实例）用独立 Label + 独立配置（监听口在配置里，天然隔离）：

```bash
codex-layer start --config ~/layer-gray.toml --label com.codex-layer.gray
codex-layer status --label com.codex-layer.gray
```

## launchctl 直查（程序外壳等价命令）

包命令背后就是这几条；排查 launchd 侧问题时直接用：

```bash
UID_N=$(id -u)
launchctl print gui/$UID_N/com.codex-layer.proxy          # status 的数据源（state/pid）
launchctl print gui/$UID_N/com.codex-layer.proxy >/tmp/print.txt 2>&1  # 完整转储
launchctl kickstart -k gui/$UID_N/com.codex-layer.proxy   # 只重启进程（不重读 plist；
                                                            # 改了配置请用 restart）
launchctl list | grep codex-layer                          # 快速看 pid/上次退出码
```

注意：`kickstart -k` 不会重读改过的 plist；`start/restart` 每次都会重新生成
plist（ProgramArguments 里配置路径为绝对路径），保证 plist 与磁盘一致。

## plist 与文件位置

| 内容 | 位置 |
| --- | --- |
| LaunchAgent plist | `~/Library/LaunchAgents/<label>.plist`（程序生成，勿手改；restart 会重写） |
| launchd 标准输出/错误 | `<日志目录>/<label>.out.log` / `.err.log`（日志目录 = 配置 `[log].path` 的父目录，默认 `~/.codex-layer`） |
| 诊断日志（JSONL） | 配置 `[log].path`，默认 `~/.codex-layer/codex-layer.log` |
| 配置默认查找 | `$CODEX_LAYER_CONFIG` → `./codex-layer.toml` → `~/.codex-layer/codex-layer.toml` |

plist 里 ProgramArguments 为 `<二进制> serve --config <绝对路径>`，
EnvironmentVariables 带 `CODEX_LAYER_CONFIG`；KeepAlive + RunAtLoad，
崩溃自动拉起、登录自启。

## 监听端口覆盖

默认监听 `127.0.0.1:12333`。换端口只改配置的 `[server] port`：实际监听口**只来自配置**。
launchd 常驻时 plist 里**不含端口**——`ProgramArguments` 只有
`serve --config <绝对路径>`，`EnvironmentVariables` 只带 `CODEX_LAYER_CONFIG` 等，
所以改完配置直接 `codex-layer restart` 就生效，不必也不该去 plist 里塞端口。

环境变量 `CODEX_LAYER_PROXY_PORT` **不改变监听端口**，它只设定「回环守卫」认定为本代理
的端口：当图片上游 base_url 被误配成指回本地代理自身时，守卫据此直接拒答并明确报错，
而不是发出一次真实的回环请求。守卫的识别集除了这个生效端口，还包含新旧默认端口，
因此改号前把上游配成旧默认 57321 的用户仍会被拦下。历史变量名
`CODEX_PLUS_PROTOCOL_PROXY_PORT` 仍会被读取，但未设新名时才生效；两者同时设为不同
值时以新名为准。旧名属弃用入口：只要它参与生效，诊断日志会写一条
`codex_layer.deprecated_env`（每进程仅一次）提醒改名。

## 关停与排空（restart 为什么会等一下、等多久有上限）

收到退出信号（launchd bootout / restart / 前台 Ctrl-C）后，进程按三层收口：

1. **立即停止接收新请求**（既有优雅关停语义）。
2. **在途流最多再跑 `[server].drain_seconds` 秒**（默认 45）：界内自然跑完的
   回合完全不受打扰；到点仍未结束的流由本层补一帧**显式终止事件**收口
   （Responses 流为 `response.failed`，错误类型 `drain_deadline`；Chat 直通流
   为 SSE 错误帧）——被截断的一轮流绝不会伪装成正常完成，客户端拿到的
   永远是可判定的结果，下一轮重发即可。
3. **进程硬上界 = drain_seconds + 10 秒宽限**：个别连接不读字节之类的病态
   场景到点直接退出进程、释放监听口。三层加起来，**重启的无人监听空窗从此
   有界**：最坏 ≈ drain_seconds + 宽限 + launchd 拉起新实例的秒级开销。

`drain_seconds` 的取舍：调小（如 5）→ 重启几乎即刻可用，代价是在途回合更
容易被收口；调大（如 120）→ 回合几乎不被腰斩，代价是重启停顿变长；
设 `0` → 完全不等待，信号到达即收口在途流。改动后 `restart` 生效。

`restart` 的就绪等待窗口按上式自动推导，并在未就绪时**按证据报告原因**：
「旧实例仍在排空（含上界秒数）」「端口被其它进程占用（含 pid 与核对命令）」
「新进程已注册但未监听（疑似启动即失败）」——判定不了就明说判定不了并列出
三条核对命令，不再把猜测写死成「端口被占、上游不可达」。刚 restart 后
`status` 若显示「未注册」，输出会同时提示这可能只是新旧交替窗口，稍后重跑
复核即可。

## 日志事件排查

`codex-layer status` 报「未注册」→ `launchctl print` 的 stderr 核对 Label/域；
`state = not running` → 看 `<label>.err.log`，多为配置问题（端口占用、上游地址空）。

启动后前缀仍冷算 → 查诊断日志事件：

```bash
tail -f ~/.codex-layer/codex-layer.log | rg prefix_stabilizer
```

| 事件 | 判读 |
| --- | --- |
| `prefix_stabilizer_prefix_baseline` | 该会话前缀基线已登记（正常） |
| `prefix_stabilizer_prefix_divergence` | 前缀字节真实突变，附分叉点新旧对比，定位到块 |
| `prefix_stabilizer_dedup_last_folded` | 同名时区块被折叠（instances 计数 = 客户端累积注入次数） |
| `prefix_stabilizer_compaction_appended` | 压缩轮 tools 回填发生 |
| `prefix_stabilizer_unpaired_notice_stripped` | 孤立图片占位残片被移除 |
| `codex_layer.started` / `conversion_rejected` | 常驻启动 / 转换层主动拒答（400 闸门） |
| `codex_layer.deprecated_env` | 端口覆盖仍在用历史变量名，附新名与生效端口 |

## 卸载 / 回滚

```bash
codex-layer stop                                   # 注销常驻
rm ~/Library/LaunchAgents/com.codex-layer.proxy.plist
rm ~/.local/bin/codex-layer                        # 二进制按安装 PREFIX 删
```

## 与本机既有服务的隔离

包命令的默认 Label 是 `com.codex-layer.proxy`，默认监听 12333；历史上用过的两个端口
——旧默认 57321 与用户自持常驻曾占的 57331——都不在新默认的占用范围内，因此本机
已有的自持实例不会与它抢端口。同机还要跑第二个实例时，必须换 Label 并在配置里显式
写 `[server] port`。端到端验证一律用临时 Label + 临时端口（如
`com.codex-layer.e2e-*` + 5734x），验完 bootout。
