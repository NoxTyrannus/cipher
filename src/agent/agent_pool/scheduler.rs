use tokio::time::{interval, Duration};

use super::registry::SharedRegistry;

#[derive(Debug, Clone)]
pub struct SchedulerConfig {
    pub tick_ms: u64,
}

impl Default for SchedulerConfig {
    fn default() -> Self {
        Self { tick_ms: 100 }
    }
}

pub struct Scheduler {
    config: SchedulerConfig,
    registry: SharedRegistry,
}

impl Scheduler {
    pub fn new(registry: SharedRegistry) -> Self {
        Self {
            config: SchedulerConfig::default(),
            registry,
        }
    }

    pub fn with_config(registry: SharedRegistry, config: SchedulerConfig) -> Self {
        Self { config, registry }
    }

    pub fn spawn(self) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            let mut tick = interval(Duration::from_millis(self.config.tick_ms));
            loop {
                tick.tick().await;
                let reg = self.registry.read().await;
                let counts = reg.count_by_status();
                let total = reg.len();

                tracing::debug!(
                    total = total,
                    running = counts
                        .get(&super::registry::AgentStatus::Running)
                        .copied()
                        .unwrap_or(0),
                    pending = counts
                        .get(&super::registry::AgentStatus::Pending)
                        .copied()
                        .unwrap_or(0),
                    idle = counts
                        .get(&super::registry::AgentStatus::Idle)
                        .copied()
                        .unwrap_or(0),
                    "agent pool scheduler tick"
                );
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::super::registry::InstanceRegistry;
    use super::*;
    use std::sync::Arc;

    use tokio::sync::RwLock;

    #[test]
    fn scheduler_default_config_is_100ms() {
        let cfg = SchedulerConfig::default();
        assert_eq!(cfg.tick_ms, 100);
    }

    #[tokio::test]
    async fn scheduler_spawns_and_runs() {
        let registry = InstanceRegistry::new();
        let shared = Arc::new(RwLock::new(registry));
        let scheduler = Scheduler::new(shared);
        let handle = scheduler.spawn();

        tokio::time::sleep(Duration::from_millis(50)).await;

        assert!(!handle.is_finished());

        handle.abort();
    }
}
