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

// ———— 运行面：真实 launchctl 调用（macOS），非 macOS 给不支持错误 ————

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

    if kind == "restart" {
        // 旧注册先摘掉；不存在也算成功（restart 等价 start）。
        let _ = run_launchctl(&["bootout", &target]);
    }
    let output = run_launchctl(&["bootstrap", &domain, &plist_path.display().to_string()]);
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        // 新版 macOS 的 bootstrap 错误文本不带身份（Input/output error），
        // 用 launchctl print 复核注册态再下结论。
        let print_ok = run_launchctl(&["print", &target]).status.success();
        if should_report_already_bootstrapped(&stderr, print_ok) {
            bail!(
                "{label} 已在注册（{domain}）；要让新配置/新 plist 生效请用 \
                 `codex-layer restart --label {label}`"
            );
        }
        bail!("launchctl bootstrap 失败：{}", stderr.trim());
    }
    println!("已注册常驻服务：{label}（{domain}）");
    println!("  plist:  {}", plist_path.display());
    println!("  配置:   {}", config_path.display());
    println!("  下一步：`codex-layer status` 看 pid 与监听口");
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
        println!("注册:     未注册");
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
