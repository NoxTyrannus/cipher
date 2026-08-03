use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

use super::communication::{NodeResult, NodeStatus};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SubAgentStatus {
    Pending,

    Running,

    Completed,

    Failed,
}

#[derive(Debug, Clone)]
pub struct SubAgentInstance {
    pub id: String,
    pub node_id: String,
    pub task_context: String,
    pub capability_ids: Vec<String>,
    pub status: SubAgentStatus,
    pub logs: Vec<String>,
    pub tool_call_count: u32,
    pub error: Option<String>,
}

impl SubAgentInstance {
    fn new(node_id: &str, task_context: &str, capability_ids: Vec<String>) -> Self {
        Self {
            id: String::new(),
            node_id: node_id.to_string(),
            task_context: task_context.to_string(),
            capability_ids,
            status: SubAgentStatus::Pending,
            logs: Vec::new(),
            tool_call_count: 0,
            error: None,
        }
    }

    pub fn append_log(&mut self, line: &str) {
        self.logs.push(line.to_string());
    }

    pub fn into_node_result(self) -> NodeResult {
        let status = match self.status {
            SubAgentStatus::Completed => NodeStatus::Completed,
            _ => NodeStatus::Failed,
        };
        NodeResult {
            node_id: self.node_id,
            status,
            summary: self.logs.last().cloned().unwrap_or_default(),
            error: self.error,
            tool_call_count: self.tool_call_count,
            tool_call_logs: self.logs,
        }
    }
}

pub struct SubAgentPool {
    counter: AtomicUsize,
    instances: Mutex<HashMap<String, SubAgentInstance>>,
}

impl SubAgentPool {
    pub fn new() -> Self {
        Self {
            counter: AtomicUsize::new(0),
            instances: Mutex::new(HashMap::new()),
        }
    }

    pub fn spawn(
        &self,
        node_id: &str,
        task_context: &str,
        capability_ids: Vec<String>,
    ) -> SubAgentHandle {
        let n = self.counter.fetch_add(1, Ordering::SeqCst);
        let id = format!("subagent-{n}");
        let mut inst = SubAgentInstance::new(node_id, task_context, capability_ids);
        inst.id = id.clone();
        let handle = SubAgentHandle {
            id: id.clone(),
            node_id: node_id.to_string(),
        };
        self.instances
            .lock()
            .expect("subagent pool poisoned")
            .insert(id, inst);
        handle
    }

    pub fn get(&self, id: &str) -> Option<SubAgentInstance> {
        self.instances
            .lock()
            .expect("subagent pool poisoned")
            .get(id)
            .cloned()
    }

    pub fn update_status(&self, id: &str, status: SubAgentStatus) {
        if let Some(inst) = self
            .instances
            .lock()
            .expect("subagent pool poisoned")
            .get_mut(id)
        {
            inst.status = status;
        }
    }

    pub fn append_log(&self, id: &str, line: &str) {
        if let Some(inst) = self
            .instances
            .lock()
            .expect("subagent pool poisoned")
            .get_mut(id)
        {
            inst.logs.push(line.to_string());
        }
    }

    pub fn mark_failed(&self, id: &str, error: &str) {
        if let Some(inst) = self
            .instances
            .lock()
            .expect("subagent pool poisoned")
            .get_mut(id)
        {
            inst.status = SubAgentStatus::Failed;
            inst.error = Some(error.to_string());
        }
    }

    pub fn mark_completed(&self, id: &str, tool_call_count: u32) {
        if let Some(inst) = self
            .instances
            .lock()
            .expect("subagent pool poisoned")
            .get_mut(id)
        {
            inst.status = SubAgentStatus::Completed;
            inst.tool_call_count = tool_call_count;
        }
    }

    pub fn collect_results(&self) -> Vec<NodeResult> {
        let mut map = self.instances.lock().expect("subagent pool poisoned");
        let mut results: Vec<NodeResult> = Vec::new();
        for (_, inst) in map.drain() {
            results.push(inst.into_node_result());
        }
        results
    }

    pub fn count(&self) -> usize {
        self.instances.lock().expect("subagent pool poisoned").len()
    }

    pub fn find_by_node_id(&self, node_id: &str) -> Option<String> {
        self.instances
            .lock()
            .expect("subagent pool poisoned")
            .iter()
            .find(|(_, inst)| inst.node_id == node_id)
            .map(|(id, _)| id.clone())
    }

    pub fn execute(
        &self,
        id: &str,
        dispatcher: &super::dispatcher::CapabilityDispatcher,
        tool_calls: &[crate::logic::model::provider::ToolCall],
    ) -> NodeResult {
        let inst = match self.get(id) {
            Some(inst) => inst,
            None => {
                return NodeResult {
                    node_id: id.to_string(),
                    status: NodeStatus::Failed,
                    summary: String::new(),
                    error: Some(format!("subagent not found: {id}")),
                    tool_call_count: 0,
                    tool_call_logs: vec![],
                };
            }
        };

        let allowed_caps: std::collections::HashSet<&str> =
            inst.capability_ids.iter().map(|s| s.as_str()).collect();

        self.update_status(id, SubAgentStatus::Running);

        let mut errors: Vec<String> = Vec::new();
        for call in tool_calls {
            if !allowed_caps.contains(call.name.as_str()) {
                let msg = format!(
                    "PERMISSION_DENIED: '{}' not in assigned capability_ids {:?}",
                    call.name, inst.capability_ids
                );
                self.append_log(id, &msg);
                errors.push(msg);
                continue;
            }

            self.append_log(id, &format!("DISPATCH: {} (id={})", call.name, call.id));
            match dispatcher.dispatch(call) {
                Ok(output) => {
                    self.append_log(id, &format!("OK: {} → {output}", call.name));
                }
                Err(e) => {
                    let msg = format!("ERR: {} → {e}", call.name);
                    self.append_log(id, &msg);
                    errors.push(msg);
                }
            }
        }

        let tool_call_count = tool_calls.len() as u32;
        if errors.is_empty() {
            self.mark_completed(id, tool_call_count);
        } else {
            let error_msg = errors.join("; ");
            self.mark_failed(id, &error_msg);

            if let Some(inst) = self
                .instances
                .lock()
                .expect("subagent pool poisoned")
                .get_mut(id)
            {
                inst.tool_call_count = tool_call_count;
            }
        }

        let inst = self.get(id).ok_or_else(|| {
            crate::common::AgentError::NotFound(format!("subagent {id} not found after execute"))
        });
        match inst {
            Ok(inst) => inst.into_node_result(),
            Err(e) => NodeResult {
                node_id: id.to_string(),
                status: NodeStatus::Failed,
                summary: String::new(),
                error: Some(e.to_string()),
                tool_call_count: 0,
                tool_call_logs: vec![],
            },
        }
    }
}

impl Default for SubAgentPool {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Clone)]
pub struct SubAgentHandle {
    pub id: String,
    pub node_id: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn spawn_creates_instance() {
        let p = SubAgentPool::new();
        let h = p.spawn("n1", "do task", vec!["cap1".into()]);
        assert!(h.id.starts_with("subagent-"));
        assert_eq!(h.node_id, "n1");

        let inst = p.get(&h.id).unwrap();
        assert_eq!(inst.status, SubAgentStatus::Pending);
        assert_eq!(inst.task_context, "do task");
    }

    #[test]
    fn update_status_and_log() {
        let p = SubAgentPool::new();
        let h = p.spawn("n1", "task", vec![]);

        p.update_status(&h.id, SubAgentStatus::Running);
        assert_eq!(p.get(&h.id).unwrap().status, SubAgentStatus::Running);

        p.append_log(&h.id, "step 1 done");
        p.append_log(&h.id, "step 2 done");
        assert_eq!(p.get(&h.id).unwrap().logs.len(), 2);
    }

    #[test]
    fn mark_completed_and_collect() {
        let p = SubAgentPool::new();
        let h1 = p.spawn("n1", "task1", vec!["cap1".into()]);
        let h2 = p.spawn("n2", "task2", vec!["cap2".into()]);

        p.mark_completed(&h1.id, 5);
        p.mark_failed(&h2.id, "error: timeout");

        let results = p.collect_results();
        assert_eq!(results.len(), 2);

        let r1 = results.iter().find(|r| r.node_id == "n1").unwrap();
        assert_eq!(r1.status, NodeStatus::Completed);
        assert_eq!(r1.tool_call_count, 5);

        let r2 = results.iter().find(|r| r.node_id == "n2").unwrap();
        assert_eq!(r2.status, NodeStatus::Failed);
        assert!(r2.error.as_deref().unwrap().contains("timeout"));
    }

    #[test]
    fn find_by_node_id() {
        let p = SubAgentPool::new();
        let h = p.spawn("node-a", "task", vec![]);
        let found = p.find_by_node_id("node-a").unwrap();
        assert_eq!(found, h.id);
        assert!(p.find_by_node_id("nonexistent").is_none());
    }

    #[test]
    fn count_grows_on_spawn() {
        let p = SubAgentPool::new();
        assert_eq!(p.count(), 0);
        p.spawn("n1", "t1", vec![]);
        p.spawn("n2", "t2", vec![]);
        assert_eq!(p.count(), 2);
    }

    #[test]
    fn handles_arc_shareable() {
        let p = Arc::new(SubAgentPool::new());
        let p2 = Arc::clone(&p);
        let h = p.spawn("n1", "t1", vec![]);
        let g = p2.get(&h.id).unwrap();
        assert_eq!(g.id, h.id);
    }

    #[test]
    fn instance_into_node_result_preserves_error() {
        let mut inst = SubAgentInstance::new("n1", "task", vec![]);
        inst.append_log("step 1: attempted HTTP call");
        inst.append_log("step 2: connection refused");
        inst.status = SubAgentStatus::Failed;
        inst.error = Some("something broke".into());
        inst.tool_call_count = 3;

        let nr = inst.into_node_result();
        assert_eq!(nr.node_id, "n1");
        assert_eq!(nr.status, NodeStatus::Failed);
        assert_eq!(nr.error.as_deref(), Some("something broke"));
        assert_eq!(nr.tool_call_count, 3);
        assert_eq!(
            nr.tool_call_logs.len(),
            2,
            "tool_call_logs should be preserved"
        );
        assert!(nr.tool_call_logs[0].contains("HTTP"));
    }

    #[test]
    fn subagent_execute_single_tool() {
        use crate::agent::dispatcher::CapabilityDispatcher;
        use crate::data::duckdb::loader::BaseCapabilityRow;
        use crate::data::duckdb::Registry;
        use crate::logic::capability::base::BaseCapability;
        use crate::logic::capability::executor::CapabilityExecutor;
        use crate::logic::model::provider::ToolCall;

        struct EchoCap;
        impl BaseCapability for EchoCap {
            fn id(&self) -> &'static str {
                "echo"
            }
            fn name(&self) -> &'static str {
                "Echo"
            }
            fn execute(
                &self,
                input: &crate::logic::capability::base::Schema,
            ) -> Result<crate::logic::capability::base::Schema, crate::common::AgentError>
            {
                Ok(input.clone())
            }
        }

        let mut reg = Registry::new();
        reg.base_capabilities.insert(
            "echo".to_string(),
            BaseCapabilityRow {
                id: "echo".to_string(),
                name: "Echo".to_string(),
                cap_type: "function".to_string(),
                description: "Echoes its input".to_string(),
                schema_in: serde_json::json!({}),
                schema_out: serde_json::json!({}),
                executor: "echo".to_string(),
                version: "1".to_string(),
                enabled: true,
                tombstoned_at: None,
                metadata: None,
            },
        );
        let mut executor = CapabilityExecutor::new();
        executor.register(Arc::new(EchoCap));
        let dispatcher = CapabilityDispatcher::new(&reg, &executor);

        let pool = SubAgentPool::new();
        let handle = pool.spawn("node-1", "echo the input", vec!["echo".into()]);

        let tool_calls = vec![ToolCall {
            id: "tc-1".to_string(),
            name: "echo".to_string(),
            arguments: serde_json::json!({"message": "hello from subagent"}),
        }];
        let result = pool.execute(&handle.id, &dispatcher, &tool_calls);

        assert_eq!(result.node_id, "node-1");
        assert_eq!(result.status, NodeStatus::Completed);
        assert_eq!(result.tool_call_count, 1);
        assert!(
            result.error.is_none(),
            "should have no error, got: {:?}",
            result.error
        );

        assert!(
            result.summary.contains("OK: echo →"),
            "summary should contain OK log, got: {}",
            result.summary
        );

        assert_eq!(
            result.tool_call_logs.len(),
            2,
            "expected DISPATCH + OK = 2 log lines"
        );
        assert!(
            result.tool_call_logs[0].contains("DISPATCH: echo"),
            "first log should be DISPATCH, got: {}",
            result.tool_call_logs[0]
        );
        assert!(
            result.tool_call_logs[1].contains("OK: echo →"),
            "second log should be OK, got: {}",
            result.tool_call_logs[1]
        );

        let inst = pool.get(&handle.id).unwrap();
        assert_eq!(inst.status, SubAgentStatus::Completed);
        assert_eq!(inst.tool_call_count, 1);
    }

    #[test]
    fn subagent_cannot_use_unassigned_capability() {
        use crate::agent::dispatcher::CapabilityDispatcher;
        use crate::data::duckdb::loader::BaseCapabilityRow;
        use crate::data::duckdb::Registry;
        use crate::logic::capability::base::BaseCapability;
        use crate::logic::capability::executor::CapabilityExecutor;
        use crate::logic::model::provider::ToolCall;

        struct EchoCap;
        impl BaseCapability for EchoCap {
            fn id(&self) -> &'static str {
                "echo"
            }
            fn name(&self) -> &'static str {
                "Echo"
            }
            fn execute(
                &self,
                input: &crate::logic::capability::base::Schema,
            ) -> Result<crate::logic::capability::base::Schema, crate::common::AgentError>
            {
                Ok(input.clone())
            }
        }

        struct HttpCap;
        impl BaseCapability for HttpCap {
            fn id(&self) -> &'static str {
                "http"
            }
            fn name(&self) -> &'static str {
                "Http"
            }
            fn execute(
                &self,
                input: &crate::logic::capability::base::Schema,
            ) -> Result<crate::logic::capability::base::Schema, crate::common::AgentError>
            {
                Ok(input.clone())
            }
        }

        let mut reg = Registry::new();
        reg.base_capabilities.insert(
            "echo".to_string(),
            BaseCapabilityRow {
                id: "echo".to_string(),
                name: "Echo".to_string(),
                cap_type: "function".to_string(),
                description: "Echoes its input".to_string(),
                schema_in: serde_json::json!({}),
                schema_out: serde_json::json!({}),
                executor: "echo".to_string(),
                version: "1".to_string(),
                enabled: true,
                tombstoned_at: None,
                metadata: None,
            },
        );
        reg.base_capabilities.insert(
            "http".to_string(),
            BaseCapabilityRow {
                id: "http".to_string(),
                name: "Http".to_string(),
                cap_type: "function".to_string(),
                description: "Makes test HTTP calls".to_string(),
                schema_in: serde_json::json!({}),
                schema_out: serde_json::json!({}),
                executor: "http".to_string(),
                version: "1".to_string(),
                enabled: true,
                tombstoned_at: None,
                metadata: None,
            },
        );

        let mut executor = CapabilityExecutor::new();
        executor.register(Arc::new(EchoCap));
        executor.register(Arc::new(HttpCap));
        let dispatcher = CapabilityDispatcher::new(&reg, &executor);

        let pool = SubAgentPool::new();
        let handle = pool.spawn("node-1", "test permission gate", vec!["echo".into()]);

        let tool_calls = vec![ToolCall {
            id: "tc-1".to_string(),
            name: "http".to_string(),
            arguments: serde_json::json!({"url": "https://example.com"}),
        }];
        let result = pool.execute(&handle.id, &dispatcher, &tool_calls);

        assert_eq!(result.node_id, "node-1");
        assert_eq!(result.status, NodeStatus::Failed);
        assert_eq!(result.tool_call_count, 1);
        assert!(
            result
                .error
                .as_deref()
                .unwrap()
                .contains("PERMISSION_DENIED"),
            "error should contain PERMISSION_DENIED, got: {:?}",
            result.error
        );
        assert!(
            result.error.as_deref().unwrap().contains("http"),
            "error should mention 'http', got: {:?}",
            result.error
        );
        assert!(
            result.error.as_deref().unwrap().contains("capability_ids"),
            "error should mention capability_ids, got: {:?}",
            result.error
        );

        assert_eq!(result.tool_call_logs.len(), 1);
        assert!(
            result.tool_call_logs[0].contains("PERMISSION_DENIED"),
            "log should contain PERMISSION_DENIED, got: {}",
            result.tool_call_logs[0]
        );

        let inst = pool.get(&handle.id).unwrap();
        assert_eq!(inst.status, SubAgentStatus::Failed);
    }
}
