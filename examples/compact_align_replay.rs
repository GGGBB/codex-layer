// 压缩轮对齐的回放取证工具。
// 离线回放（compact_prefix_align）：把 /tmp/compact_forensics 的真实语料逐对喂给
// 「前缀对齐 + ChatCompletions 转换层」，验证压缩轮对齐后：
//   1) tools 序列化与主会话轮逐字节一致；
//   2) 压缩轮 chat messages 数组是主会话轮 messages 的纯前缀追加（common == len(prev)）；
//   3) 未对齐（旧行为）时前缀断裂，作为对照。
// 只输出数值统计，不落盘任何正文。语料含用户数据，绝不进仓库。
// 用法：cargo run --example compact_align_replay -- /tmp/compact_forensics
use codex_layer::protocol_proxy::responses_to_chat_completions_with_options;
use serde_json::Value;
use std::fs;
use std::path::PathBuf;

fn load(dir: &PathBuf, name: &str) -> Value {
    let path = dir.join(name);
    fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("读 {path:?} 失败: {e}"))
        .parse::<Value>()
        .expect("语料必须是 JSON")
}

fn common_prefix_len(a: &[Value], b: &[Value]) -> usize {
    a.iter().zip(b.iter()).take_while(|(x, y)| x == y).count()
}

fn tools_bytes(v: &Value) -> String {
    serde_json::to_string(&v.get("tools").cloned().unwrap_or(Value::Null)).unwrap()
}

fn main() {
    let dir = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp/compact_forensics"));
    // 语料对（seq=3/5 = 主会话轮 → 压缩轮本体；seq=4/6 为对照对）。
    let pairs: [(&str, &str, &str); 2] = [
        (
            "0003-1791654847652-tools-drift-prev.json",
            "0003-1791654847652-tools-drift-now.json",
            "seq3",
        ),
        (
            "0005-1791656958426-tools-drift-prev.json",
            "0005-1791656958426-tools-drift-now.json",
            "seq5",
        ),
    ];
    for (prev_name, now_name, label) in pairs {
        let prev = load(&dir, prev_name);
        let now = load(&dir, now_name);
        // 主会话轮先过 relay 接线（remember_tools 的等价物：直接灌缓存）。
        codex_layer::compaction_tools::remember_tools(&prev);
        let chat_prev =
            responses_to_chat_completions_with_options(prev.clone(), false, true).unwrap();
        // 旧行为对照：不对齐直接转换。
        let chat_now_old =
            responses_to_chat_completions_with_options(now.clone(), false, true).unwrap();
        // 新行为：前缀对齐后转换。
        let (aligned, report) = codex_layer::compaction_align::align_compaction_request(&now)
            .expect("压缩轮必须检出");
        let chat_now =
            responses_to_chat_completions_with_options(aligned.clone(), false, true).unwrap();

        let msgs_prev = chat_prev["messages"].as_array().unwrap();
        let msgs_old = chat_now_old["messages"].as_array().unwrap();
        let msgs_new = chat_now["messages"].as_array().unwrap();
        let cov_old = common_prefix_len(msgs_prev, msgs_old);
        let cov_new = common_prefix_len(msgs_prev, msgs_new);
        let tools_same_new = tools_bytes(&chat_now) == tools_bytes(&chat_prev);
        let tools_same_old = tools_bytes(&chat_now_old) == tools_bytes(&chat_prev);
        // 对齐后的压缩轮必须与直接再转换一次逐字节一致（幂等，稳定层无抖动）。
        let (aligned2, _) = codex_layer::compaction_align::align_compaction_request(&aligned)
            .expect("对齐后仍是压缩轮形态");
        let chat_again =
            responses_to_chat_completions_with_options(aligned2, false, true).unwrap();
        let idempotent = serde_json::to_string(&chat_again).unwrap()
            == serde_json::to_string(&chat_now).unwrap();

        println!(
            "[{label}] input 项 {}→{} | chat msgs {}→(旧 {} / 新 {}) | 前缀覆盖 旧 {}/{} 新 {}/{} | tools 字节一致 旧={} 新={} | 幂等={}",
            prev["input"].as_array().map_or(0, |v| v.len()),
            now["input"].as_array().map_or(0, |v| v.len()),
            msgs_prev.len(),
            msgs_old.len(),
            msgs_new.len(),
            cov_old,
            msgs_prev.len(),
            cov_new,
            msgs_prev.len(),
            tools_same_old,
            tools_same_new,
            idempotent,
        );
        println!(
            "[{label}] align 报告: instances={} moved={} appended={} tools_backfilled={} skipped_mixed={}",
            report.instances,
            report.moved,
            report.appended,
            report.tools_backfilled,
            report.skipped_mixed
        );
        assert!(report.tools_backfilled, "{label}: tools 必须回填命中");
        assert!(tools_same_new, "{label}: 对齐后 chat tools 必须与主会话轮逐字节一致");
        assert_eq!(cov_new, msgs_prev.len(), "{label}: 对齐后 messages 必须纯前缀追加");
        assert!(idempotent, "{label}: 对齐必须幂等");
        assert!(cov_old < msgs_prev.len() || !tools_same_old, "{label}: 对照应显示旧行为分叉");
    }
    println!("回放全部过门：压缩轮对齐后前缀纯追加成立。");
}
