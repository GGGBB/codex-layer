//! TOML 配置层。
//!
//! 本项目自有的配置入口；内部数据模型仍留在 `settings.rs`，
//! 作为内部载体与迁移兼容路径。
//!
//! 设计原则：字段名直白、未知字段直接报错（写错配置要立刻炸，不要静默按默认跑）。
//! 一份最小配置只需要 `[upstream]` 的 `base_url` / `api_key` / `protocol`。
//!
//! ```toml
//! [server]
//! host = "127.0.0.1"
//! port = 12333
//!
//! [upstream]
//! base_url = "http://127.0.0.1:11234/v1"
//! api_key = "YOUR_UPSTREAM_KEY"
//! protocol = "chat_completions"
//!
//! [stabilizer]
//! prompt_prefix = true
//! ```

use std::path::{Path, PathBuf};

use anyhow::Context;
use serde::Deserialize;

use crate::paths;
use crate::settings::{
    BackendSettings, RelayHeaderKeyValue, RelayModelRoute, RelayMode, RelayProfile, RelayProtocol,
};

use crate::protocol_proxy::DEFAULT_PROXY_PORT;

/// 默认监听端口，可被 `[server] port` 覆盖。
///
/// 取值派生自 [`DEFAULT_PROXY_PORT`]，不在本模块另写一份端口字面量：「默认监听口」与
/// 「回环守卫的默认识别形态」必须共用同一个真源，否则用户没写 `[server] port` 时会出现
/// 代理实际监听 A 、守卫按 B 判定，把真实回环风险放过去。
pub const DEFAULT_LISTEN_PORT: u16 = DEFAULT_PROXY_PORT;

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub server: ServerConfig,
    #[serde(default)]
    pub upstream: UpstreamConfig,
    #[serde(default)]
    pub stabilizer: StabilizerConfig,
    #[serde(default)]
    pub stability: StabilityConfig,
    #[serde(default)]
    pub images: ImagesConfig,
    #[serde(default)]
    pub timeouts: TimeoutConfig,
    #[serde(default)]
    pub log: LogConfig,
    #[serde(default)]
    pub model_route: Vec<ModelRouteConfig>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServerConfig {
    /// 监听地址。默认只监听回环：本层可能持有上游密钥，不该默认暴露到局域网。
    #[serde(default = "default_host")]
    pub host: String,
    #[serde(default = "default_port")]
    pub port: u16,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            host: default_host(),
            port: default_port(),
        }
    }
}

fn default_host() -> String {
    "127.0.0.1".to_string()
}

fn default_port() -> u16 {
    DEFAULT_LISTEN_PORT
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct UpstreamConfig {
    #[serde(default)]
    pub base_url: String,
    #[serde(default)]
    pub api_key: String,
    /// 上游方言：`responses` 或 `chat_completions`。
    #[serde(default)]
    pub protocol: ProtocolFormat,
    /// 上游的默认模型名，请求未带 model 时使用。
    #[serde(default)]
    pub model: String,
    /// 上游不需要认证：转发时不注入任何 Authorization。
    #[serde(default)]
    pub no_auth: bool,
    /// 固定 User-Agent；留空则透传 codex 客户端的原始 UA。
    #[serde(default)]
    pub user_agent: String,
    /// 追加到上游请求的自定义头。
    #[serde(default)]
    pub header: Vec<HeaderConfig>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HeaderConfig {
    #[serde(default)]
    pub key: String,
    #[serde(default)]
    pub value: String,
}

/// 上游方言的配置文件写法。同时接受 `chat_completions` / `chatCompletions` /
/// `chat-completions` 三种常见拼法，避免用户按驼峰或连字符写法踩空。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProtocolFormat {
    #[default]
    Responses,
    #[serde(alias = "chatCompletions", alias = "chat-completions")]
    ChatCompletions,
}

impl ProtocolFormat {
    pub fn to_relay_protocol(self) -> RelayProtocol {
        match self {
            Self::Responses => RelayProtocol::Responses,
            Self::ChatCompletions => RelayProtocol::ChatCompletions,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StabilizerConfig {
    /// 「prompt 前缀稳定层」总开关：resize notice 剥离、system 块规范序、
    /// 末轮 user 标记、压缩轮 tools 回填与前缀对齐、前缀分叉探针。
    /// 关掉之后转发行为与不开本层逐字节一致。
    #[serde(default = "default_true")]
    pub prompt_prefix: bool,
    /// 严格 OpenAI 协议：剥掉各家私有的 reasoning 方言。
    /// 接自建服务器（mlx-serve、llama.cpp、vLLM）时一般不需要开。
    #[serde(default)]
    pub standard_openai_protocol: bool,
}

impl Default for StabilizerConfig {
    fn default() -> Self {
        Self {
            prompt_prefix: default_true(),
            standard_openai_protocol: false,
        }
    }
}

fn default_true() -> bool {
    true
}

/// 稳定性监看数据面（issue #19）：`GET /stability` 只读快照的开关与读取边界。
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StabilityConfig {
    /// 是否启用 `GET /stability` 响应。路由始终注册（照 /healthz 写法挂在
    /// fallback 之前）；默认关，关闭时 handler 显式返回 404「未启用」。
    #[serde(default)]
    pub enabled: bool,
    /// 读诊断日志的尾部字节窗口上限：日志可达上百 MB，只读尾部窗口。
    #[serde(default = "default_stability_window_bytes")]
    pub window_bytes: u64,
    /// 是否在快照里输出 divergence 的 `oldSnip`/`newSnip` 截断片段（上限 120 字符）。
    /// 默认关：snip 可能含用户提示词，只给长度与位置不外传。
    #[serde(default)]
    pub show_prompt_snippets: bool,
}

impl Default for StabilityConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            window_bytes: default_stability_window_bytes(),
            show_prompt_snippets: false,
        }
    }
}

fn default_stability_window_bytes() -> u64 {
    8 * 1024 * 1024
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct ImagesConfig {
    /// 对这些模型剥离图片块（图片不适合发给纯文本上游时用）。
    #[serde(default)]
    pub strip_models: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TimeoutConfig {
    /// 建立 TCP 连接的超时。
    #[serde(default = "default_connect_timeout")]
    pub connect_seconds: u64,
    /// 非流式请求等待响应头的超时。
    #[serde(default = "default_header_timeout")]
    pub header_seconds: u64,
    /// 流式请求等待首个响应头的超时；长思考的上游要留足余量。
    #[serde(default = "default_stream_header_timeout")]
    pub stream_header_seconds: u64,
}

impl Default for TimeoutConfig {
    fn default() -> Self {
        Self {
            connect_seconds: default_connect_timeout(),
            header_seconds: default_header_timeout(),
            stream_header_seconds: default_stream_header_timeout(),
        }
    }
}

fn default_connect_timeout() -> u64 {
    5
}

fn default_header_timeout() -> u64 {
    30
}

fn default_stream_header_timeout() -> u64 {
    120
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct LogConfig {
    /// 诊断日志（JSONL）路径。留空则用 `~/.codex-layer/codex-layer.log`。
    #[serde(default)]
    pub path: String,
}

/// 按模型改写上游：命中 `model` 的请求转发到本节自己的 `base_url`。
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct ModelRouteConfig {
    #[serde(default)]
    pub model: String,
    #[serde(default)]
    pub base_url: String,
    #[serde(default)]
    pub api_key: String,
    #[serde(default)]
    pub protocol: ProtocolFormat,
    #[serde(default)]
    pub no_auth: bool,
    /// 改写后的模型名；留空表示只换上游不改模型名。
    #[serde(default)]
    pub target_model: String,
}

impl Config {
    /// 配置文件路径：`--config` 已由 main 处理，这里看
    /// `CODEX_LAYER_CONFIG` -> `./codex-layer.toml` -> `~/.codex-layer/codex-layer.toml`。
    pub fn default_path() -> PathBuf {
        if let Some(path) = std::env::var_os("CODEX_LAYER_CONFIG") {
            let path = PathBuf::from(path);
            if !path.as_os_str().is_empty() {
                return path;
            }
        }
        let local = PathBuf::from("codex-layer.toml");
        if local.is_file() {
            return local;
        }
        paths::default_app_state_dir().join("codex-layer.toml")
    }

    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("无法读取配置文件 {}", path.display()))?;
        let config: Config = toml::from_str(&text)
            .with_context(|| format!("配置文件 {} 解析失败", path.display()))?;
        config.validate()?;
        Ok(config)
    }

    /// 配置期校验：把「跑起来才发现地址为空」提前到启动时。
    pub fn validate(&self) -> anyhow::Result<()> {
        if self.upstream.base_url.trim().is_empty() && self.model_route.is_empty() {
            anyhow::bail!("配置缺少上游地址：请填写 [upstream] base_url，或至少一条 [[model_route]]");
        }
        if !self.upstream.base_url.trim().is_empty() {
            reqwest::Url::parse(self.upstream.base_url.trim())
                .with_context(|| format!("upstream.base_url 不是合法 URL：{:?}", self.upstream.base_url))?;
        }
        if self.upstream.api_key.trim().is_empty() && !self.upstream.no_auth {
            anyhow::bail!("配置缺少上游密钥：请填写 [upstream] api_key，或设置 no_auth = true");
        }
        for route in &self.model_route {
            if route.model.trim().is_empty() {
                anyhow::bail!("[[model_route]] 缺少 model");
            }
            if route.base_url.trim().is_empty() {
                anyhow::bail!("[[model_route]] model={:?} 缺少 base_url", route.model);
            }
            if route.api_key.trim().is_empty() && !route.no_auth {
                anyhow::bail!(
                    "[[model_route]] model={:?} 缺少 api_key（或设置 no_auth = true）",
                    route.model
                );
            }
        }
        Ok(())
    }

    /// 转成转换层使用的内部载体。
    pub fn to_backend_settings(&self) -> anyhow::Result<BackendSettings> {
        let custom_headers: Vec<RelayHeaderKeyValue> = self
            .upstream
            .header
            .iter()
            .filter(|header| !header.key.trim().is_empty())
            .map(|header| RelayHeaderKeyValue {
                key: header.key.trim().to_string(),
                value: header.value.trim().to_string(),
            })
            .collect();
        crate::relay_headers::validate(&custom_headers)?;

        let mut route_table = Vec::new();
        let mut route_profiles = Vec::new();
        for (index, route) in self.model_route.iter().enumerate() {
            let target_id = format!("route-{index}");
            route_table.push(RelayModelRoute {
                model: route.model.trim().to_string(),
                target_relay_id: target_id.clone(),
                target_model: route.target_model.trim().to_string(),
            });
            route_profiles.push(RelayProfile {
                id: target_id.clone(),
                name: target_id,
                base_url: route.base_url.trim().to_string(),
                upstream_base_url: route.base_url.trim().to_string(),
                api_key: route.api_key.clone(),
                protocol: route.protocol.to_relay_protocol(),
                relay_mode: if route.no_auth { RelayMode::PureApi } else { RelayMode::MixedApi },
                no_auth: route.no_auth,
                model: route.target_model.trim().to_string(),
                ..RelayProfile::default()
            });
        }

        let source = RelayProfile {
            id: "upstream".to_string(),
            name: "upstream".to_string(),
            base_url: self.upstream.base_url.trim().to_string(),
            upstream_base_url: self.upstream.base_url.trim().to_string(),
            api_key: self.upstream.api_key.clone(),
            protocol: self.upstream.protocol.to_relay_protocol(),
            relay_mode: if self.upstream.no_auth { RelayMode::PureApi } else { RelayMode::MixedApi },
            no_auth: self.upstream.no_auth,
            test_model: self.upstream.model.trim().to_string(),
            model: self.upstream.model.trim().to_string(),
            user_agent: self.upstream.user_agent.trim().to_string(),
            model_routes: route_table,
            custom_headers,
            model_vlm: strip_models_json(&self.images.strip_models)?,
            standard_openai_protocol: self.stabilizer.standard_openai_protocol,
            stabilize_prompt_prefix: self.stabilizer.prompt_prefix,
            ..RelayProfile::default()
        };

        let mut relay_profiles = vec![source];
        relay_profiles.extend(route_profiles);
        Ok(BackendSettings {
            relay_profiles_enabled: true,
            relay_profiles,
            active_relay_id: "upstream".to_string(),
            ..BackendSettings::default()
        })
    }
}

/// 把 `[images] strip_models = ["a","b"]` 编成转换层认识的策略表。
/// 表名沿用 `model_vlm`（与内部 JSON 设置字段同名），值只有 `strip` 一档。
fn strip_models_json(models: &[String]) -> anyhow::Result<String> {
    let mut map = serde_json::Map::new();
    for model in models {
        let model = model.trim();
        if model.is_empty() {
            continue;
        }
        map.insert(model.to_string(), serde_json::json!("strip"));
    }
    if map.is_empty() {
        return Ok(String::new());
    }
    Ok(serde_json::to_string(&map)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn minimal_toml_produces_stabilized_profile() {
        let config: Config = toml::from_str(
            r#"
[upstream]
base_url = "http://127.0.0.1:11234/v1"
api_key = "sk-test"
protocol = "chat_completions"

[stabilizer]
prompt_prefix = true
"#,
        )
        .unwrap();
        config.validate().unwrap();
        let settings = config.to_backend_settings().unwrap();
        let active = settings.active_relay_profile();
        assert_eq!(active.protocol, RelayProtocol::ChatCompletions);
        assert!(active.stabilize_prompt_prefix);
        assert_eq!(active.base_url, "http://127.0.0.1:11234/v1");
    }

    #[test]
    fn unknown_field_is_rejected() {
        let error = toml::from_str::<Config>(
            r#"
[upstream]
base_url = "http://127.0.0.1:9/v1"
api_key = "sk-test"
protcol = "responses"
"#,
        )
        .unwrap_err();
        assert!(error.to_string().contains("protcol"), "{error}");
    }

    #[test]
    fn missing_upstream_is_rejected_at_validate() {
        let config: Config = toml::from_str("[stabilizer]").unwrap();
        let error = config.validate().unwrap_err();
        assert!(error.to_string().contains("base_url"), "{error}");
    }

    #[test]
    fn model_routes_become_target_profiles() {
        let config: Config = toml::from_str(
            r#"
[upstream]
base_url = "http://127.0.0.1:9/v1"
api_key = "sk-main"

[[model_route]]
model = "gpt-5"
base_url = "http://127.0.0.1:8080/v1"
api_key = "sk-vision"
protocol = "chat_completions"
target_model = "qwen3-vl"
"#,
        )
        .unwrap();
        config.validate().unwrap();
        let settings = config.to_backend_settings().unwrap();
        let active = settings.active_relay_profile();
        assert_eq!(active.model_routes.len(), 1);
        let target = settings
            .relay_profiles
            .iter()
            .find(|profile| profile.id == active.model_routes[0].target_relay_id)
            .unwrap();
        assert_eq!(target.protocol, RelayProtocol::ChatCompletions);
        assert_eq!(target.model, "qwen3-vl");
    }

    #[test]
    fn strip_models_become_policy_table() {
        let config: Config = toml::from_str(
            r#"
[upstream]
base_url = "http://127.0.0.1:9/v1"
api_key = "sk-test"

[images]
strip_models = ["text-only-model"]
"#,
        )
        .unwrap();
        let settings = config.to_backend_settings().unwrap();
        assert!(
            settings
                .active_relay_profile()
                .model_vlm
                .contains("text-only-model")
        );
    }

    #[test]
    fn timeout_defaults_match_upstream_constants() {
        let config: Config = toml::from_str(
            r#"
[upstream]
base_url = "http://127.0.0.1:9/v1"
api_key = "sk-test"
"#,
        )
        .unwrap();
        assert_eq!(config.timeouts.connect_seconds, 5);
        assert_eq!(config.timeouts.header_seconds, 30);
        assert_eq!(config.timeouts.stream_header_seconds, 120);
    }

    /// `[stability]` 缺省时：端点关、窗口 8MB、snip 不外传（隐私默认保守）。
    #[test]
    fn stability_defaults_disabled_with_8mb_window() {
        let config: Config = toml::from_str(
            r#"
        [upstream]
        base_url = "http://127.0.0.1:9/v1"
        api_key = "sk-test"
        "#,
        )
        .unwrap();
        assert!(!config.stability.enabled);
        assert_eq!(config.stability.window_bytes, 8 * 1024 * 1024);
        assert!(!config.stability.show_prompt_snippets);
    }

    /// `[stability]` 段可显式开启；段内未知字段同样启动失败。
    #[test]
    fn stability_section_parses_and_rejects_unknown_fields() {
        let config: Config = toml::from_str(
            r#"
        [upstream]
        base_url = "http://127.0.0.1:9/v1"
        api_key = "sk-test"

        [stability]
        enabled = true
        window_bytes = 4096
        show_prompt_snippets = true
        "#,
        )
        .unwrap();
        assert!(config.stability.enabled);
        assert_eq!(config.stability.window_bytes, 4096);
        assert!(config.stability.show_prompt_snippets);

        let error = toml::from_str::<Config>(
            r#"
        [upstream]
        base_url = "http://127.0.0.1:9/v1"
        api_key = "sk-test"

        [stability]
        enabld = true
        "#,
        )
        .unwrap_err();
        assert!(error.to_string().contains("enabld"), "{error}");
    }
}
