// 稳定层的全部诊断事件都走这里：JSONL 追加 + 超限裁剪。
// 默认落点 `~/.codex-layer/codex-layer.log`，可由配置里的诊断日志路径覆盖。
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;
use serde_json::{Value, json};

static TEST_LOG_PATH: OnceLock<Mutex<Option<PathBuf>>> = OnceLock::new();

/// 测试专用进程锁：诊断日志落点覆盖是全局静态，会写日志的测试必须
/// 持它独占——否则两个测试互相覆盖全局路径，写入会落进对方的文件。
#[cfg(test)]
pub(crate) static DIAG_LOG_TEST_LOCK: Mutex<()> = Mutex::new(());

/// 事件名唯一定义处：所有写入诊断 JSONL 的事件名都必须先在这里登记，
/// 生产代码不得再散落拼字面量。已登记的名字是已发布数据契约的一部分：
/// #19 聚合与 #20 界面都按它解析，改名等于毁掉历史可比性，只允许新增。
pub mod events {
    /// codex-layer 自身：进程生命周期与请求管道事件（bin 装配层写）。
    pub mod layer {
        pub const STARTED: &str = "codex_layer.started";
        pub const DEPRECATED_ENV: &str = "codex_layer.deprecated_env";
        pub const STABILITY_BAD_REQUEST: &str = "codex_layer.stability_bad_request";
        pub const STABILITY_READ_FAILED: &str = "codex_layer.stability_read_failed";
        pub const RESPONSES_UPGRADE_REQUIRED: &str = "codex_layer.responses_upgrade_required";
        pub const PROXY_FAILED: &str = "codex_layer.proxy_failed";
        pub const CONVERSION_REJECTED: &str = "codex_layer.conversion_rejected";
        pub const UPSTREAM_RESPONSE: &str = "codex_layer.upstream_response";
        pub const UPSTREAM_ERROR: &str = "codex_layer.upstream_error";
        pub const STREAM_OPEN: &str = "codex_layer.stream_open";
        pub const STREAM_COMPLETED: &str = "codex_layer.stream_completed";
        pub const STREAM_FAILED: &str = "codex_layer.stream_failed";
        /// 本族登记表：消费侧对齐测试与验收判据的完整性检查用。
        pub const ALL: &[&str] = &[
            STARTED,
            DEPRECATED_ENV,
            STABILITY_BAD_REQUEST,
            STABILITY_READ_FAILED,
            RESPONSES_UPGRADE_REQUIRED,
            PROXY_FAILED,
            CONVERSION_REJECTED,
            UPSTREAM_RESPONSE,
            UPSTREAM_ERROR,
            STREAM_OPEN,
            STREAM_COMPLETED,
            STREAM_FAILED,
        ];
    }

    /// 协议转换层事件（lib 转换侧写）。
    pub mod proxy {
        pub const UPSTREAM_REQUEST: &str = "protocol_proxy.upstream_request";
        pub const UPSTREAM_REQUEST_FAILED: &str = "protocol_proxy.upstream_request_failed";
        pub const UPSTREAM_RESPONSE: &str = "protocol_proxy.upstream_response";
        pub const MODELS_REQUEST: &str = "protocol_proxy.models_request";
        pub const AUDIO_TRANSCRIPTIONS_REQUEST: &str = "protocol_proxy.audio_transcriptions_request";
        pub const IMAGE_REQUEST: &str = "protocol_proxy.image_request";
        pub const STABILIZER_COMPACTION_APPENDED: &str =
            "protocol_proxy.prefix_stabilizer_compaction_appended";
        pub const STABILIZER_PREFIX_BASELINE: &str =
            "protocol_proxy.prefix_stabilizer_prefix_baseline";
        pub const STABILIZER_PREFIX_DIVERGENCE: &str =
            "protocol_proxy.prefix_stabilizer_prefix_divergence";
        pub const STABILIZER_UNPAIRED_NOTICE_STRIPPED: &str =
            "protocol_proxy.prefix_stabilizer_unpaired_notice_stripped";
        pub const STABILIZER_DEDUP_LAST_FOLDED: &str =
            "protocol_proxy.prefix_stabilizer_dedup_last_folded";
        /// 本族登记表。
        pub const ALL: &[&str] = &[
            UPSTREAM_REQUEST,
            UPSTREAM_REQUEST_FAILED,
            UPSTREAM_RESPONSE,
            MODELS_REQUEST,
            AUDIO_TRANSCRIPTIONS_REQUEST,
            IMAGE_REQUEST,
            STABILIZER_COMPACTION_APPENDED,
            STABILIZER_PREFIX_BASELINE,
            STABILIZER_PREFIX_DIVERGENCE,
            STABILIZER_UNPAIRED_NOTICE_STRIPPED,
            STABILIZER_DEDUP_LAST_FOLDED,
        ];
    }

    /// sink 自身序列化失败的兜底事件。
    pub const SERIALIZATION_FAILED: &str = "diagnostic_log.serialization_failed";
}

/// 诊断事件的统一写入口：bin 与 lib 共用这一条路径，事件名取自
/// [`events`] 登记常量。失败口径在本处一处约定：诊断日志是尽力而为的
/// 旁路观测，写入失败绝不阻断或失败请求，也不重试、不降级为计数以外的
/// 动作；生产调用方统一显式 `let _ = log_event(...)` 表明知情忽略，
/// 需要断言写入结果的是测试，直接用底层 `append_diagnostic_log`。
pub fn log_event(event: &'static str, detail: impl Serialize) -> std::io::Result<()> {
    append_diagnostic_log(event, detail)
}

const MAX_DIAGNOSTIC_LOG_BYTES: u64 = 50 * 1024 * 1024;
const COMPACTED_DIAGNOSTIC_LOG_BYTES: u64 = 5 * 1024 * 1024;

#[derive(Debug, Clone, Serialize)]
struct DiagnosticRecord {
    timestamp_ms: u64,
    pid: u32,
    event: String,
    detail: Value,
}

pub fn append_diagnostic_log(event: &str, detail: impl Serialize) -> std::io::Result<()> {
    let path = diagnostic_log_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let detail = serde_json::to_value(detail).unwrap_or_else(|error| {
        json!({
            "serialization_error": error.to_string()
        })
    });
    let record = DiagnosticRecord {
        timestamp_ms: now_ms(),
        pid: std::process::id(),
        event: event.to_string(),
        detail,
    };
    let line = serde_json::to_string(&record).unwrap_or_else(|error| {
        json!({
            "timestamp_ms": now_ms(),
            "pid": std::process::id(),
            "event": events::SERIALIZATION_FAILED,
            "detail": {
                "message": error.to_string()
            }
        })
        .to_string()
    });

    compact_diagnostic_log_if_needed(&path)?;
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    writeln!(file, "{line}")?;
    Ok(())
}

pub fn clear_diagnostic_log() -> std::io::Result<()> {
    let path = diagnostic_log_path();
    clear_diagnostic_log_path(&path)
}

fn clear_diagnostic_log_path(path: &Path) -> std::io::Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

pub fn diagnostic_log_path() -> PathBuf {
    if let Some(lock) = TEST_LOG_PATH.get() {
        if let Ok(guard) = lock.lock() {
            if let Some(path) = &*guard {
                return path.clone();
            }
        }
    }
    crate::paths::default_diagnostic_log_path()
}

/// 覆盖诊断日志落点：`main` 按配置 `[log] path` 调一次；测试也用它把日志写进临时目录。
/// 传 `None` 恢复默认落点。
pub fn set_diagnostic_log_path(path: Option<PathBuf>) {
    let lock = TEST_LOG_PATH.get_or_init(|| Mutex::new(None));
    *lock.lock().expect("test log path lock poisoned") = path;
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn compact_diagnostic_log_if_needed(path: &PathBuf) -> std::io::Result<()> {
    compact_diagnostic_log(
        path,
        MAX_DIAGNOSTIC_LOG_BYTES,
        COMPACTED_DIAGNOSTIC_LOG_BYTES,
    )
}

fn compact_diagnostic_log(
    path: &PathBuf,
    max_bytes: u64,
    compacted_bytes: u64,
) -> std::io::Result<()> {
    let len = match std::fs::metadata(path) {
        Ok(metadata) => metadata.len(),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    if len <= max_bytes {
        return Ok(());
    }

    let keep = compacted_bytes.min(len);
    let mut file = std::fs::File::open(path)?;
    file.seek(SeekFrom::Start(len - keep))?;
    let mut tail = Vec::with_capacity(keep as usize);
    file.read_to_end(&mut tail)?;
    drop(file);
    if len > keep {
        if let Some(pos) = tail.iter().position(|byte| *byte == b'\n') {
            tail.drain(..=pos);
        }
    }

    crate::settings::atomic_write(path, &tail).map_err(std::io::Error::other)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compact_diagnostic_log_keeps_tail_and_drops_partial_first_line() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("codex-layer.log");
        std::fs::write(&path, "line-1\nline-2\nline-3\nline-4\n").unwrap();

        compact_diagnostic_log(&path, 12, 16).unwrap();

        let contents = std::fs::read_to_string(path).unwrap();
        assert_eq!(contents, "line-3\nline-4\n");
    }

    #[test]
    fn clear_diagnostic_log_ignores_missing_file() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("missing.log");

        clear_diagnostic_log_path(&path).unwrap();
    }

    /// 登记表完整性：名字非空、前缀归族、跨族不重复——事件名是已发布
    /// 数据契约，漂移一条就污染 #19/#20 的一类统计。
    #[test]
    fn event_table_is_complete_unique_and_prefixed() {
        for name in events::layer::ALL {
            assert!(name.starts_with("codex_layer."), "归族前缀错误：{name}");
        }
        for name in events::proxy::ALL {
            assert!(name.starts_with("protocol_proxy."), "归族前缀错误：{name}");
        }
        assert!(events::SERIALIZATION_FAILED.starts_with("diagnostic_log."));
        let mut unique: std::collections::HashSet<&str> = std::collections::HashSet::new();
        for name in events::layer::ALL
            .iter()
            .chain(events::proxy::ALL.iter())
            .chain(std::iter::once(&events::SERIALIZATION_FAILED))
        {
            assert!(unique.insert(*name), "登记表里出现重复事件名：{name}");
        }
        assert_eq!(
            unique.len(),
            events::layer::ALL.len() + events::proxy::ALL.len() + 1,
            "登记表计数与成员不符"
        );
        for name in events::layer::ALL
            .iter()
            .chain(events::proxy::ALL.iter())
        {
            assert!(!name.contains(' ') && !name.is_empty(), "事件名不得含空白：{name}");
        }
    }

    /// 统一入口的落盘形态钉死：字段名与行内键序逐字不变
    /// （timestamp_ms/pid/event/detail，由结构体声明顺决定）。
    /// 事件名用字面量钉而不用常量——常量漂了这条测试必须红。
    #[test]
    fn log_event_writes_pin_record_shape() {
        let _diag_guard = DIAG_LOG_TEST_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("codex-layer.log");
        crate::diagnostic_log::set_diagnostic_log_path(Some(path.clone()));
        let appended = log_event(events::layer::STARTED, json!({ "probe": 1 }));
        crate::diagnostic_log::set_diagnostic_log_path(None);
        appended.unwrap();
        let contents = std::fs::read_to_string(&path).unwrap();
        let line = contents
            .lines()
            .find(|line| line.contains("\"event\":\"codex_layer.started\""))
            .expect("统一入口的事件必须落盘");
        assert!(line.starts_with("{\"timestamp_ms\":"), "键序漂移：{line}");
        assert!(line.contains(",\"pid\":"), "字段名漂移：{line}");
        assert!(line.contains(",\"event\":\"codex_layer.started\",\"detail\":{\"probe\":1}}"), "字段名漂移：{line}");
        let record: Value = serde_json::from_str(line).unwrap();
        assert_eq!(record["pid"], json!(std::process::id()));
        assert!(record["timestamp_ms"].is_number());
    }
}
