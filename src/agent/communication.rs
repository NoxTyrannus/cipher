use serde::{Deserialize, Serialize};

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

    pub say_published: bool,
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
pub struct ExecutionOutput {
    pub dag: ExecutionDag,
    pub node_results: Vec<NodeResult>,
    pub status: ExecutionStatus,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ExecutionDag {
    #[serde(rename = "single")]
    Single {
        template_kind: String,
        capability_ids: Vec<String>,
        task_context: String,
    },
    #[serde(rename = "dag")]
    Dag { nodes: Vec<DagNode> },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DagNode {
    pub id: String,
    pub template_kind: String,
    pub capability_ids: Vec<String>,
    pub task_context: String,
    pub depends_on: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeResult {
    pub node_id: String,
    pub status: NodeStatus,
    pub summary: String,
    pub error: Option<String>,
    pub tool_call_count: u32,

    #[serde(default)]
    pub tool_call_logs: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum NodeStatus {
    Completed,
    Failed,

    Skipped,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ExecutionStatus {
    Success,
    PartialFailure,
    Failure,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InsightOutput {
    pub insight: InsightResult,
    pub tool_memory: Vec<ToolMemoryUpdate>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InsightResult {
    /// 洞察中台基于三问方法得出的完整判断文本。
    /// 三问只作为提示词中的思考方法存在，不再进入输出结构。
    pub insight: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolMemoryUpdate {
    pub capability_id: String,
    pub description_patch: String,
    pub rating: String,
    pub note: String,
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
    /// 与 retired_focus 一一对应；每个元素是该 focus 被淘汰时的原始记忆索引。
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
            say_published: false,
        };
        assert_eq!(ctx.status, TurnStatus::Executing);
        assert!(ctx.execution.is_none());
    }

    #[test]
    fn execution_dag_single_serialization() {
        let dag = ExecutionDag::Single {
            template_kind: "normal".into(),
            capability_ids: vec!["cap_1".into()],
            task_context: "do something".into(),
        };
        let json = serde_json::to_string(&dag).unwrap();
        assert!(json.contains("single"));
        assert!(json.contains("normal"));
    }

    #[test]
    fn execution_dag_multi_serialization() {
        let dag = ExecutionDag::Dag {
            nodes: vec![DagNode {
                id: "n1".into(),
                template_kind: "normal".into(),
                capability_ids: vec!["cap_1".into()],
                task_context: "step 1".into(),
                depends_on: vec![],
            }],
        };
        let json = serde_json::to_string(&dag).unwrap();
        assert!(json.contains("dag"));
        assert!(json.contains("n1"));
    }

    #[test]
    fn insight_output_serialization() {
        let output = InsightOutput {
            insight: InsightResult {
                insight: "执行证据已核对，目标已达成。".into(),
            },
            tool_memory: vec![],
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
