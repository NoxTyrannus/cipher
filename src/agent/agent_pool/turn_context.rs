use super::super::communication::{
    ExecutionOutput, InsightOutput, MemoryOutput, TurnContext, TurnStatus,
};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;

pub struct TurnContextStore {
    contexts: HashMap<String, TurnContext>,

    order: std::collections::VecDeque<String>,
}

const MAX_DONE_KEPT: usize = 50;

impl TurnContextStore {
    pub fn new() -> Self {
        Self {
            contexts: HashMap::new(),
            order: std::collections::VecDeque::new(),
        }
    }

    pub fn create(&mut self, ctx: TurnContext) {
        self.order.push_back(ctx.turn_id.clone());
        self.contexts.insert(ctx.turn_id.clone(), ctx);
        self.trim();
    }

    pub fn get(&self, turn_id: &str) -> Option<&TurnContext> {
        self.contexts.get(turn_id)
    }

    pub fn get_mut(&mut self, turn_id: &str) -> Option<&mut TurnContext> {
        self.contexts.get_mut(turn_id)
    }

    pub fn set_execution(&mut self, turn_id: &str, output: ExecutionOutput) -> Option<()> {
        let ctx = self.contexts.get_mut(turn_id)?;
        ctx.execution = Some(output);
        ctx.status = TurnStatus::Insighting;
        Some(())
    }

    pub fn set_insight(&mut self, turn_id: &str, output: InsightOutput) -> Option<()> {
        let ctx = self.contexts.get_mut(turn_id)?;
        ctx.insight = Some(output);
        ctx.status = TurnStatus::Memorizing;
        Some(())
    }

    pub fn set_memory(&mut self, turn_id: &str, output: MemoryOutput) -> Option<()> {
        let ctx = self.contexts.get_mut(turn_id)?;
        ctx.memory = Some(output);
        ctx.status = TurnStatus::Done;
        self.trim();
        Some(())
    }

    pub fn cancel(&mut self, turn_id: &str) -> Option<()> {
        let ctx = self.contexts.get_mut(turn_id)?;
        ctx.status = TurnStatus::Cancelled;
        self.trim();
        Some(())
    }

    pub fn remove(&mut self, turn_id: &str) -> Option<TurnContext> {
        self.contexts.remove(turn_id)
    }

    fn trim(&mut self) {
        let done_count = self
            .contexts
            .values()
            .filter(|c| matches!(c.status, TurnStatus::Done | TurnStatus::Cancelled))
            .count();
        if done_count <= MAX_DONE_KEPT {
            return;
        }
        let excess = done_count - MAX_DONE_KEPT;
        let mut removed = 0usize;
        let order = std::mem::take(&mut self.order);
        for id in order {
            if removed >= excess {
                self.order.push_back(id);
                continue;
            }
            match self.contexts.get(&id) {
                Some(ctx) if matches!(ctx.status, TurnStatus::Done | TurnStatus::Cancelled) => {
                    self.contexts.remove(&id);
                    removed += 1;
                }
                _ => self.order.push_back(id),
            }
        }
    }

    pub fn active_count(&self) -> usize {
        self.contexts
            .values()
            .filter(|c| c.status != TurnStatus::Done && c.status != TurnStatus::Cancelled)
            .count()
    }

    pub fn len(&self) -> usize {
        self.contexts.len()
    }

    pub fn is_empty(&self) -> bool {
        self.contexts.is_empty()
    }
}

impl Default for TurnContextStore {
    fn default() -> Self {
        Self::new()
    }
}

pub type SharedTurnContextStore = Arc<RwLock<TurnContextStore>>;

#[cfg(test)]
mod tests {
    use super::super::super::communication::{
        CapabilityLifecycleRecord, CapabilityLifecycleState, ExecutionOutput, ThinkDecision,
        ThinkingOutput,
    };
    use super::*;

    fn make_ctx(turn_id: &str) -> TurnContext {
        TurnContext {
            turn_id: turn_id.into(),
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
        }
    }

    #[test]
    fn store_create_and_get() {
        let mut store = TurnContextStore::new();
        store.create(make_ctx("t1"));
        assert!(store.get("t1").is_some());
    }

    #[test]
    fn store_trims_old_done_turns_keeps_active_and_recent() {
        use super::super::super::communication::MemoryOutput;
        let mut store = TurnContextStore::new();
        let done_output = MemoryOutput {
            attention: vec![],
            experience: vec![],
            preference: vec![],
            cognitive: vec![],
        };

        for i in 0..(MAX_DONE_KEPT + 20) {
            let id = format!("done-{i}");
            store.create(make_ctx(&id));
            store.set_memory(&id, done_output.clone());
        }
        assert_eq!(
            store.contexts.len(),
            MAX_DONE_KEPT,
            "old done turns must be trimmed to the cap"
        );
        assert!(store.get("done-0").is_none(), "oldest done turn trimmed");
        assert!(
            store.get(&format!("done-{}", MAX_DONE_KEPT + 19)).is_some(),
            "newest done turn kept"
        );

        store.create(make_ctx("active-1"));
        store.create(make_ctx("active-2"));
        for i in 0..10 {
            let id = format!("late-{i}");
            store.create(make_ctx(&id));
            store.set_memory(&id, done_output.clone());
        }
        assert!(store.get("active-1").is_some());
        assert!(store.get("active-2").is_some());

        let done_count = store
            .contexts
            .values()
            .filter(|c| matches!(c.status, TurnStatus::Done | TurnStatus::Cancelled))
            .count();
        assert!(
            done_count <= MAX_DONE_KEPT,
            "done turns must stay within cap, got {done_count}"
        );
    }

    #[test]
    fn store_set_execution_transitions_status() {
        let mut store = TurnContextStore::new();
        store.create(make_ctx("t1"));

        let output = ExecutionOutput {
            task_design: "test".into(),
            task_status: "waiting".into(),
            lifecycle_actions: vec![CapabilityLifecycleRecord {
                capability_id: "subagent.run".into(),
                capability_name: "Run Subagent".into(),
                arguments_summary: "{}".into(),
                lifecycle_state: CapabilityLifecycleState::Accepted,
                invocation_ref: None,
                error: None,
                capability_call_logs: vec![],
            }],
            subagent_states: vec![],
        };
        store.set_execution("t1", output);
        assert_eq!(store.get("t1").unwrap().status, TurnStatus::Insighting);
    }

    #[test]
    fn store_full_chain_status_transitions() {
        let mut store = TurnContextStore::new();
        store.create(make_ctx("t1"));
        assert_eq!(store.get("t1").unwrap().status, TurnStatus::Executing);

        store.set_execution(
            "t1",
            ExecutionOutput {
                task_design: "test".into(),
                task_status: "waiting".into(),
                lifecycle_actions: vec![],
                subagent_states: vec![],
            },
        );
        assert_eq!(store.get("t1").unwrap().status, TurnStatus::Insighting);

        store.set_insight(
            "t1",
            InsightOutput {
                insight: super::super::super::communication::InsightResult {
                    insight: "ok".into(),
                },
                usage_observations: vec![],
            },
        );
        assert_eq!(store.get("t1").unwrap().status, TurnStatus::Memorizing);

        store.set_memory("t1", MemoryOutput::default());
        assert_eq!(store.get("t1").unwrap().status, TurnStatus::Done);
    }

    #[test]
    fn store_cancel() {
        let mut store = TurnContextStore::new();
        store.create(make_ctx("t1"));
        store.cancel("t1");
        assert_eq!(store.get("t1").unwrap().status, TurnStatus::Cancelled);
    }

    #[test]
    fn store_active_count() {
        let mut store = TurnContextStore::new();
        store.create(make_ctx("t1"));
        assert_eq!(store.active_count(), 1);

        let mut ctx2 = make_ctx("t2");
        ctx2.status = TurnStatus::Done;
        store.create(ctx2);
        assert_eq!(store.active_count(), 1);
    }
}
