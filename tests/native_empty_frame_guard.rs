//! 原生 Responses 方言下空终帧护栏的集成测试（issue #83 的 P0 本体）。
//!
//! 本机 dogfood 全程是原生方言（MLX 直接回 Responses SSE），护栏必须住在这条
//! 透传支路上。断言的是**客户端看到什么**与**上游实际收到什么**：
//! 1) 首轮静默 completed（只有 reasoning）→ 同体重发一轮，客户端拿到单条
//!    created + 单个合并终帧（首轮条目垫底、本轮条目接位、id 回填首轮），
//!    全程无 failed、无重复身份事件；
//! 2) 连续静默打满额度 → 仍下发一个 completed 终帧（合并形态），绝不静默断流；
//!    第二次重发带推动文案，且推动项只出现在重发体里，不进客户端历史。

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

/// 把一次上游回包整成原始 HTTP 报文。
fn raw_response(status_line: &str, content_type: &str, body: &str) -> Vec<u8> {
    format!(
        "HTTP/1.1 {status_line}\r\ncontent-type: {content_type}\r\nconnection: close\r\ncontent-length: {}\r\n\r\n{body}",
        body.len()
    )
    .into_bytes()
}

fn sse_response(body: &str) -> Vec<u8> {
    raw_response("200 OK", "text/event-stream", body)
}

/// 按脚本回包的假上游：每次请求按顺序取一条响应原样写回，并抓取请求体。
async fn mock_upstream(
    responses: Vec<Vec<u8>>,
) -> (std::net::SocketAddr, tokio::task::JoinHandle<Vec<Value>>) {
    let upstream = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = upstream.local_addr().unwrap();
    let handle = tokio::spawn(async move {
        let mut captured = Vec::new();
        for response in responses {
            let (mut stream, _) = upstream.accept().await.unwrap();
            let mut buffer = Vec::new();
            let mut chunk = [0u8; 8192];
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
            stream.write_all(&response).await.unwrap();
        }
        captured
    });
    (addr, handle)
}

async fn spawn_proxy(
    upstream_addr: std::net::SocketAddr,
    log_path: &std::path::Path,
) -> (u16, ChildGuard) {
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
        .env("CODEX_LAYER_HOME", log_path.parent().unwrap())
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

type Harness = (
    u16,
    ChildGuard,
    tempfile::TempDir,
    std::path::PathBuf,
    tokio::task::JoinHandle<Vec<Value>>,
);

async fn harness(responses: Vec<Vec<u8>>) -> Harness {
    let (upstream_addr, capture) = mock_upstream(responses).await;
    let dir = tempfile::tempdir().unwrap();
    let log_path = dir.path().join("codex-layer.log");
    std::fs::write(&log_path, "").unwrap();
    let (port, guard) = spawn_proxy(upstream_addr, &log_path).await;
    (port, guard, dir, log_path, capture)
}

async fn post(port: u16, body: &Value) -> String {
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

fn normal_request(key: &str) -> Value {
    json!({
        "model": "m",
        "instructions": "core instructions",
        "prompt_cache_key": key,
        "stream": true,
        "tools": [{"type": "function", "name": "exec_command", "parameters": {}}],
        "input": [
            {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "继续盯住这个任务"}]},
        ],
    })
}

/// 静默轮：只有 reasoning 条目的 completed（本机 MLX 的病灶形态）。
fn silent_stream(id: &str, item_id: &str, reasoning_tokens: u64) -> Vec<u8> {
    sse_response(&format!(
        concat!(
            "event: response.created\ndata: {{\"type\":\"response.created\",\"response\":{{\"id\":\"{id}\",\"status\":\"in_progress\"}}}}\n\n",
            "event: response.output_item.done\ndata: {{\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{{\"id\":\"{item_id}\",\"type\":\"reasoning\"}}}}\n\n",
            "event: response.completed\ndata: {{\"type\":\"response.completed\",\"response\":{{\"id\":\"{id}\",\"status\":\"completed\",\"output\":[{{\"id\":\"{item_id}\",\"type\":\"reasoning\"}}],\"usage\":{{\"output_tokens_details\":{{\"reasoning_tokens\":{rt}}},\"total_tokens\":9000}}}}}}\n\n",
        ),
        id = id,
        item_id = item_id,
        rt = reasoning_tokens
    ))
}

/// 交付轮：带可见正文的 completed。
fn delivered_stream(id: &str, item_id: &str, text: &str) -> Vec<u8> {
    sse_response(&format!(
        concat!(
            "event: response.created\ndata: {{\"type\":\"response.created\",\"response\":{{\"id\":\"{id}\",\"status\":\"in_progress\"}}}}\n\n",
            "event: response.output_item.done\ndata: {{\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{{\"id\":\"{item_id}\",\"type\":\"message\",\"role\":\"assistant\",\"content\":[{{\"type\":\"output_text\",\"text\":\"{text}\"}}]}}}}\n\n",
            "event: response.completed\ndata: {{\"type\":\"response.completed\",\"response\":{{\"id\":\"{id}\",\"status\":\"completed\",\"output\":[{{\"id\":\"{item_id}\",\"type\":\"message\",\"role\":\"assistant\",\"content\":[{{\"type\":\"output_text\",\"text\":\"{text}\"}}]}}]}}}}\n\n",
        ),
        id = id,
        item_id = item_id,
        text = text
    ))
}

/// 数子串出现次数。
fn count_needle(haystack: &str, needle: &str) -> usize {
    haystack.matches(needle).count()
}

fn read_log_records(log_path: &std::path::Path) -> Vec<Value> {
    let mut raw = String::new();
    std::fs::File::open(log_path)
        .expect("诊断日志必须存在")
        .read_to_string(&mut raw)
        .unwrap();
    raw.lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .collect()
}

#[tokio::test]
async fn silent_first_round_retries_once_and_client_gets_merged_single_stream() {
    let (port, _guard, _dir, log_path, capture) = harness(vec![
        silent_stream("resp_native_1", "reason_1", 4200),
        delivered_stream("resp_native_2", "msg_2", "第二轮正文"),
    ])
    .await;
    let key = format!("native-guard-{}", std::process::id());

    let seen = post(port, &normal_request(&key)).await;
    assert!(seen.contains("200 OK"), "静默轮经重发后应放行, got={seen}");
    // 单条身份事件：第二轮的 created 必须被护栏吞掉。
    assert_eq!(
        count_needle(&seen, "event: response.created"),
        1,
        "客户端只能看到一条 created, got={seen}"
    );
    // 单个终帧，且必须是合并帧：首轮 reasoning 垫底、本轮正文接位、id 回填首轮。
    assert_eq!(
        count_needle(&seen, "event: response.completed"),
        1,
        "客户端只能看到一个 completed, got={seen}"
    );
    assert!(seen.contains("第二轮正文"), "本轮正文必须出门, got={seen}");
    assert!(
        seen.contains("reason_1"),
        "首轮条目必须留在终帧里, got={seen}"
    );
    assert!(
        seen.contains("resp_native_1"),
        "终帧 id 必须回填首轮身份, got={seen}"
    );
    assert!(
        !seen.contains("resp_native_2"),
        "第二轮的响应身份不许出门, got={seen}"
    );
    assert!(
        seen.contains("\"output_index\":1"),
        "本轮条目必须按位移接位, got={seen}"
    );
    assert!(
        !seen.contains("response.failed"),
        "重发成功不得报失败, got={seen}"
    );

    // 上游实收：两轮同体重采样（第一次重试赌温度，不动请求体）。
    let sent = tokio::time::timeout(Duration::from_secs(5), capture)
        .await
        .expect("上游应在 5 秒内收完两次请求")
        .unwrap();
    assert_eq!(sent.len(), 2, "静默一轮只允许重发一次, sent={sent:?}");
    assert_eq!(
        serde_json::to_string(&sent[0]).unwrap(),
        serde_json::to_string(&sent[1]).unwrap(),
        "第一次重试必须同体（前缀全命中），不发推动文案"
    );

    // 诊断账：retry 事件带被丢轮的 reasoning token，收口事件带真实上游轮次。
    let records = read_log_records(&log_path);
    let retry = records
        .iter()
        .find(|record| record["event"] == json!("codex_layer.empty_frame_retry"))
        .expect("必须记 empty_frame_retry");
    assert_eq!(retry["detail"]["attempt"], json!(1));
    assert_eq!(retry["detail"]["nudged"], json!(false));
    assert_eq!(
        retry["detail"]["droppedUsage"]["reasoning_tokens"],
        json!(4200),
        "白烧的 reasoning token 必须可见, detail={}",
        retry["detail"]
    );
    let completed_round = records
        .iter()
        .find(|record| {
            record["event"] == json!("codex_layer.stream_completed")
                && record["detail"]["attempts"].is_number()
        })
        .expect("原生支路的 stream_completed 必须带 attempts");
    assert_eq!(completed_round["detail"]["attempts"], json!(2));
}

#[tokio::test]
async fn exhausted_silence_still_delivers_merged_completed_and_nudges_second_retry() {
    let (port, _guard, _dir, log_path, capture) = harness(vec![
        silent_stream("resp_native_1", "reason_1", 100),
        silent_stream("resp_native_2", "reason_2", 200),
        silent_stream("resp_native_3", "reason_3", 300),
    ])
    .await;
    let key = format!("native-exhausted-{}", std::process::id());

    let seen = post(port, &normal_request(&key)).await;
    assert!(seen.contains("200 OK"), "额度用尽也应放行, got={seen}");
    assert_eq!(count_needle(&seen, "event: response.created"), 1);
    assert_eq!(
        count_needle(&seen, "event: response.completed"),
        1,
        "红线：绝不静默断流，最后一帧必须是合并 completed, got={seen}"
    );
    assert!(
        !seen.contains("response.failed"),
        "静默不是失败，不得补失败帧, got={seen}"
    );
    // 合并帧把三轮 reasoning 条目全部垫齐，与已流式下发的条目对得上账。
    for item_id in ["reason_1", "reason_2", "reason_3"] {
        assert!(seen.contains(item_id), "终帧必须包含 {item_id}, got={seen}");
    }
    assert!(
        seen.contains("resp_native_1"),
        "id 必须回填首轮, got={seen}"
    );

    let sent = tokio::time::timeout(Duration::from_secs(5), capture)
        .await
        .expect("上游应在 5 秒内收完三次请求")
        .unwrap();
    assert_eq!(
        sent.len(),
        3,
        "额度 2 次重发 + 首轮 = 三次上游, sent={sent:?}"
    );
    // 第一次同体，第二次带推动文案；推动项只进重发体。
    let body_of = |req: &Value| req["input"].as_array().unwrap().clone();
    assert_eq!(body_of(&sent[0]), body_of(&sent[1]), "第一次重试必须同体");
    let third_items = body_of(&sent[2]);
    assert_eq!(
        third_items.len(),
        2,
        "第二次重试的 input 必须多一条推动项, items={third_items:?}"
    );
    let nudge_text = third_items[1]["content"][0]["text"].as_str().unwrap();
    assert!(
        nudge_text.contains("[relay-empty-frame-nudge]"),
        "推动项必须带识别标记, text={nudge_text}"
    );
    assert!(
        !nudge_text.contains("<current_user_turn>"),
        "推动项不得携带轮次标记, text={nudge_text}"
    );

    let records = read_log_records(&log_path);
    let exhausted = records
        .iter()
        .find(|record| record["event"] == json!("codex_layer.empty_frame_exhausted"))
        .expect("额度用尽必须记 empty_frame_exhausted");
    assert_eq!(exhausted["detail"]["attempts"], json!(2));
    assert_eq!(
        exhausted["detail"]["droppedUsage"]["reasoning_tokens"],
        json!(600),
        "两轮被丢的 reasoning 账要累加, detail={}",
        exhausted["detail"]
    );
}
