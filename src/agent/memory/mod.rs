pub mod capability_agent;
pub mod cognitive_agent;
pub mod experience_agent;
pub mod memory_version;
pub mod preference_agent;

use crate::data::duckdb::Registry;
use crate::logic::capability::executor::CapabilityExecutor;
use crate::logic::model::prompts::CapabilityPromptEntry;
use std::sync::Arc;

/// 按 actor_id 查询注册表中该 agent 的可用能力定义，组装为 LLM 可见的能力表条目。
///
/// 从 memory_platform.rs 抽取（T3 共享函数）：memory_platform（attention-agent）与
/// experience / preference / cognitive 三个记忆 agent 共用；行为与原实现一致。
pub fn memory_capability_entries(
    registry: &Registry,
    executor: &Arc<CapabilityExecutor>,
    actor_id: &str,
) -> Vec<CapabilityPromptEntry> {
    let Ok(service) = crate::logic::capability::service::CapabilityService::new(registry, executor)
    else {
        return Vec::new();
    };
    let Ok(defs) = service.definitions_for_agent(actor_id) else {
        return Vec::new();
    };
    defs.into_iter()
        .map(|d| CapabilityPromptEntry {
            capability_id: d.capability_id,
            capability_name: d.capability_name,
            description: d.description,
        })
        .collect()
}
