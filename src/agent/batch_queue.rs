//! 三中台机制式排队合并（v0.4.7）：状态驱动、无定时窗口。
//!
//! 用户决策语义落点（主会话授权裁决，见任务书 §2.2）：
//! - **批 = 连续处理组**：批内逐轮处理（每轮独立 LLM 调用、独立产物、独立触发下游），
//!   合并只发生在**消息接入层**——处理中到达的飞行消息不中断当前轮、批完成后立即作为
//!   下一批连续处理（消除队列空隙），而非强行多轮合并单次 LLM（每轮产物必须独立对应下游）；
//! - 状态驱动：空闲收到第一条 → 开批立即处理；处理中到达 → 飞行缓冲；批完成 → 飞行缓冲
//!   非空立即下一批，空则回空闲等待。无定时窗口、无额外延迟。
//!
//! `len()` = **待处理消息数**口径：飞行缓冲条数 + 当前批（处理中）条数——所有已入队
//! 未完成的消息。状态栏（memory_pending / execution_pending / insight_pending）按此
//! 口径发布，长跑积压随处理连续推进而回稳（不再出现「积压恒高」）。

use std::collections::VecDeque;
use tokio::sync::mpsc;

/// 批次队列状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BatchQueueState {
    /// 空闲：等待第一条消息开批。
    Idle,
    /// 批处理中：新消息进入飞行缓冲。
    Processing,
}

/// `push` 结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PushOutcome {
    /// 空闲 → 开批：该消息成为当前批首条，调用方应立即取出并处理。
    StartedBatch,
    /// 处理中 → 已进入飞行缓冲（下一批）。
    Buffered,
}

/// 机制式排队合并状态机（批 = 连续处理组）。
pub struct PendingBatchQueue<T> {
    state: BatchQueueState,
    /// 当前批（处理中）——开批后由调用方 `take_current_batch` 取出逐轮处理。
    current: Vec<T>,
    /// 飞行缓冲：处理中到达的消息，批完成后整体成为下一批。
    flight: VecDeque<T>,
}

impl<T> Default for PendingBatchQueue<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T> PendingBatchQueue<T> {
    pub fn new() -> Self {
        Self {
            state: BatchQueueState::Idle,
            current: Vec::new(),
            flight: VecDeque::new(),
        }
    }

    pub fn state(&self) -> BatchQueueState {
        self.state
    }

    /// 入队：
    /// - 空闲 → 开批（当前批 = [item]，状态 Processing），返回 `StartedBatch`；
    /// - 处理中 → 进入飞行缓冲，返回 `Buffered`。
    pub fn push(&mut self, item: T) -> PushOutcome {
        match self.state {
            BatchQueueState::Idle => {
                self.state = BatchQueueState::Processing;
                self.current.push(item);
                PushOutcome::StartedBatch
            }
            BatchQueueState::Processing => {
                self.flight.push_back(item);
                PushOutcome::Buffered
            }
        }
    }

    /// 取出当前批（开批后、处理前调用；取出后当前批为空，状态保持 Processing）。
    pub fn take_current_batch(&mut self) -> Vec<T> {
        std::mem::take(&mut self.current)
    }

    /// 批处理完成：
    /// - 飞行缓冲非空 → 整体成为下一批（状态保持 Processing），返回 `true`（继续处理）；
    /// - 空 → 回空闲等待，返回 `false`。
    pub fn on_batch_finished(&mut self) -> bool {
        if self.flight.is_empty() {
            self.state = BatchQueueState::Idle;
            false
        } else {
            self.current = self.flight.drain(..).collect();
            true
        }
    }

    /// 待处理消息数 = 飞行缓冲条数 + 当前批条数（已入队未完成）。
    pub fn len(&self) -> usize {
        self.current.len() + self.flight.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// 取下一批：当前批非空（上轮 `on_batch_finished` 已把飞行缓冲转入）→ 直接取出；
    /// 否则空闲阻塞等 `rx` 第一条开批。`rx` 关闭 → `None`（结束）。
    pub async fn next_batch(&mut self, rx: &mut mpsc::Receiver<T>) -> Option<Vec<T>> {
        if self.state == BatchQueueState::Processing && !self.current.is_empty() {
            return Some(std::mem::take(&mut self.current));
        }
        let first = rx.recv().await?;
        // 空闲 → 开批；防御路径（Processing 且 current 空）→ 也作为当前批首条处理。
        if self.state == BatchQueueState::Idle {
            self.push(first);
        } else {
            self.current.push(first);
        }
        Some(std::mem::take(&mut self.current))
    }

    /// 批处理开始/每轮完成后吸收 channel 存量到飞行缓冲（处理中到达 → 下一批）。
    pub fn absorb_channel(&mut self, rx: &mut mpsc::Receiver<T>) {
        while let Ok(item) = rx.try_recv() {
            self.push(item);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn q() -> PendingBatchQueue<u32> {
        PendingBatchQueue::new()
    }

    #[test]
    fn idle_push_opens_batch_and_starts_processing() {
        let mut queue = q();
        assert_eq!(queue.state(), BatchQueueState::Idle);
        assert!(queue.is_empty());

        let outcome = queue.push(1);
        assert_eq!(outcome, PushOutcome::StartedBatch);
        assert_eq!(queue.state(), BatchQueueState::Processing);
        assert_eq!(queue.len(), 1, "开批后当前批计入待处理");
        assert_eq!(queue.take_current_batch(), vec![1]);
        assert_eq!(queue.len(), 0);
    }

    #[test]
    fn processing_push_buffers_to_flight() {
        let mut queue = q();
        queue.push(1);
        queue.take_current_batch();

        assert_eq!(queue.push(2), PushOutcome::Buffered);
        assert_eq!(queue.push(3), PushOutcome::Buffered);
        assert_eq!(queue.len(), 2, "飞行缓冲计入待处理");
    }

    #[test]
    fn on_batch_finished_with_flight_opens_next_batch_immediately() {
        let mut queue = q();
        queue.push(1);
        queue.take_current_batch();
        queue.push(2);
        queue.push(3);

        // 批 [1] 完成 → 飞行 [2,3] 立即成为下一批（无定时窗口）。
        assert!(queue.on_batch_finished());
        assert_eq!(queue.state(), BatchQueueState::Processing);
        assert_eq!(queue.len(), 2);
        assert_eq!(queue.take_current_batch(), vec![2, 3]);

        // 批 [2,3] 完成，飞行空 → 回空闲。
        assert!(!queue.on_batch_finished());
        assert_eq!(queue.state(), BatchQueueState::Idle);
        assert!(queue.is_empty());
    }

    #[test]
    fn continuous_merge_no_loss_no_starvation() {
        // 模拟连续到达：每批处理完成后飞行缓冲立即开下一批，消息不漏、不饿死。
        let mut queue = q();
        let mut processed: Vec<u32> = Vec::new();
        let mut next_item = 0u32;
        queue.push(next_item);
        next_item += 1;

        // 处理 5 个批；每批处理期间补发 2 条（进飞行缓冲）。
        for _ in 0..5 {
            let batch = queue.take_current_batch();
            assert!(!batch.is_empty(), "批不得为空（不饿死）");
            processed.extend(batch.iter().copied());
            for _ in 0..2 {
                queue.push(next_item);
                next_item += 1;
            }
            let has_next = queue.on_batch_finished();
            if next_item < 15 {
                assert!(has_next, "有飞行缓冲时立即开下一批");
            }
        }
        // 全部消化：剩余批次处理完。
        let mut safety = 0;
        while !queue.is_empty() {
            let batch = queue.take_current_batch();
            processed.extend(batch.iter().copied());
            queue.on_batch_finished();
            safety += 1;
            assert!(safety < 10, "连续合批不得死循环");
        }
        let expected: Vec<u32> = (0..next_item).collect();
        assert_eq!(processed, expected, "连续合批不漏条、保序");
    }

    #[test]
    fn len_semantics_current_plus_flight() {
        let mut queue = q();
        assert_eq!(queue.len(), 0);
        queue.push(1); // 开批
        assert_eq!(queue.len(), 1);
        queue.take_current_batch();
        assert_eq!(queue.len(), 0);
        queue.push(2); // 飞行
        queue.push(3);
        assert_eq!(queue.len(), 2);
        queue.on_batch_finished(); // 飞行 → 当前批
        assert_eq!(queue.len(), 2);
    }

    #[tokio::test]
    async fn next_batch_recv_path_and_rx_close_terminates() {
        let (tx, mut rx) = mpsc::channel::<u32>(8);
        let mut queue = q();
        tx.send(10).await.unwrap();
        tx.send(20).await.unwrap();

        // 空闲 → recv 第一条开批；其余仍在 channel。
        assert_eq!(queue.next_batch(&mut rx).await, Some(vec![10]));
        // 批开始：吸收 channel 存量 → 飞行缓冲。
        queue.absorb_channel(&mut rx);
        assert_eq!(queue.len(), 1);
        // 批完成 → 飞行缓冲开下一批。
        assert!(queue.on_batch_finished());
        assert_eq!(queue.next_batch(&mut rx).await, Some(vec![20]));
        assert!(!queue.on_batch_finished());

        drop(tx);
        assert_eq!(queue.next_batch(&mut rx).await, None, "rx 关闭 → 结束");
    }

    #[tokio::test]
    async fn absorb_channel_during_processing_feeds_flight() {
        let (tx, mut rx) = mpsc::channel::<u32>(8);
        let mut queue = q();
        tx.send(1).await.unwrap();
        assert_eq!(queue.next_batch(&mut rx).await, Some(vec![1]));

        // 处理中到达 2、3。
        tx.send(2).await.unwrap();
        tx.send(3).await.unwrap();
        queue.absorb_channel(&mut rx);
        assert_eq!(queue.len(), 2, "处理中到达 → 飞行缓冲");
        assert!(queue.on_batch_finished());
        assert_eq!(queue.take_current_batch(), vec![2, 3]);
    }

    #[tokio::test]
    async fn long_run_simulation_backlog_stabilizes() {
        // 长跑模拟：60 条初始积压 + 批处理期间持续到达（每轮 3 条，共注入至 240 条）。
        // 连续处理组语义下：无消息丢失、全局保序、积压随处理推进消化（rx 关闭即全清）。
        let (tx, mut rx) = mpsc::channel::<u32>(256);
        let mut tx = Some(tx);
        let mut queue = q();
        let initial = 60u32;
        for i in 0..initial {
            tx.as_ref().unwrap().send(i).await.unwrap();
        }
        let injection_target = 240u32;
        let mut next_inject = initial;
        let mut processed: Vec<u32> = Vec::new();
        let mut iterations = 0usize;
        loop {
            let Some(batch) = queue.next_batch(&mut rx).await else {
                break;
            };
            queue.absorb_channel(&mut rx);
            for item in batch {
                processed.push(item);
                // 模拟批处理期间新到达 3 条 → 飞行缓冲。
                if next_inject < injection_target {
                    for _ in 0..3 {
                        tx.as_ref().unwrap().send(next_inject).await.unwrap();
                        next_inject += 1;
                    }
                }
                queue.absorb_channel(&mut rx);
            }
            queue.on_batch_finished();
            iterations += 1;
            assert!(iterations < 1000, "长跑模拟不得死循环");
            if next_inject >= injection_target {
                tx.take(); // 停止注入：channel 消化完剩余后关闭。
            }
        }
        assert_eq!(
            processed,
            (0..injection_target).collect::<Vec<u32>>(),
            "长跑模拟无消息丢失且保序"
        );
        assert!(queue.is_empty(), "全部消化后队列为空");
    }
}
