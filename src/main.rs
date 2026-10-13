//! codex-layer —— 独立运行的 codex relay 稳定层。
//!
//! HTTP 管道基于 axum：路由判定继续复用转换层的 `is_*_proxy_path`（codex 客户端会
//! 发来多种路径别名，两侧必须认同一套），转发/回流行为逐条对齐客户端的既有期待：
//!
//! - `POST /v1/responses`（含 `/responses`、`/v1/v1/responses`、`/codex/v1/responses`
//!   与四种 `/responses/compact` 写法）：按上游方言转换请求，流式时把上游 SSE
//!   转成 Responses 事件流；压缩轮无论上游方言，都重组为恰好一个 `compaction` 输出项。
//! - `POST /v1/chat/completions`：上游同为 Chat Completions 时原样转发。
//! - `GET /v1/models`：拉上游模型列表。
//! - `POST /v1/audio/transcriptions`、`/v1/images/generations`、`/v1/images/edits`：透传。
//! - `GET` 响应式路径：426 提示改用 HTTP（codex 会尝试 WebSocket 传输）。
//!
//! 上游失败一律走 `responses_error_from_upstream`，把各家错误整成 Responses 错误信封，
//! 失败原因同时落进诊断日志——否则现场只能看到「502 Unknown error」。

use std::collections::HashMap;
use std::error::Error as StdError;
use std::io::Read;
use std::sync::Arc;

use anyhow::Context;
use axum::body::{Body, Bytes};
use axum::extract::Request;
use axum::extract::Query;
use axum::extract::State;
use axum::http::{HeaderMap, Method, StatusCode, header};
use axum::response::Response;
use axum::routing::any;
use axum::Router;
use futures_util::StreamExt;
use serde_json::{Value, json};

use codex_layer::config::Config;
use codex_layer::cli::{self, Command};
use codex_layer::diagnostic_log;
use codex_layer::diagnostic_log::events;
use codex_layer::http_client;
use codex_layer::protocol_proxy::{
    self, ChatSseToResponsesConverter, CompactionSseConverter, ProxySessionHeaders, UpstreamWireApi,
    UpstreamProxyResponse, chat_completion_to_response, chat_completion_to_response_with_request,
    NativeResponsesSseObserver, native_stream_failure_frame,
    is_audio_transcriptions_proxy_path, is_chat_completions_proxy_path, is_image_edits_proxy_path,
    is_image_generations_proxy_path, is_models_proxy_path, is_responses_proxy_path,
    open_audio_transcriptions_proxy_request, open_chat_completions_proxy_request,
    open_image_edits_proxy_request, open_image_generations_proxy_request,
    open_models_proxy_request, open_responses_proxy_request_for_path_with_session_headers,
    responses_error_from_upstream, wrap_non_stream_response_as_compaction,
    upstream_conversion_error_response,
};
use codex_layer::stability::{self, StabilityAggregateOptions};
use codex_layer::stability_ui;

/// 请求体上限（含解压后）：32 MB。
const MAX_HTTP_BODY_BYTES: usize = 32 * 1024 * 1024;

type StreamItem = Result<Bytes, Box<dyn StdError + Send + Sync + 'static>>;

fn main() -> anyhow::Result<()> {
    // ollama 式包命令：子命令与参数解析在 cli::parse_cli（纯函数，可单测）。
    let args: Vec<String> = std::env::args().skip(1).collect();
    let command = cli::parse_cli(&args)?;
    match command {
        Command::Help => {
            print_usage();
            Ok(())
        }
        Command::Version => {
            println!("codex-layer {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        Command::Serve { config } => run_serve(config),
        // 常驻四命令：macOS 走 launchctl，其余平台明确报不支持。
        command => cli::run_launchd_command(&command, &Config::default_path),
    }
}

/// 前台运行（serve，默认子命令）：装配与监听逻辑与旧 main 同源。
fn run_serve(config_override: Option<std::path::PathBuf>) -> anyhow::Result<()> {
    let config_path = config_override.unwrap_or_else(Config::default_path);
    let config = Config::load(&config_path)?;

    init_tracing();
    // 诊断日志路径可由配置改写（转换层的所有稳定化事件都落这里）。
    if !config.log.path.trim().is_empty() {
        diagnostic_log::set_diagnostic_log_path(Some(config.log.path.trim().into()));
    }
    http_client::set_connect_timeout(std::time::Duration::from_secs(config.timeouts.connect_seconds));
    protocol_proxy::set_upstream_timeouts(
        std::time::Duration::from_secs(config.timeouts.header_seconds),
        std::time::Duration::from_secs(config.timeouts.stream_header_seconds),
    );

    let settings = config.to_backend_settings()?;
    let active = settings.active_relay_profile();
    // 独立二进制的上游来自 TOML，注入进程内载体；转换层的纯函数不读配置，
    // 只有入口的装配流需要它。
    protocol_proxy::set_runtime_settings(settings.clone());

    // 稳定性监看端点（issue #19）：路由始终注册（Router 的状态泛型不能按配置
    // 分叉，强行条件注册会编译不过）；`[stability] enabled = false`（默认）时
    // 由 handler 显式返回 404「未启用」，与陌生路径同状态码。
    let stability_state = Arc::new(StabilityState {
        enabled: config.stability.enabled,
        window_bytes: config.stability.window_bytes.max(1),
        show_prompt_snippets: config.stability.show_prompt_snippets,
    });
    // 路径必须在 fallback 之前显式注册（照 /healthz 的写法），否则会被当成
    // OpenAI 请求处理。
    let app = Router::new()
        .route("/healthz", any(healthz))
        .route("/stability", any(stability_endpoint))
        .route("/stability/ui", any(stability_ui_endpoint))
        .with_state(stability_state)
        .fallback(any(dispatch));

    let host = config.server.host.clone();
    let port = config.server.port;
    let runtime = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;
    runtime.block_on(async move {
        let listener = tokio::net::TcpListener::bind((host.as_str(), port))
            .await
            .with_context(|| format!("无法监听 {host}:{port}"))?;
        tracing::info!(
            target: "codex-layer",
            listen = %format!("{host}:{port}"),
            upstream = %active.base_url,
            protocol = ?active.protocol,
            stabilize_prompt_prefix = active.stabilize_prompt_prefix,
            "codex-layer 已启动；把 codex 的 base_url 指到这里"
        );
        log_event(
            events::layer::STARTED,
            json!({
                "listen": format!("{host}:{port}"),
                "config": config_path.display().to_string(),
                "upstream": active.base_url,
                "protocol": format!("{:?}", active.protocol),
                "stabilizePromptPrefix": active.stabilize_prompt_prefix,
            }),
        );
        axum::serve(listener, app)
            .with_graceful_shutdown(shutdown_signal())
            .await?;
        Ok::<(), anyhow::Error>(())
    })
}

// 旧 resolve_config_path 已并入 cli::parse_cli（纯函数化，含子命令）。

fn print_usage() {
    println!(
        "codex-layer {} —— codex relay 的稳定层

用法：
  codex-layer [serve]   [--config <路径> | -c <路径>]   前台运行（默认子命令）
  codex-layer start     [--config <路径>] [--label <L>]  注册 LaunchAgent 常驻（macOS）
  codex-layer stop      [--label <L>]                    注销常驻（macOS）
  codex-layer restart   [--config <路径>] [--label <L>]  重写 plist 并整包重载
  codex-layer status    [--config <路径>] [--label <L>]  打印 pid / 监听口 / 配置路径
  codex-layer version                                     打印版本

常驻默认 Label：com.codex-layer.proxy；start 会把配置绝对路径与
CODEX_LAYER_CONFIG 环境写进 plist，标准输出/错误重定向到日志目录。
start/stop/restart/status 仅 macOS；serve/version 平台不受限。

配置默认查找顺序：
  $CODEX_LAYER_CONFIG -> ./codex-layer.toml -> ~/.codex-layer/codex-layer.toml

接入方式：把 codex 的 model_provider base_url 指到本代理（默认 127.0.0.1:57321/v1）。",
        env!("CARGO_PKG_VERSION")
    );
}

fn init_tracing() {
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::util::SubscriberInitExt;

    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    let format = tracing_subscriber::fmt::layer().with_target(false);
    // 同时把 info 以上写进诊断日志所在目录的运行日志，便于长会话事后取证。
    let registry = tracing_subscriber::registry().with(filter).with(format);
    let _ = registry.try_init();
}

async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut stream) => {
                stream.recv().await;
            }
            Err(_) => std::future::pending::<()>().await,
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
    tracing::info!(target: "codex-layer", "收到退出信号，停止接收新请求");
}

async fn healthz() -> Response {
    json_response(
        StatusCode::OK,
        json!({"status": "ok", "version": env!("CARGO_PKG_VERSION")}),
    )
}

/// `/stability` 端点的装配层状态：聚合本体是 `stability.rs` 的纯函数，
/// 这里只携带读取窗口与隐私开关（由 `[stability]` 配置注入）。
struct StabilityState {
    enabled: bool,
    window_bytes: u64,
    show_prompt_snippets: bool,
}

/// `GET /stability`：惰性读诊断日志尾部窗口，返回稳定性快照（只读，issue #19）。
/// 聚合是纯函数；时钟与文件窗口形态等装配层字段在这里补上。
async fn stability_endpoint(
    State(state): State<Arc<StabilityState>>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    if !state.enabled {
        return json_response(
            StatusCode::NOT_FOUND,
            json!({
                "status": "failed",
                "message": "稳定性监看端点未启用：在配置 [stability] 中设 enabled = true",
            }),
        );
    }
    let window_bytes = match parse_window_bytes_override(&params, state.window_bytes) {
        Ok(bytes) => bytes,
        Err(message) => {
            log_event(
                events::layer::STABILITY_BAD_REQUEST,
                json!({ "message": message }),
            );
            return json_response(
                StatusCode::BAD_REQUEST,
                json!({"status": "failed", "message": message}),
            );
        }
    };
    let options = StabilityAggregateOptions {
        show_prompt_snippets: state.show_prompt_snippets,
        ..StabilityAggregateOptions::default()
    };
    let path = diagnostic_log::diagnostic_log_path();
    let read = tokio::task::spawn_blocking(move || read_log_tail(&path, window_bytes)).await;
    let tail = match read {
        Ok(Ok(tail)) => tail,
        Ok(Err(error)) => {
            log_event(
                events::layer::STABILITY_READ_FAILED,
                json!({ "error": error.to_string() }),
            );
            return json_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                json!({"status": "failed", "message": format!("无法读取诊断日志：{error}")}),
            );
        }
        Err(error) => {
            return json_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                json!({"status": "failed", "message": format!("读取任务失败：{error}")}),
            );
        }
    };
    let lines: Vec<&str> = tail.text.split('\n').collect();
    let mut snapshot = stability::aggregate_stability_snapshot(&lines, &options);
    snapshot["generatedAtMs"] = json!(unix_now_ms());
    snapshot["window"]["fileBytes"] = json!(tail.file_bytes);
    snapshot["window"]["windowBytes"] = json!(window_bytes);
    snapshot["window"]["truncated"] = json!(tail.truncated);
    json_response(StatusCode::OK, snapshot)
}

/// `/stability` 查询参数 `window_bytes` 覆盖的上下界：下限 1 KB 防空转，
/// 上限 64 MB 防监看页误读整本日志。
const MIN_WINDOW_BYTES_OVERRIDE: u64 = 1_024;
const MAX_WINDOW_BYTES_OVERRIDE: u64 = 67_108_864;

/// 解析查询串里的 `window_bytes` 覆盖：缺省用配置默认值；非数字或 0 明确
/// 报 400（fail-loud：界面传错窗口要让它看见，静默吞掉会让监看页拿到
/// 口径不符的快照而不自知）；其余钳进上下界。
fn parse_window_bytes_override(
    params: &HashMap<String, String>,
    default_bytes: u64,
) -> Result<u64, String> {
    let Some(raw) = params.get("window_bytes") else {
        return Ok(default_bytes);
    };
    let parsed: u64 = raw
        .trim()
        .parse()
        .map_err(|_| format!("window_bytes 必须是字节数正整数（收到 {raw:?}）"))?;
    if parsed == 0 {
        return Err("window_bytes 不能为 0".to_string());
    }
    Ok(parsed.clamp(MIN_WINDOW_BYTES_OVERRIDE, MAX_WINDOW_BYTES_OVERRIDE))
}

/// `GET /stability/ui` 监看页（issue #20）：自包含 HTML，零外部依赖，
/// 始终 200——数据面未启用时渲染可读的禁用页而不是报错页。
async fn stability_ui_endpoint(State(state): State<Arc<StabilityState>>) -> Response {
    let html = stability_ui::render(!state.enabled);
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/html; charset=utf-8")
        .header(header::CACHE_CONTROL, "no-store")
        .body(Body::from(html))
        .unwrap_or_else(|error| {
            json_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                json!({"status": "failed", "message": format!("构建页面响应失败：{error}")}),
            )
        })
}

struct LogTail {
    text: String,
    file_bytes: u64,
    truncated: bool,
}

/// 读日志尾部 `window_bytes` 字节并对齐换行边界：首尾残缺行都丢弃——
/// 截断产物不该伪装成「解析失败」污染计数。日志不存在算空窗口（404 之外的
/// 最诚实形态是空快照）。
fn read_log_tail(path: &std::path::Path, window_bytes: u64) -> anyhow::Result<LogTail> {
    use std::io::{Seek, SeekFrom};
    let mut file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(LogTail {
                text: String::new(),
                file_bytes: 0,
                truncated: false,
            });
        }
        Err(error) => return Err(error.into()),
    };
    let file_bytes = file.metadata()?.len();
    let start = file_bytes.saturating_sub(window_bytes);
    if start > 0 {
        file.seek(SeekFrom::Start(start))?;
    }
    let mut tail = Vec::new();
    file.read_to_end(&mut tail)?;
    let mut text = String::from_utf8_lossy(&tail).into_owned();
    if start > 0 {
        if let Some(pos) = text.find('\n') {
            text.drain(..=pos);
        } else {
            text.clear();
        }
    }
    if let Some(pos) = text.rfind('\n') {
        text.truncate(pos + 1);
    } else {
        text.clear();
    }
    Ok(LogTail {
        text,
        file_bytes,
        truncated: start > 0,
    })
}

fn unix_now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

async fn dispatch(request: Request) -> Response {
    let (parts, body) = request.into_parts();
    let path = parts.uri.path().to_string();
    let method = parts.method.clone();
    let headers = parts.headers;

    if method == Method::OPTIONS {
        return empty_response(StatusCode::NO_CONTENT);
    }

    let content_encoding = header_value(&headers, "content-encoding");
    let user_agent = header_value(&headers, "user-agent");
    let beta_features = header_value(&headers, "x-codex-beta-features");
    let session_headers = ProxySessionHeaders {
        session_id: header_str(&headers, "session-id"),
        thread_id: header_str(&headers, "thread-id"),
        opencode_session: header_str(&headers, "x-opencode-session"),
    };

    if method == Method::GET && is_models_proxy_path(&path) {
        return models_proxy(user_agent.as_deref()).await;
    }
    if method == Method::GET && is_responses_proxy_path(&path) {
        // codex 会试探 WebSocket 传输；本层只支持 HTTP。
        log_event(
            events::layer::RESPONSES_UPGRADE_REQUIRED,
            json!({ "path": path }),
        );
        return json_response(
            StatusCode::UPGRADE_REQUIRED,
            json!({
                "status": "upgrade_required",
                "message": "WebSocket transport is not supported by the local protocol proxy; retry with HTTP."
            }),
        );
    }

    let raw = match axum::body::to_bytes(body, MAX_HTTP_BODY_BYTES).await {
        Ok(raw) => raw,
        Err(error) => {
            return proxy_error(
                &path,
                StatusCode::PAYLOAD_TOO_LARGE,
                &format!("请求体读取失败或超过上限：{error}"),
            );
        }
    };

    if is_audio_transcriptions_proxy_path(&path) && method == Method::POST {
        let content_type = header_value(&headers, "content-type").unwrap_or_default();
        return raw_upstream_proxy(
            open_audio_transcriptions_proxy_request(&raw, &content_type, user_agent.as_deref()).await,
            "audio_transcriptions",
            &path,
        )
        .await;
    }
    if (is_image_generations_proxy_path(&path)) && method == Method::POST {
        return raw_upstream_proxy(
            open_image_generations_proxy_request(&raw, user_agent.as_deref()).await,
            "image_generations",
            &path,
        )
        .await;
    }
    if is_image_edits_proxy_path(&path) && method == Method::POST {
        let content_type = header_value(&headers, "content-type").unwrap_or_default();
        return raw_upstream_proxy(
            open_image_edits_proxy_request(&raw, &content_type, user_agent.as_deref()).await,
            "image_edits",
            &path,
        )
        .await;
    }

    let text_body = match decode_request_body(&raw, content_encoding.as_deref()) {
        Ok(text) => text,
        Err(error) => {
            return proxy_error(&path, StatusCode::BAD_REQUEST, &error.to_string());
        }
    };

    if is_chat_completions_proxy_path(&path) && method == Method::POST {
        return chat_completions_proxy(&text_body, user_agent.as_deref(), &path).await;
    }
    if is_responses_proxy_path(&path) && method == Method::POST {
        return responses_proxy(
            &text_body,
            user_agent.as_deref(),
            beta_features.as_deref(),
            session_headers,
            &path,
        )
        .await;
    }

    json_response(
        StatusCode::NOT_FOUND,
        json!({
            "error": {
                "message": format!("codex-layer 不处理该路径：{method} {path}"),
                "type": "invalid_request_error",
            }
        }),
    )
}

/// 按头名取非空值；头名写错（非法字符）当作未配置，不报错。
fn header_value(headers: &HeaderMap, name: &str) -> Option<String> {
    header_str(headers, name).map(str::to_string)
}

fn header_str<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers
        .get(header::HeaderName::from_bytes(name.as_bytes()).ok()?)
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

/// 请求体解压：支持 `identity`、`zstd`、`gzip`。解压后仍然受 `MAX_HTTP_BODY_BYTES` 约束。
fn decode_request_body(body: &[u8], content_encoding: Option<&str>) -> anyhow::Result<String> {
    let encoding = content_encoding.unwrap_or_default().trim();
    let decoded: Vec<u8> = if encoding.is_empty() || encoding.eq_ignore_ascii_case("identity") {
        body.to_vec()
    } else if encoding.eq_ignore_ascii_case("zstd") {
        let decoder = zstd::stream::read::Decoder::new(std::io::Cursor::new(body))?;
        let mut limited = decoder.take((MAX_HTTP_BODY_BYTES + 1) as u64);
        let mut decoded = Vec::new();
        std::io::Read::read_to_end(&mut limited, &mut decoded)?;
        if decoded.len() > MAX_HTTP_BODY_BYTES {
            anyhow::bail!("解压后的请求体超过大小限制");
        }
        decoded
    } else if encoding.eq_ignore_ascii_case("gzip") {
        let mut decoder = flate2::read::GzDecoder::new(body);
        let mut decoded = Vec::new();
        std::io::Read::read_to_end(&mut decoder, &mut decoded)?;
        if decoded.len() > MAX_HTTP_BODY_BYTES {
            anyhow::bail!("解压后的请求体超过大小限制");
        }
        decoded
    } else {
        anyhow::bail!("不支持的 Content-Encoding：{encoding}");
    };
    String::from_utf8(decoded).map_err(|error| anyhow::anyhow!("请求体不是 UTF-8：{error}"))
}

fn log_event(event: &'static str, detail: Value) {
    // 纯转发：写入能力与失败口径都在 lib 统一入口（#31），bin 侧不留独立实现。
    diagnostic_log::log_event(event, detail)
}

fn proxy_error(path: &str, status: StatusCode, message: &str) -> Response {
    log_event(
        events::layer::PROXY_FAILED,
        json!({ "path": path, "status": status.as_u16(), "error": message }),
    );
    tracing::warn!(target: "codex-layer", path = %path, error = %message, "请求失败");
    json_response(
        status,
        json!({ "status": "failed", "message": message }),
    )
}

/// 转换层主动拒答的出口：闸门错误返回结构化的 Responses 错误（400 +
/// 可行动文案），其余错误保持原有的 502 + 文本消息行为不变。
fn proxy_conversion_error(
    path: &str,
    error: &anyhow::Error,
    request_id: Option<&str>,
) -> Response {
    if let Some((status_code, body)) = upstream_conversion_error_response(error) {
        let status = StatusCode::from_u16(status_code).unwrap_or(StatusCode::BAD_REQUEST);
        log_event(
            events::layer::CONVERSION_REJECTED,
            json!({ "path": path, "status": status_code, "error": error.to_string(), "requestId": request_id }),
        );
        tracing::warn!(target: "codex-layer", path = %path, error = %error, "转换层拒绝请求");
        return json_response(status, body);
    }
    proxy_error(path, StatusCode::BAD_GATEWAY, &error.to_string())
}

fn json_response(status: StatusCode, value: Value) -> Response {
    let body = serde_json::to_vec(&value).unwrap_or_else(|_| b"{}".to_vec());
    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "application/json; charset=utf-8")
        .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
        .body(Body::from(body))
        .unwrap()
}

fn empty_response(status: StatusCode) -> Response {
    Response::builder()
        .status(status)
        .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
        .header(
            header::ACCESS_CONTROL_ALLOW_METHODS,
            "GET, POST, OPTIONS",
        )
        .header(
            header::ACCESS_CONTROL_ALLOW_HEADERS,
            "Content-Type, Authorization",
        )
        .body(Body::empty())
        .unwrap()
}

/// 语音转写 / 图片生成这类旁路：不做协议转换，状态码与正文原样带回。
async fn raw_upstream_proxy(
    result: anyhow::Result<UpstreamProxyResponse>,
    kind: &str,
    path: &str,
) -> Response {
    let upstream = match result {
        Ok(upstream) => upstream,
        Err(error) => {
            log_event(
                events::layer::PROXY_FAILED,
                json!({ "kind": kind, "path": path, "error": error.to_string() }),
            );
            return json_response(StatusCode::BAD_GATEWAY, json!({"status": "failed", "message": error.to_string()}));
        }
    };
    let status = StatusCode::from_u16(upstream.status_code).unwrap_or(StatusCode::BAD_GATEWAY);
    let content_type = if upstream.content_type.is_empty() {
        "application/json; charset=utf-8".to_string()
    } else {
        upstream.content_type.clone()
    };
    log_event(
        events::layer::UPSTREAM_RESPONSE,
        json!({ "kind": kind, "path": path, "statusCode": upstream.status_code }),
    );
    match upstream.response.bytes().await {
        Ok(body) => Response::builder()
            .status(status)
            .header(header::CONTENT_TYPE, content_type)
            .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
            .body(Body::from(body))
            .unwrap(),
        Err(error) => proxy_error(path, StatusCode::BAD_GATEWAY, &error.to_string()),
    }
}

/// 上游错误的统一出口：错误信封按 Responses 协议整形，状态码沿用上游。
async fn upstream_error(
    upstream: UpstreamProxyResponse,
    path: &str,
    request_id: Option<&str>,
) -> anyhow::Result<Response> {
    let status_code = upstream.status_code;
    let content_type = upstream.content_type.clone();
    let body = upstream.response.bytes().await?;
    let error = responses_error_from_upstream(status_code, &content_type, &body);
    log_event(
        events::layer::UPSTREAM_ERROR,
        json!({ "path": path, "statusCode": status_code, "message": error, "requestId": request_id }),
    );
    Ok(json_response(
        StatusCode::from_u16(status_code).unwrap_or(StatusCode::BAD_GATEWAY),
        error,
    ))
}

async fn models_proxy(user_agent: Option<&str>) -> Response {
    let upstream = match open_models_proxy_request(user_agent).await {
        Ok(upstream) => upstream,
        Err(error) => return proxy_error("/v1/models", StatusCode::BAD_GATEWAY, &error.to_string()),
    };
    if !upstream.is_success() {
        return match upstream_error(upstream, "/v1/models", None).await {
            Ok(response) => response,
            Err(error) => proxy_error("/v1/models", StatusCode::BAD_GATEWAY, &error.to_string()),
        };
    }
    passthrough_response(upstream, "/v1/models").await
}

/// 非流式透传：状态码、Content-Type、正文原样带回。
async fn passthrough_response(upstream: UpstreamProxyResponse, path: &str) -> Response {
    let status = StatusCode::from_u16(upstream.status_code).unwrap_or(StatusCode::BAD_GATEWAY);
    let content_type = if upstream.content_type.is_empty() {
        "application/json; charset=utf-8".to_string()
    } else {
        upstream.content_type.clone()
    };
    match upstream.response.bytes().await {
        Ok(body) => Response::builder()
            .status(status)
            .header(header::CONTENT_TYPE, content_type)
            .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
            .body(Body::from(body))
            .unwrap(),
        Err(error) => proxy_error(path, StatusCode::BAD_GATEWAY, &error.to_string()),
    }
}

async fn responses_proxy(
    body: &str,
    user_agent: Option<&str>,
    beta_features: Option<&str>,
    session_headers: ProxySessionHeaders<'_>,
    path: &str,
) -> Response {
    let request_json: Option<Value> = serde_json::from_str(body).ok();
    // 请求级关联 id（issue #19）：只进诊断日志，绝不进发往上游的请求体或请求头——
    // 出站体每轮多一个随机字段 = 前缀每轮都变 = 钉定与深缓存全废。
    let request_id = uuid::Uuid::new_v4().to_string();
    let model = request_json
        .as_ref()
        .and_then(|request| request.get("model"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();

    // 上游配置来自启动时注入的运行时载体；这里只透传客户端上下文（UA、beta、会话头）。
    let upstream = match open_responses_proxy_request_for_path_with_session_headers(
        body,
        user_agent,
        path,
        beta_features,
        session_headers,
    )
    .await
    {
        Ok(upstream) => upstream,
        Err(error) => return proxy_conversion_error(path, &error, Some(&request_id)),
    };
    if !upstream.is_success() {
        return match upstream_error(upstream, path, Some(&request_id)).await {
            Ok(response) => response,
            Err(error) => proxy_error(path, StatusCode::BAD_GATEWAY, &error.to_string()),
        };
    }

    if upstream.is_stream {
        log_event(
            events::layer::STREAM_OPEN,
            json!({ "path": path, "wireApi": format!("{:?}", upstream.wire_api), "compaction": upstream.compaction, "requestId": request_id.clone() }),
        );
        return sse_response(stream_from_upstream(
            upstream,
            request_json,
            model,
            request_id,
        ));
    }

    let upstream_body = match upstream.response.bytes().await {
        Ok(bytes) => bytes,
        Err(error) => return proxy_error(path, StatusCode::BAD_GATEWAY, &error.to_string()),
    };
    if upstream.compaction {
        // 压缩轮非流式：无论上游方言，都重组为恰好一个 compaction 输出项。
        match wrap_non_stream_response_as_compaction(&upstream_body, &model) {
            Ok(payload) => Response::builder()
                .status(StatusCode::OK)
                .header(header::CONTENT_TYPE, "text/event-stream; charset=utf-8")
                .header(header::CACHE_CONTROL, "no-cache")
                .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
                .body(Body::from(payload))
                .unwrap(),
            Err(error) => proxy_error(path, StatusCode::BAD_GATEWAY, &error.to_string()),
        }
    } else if upstream.wire_api == UpstreamWireApi::Responses {
        Response::builder()
            .status(StatusCode::OK)
            .header(
                header::CONTENT_TYPE,
                if upstream.content_type.is_empty() {
                    "application/json; charset=utf-8"
                } else {
                    &upstream.content_type
                },
            )
            .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
            .body(Body::from(upstream_body))
            .unwrap()
    } else {
        // Chat Completions 上游的非流式回包：整成 Responses 响应对象。
        let converted = if let Some(request_json) = request_json.as_ref() {
            chat_completion_to_response_with_request(
                match serde_json::from_slice::<Value>(&upstream_body) {
                    Ok(json) => json,
                    Err(error) => {
                        return proxy_error(path, StatusCode::BAD_GATEWAY, &error.to_string())
                    }
                },
                request_json,
            )
        } else {
            chat_completion_to_response(match serde_json::from_slice::<Value>(&upstream_body) {
                Ok(json) => json,
                Err(error) => {
                    return proxy_error(path, StatusCode::BAD_GATEWAY, &error.to_string());
                }
            })
        };
        match converted.and_then(|json| Ok(serde_json::to_vec(&json)?)) {
            Ok(payload) => Response::builder()
                .status(StatusCode::OK)
                .header(header::CONTENT_TYPE, "application/json; charset=utf-8")
                .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
                .body(Body::from(payload))
                .unwrap(),
            Err(error) => proxy_error(path, StatusCode::BAD_GATEWAY, &error.to_string()),
        }
    }
}

async fn chat_completions_proxy(body: &str, user_agent: Option<&str>, path: &str) -> Response {
    let upstream = match open_chat_completions_proxy_request(body, user_agent).await
    {
        Ok(upstream) => upstream,
        Err(error) => return proxy_conversion_error(path, &error, None),
    };
    let status = StatusCode::from_u16(upstream.status_code).unwrap_or(StatusCode::BAD_GATEWAY);
    let content_type = if upstream.content_type.is_empty() {
        "application/json; charset=utf-8".to_string()
    } else {
        upstream.content_type.clone()
    };
    if upstream.is_stream && upstream.is_success() {
        // 客户端与上游同为 Chat Completions：SSE 原样回流，不做方言转换。
        let stream = upstream
            .response
            .bytes_stream()
            .map(|chunk| chunk.map_err(|error| -> Box<dyn StdError + Send + Sync> { error.into() }));
        return Response::builder()
            .status(status)
            .header(header::CONTENT_TYPE, content_type)
            .header(header::CACHE_CONTROL, "no-cache")
            .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
            .body(Body::from_stream(stream))
            .unwrap();
    }
    match upstream.response.bytes().await {
        Ok(payload) => Response::builder()
            .status(status)
            .header(header::CONTENT_TYPE, content_type)
            .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
            .body(Body::from(payload))
            .unwrap(),
        Err(error) => proxy_error(path, StatusCode::BAD_GATEWAY, &error.to_string()),
    }
}

/// 把上游回包变成一条 Responses 事件流：压缩轮、Responses 方言、Chat 方言三条分支。
fn stream_from_upstream(
    upstream: UpstreamProxyResponse,
    request_json: Option<Value>,
    model: String,
    request_id: String,
) -> impl futures_util::Stream<Item = StreamItem> + Send + 'static {
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<StreamItem>();
    let wire_api = upstream.wire_api;
    let response = upstream.response;
    tokio::spawn(async move {
        // 后面每个诊断事件只借用一次：影子绑定成 &str，json! 序列化时各自复制。
        let request_id = request_id.as_str();
        let mut chunks = response.bytes_stream();
        if upstream.compaction {
            // 压缩轮无增量展示诉求：收齐上游文本后一次性下发单个 compaction 项。
            // SSE 事件可能被网络 chunk 拆开，converter 内部按事件边界缓冲。
            let mut converter = CompactionSseConverter::new(&model);
            if wire_api != UpstreamWireApi::Responses {
                converter = converter.with_chat_upstream();
            }
            while let Some(chunk) = chunks.next().await {
                match chunk {
                    Ok(bytes) => converter.push_upstream_bytes(&bytes),
                    Err(error) => {
                        converter.fail(format!("Stream error: {error}"), None);
                        break;
                    }
                }
            }
            let payload = converter.finish();
            let _ = tx.send(Ok(Bytes::from(payload)));
            // 压缩轮也要有收口事件（issue #19）：没有收口的 stream_open 会永远卡在
            // unpaired 里，单轮耗时分布统计也就无从谈起。
            // 口径：收口时间记在 payload 下发之时，不含客户端读流耗时——
            // 压缩轮耗时系统性偏小是有意的服务端侧口径，便于跨轮对比（PR #24 review）。
            log_event(
                events::layer::STREAM_COMPLETED,
                json!({ "model": model.clone(), "requestId": request_id, "compaction": true }),
            );
            return;
        }
        if wire_api == UpstreamWireApi::Responses {
            // 原生 Responses 透传支路：字节原样转发，观察者只旁路读，不产出任何字节。
            // 守卫要解决的是「上游在完成事件前断流」：原样转发时客户端只看到连接关闭，
            // 分不清正常结束还是半途而废，所以收口时由本层补一帧显式 response.failed。
            let mut observer = NativeResponsesSseObserver::default();
            while let Some(chunk) = chunks.next().await {
                match chunk {
                    Ok(bytes) => {
                        observer.push_bytes(&bytes);
                        if tx.send(Ok(bytes)).is_err() {
                            // 客户端已经断开：没有接收方，补帧无意义，直接收工。
                            return;
                        }
                    }
                    Err(error) => {
                        // 读流错误：先补帧，再按既有行为把错误交给 HTTP 层中止 body；
                        // 这个顺序保证客户端至少还能拿到一个显式终止信号。
                        let reason = close_native_responses_stream(observer, "stream_error", &tx);
                        let _ = log_event(
                            "codex_layer.stream_failed",
                            json!({ "reason": reason.unwrap_or_else(|| "原生 Responses 上游已自报终止失败".to_string()), "requestId": request_id }),
                        );
                        let item: StreamItem =
                            Err(Box::new(error) as Box<dyn StdError + Send + Sync>);
                        let _ = tx.send(item);
                        return;
                    }
                }
            }
            // HTTP EOF 不是 Responses 协议的完成事件，`[DONE]` 也不算：
            // 没观察到完成事件就一律补失败帧（与 Chat 泵的收口口径一致）。
            match close_native_responses_stream(observer, "stream_incomplete", &tx) {
                Some(reason) => {
                    let _ = log_event(
                        "codex_layer.stream_failed",
                        json!({ "reason": reason, "requestId": request_id }),
                    );
                }
                None => {
                    let _ = log_event(
                        "codex_layer.stream_completed",
                        json!({ "model": model, "requestId": request_id }),
                    );
                }
            }
            return;
        }
        // Chat Completions 上游 → Responses 事件流
        let mut converter = request_json
            .as_ref()
            .map(ChatSseToResponsesConverter::with_request)
            .unwrap_or_default();
        while let Some(chunk) = chunks.next().await {
            match chunk {
                Ok(bytes) => {
                    let converted = converter.push_bytes(&bytes);
                    if !converted.is_empty() && tx.send(Ok(Bytes::from(converted))).is_err() {
                        return;
                    }
                    if converter.has_failed() {
                        log_event(
                            events::layer::STREAM_FAILED,
                            json!({ "reason": "上游返回了流式错误事件", "requestId": request_id }),
                        );
                        return;
                    }
                }
                Err(error) => {
                    let failed = converter.fail(
                        "上游响应流读取失败".to_string(),
                        Some("stream_error".to_string()),
                    );
                    let _ = tx.send(Ok(Bytes::from(failed)));
                    log_event(
                        events::layer::STREAM_FAILED,
                        json!({ "reason": error.to_string(), "requestId": request_id }),
                    );
                    return;
                }
            }
        }
        // EOF 不是 Chat 协议的完成事件：已发出的文本和工具增量不能自动重放，
        // 必须显式补一个 failed 事件，否则客户端会永远等下去。
        if !converter.has_terminal_event() {
            let failed = converter.fail(
                "上游在完成事件前结束了响应流".to_string(),
                Some("stream_incomplete".to_string()),
            );
            let _ = tx.send(Ok(Bytes::from(failed)));
            log_event(
                events::layer::STREAM_FAILED,
                json!({ "reason": "上游缺少完成事件", "requestId": request_id }),
            );
            return;
        }
        let tail = converter.finish();
        let terminal_failed = converter.terminal_failed();
        if !tail.is_empty() {
            let _ = tx.send(Ok(Bytes::from(tail)));
        }
        // failed 收口的一轮绝不能记成「正常完成」（issue #13 残余 3）：终态事件是
        // response.failed 时，诊断记 stream_failed。
        if terminal_failed {
            log_event(
                events::layer::STREAM_FAILED,
                json!({ "reason": "转换层以 response.failed 终态收口", "requestId": request_id }),
            );
        } else {
            log_event(
                events::layer::STREAM_COMPLETED,
                json!({ "model": model, "requestId": request_id }),
            );
        }
    });
    futures_util::stream::unfold(rx, |mut rx| async move {
        rx.recv().await.map(|item| (item, rx))
    })
}

/// 原生透传支路的收口。守卫未证明成功时，向客户端补一帧显式 `response.failed`
///（口径同转换层：只发 failed 事件、不补 `[DONE]`，见 `native_stream_failure_frame`）。
/// 上游自己已经宣告过终止失败（failed / incomplete / error）时**不补帧**：客户端手里
/// 已有显式终止信号，再补只是重复噪音；但这一轮仍算失败，所以照样返回原因。
/// 返回 `Some(原因)` 表示这一轮诊断应记 `stream_failed`，`None` 表示观察到了完成事件。
fn close_native_responses_stream(
    observer: NativeResponsesSseObserver,
    error_type: &str,
    tx: &tokio::sync::mpsc::UnboundedSender<StreamItem>,
) -> Option<String> {
    let saw_terminal_failure = observer.saw_terminal_failure();
    let reason = observer.finish()?;
    if !saw_terminal_failure {
        let _ = tx.send(Ok(Bytes::from(native_stream_failure_frame(
            &reason,
            error_type,
        ))));
    }
    Some(reason)
}

fn sse_response(stream: impl futures_util::Stream<Item = StreamItem> + Send + 'static) -> Response {
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/event-stream; charset=utf-8")
        .header(header::CACHE_CONTROL, "no-cache")
        .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
        .body(Body::from_stream(stream))
        .unwrap_or_else(|error| {
            proxy_error("/v1/responses", StatusCode::BAD_GATEWAY, &error.to_string())
        })
}

/// 收口函数的单元对拍：补帧与否只看「上游有没有自报终止失败」，
/// 而「这一轮算不算失败」只看守卫收口结果，两者必须分开成立。
#[cfg(test)]
mod native_stream_guard_tests {
    use super::{Bytes, StreamItem, close_native_responses_stream, native_stream_failure_frame};
    use codex_layer::protocol_proxy::NativeResponsesSseObserver;

    fn observer_with(payload: &str) -> NativeResponsesSseObserver {
        let mut observer = NativeResponsesSseObserver::default();
        observer.push_bytes(payload.as_bytes());
        observer
    }

    /// 未证明成功且上游没自报失败：必须补且只补一帧，内容就是失败收口帧。
    #[test]
    fn unproven_stream_gets_exactly_one_failure_frame() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<StreamItem>();
        let observer = observer_with(
            "data: {\"type\":\"response.output_text.delta\",\"delta\":\"半句\"}\n\n",
        );
        let reason = close_native_responses_stream(observer, "stream_incomplete", &tx)
            .expect("无终止事件的流不得算成功");
        let item = rx.try_recv().expect("必须补一帧").expect("补帧不是错误项");
        assert_eq!(
            item,
            Bytes::from(native_stream_failure_frame(&reason, "stream_incomplete")),
            "补的帧必须就是失败收口帧，错误 type 沿用调用方口径"
        );
        assert!(
            rx.try_recv().is_err(),
            "只能补一帧，多补会被客户端当成重复终态"
        );
    }

    /// 上游自报 `response.failed`：仍算失败（要返回原因），但不补帧。
    #[test]
    fn self_reported_failure_reports_reason_without_a_frame() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<StreamItem>();
        let observer = observer_with("data: {\"type\":\"response.failed\"}\n\n");
        let reason = close_native_responses_stream(observer, "stream_incomplete", &tx)
            .expect("上游自报失败也是失败");
        assert!(reason.contains("response.failed"));
        assert!(
            rx.try_recv().is_err(),
            "客户端已有显式终止信号，再补只是重复噪音"
        );
    }

    /// 观察到完成事件：既不补帧也不报失败。
    #[test]
    fn completed_stream_appends_nothing_and_reports_no_failure() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<StreamItem>();
        let observer = observer_with(
            "data: {\"type\":\"response.completed\"}\n\ndata: [DONE]\n\n",
        );
        assert!(
            close_native_responses_stream(observer, "stream_incomplete", &tx).is_none()
        );
        assert!(rx.try_recv().is_err());
    }
}

#[cfg(test)]
mod stability_endpoint_tests {
    use super::{
        MAX_WINDOW_BYTES_OVERRIDE, MIN_WINDOW_BYTES_OVERRIDE, parse_window_bytes_override,
    };
    use std::collections::HashMap;

    fn query(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn window_bytes_override_absent_uses_config_default() {
        let params = query(&[]);
        assert_eq!(parse_window_bytes_override(&params, 8192), Ok(8192));
    }

    #[test]
    fn window_bytes_override_clamps_to_bounds() {
        let small = query(&[("window_bytes", "10")]);
        assert_eq!(
            parse_window_bytes_override(&small, 8192),
            Ok(MIN_WINDOW_BYTES_OVERRIDE)
        );
        let big = query(&[("window_bytes", "999999999999")]);
        assert_eq!(
            parse_window_bytes_override(&big, 8192),
            Ok(MAX_WINDOW_BYTES_OVERRIDE)
        );
        let mid = query(&[("window_bytes", " 65536 ")]);
        assert_eq!(parse_window_bytes_override(&mid, 8192), Ok(65536));
    }

    #[test]
    fn window_bytes_override_fails_loud_on_garbage() {
        for bad in ["abc", "0", "-1", "12.5", ""] {
            let params = query(&[("window_bytes", bad)]);
            assert!(
                parse_window_bytes_override(&params, 8192).is_err(),
                "非法值 {bad:?} 必须 fail-loud 报 400，而不是静默钳位或吞掉"
            );
        }
    }
}
