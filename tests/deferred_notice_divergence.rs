//! 延迟工具命名空间通告块的监看口径集成测试（issue #36）：起真实二进制走
//! 完整代理链路，三轮请求同 prompt_cache_key，前两轮在 Added 与 Removed 之间
//! 翻转，断言
//! 1) 发往上游的头部 system 合并文本前两轮逐字节相等，通告块落在瞬态区；
//! 2) 探针同键上翻转不新增形态：baseline 恰一条，翻转轮之间零 divergence；
//! 3) 第三轮真语义变化必须被判分叉——探针活性对照，排除「零分叉是因为
//!    探针压根没跑」的假阳性（修复前第二轮必报 divergence，firstDiff 在
//!    50KB 附近）。

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

/// 假上游：连续服务 n 次 Chat SSE 正常收口，抓取发往上游的完整报文。
async fn mock_upstream_times(
    n: usize,
) -> (std::net::SocketAddr, tokio::task::JoinHandle<Vec<Value>>) {
    let upstream = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = upstream.local_addr().unwrap();
    let handle = tokio::spawn(async move {
        let sse = concat!(
            "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\n\r\n",
            "data: {\"id\":\"c1\",\"object\":\"chat.completion.chunk\",\"model\":\"m\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"hi\"},\"finish_reason\":null}]}\n\n",
            "data: {\"id\":\"c1\",\"object\":\"chat.completion.chunk\",\"model\":\"m\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
            "data: [DONE]\n\n",
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

/// 组装一轮 responses 请求：通告块以独立 system 消息注入 system 区（真实
/// 客户端形态，文本自带前导换行）；`notice` 为 None 时不注入。
fn responses_request(key: &str, instructions: &str, notice: Option<&str>) -> Value {
    let mut input = vec![json!({"role": "developer", "content": "tools doc"})];
    if let Some(notice) = notice {
        input.push(json!({"role": "system", "content": format!("\n<tools>\n{notice}\n- mcp__ddg_search\n</tools>")}));
    }
    input.push(json!({"role": "user", "content": "first question"}));
    input.push(json!({"role": "assistant", "content": "answer"}));
    input.push(json!({"role": "user", "content": "second question"}));
    json!({
        "model": "m",
        "instructions": instructions,
        "prompt_cache_key": key,
        "stream": true,
        "tools": [{"type": "function", "function": {"name": "exec_command", "parameters": {}}}],
        "input": input,
    })
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

fn event_records<'a>(records: &'a [Value], event: &str) -> Vec<&'a Value> {
    records
        .iter()
        .filter(|record| record["event"].as_str() == Some(event))
        .collect()
}

#[tokio::test]
async fn notice_flip_adds_no_new_prefix_shape() {
    let (upstream_addr, capture) = mock_upstream_times(3).await;
    let dir = tempfile::tempdir().unwrap();
    let log_path = dir.path().join("codex-layer.log");
    std::fs::write(&log_path, "").unwrap();
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
            dir.path().join("tools-cache.json"),
        )
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let _guard = ChildGuard(child);
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

    // 同一 prompt_cache_key：前两轮通告块 Added / Removed 翻转。
    let key = format!("e2e-nsblock-{}", std::process::id());
    let first = post_stream(
        proxy_port,
        &responses_request(&key, "core instructions text", Some("Added deferred tool namespaces:")),
    )
    .await;
    assert!(first.contains("200 OK"), "第一轮应放行，got={first}");
    let second = post_stream(
        proxy_port,
        &responses_request(
            &key,
            "core instructions text",
            Some("Removed deferred tool namespaces:"),
        ),
    )
    .await;
    assert!(second.contains("200 OK"), "第二轮应放行，got={second}");
    // 第三轮：真语义变化（instructions 改写）——探针活性对照，它必须被判分叉；
    // 没有这条对照，上面的「翻转不判叉」可能只是探针压根没跑的假阳性。
    let third = post_stream(
        proxy_port,
        &responses_request(
            &key,
            "rewritten instructions with clearly different text",
            Some("Removed deferred tool namespaces:"),
        ),
    )
    .await;
    assert!(third.contains("200 OK"), "第三轮应放行，got={third}");

    let captured = capture.await.unwrap();
    let head_first = serde_json::to_string(&captured[0]["messages"][0]).unwrap();
    let head_second = serde_json::to_string(&captured[1]["messages"][0]).unwrap();
    let head_third = serde_json::to_string(&captured[2]["messages"][0]).unwrap();
    assert_eq!(
        head_first, head_second,
        "通告块翻转不得扰动发往上游的头部 system 字节"
    );
    assert_ne!(
        head_first, head_third,
        "instructions 改写必须真实反映到头部（对照轮的前缀确实变了）"
    );
    for (index, verb) in [(0usize, "Added"), (1usize, "Removed")] {
        let messages = captured[index]["messages"].as_array().unwrap();
        let last_user = messages
            .iter()
            .rposition(|m| m["role"].as_str() == Some("user"))
            .unwrap();
        assert_eq!(
            messages[last_user - 1]["content"].as_str().unwrap(),
            format!("<tools>\n{verb} deferred tool namespaces:\n- mcp__ddg_search\n</tools>"),
            "通告块必须原样落在瞬态区（末条 user 之前），第 {index} 轮"
        );
        assert!(
            !messages[0]["content"].as_str().unwrap().contains("deferred tool namespaces"),
            "头部 system 不得残留通告块"
        );
    }

    // 监看口径：翻转不新增形态；全进程只有一条 divergence，且来自对照轮。
    let records = read_log_lines(&log_path);
    let baselines = event_records(&records, "protocol_proxy.prefix_stabilizer_prefix_baseline");
    let divergences = event_records(&records, "protocol_proxy.prefix_stabilizer_prefix_divergence");
    assert_eq!(
        baselines.len(),
        1,
        "同键应只登记一个翻转形态（首轮 baseline），records={records:?}"
    );
    assert_eq!(
        divergences.len(),
        1,
        "Added 与 Removed 之间不得判分叉，唯一的 divergence 必须来自第三轮真语义变化，records={records:?}"
    );
    // 对照轮的分叉详情齐备：偏移、两侧长度与分叉点切片都在
    // （instructions 在头部最前端，分叉偏移本就是 0，只验字段与长度变化）。
    let detail = &divergences[0]["detail"];
    let first_diff = detail["firstDiff"].as_u64();
    let old_len = detail["oldLen"].as_u64();
    let new_len = detail["newLen"].as_u64();
    assert!(
        first_diff.is_some() && old_len.is_some() && new_len.is_some(),
        "对照轮分叉详情必须齐备（偏移与新旧长度），detail={detail}"
    );
    assert_ne!(old_len, new_len, "对照轮新旧形态长度必须不同，detail={detail}");
}
