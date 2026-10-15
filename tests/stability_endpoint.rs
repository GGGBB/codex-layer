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

/// 带响应头的 GET：返回 (状态, 小写头块, body)，供 Content-Type 等断言。
async fn http_get_parts(port: u16, path: &str) -> (String, String, String) {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    stream
        .write_all(format!("GET {path} HTTP/1.1\r\nhost: x\r\nconnection: close\r\n\r\n").as_bytes())
        .await
        .unwrap();
    let mut raw = String::new();
    stream.read_to_string(&mut raw).await.unwrap();
    let (head, body) = raw.split_once("\r\n\r\n").unwrap_or((raw.as_str(), ""));
    let status = head.split_whitespace().nth(1).unwrap_or("?").to_string();
    (status, head.to_lowercase(), body.to_string())
}

/// `/stability/ui`（开启态）：200 + text/html + 关键挂载点齐备，
/// 数据面标记为启用（false），且不得残留占位符。
#[tokio::test]
async fn stability_ui_enabled_returns_self_contained_page() {
    let harness = spawn_proxy(&[], "\n[stability]\nenabled = true\n").await;
    let (status, head, body) = http_get_parts(harness.proxy_port, "/stability/ui").await;
    assert_eq!(status, "200", "/stability/ui 必须 200，body={body}");
    assert!(
        head.contains("content-type: text/html"),
        "Content-Type 必须是 text/html，头块={head}"
    );
    assert!(body.contains(r#"data-plane-disabled="false""#), "开启态标记必须是 false");
    assert!(!body.contains("__DATA_PLANE_DISABLED__"), "占位符不得残留");
    for marker in [
        r#"id="dashboard""#,
        r#"id="timeline""#,
        r#"id="divergence-table""#,
        r#"id="turns-table""#,
    ] {
        assert!(body.contains(marker), "缺少挂载点 {marker}");
    }
    // 零外部依赖：页面不得引用任何网络资源。
    assert!(!body.contains(r#"src="http"#), "页面不得引用外部脚本");
    assert!(!body.contains(r#"href="http"#), "页面不得引用外部样式/资源");
}

/// 默认（数据面未启用）：`/stability/ui` 仍是 200 的可读禁用页
/// （`data-plane-disabled="true"`），而不是报错；同进程 `/stability` 保持 404。
#[tokio::test]
async fn stability_ui_shows_readable_disabled_page_by_default() {
    let harness = spawn_proxy(&[], "").await;
    let (status, _, body) = http_get_parts(harness.proxy_port, "/stability/ui").await;
    assert_eq!(status, "200", "禁用页也必须 200，body={body}");
    assert!(body.contains(r#"data-plane-disabled="true""#), "默认必须是禁用标记");
    assert!(body.contains(r#"id="disabled-page""#), "禁用页挂载点必须存在");
    let (api_status, _) = http_get_ok(harness.proxy_port, "/stability").await;
    assert_eq!(api_status, "404", "禁用时数据面仍是显式 404");
}

/// 合成日志四态 + 界面红线：完成/失败/未收口/终态-only 与分叉各归其账；
/// recentTurns 明细（含压缩轮、outcome=null 的未收口行）与 unpaired 计数
/// 正确；提示词内容（snip）默认既不进 `/stability` JSON，也不进页面 HTML。
#[tokio::test]
async fn synthesized_log_covers_all_turn_states_and_page_never_carries_snips() {
    let secret = "合成日志里的提示词内容-绝不进响应";
    let seed = vec![
        // 完整压缩轮：开流 r1（compaction）+ 收口 r1。
        json!({"timestamp_ms": 1000, "pid": 4242, "event": "codex_layer.stream_open",
               "detail": {"path": "/v1/responses", "requestId": "r1", "compaction": true}}),
        json!({"timestamp_ms": 1200, "pid": 4242, "event": "codex_layer.stream_completed",
               "detail": {"model": "m", "requestId": "r1"}}),
        // 失败轮：开流 r2 + stream_failed r2。
        json!({"timestamp_ms": 2000, "pid": 4242, "event": "codex_layer.stream_open",
               "detail": {"path": "/v1/responses", "requestId": "r2"}}),
        json!({"timestamp_ms": 2300, "pid": 4242, "event": "codex_layer.stream_failed",
               "detail": {"requestId": "r2"}}),
        // 未收口轮：开流 r3，窗口内没有收口。
        json!({"timestamp_ms": 3000, "pid": 4242, "event": "codex_layer.stream_open",
               "detail": {"path": "/v1/responses", "requestId": "r3"}}),
        // 终态-only：收口 r4 没有对应开流（日志裁剪/窗口截断形态）。
        json!({"timestamp_ms": 4000, "pid": 4242, "event": "codex_layer.stream_completed",
               "detail": {"model": "m", "requestId": "r4"}}),
        // 一次前缀分叉（snip 携带敏感内容，默认必须只报长度与位置）。
        json!({"timestamp_ms": 4500, "pid": 4242,
               "event": "protocol_proxy.prefix_stabilizer_prefix_divergence",
               "detail": {"oldLen": 12, "newLen": 24, "firstDiff": 5,
                          "oldSnip": secret, "newSnip": "b"}}),
    ];
    let harness = spawn_proxy(&seed, "\n[stability]\nenabled = true\n").await;

    let (status, body) = http_get_ok(harness.proxy_port, "/stability").await;
    assert_eq!(status, "200", "body={body}");
    let snapshot: Value = serde_json::from_str(&body).unwrap();

    assert_eq!(snapshot["turns"]["total"], json!(3), "开流 3 条 = 3 轮");
    assert_eq!(snapshot["turns"]["compaction"], json!(1), "压缩轮 1 条");
    assert_eq!(snapshot["turns"]["completed"], json!(2), "completed 事件含终态-only 的 r4");
    assert_eq!(snapshot["turns"]["failed"], json!(1));
    assert_eq!(snapshot["durationsMs"]["unpaired"]["openWithoutTerminal"], json!(1), "r3 未收口");
    assert_eq!(snapshot["durationsMs"]["unpaired"]["terminalWithoutOpen"], json!(1), "r4 无开流");
    assert_eq!(snapshot["stability"]["divergence"], json!(1));

    // recentTurns：新→旧；分叉事件不占轮次行（轮次行只来自开流/收口/拒绝/上游错误）。
    let turns = snapshot["recentTurns"].as_array().unwrap();
    assert_eq!(turns.len(), 4, "轮次行 = r1/r2/r3 开流行 + r4 终态-only 行");
    assert_eq!(turns[0]["requestId"], json!("r4"));
    assert_eq!(turns[0]["outcome"], json!("completed"), "终态-only 行照实入表");
    let r3_row = turns.iter().find(|row| row["requestId"] == json!("r3")).unwrap();
    assert!(r3_row["outcome"].is_null(), "未收口行 outcome=null，由界面渲染成「开着未收口」");
    let r1_row = turns.iter().find(|row| row["requestId"] == json!("r1")).unwrap();
    assert_eq!(r1_row["compaction"], json!(true), "压缩轮标记要能在明细里看出来");
    assert_eq!(r1_row["outcome"], json!("completed"));
    assert_eq!(r1_row["durationMs"], json!(200));

    // 红线：snip 内容绝不进 JSON，也绝不进页面 HTML。
    assert!(!body.contains(secret), "snip 默认绝不进 /stability 响应");
    let (ui_status, _, ui_body) = http_get_parts(harness.proxy_port, "/stability/ui").await;
    assert_eq!(ui_status, "200");
    assert!(!ui_body.contains(secret), "snip 绝不进监看页 HTML");

    // 空窗口形态：没有分叉时 divergence=0、比率未定义而不是 0（不给假安全读数）。
    let empty_harness = spawn_proxy(&[], "\n[stability]\nenabled = true\n").await;
    let (_, empty_body) = http_get_ok(empty_harness.proxy_port, "/stability").await;
    let empty: Value = serde_json::from_str(&empty_body).unwrap();
    assert_eq!(empty["stability"]["divergence"], json!(0));
    assert!(empty["stability"]["divergencePerTurn"].is_null(), "0 轮时比率是未定义而非 0");
    let (_, _, empty_ui) = http_get_parts(empty_harness.proxy_port, "/stability/ui").await;
    assert!(empty_ui.contains(r#"id="timeline-empty""#), "空窗口要显式呈现「窗口内无数据」挂载点");
}

/// `?window_bytes` 覆盖：合法值生效（放大到全量解析、缩小被钳到下限），
/// 非法值（非数字、0）结构化 400——fail-loud，不静默。
#[tokio::test]
async fn window_bytes_query_override_takes_effect_and_rejects_garbage() {
    // 60 行填充事件 ≈ 8 KB：小窗口截断只读尾部，大窗口全量解析。
    let filler = |index: usize| {
        json!({"timestamp_ms": 10_000 + index as u64, "pid": 4242,
               "event": "noise.filler_event", "detail": {"pad": "x", "index": index}})
    };
    let mut seed: Vec<Value> = (0..60).map(filler).collect();
    // 真实轮次放在尾部：小窗口也必须能看见它们。
    seed.push(json!({"timestamp_ms": 90_000, "pid": 4242, "event": "codex_layer.stream_open",
                     "detail": {"path": "/v1/responses", "requestId": "w1"}}));
    seed.push(json!({"timestamp_ms": 91_000, "pid": 4242, "event": "codex_layer.stream_completed",
                     "detail": {"model": "m", "requestId": "w1"}}));
    let harness = spawn_proxy(&seed, "\n[stability]\nenabled = true\nwindow_bytes = 1024\n").await;

    // 配置小窗口：窗口口径如实回报，且必然截断。
    let (status, body) = http_get_ok(harness.proxy_port, "/stability").await;
    assert_eq!(status, "200", "body={body}");
    let snapshot: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(snapshot["window"]["windowBytes"], json!(1024));
    assert_eq!(snapshot["window"]["truncated"], json!(true));

    // 查询覆盖放大到 64 MB：全量解析，seed 的轮次必须进快照。
    let (status, body) =
        http_get_ok(harness.proxy_port, "/stability?window_bytes=67108864").await;
    assert_eq!(status, "200", "body={body}");
    let snapshot: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(snapshot["window"]["windowBytes"], json!(67108864), "覆盖值必须回显");
    assert_eq!(snapshot["window"]["truncated"], json!(false));
    assert_eq!(snapshot["turns"]["total"], json!(1), "seed 的 w1 轮必须被全量窗口看见");

    // 低于下限的合法数字：钳到 1024 而不是报错。
    let (status, body) = http_get_ok(harness.proxy_port, "/stability?window_bytes=512").await;
    assert_eq!(status, "200", "body={body}");
    let snapshot: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(snapshot["window"]["windowBytes"], json!(1024), "512 应被钳到下限 1024");

    // 非法值：结构化 400。
    for bad in ["abc", "0"] {
        let (status, body) =
            http_get_ok(harness.proxy_port, &format!("/stability?window_bytes={bad}")).await;
        assert_eq!(status, "400", "非法值 {bad:?} 必须 400，body={body}");
        let parsed: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(parsed["status"], json!("failed"));
        assert!(
            parsed["message"].as_str().unwrap().contains("window_bytes"),
            "错误消息必须点名 window_bytes，body={body}"
        );
    }
}

/// 监看界面的时间显示必须带日期（issue #20 的后续修订）：
/// 日志窗口可跨多日，只显示时分让跨天的行无法判读。这里钉住渲染入口，
/// 防止后续改动把表格与提示的日期又裁回时分。
#[tokio::test]
async fn stability_ui_renders_timestamps_with_date() {
    let harness = spawn_proxy(&[], "\n[stability]\nenabled = true\n").await;
    let (status, _, body) = http_get_parts(harness.proxy_port, "/stability/ui").await;
    assert_eq!(status, "200", "body={body}");
    for marker in [
        "function fmtDateTime(",
        "text: fmtDateTime(d.timestampMs)",
        "text: fmtDateTime(t.timestampMs)",
        "fmtDateTime(d.timestampMs) + \" · 前缀分叉",
        "var tip = fmtDateTime(t.timestampMs)",
        "function fmtRange(",
    ] {
        assert!(
            body.contains(marker),
            "时间显示缺少带日期的渲染入口 {marker:?}，页面长度={}",
            body.len()
        );
    }
    // 表格的时间列不许残留「只有时分」的渲染入口（fmtTime 只允许作为 fmtDateTime 的内部实现）。
    assert!(
        !body.contains("text: fmtTime("),
        "表格时间列必须走带日期的格式化"
    );
}
