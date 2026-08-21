use serde::{Deserialize, Serialize};

// T0 冻结的执行中台共享契约。旧 ExecutionDag/NodeResult/ExecutionStatus 已删除，
// 新执行中台主输出统一使用这些类型。
pub use crate::agent::execution_types::{
    CapabilityLifecycleRecord, CapabilityLifecycleState, ExecutionOutput, SubagentLifecycle,
    SubagentLifecycleKind, SubagentRuntimeState, SubagentStartup, UsageObservation,
};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum AgentMessage {
    Execute { turn_id: String },

    MessageDeliver { turn_id: String, message: String },

    ExecutionDone { turn_id: String },

    InsightDone { turn_id: String },

    Cancel { turn_id: String },
}

#[derive(Debug, Clone)]
pub struct TurnContext {
    pub turn_id: String,

    pub thinking: ThinkingOutput,

    pub execution: Option<ExecutionOutput>,

    pub insight: Option<InsightOutput>,

    pub memory: Option<MemoryOutput>,

    pub status: TurnStatus,

    pub user_message: String,

    pub input_kind: String,
}

#[derive(Debug, Clone)]
pub struct ThinkingOutput {
    pub decision: ThinkDecision,
    pub goal: String,
    pub constraints: Vec<String>,

    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ThinkDecision {
    Execute,

    Failure,

    Reply,

    Cancel,

    Inherit,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TurnStatus {
    Executing,
    Insighting,
    Memorizing,
    Done,
    Cancelled,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InsightOutput {
    pub insight: InsightResult,
    #[serde(default)]
    pub usage_observations: Vec<UsageObservation>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InsightResult {
    pub insight: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MemoryOutput {
    pub attention: Vec<AttentionFragment>,
    pub experience: Vec<ExperienceFragment>,
    pub preference: Vec<PreferenceFragment>,
    pub cognitive: Vec<CognitiveFragment>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AttentionFragment {
    pub focus: String,
    pub content: String,
    #[serde(default)]
    pub source_refs: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExperienceFragment {
    pub title: String,
    pub summary: String,
    #[serde(default)]
    pub source_refs: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PreferenceFragment {
    pub key: String,
    pub value: String,
    #[serde(default)]
    pub source_refs: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CognitiveFragment {
    pub entity: String,
    pub relation: String,
    pub target: String,
}

#[derive(Debug, Clone)]
pub struct AttentionRetireBatch {
    pub retired_focus: Vec<String>,
    pub source_refs: Vec<Vec<String>>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_message_serialization() {
        let msg = AgentMessage::Execute {
            turn_id: "t1".into(),
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains("Execute"));
        assert!(json.contains("t1"));
    }

    #[test]
    fn turn_context_default_status() {
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
        };
        assert_eq!(ctx.status, TurnStatus::Executing);
        assert!(ctx.execution.is_none());
    }

    #[test]
    fn new_execution_output_serialization() {
        let output = ExecutionOutput {
            task_design: "test".into(),
            task_status: "done".into(),
            lifecycle_actions: vec![CapabilityLifecycleRecord {
                capability_id: "subagent.create".into(),
                capability_name: "Create Subagent".into(),
                arguments_summary: "{}".into(),
                lifecycle_state: CapabilityLifecycleState::Completed,
                invocation_ref: None,
                error: None,
                capability_call_logs: vec!["OK subagent.create".into()],
            }],
            subagent_states: vec![],
        };
        let json = serde_json::to_string(&output).unwrap();
        assert!(json.contains("subagent.create"));
        assert!(json.contains("completed"));
    }

    #[test]
    fn insight_output_serialization() {
        let output = InsightOutput {
            insight: InsightResult {
                insight: "方向正确，继续执行。".into(),
            },
            usage_observations: vec![],
        };
        let json = serde_json::to_string(&output).unwrap();
        assert!(json.contains("insight"));
    }

    #[test]
    fn memory_output_default_is_empty() {
        let m = MemoryOutput::default();
        assert!(m.attention.is_empty());
        assert!(m.experience.is_empty());
    }
}
