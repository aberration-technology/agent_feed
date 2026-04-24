use agent_feed_core::AgentEvent;
use std::collections::VecDeque;

#[derive(Clone, Debug)]
pub struct InMemoryStore {
    capacity: usize,
    events: VecDeque<AgentEvent>,
}

impl Default for InMemoryStore {
    fn default() -> Self {
        Self::new(10_000)
    }
}

impl InMemoryStore {
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        Self {
            capacity: capacity.max(1),
            events: VecDeque::new(),
        }
    }

    pub fn push(&mut self, event: AgentEvent) {
        if self.events.len() >= self.capacity {
            self.events.pop_front();
        }
        self.events.push_back(event);
    }

    #[must_use]
    pub fn events(&self) -> Vec<AgentEvent> {
        self.events.iter().cloned().collect()
    }
}
