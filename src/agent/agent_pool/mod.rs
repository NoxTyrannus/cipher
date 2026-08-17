pub mod channels;
pub mod registry;
pub mod scheduler;
pub mod turn_context;

use std::sync::Arc;
use tokio::sync::{watch, RwLock};

use super::communication::{
    AgentMessage, ExecutionOutput, InsightOutput, MemoryOutput, TurnContext, TurnStatus,
};
use super::execution_types::{SubagentLifecycle, SubagentRuntimeState};
use channels::{create_message_bus, MessageBus, MessageReceivers, TriggerEvent};
use registry::{AgentEntry, AgentIdentity, AgentStatus, InstanceRegistry, SharedRegistry};
use scheduler::Scheduler;
use turn_context::{SharedTurnContextStore, TurnContextStore};

#[derive(Debug)]
pub struct AgentPoolSnapshot {
    pub entries: Vec<AgentEntry>,
    /// subagent 展示状态（持久生命周期 / trigger / startup / last_output 截断）。
    /// 思考引擎自感知的 subagent 来源（与 entries 的运行时身份分离）。
    pub subagent_states: Vec<SubagentRuntimeState>,
    pub execution_pending_depth: usize,
    pub insight_pending_depth: usize,
    pub memory_pending_depth: usize,
    pub execution_active_batch: Option<String>,
    pub insight_active_batch: Option<String>,
    pub memory_active_batch: Option<String>,
    pub cognitive_remaining: u32,
    pub repair_in_flight: u32,
    pub captured_at: std::time::Instant,
}

impl Clone for AgentPoolSnapshot {
    fn clone(&self) -> Self {
        Self {
            entries: self.entries.clone(),
            subagent_states: self.subagent_states.clone(),
            execution_pending_depth: self.execution_pending_depth,
            insight_pending_depth: self.insight_pending_depth,
            memory_pending_depth: self.memory_pending_depth,
            execution_active_batch: self.execution_active_batch.clone(),
            insight_active_batch: self.insight_active_batch.clone(),
            memory_active_batch: self.memory_active_batch.clone(),
            cognitive_remaining: self.cognitive_remaining,
            repair_in_flight: self.repair_in_flight,
            captured_at: std::time::Instant::now(),
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct PlatformStatus {
    pub execution_pending: usize,
    pub insight_pending: usize,
    pub memory_pending: usize,
    pub execution_active: Option<String>,
    pub insight_active: Option<String>,
    pub memory_active: Option<String>,
    pub cognitive_remaining: u32,
    pub repair_in_flight: u32,
}

#[derive(Debug, Clone)]
pub struct PlatformEvent {
    pub kind: String,

    pub detail: String,
}

pub struct AgentPool {
    registry: SharedRegistry,

    message_bus: MessageBus,

    turn_contexts: SharedTurnContextStore,

    _scheduler_handle: tokio::task::JoinHandle<()>,

    state_tx: watch::Sender<AgentPoolSnapshot>,

    platform_status: Arc<tokio::sync::RwLock<PlatformStatus>>,

    event_bus: tokio::sync::broadcast::Sender<PlatformEvent>,
}

impl AgentPool {
    pub fn new() -> (Self, MessageReceivers) {
        let registry = Arc::new(RwLock::new(InstanceRegistry::new()));
        let turn_contexts = Arc::new(RwLock::new(TurnContextStore::new()));
        let (message_bus, receivers) = create_message_bus();

        let scheduler = Scheduler::new(registry.clone());
        let scheduler_handle = scheduler.spawn();

        let initial_snapshot = AgentPoolSnapshot {
            entries: vec![],
            subagent_states: vec![],
            execution_pending_depth: 0,
            insight_pending_depth: 0,
            memory_pending_depth: 0,
            execution_active_batch: None,
            insight_active_batch: None,
            memory_active_batch: None,
            cognitive_remaining: 0,
            repair_in_flight: 0,
            captured_at: std::time::Instant::now(),
        };
        let (state_tx, _) = watch::channel(initial_snapshot);
        let (event_bus, _) = tokio::sync::broadcast::channel::<PlatformEvent>(64);

        let pool = Self {
            registry,
            message_bus,
            turn_contexts,
            _scheduler_handle: scheduler_handle,
            state_tx,
            platform_status: Arc::new(tokio::sync::RwLock::new(PlatformStatus::default())),
            event_bus,
        };

        (pool, receivers)
    }

    pub fn subscribe_state(&self) -> watch::Receiver<AgentPoolSnapshot> {
        self.state_tx.subscribe()
    }

    pub async fn create_turn_context(&self, ctx: TurnContext) {
        self.turn_contexts.write().await.create(ctx);
    }

    pub async fn get_turn_context(&self, turn_id: &str) -> Option<TurnContext> {
        self.turn_contexts.read().await.get(turn_id).cloned()
    }

    pub async fn set_execution(&self, turn_id: &str, output: ExecutionOutput) {
        self.turn_contexts
            .write()
            .await
            .set_execution(turn_id, output);
    }

    pub async fn set_insight(&self, turn_id: &str, output: InsightOutput) {
        self.turn_contexts
            .write()
            .await
            .set_insight(turn_id, output);
    }

    pub async fn set_memory(&self, turn_id: &str, output: MemoryOutput) {
        self.turn_contexts.write().await.set_memory(turn_id, output);
    }

    pub async fn mark_done(&self, turn_id: &str) {
        let mut store = self.turn_contexts.write().await;
        if let Some(ctx) = store.get_mut(turn_id) {
            ctx.status = TurnStatus::Done;
        }
    }

    pub async fn cancel_turn(&self, turn_id: &str) {
        self.turn_contexts.write().await.cancel(turn_id);
    }

    pub async fn send_execute(&self, turn_id: &str) -> Result<(), String> {
        self.message_bus
            .send_to_execution_backpressure(AgentMessage::Execute {
                turn_id: turn_id.to_string(),
            })
            .await
    }

    pub async fn send_execution_done(&self, turn_id: &str) -> Result<(), String> {
        self.message_bus
            .send_to_insight_backpressure(AgentMessage::ExecutionDone {
                turn_id: turn_id.to_string(),
            })
            .await
    }

    pub async fn send_insight_done(&self, turn_id: &str) -> Result<(), String> {
        self.message_bus
            .send_to_memory_backpressure(AgentMessage::InsightDone {
                turn_id: turn_id.to_string(),
            })
            .await
    }

    pub fn send_cancel(&self, turn_id: &str) {
        let result = self.message_bus.send_to_execution(AgentMessage::Cancel {
            turn_id: turn_id.to_string(),
        });
        if let Err(e) = result {
            tracing::warn!("agent_pool: send_cancel failed turn_id={turn_id}: {e:?}");
        }
    }

    pub async fn send_trigger(&self, turn_id: &str, reason: &str) -> Result<(), String> {
        self.message_bus
            .send_trigger_backpressure(TriggerEvent {
                turn_id: turn_id.to_string(),
                reason: reason.to_string(),
            })
            .await
    }

    pub fn message_bus(&self) -> MessageBus {
        self.message_bus.clone()
    }

    /// 注册核心平台身份（思考引擎实例 / 执行 / 洞察 / 记忆中台），仅启动装配使用。
    ///
    /// 业务操作禁止触碰四组核心身份：本方法拒绝 subagent 身份，subagent 只能经
    /// `register_subagent` / `update_subagent_status` / `remove_subagent` 操作。
    pub async fn register_platform(&self, id: &str, identity: AgentIdentity) {
        if identity.is_subagent() {
            tracing::warn!(
                "agent_pool: register_platform 拒绝 subagent 身份（请走 register_subagent）: {id}"
            );
            return;
        }
        let entry = AgentEntry {
            id: id.to_string(),
            identity,
            status: AgentStatus::Idle,
            created_at: std::time::Instant::now(),
            last_heartbeat: std::time::Instant::now(),
            heartbeat_source: None,
        };
        self.registry.write().await.register(entry);
    }

    /// 注册 subagent 运行时身份 + 展示状态（业务侧唯一注册入口）。
    pub async fn register_subagent(&self, state: SubagentRuntimeState, status: AgentStatus) {
        self.registry.write().await.register_subagent(state, status);
    }

    /// 更新 subagent 运行身份状态（idle/running/pending）。
    pub async fn update_subagent_status(&self, id: &str, status: AgentStatus) {
        self.registry
            .write()
            .await
            .update_subagent_status(id, status);
    }

    /// 更新 subagent 持久生命周期（展示状态）。
    pub async fn update_subagent_lifecycle(&self, id: &str, lifecycle: SubagentLifecycle) {
        self.registry
            .write()
            .await
            .set_subagent_lifecycle(id, lifecycle);
    }

    /// 更新 subagent last_output 有界截断文本（供快照/思考引擎异步感知）。
    pub async fn set_subagent_last_output(&self, id: &str, truncated: Option<String>) {
        self.registry
            .write()
            .await
            .set_subagent_last_output(id, truncated);
    }

    /// subagent 主动心跳上报（1 秒频率由 subagent 实例侧保证；AgentPool 不轮询业务文件）。
    pub async fn touch_subagent_heartbeat(&self, subagent_id: &str, source: &str) {
        self.registry
            .write()
            .await
            .touch_subagent_heartbeat(subagent_id, source);
    }

    /// 移除 subagent（tombstoned 移出池）。
    pub async fn remove_subagent(&self, id: &str) {
        self.registry.write().await.remove_subagent(id);
    }

    /// subagent 展示状态快照（思考引擎自感知来源）。
    pub async fn subagent_states(&self) -> Vec<SubagentRuntimeState> {
        self.registry.read().await.subagent_snapshot()
    }

    pub async fn snapshot(&self) -> Vec<AgentEntry> {
        self.registry
            .read()
            .await
            .snapshot()
            .into_iter()
            .cloned()
            .collect()
    }

    pub async fn snapshot_detailed(&self) -> AgentPoolSnapshot {
        let status = self.platform_status.read().await;
        let reg = self.registry.read().await;
        let snapshot = AgentPoolSnapshot {
            entries: reg.snapshot().into_iter().cloned().collect(),
            subagent_states: reg.subagent_snapshot(),
            execution_pending_depth: status.execution_pending,
            insight_pending_depth: status.insight_pending,
            memory_pending_depth: status.memory_pending,
            execution_active_batch: status.execution_active.clone(),
            insight_active_batch: status.insight_active.clone(),
            memory_active_batch: status.memory_active.clone(),
            cognitive_remaining: status.cognitive_remaining,
            repair_in_flight: status.repair_in_flight,
            captured_at: std::time::Instant::now(),
        };
        drop(status);
        drop(reg);
        let _ = self.state_tx.send(snapshot.clone());
        snapshot
    }

    pub async fn update_platform_status(&self, f: impl FnOnce(&mut PlatformStatus)) {
        f(&mut *self.platform_status.write().await);
    }

    pub fn publish_event(&self, kind: impl Into<String>, detail: impl Into<String>) {
        let _ = self.event_bus.send(PlatformEvent {
            kind: kind.into(),
            detail: detail.into(),
        });
    }

    pub fn subscribe_events(&self) -> tokio::sync::broadcast::Receiver<PlatformEvent> {
        self.event_bus.subscribe()
    }
}

#[cfg(test)]
mod tests {
    use super::super::communication::{ThinkDecision, ThinkingOutput};
    use super::*;

    #[tokio::test]
    async fn agent_pool_create_and_read_turn_context() {
        let (pool, _receivers) = AgentPool::new();
        let ctx = TurnContext {
            turn_id: "t1".into(),
            thinking: ThinkingOutput {
                decision: ThinkDecision::Execute,
                goal: "test".into(),
                constraints: vec![],
                message: String::new(),
            },
            execution: None,
            insight: None,
            memory: None,
            status: TurnStatus::Executing,
            user_message: String::new(),
            input_kind: "user".into(),
            say_published: false,
        };
        pool.create_turn_context(ctx).await;

        let read = pool.get_turn_context("t1").await;
        assert!(read.is_some());
        assert_eq!(read.unwrap().status, TurnStatus::Executing);
    }

    #[tokio::test]
    async fn agent_pool_send_execute_dm() {
        let (pool, mut receivers) = AgentPool::new();
        pool.send_execute("t1").await.unwrap();

        let msg = receivers.execution_rx.try_recv();
        assert!(msg.is_ok());
        match msg.unwrap() {
            AgentMessage::Execute { turn_id } => assert_eq!(turn_id, "t1"),
            _ => panic!("expected Execute"),
        }
    }

    #[tokio::test]
    async fn agent_pool_full_dm_chain() {
        let (pool, mut receivers) = AgentPool::new();

        pool.send_execute("t1").await.unwrap();
        let msg = receivers.execution_rx.try_recv().unwrap();
        assert!(matches!(msg, AgentMessage::Execute { .. }));

        pool.send_execution_done("t1").await.unwrap();
        let msg = receivers.insight_rx.try_recv().unwrap();
        assert!(matches!(msg, AgentMessage::ExecutionDone { .. }));

        pool.send_insight_done("t1").await.unwrap();
        let msg = receivers.memory_rx.try_recv().unwrap();
        assert!(matches!(msg, AgentMessage::InsightDone { .. }));
    }

    #[tokio::test]
    async fn agent_pool_cancel_turn() {
        let (pool, mut receivers) = AgentPool::new();
        pool.send_cancel("t1");

        let msg = receivers.execution_rx.try_recv().unwrap();
        assert!(matches!(msg, AgentMessage::Cancel { .. }));
    }

    #[tokio::test]
    async fn pool_register_subagent_and_heartbeat_observable() {
        let (pool, _receivers) = AgentPool::new();
        let state = SubagentRuntimeState {
            subagent_id: "sg_pool".to_string(),
            lifecycle: SubagentLifecycle::Running,
            last_output_truncated: None,
            trigger: Some(serde_json::json!({"type": "schedule"})),
            startup: super::super::execution_types::SubagentStartup::Scheduled,
            lifecycle_kind: super::super::execution_types::SubagentLifecycleKind::Resident,
        };
        pool.register_subagent(state, AgentStatus::Running).await;

        let before = pool
            .snapshot()
            .await
            .into_iter()
            .find(|e| e.id == "sg_pool")
            .expect("entry present")
            .last_heartbeat;
        pool.touch_subagent_heartbeat("sg_pool", "subagent-runtime")
            .await;
        let after = pool
            .snapshot()
            .await
            .into_iter()
            .find(|e| e.id == "sg_pool")
            .expect("entry present")
            .last_heartbeat;
        assert!(after >= before);
        assert_eq!(
            pool.snapshot()
                .await
                .into_iter()
                .find(|e| e.id == "sg_pool")
                .unwrap()
                .heartbeat_source
                .as_deref(),
            Some("subagent-runtime")
        );
    }

    #[tokio::test]
    async fn pool_detailed_snapshot_exposes_subagent_display_fields() {
        let (pool, _receivers) = AgentPool::new();
        let state = SubagentRuntimeState {
            subagent_id: "sg_disp".to_string(),
            lifecycle: SubagentLifecycle::Failed,
            last_output_truncated: Some("failed: timeout".to_string()),
            trigger: Some(serde_json::json!({"type": "condition"})),
            startup: super::super::execution_types::SubagentStartup::Condition,
            lifecycle_kind: super::super::execution_types::SubagentLifecycleKind::Temporary,
        };
        pool.register_subagent(state, AgentStatus::Idle).await;

        let snapshot = pool.snapshot_detailed().await;
        assert_eq!(snapshot.subagent_states.len(), 1);
        assert_eq!(snapshot.subagent_states[0].subagent_id, "sg_disp");
        assert_eq!(
            snapshot.subagent_states[0].lifecycle,
            SubagentLifecycle::Failed
        );
        assert_eq!(
            snapshot.subagent_states[0].startup,
            super::super::execution_types::SubagentStartup::Condition
        );
        assert_eq!(
            snapshot.subagent_states[0].trigger,
            Some(serde_json::json!({"type": "condition"}))
        );
    }

    #[tokio::test]
    async fn pool_core_four_not_operable_via_subagent_or_business_paths() {
        let (pool, _receivers) = AgentPool::new();
        pool.register_platform("execution-platform", AgentIdentity::ExecutionPlatform)
            .await;
        pool.register_platform("insight-platform", AgentIdentity::InsightPlatform)
            .await;
        pool.register_platform("memory-platform", AgentIdentity::MemoryPlatform)
            .await;

        // register_platform 拒绝 subagent 身份（subagent 不能伪装成核心身份注册）。
        pool.register_platform(
            "fake-subagent",
            AgentIdentity::SubagentRunning {
                agent_id: "fake".to_string(),
            },
        )
        .await;
        let entries = pool.snapshot().await;
        assert!(entries.iter().all(|e| e.id != "fake-subagent"));

        // 核心身份无法经 subagent 专用方法被修改/移除。
        pool.update_subagent_status("execution-platform", AgentStatus::Running)
            .await;
        pool.touch_subagent_heartbeat("execution-platform", "business")
            .await;
        pool.remove_subagent("execution-platform").await;
        let entries = pool.snapshot().await;
        assert!(entries.iter().any(|e| e.id == "execution-platform"));
        assert_eq!(
            entries
                .iter()
                .find(|e| e.id == "execution-platform")
                .unwrap()
                .status,
            AgentStatus::Idle,
            "核心身份状态不应被业务路径改动"
        );
    }

    #[tokio::test]
    async fn pool_subagent_run_status_and_remove() {
        let (pool, _receivers) = AgentPool::new();
        let state = SubagentRuntimeState {
            subagent_id: "sg_run".to_string(),
            lifecycle: SubagentLifecycle::Created,
            last_output_truncated: None,
            trigger: None,
            startup: super::super::execution_types::SubagentStartup::Normal,
            lifecycle_kind: super::super::execution_types::SubagentLifecycleKind::Temporary,
        };
        pool.register_subagent(state, AgentStatus::Idle).await;
        pool.update_subagent_status("sg_run", AgentStatus::Running)
            .await;
        pool.update_subagent_lifecycle("sg_run", SubagentLifecycle::Running)
            .await;
        assert_eq!(
            pool.snapshot()
                .await
                .into_iter()
                .find(|e| e.id == "sg_run")
                .unwrap()
                .status,
            AgentStatus::Running
        );
        assert_eq!(
            pool.subagent_states().await[0].lifecycle,
            SubagentLifecycle::Running
        );

        // failed 实例回 idle 身份但保留在池。
        pool.update_subagent_lifecycle("sg_run", SubagentLifecycle::Failed)
            .await;
        pool.update_subagent_status("sg_run", AgentStatus::Idle)
            .await;
        assert_eq!(
            pool.subagent_states().await[0].lifecycle,
            SubagentLifecycle::Failed
        );

        // tombstoned 移出池。
        pool.remove_subagent("sg_run").await;
        assert!(pool.subagent_states().await.is_empty());
        assert!(pool.snapshot().await.iter().all(|e| e.id != "sg_run"));
    }
}
