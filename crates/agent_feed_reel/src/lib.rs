use agent_feed_core::Bulletin;
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use time::OffsetDateTime;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ReelSnapshot {
    #[serde(with = "time::serde::rfc3339")]
    pub generated_at: OffsetDateTime,
    pub active: Option<Bulletin>,
    pub bulletins: Vec<Bulletin>,
}

#[derive(Clone, Debug)]
pub struct ReelBuffer {
    capacity: usize,
    bulletins: VecDeque<Bulletin>,
}

impl Default for ReelBuffer {
    fn default() -> Self {
        Self::new(128)
    }
}

impl ReelBuffer {
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        Self {
            capacity: capacity.max(1),
            bulletins: VecDeque::new(),
        }
    }

    pub fn push(&mut self, bulletin: Bulletin) {
        if self.bulletins.len() >= self.capacity {
            self.bulletins.pop_front();
        }
        self.bulletins.push_back(bulletin);
    }

    #[must_use]
    pub fn snapshot(&self) -> ReelSnapshot {
        let bulletins = self.bulletins.iter().cloned().collect::<Vec<_>>();
        let active = self.bulletins.back().cloned();
        ReelSnapshot {
            generated_at: OffsetDateTime::now_utc(),
            active,
            bulletins,
        }
    }
}
