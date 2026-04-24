use agent_feed_core::{AgentEvent, Bulletin};
use serde::Serialize;
use time::OffsetDateTime;

#[derive(Clone, Debug, Serialize)]
pub struct HealthView {
    pub status: &'static str,
    pub bind: String,
    pub ingested_events: u64,
    pub emitted_bulletins: u64,
    pub dropped_events: u64,
}

#[derive(Clone, Debug, Serialize)]
pub struct AgentsView {
    pub agents: Vec<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct SessionsView {
    pub sessions: Vec<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct AdaptersView {
    pub adapters: Vec<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct EventsView {
    pub events: Vec<AgentEvent>,
}

#[derive(Clone, Debug, Serialize)]
pub struct BulletinsView {
    pub bulletins: Vec<Bulletin>,
}

#[derive(Clone, Debug, Serialize)]
pub struct SseBulletin {
    #[serde(rename = "type")]
    pub message_type: &'static str,
    pub bulletin: Bulletin,
}

#[derive(Clone, Debug, Serialize)]
pub struct IngestView {
    pub accepted: bool,
    pub event_id: String,
    pub bulletin_id: Option<String>,
    pub bulletin_ids: Vec<String>,
    #[serde(with = "time::serde::rfc3339")]
    pub received_at: OffsetDateTime,
}
