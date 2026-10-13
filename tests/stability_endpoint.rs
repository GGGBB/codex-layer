//! `/stability` 数据面端到端测试（issue #19）：起真实二进制，覆盖
//! 开关两态、快照结构、snip 默认不外传，以及最重要的红线——
//! **requestId 只进诊断日志，绝不进发往上游的请求体或请求头**。

use serde_json::{Value, json};
use std::io::Read;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

/// 测试守卫：Drop 时杀掉子进程，断言失败也不留常驻二进制。
struct ChildGuard(Child);

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn reserve_port() -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.local_addr().unwrap().port()
}

struct Harness {
    dir: tempfile::TempDir,
    proxy_port: u16,
    log_path: std::path::PathBuf,
    _child: ChildGuard,
}

/// 写配置并拉起二进制；`seed_log` 里的 JSONL 行预填进诊断日志，
/// 让快照断言不依赖竞态时序。
async fn spawn_proxy(seed_log: &[Value], stability_config: &str) -> Harness {
    let dir = tempfile::tempdir().unwrap();
    let log_path = dir.path().join("codex-layer.log");
    let mut seed = String::new();
    for record in seed_log {
        seed.push_str(&record.to_string());
        seed.push('\n');
    }
    std::fs::write(&log_path, seed).unwrap();

    let proxy_port = reserve_port();
    let config_path = dir.path().join("codex-layer.toml");
    std::fs::write(
        &config_path,
        format!(
            r#"
[server]
host = "127.0.0.1"
port = {proxy_port}

[upstream]
base_url = "http://127.0.0.1:9/v1"
api_key = "sk-e2e-test"
protocol = "chat_completions"

[log]
path = "{}"
{stability_config}"#,
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

    // 等 /healthz 就绪（最多 5 秒）。
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match http_get(proxy_port, "/healthz").await {
            Ok((ref ready_status, _)) if ready_status == "200" => break,
            _ if Instant::now() < deadline => tokio::time::sleep(Duration::from_millis(100)).await,
            _ => panic!("codex-layer 未在 5 秒内就绪"),
        }
    }

    Harness {
        dir,
        proxy_port,
        log_path,
        _child: ChildGuard(child),
    }
}

/// 裸 HTTP GET；服务未就绪时返回 Err 供轮询重试。
async fn http_get(port: u16, path: &str) -> std::io::Result<(String, String)> {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).await?;
    stream
        .write_all(format!("GET {path} HTTP/1.1\r\nhost: x\r\nconnection: close\r\n\r\n").as_bytes())
        .await?;
    let mut raw = String::new();
    stream.read_to_string(&mut raw).await?;
    let (head, body) = raw.split_once("\r\n\r\n").unwrap_or((raw.as_str(), ""));
    let status = head.split_whitespace().nth(1).unwrap_or("?").to_string();
    Ok((status, body.to_string()))
}

/// 已就绪前提下的 GET：直接 unwrap。
async fn http_get_ok(port: u16, path: &str) -> (String, String) {
    http_get(port, path).await.unwrap()
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

fn event_records<'a>(records: &'a [Value], event: &str) -> Vec<&'a Value> {
    records
        .iter()
        .filter(|record| record["event"] == json!(event))
        .collect()
}

/// 默认（无 `[stability]` 段）：端点显式 404「未启用」，响应里没有任何快照数据。
#[tokio::test]
async fn stability_endpoint_is_disabled_by_default() {
    let harness = spawn_proxy(&[], "").await;
    let (status, body) = http_get_ok(harness.proxy_port, "/stability").await;
    assert_eq!(status, "404", "默认关必须 404，body={body}");
    assert!(body.contains("未启用"), "要给出可行动提示，body={body}");
}

/// 开启后：结构完整的 JSON 快照；钉定器与配对计数正确；
/// `oldSnip`/`newSnip` 默认不外传；响应里不得出现 API key。
#[tokio::test]
async fn enabled_endpoint_returns_structured_snapshot_without_leaking_snips_or_keys() {
    let secret_snip = "用户的私密提示词内容-绝不该出现在响应里";
    let seed = vec![
        json!({"timestamp_ms": 1000, "pid": 4242, "event": "codex_layer.started",
               "detail": {"listen": "127.0.0.1:1", "upstream": "http://x/v1"}}),
        json!({"timestamp_ms": 2000, "pid": 4242, "event": "codex_layer.stream_open",
               "detail": {"path": "/v1/responses", "requestId": "r1"}}),
        json!({"timestamp_ms": 2500, "pid": 4242, "event": "codex_layer.stream_completed",
               "detail": {"model": "m", "requestId": "r1"}}),
        json!({"timestamp_ms": 2600, "pid": 4242, "event": "codex_layer.stream_open",
               "detail": {"path": "/v1/responses", "requestId": "r2"}}),
        json!({"timestamp_ms": 2700, "pid": 4242,
               "event": "protocol_proxy.prefix_stabilizer_prefix_divergence",
               "detail": {"oldLen": 10, "newLen": 20, "firstDiff": 3,
                          "oldSnip": secret_snip, "newSnip": "b"}}),
    ];
    let harness = spawn_proxy(&seed, "\n[stability]\nenabled = true\n").await;

    let (status, body) = http_get_ok(harness.proxy_port, "/stability").await;
    assert_eq!(status, "200", "开启后应返回快照，body={body}");
    let snapshot: Value = serde_json::from_str(&body).unwrap();

    // 预填日志的聚合各归其账（本进程自己的 started 只会影响 session.started
    // 的取值，不影响下面这些计数——seed 与自身事件不共用 requestId）。
    assert_eq!(snapshot["turns"]["total"], json!(2));
    assert_eq!(snapshot["turns"]["completed"], json!(1));
    assert_eq!(snapshot["durationsMs"]["samples"], json!(1));
    assert_eq!(snapshot["durationsMs"]["p50"], json!(500));
    assert_eq!(snapshot["stability"]["divergence"], json!(1));
    assert_eq!(snapshot["stability"]["recentDivergences"][0]["oldLen"], json!(10));
    // 默认：snip 只字不出。
    assert!(snapshot["stability"]["recentDivergences"][0].get("oldSnip").is_none());
    assert!(!body.contains(secret_snip), "snip 默认绝不外传");
    assert!(!body.contains("sk-e2e-test"), "端点响应绝不带出 API key");
    assert!(!body.contains("api_key"), "端点响应绝不带出配置密钥字段");
    // 装配层字段齐全。
    assert!(snapshot["generatedAtMs"].is_number());
    assert!(snapshot["window"]["fileBytes"].is_number());
    let _ = &harness.dir;
}

/// 红线（#19 最重要一条）：走完整代理链路后，**发往上游的请求体与头都
/// 不含 requestId**，而诊断日志的 stream_open/stream_completed 带着同一个
/// requestId。前者钉「深缓存不被毁」，后者钉「配对地基可用」。
#[tokio::test]
async fn request_id_never_leaves_through_upstream_request() {
    // 假上游：Chat SSE 正常收口，同时抓取发往上游的完整报文。
    let upstream = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream_addr = upstream.local_addr().unwrap();
    let capture = tokio::spawn(async move {
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
        let sse = concat!(
            "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\n\r\n",
            "data: {\"id\":\"c1\",\"object\":\"chat.completion.chunk\",\"model\":\"m\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"hi\"},\"finish_reason\":null}]}\n\n",
            "data: {\"id\":\"c1\",\"object\":\"chat.completion.chunk\",\"model\":\"m\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
            "data: [DONE]\n\n",
        );
        stream.write_all(sse.as_bytes()).await.unwrap();
        String::from_utf8_lossy(&buffer).to_string()
    });


    let dir = tempfile::tempdir().unwrap();
    let log_path = dir.path().join("codex-layer.log");
    let proxy_port = reserve_port();
    let config_path = dir.path().join("codex-layer.toml");
    std::fs::write(
        &config_path,
        format!(
            r#"
[server]
port = {proxy_port}

[upstream]
base_url = "http://{upstream_addr}/v1"
api_key = "sk-e2e-test"
protocol = "chat_completions"

[log]
path = "{}"

[stability]
enabled = true
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
    let _guard = ChildGuard(child);
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match http_get(proxy_port, "/healthz").await {
            Ok((ref ready_status, _)) if ready_status == "200" => break,
            _ if Instant::now() < deadline => tokio::time::sleep(Duration::from_millis(100)).await,
            _ => panic!("codex-layer 未在 5 秒内就绪"),
        }
    }

    // 通过完整代理链路发一轮流式请求。
    let mut stream = TcpStream::connect(("127.0.0.1", proxy_port)).await.unwrap();
    let body = json!({"model": "m", "input": "红线测试", "stream": true}).to_string();
    stream
        .write_all(
            format!(
                "POST /v1/responses HTTP/1.1\r\nhost: x\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                body.len(),
                body
            )
            .as_bytes(),
        )
        .await
        .unwrap();
    let mut proxied = String::new();
    stream.read_to_string(&mut proxied).await.unwrap();
    assert!(proxied.contains("200 OK"), "代理链路应放行，got={proxied}");

    let captured = capture.await.unwrap();
    let captured_head = captured.to_lowercase();
    assert!(
        !captured_head.contains("requestid"),
        "requestId 泄漏进出站报文（头或体）！出站报文={captured}"
    );
    // 钉住转换确实发生了（出站体是本层重建的 Chat 请求，而不是原样透传）。
    assert!(captured.contains("\"messages\""), "出站应是对 Chat 方言的重建体");

    // 诊断侧：开流与收口共享同一个 requestId，配对地基可用。
    let records = read_log_lines(&log_path);
    let opens = event_records(&records, "codex_layer.stream_open");
    let completed = event_records(&records, "codex_layer.stream_completed");
    assert_eq!(opens.len(), 1, "应恰好一条 stream_open，records={records:?}");
    assert_eq!(completed.len(), 1, "应恰好一条 stream_completed");
    let open_id = opens[0]["detail"]["requestId"].as_str().unwrap().to_string();
    let done_id = completed[0]["detail"]["requestId"].as_str().unwrap().to_string();
    assert!(!open_id.is_empty());
    assert_eq!(open_id, done_id, "开流与收口必须共享 requestId");
    // 出站报文里也不能出现日志里那个具体的 id 值。
    assert!(!captured.contains(&open_id), "出站报文不得携带日志侧的 requestId 值");
}
