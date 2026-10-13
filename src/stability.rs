//! 稳定性监看数据面（issue #19）：把诊断日志 JSONL 聚合成分发用的快照。
//!
//! 纯函数纪律（`docs/CODING_STANDARDS.md`）：本模块**不读全局配置、不摸环境变量、
//! 不读文件、不碰时钟**。输入是日志尾部窗口的原始行（`&[&str]`），输出是可序列化的
//! `Value` 快照。文件读取与 `generatedAtMs` 注入在 `main.rs` 装配层完成。
//!
//! 隐私红线：`oldSnip`/`newSnip` 可能含用户提示词，默认只给长度与位置；
//! `show_prompt_snippets` 开启时才输出截断到 120 字符的片段，并带明确警示字段。

use serde_json::{Value, json};
use crate::diagnostic_log::events;
use std::collections::HashMap;

/// 快照保留的最近 divergence 条数默认上限。
pub const DEFAULT_RECENT_DIVERGENCES: usize = 10;
/// 快照保留的最近轮次条数默认上限（界面「最近轮次表」消费，issue #20）。
pub const DEFAULT_RECENT_TURNS: usize = 20;
/// `show_prompt_snippets` 开启时单个 snip 的字符上限。
pub const MAX_SNIPPET_CHARS: usize = 120;

/// 聚合开关（由装配层从 `[stability]` 配置注入；纯函数本体不读配置）。
#[derive(Debug, Clone)]
pub struct StabilityAggregateOptions {
    /// 是否输出 divergence 片段的截断内容（默认关）。
    pub show_prompt_snippets: bool,
    /// 最近 divergence 明细保留条数。
    pub recent_divergences: usize,
    /// 最近轮次明细保留条数。
    pub recent_turns: usize,
}

impl Default for StabilityAggregateOptions {
    fn default() -> Self {
        Self {
            show_prompt_snippets: false,
            recent_divergences: DEFAULT_RECENT_DIVERGENCES,
            recent_turns: DEFAULT_RECENT_TURNS,
        }
    }
}

/// 单行 JSONL 解析后的记录；解析失败或形态不对（缺 event / timestamp_ms）的行
/// 计入 `parseFailures` 后跳过，不整体报错。
struct ParsedRecord {
    timestamp_ms: u64,
    pid: u64,
    event: String,
    detail: Value,
}

/// 最近轮次明细行（issue #20 的「最近轮次表」消费）：`outcome` 为空表示
/// 窗口内仍开着未收口；终态-only 行（收口找不到开流）也照实入表。
#[derive(Debug)]
struct TurnRow {
    timestamp_ms: u64,
    path: Option<String>,
    compaction: bool,
    request_id: Option<String>,
    outcome: Option<&'static str>,
    duration_ms: Option<u64>,
}

fn parse_record(line: &str) -> Option<ParsedRecord> {
    let value: Value = serde_json::from_str(line).ok()?;
    let timestamp_ms = value.get("timestamp_ms")?.as_u64()?;
    let event = value.get("event")?.as_str()?.to_string();
    Some(ParsedRecord {
        timestamp_ms,
        // pid 缺失不算解析失败：配对键用哨兵值 0。
        pid: value.get("pid").and_then(Value::as_u64).unwrap_or(0),
        detail: value.get("detail").cloned().unwrap_or(Value::Null),
        event,
    })
}

/// 把日志尾部窗口的原始行聚合成稳定性快照（纯函数）。
///
/// 轮次口径：`stream_open` 计一轮（`compaction=true` 另计压缩轮）；
/// 单轮耗时用 `requestId` 精确配对 `stream_open` → `stream_completed` /
/// `stream_failed`，配对不上的显式计入 `durationsMs.unpaired`，不静默丢弃。
pub fn aggregate_stability_snapshot(lines: &[&str], options: &StabilityAggregateOptions) -> Value {
    let mut parse_failures = 0usize;
    let mut records: Vec<ParsedRecord> = Vec::new();
    for line in lines {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        match parse_record(trimmed) {
            Some(record) => records.push(record),
            None => parse_failures += 1,
        }
    }

    // —— 会话概览：最近一次 started + 全窗口时间范围 ——
    let mut started: Option<(u64, u64, Value)> = None;
    let mut first_ts: Option<u64> = None;
    let mut last_ts: Option<u64> = None;

    // —— 轮次与失败计数 ——
    let mut opens_total = 0usize;
    let mut opens_compaction = 0usize;
    let mut completed = 0usize;
    let mut failed = 0usize;
    let mut conversion_rejected = 0usize;
    let mut upstream_error = 0usize;

    // —— 稳定性信号 ——
    let mut baseline = 0usize;
    let mut divergence = 0usize;
    let mut divergence_details: Vec<(u64, &Value)> = Vec::new();
    let mut compaction_appended = 0usize;
    let mut dedup_last_folded = 0usize;
    let mut unpaired_notice_stripped = 0usize;

    // —— requestId 配对：键含 pid，跨进程重启不误配 ——
    // (pid, requestId) → 开流在 turn_rows 里占的行号；收口按号回写 outcome 与耗时。
    let mut open_index: HashMap<(u64, String), usize> = HashMap::new();
    let mut turn_rows: Vec<TurnRow> = Vec::new();
    let mut durations: Vec<u64> = Vec::new();
    let mut terminal_without_open = 0usize;
    // 开流缺 requestId：天然配不上，显式计入 unpaired，不静默丢。
    let mut open_missing_request_id = 0usize;

    for record in &records {
        first_ts = Some(first_ts.map_or(record.timestamp_ms, |t| t.min(record.timestamp_ms)));
        last_ts = Some(last_ts.map_or(record.timestamp_ms, |t| t.max(record.timestamp_ms)));

        match record.event.as_str() {
            events::layer::STARTED => {
                started = Some((record.timestamp_ms, record.pid, record.detail.clone()));
            }
            events::layer::STREAM_OPEN => {
                opens_total += 1;
                let compaction =
                    record.detail.get("compaction").and_then(Value::as_bool) == Some(true);
                if compaction {
                    opens_compaction += 1;
                }
                let request_id = record
                    .detail
                    .get("requestId")
                    .and_then(Value::as_str)
                    .map(str::to_string);
                let index = turn_rows.len();
                turn_rows.push(TurnRow {
                    timestamp_ms: record.timestamp_ms,
                    path: record
                        .detail
                        .get("path")
                        .and_then(Value::as_str)
                        .map(str::to_string),
                    compaction,
                    request_id: request_id.clone(),
                    outcome: None,
                    duration_ms: None,
                });
                match request_id {
                    Some(id) => {
                        open_index.insert((record.pid, id), index);
                    }
                    None => open_missing_request_id += 1,
                }
            }
            events::layer::STREAM_COMPLETED => {
                completed += 1;
                pair_terminal(
                    record,
                    "completed",
                    &mut open_index,
                    &mut turn_rows,
                    &mut durations,
                    &mut terminal_without_open,
                );
            }
            events::layer::STREAM_FAILED => {
                failed += 1;
                pair_terminal(
                    record,
                    "failed",
                    &mut open_index,
                    &mut turn_rows,
                    &mut durations,
                    &mut terminal_without_open,
                );
            }
            events::layer::CONVERSION_REJECTED => {
                conversion_rejected += 1;
                push_event_row(record, "rejected", &mut turn_rows);
            }
            events::layer::UPSTREAM_ERROR => {
                upstream_error += 1;
                push_event_row(record, "upstream_error", &mut turn_rows);
            }
            events::proxy::STABILIZER_PREFIX_BASELINE => baseline += 1,
            events::proxy::STABILIZER_PREFIX_DIVERGENCE => {
                divergence += 1;
                divergence_details.push((record.timestamp_ms, &record.detail));
            }
            events::proxy::STABILIZER_COMPACTION_APPENDED => compaction_appended += 1,
            events::proxy::STABILIZER_DEDUP_LAST_FOLDED => dedup_last_folded += 1,
            events::proxy::STABILIZER_UNPAIRED_NOTICE_STRIPPED => {
                unpaired_notice_stripped += 1
            }
            _ => {}
        }
    }

    // 最近 N 条 divergence：新→旧排序；snip 默认只报长度与位置。
    divergence_details.sort_by_key(|(timestamp_ms, _)| std::cmp::Reverse(*timestamp_ms));
    let recent: Vec<Value> = divergence_details
        .into_iter()
        .take(options.recent_divergences)
        .map(|(timestamp_ms, detail)| {
            let mut item = json!({
                "timestampMs": timestamp_ms,
                "firstDiff": detail.get("firstDiff").cloned().unwrap_or(Value::Null),
                "oldLen": detail.get("oldLen").cloned().unwrap_or(Value::Null),
                "newLen": detail.get("newLen").cloned().unwrap_or(Value::Null),
            });
            if options.show_prompt_snippets {
                item["oldSnip"] = snippet_value(detail.get("oldSnip"));
                item["newSnip"] = snippet_value(detail.get("newSnip"));
            }
            item
        })
        .collect();

    let durations_sorted = {
        durations.sort_unstable();
        durations
    };
    let open_without_terminal = open_index.len() + open_missing_request_id;

    // 最近轮次：新→旧；outcome 为 null 的行由界面渲染成「开着未收口」。
    let mut recent_turn_rows: Vec<&TurnRow> = turn_rows.iter().collect();
    recent_turn_rows.sort_by_key(|row| std::cmp::Reverse(row.timestamp_ms));
    let recent_turns: Vec<Value> = recent_turn_rows
        .into_iter()
        .take(options.recent_turns)
        .map(|row| {
            json!({
                "timestampMs": row.timestamp_ms,
                "path": row.path,
                "compaction": row.compaction,
                "requestId": row.request_id,
                "outcome": row.outcome,
                "durationMs": row.duration_ms,
            })
        })
        .collect();

    json!({
        "window": {
            "linesParsed": records.len(),
            "parseFailures": parse_failures,
        },
        "session": {
            "started": started.as_ref().map(|(timestamp_ms, pid, detail)| json!({
                "timestampMs": timestamp_ms,
                "pid": pid,
                "detail": detail,
            })),
            "firstTimestampMs": first_ts,
            "lastTimestampMs": last_ts,
        },
        "turns": {
            "total": opens_total,
            "compaction": opens_compaction,
            "completed": completed,
            "failed": failed,
            "conversionRejected": conversion_rejected,
            "upstreamError": upstream_error,
        },
        "recentTurns": recent_turns,
        "stability": {
            "baseline": baseline,
            "divergence": divergence,
            // divergence / 轮次：0 轮时是「未定义」而不是 0，不给假安全读数。
            "divergencePerTurn": if opens_total > 0 {
                json!(((divergence as f64 / opens_total as f64) * 1e4).round() / 1e4)
            } else {
                Value::Null
            },
            "recentDivergences": recent,
            "snippets": if options.show_prompt_snippets { "truncated" } else { "redacted" },
            "snippetsWarning": if options.show_prompt_snippets {
                json!("已开启片段外传：oldSnip/newSnip 可能包含用户提示词内容，上限 120 字符，勿将响应贴入公开渠道。")
            } else {
                Value::Null
            },
            "stabilizerActions": {
                "compactionAppended": compaction_appended,
                "dedupLastFolded": dedup_last_folded,
                "unpairedNoticeStripped": unpaired_notice_stripped,
            },
        },
        "durationsMs": {
            "samples": durations_sorted.len(),
            "p50": percentile_nearest_rank(&durations_sorted, 0.50),
            "p95": percentile_nearest_rank(&durations_sorted, 0.95),
            "max": durations_sorted.last().copied(),
            "unpaired": {
                // 开流了但窗口里没等到收口（含缺 requestId 的开流）。
                "openWithoutTerminal": open_without_terminal,
                // 收口了但窗口里没有开流（含缺 requestId 的收口、日志裁剪掉开流行）。
                "terminalWithoutOpen": terminal_without_open,
            },
        },
    })
}

/// 收口事件与开流按 `(pid, requestId)` 配对：配得上就回写开流行并采样耗时；
/// 配不上（含缺 requestId）计数不静默丢，并照实入一行终态-only 明细。
/// 时钟回拨导致的负耗时钳到 0：日志异常不该让分布统计整体报错。
fn pair_terminal(
    record: &ParsedRecord,
    outcome: &'static str,
    open_index: &mut HashMap<(u64, String), usize>,
    turn_rows: &mut Vec<TurnRow>,
    durations: &mut Vec<u64>,
    terminal_without_open: &mut usize,
) {
    let request_id = record.detail.get("requestId").and_then(Value::as_str);
    let paired =
        request_id.and_then(|id| open_index.remove(&(record.pid, id.to_string())));
    match paired {
        Some(index) => {
            let duration = record.timestamp_ms.saturating_sub(turn_rows[index].timestamp_ms);
            turn_rows[index].outcome = Some(outcome);
            turn_rows[index].duration_ms = Some(duration);
            durations.push(duration);
        }
        None => {
            *terminal_without_open += 1;
            turn_rows.push(TurnRow {
                timestamp_ms: record.timestamp_ms,
                path: None,
                compaction: false,
                request_id: request_id.map(str::to_string),
                outcome: Some(outcome),
                duration_ms: None,
            });
        },
    }
}

/// 拒绝 / 上游故障事件入最近轮次表（它们通常没有开流语境，耗时未知）。
fn push_event_row(
    record: &ParsedRecord,
    outcome: &'static str,
    turn_rows: &mut Vec<TurnRow>,
) {
    turn_rows.push(TurnRow {
        timestamp_ms: record.timestamp_ms,
        path: record
            .detail
            .get("path")
            .and_then(Value::as_str)
            .map(str::to_string),
        compaction: false,
        request_id: record
            .detail
            .get("requestId")
            .and_then(Value::as_str)
            .map(str::to_string),
        outcome: Some(outcome),
        duration_ms: None,
    });
}

/// 最近秩法（nearest-rank，向上取秩）：p 分位取排序后第 `ceil(p*n)` 个值。
/// 空样本返回 null 而不是 0，避免「没有样本」被读成「耗时为 0」。
fn percentile_nearest_rank(sorted: &[u64], percentile: f64) -> Option<u64> {
    if sorted.is_empty() {
        return None;
    }
    let rank = (percentile * sorted.len() as f64).ceil() as usize;
    Some(sorted[rank.clamp(1, sorted.len()) - 1])
}

/// snip 的对外形态：字符截断到上限；非字符串 detail（异常日志）如实给 null。
fn snippet_value(raw: Option<&Value>) -> Value {
    match raw.and_then(Value::as_str) {
        Some(text) => {
            let truncated: String = text.chars().take(MAX_SNIPPET_CHARS).collect();
            Value::String(truncated)
        }
        None => Value::Null,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(timestamp_ms: u64, pid: u64, event: &str, detail: Value) -> String {
        json!({ "timestamp_ms": timestamp_ms, "pid": pid, "event": event, "detail": detail })
            .to_string()
    }

    fn open(timestamp_ms: u64, pid: u64, request_id: Option<&str>) -> String {
        let detail = match request_id {
            Some(id) => json!({ "path": "/v1/responses", "requestId": id }),
            None => json!({ "path": "/v1/responses" }),
        };
        line(timestamp_ms, pid, events::layer::STREAM_OPEN, detail)
    }

    fn terminal(timestamp_ms: u64, pid: u64, event: &str, request_id: Option<&str>) -> String {
        let detail = match request_id {
            Some(id) => json!({ "requestId": id }),
            None => json!({}),
        };
        line(timestamp_ms, pid, event, detail)
    }

    /// 并发交错的开流/收口必须按 requestId 精确配对；配不上的两个方向都进 unpaired。
    #[test]
    fn concurrent_requests_pair_by_request_id_and_count_unpaired() {
        let lines = [
            open(1000, 7, Some("A")),
            open(1100, 7, Some("B")),
            terminal(1600, 7, events::layer::STREAM_COMPLETED, Some("B")),
            terminal(2600, 7, events::layer::STREAM_COMPLETED, Some("A")),
            // 开流了没等到收口；开流缺 requestId 同样配不上。
            open(2700, 7, Some("C")),
            open(2750, 7, None),
            // 收口找不到开流；收口缺 requestId 同样配不上。
            terminal(3000, 7, events::layer::STREAM_COMPLETED, Some("D")),
            terminal(3100, 7, events::layer::STREAM_FAILED, None),
        ];
        let lines: Vec<&str> = lines.iter().map(String::as_str).collect();
        let snapshot = aggregate_stability_snapshot(&lines, &StabilityAggregateOptions::default());

        assert_eq!(snapshot["turns"]["total"], json!(4));
        assert_eq!(snapshot["turns"]["completed"], json!(3));
        assert_eq!(snapshot["turns"]["failed"], json!(1));
        // A=1600ms、B=500ms；按 requestId 配对不受交错顺序影响。
        let durations = &snapshot["durationsMs"];
        assert_eq!(durations["samples"], json!(2));
        assert_eq!(durations["p50"], json!(500));
        assert_eq!(durations["p95"], json!(1600));
        assert_eq!(durations["max"], json!(1600));
        assert_eq!(durations["unpaired"]["openWithoutTerminal"], json!(2));
        assert_eq!(durations["unpaired"]["terminalWithoutOpen"], json!(2));
        // 最近轮次表：新→旧；开流行被收口回写，终态-only 行照实入表。
        let turns = snapshot["recentTurns"].as_array().unwrap();
        assert_eq!(turns[0]["timestampMs"], json!(3100));
        assert_eq!(turns[0]["outcome"], json!("failed"));
        assert!(turns[0]["durationMs"].is_null(), "配不上开流的收口没有耗时");
        let a_row = turns
            .iter()
            .find(|row| row["requestId"] == json!("A"))
            .unwrap();
        assert_eq!(a_row["timestampMs"], json!(1000), "行时间戳锚在开流时刻");
        assert_eq!(a_row["outcome"], json!("completed"));
        assert_eq!(a_row["durationMs"], json!(1600));
        let c_row = turns
            .iter()
            .find(|row| row["requestId"] == json!("C"))
            .unwrap();
        assert!(c_row["outcome"].is_null(), "未收口的开流 outcome 为 null");
    }

    /// 同名 requestId 跨 pid（进程重启）不得互配。
    #[test]
    fn identical_request_ids_across_pids_do_not_cross_pair() {
        let lines = [
            open(1000, 1, Some("same")),
            terminal(5000, 2, events::layer::STREAM_COMPLETED, Some("same")),
        ];
        let lines: Vec<&str> = lines.iter().map(String::as_str).collect();
        let snapshot = aggregate_stability_snapshot(&lines, &StabilityAggregateOptions::default());
        assert_eq!(snapshot["durationsMs"]["samples"], json!(0));
        assert_eq!(snapshot["durationsMs"]["unpaired"]["openWithoutTerminal"], json!(1));
        assert_eq!(snapshot["durationsMs"]["unpaired"]["terminalWithoutOpen"], json!(1));
    }

    /// divergence 是核心信号：计数、比率、新→旧顺序都要正确；
    /// snip 默认整体隐去，开启后才截断外传并带警示。
    #[test]
    fn divergence_counts_ratio_order_and_snippet_gating() {
        let long_snip = "提".repeat(300);
        let mut lines: Vec<String> = (0..5).map(|i| open(1000 + i, 9, Some(&format!("r{i}")))).collect();
        lines.push(line(
            2000,
            9,
            events::proxy::STABILIZER_PREFIX_BASELINE,
            json!({ "oldLen": 0, "newLen": 500, "firstDiff": null, "oldSnip": "", "newSnip": "abc" }),
        ));
        lines.push(line(
            3000,
            9,
            events::proxy::STABILIZER_PREFIX_DIVERGENCE,
            json!({ "oldLen": 1000, "newLen": 1100, "firstDiff": 42, "oldSnip": long_snip, "newSnip": "x" }),
        ));
        lines.push(line(
            4000,
            9,
            events::proxy::STABILIZER_PREFIX_DIVERGENCE,
            json!({ "oldLen": 2000, "newLen": 2050, "firstDiff": 7, "oldSnip": "y", "newSnip": "z" }),
        ));
        let owned: Vec<&str> = lines.iter().map(String::as_str).collect();

        let snapshot = aggregate_stability_snapshot(&owned, &StabilityAggregateOptions::default());
        let stability = &snapshot["stability"];
        assert_eq!(stability["baseline"], json!(1));
        assert_eq!(stability["divergence"], json!(2));
        assert_eq!(stability["divergencePerTurn"], json!(0.4));
        // 新→旧：ts=4000 在前。
        assert_eq!(stability["recentDivergences"][0]["timestampMs"], json!(4000));
        assert_eq!(stability["recentDivergences"][1]["timestampMs"], json!(3000));
        assert_eq!(stability["recentDivergences"][0]["firstDiff"], json!(7));
        assert_eq!(stability["recentDivergences"][0]["oldLen"], json!(2000));
        // 默认：snip 只字不出，连 key 都不出现；只留长度与位置。
        assert_eq!(stability["snippets"], json!("redacted"));
        assert!(stability["recentDivergences"][0].get("oldSnip").is_none());
        assert!(stability["recentDivergences"][0].get("newSnip").is_none());
        assert!(
            !snapshot.to_string().contains(&long_snip[..30]),
            "默认配置下用户提示词片段绝不允许出现在快照里"
        );

        let shown = aggregate_stability_snapshot(
            &owned,
            &StabilityAggregateOptions { show_prompt_snippets: true, recent_divergences: 10, recent_turns: 20 },
        );
        let shown_stability = &shown["stability"];
        assert_eq!(shown_stability["snippets"], json!("truncated"));
        let snip = shown_stability["recentDivergences"][0]["newSnip"]
            .as_str()
            .unwrap()
            .to_string();
        assert!(shown_stability["snippetsWarning"].is_string());
        // ts=4000 的 newSnip 是 "z"；截断只影响超上限的旧条目。
        assert_eq!(snip, "z");
        let old_snip = shown_stability["recentDivergences"][1]["oldSnip"]
            .as_str()
            .unwrap()
            .chars()
            .count();
        assert_eq!(old_snip, MAX_SNIPPET_CHARS, "超长 snip 必须截到 120 字符上限");
    }

    /// 0 轮时比率是 null（未定义）而不是 0，避免假安全。

    /// 拒绝/上游故障事件也要进最近轮次表：它们没有开流语境，耗时置空。
    #[test]
    fn rejected_and_upstream_errors_appear_in_recent_turns() {
        let lines = [
            line(
                1500,
                7,
                events::layer::CONVERSION_REJECTED,
                json!({ "path": "/v1/responses", "requestId": "rj-1" }),
            ),
            line(
                1600,
                7,
                events::layer::UPSTREAM_ERROR,
                json!({ "path": "/v1/responses", "requestId": "ue-1" }),
            ),
        ];
        let owned: Vec<&str> = lines.iter().map(String::as_str).collect();
        let snapshot = aggregate_stability_snapshot(&owned, &StabilityAggregateOptions::default());
        let turns = snapshot["recentTurns"].as_array().unwrap();
        assert_eq!(turns.len(), 2);
        assert_eq!(turns[0]["outcome"], json!("upstream_error"));
        assert_eq!(turns[1]["outcome"], json!("rejected"));
        assert!(turns[0]["durationMs"].is_null());
        // 它们不参与 completed/failed 配对，unpaired 保持双零。
        assert_eq!(snapshot["durationsMs"]["unpaired"]["terminalWithoutOpen"], json!(0));
    }
    #[test]
    fn ratio_is_null_without_opens() {
        let lines = vec![line(
            1000,
            9,
            events::proxy::STABILIZER_PREFIX_DIVERGENCE,
            json!({ "oldLen": 1, "newLen": 2, "firstDiff": 0 }),
        )];
        let owned: Vec<&str> = lines.iter().map(String::as_str).collect();
        let snapshot = aggregate_stability_snapshot(&owned, &StabilityAggregateOptions::default());
        assert!(snapshot["stability"]["divergencePerTurn"].is_null());
        assert!(snapshot["durationsMs"]["p50"].is_null());
        assert!(snapshot["durationsMs"]["max"].is_null());
    }

    /// 钉定器生效计数与压缩轮计数各归各的账。
    #[test]
    fn stabilizer_actions_and_compaction_turns_are_counted() {
        let mut lines: Vec<String> = vec![
            line(10, 3, events::proxy::STABILIZER_COMPACTION_APPENDED, json!({})),
            line(11, 3, events::proxy::STABILIZER_COMPACTION_APPENDED, json!({})),
            line(12, 3, events::proxy::STABILIZER_DEDUP_LAST_FOLDED, json!({})),
            line(13, 3, events::proxy::STABILIZER_UNPAIRED_NOTICE_STRIPPED, json!({})),
            line(14, 3, events::proxy::STABILIZER_UNPAIRED_NOTICE_STRIPPED, json!({})),
            line(15, 3, events::proxy::STABILIZER_UNPAIRED_NOTICE_STRIPPED, json!({})),
        ];
        lines.push(open(20, 3, Some("c1")));
        // 压缩轮开流带 compaction=true。
        lines.push(json!({
            "timestamp_ms": 21, "pid": 3, "event": events::layer::STREAM_OPEN,
            "detail": { "path": "/v1/responses", "requestId": "c2", "compaction": true }
        }).to_string());
        lines.push(json!({
            "timestamp_ms": 22, "pid": 3, "event": events::layer::STREAM_OPEN,
            "detail": { "path": "/v1/responses", "requestId": "c3", "compaction": false }
        }).to_string());
        let owned: Vec<&str> = lines.iter().map(String::as_str).collect();
        let snapshot = aggregate_stability_snapshot(&owned, &StabilityAggregateOptions::default());
        assert_eq!(snapshot["turns"]["total"], json!(3));
        assert_eq!(snapshot["turns"]["compaction"], json!(1));
        assert_eq!(snapshot["stability"]["stabilizerActions"]["compactionAppended"], json!(2));
        assert_eq!(snapshot["stability"]["stabilizerActions"]["dedupLastFolded"], json!(1));
        assert_eq!(
            snapshot["stability"]["stabilizerActions"]["unpairedNoticeStripped"],
            json!(3)
        );
    }

    /// 空日志、坏行、缺字段的行：计数后跳过，快照结构完整。
    #[test]
    fn empty_and_garbage_lines_survive() {
        let lines = vec![
            "",
            "not json at all",
            "{\"no_event\":1}",
            "{\"timestamp_ms\":\"不是数字\",\"event\":\"x\"}",
            "{\"timestamp_ms\":5,\"event\":\"codex_layer.unknown_event\"}",
        ];
        let snapshot = aggregate_stability_snapshot(&lines, &StabilityAggregateOptions::default());
        assert_eq!(snapshot["window"]["linesParsed"], json!(1));
        // 空行是换行/窗口边界的正常形态，不算解析失败；其余 3 条坏行才计数。
        assert_eq!(snapshot["window"]["parseFailures"], json!(3));
        assert!(snapshot["session"]["started"].is_null());
        assert_eq!(snapshot["session"]["firstTimestampMs"], json!(5));
        assert_eq!(snapshot["session"]["lastTimestampMs"], json!(5));
        assert_eq!(snapshot["turns"]["total"], json!(0));
    }

    /// 会话概览取最近一次 started；时间范围覆盖全窗口。
    #[test]
    fn session_uses_latest_started_and_full_time_range() {
        let lines = [
            line(
                100,
                1,
                events::layer::STARTED,
                json!({ "listen": "127.0.0.1:57321" }),
            ),
            line(
                500,
                2,
                events::layer::STARTED,
                json!({ "listen": "127.0.0.1:57331", "upstream": "http://127.0.0.1:11234/v1" }),
            ),
            open(700, 2, Some("r1")),
        ];
        let owned: Vec<&str> = lines.iter().map(String::as_str).collect();
        let snapshot = aggregate_stability_snapshot(&owned, &StabilityAggregateOptions::default());
        assert_eq!(snapshot["session"]["started"]["pid"], json!(2));
        assert_eq!(snapshot["session"]["started"]["timestampMs"], json!(500));
        assert_eq!(snapshot["session"]["started"]["detail"]["listen"], json!("127.0.0.1:57331"));
        assert_eq!(snapshot["session"]["firstTimestampMs"], json!(100));
        assert_eq!(snapshot["session"]["lastTimestampMs"], json!(700));
    }

    /// 最近秩分位的钉定：1..=100 的样本，p50/p95/max 各就各位。
    #[test]
    fn percentile_nearest_rank_pins_convention() {
        let sorted: Vec<u64> = (1..=100).collect();
        assert_eq!(percentile_nearest_rank(&sorted, 0.50), Some(50));
        assert_eq!(percentile_nearest_rank(&sorted, 0.95), Some(95));
        assert_eq!(percentile_nearest_rank(&sorted, 1.00), Some(100));
        let single = vec![42u64];
        assert_eq!(percentile_nearest_rank(&single, 0.50), Some(42));
        assert_eq!(percentile_nearest_rank(&[], 0.50), None);
    }

    /// 最近 divergence 条数上限生效（超出后丢最旧的）。
    #[test]
    fn recent_divergence_cap_keeps_newest() {
        let mut lines: Vec<String> = (1..=12u64)
            .map(|i| {
                line(
                    1000 + i,
                    1,
                    events::proxy::STABILIZER_PREFIX_DIVERGENCE,
                    json!({ "oldLen": i, "newLen": i, "firstDiff": i }),
                )
            })
            .collect();
        lines.push(line(
            999,
            1,
            events::proxy::STABILIZER_PREFIX_BASELINE,
            json!({ "oldLen": 0, "newLen": 0, "firstDiff": null }),
        ));
        let owned: Vec<&str> = lines.iter().map(String::as_str).collect();
        let snapshot = aggregate_stability_snapshot(
            &owned,
            &StabilityAggregateOptions { show_prompt_snippets: false, recent_divergences: 5, recent_turns: 20 },
        );
        let recent = snapshot["stability"]["recentDivergences"].as_array().unwrap();
        assert_eq!(recent.len(), 5);
        assert_eq!(recent[0]["timestampMs"], json!(1012));
        assert_eq!(recent[4]["timestampMs"], json!(1008));
        assert_eq!(snapshot["stability"]["divergence"], json!(12));
    }
}
