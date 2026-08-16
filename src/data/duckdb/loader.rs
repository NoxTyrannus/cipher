use crate::common::AgentError;
use secrecy::{ExposeSecret, SecretString};
use serde::Deserialize;
use std::collections::{HashMap, HashSet};

#[derive(Debug, Clone, Deserialize)]
pub struct ModelRow {
    pub id: String,
    pub name: String,
    pub provider: String,
    pub api_url: String,
    pub api_type: String,

    #[serde(default = "default_api_protocol_for_serde")]
    pub api_protocol: String,
    #[serde(default)]
    pub api_key: Option<String>,
    pub model_id: String,
    #[serde(default)]
    pub config: Option<serde_json::Value>,
}

fn default_api_protocol_for_serde() -> String {
    "openai-v1".to_string()
}

pub fn default_api_protocol(api_type: &str) -> String {
    if api_type.eq_ignore_ascii_case("anthropic") {
        "anthropic-messages".to_string()
    } else {
        "openai-v1".to_string()
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct AgentRow {
    pub id: String,
    pub name: String,
    pub mode: String,
    #[serde(default)]
    pub prompt: Option<String>,
    pub tool_caps: Vec<String>,
    #[serde(default)]
    pub config: Option<serde_json::Value>,
    #[serde(default)]
    pub display_name: Option<String>,
    #[serde(default)]
    pub is_default: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct BaseCapabilityRow {
    pub id: String,
    pub name: String,
    #[serde(rename = "type")]
    pub cap_type: String,
    pub description: String,
    pub schema_in: serde_json::Value,
    pub schema_out: serde_json::Value,
    pub executor: String,
    pub version: String,
    pub enabled: bool,
    #[serde(default)]
    pub tombstoned_at: Option<String>,
    #[serde(default)]
    pub metadata: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CompositeCapabilityRow {
    pub id: String,
    pub name: String,
    pub description: String,
    #[serde(default)]
    pub schema_in: Option<serde_json::Value>,
    #[serde(default)]
    pub schema_out: Option<serde_json::Value>,
    #[serde(default)]
    pub executor: Option<String>,
    pub dag: serde_json::Value,
    pub version: String,
    pub enabled: bool,
    #[serde(default)]
    pub tombstoned_at: Option<String>,
    #[serde(default)]
    pub metadata: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct UsageMethodRow {
    pub id: String,
    pub capability_id: String,
    pub name: String,
    pub prompt: String,
    #[serde(default)]
    pub examples: Option<serde_json::Value>,
    #[serde(default)]
    pub metadata: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Default)]
pub struct Registry {
    pub models: HashMap<String, ModelRow>,
    pub agents: HashMap<String, AgentRow>,
    pub base_capabilities: HashMap<String, BaseCapabilityRow>,
    pub composite_capabilities: HashMap<String, CompositeCapabilityRow>,
    pub usage_methods: HashMap<String, UsageMethodRow>,
}

impl Registry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn validate(&self) -> Result<(), AgentError> {
        let mut owners = HashMap::new();

        for row in self.base_capabilities.values() {
            if !row.enabled || row.tombstoned_at.is_some() {
                return Err(AgentError::Bootstrap(format!(
                    "non-executable base_capability '{}' entered the runtime registry",
                    row.id
                )));
            }
            register_contract_id(&mut owners, &row.id, "base_capability")?;
        }
        for row in self.composite_capabilities.values() {
            if !row.enabled || row.tombstoned_at.is_some() {
                return Err(AgentError::Bootstrap(format!(
                    "non-executable composite_capability '{}' entered the runtime registry",
                    row.id
                )));
            }
            register_contract_id(&mut owners, &row.id, "composite_capability")?;
        }
        for row in self.usage_methods.values() {
            register_contract_id(&mut owners, &row.id, "usage_method")?;
        }

        let capability_ids: HashSet<&str> = self
            .base_capabilities
            .values()
            .map(|row| row.id.as_str())
            .chain(
                self.composite_capabilities
                    .values()
                    .map(|row| row.id.as_str()),
            )
            .collect();

        for usage in self.usage_methods.values() {
            if !capability_ids.contains(usage.capability_id.as_str()) {
                return Err(AgentError::Bootstrap(format!(
                    "usage_method '{}' references unknown capability_id '{}'",
                    usage.id, usage.capability_id
                )));
            }
        }
        for agent in self.agents.values() {
            for capability_id in &agent.tool_caps {
                if !capability_ids.contains(capability_id.as_str()) {
                    return Err(AgentError::Bootstrap(format!(
                        "agent '{}' authorizes unknown or non-executable capability_id '{}'",
                        agent.id, capability_id
                    )));
                }
            }
        }

        Ok(())
    }
}

fn register_contract_id<'a>(
    owners: &mut HashMap<&'a str, &'static str>,
    id: &'a str,
    table: &'static str,
) -> Result<(), AgentError> {
    if let Some(existing) = owners.insert(id, table) {
        return Err(AgentError::Bootstrap(format!(
            "capability contract id '{id}' is used by both {existing} and {table}"
        )));
    }
    Ok(())
}

fn validate_persisted_contracts(conn: &duckdb::Connection) -> Result<(), AgentError> {
    let mut owners = HashMap::<String, &'static str>::new();
    let mut executable_ids = HashSet::<String>::new();

    for (table, query) in [
        (
            "base_capability",
            "SELECT id, enabled, tombstoned_at IS NULL FROM base_capability",
        ),
        (
            "composite_capability",
            "SELECT id, enabled, tombstoned_at IS NULL FROM composite_capability",
        ),
    ] {
        let mut statement = conn.prepare(query).map_err(|error| {
            AgentError::Bootstrap(format!("prepare persisted {table} identities: {error}"))
        })?;
        let rows = statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, bool>(1)?,
                    row.get::<_, bool>(2)?,
                ))
            })
            .map_err(|error| {
                AgentError::Bootstrap(format!("query persisted {table} identities: {error}"))
            })?;
        for row in rows {
            let (id, enabled, not_tombstoned) = row.map_err(|error| {
                AgentError::Bootstrap(format!("read persisted {table} identity: {error}"))
            })?;
            register_persisted_id(&mut owners, &id, table)?;
            if enabled && not_tombstoned {
                executable_ids.insert(id);
            }
        }
    }

    let mut statement = conn
        .prepare("SELECT id, capability_id FROM usage_method")
        .map_err(|error| {
            AgentError::Bootstrap(format!("prepare persisted usage identities: {error}"))
        })?;
    let rows = statement
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .map_err(|error| {
            AgentError::Bootstrap(format!("query persisted usage identities: {error}"))
        })?;
    for row in rows {
        let (id, capability_id) = row.map_err(|error| {
            AgentError::Bootstrap(format!("read persisted usage identity: {error}"))
        })?;
        register_persisted_id(&mut owners, &id, "usage_method")?;
        if !executable_ids.contains(&capability_id) {
            return Err(AgentError::Bootstrap(format!(
                "usage_method '{id}' references unknown or non-executable capability_id '{capability_id}'"
            )));
        }
    }
    Ok(())
}

fn register_persisted_id(
    owners: &mut HashMap<String, &'static str>,
    id: &str,
    table: &'static str,
) -> Result<(), AgentError> {
    if id.trim().is_empty() {
        return Err(AgentError::Bootstrap(format!(
            "{table} contains an empty capability contract id"
        )));
    }
    if let Some(existing) = owners.insert(id.to_string(), table) {
        return Err(AgentError::Bootstrap(format!(
            "capability contract id '{id}' is used by both {existing} and {table}"
        )));
    }
    Ok(())
}

fn parse_json(value: String, context: &str) -> Result<serde_json::Value, AgentError> {
    serde_json::from_str(&value)
        .map_err(|error| AgentError::Bootstrap(format!("invalid JSON in {context}: {error}")))
}

fn parse_optional_json(
    value: Option<String>,
    context: &str,
) -> Result<Option<serde_json::Value>, AgentError> {
    value.map(|value| parse_json(value, context)).transpose()
}

fn parse_capability_ids(value: String, context: &str) -> Result<Vec<String>, AgentError> {
    let value = parse_json(value, context)?;
    let values = value.as_array().ok_or_else(|| {
        AgentError::Bootstrap(format!("{context} must be an array of capability IDs"))
    })?;
    let mut ids = Vec::with_capacity(values.len());
    let mut seen = HashSet::with_capacity(values.len());
    for value in values {
        let id = value
            .as_str()
            .filter(|id| !id.trim().is_empty())
            .ok_or_else(|| {
                AgentError::Bootstrap(format!(
                    "{context} must contain only non-empty capability IDs"
                ))
            })?;
        if !seen.insert(id) {
            return Err(AgentError::Bootstrap(format!(
                "{context} contains duplicate capability_id '{id}'"
            )));
        }
        ids.push(id.to_string());
    }
    Ok(ids)
}

pub fn load_capability_into_memory(
    conn: &duckdb::Connection,
    registry: &mut Registry,
) -> Result<(), AgentError> {
    load_models(conn, registry)?;
    load_agents(conn, registry)?;
    load_base_capabilities(conn, registry)?;
    load_composite_capabilities(conn, registry)?;
    load_usage_methods(conn, registry)?;
    Ok(())
}

fn load_models(conn: &duckdb::Connection, registry: &mut Registry) -> Result<(), AgentError> {
    let mut statement = conn
        .prepare(
            "SELECT id, name, provider, api_url, api_type, api_protocol, api_key, model_id, \
             CAST(config AS VARCHAR) FROM model",
        )
        .map_err(|error| AgentError::Bootstrap(format!("prepare model: {error}")))?;
    let rows = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, String>(5)?,
                row.get::<_, String>(6)?,
                row.get::<_, String>(7)?,
                row.get::<_, Option<String>>(8)?,
            ))
        })
        .map_err(|error| AgentError::Bootstrap(format!("query model: {error}")))?;

    for row in rows {
        let (id, name, provider, api_url, api_type, api_protocol, api_key, model_id, config) =
            row.map_err(|error| AgentError::Bootstrap(format!("row model: {error}")))?;
        let config = parse_optional_json(config, &format!("model '{id}'.config"))?;
        let api_key = (!api_key.is_empty()).then_some(api_key);
        registry.models.insert(
            id.clone(),
            ModelRow {
                id,
                name,
                provider,
                api_url,
                api_type,
                api_protocol,
                api_key,
                model_id,
                config,
            },
        );
    }
    Ok(())
}

fn load_agents(conn: &duckdb::Connection, registry: &mut Registry) -> Result<(), AgentError> {
    let mut statement = conn
        .prepare(
            "SELECT id, name, mode, prompt, CAST(tool_caps AS VARCHAR), \
             CAST(config AS VARCHAR), display_name, is_default FROM agent",
        )
        .map_err(|error| AgentError::Bootstrap(format!("prepare agent: {error}")))?;
    let rows = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, Option<String>>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, Option<String>>(5)?,
                row.get::<_, Option<String>>(6)?,
                row.get::<_, bool>(7)?,
            ))
        })
        .map_err(|error| AgentError::Bootstrap(format!("query agent: {error}")))?;

    for row in rows {
        let (id, name, mode, prompt, tool_caps, config, display_name, is_default) =
            row.map_err(|error| AgentError::Bootstrap(format!("row agent: {error}")))?;
        let tool_caps = parse_capability_ids(tool_caps, &format!("agent '{id}'.tool_caps"))?;
        let config = parse_optional_json(config, &format!("agent '{id}'.config"))?;
        registry.agents.insert(
            id.clone(),
            AgentRow {
                id,
                name,
                mode,
                prompt,
                tool_caps,
                config,
                display_name,
                is_default,
            },
        );
    }
    Ok(())
}

fn load_base_capabilities(
    conn: &duckdb::Connection,
    registry: &mut Registry,
) -> Result<(), AgentError> {
    let mut statement = conn
        .prepare(
            "SELECT id, name, type, description, CAST(schema_in AS VARCHAR), \
             CAST(schema_out AS VARCHAR), executor, version, enabled, \
             CAST(tombstoned_at AS VARCHAR), CAST(metadata AS VARCHAR) \
             FROM base_capability \
             WHERE enabled = TRUE AND tombstoned_at IS NULL",
        )
        .map_err(|error| AgentError::Bootstrap(format!("prepare base_capability: {error}")))?;
    let rows = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, String>(5)?,
                row.get::<_, String>(6)?,
                row.get::<_, String>(7)?,
                row.get::<_, bool>(8)?,
                row.get::<_, Option<String>>(9)?,
                row.get::<_, Option<String>>(10)?,
            ))
        })
        .map_err(|error| AgentError::Bootstrap(format!("query base_capability: {error}")))?;

    for row in rows {
        let (
            id,
            name,
            cap_type,
            description,
            schema_in,
            schema_out,
            executor,
            version,
            enabled,
            tombstoned_at,
            metadata,
        ) = row.map_err(|error| AgentError::Bootstrap(format!("row base_capability: {error}")))?;
        let schema_in = parse_json(schema_in, &format!("base_capability '{id}'.schema_in"))?;
        let schema_out = parse_json(schema_out, &format!("base_capability '{id}'.schema_out"))?;
        let metadata = parse_optional_json(metadata, &format!("base_capability '{id}'.metadata"))?;
        registry.base_capabilities.insert(
            id.clone(),
            BaseCapabilityRow {
                id,
                name,
                cap_type,
                description,
                schema_in,
                schema_out,
                executor,
                version,
                enabled,
                tombstoned_at,
                metadata,
            },
        );
    }
    Ok(())
}

fn load_composite_capabilities(
    conn: &duckdb::Connection,
    registry: &mut Registry,
) -> Result<(), AgentError> {
    let mut statement = conn
        .prepare(
            "SELECT id, name, description, CAST(schema_in AS VARCHAR), \
             CAST(schema_out AS VARCHAR), executor, CAST(dag AS VARCHAR), version, enabled, \
             CAST(tombstoned_at AS VARCHAR), CAST(metadata AS VARCHAR) \
             FROM composite_capability \
             WHERE enabled = TRUE AND tombstoned_at IS NULL",
        )
        .map_err(|error| AgentError::Bootstrap(format!("prepare composite_capability: {error}")))?;
    let rows = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, Option<String>>(3)?,
                row.get::<_, Option<String>>(4)?,
                row.get::<_, Option<String>>(5)?,
                row.get::<_, String>(6)?,
                row.get::<_, String>(7)?,
                row.get::<_, bool>(8)?,
                row.get::<_, Option<String>>(9)?,
                row.get::<_, Option<String>>(10)?,
            ))
        })
        .map_err(|error| AgentError::Bootstrap(format!("query composite_capability: {error}")))?;

    for row in rows {
        let (
            id,
            name,
            description,
            schema_in,
            schema_out,
            executor,
            dag,
            version,
            enabled,
            tombstoned_at,
            metadata,
        ) = row
            .map_err(|error| AgentError::Bootstrap(format!("row composite_capability: {error}")))?;
        let schema_in =
            parse_optional_json(schema_in, &format!("composite_capability '{id}'.schema_in"))?;
        let schema_out = parse_optional_json(
            schema_out,
            &format!("composite_capability '{id}'.schema_out"),
        )?;
        let dag = parse_json(dag, &format!("composite_capability '{id}'.dag"))?;
        let metadata =
            parse_optional_json(metadata, &format!("composite_capability '{id}'.metadata"))?;
        registry.composite_capabilities.insert(
            id.clone(),
            CompositeCapabilityRow {
                id,
                name,
                description,
                schema_in,
                schema_out,
                executor,
                dag,
                version,
                enabled,
                tombstoned_at,
                metadata,
            },
        );
    }
    Ok(())
}

fn load_usage_methods(
    conn: &duckdb::Connection,
    registry: &mut Registry,
) -> Result<(), AgentError> {
    let mut statement = conn
        .prepare(
            "SELECT id, capability_id, name, prompt, CAST(examples AS VARCHAR), \
             CAST(metadata AS VARCHAR) FROM usage_method",
        )
        .map_err(|error| AgentError::Bootstrap(format!("prepare usage_method: {error}")))?;
    let rows = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, Option<String>>(4)?,
                row.get::<_, Option<String>>(5)?,
            ))
        })
        .map_err(|error| AgentError::Bootstrap(format!("query usage_method: {error}")))?;

    for row in rows {
        let (id, capability_id, name, prompt, examples, metadata) =
            row.map_err(|error| AgentError::Bootstrap(format!("row usage_method: {error}")))?;
        let examples = parse_optional_json(examples, &format!("usage_method '{id}'.examples"))?;
        let metadata = parse_optional_json(metadata, &format!("usage_method '{id}'.metadata"))?;
        registry.usage_methods.insert(
            id.clone(),
            UsageMethodRow {
                id,
                capability_id,
                name,
                prompt,
                examples,
                metadata,
            },
        );
    }
    Ok(())
}

pub fn load_all_into_memory(conn: &duckdb::Connection) -> Result<Registry, AgentError> {
    validate_persisted_contracts(conn)?;
    let mut registry = Registry::new();
    load_capability_into_memory(conn, &mut registry)?;
    registry.validate()?;
    Ok(registry)
}

pub fn has_configured_model(conn: &duckdb::Connection) -> Result<bool, AgentError> {
    let mut statement = conn
        .prepare("SELECT COUNT(*) FROM model WHERE api_key IS NOT NULL AND api_key != ''")
        .map_err(|error| AgentError::Bootstrap(format!("has_configured_model prepare: {error}")))?;
    let count: i64 = statement
        .query_map([], |row| row.get(0))
        .map_err(|error| AgentError::Bootstrap(format!("has_configured_model query: {error}")))?
        .next()
        .ok_or_else(|| AgentError::Bootstrap("has_configured_model returned no row".to_string()))?
        .map_err(|error| AgentError::Bootstrap(format!("has_configured_model row: {error}")))?;
    Ok(count > 0)
}

pub fn update_model_api_key_by_provider(
    conn: &duckdb::Connection,
    provider: &str,
    api_key: &SecretString,
) -> Result<usize, AgentError> {
    conn.execute(
        "UPDATE model SET api_key = ?, updated_at = now() WHERE provider = ?",
        duckdb::params![api_key.expose_secret(), provider],
    )
    .map_err(|error| AgentError::Bootstrap(format!("update_model_api_key_by_provider: {error}")))
}

pub fn write_usage_observation(
    conn: &duckdb::Connection,
    capability_id: &str,
    description_patch: &str,
    rating: &str,
    note: &str,
) -> Result<(), AgentError> {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(capability_id.as_bytes());
    let suffix: String = digest[..6]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    let id = format!("um_{suffix}");
    let metadata = serde_json::json!({ "rating": rating, "note": note });
    conn.execute(
        "INSERT INTO usage_method (id, capability_id, name, prompt, examples, metadata, updated_at) \
         VALUES (?, ?, ?, ?, NULL, ?, now()) \
         ON CONFLICT (id) DO UPDATE SET \
             prompt = excluded.prompt, \
             metadata = excluded.metadata, \
             updated_at = now()",
        duckdb::params![
            id,
            capability_id,
            capability_id,
            description_patch,
            metadata.to_string(),
        ],
    )
    .map_err(|error| {
        AgentError::Bootstrap(format!(
            "write_usage_observation({capability_id}): {error}"
        ))
    })?;
    Ok(())
}

pub fn insert_model(conn: &duckdb::Connection, row: &ModelRow) -> Result<(), AgentError> {
    let config = row.config.as_ref().map(serde_json::Value::to_string);
    conn.execute(
        "INSERT OR REPLACE INTO model \
         (id, name, provider, api_url, api_type, api_protocol, api_key, model_id, config) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
        duckdb::params![
            row.id,
            row.name,
            row.provider,
            row.api_url,
            row.api_type,
            row.api_protocol,
            row.api_key.as_deref().unwrap_or(""),
            row.model_id,
            config
        ],
    )
    .map_err(|error| AgentError::Bootstrap(format!("insert_model {}: {error}", row.id)))?;
    Ok(())
}

pub fn rename_agent(
    conn: &duckdb::Connection,
    id: &str,
    display_name: &str,
) -> Result<(), AgentError> {
    conn.execute(
        "UPDATE agent SET display_name = ?, updated_at = now() WHERE id = ?",
        duckdb::params![display_name, id],
    )
    .map_err(|error| AgentError::Bootstrap(format!("rename_agent {id}: {error}")))?;
    Ok(())
}

pub fn set_default_agent(conn: &duckdb::Connection, id: &str) -> Result<(), AgentError> {
    conn.execute(
        "UPDATE agent SET is_default = (id = ?), updated_at = now()",
        duckdb::params![id],
    )
    .map_err(|error| AgentError::Bootstrap(format!("set_default_agent {id}: {error}")))?;
    Ok(())
}

pub fn find_provider_sample(
    conn: &duckdb::Connection,
    provider: &str,
) -> Result<Option<ModelRow>, AgentError> {
    let registry = load_all_into_memory(conn)?;
    Ok(registry
        .models
        .values()
        .find(|model| model.provider == provider)
        .cloned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::duckdb::schema::create_all_tables;
    use serde_json::json;

    fn base(id: &str) -> BaseCapabilityRow {
        BaseCapabilityRow {
            id: id.to_string(),
            name: id.to_string(),
            cap_type: "function".to_string(),
            description: "test capability".to_string(),
            schema_in: json!({"type": "object"}),
            schema_out: json!({"type": "object"}),
            executor: id.to_string(),
            version: "1.0.0".to_string(),
            enabled: true,
            tombstoned_at: None,
            metadata: None,
        }
    }

    fn composite(id: &str) -> CompositeCapabilityRow {
        CompositeCapabilityRow {
            id: id.to_string(),
            name: id.to_string(),
            description: "test composite".to_string(),
            schema_in: Some(json!({"type": "object"})),
            schema_out: Some(json!({"type": "object"})),
            executor: Some("dag".to_string()),
            dag: json!([]),
            version: "1.0.0".to_string(),
            enabled: true,
            tombstoned_at: None,
            metadata: None,
        }
    }

    fn usage(id: &str, capability_id: &str) -> UsageMethodRow {
        UsageMethodRow {
            id: id.to_string(),
            capability_id: capability_id.to_string(),
            name: id.to_string(),
            prompt: "use it".to_string(),
            examples: None,
            metadata: None,
        }
    }

    fn agent(id: &str, tool_caps: &[&str]) -> AgentRow {
        AgentRow {
            id: id.to_string(),
            name: id.to_string(),
            mode: "unni".to_string(),
            prompt: None,
            tool_caps: tool_caps.iter().map(|id| (*id).to_string()).collect(),
            config: None,
            display_name: None,
            is_default: false,
        }
    }

    #[test]
    fn validates_explicit_base_and_composite_references() {
        let mut registry = Registry::new();
        registry
            .base_capabilities
            .insert("base".to_string(), base("base"));
        registry
            .composite_capabilities
            .insert("composite".to_string(), composite("composite"));
        registry
            .usage_methods
            .insert("use-base".to_string(), usage("use-base", "base"));
        registry.usage_methods.insert(
            "use-composite".to_string(),
            usage("use-composite", "composite"),
        );

        registry.validate().expect("valid registry");
    }

    #[test]
    fn rejects_contract_id_collision_across_tables() {
        let mut registry = Registry::new();
        registry
            .base_capabilities
            .insert("same".to_string(), base("same"));
        registry
            .composite_capabilities
            .insert("same".to_string(), composite("same"));

        let error = registry.validate().expect_err("collision must fail");
        assert!(error.to_string().contains("same"));
    }

    #[test]
    fn rejects_usage_method_without_explicit_capability_id_match() {
        let mut registry = Registry::new();
        registry
            .base_capabilities
            .insert("stable-id".to_string(), base("stable-id"));
        registry.usage_methods.insert(
            "usage".to_string(),
            usage("usage", "capability-display-name"),
        );

        let error = registry
            .validate()
            .expect_err("unknown capability reference must fail");
        assert!(error.to_string().contains("capability-display-name"));
    }

    #[test]
    fn rejects_agent_authorization_for_non_executable_capability() {
        let mut registry = Registry::new();
        registry
            .base_capabilities
            .insert("active".to_string(), base("active"));
        registry.agents.insert(
            "agent".to_string(),
            agent("agent", &["disabled-or-unknown"]),
        );

        let error = registry
            .validate()
            .expect_err("unknown tool authorization must fail");
        assert!(error.to_string().contains("disabled-or-unknown"));
    }

    #[test]
    fn loads_only_enabled_non_tombstoned_capabilities() {
        let connection = duckdb::Connection::open_in_memory().expect("open DuckDB");
        create_all_tables(&connection).expect("create schema");
        connection
            .execute_batch(
                "INSERT INTO base_capability \
                 (id, name, type, description, schema_in, schema_out, executor, version, enabled) \
                 VALUES \
                 ('active', 'Active', 'function', 'active', '{}', '{}', 'active', '1', true), \
                 ('disabled', 'Disabled', 'function', 'disabled', '{}', '{}', 'disabled', '1', false); \
                 INSERT INTO composite_capability \
                 (id, name, description, schema_in, schema_out, executor, dag, version, enabled, tombstoned_at) \
                 VALUES \
                 ('tombstoned', 'Tombstoned', 'old', '{}', '{}', 'dag', '[]', '1', true, \
                  TIMESTAMP '2026-01-01 00:00:00');",
            )
            .expect("insert capability audit rows");

        let registry = load_all_into_memory(&connection).expect("load registry");
        assert_eq!(
            registry
                .base_capabilities
                .keys()
                .map(String::as_str)
                .collect::<Vec<_>>(),
            ["active"]
        );
        assert!(registry.composite_capabilities.is_empty());
    }

    #[test]
    fn rejects_usage_id_collision_with_filtered_capability() {
        let connection = duckdb::Connection::open_in_memory().expect("open DuckDB");
        create_all_tables(&connection).expect("create schema");
        connection
            .execute_batch(
                "INSERT INTO base_capability \
                 (id, name, type, description, schema_in, schema_out, executor, version, enabled) \
                 VALUES \
                 ('disabled', 'Disabled', 'function', 'disabled', '{}', '{}', 'disabled', '1', false), \
                 ('active', 'Active', 'function', 'active', '{}', '{}', 'active', '1', true); \
                 INSERT INTO usage_method (id, capability_id, name, prompt) \
                 VALUES ('disabled', 'active', 'collision', 'use it');",
            )
            .expect("insert collision fixture");

        let error = load_all_into_memory(&connection).expect_err("collision must fail");
        assert!(error.to_string().contains("disabled"));
    }

    #[test]
    fn loads_stable_contract_columns() {
        let connection = duckdb::Connection::open_in_memory().expect("open DuckDB");
        create_all_tables(&connection).expect("create schema");
        connection
            .execute(
                "INSERT INTO base_capability \
                 (id, name, type, description, schema_in, schema_out, executor, version, enabled) \
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
                duckdb::params![
                    "base",
                    "Base",
                    "function",
                    "description",
                    "{}",
                    "{}",
                    "native:base",
                    "1.2.3",
                    true
                ],
            )
            .expect("insert base capability");
        connection
            .execute(
                "INSERT INTO usage_method (id, capability_id, name, prompt) \
                 VALUES (?, ?, ?, ?)",
                duckdb::params!["usage", "base", "Usage", "Use base"],
            )
            .expect("insert usage method");

        let registry = load_all_into_memory(&connection).expect("load registry");
        let base = registry.base_capabilities.get("base").expect("base row");
        assert_eq!(base.description, "description");
        assert_eq!(base.executor, "native:base");
        assert_eq!(base.version, "1.2.3");
        assert!(base.enabled);
        assert_eq!(registry.usage_methods["usage"].capability_id, "base");
    }
}
