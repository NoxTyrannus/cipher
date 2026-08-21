use super::provider::LlmProvider;
use std::collections::HashMap;
use std::sync::Arc;

#[derive(Default, Clone)]
pub struct ProviderRegistry {
    providers: HashMap<String, Arc<dyn LlmProvider>>,
}

impl ProviderRegistry {
    pub fn new() -> Self {
        Self {
            providers: HashMap::new(),
        }
    }

    pub fn register(&mut self, p: Arc<dyn LlmProvider>) {
        self.providers.insert(p.id().to_string(), p);
    }

    pub fn pick_by_kind(&self, kind: &str) -> Option<&Arc<dyn LlmProvider>> {
        self.providers.get(kind)
    }
}

#[cfg(test)]
mod tests {}
