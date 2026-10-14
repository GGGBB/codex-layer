//! ollama 式包命令：`codex-layer [serve|start|stop|restart|status|version]`。
//!
//! 常驻路径走 macOS LaunchAgent（launchctl bootstrap/bootout/print）。
//! 设计纪律（对应任务书的可测性要求）：
//! - 参数解析（`parse_cli`）、plist 生成（`launch_agent_plist`）、
//!   `launchctl print` 输出解析（`parse_launchd_status`）、`launchd_domain_for_uid`
//!   全是纯函数：不调 launchctl、Linux CI 可跑；Label、端口、路径全部
//!   参数注入——端到端自测用临时 Label + 临时端口，绝不碰真实常驻实例。
//! - serve / version 平台无关；start / stop / restart / status 在非 macOS
//!   给出明确可读的不支持错误。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::bail;

/// 默认 LaunchAgent Label：本项目自己的命名空间。与真实常驻实例
/// （如用户自持的 com.user.*）互不占用；`--label` 可覆盖。
pub const DEFAULT_LABEL: &str = "com.codex-layer.proxy";

/// 非 macOS 平台 launchd 子命令的不支持文案（明确、可读、给出路）。
pub const LAUNCHD_UNSUPPORTED_MESSAGE: &str =
    "常驻注册（start/stop/restart/status）依赖 macOS launchd，当前平台不支持；\
     前台请直接 `codex-layer --config <路径>`（serve），常驻可自行用 systemd/计划任务托管";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    /// 前台运行（默认子命令）。
    Serve { config: Option<PathBuf> },
    /// 生成 plist 并 launchctl bootstrap 注册常驻。
    Start { config: Option<PathBuf>, label: String },
    /// launchctl bootout 注销常驻。
    Stop { config: Option<PathBuf>, label: String },
    /// 重写 plist + bootout + bootstrap，整包重载（plist 与配置一起生效）。
    Restart { config: Option<PathBuf>, label: String },
    /// 解析 launchctl print，给出 pid、监听口、配置路径。
    Status { config: Option<PathBuf>, label: String },
    Version,
    Help,
}

/// 解析命令行。`--config/-c` 全子命令透传；`--label` 只对常驻四命令开放
/// （serve 是前台进程，label 对它没有意义，出现即报错而不是静默忽略）。
pub fn parse_cli(args: &[String]) -> anyhow::Result<Command> {
    // 首个非 flag token 才是子命令；flag 开头则默认 serve，且该 token 必须
    // 回到参数扫描（peek 不消耗，避免把 `-c` / `--help` 自身吞掉）。
    let mut iter = args.iter().peekable();
    let subcommand = match iter.peek() {
        Some(first) if !first.starts_with('-') => {
            iter.next().expect("peek 已确认存在").as_str()
        }
        _ => "serve",
    };
    let mut config_path: Option<PathBuf> = None;
    let mut label: Option<String> = None;
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "-h" | "--help" => return Ok(Command::Help),
            "--version" => return Ok(Command::Version),
            "--config" | "-c" => {
                let Some(value) = iter.next() else {
                    bail!("--config 需要跟一个路径");
                };
                config_path = Some(PathBuf::from(value));
            }
            "--label" => {
                let Some(value) = iter.next() else {
                    bail!("--label 需要跟一个 Label");
                };
                if value.trim().is_empty() {
                    bail!("--label 不能为空");
                }
                label = Some(value.clone());
            }
            other => bail!("未知参数 `{other}`；`codex-layer --help` 查看用法"),
        }
    }
    let label_or_default = || label.clone().unwrap_or_else(|| DEFAULT_LABEL.to_string());
    match subcommand {
        "serve" => {
            if label.is_some() {
                bail!("--label 只对 start / stop / restart / status 有意义；serve 是前台进程");
            }
            Ok(Command::Serve { config: config_path })
        }
        "start" => Ok(Command::Start {
            config: config_path,
            label: label_or_default(),
        }),
        "stop" => Ok(Command::Stop {
            config: config_path,
            label: label_or_default(),
        }),
        "restart" => Ok(Command::Restart {
            config: config_path,
            label: label_or_default(),
        }),
        "status" => Ok(Command::Status {
            config: config_path,
            label: label_or_default(),
        }),
        "version" => Ok(Command::Version),
        other => {
            bail!("未知子命令 `{other}`；可用：serve / start / stop / restart / status / version")
        }
    }
}

/// plist 生成的输入：全部外部事实参数注入（二进制路径、日志路径、
/// 配置与环境变量），launchd 域与 Label 由调用方给定。
#[derive(Debug, Clone)]
pub struct LaunchAgentSpec {
    pub label: String,
    /// 二进制绝对路径（current_exe）。
    pub program: PathBuf,
    /// 追加参数，如 ["serve", "--config", "<绝对配置路径>"]；plist 的
    /// ProgramArguments 首项由 `program` 自动补全（launchd 约定）。
    pub program_args: Vec<String>,
    /// EnvironmentVariables 注入（任务书要求带 CODEX_LAYER_CONFIG）。
    pub environment: BTreeMap<String, String>,
    pub stdout_path: PathBuf,
    pub stderr_path: PathBuf,
    pub working_directory: PathBuf,
}

/// 生成 LaunchAgent plist 文本。KeepAlive + RunAtLoad + 标准输出/错误
/// 重定向固定给出；字符串字段统一 XML 转义（路径含 `&` 等字符不产生
/// 非法 plist）。
pub fn launch_agent_plist(spec: &LaunchAgentSpec) -> String {
    let mut out = String::new();
    out.push_str("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
    out.push_str("<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n");
    out.push_str("<plist version=\"1.0\">\n<dict>\n");
    plist_string_entry(&mut out, "Label", &spec.label);
    plist_string_entry(&mut out, "Program", &spec.program.display().to_string());
    out.push_str("  <key>ProgramArguments</key>\n  <array>\n");
    out.push_str(&format!(
        "    <string>{}</string>\n",
        xml_escape(&spec.program.display().to_string())
    ));
    for arg in &spec.program_args {
        out.push_str(&format!("    <string>{}</string>\n", xml_escape(arg)));
    }
    out.push_str("  </array>\n");
    out.push_str("  <key>EnvironmentVariables</key>\n  <dict>\n");
    for (key, value) in &spec.environment {
        out.push_str(&format!(
            "    <key>{}</key>\n    <string>{}</string>\n",
            xml_escape(key),
            xml_escape(value)
        ));
    }
    out.push_str("  </dict>\n");
    plist_string_entry(
        &mut out,
        "WorkingDirectory",
        &spec.working_directory.display().to_string(),
    );
    plist_string_entry(
        &mut out,
        "StandardOutPath",
        &spec.stdout_path.display().to_string(),
    );
    plist_string_entry(
        &mut out,
        "StandardErrorPath",
        &spec.stderr_path.display().to_string(),
    );
    out.push_str("  <key>RunAtLoad</key>\n  <true/>\n");
    out.push_str("  <key>KeepAlive</key>\n  <true/>\n");
    out.push_str("</dict>\n</plist>\n");
    out
}

fn plist_string_entry(out: &mut String, key: &str, value: &str) {
    out.push_str(&format!(
        "  <key>{}</key>\n  <string>{}</string>\n",
        xml_escape(key),
        xml_escape(value)
    ));
}

fn xml_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

/// launchd 用户域：LaunchAgent 一律 `gui/<uid>`。uid 参数注入（自测可传任意值）。
pub fn launchd_domain_for_uid(uid: u32) -> String {
    format!("gui/{uid}")
}

/// 解析 `id -u` 输出；非纯数字按解析失败处理。
pub fn parse_uid(output: &str) -> Option<u32> {
    output.trim().parse().ok()
}

/// `launchctl print` 的模型视图：注册状态 + pid。
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct LaunchdStatus {
    /// launchd 的 `state = ...` 原文（running / not running ...）。
    pub state: Option<String>,
    pub pid: Option<u32>,
}

/// 解析 `launchctl print <domain>/<label>` 的 stdout。只取顶层
/// `state =` / `pid =` 两行；其余字段（详情转储）不消费。
pub fn parse_launchd_status(output: &str) -> LaunchdStatus {
    let mut status = LaunchdStatus::default();
    for line in output.lines() {
        let line = line.trim_start();
        // 顶层字段先于 detail 子段出现，只认各自的首次命中；嵌套的
        // 同名行（如 jobs 里的 state）不得覆盖顶层值。
        if let Some(rest) = line.strip_prefix("state =") {
            if status.state.is_none() {
                status.state = Some(rest.trim().to_string());
            }
        } else if let Some(rest) = line.strip_prefix("pid =") {
            if status.pid.is_none() {
                status.pid = rest.trim().parse().ok();
            }
        }
    }
    status
}

/// LaunchAgent plist 的落盘位置：`<home>/Library/LaunchAgents/<label>.plist`。
pub fn launch_agent_plist_path(home: &Path, label: &str) -> PathBuf {
    home.join("Library")
        .join("LaunchAgents")
        .join(format!("{label}.plist"))
}

/// launchctl 子命令失败时是否属「服务不存在」。stderr 原文一并回传，
/// 调用方据此给可读文案。
pub fn launchctl_error_is_missing(stderr: &str) -> bool {
    // 实测形态：bootstrap 前查询报 `Could not find service …`；对未注册的
    // label 执行 bootout 报 `Boot-out failed: 3: No such process`。
    stderr.contains("Could not find service") || stderr.contains("No such process")
}

/// launchctl bootstrap 失败时是否属「已在注册」。
pub fn launchctl_error_is_already_bootstrapped(stderr: &str) -> bool {
    stderr.contains("Already exists") || stderr.contains("service already loaded")
}

/// bootstrap 失败后的「已在注册」终判。新版 macOS 对已注册 label 只给
/// `Bootstrap failed: 5: Input/output error`（本机实测），文本不可靠；
/// 用 `launchctl print` 复核注册态作权威判据，stderr 分类兼容旧版。
pub fn should_report_already_bootstrapped(stderr: &str, print_ok: bool) -> bool {
    print_ok || launchctl_error_is_already_bootstrapped(stderr)
}

/// restart 摘除旧注册后、紧随的 bootstrap 失败是否属卸除窗口的瞬态、
/// 应有界重试（#15 期望行为 1）。`start` 没有紧随 bootout（bootout_ran=false）
/// 时不重试。瞬态判据两类：
/// - 旧版形态：`Bootstrap failed: 5: Input/output error (Already exists)`
///   一类带身份文本（复用 `launchctl_error_is_already_bootstrapped`）；
/// - 新版形态（本机 2026-10 restart 复现实测）：只给
///   `Bootstrap failed: 5: Input/output error`，不带身份信息。
///   确定性失败（plist 不存在）也给同形错误，所以重试必须有界，
///   耗尽后由 `launchctl print` 终判（#15 根因：bootout 是异步的）。
pub fn should_retry_bootstrap_after_bootout(bootout_ran: bool, stderr: &str) -> bool {
    if !bootout_ran {
        return false;
    }
    if launchctl_error_is_already_bootstrapped(stderr) {
        return true;
    }
    stderr.contains("Bootstrap failed:") && stderr.contains("Input/output error")
}

/// bootstrap 重试耗尽后的终态判定（#15 期望行为 2）。终判以
/// `launchctl print` 为权威（bootstrap 的 I/O error 文本不带身份，不可靠）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BootstrapTerminalOutcome {
    /// print 复核已注册：bootstrap 其实在卸除窗口内已生效，别再重复卸载。
    Registered,
    /// print 复核未注册：服务已被本次 restart 卸掉，必须显式恢复。
    Unregistered,
}

pub fn bootstrap_terminal_outcome(print_ok: bool) -> BootstrapTerminalOutcome {
    if print_ok {
        BootstrapTerminalOutcome::Registered
    } else {
        BootstrapTerminalOutcome::Unregistered
    }
}

/// 「未注册」终态的完整恢复命令（含 --label 的确切形态，可直接复制）。
pub fn restart_unregistered_recovery(label: &str, config_path: &Path) -> String {
    format!(
        // 子命令必须在前（parse_cli 约定）；flag 在前会被当作 serve 报
        // 未知参数，不可复制的命令没有意义（#15 验收：可直接执行）。
        "codex-layer start --label {label} -c {}",
        config_path.display()
    )
}

/// 监听口探测结果的三分类（#15 次要缺陷 1 的判定侧）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ListenProbe {
    Ready,
    /// connect 被拒：这个地址上确实没人在听。
    Refused,
    /// 超时 / 网络不可达等：无法判定，保守归为不可达。
    Unreachable,
}

pub fn classify_listen_probe(result: &std::io::Result<std::net::TcpStream>) -> ListenProbe {
    match result {
        Ok(_) => ListenProbe::Ready,
        // 本机实测形态：未监听时 connect 报 Connection refused (os error 61)
        Err(error) if error.kind() == std::io::ErrorKind::ConnectionRefused => ListenProbe::Refused,
        _ => ListenProbe::Unreachable,
    }
}

/// 监听口就绪等待窗口（秒，issue #34 期望行为 3）。restart 期间要覆盖
/// 「旧实例有界排空（≤ drain_seconds）+ 进程硬上界宽限 + 新实例 bind 重试
/// 余量」；固定 3 秒的旧窗口比排空上界短一个数量级，必然在未就绪时提前
/// 收工——那行的「未就绪」因此几乎总是误报。
pub fn listen_ready_window_seconds(drain_seconds: u64) -> u64 {
    let covering = drain_seconds + crate::drain::FORCE_EXIT_GRACE.as_secs() + 5;
    covering.max(5)
}

/// restart 就绪等待收口时的证据包（探测侧采集，判定全在纯函数）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RestartWaitEvidence {
    /// 窗口内最后一次探测结果。
    pub last_probe: ListenProbe,
    /// bootout 前记录的旧实例 pid（start 没有）。
    pub old_pid: Option<u32>,
    pub old_alive: bool,
    /// 当前 launchd 注册视图里的 pid。
    pub new_pid: Option<u32>,
    /// 正在监听该端口的进程 pid（lsof 采集）。
    pub listener_pids: Vec<u32>,
    pub drain_seconds: u64,
}

/// 「没能在窗口内确认新实例接管」的三分类判定（issue #34 期望行为 4）。
/// 只报告有证据支撑的结论；判定不了就是判定不了。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RestartWaitOutcome {
    /// 新实例已在监听（窗口耗尽前一瞬才接管）。
    ReadyFresh,
    /// 端口有人监听，但还是旧实例在服务（它在排空上界内会退出并交接）。
    ReadyServedByOld { old_pid: u32, drain_seconds: u64 },
    /// 端口空着、旧进程还活着：旧实例在排空，新实例还没抢到端口。
    OldInstanceDraining { old_pid: u32, drain_seconds: u64 },
    /// 端口被与本次操作无关的进程占着。
    ForeignListener { pids: Vec<u32> },
    /// 新进程已注册（launchd 有 pid）但端口没人听：多半启动即失败。
    NewProcessNotListening,
    /// 三条线索都对不上：如实承认判定不了，给核对命令。
    Indeterminate,
}

/// 判定优先级：外来占用 > 旧实例相关态 > 新进程失败 > 判定不了。
/// 「端口被占」必须先于其他判定——外来占用时旧实例的一切行为都不相干。
pub fn classify_restart_wait(evidence: &RestartWaitEvidence) -> RestartWaitOutcome {
    let foreign: Vec<u32> = evidence
        .listener_pids
        .iter()
        .copied()
        .filter(|pid| Some(*pid) != evidence.old_pid && Some(*pid) != evidence.new_pid)
        .collect();
    if !foreign.is_empty() {
        return RestartWaitOutcome::ForeignListener { pids: foreign };
    }
    if evidence.last_probe == ListenProbe::Ready {
        if let Some(old_pid) = evidence.old_pid {
            if evidence.old_alive && evidence.new_pid == Some(old_pid) {
                return RestartWaitOutcome::ReadyServedByOld {
                    old_pid,
                    drain_seconds: evidence.drain_seconds,
                };
            }
        }
        return RestartWaitOutcome::ReadyFresh;
    }
    if let Some(old_pid) = evidence.old_pid {
        if evidence.old_alive {
            return RestartWaitOutcome::OldInstanceDraining {
                old_pid,
                drain_seconds: evidence.drain_seconds,
            };
        }
    }
    if evidence.new_pid.is_some() && evidence.listener_pids.is_empty() {
        return RestartWaitOutcome::NewProcessNotListening;
    }
    RestartWaitOutcome::Indeterminate
}

/// 各判定态的终端话术。红线（issue #34 期望行为 4）：不许把猜测写成结论；
/// 「仍在排空」不许说成「服务挂了」；就绪但接管者是旧实例时不许报「未就绪」。
pub fn format_restart_wait(
    outcome: &RestartWaitOutcome,
    host: &str,
    port: u16,
    waited_secs: f64,
    label: &str,
) -> String {
    let waited = format!("{waited_secs:.1}");
    match outcome {
        RestartWaitOutcome::ReadyFresh => {
            format!("监听口:   {host}:{port} 已就绪（等待 {waited}s）")
        }
        RestartWaitOutcome::ReadyServedByOld {
            old_pid,
            drain_seconds,
        } => format!(
            "监听口:   {host}:{port} 已就绪，但接管者仍是旧实例（pid={old_pid}）——\
             它在排空上界 {drain_seconds}s 内退出后端口即交接给新实例；\
             稍后用 `codex-layer status --label {label}` 复核 PID 已更换"
        ),
        RestartWaitOutcome::OldInstanceDraining {
            old_pid,
            drain_seconds,
        } => format!(
            "监听口:   {host}:{port} 尚未交接（已等待 {waited}s）——有证据指向：\
             旧实例（pid={old_pid}）仍在排空在途请求（上界 {drain_seconds}s，\
             空窗以此为界）；稍后用 `codex-layer status --label {label}` 复核"
        ),
        RestartWaitOutcome::ForeignListener { pids } => {
            let pids = pids
                .iter()
                .map(u32::to_string)
                .collect::<Vec<_>>()
                .join(", ");
            format!(
                "监听口:   {host}:{port} 未就绪（已等待 {waited}s）——有证据指向：\
                 端口被其它进程占用（pid: {pids}）；核对：\
                 lsof -nP -iTCP:{port} -sTCP:LISTEN"
            )
        }
        RestartWaitOutcome::NewProcessNotListening => format!(
            "监听口:   {host}:{port} 未就绪（已等待 {waited}s）——有证据指向：\
             新进程已注册但未监听，疑似启动即失败；看 launchd stderr 重定向日志\
             （{label}.err.log）里的报错"
        ),
        RestartWaitOutcome::Indeterminate => format!(
            "监听口:   {host}:{port} 未就绪（已等待 {waited}s）——判定不了原因，逐条核对：\
             1) 旧实例排空中（launchctl print 看 PID 与 state）；\
             2) 端口被占（lsof -nP -iTCP:{port} -sTCP:LISTEN）；\
             3) 新进程启动失败（launchd stderr 重定向日志 {label}.err.log）"
        ),
    }
}

/// `status` 对「未注册」的过渡期提示（issue #34 期望行为 5）：
/// restart 的新旧交替窗口里 launchctl print 同样查无此项，终判口吻会误导。
pub fn status_unregistered_hint() -> &'static str {
    "未注册（若刚执行过 restart，这可能只是新旧交替窗口：旧实例排空后才交接；\
     稍后重跑 status 复核）"
}

/// 「已在注册」终态的下一步建议（#15 次要缺陷 3）。不再指向 restart：
/// restart 内部是 bootout+bootstrap，正是会卸服务的路径（#15 事故主体），
/// 建议回路不压在上面；给 status 复核与 stop → start 两段式安全路径。
pub fn already_registered_guidance(label: &str, domain: &str) -> String {
    format!(
        "{label} 已在注册（{domain}），用 `codex-layer status --label {label}` 复核；\
         要让新配置/新 plist 生效，先 `codex-layer stop --label {label}`，\
         确认输出「未注册」后再 `codex-layer start --label {label}`"
    )
}

// ———— 运行面：真实 launchctl 调用（macOS），非 macOS 给不支持错误 ————

/// 卸除窗口重试参数（#15 期望行为 1）。本机实测：bootout 卸除窗口亚秒级
/// 排空（重启往返见 PR #16 实测记录）；上限按 issue 建议约 3s / 10 次量级。
#[cfg(target_os = "macos")]
const BOOTSTRAP_RETRY_MAX_ATTEMPTS: usize = 10;
#[cfg(target_os = "macos")]
const BOOTSTRAP_RETRY_INTERVAL: std::time::Duration = std::time::Duration::from_millis(300);

/// 探测节奏沿用 #15 的实测参数量级；窗口长度改由
/// `listen_ready_window_seconds` 按排空上界推导（issue #34）。
#[cfg(target_os = "macos")]
const LISTEN_READY_INTERVAL: std::time::Duration = std::time::Duration::from_millis(300);

/// 就绪探测的一次采样。
#[cfg(target_os = "macos")]
fn probe_listen(host: &str, port: u16) -> ListenProbe {
    let probe = format!("{host}:{port}")
        .parse::<std::net::SocketAddr>()
        .map_err(std::io::Error::other)
        .and_then(std::net::TcpStream::connect);
    classify_listen_probe(&probe)
}

/// 进程存活探测（`ps -p`）。pid 为 None 恒 false。
#[cfg(target_os = "macos")]
fn process_alive(pid: Option<u32>) -> bool {
    let Some(pid) = pid else {
        return false;
    };
    std::process::Command::new("ps")
        .arg("-p")
        .arg(pid.to_string())
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false)
}

/// 正在监听该端口的进程 pid 集合（lsof `-t` 只吐 pid）。
#[cfg(target_os = "macos")]
fn listener_pids_on_port(port: u16) -> Vec<u32> {
    let output = std::process::Command::new("lsof")
        .args(["-nP", "-iTCP", &format!(":{port}"), "-sTCP:LISTEN", "-t"])
        .output();
    let Ok(output) = output else {
        return Vec::new();
    };
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| line.trim().parse().ok())
        .collect()
}

/// launchd 注册视图里的当前 pid。
#[cfg(target_os = "macos")]
fn launchd_pid(target: &str) -> Option<u32> {
    let output = run_launchctl(&["print", target]);
    if !output.status.success() {
        return None;
    }
    parse_launchd_status(&String::from_utf8_lossy(&output.stdout)).pid
}

/// bootstrap 带卸除窗口重试（#15 期望行为 1）：耗尽或遇确定性错误时返回
/// 最后一错；重试途中成功也算成功。
#[cfg(target_os = "macos")]
fn bootstrap_with_retry(bootout_ran: bool, domain: &str, plist_path: &Path) -> Result<(), String> {
    let plist_arg = plist_path.display().to_string();
    let mut last_error = String::new();
    for attempt in 1..=BOOTSTRAP_RETRY_MAX_ATTEMPTS {
        let output = run_launchctl(&["bootstrap", domain, &plist_arg]);
        if output.status.success() {
            return Ok(());
        }
        last_error = String::from_utf8_lossy(&output.stderr).trim().to_string();
        if !should_retry_bootstrap_after_bootout(bootout_ran, &last_error) {
            break;
        }
        if attempt < BOOTSTRAP_RETRY_MAX_ATTEMPTS {
            std::thread::sleep(BOOTSTRAP_RETRY_INTERVAL);
        }
    }
    Err(last_error)
}

/// 有界等待监听口就绪并如实报告（issue #34 期望行为 2/3/4）。
/// 就绪判据加了「接管者必须不是旧实例」：旧实例排空期间端口照常可连，
/// 只看 connect 会把「旧实例还在服务」报成 restart 完成。
#[cfg(target_os = "macos")]
fn wait_listen_ready_and_report(
    host: &str,
    port: u16,
    drain_seconds: u64,
    old_pid: Option<u32>,
    launchd_target: &str,
    label: &str,
) {
    use std::time::Instant;
    let window =
        std::time::Duration::from_secs(listen_ready_window_seconds(drain_seconds));
    let started = Instant::now();
    // 循环首轮必然赋值后才可能 break/return，无「未初始化读取」路径。
    let mut last_probe;
    loop {
        last_probe = probe_listen(host, port);
        if last_probe == ListenProbe::Ready {
            let current = launchd_pid(launchd_target);
            let stale = old_pid.is_some() && current == old_pid && process_alive(old_pid);
            if !stale {
                println!(
                    "监听口:   {host}:{port} 已就绪（等待 {:.1}s）",
                    started.elapsed().as_secs_f64()
                );
                return;
            }
            // 旧实例仍在服务端口：继续等它按上界退出，直到窗口耗尽。
        }
        if started.elapsed() >= window {
            break;
        }
        std::thread::sleep(LISTEN_READY_INTERVAL);
    }
    let evidence = RestartWaitEvidence {
        last_probe,
        old_pid,
        old_alive: process_alive(old_pid),
        new_pid: launchd_pid(launchd_target),
        listener_pids: listener_pids_on_port(port),
        drain_seconds,
    };
    let outcome = classify_restart_wait(&evidence);
    println!(
        "{}",
        format_restart_wait(&outcome, host, port, started.elapsed().as_secs_f64(), label)
    );
}

/// 常驻命令总入口（start/stop/restart/status）。`config_lookup` 注入
/// 配置路径的默认查找逻辑（Config::default_path），便于测试与复用。
pub fn run_launchd_command(
    command: &Command,
    config_lookup: &dyn Fn() -> PathBuf,
) -> anyhow::Result<()> {
    let (kind, config_arg, label) = match command {
        Command::Start { config, label } => ("start", config, label),
        Command::Stop { config, label } => ("stop", config, label),
        Command::Restart { config, label } => ("restart", config, label),
        Command::Status { config, label } => ("status", config, label),
        _ => return Ok(()),
    };
    #[cfg(target_os = "macos")]
    return run_launchd_macos(kind, config_arg.as_deref(), label, config_lookup);
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (kind, config_arg, label, config_lookup);
        bail!("{LAUNCHD_UNSUPPORTED_MESSAGE}");
    }
}

#[cfg(target_os = "macos")]
fn run_launchd_macos(
    kind: &str,
    config_arg: Option<&Path>,
    label: &str,
    config_lookup: &dyn Fn() -> PathBuf,
) -> anyhow::Result<()> {
    use anyhow::Context;

    let uid = current_uid()?;
    let domain = launchd_domain_for_uid(uid);
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .context("HOME 未设置，无法定位 LaunchAgents 目录")?;
    let target = format!("{domain}/{label}");

    match kind {
        "status" => return print_status(&domain, label, config_arg, config_lookup),
        "stop" => {
            let output = run_launchctl(&["bootout", &target]);
            if !output.status.success() {
                let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
                if launchctl_error_is_missing(&stderr) {
                    println!("未注册：{label}（{domain}）");
                    return Ok(());
                }
                bail!("launchctl bootout 失败：{}", stderr.trim());
            }
            println!("已注销常驻服务：{label}");
            return Ok(());
        }
        _ => {}
    }

    // start / restart：配置与日志路径先备齐（坏配置当场拒绝注册，
    // 避免 KeepAlive crash-loop），plist 重写入盘；restart 先摘旧注册
    // 再 bootstrap（kickstart 不会重读改过的 plist，整包重载更确定）。
    let config_path = prepare_config_path(config_arg, config_lookup)?;
    let (stdout_path, stderr_path) = prepare_log_paths(&config_path, label)?;
    let program = std::env::current_exe().context("无法解析当前二进制路径（current_exe）")?;
    let mut environment = BTreeMap::new();
    environment.insert(
        "CODEX_LAYER_CONFIG".to_string(),
        config_path.display().to_string(),
    );
    let spec = LaunchAgentSpec {
        label: label.to_string(),
        program,
        // 常驻固定 serve + 绝对配置路径：launchd 环境里相对路径的
        // 默认查找不可靠，配置路径写死进 plist。
        program_args: vec![
            "serve".to_string(),
            "--config".to_string(),
            config_path.display().to_string(),
        ],
        environment,
        stdout_path,
        stderr_path,
        working_directory: home.clone(),
    };
    let plist_path = launch_agent_plist_path(&home, label);
    let plist_dir = plist_path.parent().expect("plist 路径必带父目录");
    std::fs::create_dir_all(plist_dir)
        .with_context(|| format!("无法创建 LaunchAgents 目录 {}", plist_dir.display()))?;
    std::fs::write(&plist_path, launch_agent_plist(&spec))
        .with_context(|| format!("无法写入 plist {}", plist_path.display()))?;

    // bootout 之前先记下旧实例 pid：随后「谁在监听/谁还活着」的归因全靠它
    // 区分新旧（issue #34）。start 没有旧实例，保持 None。
    let old_pid = if kind == "restart" {
        launchd_pid(&target)
    } else {
        None
    };
    if kind == "restart" {
        // 旧注册先摘掉；不存在也算成功（restart 等价 start）。
        // 注意 bootout 成功与否决定随后 bootstrap 是否可重试（#15：
        // bootout 异步，卸除窗口内 bootstrap 只给 error 5）。
        let _ = run_launchctl(&["bootout", &target]);
        if let Some(pid) = old_pid {
            println!("旧实例:  pid={pid} 已收到退出信号；若有在途流，按排空上界有界收口后退出");
        }
    }
    let bootout_ran = kind == "restart";
    let bootstrap_error = bootstrap_with_retry(bootout_ran, &domain, &plist_path).err();
    if let Some(stderr) = &bootstrap_error {
        // 重试耗尽或确定性失败：launchctl print 复核注册态作终判。
        let print_ok = run_launchctl(&["print", &target]).status.success();
        if !bootout_ran && should_report_already_bootstrapped(stderr, print_ok) {
            // `start` 撞上已注册服务的典型场景：建议 status 复核 +
            // stop → start 两段式；不再把回路指向 restart
            // （#15 次要缺陷 3：restart 正是会卸服务的路径）。
            bail!("{}", already_registered_guidance(label, &domain));
        }
        match bootstrap_terminal_outcome(print_ok) {
            // bootstrap 的 I/O error 文本不带身份；print 复核为已注册即
            // 视作在卸除窗口内已生效，给出复核指引即可
            // （#15 期望行为 2「已注册」支）。
            BootstrapTerminalOutcome::Registered => println!(
                "已注册常驻服务：{label}（{domain}，末次 bootstrap 报错但 print 复核已生效）"
            ),
            BootstrapTerminalOutcome::Unregistered => bail!(
                "launchctl bootstrap 失败（重试 {BOOTSTRAP_RETRY_MAX_ATTEMPTS} 次耗尽）：{stderr}\n\
                 服务当前未注册（已被本次 {kind} 卸载），恢复命令：{}",
                restart_unregistered_recovery(label, &config_path)
            ),
        }
    } else {
        println!("已注册常驻服务：{label}（{domain}）");
        println!("  plist:  {}", plist_path.display());
        println!("  配置:   {}", config_path.display());
    }
    // 注册成功 / 终判已生效后：有界等待监听口就绪（#15 次要缺陷 1；
    // 窗口按排空上界推导、未就绪归因讲证据，issue #34）。
    // 未就绪不作为失败退出，但只报告有证据支撑的原因。
    match crate::config::Config::load(&config_path) {
        Ok(config) => {
            let host = config.server.host.clone();
            let port = config.server.port;
            let drain_seconds = config.server.drain_seconds;
            wait_listen_ready_and_report(&host, port, drain_seconds, old_pid, &target, label);
        }
        Err(error) => println!("监听口:   未知（配置读取失败：{error}）"),
    }
    Ok(())
}

/// start/restart 用的配置路径：显式 > 注入的默认查找；必须存在且可解析
/// （注册一个 KeepAlive 会 crash-loop 的坏配置是事故源）。
#[cfg(target_os = "macos")]
fn prepare_config_path(
    config_arg: Option<&Path>,
    config_lookup: &dyn Fn() -> PathBuf,
) -> anyhow::Result<PathBuf> {
    use anyhow::Context;
    let candidate = config_arg
        .map(Path::to_path_buf)
        .unwrap_or_else(config_lookup);
    let resolved = candidate
        .canonicalize()
        .with_context(|| format!("配置文件不存在：{}", candidate.display()))?;
    crate::config::Config::load(&resolved)
        .map_err(|error| anyhow::anyhow!("常驻注册前配置校验失败：{error}"))?;
    Ok(resolved)
}

/// launchd 重定向的 stdout/stderr 落点：诊断日志同目录，按 Label 分文件
/// （两个常驻实例共享同一条重定向流会互踩）。
#[cfg(target_os = "macos")]
fn prepare_log_paths(config_path: &Path, label: &str) -> anyhow::Result<(PathBuf, PathBuf)> {
    use anyhow::Context;
    let config = crate::config::Config::load(config_path)?;
    let mut log_dir = if config.log.path.trim().is_empty() {
        crate::paths::default_app_state_dir()
    } else {
        PathBuf::from(&config.log.path)
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| crate::paths::default_app_state_dir())
    };
    if log_dir.as_os_str().is_empty() {
        log_dir = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    }
    std::fs::create_dir_all(&log_dir)
        .with_context(|| format!("无法创建日志目录 {}", log_dir.display()))?;
    Ok((
        log_dir.join(format!("{label}.out.log")),
        log_dir.join(format!("{label}.err.log")),
    ))
}

#[cfg(target_os = "macos")]
fn print_status(
    domain: &str,
    label: &str,
    config_arg: Option<&Path>,
    config_lookup: &dyn Fn() -> PathBuf,
) -> anyhow::Result<()> {
    let output = run_launchctl(&["print", &format!("{domain}/{label}")]);
    let registered = output.status.success();
    let status = parse_launchd_status(&String::from_utf8_lossy(&output.stdout));
    let config_path = config_arg
        .map(Path::to_path_buf)
        .unwrap_or_else(config_lookup);
    println!("Label:    {label}");
    println!("域:       {domain}");
    if !registered {
        // 「未注册」在 restart 的新旧交替窗口里同样是瞬时形态，
        // 终判口吻会误导（issue #34 期望行为 5）。
        println!("注册:     {}", status_unregistered_hint());
    } else {
        let state = status.state.as_deref().unwrap_or("unknown");
        println!("注册:     已注册（state={state}）");
        match status.pid {
            Some(pid) => println!("PID:      {pid}"),
            None => println!("PID:      -（未运行）"),
        }
    }
    println!("配置:     {}", config_path.display());
    // 监听口来自配置（未启动也能报）；配置坏了降级为一行报错而不是崩。
    match crate::config::Config::load(&config_path) {
        Ok(config) => {
            println!("监听口:   {}:{}", config.server.host, config.server.port);
            let log_path = if config.log.path.trim().is_empty() {
                crate::paths::default_app_state_dir().join(crate::paths::DIAGNOSTIC_LOG_FILE)
            } else {
                PathBuf::from(&config.log.path)
            };
            println!("诊断日志: {}", log_path.display());
        }
        Err(error) => println!("监听口:   未知（配置读取失败：{error}）"),
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn current_uid() -> anyhow::Result<u32> {
    use anyhow::Context;
    let output = std::process::Command::new("id")
        .arg("-u")
        .output()
        .context("无法执行 id -u")?;
    anyhow::ensure!(
        output.status.success(),
        "id -u 执行失败：{}",
        String::from_utf8_lossy(&output.stderr).trim()
    );
    parse_uid(&String::from_utf8_lossy(&output.stdout)).context("id -u 输出无法解析为 uid")
}

/// launchctl 薄封装：执行失败（ENOENT 等）合成一个失败 Output，
/// 让调用方只面对「stderr 文本」一种错误形态。
#[cfg(target_os = "macos")]
fn run_launchctl(args: &[&str]) -> std::process::Output {
    std::process::Command::new("launchctl")
        .args(args)
        .output()
        .unwrap_or_else(|error| std::process::Output {
            status: std::process::ExitStatus::default(),
            stdout: Vec::new(),
            stderr: format!("无法执行 launchctl：{error}").into_bytes(),
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn arg(value: &str) -> String {
        value.to_string()
    }

    #[test]
    fn parse_cli_defaults_to_serve_and_passes_config_through() {
        assert_eq!(parse_cli(&[]).unwrap(), Command::Serve { config: None });
        assert_eq!(
            parse_cli(&[arg("serve"), arg("--config"), arg("/tmp/a.toml")]).unwrap(),
            Command::Serve {
                config: Some(PathBuf::from("/tmp/a.toml"))
            }
        );
        // 默认子命令下短参数同样生效
        assert_eq!(
            parse_cli(&[arg("-c"), arg("/tmp/a.toml")]).unwrap(),
            Command::Serve {
                config: Some(PathBuf::from("/tmp/a.toml"))
            }
        );
    }

    #[test]
    fn parse_cli_all_subcommands_accept_config_flag() {
        // --config/-c 全子命令透传（含 serve）
        for name in ["serve", "start", "stop", "restart", "status"] {
            let parsed = parse_cli(&[arg(name), arg("-c"), arg("/tmp/b.toml")]).unwrap();
            let config = match parsed {
                Command::Serve { config } => config,
                Command::Start { config, .. }
                | Command::Stop { config, .. }
                | Command::Restart { config, .. }
                | Command::Status { config, .. } => config,
                other => panic!("意外命令：{other:?}"),
            };
            assert_eq!(config, Some(PathBuf::from("/tmp/b.toml")), "{name}");
        }
        // --label 对常驻四命令 + config 同样透传（serve 拒绝 --label 另测）
        for name in ["start", "stop", "restart", "status"] {
            let parsed = parse_cli(&[
                arg(name),
                arg("--label"),
                arg("com.example.x"),
                arg("-c"),
                arg("/tmp/b.toml"),
            ])
            .unwrap();
            match parsed {
                Command::Start { config, label }
                | Command::Stop { config, label }
                | Command::Restart { config, label }
                | Command::Status { config, label } => {
                    assert_eq!(config, Some(PathBuf::from("/tmp/b.toml")), "{name}");
                    assert_eq!(label, "com.example.x", "{name}");
                }
                other => panic!("意外命令：{other:?}"),
            }
        }
    }

    #[test]
    fn parse_cli_label_defaults_and_override() {
        let parsed = parse_cli(&[arg("start")]).unwrap();
        assert_eq!(
            parsed,
            Command::Start {
                config: None,
                label: DEFAULT_LABEL.to_string(),
            }
        );
        let parsed = parse_cli(&[arg("stop"), arg("--label"), arg("com.user.custom")]).unwrap();
        assert_eq!(
            parsed,
            Command::Stop {
                config: None,
                label: "com.user.custom".to_string(),
            }
        );
    }

    #[test]
    fn parse_cli_rejects_bad_input_loudly() {
        // serve 不接受 --label（静默忽略会埋错）。
        parse_cli(&[arg("serve"), arg("--label"), arg("x")]).unwrap_err();
        // --config 缺值 / --label 缺值 / 空 Label / 未知子命令 / 未知参数
        parse_cli(&[arg("start"), arg("--config")]).unwrap_err();
        parse_cli(&[arg("start"), arg("--label")]).unwrap_err();
        parse_cli(&[arg("start"), arg("--label"), arg("  ")]).unwrap_err();
        parse_cli(&[arg("daemon")]).unwrap_err();
        parse_cli(&[arg("--port"), arg("57341")]).unwrap_err();
    }

    #[test]
    fn parse_cli_help_and_version_win_anywhere() {
        assert_eq!(parse_cli(&[arg("--help")]).unwrap(), Command::Help);
        assert_eq!(
            parse_cli(&[arg("start"), arg("--version")]).unwrap(),
            Command::Version
        );
        assert_eq!(parse_cli(&[arg("version")]).unwrap(), Command::Version);
    }

    fn sample_spec() -> LaunchAgentSpec {
        LaunchAgentSpec {
            label: "com.codex-layer.e2e".to_string(),
            program: PathBuf::from("/opt/bin/codex-layer"),
            program_args: vec![arg("serve"), arg("--config"), arg("/tmp/x.toml")],
            environment: BTreeMap::from([(
                "CODEX_LAYER_CONFIG".to_string(),
                "/tmp/x.toml".to_string(),
            )]),
            stdout_path: PathBuf::from("/logs/codex-layer.out.log"),
            stderr_path: PathBuf::from("/logs/codex-layer.err.log"),
            working_directory: PathBuf::from("/Users/tester"),
        }
    }

    #[test]
    fn plist_contains_required_launchd_keys() {
        let plist = launch_agent_plist(&sample_spec());
        for key in [
            "<key>Label</key>",
            "<string>com.codex-layer.e2e</string>",
            "<key>Program</key>",
            "<key>ProgramArguments</key>",
            "<key>EnvironmentVariables</key>",
            "<key>CODEX_LAYER_CONFIG</key>",
            "<key>WorkingDirectory</key>",
            "<key>StandardOutPath</key>",
            "<key>StandardErrorPath</key>",
            "<key>RunAtLoad</key>",
            "<key>KeepAlive</key>",
        ] {
            assert!(plist.contains(key), "plist 缺少 {key}:\n{plist}");
        }
        // ProgramArguments 首项必须是二进制本体（launchd 约定）：
        // 紧跟 <array> 的第一个 <string>。
        let args_block = plist
            .split("<key>ProgramArguments</key>")
            .nth(1)
            .and_then(|rest| rest.split("</array>").next())
            .expect("ProgramArguments 块");
        let first_string = args_block
            .split("<string>")
            .nth(1)
            .and_then(|rest| rest.split("</string>").next())
            .expect("首项 string");
        assert_eq!(first_string, "/opt/bin/codex-layer");
    }

    #[test]
    fn plist_escapes_special_characters() {
        let mut spec = sample_spec();
        spec.label = "com.x&y".to_string();
        spec.program_args = vec![arg("/tmp/a&b<c>.toml")];
        let plist = launch_agent_plist(&spec);
        assert!(plist.contains("<string>com.x&amp;y</string>"));
        assert!(plist.contains("<string>/tmp/a&amp;b&lt;c&gt;.toml</string>"));
        assert!(!plist.contains("<string>/tmp/a&b"), "裸 & 不得出现");
    }

    #[test]
    fn launchd_domain_and_paths_are_injectable() {
        assert_eq!(launchd_domain_for_uid(501), "gui/501");
        assert_eq!(
            launch_agent_plist_path(Path::new("/Users/t"), "com.example.a"),
            PathBuf::from("/Users/t/Library/LaunchAgents/com.example.a.plist")
        );
        assert_eq!(parse_uid("501\n"), Some(501));
        assert_eq!(parse_uid("abc"), None);
    }

    #[test]
    fn parse_launchd_status_reads_state_and_pid() {
        let sample = "interface => 0x0\n\tstate = running\n\tpid = 4321\n\tjobs = {\n\t\tstate = ignored\n\t}\n";
        let status = parse_launchd_status(sample);
        assert_eq!(status.state.as_deref(), Some("running"));
        assert_eq!(status.pid, Some(4321));
        // 未运行形态：有 state 无 pid
        let status = parse_launchd_status("state = not running\n");
        assert_eq!(status.state.as_deref(), Some("not running"));
        assert_eq!(status.pid, None);
        // 空输入 / 垃圾输出不 panic
        assert_eq!(parse_launchd_status(""), LaunchdStatus::default());
    }

    #[test]
    fn launchctl_error_classification() {
        assert!(launchctl_error_is_missing(
            "Could not find service \"com.codex-layer.proxy\" in domain for user: 501"
        ));
        // 本机实测：对未注册 label 执行 bootout 的真实 stderr 形态
        assert!(launchctl_error_is_missing("Boot-out failed: 3: No such process"));
        assert!(!launchctl_error_is_missing("Operation not permitted"));
        assert!(launchctl_error_is_already_bootstrapped(
            "Bootstrap failed: 5: Input/output error (Already exists)"
        ));
        assert!(!launchctl_error_is_already_bootstrapped("Could not find service"));
    }

    #[test]
    fn already_bootstrapped_terminal_check_uses_print_as_authority() {
        // 本机实测形态：新版 macOS 重复 bootstrap 只给 I/O error 文本，
        // print 复核成功即终判「已在注册」。
        let raw = "Bootstrap failed: 5: Input/output error\nTry re-running the command as root";
        assert!(!launchctl_error_is_already_bootstrapped(raw));
        assert!(should_report_already_bootstrapped(raw, true));
        assert!(!should_report_already_bootstrapped(raw, false));
        // 旧版文本命中 + print_ok 短路同样成立
        assert!(should_report_already_bootstrapped("Already exists", false));
    }

    #[test]
    fn transient_bootstrap_error_after_bootout_is_retried() {
        // 本机实测（2026-10，#15 复现）：bootout 成功后的卸除窗口里，
        // bootstrap 只给不带身份的 error 5 —— 静默卸载事故的根源形态，
        // 必须有界重试。
        let raw = "Bootstrap failed: 5: Input/output error";
        assert!(should_retry_bootstrap_after_bootout(true, raw));
        // 旧版带身份文本：本质是「还没拆完」，同属瞬态。
        assert!(should_retry_bootstrap_after_bootout(
            true,
            "Bootstrap failed: 5: Input/output error (Already exists)"
        ));
        // start 无前置 bootout：同形文本更可能是确定性失败（plist 非法等），
        // 重试救不回来，立刻失败交给终态判定。
        assert!(!should_retry_bootstrap_after_bootout(false, raw));
        // 瞬态族之外的失败不重试。
        assert!(!should_retry_bootstrap_after_bootout(
            true,
            "Bootstrap failed: 125: Operation not permitted"
        ));
    }

    #[test]
    fn bootstrap_terminal_outcome_print_is_authority() {
        // 终判以 launchctl print 为权威：已注册→给复核指引；
        // 未注册→明确已被本次操作卸载 + 可直接复制的恢复命令（见下）。
        assert_eq!(
            bootstrap_terminal_outcome(true),
            BootstrapTerminalOutcome::Registered
        );
        assert_eq!(
            bootstrap_terminal_outcome(false),
            BootstrapTerminalOutcome::Unregistered
        );
    }

    #[test]
    fn unregistered_recovery_command_is_directly_copyable() {
        let cmd = restart_unregistered_recovery(
            "com.codex-layer.test.restart5",
            std::path::Path::new("/tmp/smoke/config.toml"),
        );
        assert_eq!(
            cmd,
            "codex-layer start --label com.codex-layer.test.restart5 -c /tmp/smoke/config.toml"
        );
        // roundtrip：恢复命令本身必须能被 parse_cli 解回 Start，
        // 「可直接复制」才不是空话。
        let tokens: Vec<String> = cmd.split(' ').skip(1).map(str::to_string).collect();
        match parse_cli(&tokens).expect("恢复命令必须可被解析") {
            Command::Start { config, label } => {
                assert_eq!(label, "com.codex-layer.test.restart5");
                assert_eq!(
                    config.unwrap(),
                    std::path::PathBuf::from("/tmp/smoke/config.toml")
                );
            }
            other => panic!("期望 Start，实为 {other:?}"),
        }
    }

    #[test]
    fn already_registered_guidance_never_points_at_restart() {
        // #15 次要缺陷 3：旧文案把「已在注册」的出路指向 restart，
        // 而 restart 正是会卸服务的路径；新文案建议 status 复核 +
        // stop → start 两段式，全文不得出现 restart。
        let text = already_registered_guidance("com.codex-layer.proxy", "gui/501");
        assert!(!text.contains("restart"), "{text}");
        assert!(
            text.contains("status --label com.codex-layer.proxy"),
            "{text}"
        );
        assert!(
            text.contains("stop --label com.codex-layer.proxy"),
            "{text}"
        );
        assert!(
            text.contains("start --label com.codex-layer.proxy"),
            "{text}"
        );
        // 顺序约束：先 stop、确认未注册、再 start。
        let stop_at = text.find("stop --label").unwrap();
        let start_at = text.find("start --label").unwrap();
        assert!(stop_at < start_at, "{text}");
    }

    #[test]
    fn guidance_and_recovery_strings_are_clean_single_line() {
        // 多行字符串续行极易把 `\"` 当续行写错，留下裸换行与散字符；
        // 用单行性把这些文案钉死。
        let text = already_registered_guidance("com.codex-layer.proxy", "gui/501");
        assert!(!text.contains('\n'), "{text:?}");
        assert!(!text.contains("\""), "{text:?}");
        let cmd = restart_unregistered_recovery(
            "com.codex-layer.proxy",
            std::path::Path::new("/tmp/cfg.toml"),
        );
        assert!(!cmd.contains('\n'), "{cmd:?}");
    }

    #[test]
    fn listen_ready_window_covers_drain_bound() {
        // 排空上界 45s 时窗口必须 ≥ 45 + 宽限（10）+ 余量（5）。
        let window = listen_ready_window_seconds(45);
        assert!(
            window >= 45 + crate::drain::FORCE_EXIT_GRACE.as_secs() + 5,
            "窗口 {window}s 盖不住排空上界"
        );
        // drain=0 也要留出 bind 余量；任何取值都必须严格大于排空上界。
        assert!(listen_ready_window_seconds(0) > 0);
        assert!(listen_ready_window_seconds(0) >= 15);
        for drain in [0u64, 1, 45, 600] {
            assert!(listen_ready_window_seconds(drain) > drain);
        }
    }

    fn evidence(last_probe: ListenProbe, listeners: Vec<u32>) -> RestartWaitEvidence {
        RestartWaitEvidence {
            last_probe,
            old_pid: Some(100),
            old_alive: true,
            new_pid: Some(200),
            listener_pids: listeners,
            drain_seconds: 45,
        }
    }

    #[test]
    fn restart_wait_attribution_three_way_split() {
        // 类 1：旧实例仍在排空（端口空着、旧进程活着、launchd 已是新 pid）。
        let outcome = classify_restart_wait(&evidence(ListenProbe::Refused, vec![]));
        assert_eq!(
            outcome,
            RestartWaitOutcome::OldInstanceDraining {
                old_pid: 100,
                drain_seconds: 45
            }
        );
        // 类 2：端口被无关进程占着——外来占用优先级最高，
        // 即便旧实例还活着也不能把锅排给排空。
        // 旧实例 pid 出现在监听列表里不算「外来占用」，必须被滤掉；
        // 真正的外来者（777）单独成立。
        let mut foreign = evidence(ListenProbe::Refused, vec![777]);
        foreign.listener_pids.push(100);
        assert_eq!(
            classify_restart_wait(&foreign),
            RestartWaitOutcome::ForeignListener { pids: vec![777] }
        );
        // 类 3：新进程注册了但端口没人听，且旧进程已经死了。
        let mut dead_old = evidence(ListenProbe::Refused, vec![]);
        dead_old.old_alive = false;
        assert_eq!(
            classify_restart_wait(&dead_old),
            RestartWaitOutcome::NewProcessNotListening
        );
        // 端口连得上、注册视图里还是旧 pid、旧进程活着：旧实例仍在服务。
        let mut serving = evidence(ListenProbe::Ready, vec![100]);
        serving.new_pid = Some(100);
        assert_eq!(
            classify_restart_wait(&serving),
            RestartWaitOutcome::ReadyServedByOld {
                old_pid: 100,
                drain_seconds: 45
            }
        );
        // 端口连得上且接管者已换新 pid：就绪。
        let fresh = evidence(ListenProbe::Ready, vec![200]);
        assert_eq!(classify_restart_wait(&fresh), RestartWaitOutcome::ReadyFresh);
        // 全部线索落空：如实判定不了（start 场景无旧 pid、无注册、无监听）。
        let mut blank = evidence(ListenProbe::Unreachable, vec![]);
        blank.old_pid = None;
        blank.old_alive = false;
        blank.new_pid = None;
        assert_eq!(
            classify_restart_wait(&blank),
            RestartWaitOutcome::Indeterminate
        );
    }

    #[test]
    fn restart_wait_wording_reports_evidence_not_guesses() {
        // 旧实例排空态：话术必须说「排空」并给出上界，不得出现「未就绪」
        // 或旧文案里写死的「端口被占、上游不可达」。
        let text = format_restart_wait(
            &RestartWaitOutcome::OldInstanceDraining {
                old_pid: 100,
                drain_seconds: 45,
            },
            "127.0.0.1",
            12333,
            60.2,
            "com.codex-layer.proxy",
        );
        assert!(text.contains("排空"), "{text}");
        assert!(text.contains("45"), "{text}");
        assert!(!text.contains("未就绪"), "{text}");
        assert!(!text.contains("上游不可达"), "{text}");
        // 「旧实例仍在服务」同样不得报「未就绪」。
        let text = format_restart_wait(
            &RestartWaitOutcome::ReadyServedByOld {
                old_pid: 100,
                drain_seconds: 45,
            },
            "127.0.0.1",
            12333,
            60.0,
            "l",
        );
        assert!(!text.contains("未就绪"), "{text}");
        // 判定不了必须明说判定不了并给核对命令，不许把猜测写成结论。
        let text = format_restart_wait(
            &RestartWaitOutcome::Indeterminate,
            "127.0.0.1",
            12333,
            9.0,
            "l",
        );
        assert!(text.contains("判定不了"), "{text}");
        assert!(text.contains("lsof"), "{text}");
        // 端口占用态给出 pid 与 lsof 命令。
        let text = format_restart_wait(
            &RestartWaitOutcome::ForeignListener { pids: vec![7, 8] },
            "127.0.0.1",
            12333,
            9.0,
            "l",
        );
        assert!(text.contains("7, 8"), "{text}");
        assert!(text.contains("-iTCP:12333"), "{text}");
    }

    #[test]
    fn status_unregistered_hint_marks_transition_window() {
        let hint = status_unregistered_hint();
        assert!(hint.contains("restart"), "{hint}");
        assert!(hint.contains("排空"), "{hint}");
        assert!(!hint.contains('\n'), "{hint:?}");
    }

    #[test]
    fn listen_probe_classifies_real_connection_outcomes() {
        // Ready 真实分支：bind + connect 临时端口，不靠 sleep。
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let connected = std::net::TcpStream::connect(addr);
        assert_eq!(classify_listen_probe(&connected), ListenProbe::Ready);
        // 本机实测形态：未监听时 connect 报 Connection refused (os error 61)。
        let refused = Err(std::io::Error::new(
            std::io::ErrorKind::ConnectionRefused,
            "Connection refused (os error 61)",
        ));
        assert_eq!(classify_listen_probe(&refused), ListenProbe::Refused);
        // 超时/不可达等无法判定的形态保守归为 Unreachable。
        let timeout = Err(std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            "Connection timed out (os error 60)",
        ));
        assert_eq!(classify_listen_probe(&timeout), ListenProbe::Unreachable);
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn launchd_command_is_clearly_unsupported_off_macos() {
        let command = Command::Start {
            config: None,
            label: DEFAULT_LABEL.to_string(),
        };
        let error = run_launchd_command(&command, &|| PathBuf::from("unused.toml"))
            .expect_err("非 macOS 必须明确报错");
        assert!(error.to_string().contains("launchd"), "{error}");
        assert!(error.to_string().contains("serve"), "{error}");
    }
}
