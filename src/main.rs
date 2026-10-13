//! codex-layer —— 独立运行的 codex relay 稳定层。
//!
//! HTTP 管道从 CodexPlusPlus PR #2449 的 `src/launcher.rs`（commit f9be7cf）
//! 里的「手写 HTTP/1.1 + 协议代理分发」改写为 axum：路由判定继续复用转换层的
//! `is_*_proxy_path`（保证与 CodexPlusPlus 认同样的 codex 路径别名），
//! 转发/回流行为逐条对齐原实现：
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

use std::error::Error as StdError;
use std::io::Read;

use anyhow::Context;
use axum::body::{Body, Bytes};
use axum::extract::Request;
use axum::http::{HeaderMap, Method, StatusCode, header};
use axum::response::Response;
use axum::routing::any;
use axum::Router;
use futures_util::StreamExt;
use serde_json::{Value, json};

use codex_layer::config::Config;
use codex_layer::cli::{self, Command};
use codex_layer::diagnostic_log;
use codex_layer::http_client;
use codex_layer::protocol_proxy::{
    self, ChatSseToResponsesConverter, CompactionSseConverter, ProxySessionHeaders, UpstreamWireApi,
    UpstreamProxyResponse, chat_completion_to_response, chat_completion_to_response_with_request,
    is_audio_transcriptions_proxy_path, is_chat_completions_proxy_path, is_image_edits_proxy_path,
    is_image_generations_proxy_path, is_models_proxy_path, is_responses_proxy_path,
    open_audio_transcriptions_proxy_request, open_chat_completions_proxy_request,
    open_image_edits_proxy_request, open_image_generations_proxy_request,
    open_models_proxy_request, open_responses_proxy_request_for_path_with_session_headers,
    responses_error_from_upstream, wrap_non_stream_response_as_compaction,
    upstream_conversion_error_response,
};

/// 请求体上限（含解压后），与 CodexPlusPlus 的协议代理一致：32 MB。
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

    let app = Router::new()
        .route("/healthz", any(healthz))
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
        let _ = diagnostic_log::append_diagnostic_log(
            "codex_layer.started",
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
        let _ = log_event(
            "codex_layer.responses_upgrade_required",
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

fn log_event(event: &str, detail: Value) -> std::io::Result<()> {
    diagnostic_log::append_diagnostic_log(event, detail)
}

fn proxy_error(path: &str, status: StatusCode, message: &str) -> Response {
    let _ = log_event(
        "codex_layer.proxy_failed",
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
fn proxy_conversion_error(path: &str, error: &anyhow::Error) -> Response {
    if let Some((status_code, body)) = upstream_conversion_error_response(error) {
        let status = StatusCode::from_u16(status_code).unwrap_or(StatusCode::BAD_REQUEST);
        let _ = log_event(
            "codex_layer.conversion_rejected",
            json!({ "path": path, "status": status_code, "error": error.to_string() }),
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
            let _ = log_event(
                "codex_layer.proxy_failed",
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
    let _ = log_event(
        "codex_layer.upstream_response",
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
async fn upstream_error(upstream: UpstreamProxyResponse, path: &str) -> anyhow::Result<Response> {
    let status_code = upstream.status_code;
    let content_type = upstream.content_type.clone();
    let body = upstream.response.bytes().await?;
    let error = responses_error_from_upstream(status_code, &content_type, &body);
    let _ = log_event(
        "codex_layer.upstream_error",
        json!({ "path": path, "statusCode": status_code, "message": error }),
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
        return match upstream_error(upstream, "/v1/models").await {
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
        Err(error) => return proxy_conversion_error(path, &error),
    };
    if !upstream.is_success() {
        return match upstream_error(upstream, path).await {
            Ok(response) => response,
            Err(error) => proxy_error(path, StatusCode::BAD_GATEWAY, &error.to_string()),
        };
    }

    if upstream.is_stream {
        let _ = log_event(
            "codex_layer.stream_open",
            json!({ "path": path, "wireApi": format!("{:?}", upstream.wire_api), "compaction": upstream.compaction }),
        );
        return sse_response(stream_from_upstream(upstream, request_json, model));
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
        Err(error) => return proxy_conversion_error(path, &error),
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

/// 把上游回包变成一条 Responses 事件流。三条分支与 CodexPlusPlus 的转发实现一一对应。
fn stream_from_upstream(
    upstream: UpstreamProxyResponse,
    request_json: Option<Value>,
    model: String,
) -> impl futures_util::Stream<Item = StreamItem> + Send + 'static {
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<StreamItem>();
    let wire_api = upstream.wire_api;
    let response = upstream.response;
    tokio::spawn(async move {
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
            return;
        }
        if wire_api == UpstreamWireApi::Responses {
            while let Some(chunk) = chunks.next().await {
                let item = chunk.map_err(|error| -> Box<dyn StdError + Send + Sync> { error.into() });
                if tx.send(item).is_err() {
                    return;
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
                        let _ = log_event(
                            "codex_layer.stream_failed",
                            json!({ "reason": "上游返回了流式错误事件" }),
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
                    let _ = log_event(
                        "codex_layer.stream_failed",
                        json!({ "reason": error.to_string() }),
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
            let _ = log_event(
                "codex_layer.stream_failed",
                json!({ "reason": "上游缺少完成事件" }),
            );
            return;
        }
        let tail = converter.finish();
        if !tail.is_empty() {
            let _ = tx.send(Ok(Bytes::from(tail)));
        }
        let _ = log_event("codex_layer.stream_completed", json!({ "model": model }));
    });
    futures_util::stream::unfold(rx, |mut rx| async move {
        rx.recv().await.map(|item| (item, rx))
    })
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
