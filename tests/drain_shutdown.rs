//! 关停排空端到端测试（issue #34 核心验收）：起真实二进制 + 刻意长跑的
//! 假上游，验证「长流拖住重启」被上界钉死——
//! 1. 进程在配置的上界内退出（不再是数分钟的无人监听空窗）；
//! 2. 被收口的那一路流带有可判定的终止结果（显式 `response.failed` 事件，
//!    不是静默成功、不是无终止挂死）；
//! 3. 上界两态：设 0 → 立即收口；设大值 → 在途流在界内自然跑完、不受打扰。
//!
//! 端到端全部用临时目录 + 临时端口；绝不触碰 12333 上的真实常驻实例。

use serde_json::{Value, json};
use std::io::Read;
use std::net::SocketAddr;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// 测试守卫：Drop 时兜底杀进程，断言失败也不留常驻二进制。
struct ChildGuard(Child);

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

impl ChildGuard {
    fn pid(&self) -> u32 {
        self.0.id()
    }

    /// 轮询等待进程退出；返回退出耗时，超时返回 None。
    async fn exit_in(&mut self, budget: Duration) -> Option<Duration> {
        let started = Instant::now();
        loop {
            if self.0.try_wait().unwrap().is_some() {
                return Some(started.elapsed());
            }
            if started.elapsed() >= budget {
                return None;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }
}

fn reserve_port() -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.local_addr().unwrap().port()
}

/// 假上游：一条连接、一段永不终结的 chat SSE（或按剧本自然收尾）。
/// `terminal` = false：每 `gap` 吐一个增量 chunk 后挂住不结束（复现
/// 「一轮回答数分钟」的稳态）；true：先吐增量，600ms 后 finish_reason +
/// [DONE] 自然收尾（用于「界内自然跑完」一态）。
async fn fake_chat_upstream(
    listener: TcpListener,
    gap: Duration,
    terminal: bool,
) -> std::io::Result<()> {
    let (mut socket, _peer) = listener.accept().await?;
    // 读掉请求头与请求体，别让 reqwest 侧 ECONNRESET 干扰断言。
    let mut request = Vec::new();
    let mut buffer = [0u8; 4096];
    let header_end = loop {
        let read = socket.read(&mut buffer).await?;
        if read == 0 {
            return Ok(());
        }
        request.extend_from_slice(&buffer[..read]);
        if let Some(index) = request
            .windows(4)
            .position(|window| window == b"\r\n\r\n")
        {
            break index + 4;
        }
    };
    let headers = String::from_utf8_lossy(&request[..header_end]).to_ascii_lowercase();
    let content_length: usize = headers
        .lines()
        .find_map(|line| line.strip_prefix("content-length:"))
        .and_then(|value| value.trim().parse().ok())
        .unwrap_or(0);
    while request.len() < header_end + content_length {
        let read = socket.read(&mut buffer).await?;
        if read == 0 {
            return Ok(());
        }
        request.extend_from_slice(&buffer[..read]);
    }

    socket
        .write_all(
            b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncache-control: no-cache\r\n\r\n",
        )
        .await?;
    let chunk = b"data: {\"id\":\"drain\",\"created\":1,\"model\":\"m\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"a\"},\"finish_reason\":null}]}\n\n";
    socket.write_all(chunk).await?;
    if terminal {
        tokio::time::sleep(Duration::from_millis(600)).await;
        let final_chunk = b"data: {\"id\":\"drain\",\"created\":1,\"model\":\"m\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"b\"},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n";
        let _ = socket.write_all(final_chunk).await;
        let _ = socket.shutdown().await;
        return Ok(());
    }
    loop {
        tokio::time::sleep(gap).await;
        if socket.write_all(chunk).await.is_err() {
            return Ok(());
        }
    }
}

struct Harness {
    dir: tempfile::TempDir,
    proxy_port: u16,
    child: ChildGuard,
}

impl Harness {
    /// 发一路流式 POST，读到达响应体（说明在途流已建立）为止。
    async fn start_streaming_request(&self) -> TcpStream {
        let mut stream = TcpStream::connect(("127.0.0.1", self.proxy_port))
            .await
            .expect("连接代理失败");
        let body = json!({"model": "m", "stream": true, "input": "drain probe"}).to_string();
        stream
            .write_all(
                format!(
                    "POST /v1/responses HTTP/1.1\r\nhost: x\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{}",
                    body.len(),
                    body
                )
                .as_bytes(),
            )
            .await
            .unwrap();
        // 等第一帧转换后的 Responses 事件下发（上游每 gap 吐一块，这里给足余量）。
        let mut prefix = Vec::new();
        let mut buffer = [0u8; 4096];
        let deadline = Instant::now() + Duration::from_secs(8);
        loop {
            let read = tokio::time::timeout(Duration::from_millis(600), stream.read(&mut buffer))
                .await
                .unwrap_or(Ok(0))
                .unwrap();
            prefix.extend_from_slice(&buffer[..read]);
            if String::from_utf8_lossy(&prefix).contains("data:") {
                break;
            }
            assert!(Instant::now() < deadline, "在途流 8 秒内没有下发任何数据帧");
        }
        stream
    }

    fn send_sigterm(&self) {
        // 与 launchd bootout 同形态的外部 SIGTERM；不用内置测试钩子。
        let status = Command::new("kill")
            .arg("-TERM")
            .arg(self.child.pid().to_string())
            .status()
            .expect("kill -TERM 执行失败");
        assert!(status.success(), "SIGTERM 发送失败");
    }

    async fn read_to_eof(stream: &mut TcpStream) -> String {
        let mut text = String::new();
        let mut buffer = [0u8; 8192];
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            match tokio::time::timeout(remaining, stream.read(&mut buffer)).await {
                Ok(Ok(0)) | Err(_) => break,
                Ok(Ok(read)) => text.push_str(&String::from_utf8_lossy(&buffer[..read])),
                Ok(Err(_)) => break,
            }
        }
        text
    }

    fn diagnostic_lines(&self) -> Vec<Value> {
        let path = self.dir.path().join("codex-layer.log");
        let mut text = String::new();
        std::fs::File::open(&path)
            .unwrap_or_else(|error| panic!("读不到诊断日志 {}: {error}", path.display()))
            .read_to_string(&mut text)
            .unwrap();
        text.lines()
            .filter_map(|line| serde_json::from_str(line).ok())
            .collect()
    }
}

async fn spawn_harness(drain_seconds: u64, gap: Duration, terminal: bool) -> Harness {
    let dir = tempfile::tempdir().unwrap();
    let proxy_port = reserve_port();

    let upstream_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream_addr: SocketAddr = upstream_listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = fake_chat_upstream(upstream_listener, gap, terminal).await;
    });

    let config_path = dir.path().join("codex-layer.toml");
    std::fs::write(
        &config_path,
        format!(
            r#"
[server]
host = "127.0.0.1"
port = {proxy_port}
drain_seconds = {drain_seconds}

[upstream]
base_url = "http://{upstream_addr}/v1"
api_key = "sk-drain-test"
protocol = "chat_completions"

[log]
path = "{}"
"#,
            dir.path().join("codex-layer.log").display()
        ),
    )
    .unwrap();

    let child = Command::new(env!("CARGO_BIN_EXE_codex-layer"))
        .args(["serve", "--config"])
        .arg(&config_path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();

    // 等 /healthz 就绪（最多 5 秒）。
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match healthz(proxy_port).await {
            Ok(status) if status == "200" => break,
            _ if Instant::now() < deadline => tokio::time::sleep(Duration::from_millis(100)).await,
            _ => panic!("codex-layer 未在 5 秒内就绪"),
        }
    }

    Harness {
        dir,
        proxy_port,
        child: ChildGuard(child),
    }
}

async fn healthz(port: u16) -> std::io::Result<String> {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).await?;
    stream
        .write_all(b"GET /healthz HTTP/1.1\r\nhost: x\r\nconnection: close\r\n\r\n")
        .await?;
    let mut raw = String::new();
    stream.read_to_string(&mut raw).await?;
    let head = raw.split("\r\n\r\n").next().unwrap_or(raw.as_str());
    Ok(head
        .split_whitespace()
        .nth(1)
        .unwrap_or("?")
        .to_string())
}

/// 核心验收：慢流在途时收到 SIGTERM——
/// 进程必须在 `drain_seconds`（2s）加宽限内退出，且该路流带显式
/// `response.failed` 终止事件；诊断日志同步记 `stream_failed`。
#[tokio::test(flavor = "multi_thread")]
async fn long_running_stream_is_bounded_by_drain_deadline() {
    let drain_seconds = 2u64;
    let mut harness = spawn_harness(drain_seconds, Duration::from_millis(300), false).await;
    let mut client = harness.start_streaming_request().await;

    harness.send_sigterm();
    let signaled = Instant::now();

    let output = Harness::read_to_eof(&mut client).await;
    assert!(
        output.contains("event: response.failed"),
        "被排空收口的一轮流必须带可判定的终止事件，实际输出尾部：{}",
        &output[output.len().saturating_sub(600)..]
    );
    assert!(
        !output.contains("event: response.completed"),
        "排空收口不得伪装成正常完成"
    );

    let elapsed = harness
        .child
        .exit_in(Duration::from_secs(drain_seconds + 8))
        .await
        .expect("进程必须在上界 + 宽限内退出（旧行为：随最长流无上界漂移）");
    // 计时从发信号起算；容忍 CI 调度抖动，上界 + 6 秒内必须已退出。
    let since_signal = signaled.elapsed();
    assert!(
        since_signal < Duration::from_secs(drain_seconds + 6),
        "退出耗时 {elapsed:?} / 距信号 {since_signal:?} 超出上界预期"
    );

    let failed_reasons: Vec<String> = harness
        .diagnostic_lines()
        .iter()
        .filter(|record| record["event"] == "codex_layer.stream_failed")
        .filter_map(|record| record["detail"]["reason"].as_str().map(str::to_string))
        .collect();
    assert!(
        failed_reasons.iter().any(|reason| reason.contains("排空")),
        "诊断日志必须有排空收口的 stream_failed，实际：{failed_reasons:?}"
    );
}

/// 两态之一：上界设 0 → 信号一到立即收口，进程秒退（重启即刻可用）。
#[tokio::test(flavor = "multi_thread")]
async fn zero_drain_terminates_immediately() {
    let mut harness = spawn_harness(0, Duration::from_millis(300), false).await;
    let mut client = harness.start_streaming_request().await;

    harness.send_sigterm();
    let signaled = Instant::now();

    let output = Harness::read_to_eof(&mut client).await;
    assert!(
        output.contains("event: response.failed"),
        "drain=0 的收口同样必须显式可判定"
    );
    harness
        .child
        .exit_in(Duration::from_secs(6))
        .await
        .expect("drain=0 时进程必须迅速退出");
    assert!(signaled.elapsed() < Duration::from_secs(6));
}

/// 两态之二：上界给足（3s）时，界内能自然跑完的流必须自然收尾——
/// 排空的正确姿态是「等它跑完」，而不是腰斩。
#[tokio::test(flavor = "multi_thread")]
async fn natural_completion_within_drain_is_not_interrupted() {
    let mut harness = spawn_harness(3, Duration::from_millis(300), true).await;
    let mut client = harness.start_streaming_request().await;

    harness.send_sigterm();

    let output = Harness::read_to_eof(&mut client).await;
    assert!(
        output.contains("event: response.completed"),
        "界内自然完成的流必须以正常完成事件收尾，实际尾部：{}",
        &output[output.len().saturating_sub(600)..]
    );
    assert!(
        !output.contains("event: response.failed"),
        "自然完成不得被排空逻辑画蛇添足补失败帧"
    );
    harness
        .child
        .exit_in(Duration::from_secs(9))
        .await
        .expect("自然收尾后进程应立即退出（无需等满上界）");
}
