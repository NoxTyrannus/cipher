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
}

pub struct InstanceRegistry {
    entries: HashMap<String, AgentEntry>,
}

impl InstanceRegistry {
    pub fn new() -> Self {
        Self {
            entries: HashMap::new(),
        }
    }

    pub fn register(&mut self, entry: AgentEntry) {
        self.entries.insert(entry.id.clone(), entry);
    }

    pub fn update_status(&mut self, id: &str, status: AgentStatus) -> Option<()> {
        self.entries.get_mut(id).map(|e| {
            e.status = status;
        })
    }

    pub fn remove(&mut self, id: &str) -> Option<AgentEntry> {
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

    fn make_entry(id: &str) -> AgentEntry {
        AgentEntry {
            id: id.into(),
            identity: AgentIdentity::SubagentRunning {
                agent_id: id.into(),
            },
            status: AgentStatus::Running,
            created_at: std::time::Instant::now(),
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
        reg.register(make_entry("a1"));
        reg.update_status("a1", AgentStatus::Idle);
        assert_eq!(reg.get("a1").unwrap().status, AgentStatus::Idle);
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
}
