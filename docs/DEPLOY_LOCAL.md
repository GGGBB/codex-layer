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

## 卸载 / 回滚

```bash
codex-layer stop                                   # 注销常驻
rm ~/Library/LaunchAgents/com.codex-layer.proxy.plist
rm ~/.local/bin/codex-layer                        # 二进制按安装 PREFIX 删
```

## 与本机既有服务的隔离

本机存在用户自持常驻 `com.user.codex-layer`（监听 57331）：包命令的默认 Label
是 `com.codex-layer.proxy`（默认监听 57321），两者互不占用。端到端验证一律
用临时 Label + 临时端口（如 `com.codex-layer.e2e-*` + 5734x），验完 bootout。
