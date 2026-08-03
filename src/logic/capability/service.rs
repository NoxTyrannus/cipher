use super::composite::CompositeNode;
use super::executor::CapabilityExecutor;
use crate::common::{AgentError, Result};
use crate::data::duckdb::loader::{
    BaseCapabilityRow, CompositeCapabilityRow, Registry, UsageMethodRow,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::{HashMap, VecDeque};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapabilityCall {
    pub capability_id: String,
    pub capability_name: String,
    pub arguments: Value,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapabilityResult {
    pub capability_id: String,
    pub capability_name: String,
    pub output: Value,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapabilityDefinition {
    pub capability_id: String,
    pub capability_name: String,
    pub description: String,
    pub input_schema: Value,
    pub output_schema: Value,
}

#[derive(Debug, Clone)]
pub struct ProviderToolSet {
    tools: Vec<Value>,
    aliases: HashMap<String, (String, String)>,
}

impl ProviderToolSet {
    pub fn tools(&self) -> &[Value] {
        &self.tools
    }

    pub fn normalize(&self, alias: &str, arguments: Value) -> Result<CapabilityCall> {
        let (capability_id, capability_name) = self
            .aliases
            .get(alias)
            .ok_or_else(|| AgentError::NotFound(format!("provider tool alias: {alias}")))?;
        Ok(CapabilityCall {
            capability_id: capability_id.clone(),
            capability_name: capability_name.clone(),
            arguments,
        })
    }
}

pub struct CapabilityService<'a> {
    registry: &'a Registry,
    executor: &'a CapabilityExecutor,
}

enum CapabilityContract<'a> {
    Base(&'a BaseCapabilityRow),
    Composite(&'a CompositeCapabilityRow),
    Usage(&'a UsageMethodRow),
}

impl<'a> CapabilityService<'a> {
    pub fn new(registry: &'a Registry, executor: &'a CapabilityExecutor) -> Result<Self> {
        registry.validate()?;
        Ok(Self { registry, executor })
    }

    pub fn definitions_for_agent(&self, agent_id: &str) -> Result<Vec<CapabilityDefinition>> {
        let agent =
            self.registry.agents.get(agent_id).ok_or_else(|| {
                AgentError::NotFound(format!("capability actor agent: {agent_id}"))
            })?;
        agent
            .tool_caps
            .iter()
            .map(
                |capability_id| match self.resolve_contract(capability_id)? {
                    CapabilityContract::Base(row) => {
                        validate_base_authority(row)?;
                        Ok(CapabilityDefinition {
                            capability_id: row.id.clone(),
                            capability_name: row.name.clone(),
                            description: row.description.clone(),
                            input_schema: row.schema_in.clone(),
                            output_schema: row.schema_out.clone(),
                        })
                    }
                    CapabilityContract::Composite(row) => {
                        validate_composite_authority(row)?;
                        Ok(CapabilityDefinition {
                            capability_id: row.id.clone(),
                            capability_name: row.name.clone(),
                            description: row.description.clone(),
                            input_schema: row.schema_in.clone().ok_or_else(|| {
                                AgentError::Bootstrap(format!(
                                    "composite capability '{}' has no input schema",
                                    row.id
                                ))
                            })?,
                            output_schema: row.schema_out.clone().ok_or_else(|| {
                                AgentError::Bootstrap(format!(
                                    "composite capability '{}' has no output schema",
                                    row.id
                                ))
                            })?,
                        })
                    }
                    CapabilityContract::Usage(row) => Err(AgentError::Bootstrap(format!(
                        "agent '{agent_id}' authorizes non-executable usage_method '{}'",
                        row.id
                    ))),
                },
            )
            .collect()
    }

    pub fn provider_tools_for_agent(&self, agent_id: &str) -> Result<ProviderToolSet> {
        let definitions = self.definitions_for_agent(agent_id)?;
        let mut tools = Vec::with_capacity(definitions.len());
        let mut aliases = HashMap::with_capacity(definitions.len());
        for definition in definitions {
            let alias = provider_alias(&definition);
            if aliases
                .insert(
                    alias.clone(),
                    (
                        definition.capability_id.clone(),
                        definition.capability_name.clone(),
                    ),
                )
                .is_some()
            {
                return Err(AgentError::Bootstrap(
                    "provider capability alias collision".to_string(),
                ));
            }
            tools.push(serde_json::json!({
                "type": "function",
                "function": {
                    "name": alias,
                    "description": format!(
                        "[capability_id={}] {}",
                        definition.capability_id, definition.description
                    ),
                    "parameters": definition.input_schema,
                }
            }));
        }
        Ok(ProviderToolSet { tools, aliases })
    }

    pub fn execute_for_agent(
        &self,
        agent_id: &str,
        call: &CapabilityCall,
    ) -> Result<CapabilityResult> {
        let agent =
            self.registry.agents.get(agent_id).ok_or_else(|| {
                AgentError::NotFound(format!("capability actor agent: {agent_id}"))
            })?;
        let contract = self.resolve_contract(&call.capability_id)?;

        let (authority_id, authority_name) = match contract {
            CapabilityContract::Base(row) => (&row.id, &row.name),
            CapabilityContract::Composite(row) => (&row.id, &row.name),
            CapabilityContract::Usage(row) => {
                return Err(AgentError::Parse(format!(
                    "usage_method '{}' is reference material and cannot execute",
                    row.id
                )))
            }
        };
        if authority_id != &call.capability_id || authority_name != &call.capability_name {
            return Err(AgentError::Parse(format!(
                "capability identity mismatch for '{}'",
                call.capability_id
            )));
        }
        if !agent
            .tool_caps
            .iter()
            .any(|capability_id| capability_id == authority_id)
        {
            return Err(AgentError::NotFound(format!(
                "capability '{}' is unavailable to actor '{agent_id}'",
                call.capability_id
            )));
        }

        let output = match contract {
            CapabilityContract::Base(row) => self.execute_base(row, &call.arguments)?,
            CapabilityContract::Composite(row) => self.execute_composite(row, &call.arguments)?,
            CapabilityContract::Usage(_) => unreachable!("usage methods return before execution"),
        };
        Ok(CapabilityResult {
            capability_id: authority_id.clone(),
            capability_name: authority_name.clone(),
            output,
        })
    }

    fn resolve_contract(&self, capability_id: &str) -> Result<CapabilityContract<'_>> {
        if let Some(row) = self.registry.base_capabilities.get(capability_id) {
            return Ok(CapabilityContract::Base(row));
        }
        if let Some(row) = self.registry.composite_capabilities.get(capability_id) {
            return Ok(CapabilityContract::Composite(row));
        }
        if let Some(row) = self.registry.usage_methods.get(capability_id) {
            return Ok(CapabilityContract::Usage(row));
        }
        Err(AgentError::NotFound(format!(
            "capability contract: {capability_id}"
        )))
    }

    fn execute_base(&self, row: &BaseCapabilityRow, arguments: &Value) -> Result<Value> {
        validate_base_authority(row)?;
        validate_schema(&row.schema_in, arguments, &row.id, "input")?;
        let output = self.executor.execute(&row.id, self.registry, arguments)?;
        validate_schema(&row.schema_out, &output, &row.id, "output")?;
        Ok(output)
    }

    fn execute_composite(&self, row: &CompositeCapabilityRow, input: &Value) -> Result<Value> {
        validate_composite_authority(row)?;
        let schema_in = row.schema_in.as_ref().ok_or_else(|| {
            AgentError::Bootstrap(format!(
                "composite capability '{}' has no input schema",
                row.id
            ))
        })?;
        let schema_out = row.schema_out.as_ref().ok_or_else(|| {
            AgentError::Bootstrap(format!(
                "composite capability '{}' has no output schema",
                row.id
            ))
        })?;
        validate_schema(schema_in, input, &row.id, "input")?;

        let nodes: Vec<CompositeNode> = serde_json::from_value(row.dag.clone())
            .map_err(|error| AgentError::Parse(format!("composite '{}' DAG: {error}", row.id)))?;
        let sorted = topological_order(&row.id, &nodes)?;
        let mut results: HashMap<&str, Value> = HashMap::with_capacity(nodes.len());
        let mut steps = Vec::with_capacity(nodes.len());

        for index in sorted.iter().copied() {
            let node = &nodes[index];
            let base = self
                .registry
                .base_capabilities
                .get(&node.base_capability)
                .ok_or_else(|| {
                    AgentError::Parse(format!(
                        "composite '{}' references undeclared base capability '{}'",
                        row.id, node.base_capability
                    ))
                })?;
            let arguments = resolve_node_arguments(node, input, &results);
            let output = self.execute_base(base, &arguments)?;
            results.insert(&node.id, output.clone());
            steps.push(serde_json::json!({
                "node": node.id,
                "capability_id": base.id,
                "output": output,
            }));
        }

        let final_output = sorted
            .last()
            .and_then(|index| results.get(nodes[*index].id.as_str()))
            .cloned()
            .unwrap_or(Value::Null);
        let output = serde_json::json!({
            "capability_id": row.id,
            "steps": steps,
            "final": final_output,
        });
        validate_schema(schema_out, &output, &row.id, "output")?;
        Ok(output)
    }
}

fn validate_base_authority(row: &BaseCapabilityRow) -> Result<()> {
    if !row.enabled || row.tombstoned_at.is_some() {
        return Err(AgentError::NotFound(format!(
            "executable base capability: {}",
            row.id
        )));
    }
    if row.executor.trim().is_empty() || row.version.trim().is_empty() {
        return Err(AgentError::Bootstrap(format!(
            "base capability '{}' lacks executor authority metadata",
            row.id
        )));
    }
    Ok(())
}

fn provider_alias(definition: &CapabilityDefinition) -> String {
    let mut prefix: String = definition
        .capability_name
        .chars()
        .filter_map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '_' | '-') {
                Some(character)
            } else if character.is_whitespace() || character == '.' {
                Some('_')
            } else {
                None
            }
        })
        .take(40)
        .collect();
    if prefix.is_empty() {
        prefix.push_str("capability");
    }
    let digest = Sha256::digest(definition.capability_id.as_bytes());
    let suffix: String = digest[..6]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    format!("{prefix}_{suffix}")
}

fn validate_composite_authority(row: &CompositeCapabilityRow) -> Result<()> {
    if !row.enabled || row.tombstoned_at.is_some() {
        return Err(AgentError::NotFound(format!(
            "executable composite capability: {}",
            row.id
        )));
    }
    if row.version.trim().is_empty() || row.executor.as_deref() != Some("dag") {
        return Err(AgentError::Bootstrap(format!(
            "composite capability '{}' lacks trusted DAG authority metadata",
            row.id
        )));
    }
    Ok(())
}

fn validate_schema(
    schema: &Value,
    instance: &Value,
    capability_id: &str,
    boundary: &str,
) -> Result<()> {
    let validator = jsonschema::validator_for(schema).map_err(|_| {
        AgentError::Bootstrap(format!(
            "capability '{capability_id}' has an invalid {boundary} schema"
        ))
    })?;
    if !validator.is_valid(instance) {
        let detail: Vec<String> = validator
            .iter_errors(instance)
            .take(3)
            .map(|e| e.to_string())
            .collect();
        let detail = if detail.is_empty() {
            String::new()
        } else {
            format!(": {}", detail.join("; "))
        };
        return Err(AgentError::Parse(format!(
            "capability '{capability_id}' {boundary} does not match its schema{detail}"
        )));
    }
    Ok(())
}

fn topological_order(composite_id: &str, nodes: &[CompositeNode]) -> Result<Vec<usize>> {
    let mut node_indices = HashMap::with_capacity(nodes.len());
    for (index, node) in nodes.iter().enumerate() {
        if node.id.trim().is_empty() || node_indices.insert(node.id.as_str(), index).is_some() {
            return Err(AgentError::Parse(format!(
                "composite '{composite_id}' contains an empty or duplicate node ID"
            )));
        }
    }

    let mut in_degree = vec![0_usize; nodes.len()];
    let mut dependents = vec![Vec::new(); nodes.len()];
    for (index, node) in nodes.iter().enumerate() {
        for dependency in &node.depends_on {
            let dependency_index = node_indices.get(dependency.as_str()).ok_or_else(|| {
                AgentError::Parse(format!(
                    "composite '{composite_id}' node '{}' depends on unknown node '{dependency}'",
                    node.id
                ))
            })?;
            in_degree[index] += 1;
            dependents[*dependency_index].push(index);
        }
    }

    let mut queue: VecDeque<usize> = in_degree
        .iter()
        .enumerate()
        .filter_map(|(index, degree)| (*degree == 0).then_some(index))
        .collect();
    let mut sorted = Vec::with_capacity(nodes.len());
    while let Some(index) = queue.pop_front() {
        sorted.push(index);
        for dependent in &dependents[index] {
            in_degree[*dependent] -= 1;
            if in_degree[*dependent] == 0 {
                queue.push_back(*dependent);
            }
        }
    }
    if sorted.len() != nodes.len() {
        return Err(AgentError::Parse(format!(
            "composite '{composite_id}' DAG contains a cycle"
        )));
    }
    Ok(sorted)
}

fn resolve_node_arguments(
    node: &CompositeNode,
    input: &Value,
    results: &HashMap<&str, Value>,
) -> Value {
    let Some(arguments) = &node.args else {
        return node
            .depends_on
            .first()
            .and_then(|dependency| results.get(dependency.as_str()))
            .map_or_else(
                || input.clone(),
                |output| serde_json::json!({"input": output}),
            );
    };
    let mut resolved = arguments.clone();
    if let Some(object) = resolved.as_object_mut() {
        if object.remove("$input").is_some() {
            object.insert("input".to_string(), input.clone());
        }
        for dependency in &node.depends_on {
            if let Some(output) = results.get(dependency.as_str()) {
                object.insert(format!("${dependency}"), output.clone());
            }
        }
    }
    resolved
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::duckdb::loader::{AgentRow, BaseCapabilityRow, CompositeCapabilityRow};
    use crate::logic::capability::base::{BaseCapability, Schema};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    const BASE_ID: &str = "base.echo.v1";
    const ECHO_EXECUTOR: &str = "builtin.echo.v1";

    struct EchoExecutor;

    impl BaseCapability for EchoExecutor {
        fn id(&self) -> &'static str {
            ECHO_EXECUTOR
        }

        fn name(&self) -> &'static str {
            "Builtin Echo"
        }

        fn execute(&self, input: &Schema) -> Result<Schema> {
            Ok(input.clone())
        }
    }

    struct InvalidOutputExecutor;

    impl BaseCapability for InvalidOutputExecutor {
        fn id(&self) -> &'static str {
            "builtin.invalid-output.v1"
        }

        fn name(&self) -> &'static str {
            "Invalid Output"
        }

        fn execute(&self, _input: &Schema) -> Result<Schema> {
            Ok(Value::String("not an object".to_string()))
        }
    }

    struct CountingExecutor {
        calls: Arc<AtomicUsize>,
    }

    impl BaseCapability for CountingExecutor {
        fn id(&self) -> &'static str {
            "builtin.counting.v1"
        }

        fn name(&self) -> &'static str {
            "Counting"
        }

        fn execute(&self, input: &Schema) -> Result<Schema> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(input.clone())
        }
    }

    fn object_schema() -> Value {
        serde_json::json!({"type": "object"})
    }

    fn required_value_schema() -> Value {
        serde_json::json!({
            "type": "object",
            "required": ["value"],
            "properties": {"value": {"type": "string"}},
            "additionalProperties": false
        })
    }

    fn base_row(id: &str, name: &str, executor: &str) -> BaseCapabilityRow {
        BaseCapabilityRow {
            id: id.to_string(),
            name: name.to_string(),
            cap_type: "function".to_string(),
            description: "test capability".to_string(),
            schema_in: object_schema(),
            schema_out: object_schema(),
            executor: executor.to_string(),
            version: "1.0.0".to_string(),
            enabled: true,
            tombstoned_at: None,
            metadata: None,
        }
    }

    fn agent(id: &str, tool_caps: &[&str]) -> AgentRow {
        AgentRow {
            id: id.to_string(),
            name: id.to_string(),
            mode: "unni".to_string(),
            prompt: None,
            tool_caps: tool_caps.iter().map(|value| (*value).to_string()).collect(),
            config: None,
            display_name: None,
            is_default: false,
        }
    }

    fn executor_with_echo() -> CapabilityExecutor {
        let mut executor = CapabilityExecutor::new();
        executor.register(Arc::new(EchoExecutor));
        executor
    }

    fn registry_with_echo(actor_caps: &[&str]) -> Registry {
        let mut registry = Registry::new();
        registry.base_capabilities.insert(
            BASE_ID.to_string(),
            base_row(BASE_ID, "echo", ECHO_EXECUTOR),
        );
        registry
            .agents
            .insert("actor".to_string(), agent("actor", actor_caps));
        registry
    }

    #[test]
    fn rejects_name_mismatch_against_authority() {
        let registry = registry_with_echo(&[BASE_ID]);
        let executor = executor_with_echo();
        let service = CapabilityService::new(&registry, &executor).unwrap();
        let error = service
            .execute_for_agent(
                "actor",
                &CapabilityCall {
                    capability_id: BASE_ID.to_string(),
                    capability_name: "guessed-name".to_string(),
                    arguments: serde_json::json!({}),
                },
            )
            .unwrap_err();
        assert!(error.to_string().contains("identity mismatch"));
    }

    #[test]
    fn rejects_unauthorized_actor_even_when_id_is_known() {
        let registry = registry_with_echo(&[]);
        let executor = executor_with_echo();
        let service = CapabilityService::new(&registry, &executor).unwrap();
        let error = service
            .execute_for_agent(
                "actor",
                &CapabilityCall {
                    capability_id: BASE_ID.to_string(),
                    capability_name: "echo".to_string(),
                    arguments: serde_json::json!({}),
                },
            )
            .unwrap_err();
        assert!(matches!(error, AgentError::NotFound(_)));
    }

    #[test]
    fn invalid_input_is_rejected_before_executor_runs() {
        let calls = Arc::new(AtomicUsize::new(0));
        let mut executor = CapabilityExecutor::new();
        executor.register(Arc::new(CountingExecutor {
            calls: Arc::clone(&calls),
        }));
        let mut registry = Registry::new();
        let mut row = base_row(BASE_ID, "echo", "builtin.counting.v1");
        row.schema_in = required_value_schema();
        registry.base_capabilities.insert(BASE_ID.to_string(), row);
        registry
            .agents
            .insert("actor".to_string(), agent("actor", &[BASE_ID]));
        let service = CapabilityService::new(&registry, &executor).unwrap();

        let error = service
            .execute_for_agent(
                "actor",
                &CapabilityCall {
                    capability_id: BASE_ID.to_string(),
                    capability_name: "echo".to_string(),
                    arguments: serde_json::json!({}),
                },
            )
            .unwrap_err();
        assert!(error.to_string().contains("input does not match"));
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn invalid_output_is_rejected_at_service_boundary() {
        let mut executor = CapabilityExecutor::new();
        executor.register(Arc::new(InvalidOutputExecutor));
        let mut registry = registry_with_echo(&[BASE_ID]);
        registry
            .base_capabilities
            .get_mut(BASE_ID)
            .unwrap()
            .executor = "builtin.invalid-output.v1".to_string();
        let service = CapabilityService::new(&registry, &executor).unwrap();

        let error = service
            .execute_for_agent(
                "actor",
                &CapabilityCall {
                    capability_id: BASE_ID.to_string(),
                    capability_name: "echo".to_string(),
                    arguments: serde_json::json!({}),
                },
            )
            .unwrap_err();
        assert!(error.to_string().contains("output does not match"));
    }

    #[test]
    fn usage_method_cannot_execute() {
        let mut registry = registry_with_echo(&[]);
        registry.usage_methods.insert(
            "usage.echo.v1".to_string(),
            UsageMethodRow {
                id: "usage.echo.v1".to_string(),
                capability_id: BASE_ID.to_string(),
                name: "echo_example".to_string(),
                prompt: "Use echo for tests".to_string(),
                examples: None,
                metadata: None,
            },
        );
        let executor = executor_with_echo();
        let service = CapabilityService::new(&registry, &executor).unwrap();

        let error = service
            .execute_for_agent(
                "actor",
                &CapabilityCall {
                    capability_id: "usage.echo.v1".to_string(),
                    capability_name: "echo_example".to_string(),
                    arguments: serde_json::json!({}),
                },
            )
            .unwrap_err();
        assert!(error.to_string().contains("cannot execute"));
    }

    #[test]
    fn registry_executor_key_is_used_and_not_exposed() {
        let registry = registry_with_echo(&[BASE_ID]);
        let executor = executor_with_echo();
        let service = CapabilityService::new(&registry, &executor).unwrap();
        let result = service
            .execute_for_agent(
                "actor",
                &CapabilityCall {
                    capability_id: BASE_ID.to_string(),
                    capability_name: "echo".to_string(),
                    arguments: serde_json::json!({"value": 1}),
                },
            )
            .unwrap();

        assert_eq!(result.capability_id, BASE_ID);
        assert_eq!(result.capability_name, "echo");
        assert_eq!(result.output, serde_json::json!({"value": 1}));
        assert!(!serde_json::to_value(result)
            .unwrap()
            .to_string()
            .contains(ECHO_EXECUTOR));
    }

    #[test]
    fn composite_executes_only_base_capabilities_declared_in_its_dag() {
        let allowed_calls = Arc::new(AtomicUsize::new(0));
        let other_calls = Arc::new(AtomicUsize::new(0));
        struct OtherExecutor {
            calls: Arc<AtomicUsize>,
        }
        impl BaseCapability for OtherExecutor {
            fn id(&self) -> &'static str {
                "builtin.other.v1"
            }

            fn name(&self) -> &'static str {
                "Other"
            }

            fn execute(&self, input: &Schema) -> Result<Schema> {
                self.calls.fetch_add(1, Ordering::SeqCst);
                Ok(input.clone())
            }
        }

        let mut executor = CapabilityExecutor::new();
        executor.register(Arc::new(CountingExecutor {
            calls: Arc::clone(&allowed_calls),
        }));
        executor.register(Arc::new(OtherExecutor {
            calls: Arc::clone(&other_calls),
        }));

        let composite_id = "memory.settle.v1";
        let other_id = "base.other.v1";
        let mut registry = Registry::new();
        registry.base_capabilities.insert(
            BASE_ID.to_string(),
            base_row(BASE_ID, "echo", "builtin.counting.v1"),
        );
        registry.base_capabilities.insert(
            other_id.to_string(),
            base_row(other_id, "other", "builtin.other.v1"),
        );
        registry.composite_capabilities.insert(
            composite_id.to_string(),
            CompositeCapabilityRow {
                id: composite_id.to_string(),
                name: "memory_settle".to_string(),
                description: "settle one memory batch".to_string(),
                schema_in: Some(object_schema()),
                schema_out: Some(serde_json::json!({
                    "type": "object",
                    "required": ["final", "steps"]
                })),
                executor: Some("dag".to_string()),
                dag: serde_json::json!([{
                    "id": "allowed",
                    "base_capability": BASE_ID
                }]),
                version: "1.0.0".to_string(),
                enabled: true,
                tombstoned_at: None,
                metadata: None,
            },
        );
        registry.agents.insert(
            "memory-agent".to_string(),
            agent("memory-agent", &[composite_id]),
        );
        let service = CapabilityService::new(&registry, &executor).unwrap();

        service
            .execute_for_agent(
                "memory-agent",
                &CapabilityCall {
                    capability_id: composite_id.to_string(),
                    capability_name: "memory_settle".to_string(),
                    arguments: serde_json::json!({"base_capability": other_id}),
                },
            )
            .unwrap();
        assert_eq!(allowed_calls.load(Ordering::SeqCst), 1);
        assert_eq!(other_calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn authorized_definitions_expose_contract_but_not_executor_metadata() {
        let registry = registry_with_echo(&[BASE_ID]);
        let executor = executor_with_echo();
        let service = CapabilityService::new(&registry, &executor).unwrap();

        let definitions = service.definitions_for_agent("actor").unwrap();
        assert_eq!(definitions.len(), 1);
        assert_eq!(definitions[0].capability_id, BASE_ID);
        assert_eq!(definitions[0].capability_name, "echo");
        let serialized = serde_json::to_string(&definitions).unwrap();
        assert!(!serialized.contains("executor"));
        assert!(!serialized.contains(ECHO_EXECUTOR));
    }

    #[test]
    fn provider_alias_normalizes_to_authoritative_dual_identity() {
        let registry = registry_with_echo(&[BASE_ID]);
        let executor = executor_with_echo();
        let service = CapabilityService::new(&registry, &executor).unwrap();
        let tool_set = service.provider_tools_for_agent("actor").unwrap();

        assert_eq!(tool_set.tools().len(), 1);
        let alias = tool_set.tools()[0]["function"]["name"].as_str().unwrap();
        assert!(alias.starts_with("echo_"));
        assert!(alias
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '_' | '-')));

        let call = tool_set
            .normalize(alias, serde_json::json!({"value": "ok"}))
            .unwrap();
        assert_eq!(call.capability_id, BASE_ID);
        assert_eq!(call.capability_name, "echo");
        assert_eq!(call.arguments, serde_json::json!({"value": "ok"}));
        assert!(tool_set.normalize(BASE_ID, serde_json::json!({})).is_err());
    }
}
