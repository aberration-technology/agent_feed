use serde::Serialize;
use std::sync::atomic::{AtomicU64, Ordering};

#[derive(Debug, Default)]
pub struct Metrics {
    ingested_events: AtomicU64,
    emitted_bulletins: AtomicU64,
    dropped_events: AtomicU64,
}

impl Metrics {
    pub fn record_ingested(&self) {
        self.ingested_events.fetch_add(1, Ordering::Relaxed);
    }

    pub fn record_emitted(&self) {
        self.emitted_bulletins.fetch_add(1, Ordering::Relaxed);
    }

    pub fn record_dropped(&self) {
        self.dropped_events.fetch_add(1, Ordering::Relaxed);
    }

    #[must_use]
    pub fn snapshot(&self) -> MetricsSnapshot {
        MetricsSnapshot {
            ingested_events: self.ingested_events.load(Ordering::Relaxed),
            emitted_bulletins: self.emitted_bulletins.load(Ordering::Relaxed),
            dropped_events: self.dropped_events.load(Ordering::Relaxed),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Serialize)]
pub struct MetricsSnapshot {
    pub ingested_events: u64,
    pub emitted_bulletins: u64,
    pub dropped_events: u64,
}
