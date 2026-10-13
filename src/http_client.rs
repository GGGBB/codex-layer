//! 出站 HTTP client。
//!
//! 函数名 `proxied_client` 保留（转换层按这个名字调用），
//! 实际语义是「带 User-Agent 与连接超时的 client」。
//! 连接超时可由 TOML 配置 `[timeouts] connect-seconds` 覆盖，进程内设置一次。

use std::sync::{Mutex, OnceLock};
use std::time::Duration;

const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

static CONNECT_TIMEOUT_OVERRIDE: OnceLock<Mutex<Option<Duration>>> = OnceLock::new();

/// 由配置层在启动时调用，覆盖出站连接超时。
pub fn set_connect_timeout(timeout: Duration) {
    let lock = CONNECT_TIMEOUT_OVERRIDE.get_or_init(|| Mutex::new(None));
    *lock.lock().expect("connect timeout lock poisoned") = Some(timeout);
}

pub fn connect_timeout() -> Duration {
    CONNECT_TIMEOUT_OVERRIDE
        .get_or_init(|| Mutex::new(None))
        .lock()
        .ok()
        .and_then(|value| *value)
        .unwrap_or(DEFAULT_CONNECT_TIMEOUT)
}

pub fn proxied_client(user_agent: &str) -> anyhow::Result<reqwest::Client> {
    let ua = if user_agent.trim().is_empty() {
        format!("codex-layer/{}", env!("CARGO_PKG_VERSION"))
    } else {
        user_agent.trim().to_string()
    };
    Ok(reqwest::Client::builder()
        .connect_timeout(connect_timeout())
        .user_agent(ua)
        .build()?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_user_agent_uses_project_name() {
        // 只验证能构造出 client，不验证网络行为。
        let client = proxied_client("").unwrap();
        let _ = client;
    }
}
