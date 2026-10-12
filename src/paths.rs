//! 状态目录与文件路径解析。
//!
//! 极小子集，来自 CodexPlusPlus PR #2449 的
//! `crates/codex-plus-core/src/paths.rs`（commit f9be7cf）：本项目只需要
//! 「设置文件在哪」与「诊断日志在哪」两条路径，以及测试用的路径覆盖钩子。
//!
//! 状态目录默认 `~/.codex-layer`，可用环境变量 `CODEX_LAYER_HOME` 整体改写；
//! 两条路径各自还有更细的覆盖变量，便于临时排障而不改配置。

use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

pub const SETTINGS_FILE: &str = "settings.json";
pub const DIAGNOSTIC_LOG_FILE: &str = "codex-layer.log";

/// 状态目录：`$CODEX_LAYER_HOME`，否则 `~/.codex-layer`。
pub fn default_app_state_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("CODEX_LAYER_HOME") {
        let dir = PathBuf::from(dir);
        if !dir.as_os_str().is_empty() {
            return dir;
        }
    }
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    home.join(".codex-layer")
}

pub fn default_settings_path() -> PathBuf {
    if let Some(path) = settings_path_override() {
        return path;
    }
    if let Some(path) = std::env::var_os("CODEX_LAYER_SETTINGS_PATH") {
        let path = PathBuf::from(path);
        if !path.as_os_str().is_empty() {
            return path;
        }
    }
    default_app_state_dir().join(SETTINGS_FILE)
}

pub fn default_diagnostic_log_path() -> PathBuf {
    if let Some(path) = std::env::var_os("CODEX_LAYER_LOG_PATH") {
        let path = PathBuf::from(path);
        if !path.as_os_str().is_empty() {
            return path;
        }
    }
    default_app_state_dir().join(DIAGNOSTIC_LOG_FILE)
}

/// 测试/进程内路径覆盖：与 CodexPlusPlus 同名同语义，移植过来的测试直接调用它。
fn settings_path_override() -> Option<PathBuf> {
    SETTINGS_PATH_OVERRIDE
        .get_or_init(|| Mutex::new(None))
        .lock()
        .ok()
        .and_then(|path| path.clone())
}

static SETTINGS_PATH_OVERRIDE: OnceLock<Mutex<Option<PathBuf>>> = OnceLock::new();

#[doc(hidden)]
pub fn set_settings_path_for_tests(path: Option<PathBuf>) -> Option<PathBuf> {
    let mut lock = SETTINGS_PATH_OVERRIDE
        .get_or_init(|| Mutex::new(None))
        .lock()
        .expect("settings path override lock poisoned");
    std::mem::replace(&mut *lock, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_paths_use_codex_layer_state_dir() {
        let previous = set_settings_path_for_tests(None);
        let path = default_settings_path();
        assert!(path.ends_with(SETTINGS_FILE));
        assert!(default_diagnostic_log_path().ends_with(DIAGNOSTIC_LOG_FILE));
        set_settings_path_for_tests(previous);
    }

    #[test]
    fn settings_path_override_wins() {
        let previous = set_settings_path_for_tests(None);
        let temp = tempfile::tempdir().unwrap();
        let target = temp.path().join("settings.json");
        let _ = set_settings_path_for_tests(Some(target.clone()));
        assert_eq!(default_settings_path(), target);
        set_settings_path_for_tests(previous);
    }
}
