// compaction 请求的 tools 回填缓存（「prompt 前缀稳定层」的组成部分，opt-in 同开关）。
//
// 背景（取证定案，见 COMPACT_PREFILL.md）：codex 客户端的 v2 压缩请求自带
// `tools: []`；而上游 chat 模板把 tools 渲染进 prompt 前部，且 relay 转换层
// 折叠历史 tool 项时依赖 tools schema——tools 由整块变空会让全历史消息重构
// （实测 231→388 msgs）、token 前缀在历史首个 tool 项处分叉，压缩轮整段
// ~15 万 token 全量冷算（约 4 分钟）。
//
// 方案：普通请求把 tools 序列化缓存下来（key = prompt_cache_key，codex 客户端
// 里等于线程 id，跨轮稳定）；压缩请求回填同一 Value——序列化逐字节一致，
// 历史折叠形态不变，token 前缀纯追加；摘要阶段禁工具调用交给 tool_choice=none
// （rewrite_request_for_compaction 已设置，不入 prompt 文本，不影响前缀）。
// 无缓存（如 relay 重启后首次即压缩）时保持旧行为（tools 置空）。
//
// 落盘层（issue #69 第 6 条）：回填缓存此前是纯内存结构，中继重启即清空，
// 而重启（改配置、换产物、发版）是本项目的常规动作——每次重启后的第一个
// 压缩轮必然退回 tools 置空的旧行为并整段冷算。落盘只作为**预热源**：
// 内存结构、上限策略与 remember_tools / preserved_tools 的签名语义一字不动。
//
// 跨重启的正确性论证（issue 要求写在这里而不是只写在 issue 里）：
// 回填的命中条件是「客户端 tools 定义没变」——缓存的键是 prompt_cache_key
// （codex 客户端里等于线程 id），值是**该键最近一次普通请求**的 tools 字节。
// 工具定义一旦变化，下一个普通轮就会覆盖缓存（以及盘上文件），压缩轮取到的
// 永远是重启前最近一轮的值；不存在「取到过期定义」的窗口，只存在「重启前
// 根本没跑过普通轮」的空窗——那与今天的内存版行为相同，保持旧行为。
//
// 三道 IO 守卫（顺序即优先级）：
// 1. 未显式 enable 一律零 IO——既有纯内存单测零副作用的根因；生产侧只有
//    `stabilize_prompt_prefix` 为真的装配点会 enable，开关关闭路径零副作用。
// 2. 读坏不写回——文件不存在 / 解析失败 / 版本不符一律只降级成「无缓存」，
//    进程不退出、不报错；在内存内容改变之前，禁止把（可能可救的）文件覆盖掉。
// 3. 写盘节流——序列化与上次落盘逐字节相同就不写；不同标脏，按最小间隔合并；
//    压缩轮立即刷，保证重启后取到的是最近一轮。
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::sync::{Mutex, OnceLock};

/// 上限 32 会话；超限整体重置（工具定义体量小，重置只会退回旧行为一次）。
const CACHE_LIMIT: usize = 32;

static CACHE: OnceLock<Mutex<BTreeMap<String, Value>>> = OnceLock::new();

/// 落盘文件的格式版本；版本不符一律只降级成「无缓存」（读坏不写回）。
const CACHE_FILE_VERSION: u64 = 1;
/// 节流窗口：落盘后至少间隔这么久才允许再写，把逐请求的抖动合并掉。
/// 刻意取常量而不加配置项——这不是用户需要调的旋钮。
const MIN_FLUSH_INTERVAL: std::time::Duration = std::time::Duration::from_secs(5);

/// 一次落盘请求的判定结果（纯判据，见 `decide_flush`）。
#[derive(Debug, PartialEq, Eq)]
enum FlushAction {
    /// 可以写盘。
    Write,
    /// 序列化结果与上次落盘（或预热读到的盘上内容）逐字节相同。
    SkipUnchanged,
    /// 盘上内容不可信（读坏）且本进程内存还没更新过：此刻写回等于拿
    /// 「可能无内容的内存」覆盖可能可救的文件。
    SkipUntrustedDisk,
    /// 内容有变化但在节流窗口内（且不是压缩轮的立即刷）：本次跳过，
    /// 更新不会丢——下一次机会重新按内容比对补刷。
    SkipThrottled,
}

/// 写盘判据（纯函数）。四条按序生效，缺一不可：
///
/// 1. `changed == false` → 不写。「与上次落盘逐字节相同就不写」是绝大多数
///    普通轮的实际路径（工具定义通常整轮不变），也是最小写原则。
///    它同时兜住「新进程内存空 + 盘上有内容」的组合：预热把盘上内容读进
///    内存后，内存序列化 == 盘上字节，一个字都不写。
/// 2. `disk_unreadable && !dirty` → 不写（**读坏不写回**）。文件坏 / 不存在 /
///    版本不符时本进程只是「没拿到预热」，内存没动过；写回就是拿可能为空
///    的内存覆盖可能可救的文件。内存一旦更新（dirty），下一次写自然覆盖坏文件。
/// 3. 节流：内容有变化、但上一次落盘尝试距今不足窗口且非压缩轮 → 跳过。
///    刻意以「上次落盘尝试」而非「dirty」为闸：真实部署里 tools 定义很少变，
///    每请求标脏是常态而内容不变是常态，变了才是罕见事件——窗口只可能挡住
///    「一秒内多次定义变更」的病态抖动，挡不住「重启前最后一次变更落盘」，
///    这正是 issue 要的第 4 条保证。进程生命周期内的首次落盘不受窗口约束
///    （新进程没有「上次尝试」可比）。丢更新不存在：跳过只影响本次，
///    后续每次机会都重新按内容比对，内容仍然未落盘就会再判 Write。
/// 4. `force`（压缩轮的立即刷语义）绕过节流——保证「这一轮重启后必须能取到」。
///    但内容未变时 force 也不写（最小写原则优先）。
fn decide_flush(
    changed: bool,
    dirty: bool,
    disk_untrusted: bool,
    window_elapsed: bool,
    force: bool,
) -> FlushAction {
    if !changed {
        FlushAction::SkipUnchanged
    } else if disk_untrusted && !dirty {
        FlushAction::SkipUntrustedDisk
    } else if !force && !window_elapsed {
        FlushAction::SkipThrottled
    } else {
        FlushAction::Write
    }
}

/// 落盘文件的字节形态：`{"version":1,"tools":{key:[...]}}`。
/// 键序取自 BTreeMap，稳定——「与上次落盘逐字节相同」的相等比较依赖这个稳定性。
fn serialize_cache(cache: &BTreeMap<String, Value>) -> Vec<u8> {
    let document = json!({
        "version": CACHE_FILE_VERSION,
        "tools": cache,
    });
    serde_json::to_vec(&document).unwrap_or_default()
}

/// 读盘并解析。返回 None = 没有可用缓存（不存在 / 解析失败 / 版本不符一律同口径）。
/// **只读**：内存不动、文件不动——「读坏不写回」由调用方的 dirty 判据保证，
/// 而不是靠这个函数做特殊处理。
fn load_cache_file(path: &std::path::Path) -> Option<(Vec<u8>, BTreeMap<String, Value>)> {
    let bytes = std::fs::read(path).ok()?;
    let parsed: Value = serde_json::from_slice(&bytes).ok()?;
    if parsed.get("version").and_then(Value::as_u64) != Some(CACHE_FILE_VERSION) {
        return None;
    }
    let entries = parsed.get("tools")?.as_object()?;
    let mut loaded = BTreeMap::new();
    for (key, tools) in entries {
        if tools.is_array() && !key.trim().is_empty() {
            loaded.insert(key.clone(), tools.clone());
        }
    }
    Some((bytes, loaded))
}

/// 把预热内容并进内存缓存：只补缺，不删已有键（本进程写过的优先），
/// 并沿用既有上限策略（超限整体重置，只退回旧行为一次）。
fn merge_warmed(loaded: &BTreeMap<String, Value>) {
    let Ok(mut cache) = CACHE.get_or_init(Default::default).lock() else {
        return;
    };
    for (key, tools) in loaded {
        cache.entry(key.clone()).or_insert_with(|| tools.clone());
    }
    if cache.len() > CACHE_LIMIT {
        cache.clear();
    }
}

/// 落盘层的进程内状态。`enabled=false` 时所有 IO 入口一律早退：
/// 「既有纯内存单测零副作用」与「稳定层开关关闭路径零 IO」都由这一个开关实现。
#[derive(Debug)]
struct PersistState {
    enabled: bool,
    /// enable 时解析一次；测试经 for_tests 钩子指到临时目录。
    path: Option<std::path::PathBuf>,
    /// 预热只读一次文件；此后即便文件被别的东西改坏也不重读（以内存为准）。
    warmed: bool,
    /// 上次**成功落盘**（或预热读到）的字节；None = 还没有可信基线。
    last_flushed: Option<Vec<u8>>,
    /// 预热读失败（不存在 / 坏 / 版本不符）：盘上内容不可信。
    disk_unreadable: bool,
    /// 本进程内存是否更新过（`remember_tools` 置真）。读坏不写回的判据。
    dirty: bool,
    last_flush_attempt: Option<std::time::Instant>,
    /// 预热命中的条数（只进诊断字段，不落正文）。
    warmed_entries: usize,
    /// 本进程最近一次落盘尝试的结果；None = 还没尝试过。
    last_flush_ok: Option<bool>,
}

static PERSIST: OnceLock<Mutex<PersistState>> = OnceLock::new();

fn persist_state() -> &'static Mutex<PersistState> {
    PERSIST.get_or_init(|| Mutex::new(PersistState {
        enabled: false,
        path: None,
        warmed: false,
        last_flushed: None,
        disk_unreadable: false,
        dirty: false,
        last_flush_attempt: None,
        warmed_entries: 0,
        last_flush_ok: None,
    }))
}

/// **加锁纪律（本模块唯一约定处）**：`CACHE` 与 `PERSIST` 两把锁不得同时持有，
/// 也不得持一把再进本模块另一个函数。原因：`remember_tools` 天然先碰 `CACHE`，
/// 落盘又要先读内存后改状态，两处交错取锁就是锁序反转，压缩轮与请求路径并发时
/// 直接死锁（本模块开发时就撞过一次自锁）。改动本模块请维持「单锁短作用域」。
fn cache_snapshot() -> Option<Vec<u8>> {
    CACHE
        .get_or_init(Default::default)
        .lock()
        .ok()
        .map(|cache| serialize_cache(&cache))
}

/// 稳定层开启时打开落盘并按需预热（幂等，进程内只读一次文件）。
///
/// 入参是**开关值而不是读配置**：装配点把 `relay.stabilize_prompt_prefix` 传进来，
/// 开关关闭时直接早退——不 enable、不读盘、不写盘，关闭路径零副作用；本模块因此
/// 仍然不读任何全局配置（纯函数层零配置耦合的纪律不变）。
/// 返回预热命中的条数（0 = 无缓存可用，行为与今天的内存版一致）。
///
/// 读盘失败一律静默降级：不退出、不报错，也不写回覆盖（见 `decide_flush` 第 2 条）。
pub fn prepare_persistence(stabilize_enabled: bool) -> usize {
    if !stabilize_enabled {
        return 0;
    }
    // 段一：决定读不读、读哪个路径（只查询，不在持锁期间做 IO）。
    let path_to_read = {
        let Ok(state) = persist_state().lock() else {
            return 0;
        };
        if state.warmed {
            None
        } else if state.enabled {
            state.path.clone()
        } else {
            Some(crate::paths::default_tools_cache_path())
        }
    };
    // 段二：无锁 IO。
    let disk = path_to_read.as_deref().and_then(load_cache_file);
    // 段三：预热内容并进内存（只持 CACHE；disk 已是 owned 克隆）。
    if let Some((_, loaded)) = disk.as_ref() {
        merge_warmed(loaded);
    }
    // 段四：落状态（只持 PERSIST）。
    let Ok(mut state) = persist_state().lock() else {
        return 0;
    };
    if !state.enabled {
        state.enabled = true;
        if state.path.is_none() {
            state.path = path_to_read.clone();
        }
    }
    if state.warmed {
        return state.warmed_entries;
    }
    state.warmed = true;
    match disk {
        None => {
            state.disk_unreadable = true;
            0
        }
        Some((bytes, loaded)) => {
            state.last_flushed = Some(bytes);
            state.disk_unreadable = false;
            state.dirty = false;
            state.warmed_entries = loaded.len();
            state.warmed_entries
        }
    }
}

/// 普通轮的落盘机会：判据齐全但守节流窗口。返回本次是否真的写了盘。
pub fn flush_to_disk() -> bool {
    flush(false)
}

/// 压缩轮的立即刷：绕过节流，保证「这一轮重启后必须能取到」。
/// 命中与否都刷——回填未命中说明缓存刚被更新过，正是最需要留下的那份。
pub fn flush_immediate() -> bool {
    flush(true)
}

fn flush(force: bool) -> bool {
    // 段一：内存快照（CACHE 作用域内完成序列化）。
    let Some(target) = cache_snapshot() else {
        return false;
    };
    // 段二：判据（只持 PERSIST，且 decide_flush 是纯函数）。
    let (path, action) = {
        let Ok(state) = persist_state().lock() else {
            return false;
        };
        if !state.enabled {
            return false;
        }
        let Some(path) = state.path.clone() else {
            return false;
        };
        // last_flushed 为 None（新进程 / 读坏）时任何内存内容都算「有变化」：
        // 盘上没有可比对的基线。读坏的组合由下一条守卫单独挡。
        let changed = state.last_flushed.as_deref() != Some(target.as_slice());
        let window_elapsed = state
            .last_flush_attempt
            .is_none_or(|previous| previous.elapsed() >= MIN_FLUSH_INTERVAL);
        (
            path,
            decide_flush(
                changed,
                state.dirty,
                state.disk_unreadable,
                window_elapsed,
                force,
            ),
        )
    };
    if action != FlushAction::Write {
        return false;
    }
    // 段三：原子写（不持任何本模块的锁）。
    let result = crate::settings::atomic_write(&path, &target);
    let Ok(mut state) = persist_state().lock() else {
        return result.is_ok();
    };
    state.last_flush_attempt = Some(std::time::Instant::now());
    match result {
        Ok(()) => {
            state.last_flushed = Some(target);
            state.disk_unreadable = false;
            state.dirty = false;
            state.last_flush_ok = Some(true);
        }
        // 写失败保持 dirty，下一次机会再试；不新增事件名（登记表零新增是硬约束），
        // 结果只作为既有事件的附加字段暴露。
        Err(_) => {
            state.last_flush_ok = Some(false);
            return false;
        }
    }
    true
}

/// 落盘层的可观测状态（诊断字段用）。刻意只暴露计数与结果，不暴露路径、
/// 更不暴露缓存内容——文件里是工具 schema，可能含客户端 skill 描述文本，
/// 一个字都不许进诊断日志或监看接口。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PersistenceStatus {
    /// 预热命中的条数（0 = 没有可用缓存，行为同内存版）。
    pub warmed_entries: usize,
    /// 本进程最近一次落盘尝试是否成功；None = 还没尝试过（开关关闭时恒为 None）。
    pub last_flush_ok: Option<bool>,
}

/// 读落盘层状态。幂等、无 IO、不改状态。
pub fn persistence_status() -> PersistenceStatus {
    let Ok(state) = persist_state().lock() else {
        return PersistenceStatus { warmed_entries: 0, last_flush_ok: None };
    };
    PersistenceStatus {
        warmed_entries: state.warmed_entries,
        last_flush_ok: state.last_flush_ok,
    }
}

fn cache_key(body: &Value) -> Option<String> {
    body.get("prompt_cache_key")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

/// 普通请求（非压缩轮）记录 tools 序列化，供后续压缩轮回填。
pub fn remember_tools(request: &Value) {
    let Some(key) = cache_key(request) else {
        return;
    };
    let Some(tools) = request
        .get("tools")
        .filter(|tools| tools.is_array())
        .cloned()
    else {
        return;
    };
    {
        let Ok(mut cache) = CACHE.get_or_init(Default::default).lock() else {
            return;
        };
        if cache.len() >= CACHE_LIMIT && !cache.contains_key(&key) {
            cache.clear();
        }
        cache.insert(key, tools);
    }
    // 内存更新即标脏；写不写由 `decide_flush` 判（逐字节相同不写、没更新不写、
    // 节流合并）。这个每请求路径不做同步写盘。标脏放在 CACHE 锁释放之后——
    // 见本模块「加锁纪律」。
    if let Ok(mut state) = persist_state().lock() {
        state.dirty = true;
    }
}

/// 压缩请求取回该会话最近一次普通请求的 tools；无缓存返回 None（回退旧行为）。
/// 进程内首次用到缓存之前，装配点会先 `prepare_persistence(开关)` 预热，
/// 重启后的读取路径即预热路径。
pub fn preserved_tools(request: &Value) -> Option<Value> {
    let key = cache_key(request)?;
    let cache = CACHE.get_or_init(Default::default).lock().ok()?;
    cache.get(&key).cloned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;




    #[test]
    fn remember_then_preserved_round_trips_exact_bytes() {
        let request = json!({
            "prompt_cache_key": "thread-unit-1",
            "tools": [{"type": "function", "name": "exec_command", "parameters": {"type": "object"}}],
        });
        remember_tools(&request);
        let preserved = preserved_tools(&request).expect("回填必须命中");
        assert_eq!(
            serde_json::to_string(&preserved).unwrap(),
            serde_json::to_string(&request["tools"]).unwrap(),
            "回填序列化必须与普通请求逐字节一致"
        );
    }

    #[test]
    fn remember_ignores_missing_or_non_array_tools() {
        let request = json!({"prompt_cache_key": "thread-unit-2"});
        remember_tools(&request);
        assert!(preserved_tools(&request).is_none());
        let request2 = json!({"prompt_cache_key": "thread-unit-2", "tools": {"weird": true}});
        remember_tools(&request2);
        assert!(preserved_tools(&request2).is_none());
    }

    #[test]
    fn preserved_requires_known_key() {
        let compaction = json!({"prompt_cache_key": "thread-unit-never-seen", "tools": []});
        assert!(preserved_tools(&compaction).is_none());
    }

    #[test]
    fn later_normal_request_overwrites_cached_tools() {
        let first = json!({
            "prompt_cache_key": "thread-unit-3",
            "tools": [{"type": "function", "name": "old_tool"}],
        });
        remember_tools(&first);
        let second = json!({
            "prompt_cache_key": "thread-unit-3",
            "tools": [{"type": "function", "name": "new_tool"}],
        });
        remember_tools(&second);
        let preserved = preserved_tools(&second).expect("回填必须命中最新一轮");
        assert_eq!(preserved[0]["name"], json!("new_tool"));
    }

    // ---------- 落盘层（issue #69 第 6 条）----------
    //
    // 判据一律走纯函数对拍：`CACHE` / `PERSIST` 都是进程级全局，而 cargo 默认
    // 并行跑测试——别的模块（compaction_align、protocol_proxy）随时会往内存缓存
    // 里写键，任何「字节级」断言在单测里都不可重现。跨进程的真实链路交给
    // `tests/tools_cache_persistence.rs` 用两个真实进程覆盖。

    fn cache_with(pairs: &[(&str, &str)]) -> BTreeMap<String, Value> {
        pairs
            .iter()
            .map(|(key, tool)| {
                (
                    (*key).to_string(),
                    json!([{"type": "function", "function": {"name": tool}}]),
                )
            })
            .collect()
    }

    #[test]
    fn serialization_is_key_ordered_and_byte_stable() {
        // 「与上次落盘逐字节相同就不写」的地基：同一内容两次序列化必须逐字节相同，
        // 且与插入顺序无关（BTreeMap 键序）。
        let a = cache_with(&[("z-key", "z_tool"), ("a-key", "a_tool")]);
        let b = cache_with(&[("a-key", "a_tool"), ("z-key", "z_tool")]);
        let bytes_a = serialize_cache(&a);
        assert_eq!(bytes_a, serialize_cache(&b), "键序必须与插入顺序无关");
        assert_eq!(bytes_a, serialize_cache(&a), "同一内容必须逐字节稳定");
        let text = String::from_utf8(bytes_a).unwrap();
        assert!(text.contains(r#""version":1"#), "文件必须带版本号");
        assert!(
            text.find("a-key").unwrap() < text.find("z-key").unwrap(),
            "键必须按字典序落盘"
        );
    }

    #[test]
    fn load_accepts_only_matching_version_arrays_and_never_touches_the_file() {
        let dir = tempfile::tempdir().unwrap();
        // 好文件：读回内容与字节原样。
        let good = dir.path().join("good.json");
        let payload = serialize_cache(&cache_with(&[("k1", "tool_one")]));
        std::fs::write(&good, &payload).unwrap();
        let (bytes, loaded) = load_cache_file(&good).expect("好文件必须读得出来");
        assert_eq!(bytes, payload, "读回的字节必须与落盘字节一致（相等判据的地基）");
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded["k1"][0]["function"]["name"], json!("tool_one"));

        // 三种坏态一律 None，且**文件一个字节都不许变**（读坏不写回的前半段）。
        let cases = [
            ("corrupt.json", Some("not json {{{"), "内容损坏"),
            ("version.json", Some(r#"{"version":99,"tools":{"k":[]}}"#), "版本不符"),
            ("shape.json", Some(r#"{"version":1,"tools":"nope"}"#), "形态不对"),
        ];
        for (name, content, label) in cases {
            let path = dir.path().join(name);
            std::fs::write(&path, content.unwrap()).unwrap();
            assert!(load_cache_file(&path).is_none(), "{label} 必须降级成无缓存");
            assert_eq!(
                std::fs::read(&path).unwrap(),
                content.unwrap().as_bytes(),
                "{label} 的原文件必须原样保留，等下一次正常写自然覆盖"
            );
        }
        // 不存在：同口径 None，且不许凭空造出文件。
        let missing = dir.path().join("missing.json");
        assert!(load_cache_file(&missing).is_none(), "文件不存在必须降级成无缓存");
        assert!(!missing.exists(), "只读函数不得造出文件");
    }

    #[test]
    fn flush_decision_matrix_pins_all_four_guards() {
        // 正判据：内容变了、盘可信、窗口已过 → 写（普通轮的常规机会）。
        assert_eq!(
            decide_flush(true, true, false, true, false),
            FlushAction::Write,
            "内容有变化的常规机会必须放行"
        );
        // 守卫 1：内容未变。绝大多数普通轮的实际路径，一个字都不许写；
        // 也兜住「新进程预热后内存 == 盘上内容」的组合。force 也不例外。
        assert_eq!(
            decide_flush(false, true, false, true, false),
            FlushAction::SkipUnchanged,
            "与上次落盘逐字节相同就不写"
        );
        assert_eq!(
            decide_flush(false, true, false, true, true),
            FlushAction::SkipUnchanged,
            "无变化就不写，压缩轮 force 也不例外（最小写原则）"
        );
        // 守卫 2：读坏不写回。盘不可信且内存没更新过——最危险的组合是
        // 「文件坏了（读不出）+ 内存是空的」：changed 可能为真，也必须挡住。
        assert_eq!(
            decide_flush(true, false, true, true, false),
            FlushAction::SkipUntrustedDisk,
            "盘不可信且内存未更新时，坏文件必须留给下一次正常写覆盖"
        );
        // 内存更新之后（dirty），同一组合必须放行——坏文件被下一次正常写覆盖。
        assert_eq!(
            decide_flush(true, true, true, true, false),
            FlushAction::Write,
            "内存变化后的正常写必须能覆盖坏文件"
        );
        // 守卫 3：节流窗口。只按时间与 force 分流；「进程生命周期内首次
        // 落盘」由调用方以 window_elapsed=true 表达（没有上次尝试可比）。
        assert_eq!(
            decide_flush(true, true, false, false, false),
            FlushAction::SkipThrottled,
            "窗口内的重复变更不得逐请求写盘"
        );
        // 守卫 4：压缩轮立即刷绕过节流。
        assert_eq!(
            decide_flush(true, true, false, false, true),
            FlushAction::Write,
            "压缩轮必须能立即刷，保证重启后取到最近一轮"
        );
    }

    #[test]
    fn disabled_or_closed_paths_stay_free_of_io() {
        // 开关关闭：prepare 直接返回 0，不 enable、不读盘、不写盘。
        assert_eq!(
            prepare_persistence(false),
            0,
            "稳定层关闭时不得预热，也不得打开落盘"
        );
        let status = persistence_status();
        assert_eq!(status.warmed_entries, 0, "未预热不得有命中计数");
        assert!(!flush_to_disk(), "未 enable 时不得写盘");
        assert!(!flush_immediate(), "未 enable 时立即刷也不得写盘");
        assert_eq!(
            persistence_status(),
            status,
            "整条链路早退，状态不得被改动"
        );
    }
}
