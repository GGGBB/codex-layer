//! 内部数据模型。
//!
//! 为什么还要留一份同名 JSON 模型：转换层（`protocol_proxy`）与既有测试都以
//! `RelayProfile` / `BackendSettings` 为输入，字段名保持兼容后，用户可以从旧部署
//! 的 `settings.json` 直接迁移供应商配置，测试资产也能原样运行。
//! 本项目的常规配置入口是 TOML（见 `config.rs`），这里只是内部载体与兼容读取路径。
//!
//! 删掉的聚合形态内容：官方混充 Key（officialMixApiKey / configContents /
//! authContents）、sub2api、stepwise、聚合轮换（aggregateRelayProfiles）、
//! 渠道限流与冷却、codex 客户端注入开关、皮肤、会话分享等。

use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::Context;
use serde::{Deserialize, Serialize};

pub fn default_relay_base_url() -> String {
    String::new()
}

pub fn default_active_relay_id() -> String {
    "default".to_string()
}

pub fn default_relay_profiles() -> Vec<RelayProfile> {
    vec![RelayProfile::default()]
}

fn default_true() -> bool {
    true
}

fn is_false(value: &bool) -> bool {
    !*value
}

/// `apiKey` 允许写成 `null` 或缺省：与设置文件的既有解析行为保持一致，
/// 避免前端删掉输入框后解析失败。
fn deserialize_profile_api_key<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Ok(Option::<String>::deserialize(deserializer)?.unwrap_or_default())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum RelayProtocol {
    #[default]
    Responses,
    ChatCompletions,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum RelayMode {
    Official,
    #[default]
    MixedApi,
    PureApi,
    /// 仅为兼容既有设置文件保留；本项目不做聚合轮换。
    Aggregate,
}

/// 按模型改写上游：源请求的 model 命中后，转发到 `target_relay_id` 指向的
/// 上游，并可顺带改写模型名。这是本项目「多上游」的唯一形态。
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RelayModelRoute {
    pub model: String,
    #[serde(rename = "targetRelayId", default)]
    pub target_relay_id: String,
    #[serde(
        rename = "targetModel",
        default,
        skip_serializing_if = "String::is_empty",
    )]
    pub target_model: String,
}

/// 上游自定义请求头（有序列表，保住用户写的顺序）。
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RelayHeaderKeyValue {
    #[serde(default)]
    pub key: String,
    #[serde(default)]
    pub value: String,
}

/// 一个上游（在设置文件里叫「供应商」）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct RelayProfile {
    pub id: String,
    pub name: String,
    /// 该上游的默认模型；请求未带 model 时的回退值。
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub model: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub base_url: String,
    #[serde(
        rename = "upstreamBaseUrl",
        default,
        skip_serializing_if = "String::is_empty",
    )]
    pub upstream_base_url: String,
    /// 上游密钥。序列化时永远不写出，避免被误当普通配置复制走。
    #[serde(
        rename = "apiKey",
        default,
        skip_serializing,
        deserialize_with = "deserialize_profile_api_key"
    )]
    pub api_key: String,
    pub protocol: RelayProtocol,
    #[serde(rename = "relayMode", default)]
    pub relay_mode: RelayMode,
    /// 上游不需要认证：转发时不注入任何 Authorization。
    #[serde(rename = "noAuth", default)]
    pub no_auth: bool,
    /// 连通性测试用的模型名。
    #[serde(rename = "testModel", default, skip_serializing_if = "String::is_empty")]
    pub test_model: String,
    #[serde(rename = "userAgent", default, skip_serializing_if = "String::is_empty")]
    pub user_agent: String,
    #[serde(rename = "modelRoutes", default, skip_serializing_if = "Vec::is_empty")]
    pub model_routes: Vec<RelayModelRoute>,
    #[serde(rename = "customHeaders", default, skip_serializing_if = "Vec::is_empty")]
    pub custom_headers: Vec<RelayHeaderKeyValue>,
    /// 逐模型图片处理表（JSON map：模型名 -> `send-as-is` / `strip`）。
    #[serde(
        rename = "modelVlm",
        default,
        skip_serializing_if = "String::is_empty",
    )]
    pub model_vlm: String,
    #[serde(rename = "modelWindows", default, skip_serializing_if = "String::is_empty")]
    pub model_windows: String,
    #[serde(rename = "contextWindow", default, skip_serializing_if = "String::is_empty")]
    pub context_window: String,
    #[serde(
        rename = "autoCompactLimit",
        default,
        skip_serializing_if = "String::is_empty",
    )]
    pub auto_compact_limit: String,
    /// 严格 OpenAI 协议模式：剥离各家私有 reasoning 方言。
    #[serde(
        rename = "standardOpenaiProtocol",
        default,
        skip_serializing_if = "is_false",
    )]
    pub standard_openai_protocol: bool,
    /// 「prompt 前缀稳定层」开关：剥离 image_resize_notice、规范化 system
    /// 头部注入块顺序、钉住末轮 user 标记。默认关 = 转发行为逐字节不变。
    #[serde(
        rename = "stabilizePromptPrefix",
        default,
        skip_serializing_if = "is_false",
    )]
    pub stabilize_prompt_prefix: bool,
}

impl Default for RelayProfile {
    fn default() -> Self {
        Self {
            id: "default".to_string(),
            name: "默认中转".to_string(),
            model: String::new(),
            base_url: default_relay_base_url(),
            upstream_base_url: String::new(),
            api_key: String::new(),
            protocol: RelayProtocol::Responses,
            relay_mode: RelayMode::Official,
            no_auth: false,
            test_model: String::new(),
            user_agent: String::new(),
            model_routes: Vec::new(),
            custom_headers: Vec::new(),
            model_vlm: String::new(),
            model_windows: String::new(),
            context_window: String::new(),
            auto_compact_limit: String::new(),
            standard_openai_protocol: false,
            stabilize_prompt_prefix: false,
        }
    }
}

impl RelayProfile {
    /// 免认证上游：只有显声明的 pureApi + noAuth 才算，避免误配把密钥裸奔。
    pub fn uses_no_auth(&self) -> bool {
        self.relay_mode == RelayMode::PureApi && self.no_auth
    }

    pub fn has_model_routes(&self) -> bool {
        self.model_routes
            .iter()
            .any(|route| !route.model.trim().is_empty() && !route.target_relay_id.trim().is_empty())
    }
}

/// 设置文件根结构。本项目只关心上游表与激活项。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct BackendSettings {
    #[serde(rename = "relayProfilesEnabled", default = "default_true")]
    pub relay_profiles_enabled: bool,
    #[serde(rename = "relayProfiles", default = "default_relay_profiles")]
    pub relay_profiles: Vec<RelayProfile>,
    #[serde(rename = "activeRelayId", default = "default_active_relay_id")]
    pub active_relay_id: String,
    /// 旧版单值写法（未用 relayProfiles 时）：仍然兼容。
    #[serde(rename = "relayBaseUrl", default = "default_relay_base_url")]
    pub relay_base_url: String,
    #[serde(rename = "relayApiKey", default)]
    pub relay_api_key: String,
}

impl Default for BackendSettings {
    fn default() -> Self {
        Self {
            relay_profiles_enabled: true,
            relay_profiles: default_relay_profiles(),
            active_relay_id: default_active_relay_id(),
            relay_base_url: default_relay_base_url(),
            relay_api_key: String::new(),
        }
    }
}

impl BackendSettings {
    /// 本次请求使用的上游：按 `activeRelayId` 命中，未命中退回首条；
    /// 完全没有上游时，用旧版单值字段拼一个默认上游。
    pub fn active_relay_profile(&self) -> RelayProfile {
        if let Some(profile) = self
            .relay_profiles
            .iter()
            .find(|profile| profile.id == self.active_relay_id)
        {
            return profile.clone();
        }
        if let Some(profile) = self.relay_profiles.first() {
            return profile.clone();
        }
        RelayProfile {
            id: default_active_relay_id(),
            name: "默认中转".to_string(),
            base_url: if self.relay_base_url.is_empty() {
                default_relay_base_url()
            } else {
                self.relay_base_url.clone()
            },
            upstream_base_url: self.relay_base_url.clone(),
            api_key: self.relay_api_key.clone(),
            protocol: RelayProtocol::Responses,
            relay_mode: RelayMode::MixedApi,
            ..RelayProfile::default()
        }
    }
}

/// 设置文件读写（只读为主；写入仅供测试与外部工具使用）。
pub struct SettingsStore {
    path: PathBuf,
}

impl Default for SettingsStore {
    fn default() -> Self {
        Self::new(crate::paths::default_settings_path())
    }
}

impl SettingsStore {
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// 读设置；文件不存在时退回默认值。解析失败必须报错，
    /// 不能把「读坏了」当成「没配置」，否则调用方可能把空配置写回磁盘。
    pub fn load(&self) -> anyhow::Result<BackendSettings> {
        let contents = match std::fs::read_to_string(&self.path) {
            Ok(contents) => contents,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(BackendSettings::default());
            }
            Err(error) => {
                return Err(error).with_context(|| format!("无法读取设置 {}", self.path.display()));
            }
        };
        let parsed = serde_json::from_str(&contents).with_context(|| {
            format!(
                "设置 {} 解析失败，拒绝退回默认设置以免覆盖用户配置",
                self.path.display()
            )
        })?;
        Ok(parsed)
    }

    pub fn save(&self, settings: &BackendSettings) -> anyhow::Result<()> {
        let bytes = serde_json::to_vec_pretty(settings)?;
        atomic_write(&self.path, &bytes)
    }
}

/// 原子写入：先写临时文件再改名，避免写一半被读到。
pub fn atomic_write(path: &Path, bytes: &[u8]) -> anyhow::Result<()> {
    atomic_write_with(path, |file| file.write_all(bytes))
}

pub fn atomic_write_with(
    path: &Path,
    write_contents: impl FnOnce(&mut File) -> std::io::Result<()>,
) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create directory {}", parent.display()))?;
    }
    // 保留原文件权限：临时文件默认权限不一定和它一致。
    let existing_permissions = match std::fs::metadata(path) {
        Ok(metadata) => Some(metadata.permissions()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => {
            return Err(error)
                .with_context(|| format!("failed to read permissions for {}", path.display()));
        }
    };
    let temp_path = temp_path_for(path);
    let write_result = (|| -> std::io::Result<()> {
        let mut temp_file = File::create(&temp_path)?;
        write_contents(&mut temp_file)?;
        temp_file.flush()?;
        if let Some(permissions) = existing_permissions {
            temp_file.set_permissions(permissions)?;
        }
        Ok(())
    })();
    if let Err(error) = write_result {
        let _ = std::fs::remove_file(&temp_path);
        return Err(error).with_context(|| format!("failed to write {}", temp_path.display()));
    }
    std::fs::rename(&temp_path, path)
        .with_context(|| format!("failed to replace {}", path.display()))?;
    Ok(())
}

fn temp_path_for(path: &Path) -> PathBuf {
    let name = path
        .file_name()
        .map(|value| value.to_string_lossy().to_string())
        .unwrap_or_else(|| "codex-layer.tmp".to_string());
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    parent.join(format!(".{name}.tmp"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_profile_keeps_default_id_and_disabled_switches() {
        // 默认 profile 的 id 固定为 default，
        // 判定「设置退化成单条默认上游」依赖它。
        let profile = RelayProfile::default();
        assert_eq!(profile.id, "default");
        assert!(!profile.stabilize_prompt_prefix);
        assert!(!profile.uses_no_auth());
    }

    /// 钉住 camelCase 形态的设置文件仍可解析：这是旧部署的向后兼容入口，
    /// 不是历史残留。字段名契约一旦改动，旧部署的供应商配置会直接读不出来。
    #[test]
    fn camel_case_settings_json_still_parses() {
        let settings: BackendSettings = serde_json::from_value(serde_json::json!({
            "relayProfiles": [{
                "id": "chat",
                "name": "Chat",
                "baseUrl": "http://127.0.0.1:11234/v1",
                "upstreamBaseUrl": "http://127.0.0.1:11234/v1",
                "apiKey": "sk-test",
                "protocol": "chatCompletions",
                "relayMode": "mixedApi",
                "stabilizePromptPrefix": true
            }],
            "activeRelayId": "chat"
        }))
        .unwrap();
        let active = settings.active_relay_profile();
        assert_eq!(active.id, "chat");
        assert_eq!(active.protocol, RelayProtocol::ChatCompletions);
        assert!(active.stabilize_prompt_prefix);
        assert_eq!(active.api_key, "sk-test");
    }

    #[test]
    fn api_key_is_never_serialized() {
        let settings = BackendSettings {
            relay_profiles: vec![RelayProfile {
                id: "chat".to_string(),
                api_key: "sk-secret".to_string(),
                ..RelayProfile::default()
            }],
            ..BackendSettings::default()
        };
        let text = serde_json::to_string(&settings).unwrap();
        assert!(!text.contains("sk-secret"));
    }

    #[test]
    fn missing_profile_falls_back_to_legacy_single_value() {
        let settings = BackendSettings {
            relay_profiles: Vec::new(),
            relay_base_url: "http://127.0.0.1:9/v1".to_string(),
            relay_api_key: "sk-legacy".to_string(),
            ..BackendSettings::default()
        };
        let active = settings.active_relay_profile();
        assert_eq!(active.base_url, "http://127.0.0.1:9/v1");
        assert_eq!(active.api_key, "sk-legacy");
    }

    #[test]
    fn atomic_write_replaces_file() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("settings.json");
        atomic_write(&path, b"{\"a\":1}").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "{\"a\":1}");
    }
}
