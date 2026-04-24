use agent_feed_core::{AgentEvent, Bulletin, EventKind, SourceKind};
use agent_feed_highlight::bulletin_from_event;
use agent_feed_ingest::{IngestError, normalize_value};
use serde_json::json;

#[must_use]
pub fn fake_turn_complete(agent: &str, project: &str) -> AgentEvent {
    let mut event = AgentEvent::new(
        SourceKind::Generic,
        EventKind::TurnComplete,
        "turn completed",
    );
    event.agent = agent.to_string();
    event.project = Some(project.to_string());
    event.summary = Some("fake stream produced a display-safe completion.".to_string());
    event
}

#[must_use]
pub fn fake_bulletin(agent: &str, project: &str) -> Bulletin {
    bulletin_from_event(&fake_turn_complete(agent, project))
}

pub fn fake_generic_json() -> Result<AgentEvent, IngestError> {
    normalize_value(
        json!({
            "agent": "codex",
            "project": "agent_feed",
            "kind": "turn.complete",
            "title": "fake event rendered",
            "summary": "fixture event produced one bulletin"
        }),
        SourceKind::Generic,
    )
}
