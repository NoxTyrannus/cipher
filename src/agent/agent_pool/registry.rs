use crate::agent::execution_types::SubagentRuntimeState;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentIdentity {
    ThinkingEngine { instance_id: String },

    ExecutionPlatform,

    InsightPlatform,

    MemoryPlatform,

    SubagentRunning { agent_id: String },

    SubagentPending { agent_id: String },

    SubagentResident { agent_id: String },
}

impl AgentIdentity {
    /// 是否属于四组核心身份（思考引擎实例组 / 执行中台 / 洞察中台 / 记忆中台）。
    ///
    /// 核心身份不可被业务操作（注册/状态更新/移除走内部路径）；subagent 操作必须走
    /// InstanceRegistry::register_subagent / update_subagent_status / remove_subagent。
    pub fn is_core(&self) -> bool {
        matches!(
            self,
            AgentIdentity::ThinkingEngine { .. }
                | AgentIdentity::ExecutionPlatform
                | AgentIdentity::InsightPlatform
                | AgentIdentity::MemoryPlatform
        )
    }

    /// 是否属于 subagent 运行时身份。
    pub fn is_subagent(&self) -> bool {
        matches!(
            self,
            AgentIdentity::SubagentRunning { .. }
                | AgentIdentity::SubagentPending { .. }
                | AgentIdentity::SubagentResident { .. }
        )
    }
}

/// 由运行时状态推导 subagent 身份（running/pending/idle -> 对应身份变体）。
fn subagent_identity_for(status: &AgentStatus, agent_id: &str) -> AgentIdentity {
    match status {
        AgentStatus::Running => AgentIdentity::SubagentRunning {
            agent_id: agent_id.to_string(),
        },
        AgentStatus::Pending => AgentIdentity::SubagentPending {
            agent_id: agent_id.to_string(),
        },
        AgentStatus::Idle => AgentIdentity::SubagentResident {
            agent_id: agent_id.to_string(),
        },
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum AgentStatus {
    Idle,
    Running,
    Pending,
}

#[derive(Debug, Clone)]
pub struct AgentEntry {
    pub id: String,
    pub identity: AgentIdentity,
    pub status: AgentStatus,
    pub created_at: std::time::Instant,
    /// 最近一次主动上报心跳的时刻（subagent 实例 1 秒一次）。
    pub last_heartbeat: std::time::Instant,
    /// 心跳来源标识（如 subagent runtime 的 worker 标识）。
    pub heartbeat_source: Option<String>,
}

impl AgentEntry {
    /// 记录一次主动心跳（1 秒频率由 subagent 实例侧保证，AgentPool 不轮询业务文件）。
    pub fn touch_heartbeat(&mut self, source: Option<&str>) {
        self.last_heartbeat = std::time::Instant::now();
        self.heartbeat_source = source.map(str::to_string);
    }
}

pub struct InstanceRegistry {
    entries: HashMap<String, AgentEntry>,
    /// subagent 展示状态（持久生命周期 / trigger / startup / last_output 截断）。
    ///
    /// 与 entries 中对应 subagent 的 AgentEntry（运行时身份 + 心跳）分离：
    /// 运行身份状态保持 {idle, running, pending}，持久生命周期在展示状态中表达
    /// （failed 实例以 idle 身份保留在池，tombstoned 移出）。
    subagent_states: HashMap<String, SubagentRuntimeState>,
}

impl InstanceRegistry {
    pub fn new() -> Self {
        Self {
            entries: HashMap::new(),
            subagent_states: HashMap::new(),
        }
    }

    pub fn register(&mut self, entry: AgentEntry) {
        self.entries.insert(entry.id.clone(), entry);
    }

    /// 通用状态更新：拒绝 subagent 身份（subagent 状态只能经 update_subagent_status）。
    pub fn update_status(&mut self, id: &str, status: AgentStatus) -> Option<()> {
        let entry = self.entries.get_mut(id)?;
        if entry.identity.is_subagent() {
            return None;
        }
        entry.status = status;
        Some(())
    }

    pub fn remove(&mut self, id: &str) -> Option<AgentEntry> {
        self.entries.remove(id)
    }

    /// 核心 agent 主动心跳：只允许四组核心身份，subagent 心跳走 touch_subagent_heartbeat。
    pub fn touch_core_heartbeat(&mut self, id: &str, source: &str) -> Option<()> {
        let entry = self.entries.get_mut(id)?;
        if entry.identity.is_subagent() {
            return None;
        }
        entry.touch_heartbeat(Some(source));
        Some(())
    }

    /// 移除核心 agent（思考引擎实例完成后退池）；拒绝误删 subagent。
    pub fn remove_core(&mut self, id: &str) -> Option<AgentEntry> {
        let entry = self.entries.get(id)?;
        if entry.identity.is_subagent() {
            return None;
        }
        self.entries.remove(id)
    }

    pub fn get(&self, id: &str) -> Option<&AgentEntry> {
        self.entries.get(id)
    }

    pub fn snapshot(&self) -> Vec<&AgentEntry> {
        self.entries.values().collect()
    }

    pub fn count_by_status(&self) -> HashMap<AgentStatus, usize> {
        let mut counts = HashMap::new();
        for e in self.entries.values() {
            *counts.entry(e.status.clone()).or_insert(0) += 1;
        }
        counts
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    // ---------------------------------------------------------------------
    // subagent 专用操作边界（业务侧唯一入口；核心身份不受影响）
    // ---------------------------------------------------------------------

    /// 注册一个 subagent：写入运行时 AgentEntry（Subagent 身份）+ 展示状态。
    /// 返回 None 表示 id 已存在（拒绝重复注册）。
    pub fn register_subagent(
        &mut self,
        state: SubagentRuntimeState,
        status: AgentStatus,
    ) -> Option<()> {
        if self.entries.contains_key(&state.subagent_id) {
            return None;
        }
        let entry = AgentEntry {
            id: state.subagent_id.clone(),
            identity: subagent_identity_for(&status, &state.subagent_id),
            status,
            created_at: std::time::Instant::now(),
            last_heartbeat: std::time::Instant::now(),
            heartbeat_source: None,
        };
        self.entries.insert(state.subagent_id.clone(), entry);
        self.subagent_states
            .insert(state.subagent_id.clone(), state);
        Some(())
    }

    /// 更新 subagent 运行身份状态（idle/running/pending）；非 subagent 或未知 id 返回 None。
    pub fn update_subagent_status(&mut self, id: &str, status: AgentStatus) -> Option<()> {
        let entry = self.entries.get_mut(id)?;
        if !entry.identity.is_subagent() {
            return None;
        }
        entry.status = status.clone();
        entry.identity = subagent_identity_for(&status, id);
        Some(())
    }

    /// subagent 主动心跳上报（1 秒频率由 subagent 实例侧保证）；状态保持，不改变。
    pub fn touch_subagent_heartbeat(&mut self, id: &str, source: &str) -> Option<()> {
        let entry = self.entries.get_mut(id)?;
        if !entry.identity.is_subagent() {
            return None;
        }
        entry.touch_heartbeat(Some(source));
        Some(())
    }

    /// 更新 subagent 持久生命周期（展示状态）。
    pub fn set_subagent_lifecycle(
        &mut self,
        id: &str,
        lifecycle: crate::agent::execution_types::SubagentLifecycle,
    ) -> Option<()> {
        self.subagent_states.get_mut(id).map(|s| {
            s.lifecycle = lifecycle;
        })
    }

    /// 更新 subagent last_output 有界截断文本（供快照/思考引擎异步感知）。
    pub fn set_subagent_last_output(&mut self, id: &str, truncated: Option<String>) -> Option<()> {
        self.subagent_states.get_mut(id).map(|s| {
            s.last_output_truncated = truncated;
        })
    }

    /// 移除 subagent（tombstoned 移出池）：删除运行时身份 + 展示状态。
    /// 非 subagent（核心身份）或未知 id 一律 no-op，防止误删核心平台。
    pub fn remove_subagent(&mut self, id: &str) -> Option<SubagentRuntimeState> {
        if !self.is_subagent(id) && !self.subagent_states.contains_key(id) {
            return None;
        }
        self.entries.remove(id);
        self.subagent_states.remove(id)
    }

    /// subagent 展示状态快照（思考引擎自感知来源）。
    pub fn subagent_snapshot(&self) -> Vec<SubagentRuntimeState> {
        self.subagent_states.values().cloned().collect()
    }

    /// 读取单个 subagent 展示状态。
    pub fn subagent_state(&self, id: &str) -> Option<&SubagentRuntimeState> {
        self.subagent_states.get(id)
    }

    /// 该 id 是否在池中作为 subagent 注册。
    pub fn is_subagent(&self, id: &str) -> bool {
        self.entries
            .get(id)
            .map(|entry| entry.identity.is_subagent())
            .unwrap_or(false)
    }
}

impl Default for InstanceRegistry {
    fn default() -> Self {
        Self::new()
    }
}

pub type SharedRegistry = Arc<RwLock<InstanceRegistry>>;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::execution_types::{SubagentLifecycle, SubagentStartup};

    fn make_entry(id: &str) -> AgentEntry {
        AgentEntry {
            id: id.into(),
            identity: AgentIdentity::SubagentRunning {
                agent_id: id.into(),
            },
            status: AgentStatus::Running,
            created_at: std::time::Instant::now(),
            last_heartbeat: std::time::Instant::now(),
            heartbeat_source: None,
        }
    }

    fn make_core_entry(id: &str) -> AgentEntry {
        AgentEntry {
            id: id.into(),
            identity: AgentIdentity::ExecutionPlatform,
            status: AgentStatus::Idle,
            created_at: std::time::Instant::now(),
            last_heartbeat: std::time::Instant::now(),
            heartbeat_source: None,
        }
    }

    fn subagent_state(id: &str, lifecycle: SubagentLifecycle) -> SubagentRuntimeState {
        SubagentRuntimeState {
            subagent_id: id.to_string(),
            lifecycle,
            last_output_truncated: None,
            trigger: None,
            startup: SubagentStartup::Normal,
            lifecycle_kind: crate::agent::execution_types::SubagentLifecycleKind::Temporary,
        }
    }

    #[test]
    fn registry_register_and_get() {
        let mut reg = InstanceRegistry::new();
        reg.register(make_entry("a1"));
        assert!(reg.get("a1").is_some());
        assert_eq!(reg.len(), 1);
    }

    #[test]
    fn registry_update_status() {
        let mut reg = InstanceRegistry::new();
        reg.register(make_core_entry("a1"));
        reg.update_status("a1", AgentStatus::Idle);
        assert_eq!(reg.get("a1").unwrap().status, AgentStatus::Idle);
    }

    #[test]
    fn generic_update_status_rejects_subagent_identity() {
        let mut reg = InstanceRegistry::new();
        reg.register(make_entry("a1"));
        // 通用状态更新必须拒绝 subagent 身份（subagent 走专用方法）。
        assert!(reg.update_status("a1", AgentStatus::Idle).is_none());
        assert_eq!(reg.get("a1").unwrap().status, AgentStatus::Running);
    }

    #[test]
    fn registry_remove() {
        let mut reg = InstanceRegistry::new();
        reg.register(make_entry("a1"));
        let removed = reg.remove("a1");
        assert!(removed.is_some());
        assert!(reg.is_empty());
    }

    #[test]
    fn registry_count_by_status() {
        let mut reg = InstanceRegistry::new();
        reg.register(make_entry("a1"));
        let mut e2 = make_entry("a2");
        e2.status = AgentStatus::Pending;
        reg.register(e2);

        let counts = reg.count_by_status();
        assert_eq!(counts.get(&AgentStatus::Running).copied().unwrap_or(0), 1);
        assert_eq!(counts.get(&AgentStatus::Pending).copied().unwrap_or(0), 1);
    }

    #[test]
    fn registry_snapshot() {
        let mut reg = InstanceRegistry::new();
        reg.register(make_entry("a1"));
        reg.register(make_entry("a2"));
        assert_eq!(reg.snapshot().len(), 2);
    }

    #[test]
    fn registry_entry_heartbeat_fields_and_touch() {
        let mut entry = make_entry("a1");
        let before = entry.last_heartbeat;
        assert_eq!(entry.heartbeat_source, None);

        entry.touch_heartbeat(Some("runtime-worker-1"));
        assert_eq!(entry.heartbeat_source.as_deref(), Some("runtime-worker-1"));
        assert!(entry.last_heartbeat >= before);
    }

    #[test]
    fn subagent_register_keeps_status_and_identity_in_sync() {
        let mut reg = InstanceRegistry::new();
        reg.register_subagent(
            subagent_state("sg_1", SubagentLifecycle::Idle),
            AgentStatus::Idle,
        )
        .expect("register once");

        let entry = reg.get("sg_1").expect("entry present");
        assert!(entry.identity.is_subagent());
        assert_eq!(entry.status, AgentStatus::Idle);

        // 重复注册被拒绝。
        assert!(reg
            .register_subagent(
                subagent_state("sg_1", SubagentLifecycle::Idle),
                AgentStatus::Idle
            )
            .is_none());
    }

    #[test]
    fn subagent_status_update_flips_identity_variant() {
        let mut reg = InstanceRegistry::new();
        reg.register_subagent(
            subagent_state("sg_1", SubagentLifecycle::Idle),
            AgentStatus::Idle,
        )
        .unwrap();

        reg.update_subagent_status("sg_1", AgentStatus::Running)
            .unwrap();
        let entry = reg.get("sg_1").unwrap();
        assert_eq!(entry.status, AgentStatus::Running);
        assert!(matches!(
            entry.identity,
            AgentIdentity::SubagentRunning { ref agent_id } if agent_id == "sg_1"
        ));

        // 非 subagent / 未知 id 拒绝。
        assert!(reg
            .update_subagent_status("missing", AgentStatus::Idle)
            .is_none());
        reg.register(make_core_entry("core-1"));
        assert!(reg
            .update_subagent_status("core-1", AgentStatus::Idle)
            .is_none());
    }

    #[test]
    fn subagent_heartbeat_touches_and_keeps_status() {
        let mut reg = InstanceRegistry::new();
        reg.register_subagent(
            subagent_state("sg_1", SubagentLifecycle::Running),
            AgentStatus::Running,
        )
        .unwrap();
        let before = reg.get("sg_1").unwrap().last_heartbeat;

        reg.touch_subagent_heartbeat("sg_1", "subagent-runtime")
            .unwrap();
        let entry = reg.get("sg_1").unwrap();
        assert!(entry.last_heartbeat >= before);
        assert_eq!(entry.heartbeat_source.as_deref(), Some("subagent-runtime"));
        assert_eq!(entry.status, AgentStatus::Running, "心跳保持状态不变");

        // 非 subagent 心跳被拒绝。
        reg.register(make_core_entry("core-1"));
        assert!(reg.touch_subagent_heartbeat("core-1", "x").is_none());
    }

    #[test]
    fn subagent_lifecycle_and_last_output_display() {
        let mut reg = InstanceRegistry::new();
        reg.register_subagent(
            subagent_state("sg_1", SubagentLifecycle::Running),
            AgentStatus::Running,
        )
        .unwrap();

        reg.set_subagent_lifecycle("sg_1", SubagentLifecycle::Failed)
            .unwrap();
        reg.set_subagent_last_output("sg_1", Some("failed: timeout".to_string()))
            .unwrap();

        let state = reg.subagent_state("sg_1").unwrap();
        assert_eq!(state.lifecycle, SubagentLifecycle::Failed);
        assert_eq!(
            state.last_output_truncated.as_deref(),
            Some("failed: timeout")
        );

        // failed 实例仍以 idle 身份保留在池。
        reg.update_subagent_status("sg_1", AgentStatus::Idle)
            .unwrap();
        assert_eq!(reg.get("sg_1").unwrap().status, AgentStatus::Idle);
        assert!(reg.subagent_state("sg_1").is_some());
    }

    #[test]
    fn subagent_remove_purges_both_views() {
        let mut reg = InstanceRegistry::new();
        reg.register_subagent(
            subagent_state("sg_1", SubagentLifecycle::Idle),
            AgentStatus::Idle,
        )
        .unwrap();
        let removed = reg.remove_subagent("sg_1").unwrap();
        assert_eq!(removed.subagent_id, "sg_1");
        assert!(reg.get("sg_1").is_none());
        assert!(reg.subagent_state("sg_1").is_none());
        assert!(!reg.is_subagent("sg_1"));
    }

    #[test]
    fn subagent_snapshot_contains_display_fields() {
        let mut reg = InstanceRegistry::new();
        let mut state = subagent_state("sg_1", SubagentLifecycle::Running);
        state.trigger = Some(serde_json::json!({"type": "condition"}));
        state.startup = SubagentStartup::Condition;
        reg.register_subagent(state, AgentStatus::Running).unwrap();

        let snap = reg.subagent_snapshot();
        assert_eq!(snap.len(), 1);
        assert_eq!(snap[0].lifecycle, SubagentLifecycle::Running);
        assert_eq!(snap[0].startup, SubagentStartup::Condition);
        assert_eq!(
            snap[0].trigger,
            Some(serde_json::json!({"type": "condition"}))
        );
    }
}
