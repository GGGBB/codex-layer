// 稳定层的全部诊断事件都走这里：JSONL 追加 + 超限裁剪。
// 默认落点 `~/.codex-layer/codex-layer.log`，可由配置里的诊断日志路径覆盖。
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
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

    /// 历史无前缀事件名族：名字是已发布契约，原样保留；新事件禁止
    /// 再往这一族加（必须带归族前缀，见登记表完整性单测）。
    pub mod misc {
        pub const INLINE_IMAGE_DATA_URL_LEAK: &str = "inline_image_data_url_leak";
        pub const ALL: &[&str] = &[INLINE_IMAGE_DATA_URL_LEAK];
    }

    /// sink 自身序列化失败的兜底事件。
    pub const SERIALIZATION_FAILED: &str = "diagnostic_log.serialization_failed";
}

/// 诊断事件的统一写入口：bin 与 lib 共用这一条路径，事件名取自
/// [`events`] 登记常量。
///
/// ## 失败口径（全仓库唯一约定处，调用方不得自行处置错误）
/// 诊断日志是**尽力而为的旁路观测**：写入失败绝不阻断请求、绝不改变响应、
/// 绝不重试，也不向上抛错。错误在本函数内部吞掉，只累加进
/// [`diagnostic_log_write_failures`] 计数——失败不至于静默无痕，但也不越权
/// 回到请求路径。所以本入口**不返回 Result**：调用方在类型上就无从忽略错误，
/// 也不必再写 `let _ =`，「能否忽略写入错误」在本仓库只有一个答案。
///
/// 需要断言写入结果的是测试与自检，直接用底层 [`append_diagnostic_log`]；
/// 生产路径不得绕过本入口直调底层函数。
pub fn log_event(event: &'static str, detail: impl Serialize) {
    if append_diagnostic_log(event, detail).is_err() {
        DIAG_WRITE_FAILURES.fetch_add(1, Ordering::Relaxed);
    }
}

static DIAG_WRITE_FAILURES: AtomicU64 = AtomicU64::new(0);

/// 诊断写入失败累计次数（进程内）。见 [`log_event`] 的失败口径：这是写入错误
/// 唯一的降级去处，用于「日志静默无新增」时判断是不是磁盘或权限出了问题。
pub fn diagnostic_log_write_failures() -> u64 {
    DIAG_WRITE_FAILURES.load(Ordering::Relaxed)
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
        // misc 族豁免归族前缀约束（历史无前缀名是已发布契约），但只出不进。
        for name in events::misc::ALL {
            assert!(!name.contains('.'), "misc 族条目不得带前缀点：{name}");
        }
        assert!(events::SERIALIZATION_FAILED.starts_with("diagnostic_log."));
        let mut unique: std::collections::HashSet<&str> = std::collections::HashSet::new();
        for name in events::layer::ALL
            .iter()
            .chain(events::proxy::ALL.iter())
            .chain(events::misc::ALL.iter())
            .chain(std::iter::once(&events::SERIALIZATION_FAILED))
        {
            assert!(unique.insert(*name), "登记表里出现重复事件名：{name}");
        }
        assert_eq!(
            unique.len(),
            events::layer::ALL.len()
                + events::proxy::ALL.len()
                + events::misc::ALL.len()
                + 1,
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
        let failures_before = diagnostic_log_write_failures();
        log_event(events::layer::STARTED, json!({ "probe": 1 }));
        crate::diagnostic_log::set_diagnostic_log_path(None);
        // 统一入口吞错不上抛，所以写入失败不会在这里报错；用计数兜住：
        // 走到这一步计数没涨，才证明落盘真的成功了。
        assert_eq!(diagnostic_log_write_failures(), failures_before);

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

    /// 剥掉源码里的测试段：从 `#[cfg(test)]` 起，按大括号配平找到该段结束。
    /// 只数大括号、不解析字符串与注释——对本仓库的文件形状足够，且由
    /// `test_module_stripper_own_tests` 用合成输入自证，不靠"看起来对"。
    fn production_segments(source: &str) -> String {
        let mut kept = String::new();
        // None = 不在测试段内；Some(d) = 在测试段内，d 为当前未配平的大括号深度。
        let mut skip_depth: Option<i32> = None;
        for line in source.lines() {
            if skip_depth.is_none() && line.trim_start().starts_with("#[cfg(test)]") {
                skip_depth = Some(0);
                continue;
            }
            let Some(depth) = skip_depth else {
                kept.push_str(line);
                kept.push('\n');
                continue;
            };
            let next = depth + line.matches('{').count() as i32 - line.matches('}').count() as i32;
            skip_depth = if next <= 0 { None } else { Some(next) };
        }
        kept
    }

    /// 从生产段源码里挑出裸事件名字面量（带归族前缀，或属于历史无前缀族）。
    fn bare_event_literals(source: &str) -> Vec<String> {
        let production = production_segments(source);
        let chars: Vec<char> = production.chars().collect();
        let mut found = Vec::new();
        let mut i = 0;
        while i < chars.len() {
            if chars[i] != '"' {
                i += 1;
                continue;
            }
            let mut lit = String::new();
            let mut j = i + 1;
            let mut closed = false;
            while j < chars.len() {
                match chars[j] {
                    '"' => {
                        closed = true;
                        break;
                    }
                    '\\' => {
                        j += 2;
                        continue;
                    }
                    '\n' => break,
                    c => {
                        lit.push(c);
                        j += 1;
                    }
                }
            }
            let is_event_name = lit.starts_with("codex_layer.")
                || lit.starts_with("protocol_proxy.")
                || lit.starts_with("diagnostic_log.")
                || lit == events::misc::INLINE_IMAGE_DATA_URL_LEAK;
            if is_event_name {
                found.push(lit);
            }
            i = if closed { j + 1 } else { i + 1 };
        }
        found
    }

    /// 扫描器自证：生产段的字面量必须被抓到，测试段内的必须被放过。
    /// 没有这条，下面那条判据可能是"恒为 0"的假绿。
    #[test]
    fn test_module_stripper_own_tests() {
        let source = concat!(
            "// 头注释\n",
            "const BAD: &str = \"codex_layer.sneaked_in\";\n",
            "\n",
            "#[cfg(test)]\n",
            "mod tests {\n",
            "    use super::*;\n",
            "    #[test]\n",
            "    fn t() {\n",
            "        let ok = \"codex_layer.only_in_tests\";\n",
            "        assert!(!ok.is_empty());\n",
            "    }\n",
            "}\n",
            "\n",
            "const AFTER: &str = \"protocol_proxy.also_sneaked\";\n",
        );
        assert_eq!(
            bare_event_literals(source),
            vec![
                "codex_layer.sneaked_in".to_string(),
                "protocol_proxy.also_sneaked".to_string()
            ],
            "扫描器必须抓到测试段之外的字面量、放过测试段内的"
        );
    }

    /// #31 的核心判据：生产段里不得再出现裸事件名字面量。
    /// 事件名只能来自 [`events`] 登记常量——消费侧（#19 聚合 / #20 界面）
    /// 因此**在类型上**只能使用登记名，"用到的每个事件名都在定义表内"由编译器
    /// 保证；这条测试堵住的是"重新写回字面量绕过登记表"的回头路。
    /// 定义表所在文件本身豁免：它就是唯一允许书写事件名字面量的地方。
    #[test]
    fn production_code_has_no_bare_event_name_literals() {
        let files = [
            ("main.rs", include_str!("main.rs")),
            ("protocol_proxy.rs", include_str!("protocol_proxy.rs")),
            ("stability.rs", include_str!("stability.rs")),
            ("stability_ui.rs", include_str!("stability_ui.rs")),
            ("prefix_divergence.rs", include_str!("prefix_divergence.rs")),
        ];
        for (name, source) in files {
            let hits = bare_event_literals(source);
            assert!(
                hits.is_empty(),
                "{name} 的生产段出现裸事件名字面量 {hits:?}；请改用 events 登记常量"
            );
        }
    }

    /// 失败口径的可观测面：写入失败必须被吞掉（绝不上抛、绝不 panic），
    /// 同时计入 [`diagnostic_log_write_failures`]，不静默无痕。
    #[test]
    fn log_event_swallows_write_failure_and_counts_it() {
        let _diag_guard = DIAG_LOG_TEST_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let temp = tempfile::tempdir().unwrap();
        // 拿一个普通文件当父目录用：create_dir_all 必然失败，覆盖真实写失败路径。
        let blocker = temp.path().join("blocker");
        std::fs::write(&blocker, b"not-a-directory").unwrap();
        set_diagnostic_log_path(Some(blocker.join("sub").join("codex-layer.log")));

        let before = diagnostic_log_write_failures();
        log_event(events::layer::STARTED, json!({ "probe": 1 }));
        let after = diagnostic_log_write_failures();

        set_diagnostic_log_path(None);
        assert_eq!(
            after,
            before + 1,
            "写入失败必须降级为计数 +1，否则失败就真的静默了"
        );
    }
}
