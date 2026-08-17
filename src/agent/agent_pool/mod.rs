pub mod channels;
pub mod registry;
pub mod scheduler;
pub mod turn_context;

use std::sync::Arc;
use tokio::sync::{watch, RwLock};

use super::communication::{
    AgentMessage, ExecutionOutput, InsightOutput, MemoryOutput, TurnContext, TurnStatus,
};
use channels::{create_message_bus, MessageBus, MessageReceivers, TriggerEvent};
use registry::{AgentEntry, AgentIdentity, AgentStatus, InstanceRegistry, SharedRegistry};
use scheduler::Scheduler;
use turn_context::{SharedTurnContextStore, TurnContextStore};

#[derive(Debug)]
pub struct AgentPoolSnapshot {
    pub entries: Vec<AgentEntry>,
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

    pub async fn register_platform(&self, id: &str, identity: AgentIdentity) {
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
        let snapshot = AgentPoolSnapshot {
            entries: self
                .registry
                .read()
                .await
                .snapshot()
                .into_iter()
                .cloned()
                .collect(),
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
}
