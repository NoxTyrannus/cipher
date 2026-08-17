//! v0.3.1 执行中台 subagent 体系 —— T0 冻结共享契约。
//!
//! 这些类型是 TA/TB/TC 的共享契约，本阶段**只冻结、不接线**：
//! - 旧业务继续使用 `crate::agent::communication::ExecutionOutput`（TC 阶段删除）；
//! - 新类型仅通过 `#[allow(dead_code)]` 与测试引用存在，旧业务不得提前接入。
//!
//! 契约字段与命名以任务书 §4.4 / §9.2 / §10 为准：
//! - `ExecutionOutput`：执行中台单轮 LLM 输出的结构化结果（task_design / task_status /
//!   lifecycle_actions / subagent_states）；
//! - `CapabilityLifecycleRecord`：单个能力调用本轮同步可达的终态事实；
//! - `SubagentRuntimeState`：AgentPool 快照中单个 subagent 的运行时/生命周期展示状态；
//! - `SubagentDefinition`：`subagent.run` 受理时冻结的不可变定义快照；
//! - `UsageObservation`：洞察中台的 usage observation 提案（§4.4）。

use serde::{Deserialize, Serialize};

/// 执行中台单次 LLM 输出的结构化结果（v0.3.1 主输出语义）。
///
/// 替代旧 `communication::ExecutionOutput`（dag/node_results/status）作为执行中台主输出；
/// 旧类型在 TC 阶段删除前保留供旧链路使用。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ExecutionOutput {
    /// 本轮任务设计逻辑文本（松散 schema，可为空）。
    pub task_design: String,
    /// 当前状态与下一步判断文本（停/等/改/删/增……，松散 schema，可为空）。
    pub task_status: String,
    /// 本轮按声明顺序执行的能力调用生命周期事实（0/1/多个）。
    pub lifecycle_actions: Vec<CapabilityLifecycleRecord>,
    /// 本轮可见的 subagent 运行时/生命周期状态。
    pub subagent_states: Vec<SubagentRuntimeState>,
}

/// 单个能力调用本轮同步可达的生命周期终态。
///
/// `subagent.run` 在受理后立即返回 `accepted`（不等待异步完成）；
/// 其余分子能力在服务层同步执行并落到 `completed / failed / rejected`。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapabilityLifecycleState {
    /// 异步受理（如 `subagent.run`），本轮不等待结果。
    Accepted,
    /// 同步执行成功。
    Completed,
    /// 同步执行失败（能力内部错误）。
    Failed,
    /// 本轮被拒绝（前置状态不满足 / 授权失败 / 参数不合法）。
    Rejected,
}

/// 单个能力调用的生命周期事实记录。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CapabilityLifecycleRecord {
    /// 能力注册表权威 id（最小许可字段）。
    pub capability_id: String,
    /// 能力名称（服务层校验一致性；未提交时由服务层解析权威名称）。
    #[serde(default)]
    pub capability_name: String,
    /// 参数脱敏摘要（不得包含密钥/原始内容）。
    #[serde(default)]
    pub arguments_summary: String,
    /// 本轮同步可达的终态。
    pub lifecycle_state: CapabilityLifecycleState,
    /// 全局 invocation 事实引用（`<storage_root>/invocations/` 下的不可变事实文件）。
    #[serde(default)]
    pub invocation_ref: Option<String>,
    /// 失败/拒绝原因；`accepted / completed` 时为 `None`。
    #[serde(default)]
    pub error: Option<String>,
    /// 保留 `START/OK/FAIL <capability_id>: ...` 证据格式（洞察 P0 能力证据过滤依赖）。
    #[serde(default)]
    pub capability_call_logs: Vec<String>,
}

/// subagent 持久生命周期（替代旧 `ExecutionStatus` 作为结果语义）。
///
/// 状态链：`created → idle ⇄ running`；`failed` 可再次 run/update/delete；
/// `sleeping` 仅 wake 回到 idle；任意非 tombstoned → `tombstoned`（终态，记忆文件保留）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SubagentLifecycle {
    /// 刚创建，尚未 run。
    Created,
    /// 异步运行中。
    Running,
    /// 空闲（可 run / update / sleep / delete）。
    Idle,
    /// 运行失败（AgentPool 运行时身份保留为 idle，供快照展示与后续操作）。
    Failed,
    /// 软删除终态（记忆/last_output 文件保留，移出 AgentPool）。
    Tombstoned,
}

/// subagent 启动方式（triggered 正交语义的 `startup` 字段）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SubagentStartup {
    /// 普通任务驱动。
    Normal,
    /// 定时（v0.3.1 只做字段/模板/范例，不实现调度器）。
    Scheduled,
    /// 条件触发（v0.3.1 只做字段/模板/范例，不实现调度器）。
    Condition,
}

/// subagent 生命周期种类（triggered 正交语义的 `lifecycle_kind` 字段）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SubagentLifecycleKind {
    /// 一次性/任务驱动。
    Temporary,
    /// 常驻，等待后续 `subagent.run`。
    Resident,
}

/// 单个 subagent 在 AgentPool 快照中的运行时/生命周期展示状态。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SubagentRuntimeState {
    /// 稳定 subagent id（建议 `sg_<uuid>`）。
    pub subagent_id: String,
    /// 持久生命周期。
    pub lifecycle: SubagentLifecycle,
    /// `last_output.json` 的有界截断文本（用于思考引擎异步感知）。
    #[serde(default)]
    pub last_output_truncated: Option<String>,
    /// 触发配置（`{type:"schedule",...}` / `{type:"condition",...}` / null）。
    #[serde(default)]
    pub trigger: Option<serde_json::Value>,
    /// 启动方式。
    pub startup: SubagentStartup,
    /// 生命周期种类。
    pub lifecycle_kind: SubagentLifecycleKind,
}

/// subagent 运行预算（attempt/total timeout 与 max_retries 必须实际生效）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubagentBudget {
    /// 失败后最多有限重试次数（默认 0，超时/重试耗尽写失败事实，不无限重试）。
    pub max_retries: u32,
    /// 单次 attempt 超时（秒）。
    pub attempt_timeout_seconds: u64,
    /// 整个 run 总超时（秒）。
    pub total_timeout_seconds: u64,
}

/// `subagent.run` 受理时冻结的不可变定义快照（§4.3.3 / §5.2）。
///
/// 在途 run 使用冻结快照；`subagent.update` 只写持久行，不改变在途 run 的冻结快照
/// （“新运行生效”）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SubagentDefinition {
    /// 稳定 subagent id。
    pub subagent_id: String,
    /// 从模板复制的 prompt（运行期冻结）。
    pub prompt: String,
    /// 允许上限（从模板 allowlist 继承或缩小，不能扩大）。
    pub capability_allowlist: Vec<String>,
    /// 服务层从模型注册表分配的 model id（API key 不进入快照/日志）。
    pub model_id: String,
    /// 运行预算。
    pub budget: SubagentBudget,
    /// 启动方式。
    pub startup: SubagentStartup,
    /// 触发配置。
    #[serde(default)]
    pub trigger: Option<serde_json::Value>,
}

/// 洞察 usage observation 提案（§4.4）。
///
/// 洞察 LLM 只产出**提案**；服务层校验 `capability_id` 必须出现在最近一次能力调用日志中，
/// 再通过 `usage_method.observe` 分子能力写回 usage_method（不直写注册表稳定契约）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UsageObservation {
    /// 最近一次调用日志中实际出现的能力 id（服务层校验）。
    pub capability_id: String,
    /// 观察（问题/经验）。
    pub observation: String,
    /// 建议（写入 usage_method 的 prompt/metadata 等价字段）。
    pub suggestion: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn lifecycle_record(
        capability_id: &str,
        state: CapabilityLifecycleState,
    ) -> CapabilityLifecycleRecord {
        CapabilityLifecycleRecord {
            capability_id: capability_id.to_string(),
            capability_name: String::new(),
            arguments_summary: "{}".to_string(),
            lifecycle_state: state,
            invocation_ref: None,
            error: None,
            capability_call_logs: vec![],
        }
    }

    #[test]
    fn execution_output_serializes_all_contract_fields() {
        let output = ExecutionOutput {
            task_design: "设计".to_string(),
            task_status: "等待".to_string(),
            lifecycle_actions: vec![lifecycle_record(
                "subagent.run",
                CapabilityLifecycleState::Accepted,
            )],
            subagent_states: vec![SubagentRuntimeState {
                subagent_id: "sg_1".to_string(),
                lifecycle: SubagentLifecycle::Running,
                last_output_truncated: Some("brief".to_string()),
                trigger: Some(json!({"type": "schedule"})),
                startup: SubagentStartup::Normal,
                lifecycle_kind: SubagentLifecycleKind::Temporary,
            }],
        };
        let value = serde_json::to_value(&output).unwrap();
        assert_eq!(value["task_design"], "设计");
        assert_eq!(value["task_status"], "等待");
        assert_eq!(value["lifecycle_actions"][0]["lifecycle_state"], "accepted");
        assert_eq!(value["subagent_states"][0]["lifecycle"], "running");
        assert_eq!(value["subagent_states"][0]["startup"], "normal");
        assert_eq!(value["subagent_states"][0]["lifecycle_kind"], "temporary");
        assert_eq!(
            value["subagent_states"][0]["trigger"],
            json!({"type": "schedule"})
        );
    }

    #[test]
    fn lifecycle_record_keeps_evidence_log_format() {
        let record = CapabilityLifecycleRecord {
            capability_id: "file.read".to_string(),
            capability_name: "Read".to_string(),
            arguments_summary: "{\"path\":\"***\"}".to_string(),
            lifecycle_state: CapabilityLifecycleState::Completed,
            invocation_ref: Some("inv_1".to_string()),
            error: None,
            capability_call_logs: vec![
                "START file.read: ...".to_string(),
                "OK file.read: ...".to_string(),
            ],
        };
        let value = serde_json::to_value(&record).unwrap();
        assert_eq!(value["capability_id"], "file.read");
        assert_eq!(value["lifecycle_state"], "completed");
        assert_eq!(value["invocation_ref"], "inv_1");
        assert!(value["capability_call_logs"][0]
            .as_str()
            .unwrap()
            .starts_with("START "));
        assert!(value["capability_call_logs"][1]
            .as_str()
            .unwrap()
            .starts_with("OK "));
    }

    #[test]
    fn lifecycle_state_enum_round_trips_snake_case() {
        for (state, expected) in [
            (CapabilityLifecycleState::Accepted, "accepted"),
            (CapabilityLifecycleState::Completed, "completed"),
            (CapabilityLifecycleState::Failed, "failed"),
            (CapabilityLifecycleState::Rejected, "rejected"),
        ] {
            let json = serde_json::to_value(state).unwrap();
            assert_eq!(json.as_str().unwrap(), expected);
            let back: CapabilityLifecycleState = serde_json::from_value(json).unwrap();
            assert_eq!(back, state);
        }
    }

    #[test]
    fn subagent_lifecycle_enum_round_trips_snake_case() {
        for (lifecycle, expected) in [
            (SubagentLifecycle::Created, "created"),
            (SubagentLifecycle::Running, "running"),
            (SubagentLifecycle::Idle, "idle"),
            (SubagentLifecycle::Failed, "failed"),
            (SubagentLifecycle::Tombstoned, "tombstoned"),
        ] {
            let json = serde_json::to_value(lifecycle).unwrap();
            assert_eq!(json.as_str().unwrap(), expected);
            let back: SubagentLifecycle = serde_json::from_value(json).unwrap();
            assert_eq!(back, lifecycle);
        }
    }

    #[test]
    fn subagent_definition_freezes_allowlist_and_budget() {
        let definition = SubagentDefinition {
            subagent_id: "sg_9".to_string(),
            prompt: "role".to_string(),
            capability_allowlist: vec!["file.read".to_string(), "file.list".to_string()],
            model_id: "mini".to_string(),
            budget: SubagentBudget {
                max_retries: 0,
                attempt_timeout_seconds: 600,
                total_timeout_seconds: 3600,
            },
            startup: SubagentStartup::Scheduled,
            trigger: Some(json!({"type": "schedule", "cron": "* * * * *"})),
        };
        let value = serde_json::to_value(&definition).unwrap();
        assert_eq!(value["capability_allowlist"][0], "file.read");
        assert_eq!(value["budget"]["max_retries"], 0);
        assert_eq!(value["budget"]["attempt_timeout_seconds"], 600);
        assert_eq!(value["budget"]["total_timeout_seconds"], 3600);
        assert_eq!(value["startup"], "scheduled");
    }

    #[test]
    fn usage_observation_matches_4_4_proposal_shape() {
        let observation = UsageObservation {
            capability_id: "file.read".to_string(),
            observation: "读取大文件超时".to_string(),
            suggestion: "分块读取".to_string(),
        };
        let value = serde_json::to_value(&observation).unwrap();
        assert_eq!(value["capability_id"], "file.read");
        assert_eq!(value["observation"], "读取大文件超时");
        assert_eq!(value["suggestion"], "分块读取");
        let back: UsageObservation = serde_json::from_value(value).unwrap();
        assert_eq!(back.capability_id, "file.read");
    }
}
