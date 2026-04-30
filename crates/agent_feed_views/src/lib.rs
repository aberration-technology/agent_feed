use agent_feed_core::{AgentEvent, Bulletin};
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct HealthView {
    pub status: String,
    pub bind: String,
    pub ingested_events: u64,
    pub emitted_bulletins: u64,
    pub dropped_events: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct StatusView {
    pub status: String,
    pub bind: String,
    pub p2p_enabled: bool,
    pub ingested_events: u64,
    pub emitted_bulletins: u64,
    pub dropped_events: u64,
    pub stored_events: usize,
    pub stored_bulletins: usize,
    #[serde(default)]
    pub story: StoryStatusView,
    #[serde(default)]
    pub publish: Option<PublishStatusView>,
    pub captured_sources: Vec<CapturedSourceView>,
    pub capture_watchers: Vec<CaptureWatchView>,
    pub last_event_kind: Option<String>,
    #[serde(with = "time::serde::rfc3339::option")]
    pub last_event_at: Option<OffsetDateTime>,
    #[serde(with = "time::serde::rfc3339::option")]
    pub last_bulletin_at: Option<OffsetDateTime>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct StoryStatusView {
    pub open_windows: usize,
    pub retained_windows: usize,
    pub settled_windows: usize,
    pub published_stories: usize,
    pub rejected_stories: usize,
    pub deduped_stories: usize,
    pub last_decision: Option<StoryDecisionView>,
    #[serde(default)]
    pub recent_decisions: Vec<StoryDecisionView>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct StoryDecisionView {
    #[serde(with = "time::serde::rfc3339")]
    pub at: OffsetDateTime,
    pub action: String,
    pub reason: String,
    pub agent: String,
    pub project: Option<String>,
    pub session_id: Option<String>,
    pub turn_id: Option<String>,
    pub family: String,
    pub score: u8,
    pub context_score: u8,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct PublishStatusUpdate {
    pub feed: String,
    pub state: String,
    pub edge: String,
    pub network_id: String,
    pub publisher: Option<String>,
    pub pending_stories: usize,
    pub last_batch_stories: usize,
    pub last_batch_capsules: usize,
    pub last_edge_accepted: usize,
    pub last_edge_feeds: usize,
    pub last_edge_headlines: usize,
    #[serde(default)]
    pub processor_sessions: usize,
    #[serde(default)]
    pub processor_events_dropped: u64,
    #[serde(default)]
    pub processor_sessions_skipped: u64,
    #[serde(default)]
    pub ambiguous_internal_candidates: u64,
    pub detail: Option<String>,
    pub last_error: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct PublishStatusView {
    pub feed: String,
    pub state: String,
    pub edge: String,
    pub network_id: String,
    pub publisher: Option<String>,
    pub pending_stories: usize,
    pub last_batch_stories: usize,
    pub last_batch_capsules: usize,
    pub last_edge_accepted: usize,
    pub last_edge_feeds: usize,
    pub last_edge_headlines: usize,
    #[serde(default)]
    pub processor_sessions: usize,
    #[serde(default)]
    pub processor_events_dropped: u64,
    #[serde(default)]
    pub processor_sessions_skipped: u64,
    #[serde(default)]
    pub ambiguous_internal_candidates: u64,
    pub detail: Option<String>,
    pub last_error: Option<String>,
    #[serde(with = "time::serde::rfc3339")]
    pub updated_at: OffsetDateTime,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct CapturedSourceView {
    pub source: String,
    pub agent: String,
    pub adapter: String,
    pub events: usize,
    pub sessions: usize,
    pub last_event_kind: String,
    #[serde(with = "time::serde::rfc3339")]
    pub last_event_at: OffsetDateTime,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct CaptureWatchUpdate {
    pub agent: String,
    pub adapter: String,
    pub label: String,
    pub state: String,
    pub workspace: Option<String>,
    #[serde(default)]
    pub session_id: Option<String>,
    #[serde(default)]
    pub last_append_ms: Option<u64>,
    pub offset: u64,
    pub file_len: u64,
    pub imported_events: usize,
    pub filtered_events: usize,
    pub poll_ms: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct CaptureWatchView {
    pub agent: String,
    pub adapter: String,
    pub label: String,
    pub state: String,
    pub workspace: Option<String>,
    #[serde(default)]
    pub session_id: Option<String>,
    #[serde(default)]
    pub last_append_ms: Option<u64>,
    pub offset: u64,
    pub file_len: u64,
    pub imported_events: usize,
    pub filtered_events: usize,
    pub poll_ms: u64,
    #[serde(with = "time::serde::rfc3339")]
    pub updated_at: OffsetDateTime,
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
