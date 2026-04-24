use agent_feed_core::{AgentEvent, EventKind, PrivacyClass, RawAgentEvent, Severity, SourceKind};
use serde::Deserialize;
use serde_json::Value;

#[derive(Debug, thiserror::Error)]
pub enum IngestError {
    #[error("json parse failed: {0}")]
    Json(#[from] serde_json::Error),
    #[error("no JSON events were found")]
    Empty,
}

#[derive(Clone, Debug, Deserialize)]
pub struct GenericIngestEvent {
    #[serde(default)]
    pub source: Option<String>,
    #[serde(default)]
    pub agent: Option<String>,
    #[serde(default)]
    pub adapter: Option<String>,
    #[serde(default)]
    pub session_id: Option<String>,
    #[serde(default)]
    pub turn_id: Option<String>,
    #[serde(default)]
    pub item_id: Option<String>,
    #[serde(default)]
    pub project: Option<String>,
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub kind: Option<String>,
    #[serde(default)]
    pub severity: Option<String>,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub summary: Option<String>,
    #[serde(default)]
    pub tool: Option<String>,
    #[serde(default)]
    pub command: Option<String>,
    #[serde(default)]
    pub uri: Option<String>,
    #[serde(default)]
    pub files: Vec<String>,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub score_hint: Option<u8>,
}

#[must_use]
pub fn source_from_str(value: &str) -> SourceKind {
    match value {
        "codex" | "codex-jsonl" | "codex-exec-json" | "codex-hook" | "codex-transcript" => {
            SourceKind::Codex
        }
        "claude" | "claude-code" | "claude-stream-json" | "claude-hook" => SourceKind::Claude,
        "mcp" => SourceKind::Mcp,
        "otel" => SourceKind::Otel,
        "shell" => SourceKind::Shell,
        _ => SourceKind::Generic,
    }
}

#[must_use]
pub fn severity_from_str(value: &str) -> Severity {
    match value {
        "debug" => Severity::Debug,
        "notice" => Severity::Notice,
        "warning" | "warn" => Severity::Warning,
        "critical" | "error" => Severity::Critical,
        _ => Severity::Info,
    }
}

pub fn normalize_value(
    value: Value,
    default_source: SourceKind,
) -> Result<AgentEvent, IngestError> {
    let raw = RawAgentEvent::new(default_source, value);
    normalize_raw(raw)
}

pub fn normalize_raw(raw: RawAgentEvent) -> Result<AgentEvent, IngestError> {
    let fallback_title = infer_title(&raw.payload);
    let generic: GenericIngestEvent = serde_json::from_value(raw.payload)?;
    let source = generic
        .source
        .as_deref()
        .map(source_from_str)
        .unwrap_or(raw.source);
    let kind = generic
        .kind
        .as_deref()
        .map(EventKind::parse)
        .unwrap_or(EventKind::AgentMessage);

    let mut event = AgentEvent::new(source, kind, fallback_title);
    event.agent = generic.agent.unwrap_or_else(|| source.as_str().to_string());
    event.adapter = generic
        .adapter
        .unwrap_or_else(|| format!("{}-generic", source.as_str()));
    event.session_id = generic.session_id;
    event.turn_id = generic.turn_id;
    event.item_id = generic.item_id;
    event.project = generic.project;
    event.cwd = generic.cwd;
    event.severity = generic
        .severity
        .as_deref()
        .map(severity_from_str)
        .unwrap_or(event.severity);
    if let Some(title) = generic.title {
        event.title = title;
    }
    event.summary = generic.summary;
    event.tool = generic.tool;
    event.command = generic.command;
    event.uri = generic.uri;
    event.files = generic.files;
    event.tags = generic.tags;
    event.score_hint = generic.score_hint;
    event.privacy = PrivacyClass::Redacted;
    Ok(event)
}

pub fn parse_jsonl(
    input: &str,
    default_source: SourceKind,
) -> Result<Vec<AgentEvent>, IngestError> {
    let mut events = Vec::new();
    for line in input.lines().map(str::trim).filter(|line| !line.is_empty()) {
        let value = serde_json::from_str::<Value>(line)?;
        events.push(normalize_value(value, default_source)?);
    }

    if events.is_empty() {
        return Err(IngestError::Empty);
    }

    Ok(events)
}

fn infer_title(value: &Value) -> String {
    value
        .get("title")
        .and_then(Value::as_str)
        .or_else(|| value.get("message").and_then(Value::as_str))
        .or_else(|| value.get("type").and_then(Value::as_str))
        .unwrap_or("event received")
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generic_json_becomes_canonical_event() {
        let value = serde_json::json!({
            "agent": "codex",
            "project": "agent_feed",
            "kind": "turn.complete",
            "title": "turn finished",
            "files": ["src/main.rs"]
        });

        let event = normalize_value(value, SourceKind::Generic).expect("event normalizes");

        assert_eq!(event.source, SourceKind::Generic);
        assert_eq!(event.agent, "codex");
        assert_eq!(event.kind, EventKind::TurnComplete);
        assert_eq!(event.title, "turn finished");
        assert_eq!(event.files, vec!["src/main.rs"]);
    }
}
