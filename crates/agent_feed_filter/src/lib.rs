use agent_feed_core::{AgentEvent, EventKind};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct EventFilter {
    pub agents: BTreeSet<String>,
    pub projects: BTreeSet<String>,
    pub event_kinds: BTreeSet<EventKind>,
}

impl EventFilter {
    #[must_use]
    pub fn allows(&self, event: &AgentEvent) -> bool {
        (self.agents.is_empty() || self.agents.contains(&event.agent))
            && (self.projects.is_empty()
                || event
                    .project
                    .as_ref()
                    .is_some_and(|project| self.projects.contains(project)))
            && (self.event_kinds.is_empty() || self.event_kinds.contains(&event.kind))
    }
}
