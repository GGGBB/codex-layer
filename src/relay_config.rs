//! 上游地址/密钥解析与连通性测试。
//!
//! 移植自 CodexPlusPlus PR #2449 的
//! `crates/codex-plus-core/src/relay_config.rs`（commit f9be7cf），只留下协议层
//! 需要的三个函数。删掉的上游专有逻辑：从 `configContents`（codex 的
//! config.toml 快照）里挖 base_url / experimental_bearer_token、官方登录态
//! 上游、聚合模式回指本地代理等——本项目没有这些概念，地址与密钥一律显式配置。

use serde::Serialize;
use serde_json::Value;

use crate::http_client;
use crate::protocol_proxy::{chat_completions_url, responses_url};
use crate::relay_headers;
use crate::settings::{RelayProfile, RelayProtocol};

/// 上游 Base URL：`upstreamBaseUrl` 优先，其次 `baseUrl`。
///
/// 保留这个优先级是为了让从 CodexPlusPlus 迁移过来的设置不用改字段：那边
/// `baseUrl` 可能被回指到本地代理，真上游写在 `upstreamBaseUrl`。
pub fn relay_profile_base_url(profile: &RelayProfile) -> String {
    let upstream = profile.upstream_base_url.trim();
    if !upstream.is_empty() {
        return upstream.to_string();
    }
    profile.base_url.trim().to_string()
}

/// 上游密钥：免认证上游一律返回空串，绝不把本地代理的占位 token 当凭据发出去。
pub fn relay_profile_api_key(profile: &RelayProfile) -> String {
    if profile.uses_no_auth() {
        return String::new();
    }
    profile.api_key.trim().to_string()
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RelayProfileTestResult {
    pub http_status: u16,
    pub endpoint: String,
    pub response_preview: String,
}

/// 响应预览只用于人工排障，截断即可，不能把整段上游回包灌进设置里。
const RESPONSE_PREVIEW_CHARS: usize = 400;

fn relay_profile_test_payload(protocol: RelayProtocol, model: &str) -> Value {
    match protocol {
        RelayProtocol::Responses => serde_json::json!({
            "model": model,
            "input": "hi",
            "max_output_tokens": 16
        }),
        RelayProtocol::ChatCompletions => serde_json::json!({
            "model": model,
            "messages": [{ "role": "user", "content": "hi" }],
            "max_tokens": 16
        }),
    }
}

/// 连通性测试：向最小请求打一次，返回状态码与响应预览。
///
/// 404 且地址末尾没有 `/v1` 时自动补 `/v1` 重试一次——很多自建服务器和中转站
/// 暴露的路径以 `/v1/` 开头，用户容易漏写这段前缀。
pub async fn test_relay_profile(
    profile: &RelayProfile,
    model: &str,
) -> anyhow::Result<RelayProfileTestResult> {
    let base_url = relay_profile_base_url(profile);
    let base_url = base_url.trim().trim_end_matches('/');
    if base_url.is_empty() {
        anyhow::bail!("Base URL 不能为空");
    }
    let api_key = relay_profile_api_key(profile);
    if api_key.is_empty() && !profile.uses_no_auth() {
        anyhow::bail!("API Key 不能为空");
    }
    let test_model = model.trim();
    if test_model.is_empty() {
        anyhow::bail!("测试模型不能为空");
    }

    let client = http_client::proxied_client(concat!("codex-layer/", env!("CARGO_PKG_VERSION")))?;
    let endpoint = match profile.protocol {
        RelayProtocol::Responses => responses_url(base_url),
        RelayProtocol::ChatCompletions => chat_completions_url(base_url),
    };
    let payload = relay_profile_test_payload(profile.protocol, test_model);

    let (http_status, response_preview, final_endpoint) = match send_relay_test(
        &client,
        &endpoint,
        profile,
        &payload,
    )
    .await
    {
        Ok(result) => result,
        Err(error) => return Err(error),
    };
    if http_status == 404 && !base_url.ends_with("/v1") {
        let v1_endpoint = match profile.protocol {
            RelayProtocol::Responses => responses_url(&format!("{base_url}/v1")),
            RelayProtocol::ChatCompletions => chat_completions_url(&format!("{base_url}/v1")),
        };
        let (retry_status, retry_preview, _) =
            send_relay_test(&client, &v1_endpoint, profile, &payload).await?;
        // 补 /v1 后不再是 404 就采用重试结果，否则保留原始 404 与预览。
        if retry_status != 404 {
            return Ok(RelayProfileTestResult {
                http_status: retry_status,
                endpoint: v1_endpoint,
                response_preview: retry_preview,
            });
        }
    }
    let _ = final_endpoint;
    Ok(RelayProfileTestResult {
        http_status,
        endpoint,
        response_preview,
    })
}

async fn send_relay_test(
    client: &reqwest::Client,
    endpoint: &str,
    profile: &RelayProfile,
    payload: &Value,
) -> anyhow::Result<(u16, String, String)> {
    let request = client
        .post(endpoint)
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .json(payload);
    let request = relay_headers::apply(request, profile);
    let response = request
        .send()
        .await
        .map_err(|error| anyhow::anyhow!("上游请求失败：{error}"))?;
    let http_status = response.status().as_u16();
    let body = response
        .text()
        .await
        .unwrap_or_default()
        .chars()
        .take(RESPONSE_PREVIEW_CHARS)
        .collect::<String>();
    Ok((http_status, body, endpoint.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::RelayMode;

    #[test]
    fn upstream_base_url_wins() {
        let profile = RelayProfile {
            base_url: "http://127.0.0.1:57321/v1".to_string(),
            upstream_base_url: "http://127.0.0.1:11234/v1".to_string(),
            ..RelayProfile::default()
        };
        assert_eq!(relay_profile_base_url(&profile), "http://127.0.0.1:11234/v1");
    }

    #[test]
    fn no_auth_profile_has_no_credentials() {
        let profile = RelayProfile {
            relay_mode: RelayMode::PureApi,
            no_auth: true,
            api_key: "codex-layer-no-auth".to_string(),
            ..RelayProfile::default()
        };
        assert!(profile.uses_no_auth());
        assert_eq!(relay_profile_api_key(&profile), "");
    }
}
