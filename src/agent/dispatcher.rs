use super::decision::Decision;
use crate::common::{AgentError, Result};
use crate::data::duckdb::Registry;
use crate::logic::capability::composite::CompositeNode;
use crate::logic::capability::executor::CapabilityExecutor;
use crate::logic::capability::service::{CapabilityCall, CapabilityService, ProviderToolSet};
use crate::logic::model::provider::{LlmRequest, ToolCall};
use std::collections::HashMap;

#[derive(Debug, Clone)]
pub struct AuthorizedProviderTools {
    agent_id: String,
    tool_set: ProviderToolSet,
}

impl AuthorizedProviderTools {
    pub fn agent_id(&self) -> &str {
        &self.agent_id
    }

    pub fn tools(&self) -> &[serde_json::Value] {
        self.tool_set.tools()
    }

    pub fn apply_to_request(&self, request: &mut LlmRequest) {
        request.tools = self.tools().to_vec();
    }

    pub fn normalize(&self, call: &ToolCall) -> Result<CapabilityCall> {
        self.tool_set.normalize(&call.name, call.arguments.clone())
    }

    pub fn dispatch(
        &self,
        dispatcher: &CapabilityDispatcher<'_>,
        call: &ToolCall,
    ) -> Result<serde_json::Value> {
        dispatcher.dispatch_provider_call(&self.agent_id, &self.tool_set, call)
    }
}

pub struct CapabilityDispatcher<'a> {
    registry: &'a Registry,
    executor: &'a CapabilityExecutor,
}

impl<'a> CapabilityDispatcher<'a> {
    pub fn new(registry: &'a Registry, executor: &'a CapabilityExecutor) -> Self {
        Self { registry, executor }
    }

    pub fn dispatch_authorized(
        &self,
        agent_id: &str,
        call: &CapabilityCall,
    ) -> Result<serde_json::Value> {
        CapabilityService::new(self.registry, self.executor)?
            .execute_for_agent(agent_id, call)
            .map(|result| result.output)
    }

    pub fn authorize_provider_tools(&self, agent_id: &str) -> Result<AuthorizedProviderTools> {
        let tool_set = CapabilityService::new(self.registry, self.executor)?
            .provider_tools_for_agent(agent_id)?;
        Ok(AuthorizedProviderTools {
            agent_id: agent_id.to_string(),
            tool_set,
        })
    }

    pub fn dispatch_provider_call(
        &self,
        agent_id: &str,
        tool_set: &ProviderToolSet,
        call: &ToolCall,
    ) -> Result<serde_json::Value> {
        let normalized = tool_set.normalize(&call.name, call.arguments.clone())?;
        self.dispatch_authorized(agent_id, &normalized)
    }

    pub fn dispatch(&self, call: &ToolCall) -> Result<serde_json::Value> {
        if self.registry.base_capabilities.contains_key(&call.name) {
            return self
                .executor
                .execute(&call.name, self.registry, &call.arguments);
        }
        if let Some(composite) = self.registry.composite_capabilities.get(&call.name) {
            return self.expand_composite_dag(&composite.id, &call.arguments);
        }
        if self.registry.usage_methods.contains_key(&call.name) {
            return Err(AgentError::Parse(format!(
                "usage_method '{}' 是 LLM-facing 提示, 不能当 tool 调",
                call.name
            )));
        }
        Err(AgentError::NotFound(format!(
            "tool_call capability: {}",
            call.name
        )))
    }

    pub fn dispatch_all(&self, calls: &[ToolCall]) -> Vec<Result<serde_json::Value>> {
        calls.iter().map(|c| self.dispatch(c)).collect()
    }

    fn expand_composite_dag(
        &self,
        composite_id: &str,
        input: &serde_json::Value,
    ) -> Result<serde_json::Value> {
        let composite = self
            .registry
            .composite_capabilities
            .get(composite_id)
            .ok_or_else(|| AgentError::NotFound(format!("composite capability: {composite_id}")))?;

        let nodes: Vec<CompositeNode> = serde_json::from_value(composite.dag.clone())
            .map_err(|e| AgentError::Parse(format!("composite {composite_id} dag parse: {e}")))?;

        if nodes.is_empty() {
            return Ok(serde_json::json!({
                "composite": composite_id,
                "steps": [],
                "final": null
            }));
        }

        let mut node_idx: HashMap<&str, usize> = HashMap::new();
        for (i, node) in nodes.iter().enumerate() {
            node_idx.insert(&node.id, i);
        }

        let mut in_degree = vec![0usize; nodes.len()];

        let mut dependents: Vec<Vec<usize>> = vec![vec![]; nodes.len()];

        for (i, node) in nodes.iter().enumerate() {
            for dep in &node.depends_on {
                match node_idx.get(dep.as_str()) {
                    Some(&dep_idx) => {
                        in_degree[i] += 1;
                        dependents[dep_idx].push(i);
                    }
                    None => {
                        return Err(AgentError::Parse(format!(
                            "composite {composite_id}: node '{}' depends on unknown node '{dep}'",
                            node.id
                        )));
                    }
                }
            }
        }

        let mut queue: Vec<usize> = in_degree
            .iter()
            .enumerate()
            .filter(|(_, &deg)| deg == 0)
            .map(|(i, _)| i)
            .collect();

        let mut sorted: Vec<usize> = Vec::with_capacity(nodes.len());
        while let Some(idx) = queue.pop() {
            sorted.push(idx);
            for &dep_idx in &dependents[idx] {
                in_degree[dep_idx] -= 1;
                if in_degree[dep_idx] == 0 {
                    queue.push(dep_idx);
                }
            }
        }

        if sorted.len() != nodes.len() {
            return Err(AgentError::Parse(format!(
                "composite {composite_id}: DAG cycle detected (sorted {} of {} nodes)",
                sorted.len(),
                nodes.len()
            )));
        }

        let mut results: HashMap<&str, serde_json::Value> = HashMap::new();
        let mut steps: Vec<serde_json::Value> = Vec::with_capacity(nodes.len());

        for &idx in &sorted {
            let node = &nodes[idx];

            let resolved_args = if let Some(ref args) = node.args {
                let mut resolved = args.clone();

                if let Some(obj) = resolved.as_object_mut() {
                    if let Some(val) = obj.remove("$input") {
                        if val.is_null() {
                            obj.insert("input".to_string(), input.clone());
                        }
                    }

                    for dep_id in &node.depends_on {
                        if let Some(dep_result) = results.get(dep_id.as_str()) {
                            obj.insert(format!("${dep_id}"), dep_result.clone());
                        }
                    }
                }
                resolved
            } else {
                if let Some(first_dep) = node.depends_on.first() {
                    if let Some(dep_result) = results.get(first_dep.as_str()) {
                        serde_json::json!({"input": dep_result})
                    } else {
                        input.clone()
                    }
                } else {
                    input.clone()
                }
            };

            match self
                .executor
                .execute(&node.base_capability, self.registry, &resolved_args)
            {
                Ok(output) => {
                    results.insert(&node.id, output.clone());
                    steps.push(serde_json::json!({
                        "node": node.id,
                        "base_capability": node.base_capability,
                        "output": output
                    }));
                }
                Err(e) => {
                    return Err(AgentError::NotImplemented(format!(
                        "composite {composite_id} node '{}' (base: {}) failed: {e}",
                        node.id, node.base_capability
                    )));
                }
            }
        }

        let final_output = sorted
            .last()
            .and_then(|&last_idx| results.get(nodes[last_idx].id.as_str()))
            .cloned()
            .unwrap_or(serde_json::Value::Null);

        Ok(serde_json::json!({
            "composite": composite_id,
            "steps": steps,
            "final": final_output
        }))
    }

    pub fn dispatch_decision(&self, dec: &Decision) -> Result<serde_json::Value> {
        if dec.action != "call_capability" {
            return Err(AgentError::NotImplemented(format!(
                "decision action: {} (iter61 只实现 call_capability)",
                dec.action
            )));
        }
        match &dec.tool_call {
            Some(tc) => self.dispatch(tc),
            None => Err(AgentError::Parse(
                "call_capability 决策缺 tool_call".to_string(),
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::logic::capability::base::BaseCapability;
    use std::sync::Arc;

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
        ) -> Result<crate::logic::capability::base::Schema> {
            Ok(input.clone())
        }
    }

    fn make_registry_with_echo() -> Registry {
        let mut reg = Registry::new();
        reg.base_capabilities.insert(
            "echo".to_string(),
            crate::data::duckdb::loader::BaseCapabilityRow {
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
        reg
    }

    fn make_executor_with_echo() -> CapabilityExecutor {
        let mut ex = CapabilityExecutor::new();
        ex.register(Arc::new(EchoCap));
        ex
    }

    #[test]
    fn dispatcher_routes_base_capability() {
        let reg = make_registry_with_echo();
        let exec = make_executor_with_echo();
        let disp = CapabilityDispatcher::new(&reg, &exec);
        let tc = ToolCall {
            id: "c1".to_string(),
            name: "echo".to_string(),
            arguments: serde_json::json!({"x": 42}),
        };
        let r = disp.dispatch(&tc).unwrap();
        assert_eq!(r, serde_json::json!({"x": 42}));
    }

    #[test]
    fn dispatcher_unknown_name_returns_not_found() {
        let reg = Registry::new();
        let exec = CapabilityExecutor::new();
        let disp = CapabilityDispatcher::new(&reg, &exec);
        let tc = ToolCall {
            id: "c1".to_string(),
            name: "nope".to_string(),
            arguments: serde_json::json!({}),
        };
        match disp.dispatch(&tc) {
            Err(AgentError::NotFound(msg)) => {
                assert!(msg.contains("nope"), "got: {msg}");
            }
            other => panic!("expected NotFound, got: {other:?}"),
        }
    }

    #[test]
    fn dispatcher_usage_method_rejected() {
        let mut reg = Registry::new();
        reg.usage_methods.insert(
            "u1".to_string(),
            crate::data::duckdb::loader::UsageMethodRow {
                id: "u1".to_string(),
                capability_id: "echo".to_string(),
                name: "U1".to_string(),
                prompt: "test".to_string(),
                examples: None,
                metadata: None,
            },
        );
        let exec = CapabilityExecutor::new();
        let disp = CapabilityDispatcher::new(&reg, &exec);
        let tc = ToolCall {
            id: "c1".to_string(),
            name: "u1".to_string(),
            arguments: serde_json::json!({}),
        };
        match disp.dispatch(&tc) {
            Err(AgentError::Parse(msg)) => {
                assert!(msg.contains("usage_method"), "got: {msg}");
            }
            other => panic!("expected Parse, got: {other:?}"),
        }
    }

    #[test]
    fn dispatcher_dispatch_decision_call_capability() {
        let reg = make_registry_with_echo();
        let exec = make_executor_with_echo();
        let disp = CapabilityDispatcher::new(&reg, &exec);
        let dec = Decision::call_capability(ToolCall {
            id: "c1".to_string(),
            name: "echo".to_string(),
            arguments: serde_json::json!({"y": 99}),
        });
        let r = disp.dispatch_decision(&dec).unwrap();
        assert_eq!(r, serde_json::json!({"y": 99}));
    }

    #[test]
    fn thinking_decision_routes_through_dispatcher() {
        use crate::agent::thinking::ThinkingFactory;
        use crate::logic::model::provider::LlmResponse;
        let reg = make_registry_with_echo();
        let exec = make_executor_with_echo();
        let disp = CapabilityDispatcher::new(&reg, &exec);
        let f = ThinkingFactory::new();
        let inst = f.create("task-1");
        let resp = LlmResponse {
            content: "".to_string(),
            tool_calls: vec![ToolCall {
                id: "c1".to_string(),
                name: "echo".to_string(),
                arguments: serde_json::json!({"z": 7}),
            }],
            usage: None,
        };
        let dec = inst.decision(&resp).expect("decision");
        assert_eq!(dec.action, "call_capability");
        let r = disp.dispatch_decision(&dec).unwrap();
        assert_eq!(r, serde_json::json!({"z": 7}));
    }

    #[test]
    fn thinking_decision_returns_none_when_no_tool_calls() {
        use crate::agent::thinking::ThinkingFactory;
        use crate::logic::model::provider::LlmResponse;
        let f = ThinkingFactory::new();
        let inst = f.create("task-1");
        let resp = LlmResponse {
            content: "just text".to_string(),
            tool_calls: vec![],
            usage: None,
        };
        assert!(inst.decision(&resp).is_none());
    }

    #[test]
    fn authorized_dispatch_uses_actor_tool_caps_and_authority_name() {
        let mut reg = make_registry_with_echo();
        reg.agents.insert(
            "agent-1".to_string(),
            crate::data::duckdb::loader::AgentRow {
                id: "agent-1".to_string(),
                name: "Agent 1".to_string(),
                mode: "unni".to_string(),
                prompt: None,
                tool_caps: vec!["echo".to_string()],
                config: None,
                display_name: None,
                is_default: false,
            },
        );
        let exec = make_executor_with_echo();
        let dispatcher = CapabilityDispatcher::new(&reg, &exec);

        let output = dispatcher
            .dispatch_authorized(
                "agent-1",
                &CapabilityCall {
                    capability_id: "echo".to_string(),
                    capability_name: "Echo".to_string(),
                    arguments: serde_json::json!({"authorized": true}),
                },
            )
            .unwrap();
        assert_eq!(output, serde_json::json!({"authorized": true}));
    }

    #[test]
    fn provider_alias_dispatch_normalizes_before_authorization() {
        let mut reg = make_registry_with_echo();
        reg.agents.insert(
            "agent-1".to_string(),
            crate::data::duckdb::loader::AgentRow {
                id: "agent-1".to_string(),
                name: "Agent 1".to_string(),
                mode: "unni".to_string(),
                prompt: None,
                tool_caps: vec!["echo".to_string()],
                config: None,
                display_name: None,
                is_default: false,
            },
        );
        let exec = make_executor_with_echo();
        let dispatcher = CapabilityDispatcher::new(&reg, &exec);
        let service = CapabilityService::new(&reg, &exec).unwrap();
        let tool_set = service.provider_tools_for_agent("agent-1").unwrap();
        let alias = tool_set.tools()[0]["function"]["name"]
            .as_str()
            .unwrap()
            .to_string();

        let output = dispatcher
            .dispatch_provider_call(
                "agent-1",
                &tool_set,
                &ToolCall {
                    id: "provider-call-1".to_string(),
                    name: alias,
                    arguments: serde_json::json!({"normalized": true}),
                },
            )
            .unwrap();
        assert_eq!(output, serde_json::json!({"normalized": true}));

        let error = dispatcher
            .dispatch_provider_call(
                "agent-1",
                &tool_set,
                &ToolCall {
                    id: "provider-call-2".to_string(),
                    name: "echo".to_string(),
                    arguments: serde_json::json!({}),
                },
            )
            .unwrap_err();
        assert!(matches!(error, AgentError::NotFound(_)));
    }

    #[test]
    fn authorized_provider_tools_bind_actor_request_and_dispatch() {
        let mut reg = make_registry_with_echo();
        reg.agents.insert(
            "agent-1".to_string(),
            crate::data::duckdb::loader::AgentRow {
                id: "agent-1".to_string(),
                name: "Agent 1".to_string(),
                mode: "unni".to_string(),
                prompt: None,
                tool_caps: vec!["echo".to_string()],
                config: None,
                display_name: None,
                is_default: true,
            },
        );
        let exec = make_executor_with_echo();
        let dispatcher = CapabilityDispatcher::new(&reg, &exec);
        let authorized = dispatcher.authorize_provider_tools("agent-1").unwrap();

        assert_eq!(authorized.agent_id(), "agent-1");
        let mut request = LlmRequest::default();
        authorized.apply_to_request(&mut request);
        assert_eq!(request.tools, authorized.tools());
        assert_eq!(request.tools.len(), 1);

        let alias = request.tools[0]["function"]["name"]
            .as_str()
            .unwrap()
            .to_string();
        let call = ToolCall {
            id: "provider-call".to_string(),
            name: alias,
            arguments: serde_json::json!({"bound": true}),
        };
        let normalized = authorized.normalize(&call).unwrap();
        assert_eq!(normalized.capability_id, "echo");
        assert_eq!(normalized.capability_name, "Echo");
        assert_eq!(
            authorized.dispatch(&dispatcher, &call).unwrap(),
            serde_json::json!({"bound": true})
        );
    }

    fn make_registry_with_composite(dag: serde_json::Value) -> Registry {
        let mut reg = make_registry_with_echo();
        reg.composite_capabilities.insert(
            "pipeline".to_string(),
            crate::data::duckdb::loader::CompositeCapabilityRow {
                id: "pipeline".to_string(),
                name: "Pipeline".to_string(),
                description: "Test pipeline".to_string(),
                schema_in: Some(serde_json::json!({})),
                schema_out: Some(serde_json::json!({})),
                executor: Some("dag".to_string()),
                dag,
                version: "1".to_string(),
                enabled: true,
                tombstoned_at: None,
                metadata: None,
            },
        );
        reg
    }

    #[test]
    fn dispatcher_composite_dag_expansion() {
        let dag = serde_json::json!([
            {"id": "n1", "base_capability": "echo", "args": {"$input": null}, "depends_on": []},
            {"id": "n2", "base_capability": "echo", "args": {"$input": null, "$n1": null}, "depends_on": ["n1"]}
        ]);
        let reg = make_registry_with_composite(dag);
        let exec = make_executor_with_echo();
        let disp = CapabilityDispatcher::new(&reg, &exec);
        let tc = ToolCall {
            id: "c1".to_string(),
            name: "pipeline".to_string(),
            arguments: serde_json::json!({"x": 1}),
        };
        let r = disp.dispatch(&tc).unwrap();

        assert_eq!(r["composite"], "pipeline");
        assert_eq!(r["steps"].as_array().unwrap().len(), 2);
        assert_eq!(r["steps"][0]["node"], "n1");
        assert_eq!(r["steps"][0]["base_capability"], "echo");
        assert_eq!(r["steps"][1]["node"], "n2");
        assert_eq!(r["steps"][1]["base_capability"], "echo");

        assert!(r["final"].is_object());
        assert!(r["final"].get("input").is_some() || r["final"].get("x").is_some());
    }

    #[test]
    fn dispatcher_composite_dag_empty() {
        let dag = serde_json::json!([]);
        let reg = make_registry_with_composite(dag);
        let exec = make_executor_with_echo();
        let disp = CapabilityDispatcher::new(&reg, &exec);
        let tc = ToolCall {
            id: "c1".to_string(),
            name: "pipeline".to_string(),
            arguments: serde_json::json!({}),
        };
        let r = disp.dispatch(&tc).unwrap();
        assert_eq!(r["composite"], "pipeline");
        assert_eq!(r["steps"].as_array().unwrap().len(), 0);
        assert!(r["final"].is_null());
    }

    #[test]
    fn dispatcher_composite_dag_cycle_detected() {
        let dag = serde_json::json!([
            {"id": "n1", "base_capability": "echo", "args": {}, "depends_on": ["n2"]},
            {"id": "n2", "base_capability": "echo", "args": {}, "depends_on": ["n1"]}
        ]);
        let reg = make_registry_with_composite(dag);
        let exec = make_executor_with_echo();
        let disp = CapabilityDispatcher::new(&reg, &exec);
        let tc = ToolCall {
            id: "c1".to_string(),
            name: "pipeline".to_string(),
            arguments: serde_json::json!({}),
        };
        match disp.dispatch(&tc) {
            Err(AgentError::Parse(msg)) => {
                assert!(msg.contains("cycle"), "got: {msg}");
            }
            other => panic!("expected Parse(cycle), got: {other:?}"),
        }
    }

    #[test]
    fn dispatcher_composite_dag_unknown_dependency() {
        let dag = serde_json::json!([
            {"id": "n1", "base_capability": "echo", "args": {}, "depends_on": ["ghost"]}
        ]);
        let reg = make_registry_with_composite(dag);
        let exec = make_executor_with_echo();
        let disp = CapabilityDispatcher::new(&reg, &exec);
        let tc = ToolCall {
            id: "c1".to_string(),
            name: "pipeline".to_string(),
            arguments: serde_json::json!({}),
        };
        match disp.dispatch(&tc) {
            Err(AgentError::Parse(msg)) => {
                assert!(msg.contains("ghost"), "got: {msg}");
            }
            other => panic!("expected Parse(unknown dep), got: {other:?}"),
        }
    }

    #[test]
    fn dispatcher_composite_dag_unknown_base_capability() {
        let dag = serde_json::json!([
            {"id": "n1", "base_capability": "nonexistent", "args": {}, "depends_on": []}
        ]);
        let reg = make_registry_with_composite(dag);
        let exec = make_executor_with_echo();
        let disp = CapabilityDispatcher::new(&reg, &exec);
        let tc = ToolCall {
            id: "c1".to_string(),
            name: "pipeline".to_string(),
            arguments: serde_json::json!({}),
        };
        match disp.dispatch(&tc) {
            Err(AgentError::NotImplemented(msg)) => {
                assert!(msg.contains("nonexistent"), "got: {msg}");
            }
            other => panic!("expected NotImplemented, got: {other:?}"),
        }
    }

    #[test]
    fn dispatcher_composite_dag_single_node() {
        let dag = serde_json::json!([
            {"id": "n1", "base_capability": "echo", "args": {"$input": null}, "depends_on": []}
        ]);
        let reg = make_registry_with_composite(dag);
        let exec = make_executor_with_echo();
        let disp = CapabilityDispatcher::new(&reg, &exec);
        let tc = ToolCall {
            id: "c1".to_string(),
            name: "pipeline".to_string(),
            arguments: serde_json::json!({"hello": "world"}),
        };
        let r = disp.dispatch(&tc).unwrap();
        assert_eq!(r["steps"].as_array().unwrap().len(), 1);
        assert_eq!(r["steps"][0]["node"], "n1");

        assert!(r["final"].is_object());
    }

    #[test]
    fn dispatcher_composite_dag_parallel_branch() {
        let dag = serde_json::json!([
            {"id": "n1", "base_capability": "echo", "args": {"$input": null}, "depends_on": []},
            {"id": "n2", "base_capability": "echo", "args": {"data": "branch2"}, "depends_on": []},
            {"id": "n3", "base_capability": "echo", "args": {"$input": null, "$n1": null, "$n2": null}, "depends_on": ["n1", "n2"]}
        ]);
        let reg = make_registry_with_composite(dag);
        let exec = make_executor_with_echo();
        let disp = CapabilityDispatcher::new(&reg, &exec);
        let tc = ToolCall {
            id: "c1".to_string(),
            name: "pipeline".to_string(),
            arguments: serde_json::json!({"root": true}),
        };
        let r = disp.dispatch(&tc).unwrap();
        assert_eq!(r["steps"].as_array().unwrap().len(), 3);
        assert_eq!(r["composite"], "pipeline");

        assert!(r["final"].is_object());
    }
}
