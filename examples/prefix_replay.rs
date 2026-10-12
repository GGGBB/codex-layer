// 移植来源：CodexPlusPlus PR #2449 的 `crates/codex-plus-core/examples/prefix_replay.rs`，
// commit f9be7cf。从日志回放取证的前缀覆盖率工具，本项目原样带走。
// 离线回放：把取证抓到的客户端原始 /v1/responses 体逐轮喂给稳定层，
// 度量相邻轮「归一化后前一请求 messages 是否被完整覆盖（纯追加）」。
// 判据：coverage = common(上一轮messages, 本轮messages) / len(上一轮messages)。
// coverage < 1 = 前一请求历史被改写（早分叉，缓存前缀作废）；= 1 = 纯追加。
// 只输出数值与极短截断上下文，不落盘任何正文。
// 用法：cargo run --example prefix_replay -- /tmp/prefix_forensics
use serde_json::Value;
use std::fs;
use std::path::PathBuf;

fn fnv(buf: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in buf {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

fn common_prefix(a: &[u8], b: &[u8]) -> usize {
    a.iter().zip(b.iter()).take_while(|(x, y)| x == y).count()
}

/// 提取序列化结果里 "messages":[ ... ] 的字节区间（配对括号扫描，跳过字符串）。
fn messages_span(buf: &[u8]) -> (usize, usize) {
    let key = br#""messages":"#.to_vec();
    let start = buf
        .windows(key.len())
        .position(|w| w == key)
        .expect("无 messages 字段")
        + key.len();
    let mut depth = 0usize;
    let mut in_str = false;
    let mut esc = false;
    for i in start..buf.len() {
        let c = buf[i];
        if in_str {
            if esc {
                esc = false;
            } else if c == b'\\' {
                esc = true;
            } else if c == b'"' {
                in_str = false;
            }
            continue;
        }
        match c {
            b'"' => in_str = true,
            b'[' => depth += 1,
            b']' => {
                depth -= 1;
                if depth == 0 {
                    return (start, i + 1);
                }
            }
            _ => {}
        }
    }
    panic!("messages 未闭合");
}

/// 前一请求 messages 被完整覆盖时返回 None，否则返回分叉点上下文。
fn divergence(prev: &[u8], cur: &[u8]) -> Option<(f64, String, String)> {
    let c = common_prefix(prev, cur);
    let cov = c as f64 / prev.len() as f64;
    if cov >= 0.9999 {
        return None;
    }
    let cut = |buf: &[u8]| -> String {
        let s: String =
            String::from_utf8_lossy(&buf[c..(c + 90).min(buf.len())]).chars().take(90).collect();
        s.replace("data:image", "«D»")
    };
    Some((cov, cut(prev), cut(cur)))
}

fn main() {
    let dir = PathBuf::from(std::env::args().nth(1).expect("用法: prefix_replay <dir>"));
    let mut files: Vec<(u64, PathBuf)> = Vec::new();
    for entry in fs::read_dir(&dir).expect("读取目录失败") {
        let path = entry.expect("entry").path();
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if let Some(rest) = name.strip_prefix("client_") {
            if let Some(ms) = rest
                .rsplit('_')
                .next()
                .and_then(|s| s.strip_suffix(".json"))
                .and_then(|s| s.parse::<u64>().ok())
            {
                files.push((ms, path));
            }
        }
    }
    files.sort();
    println!("样本数: {}", files.len());

    let mut prev: Option<(String, Vec<u8>, Vec<u8>)> = None; // (cache_key, on_messages, off_messages)
    let mut pairs = 0usize;
    let mut viol_on = 0usize;
    let mut viol_off = 0usize;
    for (ms, path) in &files {
        let raw = fs::read_to_string(path).expect("读取样本失败");
        let body: serde_json::Value = serde_json::from_str(&raw).expect("样本非 JSON");
        // 会话归属：优先 thread_id，缺省回落 root_turn_id / prompt_cache_key
        let cm = body.get("client_metadata");
        let key = cm
            .and_then(|m| m.get("thread_id"))
            .and_then(Value::as_str)
            .or_else(|| cm.and_then(|m| m.get("root_turn_id")).and_then(Value::as_str))
            .or_else(|| body.get("prompt_cache_key").and_then(Value::as_str))
            .unwrap_or("")
            .to_string();
        let on = serde_json::to_vec(
            &codex_layer::protocol_proxy::responses_to_chat_completions_with_options(
                body.clone(),
                false,
                true,
            )
            .expect("稳定层转换失败"),
        )
        .expect("序列化失败");
        let off = serde_json::to_vec(
            &codex_layer::protocol_proxy::responses_to_chat_completions_with_options(
                body,
                false,
                false,
            )
            .expect("直通转换失败"),
        )
        .expect("序列化失败");
        let on_span = messages_span(&on);
        let off_span = messages_span(&off);
        let on_msg = on[on_span.0..on_span.1].to_vec();
        let off_msg = off[off_span.0..off_span.1].to_vec();
        let fam_on = key.clone();
        let fam_off = key.clone();
        if let Some((pkey, p_on, p_off)) = &prev {
            if pkey == &fam_on && !key.is_empty() {
                pairs += 1;
                if let Some((cov, cp, cc)) = divergence(p_on, &on_msg) {
                    viol_on += 1;
                    println!("ON  早分叉 ts={ms} coverage={cov:.4}\n  prev: {cp:?}\n  cur : {cc:?}");
                }
                if let Some((cov, cp, cc)) = divergence(p_off, &off_msg) {
                    viol_off += 1;
                    println!("OFF 早分叉 ts={ms} coverage={cov:.4}\n  prev: {cp:?}\n  cur : {cc:?}");
                }
            }
        }
        prev = Some((fam_on, on_msg, off_msg));
    }
    println!("同会话相邻对数: {pairs}");
    println!("早分叉对数: 稳定层开 {viol_on} / 稳定层关 {viol_off}");
}

