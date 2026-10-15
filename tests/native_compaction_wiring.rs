//! 原生 Responses 方言下压缩轮接线的集成测试（issue #69 第 1–5 条）。
//!
//! 假上游按脚本逐条回包，断言的是**客户端看到什么**与**降级轮发往上游什么**：
//! 1) 上游给恰好一个原生 compaction 项 → 逐字节透传，不走本地摘要包装；
//! 2) 上游 4xx 显式拒绝 trigger → 剥离 trigger 降级本地摘要，客户端拿到可用响应；
//! 3) 4xx 但判假（无关错误）→ 错误原样透传，不降级（反判据）；
//! 4) 截断/零项的流 → 显式 response.failed，无静默半截流；
//! 5) 上游自报失败的流 → 不补重复帧；
//! 6) 非流式原生项 → 透传上游 JSON，不被本地摘要冒充。

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

const OPAQUE: &str = "gAAAAABnNATIVEopaquePayload+abc/def==";

fn compaction_item_event() -> String {
    format!(
        "event: response.output_item.done\ndata: {{\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{{\"id\":\"cmp_1\",\"type\":\"compaction\",\"encrypted_content\":\"{OPAQUE}\"}}}}\n\n"
    )
}

fn created_event() -> &'static str {
    "event: response.created\ndata: {\"type\":\"response.created\",\"response\":{\"id\":\"r1\",\"status\":\"in_progress\"}}\n\n"
}

fn completed_event() -> &'static str {
    "event: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"id\":\"r1\",\"status\":\"completed\"}}\n\n"
}

fn sse_response(body: &str) -> Vec<u8> {
    raw_response("200 OK", "text/event-stream", body)
}

/// v2 远程压缩请求（trigger 在 input 末尾），带非空 tools 以便回填路径可走通。
fn trigger_request(key: &str) -> Value {
    json!({
        "model": "m",
        "instructions": "core instructions",
        "prompt_cache_key": key,
        "stream": true,
        "tools": [{"type": "function", "name": "exec_command", "parameters": {}}],
        "input": [
            {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "first question"}]},
            {"type": "compaction_trigger"},
        ],
    })
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

/// 上游给恰好一个原生 compaction 项：逐字节透传，不得出现本地摘要命名空间。
#[tokio::test]
async fn native_compaction_item_passes_through_byte_for_byte() {
    let upstream_body = format!(
        "{}{}{}",
        created_event(),
        compaction_item_event(),
        completed_event()
    );
    let (port, _guard, _dir, log_path, _capture) =
        harness(vec![sse_response(&upstream_body)]).await;
    let key = format!("native-passthrough-{}", std::process::id());

    let seen = post(port, &trigger_request(&key)).await;
    assert!(seen.contains("200 OK"), "应放行, got={seen}");
    assert!(
        seen.contains(OPAQUE),
        "原生 opaque 状态必须原样到达客户端, got={seen}"
    );
    assert!(
        !seen.contains("codexlayer:summary"),
        "上游原生项不得被本地摘要冒充, got={seen}"
    );
    assert!(
        !seen.contains("response.failed"),
        "成功轮不得补失败帧, got={seen}"
    );

    let records: Vec<Value> = read_log_lines(&log_path);
    assert!(
        records.iter().any(|record| {
            record["event"] == json!("protocol_proxy.prefix_stabilizer_compaction_appended")
                && record["detail"]["nativePassedThrough"] == json!(true)
        }),
        "应记下原生透传事件, records={records:?}"
    );
}

/// 4xx 显式拒绝 trigger：剥离 trigger 后降级本地摘要，客户端拿到可用压缩轮。
#[tokio::test]
async fn rejected_trigger_degrades_to_local_summary_once() {
    let rejection = raw_response(
        "400 Bad Request",
        "application/json",
        r#"{"error":{"message":"Unsupported item type in input","type":"invalid_request_error","param":"input[1].compaction_trigger","code":null}}"#,
    );
    let summary_stream = concat!(
        "event: response.output_text.delta\n\ndata: {\"type\":\"response.output_text.delta\",\"delta\":\"handoff summary text\"}\n\n",
        "event: response.completed\n\ndata: {\"type\":\"response.completed\",\"response\":{\"id\":\"r2\",\"status\":\"completed\"}}\n\n",
    );
    let (port, _guard, _dir, _log, capture) =
        harness(vec![rejection, sse_response(summary_stream)]).await;
    let key = format!("degrade-ok-{}", std::process::id());

    let seen = post(port, &trigger_request(&key)).await;
    assert!(seen.contains("200 OK"), "降级轮应放行, got={seen}");
    assert!(
        seen.contains("codexlayer:summary"),
        "降级轮必须落到本地摘要命名空间, got={seen}"
    );
    assert!(
        !seen.contains("response.failed"),
        "降级成功不得报失败, got={seen}"
    );

    // 上游实际收到什么，才是「降级」的实锤：两次请求，trigger 已剥净、摘要指令在末尾。
    let sent = tokio::time::timeout(Duration::from_secs(5), capture)
        .await
        .expect("上游应在 5 秒内收完两次请求")
        .unwrap();
    assert_eq!(sent.len(), 2, "降级只允许重发一次, sent={sent:?}");
    assert!(
        sent[0]["input"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["type"] == json!("compaction_trigger")),
        "第一轮必须原样带 trigger 交给上游, sent[0]={}",
        sent[0]
    );
    let second = &sent[1];
    assert!(
        !second["input"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["type"] == json!("compaction_trigger")),
        "降级轮必须剥净 trigger, second={second}"
    );
    // 摘要阶段必须禁工具调用；tools 本身允许被稳定层回填成首轮那一份——
    // 回填的目的在于保住前缀（tools 整块变空会让历史折叠形态改变、前缀全弃），
    // 禁调用靠 tool_choice=none，不靠把 tools 抹掉。
    assert_eq!(
        second["tool_choice"],
        json!("none"),
        "摘要阶段必须禁工具, second={second}"
    );
    assert_eq!(
        second["tools"],
        serde_json::to_value(&sent[0]["tools"]).unwrap(),
        "降级轮的 tools 必须与普通轮逐字节一致（回填生效，前缀纯追加）, second={second}"
    );
    let last = second["input"].as_array().unwrap().last().unwrap();
    assert_eq!(last["role"], json!("user"));
    assert!(
        last["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("CONTEXT CHECKPOINT COMPACTION"),
        "摘要指令必须落在末尾 user 项, second={second}"
    );
}

/// 反判据：4xx 但错误与 trigger 无关（例如无关字段报错）→ 原样透传，不降级。
#[tokio::test]
async fn unrelated_4xx_passes_through_without_degrading() {
    let unrelated = raw_response(
        "400 Bad Request",
        "application/json",
        r#"{"error":{"message":"Invalid value for temperature: must be a number","type":"invalid_request_error","param":"temperature","code":null}}"#,
    );
    let (port, _guard, _dir, _log, _capture) = harness(vec![unrelated]).await;
    let key = format!("degrade-nope-{}", std::process::id());

    let seen = post(port, &trigger_request(&key)).await;
    assert!(seen.contains("400"), "无关 4xx 应原样透传, got={seen}");
    assert!(
        seen.contains("temperature"),
        "上游原话必须原样带回, got={seen}"
    );
    assert!(
        !seen.contains("codexlayer:summary"),
        "判假时不得降级, got={seen}"
    );
}

/// 零 compaction 项的流（上游只发了完成事件）：必须显式 response.failed 收口。
#[tokio::test]
async fn stream_without_compaction_item_gets_explicit_failure() {
    let upstream_body = format!("{}{}", created_event(), completed_event());
    let (port, _guard, _dir, _log, _capture) = harness(vec![sse_response(&upstream_body)]).await;
    let key = format!("zero-item-{}", std::process::id());

    let seen = post(port, &trigger_request(&key)).await;
    assert!(
        seen.contains("response.failed"),
        "零项必须显式失败收口, got={seen}"
    );
    assert!(
        seen.contains("compaction_missing_item"),
        "失败帧应点名原因类型, got={seen}"
    );
}

/// 上游自报失败的流：原样透传，不补重复帧。
#[tokio::test]
async fn upstream_self_reported_failure_gets_no_duplicate_frame() {
    let upstream_body = concat!(
        "event: response.created\n\ndata: {\"type\":\"response.created\",\"response\":{\"id\":\"r3\",\"status\":\"in_progress\"}}\n\n",
        "event: response.failed\n\ndata: {\"type\":\"response.failed\",\"response\":{\"id\":\"r3\",\"status\":\"failed\",\"error\":{\"message\":\"上游自己失败了\"}}}\n\n",
    );
    let (port, _guard, _dir, _log, _capture) = harness(vec![sse_response(upstream_body)]).await;
    let key = format!("self-failed-{}", std::process::id());

    let seen = post(port, &trigger_request(&key)).await;
    assert!(
        seen.contains("上游自己失败了"),
        "上游失败原文应透传, got={seen}"
    );
    // 数「帧」而不是数字样：上游那一帧的 event: 行与 data 里的 type 都含该字样。
    assert_eq!(
        seen.matches("event: response.failed").count(),
        1,
        "不得补重复失败帧, got={seen}"
    );
}

/// 非流式原生项：透传上游 JSON（Content-Type 仍是 application/json），不被本地摘要冒充。
#[tokio::test]
async fn non_stream_native_item_is_not_rewrapped_as_local_summary() {
    let body = json!({
        "id": "r4",
        "status": "completed",
        "output": [
            {"id": "msg_1", "type": "message", "content": [{"type": "output_text", "text": "顺带说的话"}]},
            {"id": "cmp_1", "type": "compaction", "encrypted_content": OPAQUE},
        ],
    })
    .to_string();
    let (port, _guard, _dir, _log, _capture) =
        harness(vec![raw_response("200 OK", "application/json", &body)]).await;
    let key = format!("nonstream-native-{}", std::process::id());

    let mut request = trigger_request(&key);
    request["stream"] = json!(false);
    let seen = post(port, &request).await;
    assert!(seen.contains("200 OK"), "应放行, got={seen}");
    assert!(
        seen.contains("application/json"),
        "非流式原生项应保持 application/json, got={seen}"
    );
    assert!(seen.contains(OPAQUE), "opaque 必须原样到达, got={seen}");
    assert!(
        !seen.contains("codexlayer:summary") && !seen.contains("response.failed"),
        "既不得冒充本地摘要，也不得报失败, got={seen}"
    );
}

/// 截断流：原生项已给出、但上游在完成事件前断流——必须补显式失败帧，
/// 同时已透传的 opaque 状态保持逐字节不变（守卫只补帧，不改写既有字节）。
#[tokio::test]
async fn truncated_compaction_stream_still_gets_failure_frame() {
    let upstream_body = format!("{}{}", created_event(), compaction_item_event());
    let (port, _guard, _dir, _log, _capture) = harness(vec![sse_response(&upstream_body)]).await;
    let key = format!("truncated-{}", std::process::id());

    let seen = post(port, &trigger_request(&key)).await;
    assert!(
        seen.contains(OPAQUE),
        "已给出的 opaque 应原样到达, got={seen}"
    );
    assert!(
        seen.contains("response.failed"),
        "截断流必须显式失败收口，不得静默半截流, got={seen}"
    );
    assert!(
        !seen.contains("response.completed"),
        "本层不得替上游伪造完成事件, got={seen}"
    );
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
