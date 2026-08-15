#[derive(Debug, Clone)]
pub struct ContextConfig {
    pub recent_turns: usize,
    pub raw_threshold_pct: f64,
    pub rag_reserve_pct: f64,
    pub cognitive_quota_pct: f64,
    pub attention_quota_pct: f64,
    pub experience_quota_pct: f64,
    pub preference_quota_pct: f64,
    pub context_window: usize,
}

impl Default for ContextConfig {
    fn default() -> Self {
        Self {
            recent_turns: 3,
            raw_threshold_pct: 30.0,
            rag_reserve_pct: 10.0,
            cognitive_quota_pct: 5.0,
            attention_quota_pct: 5.0,
            experience_quota_pct: 5.0,
            preference_quota_pct: 3.0,
            context_window: 1_000_000,
        }
    }
}

#[derive(Debug, Clone)]
pub(super) struct ContextBudget {
    pub(super) total: usize,
    pub(super) used: usize,
}

impl ContextBudget {
    pub(super) fn new(total: usize) -> Self {
        Self { total, used: 0 }
    }

    pub(super) fn try_allocate(&mut self, n: usize) -> bool {
        if self.used + n <= self.total {
            self.used += n;
            true
        } else {
            false
        }
    }

    #[allow(dead_code)]
    pub(super) fn remaining(&self) -> usize {
        self.total.saturating_sub(self.used)
    }

    #[allow(dead_code)]
    pub(super) fn force_allocate(&mut self, n: usize) {
        self.used += n;
    }
}

#[derive(Debug, Clone)]
pub(super) struct ParsedMessage {
    pub(super) role: String,
    pub(super) content: String,
    pub(super) token_count: usize,
}
