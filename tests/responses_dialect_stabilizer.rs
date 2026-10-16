//! 原生 Responses 方言下的稳定层可见性集成测试（issue #52）：起真实二进制，
//! 上游为原生 /v1/responses 方言，断言
//! 1) 压缩轮（`tools: []` + 末尾摘要指令）的对齐同样生效：回填主会话 tools 字节、
//!    指令保持在末尾，且压缩轮不污染 tools 回填缓存；
//! 2) 分叉探针在该方言下确实在跑：同键主/子形态乒乓不误报，真语义变化必报
//!    （排除「零分叉是因为探针压根没跑」的假阳性）。

use serde_json::{Value, json};
use std::io::Read;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

struct ChildGuard(Child);

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn reserve_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// 假的原生 Responses 上游：服务 n 次 SSE 正常收口，抓取发往上游的完整报文。
async fn mock_responses_upstream(
    n: usize,
) -> (std::net::SocketAddr, tokio::task::JoinHandle<Vec<Value>>) {
    let upstream = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = upstream.local_addr().unwrap();
    let handle = tokio::spawn(async move {
        // 终帧必须带可见正文条目：空 output 的 completed 正是空终帧护栏的
        // 触发形态（issue #83），本测试测的是稳定层可见性，不该顺手点燃护栏。
        let sse = concat!(
            "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\n\r\n",
            "event: response.created\n\ndata: {\"type\":\"response.created\",\"response\":{\"id\":\"r1\",\"status\":\"in_progress\"}}\n\n",
            "event: response.completed\n\ndata: {\"type\":\"response.completed\",\"response\":{\"id\":\"r1\",\"status\":\"completed\",\"output\":[{\"type\":\"message\",\"role\":\"assistant\",\"content\":[{\"type\":\"output_text\",\"text\":\"ok\"}]}]}}\n\n",
        );
        let mut captured = Vec::new();
        for _ in 0..n {
            let (mut stream, _) = upstream.accept().await.unwrap();
            let mut buffer = Vec::new();
            let mut chunk = [0u8; 4096];
            let (header_end, content_length) = loop {
                let read = stream.read(&mut chunk).await.unwrap();
                assert!(read > 0);
                buffer.extend_from_slice(&chunk[..read]);
                let Some(end) = buffer.windows(4).position(|w| w == b"\r\n\r\n") else {
                    continue;
                };
                let headers = String::from_utf8_lossy(&buffer[..end]).to_lowercase();
                let content_length = headers
                    .lines()
                    .find_map(|line| {
                        line.split_once(':')
                            .filter(|(name, _)| name.trim() == "content-length")
                            .and_then(|(_, value)| value.trim().parse::<usize>().ok())
                    })
                    .unwrap_or(0);
                break (end + 4, content_length);
            };
            while buffer.len() < header_end + content_length {
                let read = stream.read(&mut chunk).await.unwrap();
                assert!(read > 0);
                buffer.extend_from_slice(&chunk[..read]);
            }
            let body =
                String::from_utf8_lossy(&buffer[header_end..header_end + content_length]).to_string();
            captured.push(serde_json::from_str(&body).expect("发往上游的应是合法 JSON"));
            stream.write_all(sse.as_bytes()).await.unwrap();
        }
        captured
    });
    (addr, handle)
}

fn instruction_text() -> String {
    "You are performing a CONTEXT CHECKPOINT COMPACTION. Create a handoff summary for another LLM that will resume the task.\n\nInclude:\n- Current progress and key decisions made\n- Important context, constraints, or user preferences\n- What remains to be done (clear next steps)\n- Any critical data, examples, or references needed to continue\n\nBe concise, structured, and focused on helping the next LLM seamlessly continue the work.\n".to_string()
}

fn normal_request(key: &str, instructions: &str) -> Value {
    json!({
        "model": "m",
        "instructions": instructions,
        "prompt_cache_key": key,
        "stream": true,
        "tools": [{"type": "function", "function": {"name": "exec_command", "parameters": {}}}],
        "input": [
            {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "first question"}]},
            {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "answer"}]},
            {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "second question"}]},
        ],
    })
}

/// 自动压缩客户端形态：指令实例混在头部 system 项里，tools 被整块清空。
fn compaction_request(key: &str, instructions: &str) -> Value {
    let mut request = normal_request(key, instructions);
    request["tools"] = json!([]);
    request["input"] = json!([
        {"type": "message", "role": "system", "content": [{"type": "input_text", "text": format!("环境前言\n\n{}\n", instruction_text())}]},
        {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "first question"}]},
        {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "answer"}]},
    ]);
    request
}

async fn post_stream(port: u16, body: &Value) -> String {
    let payload = body.to_string();
    let mut stream = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    stream
        .write_all(
            format!(
                "POST /v1/responses HTTP/1.1\r\nhost: x\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                payload.len(),
                payload
            )
            .as_bytes(),
        )
        .await
        .unwrap();
    let mut proxied = String::new();
    stream.read_to_string(&mut proxied).await.unwrap();
    proxied
}

fn read_log_lines(path: &std::path::Path) -> Vec<Value> {
    let mut text = String::new();
    std::fs::File::open(path)
        .unwrap()
        .read_to_string(&mut text)
        .unwrap();
    text.lines().filter_map(|line| serde_json::from_str(line).ok()).collect()
}

fn event_names(records: &[Value]) -> Vec<String> {
    records
        .iter()
        .filter_map(|record| record["event"].as_str().map(str::to_string))
        .collect()
}

async fn spawn_proxy(upstream_addr: std::net::SocketAddr, log_path: &std::path::Path) -> (u16, ChildGuard) {
    let proxy_port = reserve_port();
    let config_path = log_path.with_extension("toml");
    std::fs::write(
        &config_path,
        format!(
            r#"
[server]
port = {proxy_port}

[upstream]
base_url = "http://{upstream_addr}/v1"
api_key = "sk-e2e-test"
protocol = "responses"

[stabilizer]
prompt_prefix = true

[log]
path = "{}"
"#,
            log_path.display()
        ),
    )
    .unwrap();
    let child = Command::new(env!("CARGO_BIN_EXE_codex-layer"))
        .env("CODEX_LAYER_CONFIG", &config_path)
        // 稳定层开启后 tools 回填缓存会落盘（#69 第 6 条）：必须指到临时目录，
        // 否则 cargo test 会往真实状态目录写测试键。与 [log] path 同一隔离思路。
        .env(
            "CODEX_LAYER_TOOLS_CACHE_PATH",
            log_path.with_extension("tools-cache.json"),
        )
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let guard = ChildGuard(child);
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match TcpStream::connect(("127.0.0.1", proxy_port)).await {
            Ok(_) => break,
            Err(_) if Instant::now() < deadline => {
                tokio::time::sleep(Duration::from_millis(100)).await
            }
            Err(error) => panic!("codex-layer 未在 5 秒内就绪: {error}"),
        }
    }
    (proxy_port, guard)
}

#[tokio::test]
async fn responses_dialect_compaction_aligns_and_probe_stays_live() {
    let (upstream_addr, capture) = mock_responses_upstream(4).await;
    let dir = tempfile::tempdir().unwrap();
    let log_path = dir.path().join("codex-layer.log");
    std::fs::write(&log_path, "").unwrap();
    let (proxy_port, _guard) = spawn_proxy(upstream_addr, &log_path).await;

    let key = format!("e2e-responses-{}", std::process::id());
    // 第一轮：普通轮，tools 非空，登记回填缓存并立探针基线。
    let first = post_stream(proxy_port, &normal_request(&key, "core instructions text")).await;
    assert!(first.contains("200 OK"), "第一轮应放行, got={first}");
    // 第二轮：压缩轮。指令从头部 system 项搬到末尾唯一 user 项，tools 回填逐字节一致。
    let second = post_stream(proxy_port, &compaction_request(&key, "core instructions text")).await;
    assert!(second.contains("200 OK"), "压缩轮应放行, got={second}");
    // 第三、四轮：探针活性对照——同键主/子形态乒乓不误报，靠的是真在跑的指纹表。
    let third = post_stream(
        proxy_port,
        &normal_request(&key, "sub-agent shaped instructions"),
    )
    .await;
    assert!(third.contains("200 OK"), "子形态应放行, got={third}");
    let fourth = post_stream(proxy_port, &normal_request(&key, "core instructions text")).await;
    assert!(fourth.contains("200 OK"), "回摆主形态应放行, got={fourth}");

    let captured = capture.await.unwrap();
    assert_eq!(captured.len(), 4);

    // 压缩轮：tools 回填 + 指令末尾唯一 + 头部 system 项不再残留指令。
    let compacted = &captured[1];
    assert_eq!(
        compacted["tools"], captured[0]["tools"],
        "压缩轮 tools 必须与主会话最近一轮逐字节一致"
    );
    let items = compacted["input"].as_array().unwrap();
    let tail = items.last().unwrap();
    assert_eq!(tail["role"].as_str(), Some("user"));
    let tail_text = tail["content"][0]["text"].as_str().unwrap();
    assert!(
        tail_text.starts_with("You are performing a CONTEXT CHECKPOINT COMPACTION"),
        "末尾必须是摘要指令, got={tail_text}"
    );
    let head_text = items[0]["content"][0]["text"].as_str().unwrap();
    assert_eq!(head_text.trim(), "环境前言", "头部 system 项的指令实例必须被搬走");

    // 诊断事件：对齐事件与探针事件都要有——事件名沿用既有, 零新增。
    let records = read_log_lines(&log_path);
    let events = event_names(&records);
    assert!(
        events
            .iter()
            .any(|name| name.ends_with("prefix_stabilizer_compaction_appended")),
        "压缩轮对齐必须留下事件, got={events:?}"
    );
    let baselines: Vec<_> = events
        .iter()
        .filter(|name| name.ends_with("prefix_stabilizer_prefix_baseline"))
        .collect();
    assert_eq!(baselines.len(), 1, "同键只应有一条基线, got={events:?}");
    let divergences: Vec<_> = events
        .iter()
        .filter(|name| name.ends_with("prefix_stabilizer_prefix_divergence"))
        .collect();
    // 恰两条, 且都诚实：压缩轮把指令从头部 system 项搬走, 头部本就合法变化,
    // 探针在对齐之后观察, 这条分叉如实反映压缩轮形态（与 Chat 方言同口径）;
    // 子 agent 形态首见再报一条。第四轮回摆主形态必须静默——总数恰好说明
    // 「指纹表真在跑且回摆不误报」, 多了就是探针失准。
    assert_eq!(
        divergences.len(),
        2,
        "压缩轮头部变化与子形态首见各一条, 回摆不得再报, got={events:?}"
    );
}
