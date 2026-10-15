//! tools 回填缓存跨重启存活（issue #69 第 6 条的端到端验收）：
//! 跑**两个真实进程**——
//! 1. 第一轮进程收普通轮（tools 非空），落盘后杀掉；
//! 2. 第二轮进程**直接收压缩轮**（tools:[] + 末尾摘要指令），断言：
//!    a) 发往上游的 tools 与第一轮逐字节一致（预热命中，不再是冷算）；
//!    b) 诊断 `compaction_appended` 的 `toolsBackfilled: true`；
//!    c) 诊断日志零泄漏：工具名一个字都不许出现在日志里；
//!    d) 落盘字段只报值（toolsFlushed / toolsWarmed），不含路径与内容。
//! 全程临时目录 + 临时端口 + 独立缓存路径，绝不触碰 12333 与真实状态目录。

use serde_json::{Value, json};
use std::io::Read;
use std::net::SocketAddr;
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
async fn mock_responses_upstream(n: usize) -> (SocketAddr, tokio::task::JoinHandle<Vec<Value>>) {
    let upstream = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = upstream.local_addr().unwrap();
    let handle = tokio::spawn(async move {
        let sse = concat!(
            "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\n\r\n",
            "event: response.created\n\ndata: {\"type\":\"response.created\",\"response\":{\"id\":\"r1\",\"status\":\"in_progress\"}}\n\n",
            "event: response.completed\n\ndata: {\"type\":\"response.completed\",\"response\":{\"id\":\"r1\",\"status\":\"completed\"}}\n\n",
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
            let body = String::from_utf8_lossy(&buffer[header_end..header_end + content_length])
                .to_string();
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

fn normal_request(key: &str) -> Value {
    json!({
        "model": "m",
        "instructions": "cross restart persistence instructions",
        "prompt_cache_key": key,
        "stream": true,
        "tools": [{"type": "function", "function": {"name": "persistence_probe_tool", "parameters": {}}}],
        "input": [
            {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "first question"}]},
            {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "answer"}]},
            {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "second question"}]},
        ],
    })
}

/// 自动压缩客户端形态：指令实例混在头部 system 项里，tools 被整块清空。
fn compaction_request(key: &str) -> Value {
    let mut request = normal_request(key);
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
    text.lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect()
}

/// 起一个独立进程实例：临时配置 / 临时日志各用一份，但**缓存路径两轮共用**
/// （跨重启存活的契约就落在这一个文件上）。`CODEX_LAYER_HOME` 与缓存路径都
/// 经环境变量指向临时目录——真实状态目录一个字节都不碰。
fn spawn_proxy(upstream_addr: SocketAddr, dir: &std::path::Path, label: &str) -> (u16, ChildGuard) {
    let proxy_port = reserve_port();
    let config_path = dir.join(format!("config-{label}.toml"));
    let log_path = dir.join(format!("log-{label}.jsonl"));
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
        .env("CODEX_LAYER_HOME", dir)
        .env("CODEX_LAYER_TOOLS_CACHE_PATH", dir.join("tools-cache.json"))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let guard = ChildGuard(child);
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match std::net::TcpStream::connect(("127.0.0.1", proxy_port)) {
            Ok(_) => break,
            Err(_) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(100)),
            Err(error) => panic!("codex-layer 未在 5 秒内就绪: {error}"),
        }
    }
    (proxy_port, guard)
}

#[tokio::test]
async fn tools_cache_survives_process_restart_and_backfills() {
    let (upstream_addr, capture) = mock_responses_upstream(2).await;
    let dir = tempfile::tempdir().unwrap();

    let key = format!("e2e-persist-{}", std::process::id());
    let expected_tools = normal_request(&key)["tools"].clone();

    // —— 第一轮进程：普通轮，tools 记录并落盘，然后杀进程 ——
    let (port_one, guard_one) = spawn_proxy(upstream_addr, dir.path(), "one");
    let first = post_stream(port_one, &normal_request(&key)).await;
    assert!(first.contains("200 OK"), "第一轮应放行, got={first}");
    drop(guard_one);
    // 等进程真正退出并释放端口资源（Drop 里 kill+wait，这里只等文件系统落定）。
    tokio::time::sleep(Duration::from_millis(200)).await;

    // 落盘文件必须存在且含工具 schema（内容本身不进诊断，见下方反判据）。
    let cache_path = dir.path().join("tools-cache.json");
    let written = std::fs::read_to_string(&cache_path)
        .expect("普通轮之后必须有一次落盘（新进程首次落盘不受节流窗口约束）");
    assert!(written.contains("persistence_probe_tool"));
    assert!(written.contains(r#""version":1"#));

    // —— 第二轮进程：直接收压缩轮，内存缓存本应是空的，全靠预热 ——
    let (port_two, _guard_two) = spawn_proxy(upstream_addr, dir.path(), "two");
    let second = post_stream(port_two, &compaction_request(&key)).await;
    assert!(second.contains("200 OK"), "压缩轮应放行, got={second}");

    let captured = capture.await.unwrap();
    assert_eq!(captured.len(), 2);
    assert_eq!(
        captured[1]["tools"], captured[0]["tools"],
        "重启后的压缩轮 tools 必须与第一轮逐字节一致（预热命中，杜绝整段冷算）"
    );
    assert_eq!(captured[1]["tools"], expected_tools);

    // 诊断口径：既有事件名 + 新字段，零新增事件名；工具内容零泄漏。
    let log_path = dir.path().join("log-two.jsonl");
    let records = read_log_lines(&log_path);
    let compaction_events: Vec<_> = records
        .iter()
        .filter(|record| {
            record["event"]
                .as_str()
                .is_some_and(|name| name.ends_with("prefix_stabilizer_compaction_appended"))
        })
        .collect();
    assert_eq!(compaction_events.len(), 1, "压缩轮应恰有一条对齐事件");
    // 记录形态是 DiagnosticRecord{timestamp_ms,pid,event,detail}，字段在 detail 下。
    let detail = &compaction_events[0]["detail"];
    assert_eq!(
        detail["toolsBackfilled"],
        json!(true),
        "重启后的压缩轮必须回填命中"
    );
    assert_eq!(
        detail["toolsWarmed"],
        json!(1),
        "预热命中条数必须可观测且恰为一条"
    );
    // 只报值：路径与内容都不得进诊断。
    let raw_log = std::fs::read_to_string(&log_path).unwrap();
    assert!(
        !raw_log.contains("persistence_probe_tool"),
        "工具 schema 一个字都不许出现在诊断日志里"
    );
    assert!(
        !raw_log.contains("tools-cache"),
        "落盘路径不得出现在诊断日志里"
    );
}
