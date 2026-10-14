//! 关停排空（drain）状态：退出信号与在途流的收口上界。
//!
//! 语义分层（issue #34 期望行为 1/2）：
//! 1. 收到退出信号 → `axum` 停止接收新请求（`with_graceful_shutdown` 的既有语义）；
//! 2. 同时在途流拿到一个**收口截止点**：到点仍未自然结束的分支必须停止拉上游、
//!    补一帧可判定的终止事件后收口——不得静默截断成「看起来成功」；
//! 3. 截止点再往一个固定宽限（`FORCE_EXIT_GRACE`）是**进程级硬上界**：HTTP 服务层
//!    到点直接退出，保证监听口释放时间有界，不受个别卡死连接摆布。
//!
//! 本模块只携带同步状态与流组合器，不读任何全局配置；上界时长由装配层
//! （`main.rs` / `config`）注入，测试直接构造 `DrainState` 对拍。

use std::future::Future;
use std::pin::Pin;
use std::time::{Duration, Instant};

use futures_util::StreamExt;
use tokio::sync::watch;

// 内部约定：截止点用 `Option<Instant>` 表示，`None` = 尚未收到退出信号。
// 不得拿「无穷远的 Instant」当哨兵去 `sleep_until`：距离远超 tokio 定时器
// 安全上限，会直接 panic；永挂语义一律走 `None` 分支。

/// 硬上界相对收口截止点的额外宽限。到点终止属于兜底路径（个别连接不读字节、
/// 非流式分支卡在读上游整包等），给正常收口留足表演时间后才轮到它。
pub const FORCE_EXIT_GRACE: Duration = Duration::from_secs(10);

/// `begin` 对排空秒数的封顶（1 年）。防的是配置事故把截止点算溢出，
/// 不是给运维的调优空间；真实上界量级在几十秒。
const DRAIN_SECONDS_CAP: u64 = 365 * 24 * 3600;

/// 关停排空状态。廉价克隆，随请求分发链注入。
#[derive(Clone)]
pub struct DrainState {
    deadline: watch::Sender<Option<Instant>>,
}

impl Default for DrainState {
    fn default() -> Self {
        Self::new()
    }
}

impl DrainState {
    /// 初始为「未排空」：所有截止点等待都不生效。
    pub fn new() -> Self {
        let (sender, _) = watch::channel(None);
        Self { deadline: sender }
    }

    /// 记录退出信号并设定收口截止点。**幂等**：以最先收到的信号为准
    /// （launchd 可能对同一进程重复投递 SIGTERM，二次信号不得把截止点推远）。
    /// 返回实际生效的截止点。
    pub fn begin(&self, signal_at: Instant, drain_seconds: u64) -> Instant {
        let candidate = signal_at + Duration::from_secs(drain_seconds.min(DRAIN_SECONDS_CAP));
        self.deadline.send_if_modified(|current| {
            if current.is_none() {
                *current = Some(candidate);
                true
            } else {
                false
            }
        });
        // 未修改时接收端不会收到唤醒，但首次 begin 之后值已就位，
        // 后到的 begin 读到的当前值即生效值。
        self.deadline.borrow().expect("begin 之后截止点必然就位")
    }

    /// 当前生效的收口截止点（`None` 表示未排空）。
    pub fn deadline(&self) -> Option<Instant> {
        *self.deadline.borrow()
    }

    /// 是否已收到退出信号。
    pub fn began(&self) -> bool {
        self.deadline().is_some()
    }

    /// 等待到收口截止点。未排空时永挂；晚创建的订阅者立即读到已生效的截止点
    /// （已过点则立即返回）。
    pub async fn wait_deadline(&self) {
        let mut rx = self.deadline.subscribe();
        wait_offset(&mut rx, Duration::ZERO).await;
    }

    /// 等待进程级硬上界（收口截止点 + 固定宽限）。未排空时永挂。
    pub async fn wait_force_exit(&self) {
        let mut rx = self.deadline.subscribe();
        wait_offset(&mut rx, FORCE_EXIT_GRACE).await;
    }

    /// `'static` 的截止点等待器：自持订阅，可被装箱长期持有（流组合器用）。
    /// 必须额外攥住一份 `DrainState` 克隆：watch 的接收端不保发送端存活，
    /// 发送端一旦被释放，`changed()` 立即报错，等待会被误判成「已到点」。
    pub fn deadline_waiter(&self) -> impl Future<Output = ()> + Send + 'static {
        let keep_alive = self.clone();
        let mut rx = keep_alive.deadline.subscribe();
        async move {
            wait_offset(&mut rx, Duration::ZERO).await;
            // 仅为语义清晰：future 结束前 `keep_alive` 必须持续存活。
            drop(keep_alive);
        }
    }
}

/// 截止点 + `offset` 的有界等待。实现纪律：
/// - `None`（未排空）只配永挂，绝不进定时器；
/// - 剩余时长用 `saturating_duration_since` 计算，已过点即零时长；
/// - 与 `changed()` 对赌，发送端释放（进程收尾）按已到点处理。
async fn wait_offset(rx: &mut watch::Receiver<Option<Instant>>, offset: Duration) {
    loop {
        let deadline = *rx.borrow_and_update();
        let Some(deadline) = deadline else {
            if rx.changed().await.is_err() {
                return;
            }
            continue;
        };
        let target = deadline + offset;
        let remaining = target.saturating_duration_since(Instant::now());
        tokio::select! {
            _ = tokio::time::sleep(remaining) => return,
            changed = rx.changed() => {
                if changed.is_err() {
                    return;
                }
                // 值只会从 None 推进到真实截止点（begin 幂等），重算一轮即收敛。
            }
        }
    }
}

/// 流组合器：排空截止点一到，立刻停止转发原流，下发一帧 `terminal_frame`
/// （可判定的终止事件）后正常收尾。未排空时行为与原流完全一致；原流自然
/// 结束时**不**补帧（自然收尾优先，截止点只兜「跑不完」的底）。
///
/// 约束：`terminal_frame` 必须是终止语义（例如显式失败帧），绝不能是
/// 「看起来成功」的收尾帧（`[DONE]`、完成事件等）——这是 issue #34 的红线。
///
/// 唤醒来源是内部的 `wait_deadline`：上游完全停摆（正是排空要处理的场景）
/// 时原流不产出任何唤醒，截止点的定时器负责把任务叫醒补帧。
pub fn terminate_at_deadline<S, F>(
    upstream: S,
    drain: DrainState,
    terminal_frame: F,
) -> impl futures_util::Stream<Item = S::Item> + Send
where
    S: futures_util::Stream + Send + 'static,
    S::Item: Send,
    F: FnOnce() -> S::Item + Send + 'static,
{
    let state = CombState {
        // 内部装箱固定地址：调用方不必提供 Unpin，测试里的 unfold 流
        // 这类自引用形态也能直接进来。
        upstream: Box::pin(upstream),
        deadline: Box::pin(drain.deadline_waiter()),
        frame: Some(Box::new(terminal_frame)),
    };
    futures_util::stream::unfold(state, |mut state| async move {
        // frame 已取走 = 补帧已下发且被消费：流到此为止。
        if state.frame.is_none() {
            return None;
        }
        tokio::select! {
            item = state.upstream.next() => match item {
                Some(item) => Some((item, state)),
                // 原流自然结束：不补帧，直接收尾。
                None => None,
            },
            () = state.deadline.as_mut() => {
                let factory = state.frame.take().expect("上面判过 Some");
                // 补帧下发后 state 随下一轮的 frame.is_none() 检查一起丢弃，
                // 原流的未读残留不再有意义。
                Some((factory(), state))
            }
        }
    })
}

struct CombState<S: futures_util::Stream> {
    upstream: Pin<Box<S>>,
    deadline: Pin<Box<dyn Future<Output = ()> + Send>>,
    frame: Option<Box<dyn FnOnce() -> S::Item + Send>>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::StreamExt;

    #[test]
    fn new_state_is_not_draining() {
        let drain = DrainState::new();
        assert!(!drain.began());
        assert_eq!(drain.deadline(), None);
    }

    #[test]
    fn begin_sets_deadline_and_is_idempotent() {
        let drain = DrainState::new();
        let base = Instant::now();
        let first = drain.begin(base, 45);
        assert_eq!(first, base + Duration::from_secs(45));
        assert!(drain.began());
        // 重复信号（launchd 的二次 SIGTERM）不得把截止点推远。
        let later = drain.begin(base + Duration::from_secs(5), 45);
        assert_eq!(later, first);
    }

    #[test]
    fn begin_caps_absurd_drain_values_without_overflow() {
        let drain = DrainState::new();
        let base = Instant::now();
        let deadline = drain.begin(base, u64::MAX);
        assert_eq!(deadline, base + Duration::from_secs(DRAIN_SECONDS_CAP));
    }

    #[tokio::test(start_paused = true)]
    async fn wait_deadline_fires_at_boundary() {
        let drain = DrainState::new();
        let base = Instant::now();
        drain.begin(base, 30);
        let started = tokio::time::Instant::now();
        drain.wait_deadline().await;
        assert_eq!(started.elapsed(), Duration::from_secs(30));
    }

    #[tokio::test(start_paused = true)]
    async fn wait_deadline_created_before_begin_wakes_on_begin() {
        let drain = DrainState::new();
        let waiter = {
            let drain = drain.clone();
            tokio::spawn(async move {
                let started = tokio::time::Instant::now();
                drain.wait_deadline().await;
                started.elapsed()
            })
        };
        tokio::time::sleep(Duration::from_secs(10)).await;
        drain.begin(Instant::now(), 20);
        let elapsed = waiter.await.unwrap();
        // 等待者在 begin 前 10 秒就已挂上：总等待 = 10 + 20。
        assert_eq!(elapsed, Duration::from_secs(30));
    }

    #[tokio::test(start_paused = true)]
    async fn late_subscriber_with_passed_deadline_returns_immediately() {
        let drain = DrainState::new();
        drain.begin(Instant::now(), 0);
        // 让「已过点」坐实：虚拟时钟再走一步。
        tokio::time::sleep(Duration::from_secs(3)).await;
        tokio::time::timeout(Duration::from_secs(1), drain.wait_deadline())
            .await
            .expect("已过截止点的晚订阅者必须立即返回");
    }

    #[tokio::test(start_paused = true)]
    async fn force_exit_is_deadline_plus_grace() {
        let drain = DrainState::new();
        drain.begin(Instant::now(), 30);
        let started = tokio::time::Instant::now();
        drain.wait_force_exit().await;
        assert_eq!(
            started.elapsed(),
            Duration::from_secs(30) + FORCE_EXIT_GRACE
        );
    }

    fn slow_stream(chunks: usize, gap: Duration) -> impl futures_util::Stream<Item = &'static str> {
        futures_util::stream::unfold(0usize, move |mut n| async move {
            if n >= chunks {
                return None;
            }
            tokio::time::sleep(gap).await;
            n += 1;
            Some(("chunk", n))
        })
    }

    #[tokio::test(start_paused = true)]
    async fn combinator_passthrough_when_not_draining() {
        let drain = DrainState::new();
        let stream =
            terminate_at_deadline(slow_stream(3, Duration::from_secs(1)), drain, || "terminal");
        let items: Vec<_> = stream.collect().await;
        assert_eq!(items, vec!["chunk", "chunk", "chunk"]);
    }

    #[tokio::test(start_paused = true)]
    async fn combinator_keeps_flowing_until_deadline_not_at_signal() {
        // 信号一到就收口是错误语义（第一版实现的缺陷）：排空的意义正是
        // 让在途流在上界内自然跑完。截止前必须持续转发原流。
        let drain = DrainState::new();
        let stream = terminate_at_deadline(
            slow_stream(100, Duration::from_secs(1)),
            drain.clone(),
            || "terminal",
        );
        let collector = tokio::spawn(stream.collect::<Vec<_>>());
        drain.begin(Instant::now(), 8);
        // 信号后 6 秒（截止点之前）：必须还在转发原流，没有补终止帧收口。
        tokio::time::sleep(Duration::from_secs(6)).await;
        assert!(!collector.is_finished());
    }

    #[tokio::test(start_paused = true)]
    async fn combinator_terminates_once_at_deadline() {
        let drain = DrainState::new();
        let stream = terminate_at_deadline(
            slow_stream(100, Duration::from_secs(1)),
            drain.clone(),
            || "terminal",
        );
        let collector = tokio::spawn(stream.collect::<Vec<_>>());
        // 排空 5 秒后开始：前 5 个 chunk 已下发，第 6 秒起补终止帧并收尾。
        drain.begin(Instant::now(), 5);
        let items = tokio::time::timeout(Duration::from_secs(60), collector)
            .await
            .expect("组合器必须在截止点收口，不得永挂")
            .unwrap();
        let last = items.last().copied().expect("至少有一帧");
        assert_eq!(last, "terminal", "最后一帧必须是可判定的终止事件");
        // 终止帧只补一次：除终止帧外全部是原流数据。
        assert_eq!(items.iter().filter(|item| **item == "terminal").count(), 1);
        assert!(items.len() >= 5, "截止点前的原流帧不得丢失");
    }

    #[tokio::test(start_paused = true)]
    async fn combinator_flushes_terminal_when_deadline_already_past() {
        let drain = DrainState::new();
        drain.begin(Instant::now(), 0);
        tokio::time::sleep(Duration::from_secs(1)).await;
        let stream =
            terminate_at_deadline(slow_stream(100, Duration::from_secs(1)), drain, || "terminal");
        let items: Vec<_> = tokio::time::timeout(Duration::from_secs(5), stream.collect::<Vec<_>>())
            .await
            .expect("已过截止点必须立刻收口");
        assert_eq!(items, vec!["terminal"]);
    }

    #[tokio::test(start_paused = true)]
    async fn combinator_natural_end_never_appends_terminal() {
        // 原流在截止点前自然结束：不得画蛇添足补失败帧（把成功说成失败
        // 同样是「不可判定的终止」的一种）。
        let drain = DrainState::new();
        let stream =
            terminate_at_deadline(slow_stream(3, Duration::from_secs(1)), drain.clone(), || {
                "terminal"
            });
        let collector = tokio::spawn(stream.collect::<Vec<_>>());
        drain.begin(Instant::now(), 30);
        let items = tokio::time::timeout(Duration::from_secs(5), collector)
            .await
            .expect("自然结束的流不得被排空状态拖住")
            .unwrap();
        assert_eq!(items, vec!["chunk", "chunk", "chunk"]);
    }

    #[tokio::test(start_paused = true)]
    async fn combinator_terminates_stalled_upstream_on_time() {
        // 上游彻底停摆（不再产出也不关闭）：唤醒必须来自截止点定时器。
        let drain = DrainState::new();
        let stalled = futures_util::stream::pending::<&'static str>();
        let stream = terminate_at_deadline(stalled, drain.clone(), || "terminal");
        let collector = tokio::spawn(stream.collect::<Vec<_>>());
        drain.begin(Instant::now(), 7);
        let items = tokio::time::timeout(Duration::from_secs(60), collector)
            .await
            .expect("停摆流必须在截止点被定时器收口")
            .unwrap();
        assert_eq!(items, vec!["terminal"]);
    }
}
