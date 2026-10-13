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

use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use tokio::sync::watch;

/// 「尚未收到退出信号」的哨兵截止点。`tokio::time::sleep_until` 到该时刻
/// 等价于永不超时（远超任何进程寿命），在途流因此可以无差别地只等截止点，
/// 不必区分「有没有开始排空」两种形态。
pub const NO_DRAIN: Instant = Instant::MAX;

/// 硬上界相对收口截止点的额外宽限。到点终止属于兜底路径（个别连接不读字节、
/// 非流式分支卡在读上游整包等），给正常收口留足表演时间后才轮到它。
pub const FORCE_EXIT_GRACE: Duration = Duration::from_secs(10);

/// 关停排空状态。廉价克隆，随请求分发链注入。
#[derive(Clone)]
pub struct DrainState {
    deadline: watch::Sender<Instant>,
}

impl Default for DrainState {
    fn default() -> Self {
        Self::new()
    }
}

impl DrainState {
    /// 初始为「未排空」：所有截止点等待都不生效。
    pub fn new() -> Self {
        let (sender, _) = watch::channel(NO_DRAIN);
        Self { deadline: sender }
    }

    /// 记录退出信号并设定收口截止点。**幂等**：以最先收到的信号为准
    /// （launchd 可能对同一进程重复投递 SIGTERM，二次信号不得把截止点推远）。
    /// 返回实际生效的截止点。
    pub fn begin(&self, signal_at: Instant, drain_seconds: u64) -> Instant {
        let candidate = signal_at + Duration::from_secs(drain_seconds);
        self.deadline.send_if_modified(|current| {
            if *current == NO_DRAIN {
                *current = candidate;
                true
            } else {
                false
            }
        });
        // send_if_modified 无论是否发送都要求返回 bool 表示「是否修改」；
        // 未修改时接收端不会收到唤醒，但首次 begin 之后值已就位，
        // 后到的 begin 读到的当前值即生效值。
        *self.deadline.borrow()
    }

    /// 当前生效的收口截止点（哨兵表示未排空）。
    pub fn deadline(&self) -> Instant {
        *self.deadline.borrow()
    }

    /// 是否已收到退出信号。
    pub fn began(&self) -> bool {
        self.deadline() != NO_DRAIN
    }

    /// 等待到收口截止点。未排空时永挂；晚创建的订阅者立即读到已生效的截止点。
    pub async fn wait_deadline(&self) {
        let mut rx = self.deadline.subscribe();
        loop {
            let deadline = *rx.borrow_and_update();
            tokio::select! {
                _ = tokio::time::sleep_until(tokio::time::Instant::from_std(deadline)) => return,
                changed = rx.changed() => {
                    if changed.is_err() {
                        // 发送端已释放（进程收尾）：按已到点处理。
                        return;
                    }
                }
            }
        }
    }

    /// 等待进程级硬上界（收口截止点 + 固定宽限）。未排空时永挂。
    pub async fn wait_force_exit(&self) {
        let mut rx = self.deadline.subscribe();
        loop {
            let deadline = *rx.borrow_and_update();
            if deadline != NO_DRAIN {
                tokio::time::sleep_until(
                    tokio::time::Instant::from_std(deadline + FORCE_EXIT_GRACE),
                )
                .await;
                return;
            }
            if rx.changed().await.is_err() {
                return;
            }
        }
    }
}

/// 流组合器：排空截止点一到，立刻停止转发原流，下发一帧 `terminal_frame`
/// （可判定的终止事件）后正常收尾。未排空时行为与原流完全一致。
///
/// 约束：`terminal_frame` 必须是终止语义（例如显式失败帧），绝不能是
/// 「看起来成功」的收尾帧（`[DONE]`、完成事件等）——这是 issue #34 的红线。
pub fn terminate_at_deadline<S, F>(upstream: S, drain: DrainState, terminal_frame: F) -> TerminateAtDeadline<S>
where
    S: futures_util::Stream + Unpin,
    F: FnOnce() -> S::Item,
{
    TerminateAtDeadline {
        upstream,
        drain,
        terminal_frame: Some(Box::new(terminal_frame) as Box<dyn FnOnce() -> S::Item + Send>),
        phase: TerminatePhase::Running,
    }
}

type TerminalFrameFn<I> = Box<dyn FnOnce() -> I + Send>;

enum TerminatePhase<I> {
    Running,
    /// 补帧只发一次；原流的尾部残留不再有意义，直接丢弃。
    Flush(Option<TerminalFrameFn<I>>),
    Done,
}

/// `terminate_at_deadline` 的返回流。轮询顺序：先抢查原流的就绪（同一轮里
/// 原流恰好结束时不再叠补帧），未就绪才与截止点对赌。
pub struct TerminateAtDeadline<S: futures_util::Stream + Unpin> {
    upstream: S,
    drain: DrainState,
    terminal_frame: Option<TerminalFrameFn<S::Item>>,
    phase: TerminatePhase<S::Item>,
}

impl<S> futures_util::Stream for TerminateAtDeadline<S>
where
    S: futures_util::Stream + Unpin,
{
    type Item = S::Item;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        loop {
            match &mut this.phase {
                TerminatePhase::Running => {
                    if let Poll::Ready(item) = this.upstream.poll_next_unpin(cx) {
                        return Poll::Ready(item);
                    }
                    if this.drain.began() {
                        this.phase = TerminatePhase::Flush(this.terminal_frame.take());
                        continue;
                    }
                    return Poll::Pending;
                }
                TerminatePhase::Flush(frame) => {
                    // 原流已停。等最后一帧被消费掉再落 Done，保证补帧必然下发。
                    if let Poll::Ready(_) = this.upstream.poll_next_unpin(cx) {
                        // 排空期间原流若恰好吐出一帧，仍优先原帧（终止帧下一轮补）。
                        // 该分支极少触发，不构成语义分叉。
                    }
                    let Some(factory) = frame.take() else {
                        this.phase = TerminatePhase::Done;
                        continue;
                    };
                    this.phase = TerminatePhase::Done;
                    return Poll::Ready(Some(factory()));
                }
                TerminatePhase::Done => return Poll::Ready(None),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::StreamExt;

    #[test]
    fn new_state_is_not_draining() {
        let drain = DrainState::new();
        assert!(!drain.began());
        assert_eq!(drain.deadline(), NO_DRAIN);
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
        assert_eq!(elapsed, Duration::from_secs(20));
    }

    #[tokio::test(start_paused = true)]
    async fn late_subscriber_sees_effective_deadline_immediately_when_passed() {
        let drain = DrainState::new();
        // 模拟「截止点已过才发现的晚订阅者」：begin 用过去的虚拟时刻。
        let past = Instant::now() - Duration::from_secs(1);
        drain.begin(past, 0);
        tokio::time::timeout(Duration::from_secs(1), drain.wait_deadline())
            .await
            .expect("截止点已过的订阅者必须立即返回");
    }

    #[tokio::test(start_paused = true)]
    async fn force_exit_is_deadline_plus_grace() {
        let drain = DrainState::new();
        drain.begin(Instant::now(), 30);
        let started = tokio::time::Instant::now();
        drain.wait_force_exit().await;
        assert_eq!(started.elapsed(), Duration::from_secs(30) + FORCE_EXIT_GRACE);
    }

    async fn slow_stream(chunks: usize, gap: Duration) -> impl futures_util::Stream<Item = &'static str> {
        futures_util::stream::unfold(0usize, move |mut n| async move {
            if n >= chunks {
                return None;
            }
            tokio::time::sleep(gap).await;
            n += 1;
            Some((("chunk"), n))
        })
    }

    #[tokio::test(start_paused = true)]
    async fn combinator_passthrough_when_not_draining() {
        let drain = DrainState::new();
        let stream = terminate_at_deadline(slow_stream(3, Duration::from_secs(1)), drain, || "terminal");
        let items: Vec<_> = stream.collect().await;
        assert_eq!(items, vec!["chunk", "chunk", "chunk"]);
    }

    #[tokio::test(start_paused = true)]
    async fn combinator_terminates_once_at_deadline() {
        let drain = DrainState::new();
        let stream = terminate_at_deadline(slow_stream(100, Duration::from_secs(1)), drain.clone(), || "terminal");
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
    }

    #[tokio::test(start_paused = true)]
    async fn combinator_flushes_terminal_when_deadline_already_past() {
        let drain = DrainState::new();
        drain.begin(Instant::now() - Duration::from_secs(1), 0);
        let stream = terminate_at_deadline(slow_stream(100, Duration::from_secs(1)), drain, || "terminal");
        let items: Vec<_> = tokio::time::timeout(Duration::from_secs(5), stream.collect::<Vec<_>>())
            .await
            .expect("已过截止点必须立刻收口");
        assert_eq!(items, vec!["terminal"]);
    }
}
