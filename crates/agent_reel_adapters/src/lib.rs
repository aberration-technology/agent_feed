use agent_reel_core::{AgentEvent, EventKind, Severity, SourceKind};
use agent_reel_ingest::{IngestError, normalize_value};
use serde_json::Value;
use std::path::Path;
use time::OffsetDateTime;

#[derive(Debug, thiserror::Error)]
pub enum AdapterError {
    #[error(transparent)]
    Ingest(#[from] IngestError),
    #[error("json parse failed: {0}")]
    Json(#[from] serde_json::Error),
}

pub mod codex {
    use super::*;

    pub fn normalize_exec_json(value: Value) -> Result<AgentEvent, AdapterError> {
        let mut event = normalize_value(value, SourceKind::Codex)?;
        event.adapter = "codex.exec-json".to_string();
        if event.kind == EventKind::AgentMessage {
            event.kind = infer_codex_kind(&event.title);
        }
        Ok(event)
    }

    fn infer_codex_kind(title: &str) -> EventKind {
        match title {
            "turn.completed" | "turn.completed_success" => EventKind::TurnComplete,
            "turn.failed" => EventKind::TurnFail,
            "thread.started" | "turn.started" => EventKind::TurnStart,
            "error" => EventKind::Error,
            _ if title.starts_with("item.") => EventKind::AgentMessage,
            _ => EventKind::AgentMessage,
        }
    }

    #[derive(Clone, Debug, Default)]
    pub struct TranscriptState {
        pub session_id: Option<String>,
        pub turn_id: Option<String>,
        pub cwd: Option<String>,
        pub project: Option<String>,
        pub model: Option<String>,
    }

    pub fn normalize_transcript(
        input: &str,
        path: Option<&Path>,
    ) -> Result<Vec<AgentEvent>, AdapterError> {
        let mut state = TranscriptState::default();
        normalize_transcript_with_state(input, path, &mut state)
    }

    pub fn normalize_transcript_with_state(
        input: &str,
        path: Option<&Path>,
        state: &mut TranscriptState,
    ) -> Result<Vec<AgentEvent>, AdapterError> {
        let mut events = Vec::new();
        for line in input.lines().map(str::trim).filter(|line| !line.is_empty()) {
            let value = serde_json::from_str::<Value>(line)?;
            if let Some(event) = normalize_transcript_value(value, state, path) {
                events.push(event);
            }
        }
        Ok(events)
    }

    pub fn normalize_transcript_value(
        value: Value,
        state: &mut TranscriptState,
        path: Option<&Path>,
    ) -> Option<AgentEvent> {
        let timestamp = value.get("timestamp").and_then(Value::as_str);
        let envelope_type = value
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let payload = value.get("payload").unwrap_or(&Value::Null);
        let payload_type = payload
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default();

        if envelope_type == "session_meta" {
            state.session_id = payload
                .get("id")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned)
                .or_else(|| session_id_from_path(path));
            state.cwd = payload
                .get("cwd")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned);
            state.project = state.cwd.as_deref().and_then(project_from_cwd);
            return Some(build_event(
                state,
                timestamp,
                TranscriptEvent::new(
                    EventKind::SessionStart,
                    "codex session started",
                    62,
                    Severity::Notice,
                )
                .summary("transcript capture found a codex session."),
            ));
        }

        if envelope_type == "turn_context" {
            state.turn_id = payload
                .get("turn_id")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned)
                .or_else(|| state.turn_id.clone());
            state.cwd = payload
                .get("cwd")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned)
                .or_else(|| state.cwd.clone());
            state.model = payload
                .get("model")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned)
                .or_else(|| state.model.clone());
            state.project = state.cwd.as_deref().and_then(project_from_cwd);
            return None;
        }

        if let Some(turn_id) = payload.get("turn_id").and_then(Value::as_str) {
            state.turn_id = Some(turn_id.to_string());
        }

        match (envelope_type, payload_type) {
            ("event_msg", "task_started") => Some(build_event(
                state,
                timestamp,
                TranscriptEvent::new(
                    EventKind::TurnStart,
                    "codex turn started",
                    45,
                    Severity::Info,
                )
                .optional_summary(state.model.as_deref().map(|model| format!("model {model}"))),
            )),
            ("event_msg", "exec_command_end") => command_end_event(state, timestamp, payload),
            ("event_msg", "patch_apply_end") => patch_event(state, timestamp, payload),
            ("event_msg", "agent_message") => Some(build_event(
                state,
                timestamp,
                TranscriptEvent::new(
                    EventKind::AgentMessage,
                    "codex posted an update",
                    36,
                    Severity::Info,
                )
                .summary("assistant message recorded without raw content."),
            )),
            ("response_item", "function_call") | ("response_item", "custom_tool_call") => {
                tool_start_event(state, timestamp, payload)
            }
            _ => None,
        }
    }

    fn command_end_event(
        state: &TranscriptState,
        timestamp: Option<&str>,
        payload: &Value,
    ) -> Option<AgentEvent> {
        let status = payload
            .get("status")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let exit_code = payload.get("exit_code").and_then(Value::as_i64);
        let success = status == "completed" && exit_code.unwrap_or(0) == 0;
        let command = command_from_payload(payload);
        let duration = payload.get("duration").and_then(Value::as_str);
        let summary = match (exit_code, duration) {
            (Some(code), Some(duration)) => {
                format!("exit {code}; duration {duration}. raw output omitted.")
            }
            (Some(code), None) => format!("exit {code}. raw output omitted."),
            (None, Some(duration)) => {
                format!("status {status}; duration {duration}. raw output omitted.")
            }
            (None, None) => format!("status {status}. raw output omitted."),
        };
        Some(build_event(
            state,
            timestamp,
            TranscriptEvent::new(
                if success {
                    EventKind::ToolComplete
                } else {
                    EventKind::ToolFail
                },
                if success {
                    "codex command completed"
                } else {
                    "codex command failed"
                },
                if success { 48 } else { 84 },
                if success {
                    Severity::Info
                } else {
                    Severity::Warning
                },
            )
            .summary(summary)
            .optional_command(command),
        ))
    }

    fn patch_event(
        state: &TranscriptState,
        timestamp: Option<&str>,
        payload: &Value,
    ) -> Option<AgentEvent> {
        let success = payload
            .get("success")
            .and_then(Value::as_bool)
            .unwrap_or_else(|| payload.get("status").and_then(Value::as_str) == Some("completed"));
        let files = files_from_changes(payload.get("changes"));
        let summary = if files.is_empty() {
            "patch applied without exposing raw diff.".to_string()
        } else {
            format!("{} changed files. raw diff omitted.", files.len())
        };
        Some(build_event(
            state,
            timestamp,
            TranscriptEvent::new(
                if success {
                    EventKind::FileChanged
                } else {
                    EventKind::ToolFail
                },
                if success {
                    "codex patch applied"
                } else {
                    "codex patch failed"
                },
                if success { 78 } else { 86 },
                if success {
                    Severity::Notice
                } else {
                    Severity::Warning
                },
            )
            .summary(summary)
            .command("apply_patch")
            .files(files),
        ))
    }

    fn tool_start_event(
        state: &TranscriptState,
        timestamp: Option<&str>,
        payload: &Value,
    ) -> Option<AgentEvent> {
        let name = payload.get("name").and_then(Value::as_str)?;
        if name == "exec_command" {
            return Some(build_event(
                state,
                timestamp,
                TranscriptEvent::new(
                    EventKind::CommandExec,
                    "codex started a command",
                    42,
                    Severity::Info,
                )
                .summary("command lifecycle captured without command output.")
                .optional_command(command_from_arguments(payload.get("arguments"))),
            ));
        }
        if name == "apply_patch" {
            return Some(build_event(
                state,
                timestamp,
                TranscriptEvent::new(
                    EventKind::DiffCreated,
                    "codex started a patch",
                    64,
                    Severity::Info,
                )
                .summary("patch activity captured without raw diff.")
                .command("apply_patch"),
            ));
        }
        Some(build_event(
            state,
            timestamp,
            TranscriptEvent::new(
                EventKind::ToolStart,
                format!("codex started {name}"),
                30,
                Severity::Info,
            )
            .summary("tool call started.")
            .command(name),
        ))
    }

    #[derive(Clone, Debug)]
    struct TranscriptEvent {
        kind: EventKind,
        title: String,
        summary: Option<String>,
        command: Option<String>,
        files: Vec<String>,
        score_hint: u8,
        severity: Severity,
    }

    impl TranscriptEvent {
        fn new(
            kind: EventKind,
            title: impl Into<String>,
            score_hint: u8,
            severity: Severity,
        ) -> Self {
            Self {
                kind,
                title: title.into(),
                summary: None,
                command: None,
                files: Vec::new(),
                score_hint,
                severity,
            }
        }

        fn summary(mut self, summary: impl Into<String>) -> Self {
            self.summary = Some(summary.into());
            self
        }

        fn optional_summary(mut self, summary: Option<String>) -> Self {
            self.summary = summary;
            self
        }

        fn command(mut self, command: impl Into<String>) -> Self {
            self.command = Some(command.into());
            self
        }

        fn optional_command(mut self, command: Option<String>) -> Self {
            self.command = command;
            self
        }

        fn files(mut self, files: Vec<String>) -> Self {
            self.files = files;
            self
        }
    }

    fn build_event(
        state: &TranscriptState,
        timestamp: Option<&str>,
        draft: TranscriptEvent,
    ) -> AgentEvent {
        let mut event = AgentEvent::new(SourceKind::Codex, draft.kind, draft.title);
        event.agent = "codex".to_string();
        event.adapter = "codex.transcript".to_string();
        event.session_id = state.session_id.clone();
        event.turn_id = state.turn_id.clone();
        event.project = state.project.clone();
        event.cwd = state.cwd.clone();
        event.occurred_at = timestamp.and_then(parse_timestamp);
        event.summary = draft.summary;
        event.command = draft.command;
        event.files = draft.files;
        event.tags = vec!["codex".to_string(), "transcript".to_string()];
        event.score_hint = Some(draft.score_hint);
        event.severity = draft.severity;
        event
    }

    fn parse_timestamp(value: &str) -> Option<OffsetDateTime> {
        OffsetDateTime::parse(value, &time::format_description::well_known::Rfc3339).ok()
    }

    fn project_from_cwd(cwd: &str) -> Option<String> {
        Path::new(cwd)
            .file_name()
            .and_then(|name| name.to_str())
            .map(ToOwned::to_owned)
    }

    fn session_id_from_path(path: Option<&Path>) -> Option<String> {
        let file_name = path?.file_name()?.to_str()?;
        let id = file_name.strip_prefix("rollout-")?.strip_suffix(".jsonl")?;
        id.rsplit_once('-').map(|(_, id)| id.to_string())
    }

    fn command_from_payload(payload: &Value) -> Option<String> {
        payload
            .get("command")
            .and_then(command_value_to_string)
            .or_else(|| payload.get("parsed_cmd").and_then(command_value_to_string))
    }

    fn command_from_arguments(arguments: Option<&Value>) -> Option<String> {
        let arguments = arguments?;
        if let Some(command) = arguments.get("cmd").and_then(Value::as_str) {
            return Some(command.to_string());
        }
        if let Some(command) = arguments.get("command").and_then(command_value_to_string) {
            return Some(command);
        }
        if let Some(value) = arguments.as_str()
            && let Ok(parsed) = serde_json::from_str::<Value>(value)
        {
            return command_from_arguments(Some(&parsed));
        }
        None
    }

    fn command_value_to_string(value: &Value) -> Option<String> {
        match value {
            Value::String(command) => Some(command.to_string()),
            Value::Array(parts) => Some(
                parts
                    .iter()
                    .filter_map(Value::as_str)
                    .collect::<Vec<_>>()
                    .join(" "),
            )
            .filter(|command| !command.is_empty()),
            _ => None,
        }
    }

    fn files_from_changes(changes: Option<&Value>) -> Vec<String> {
        let Some(changes) = changes else {
            return Vec::new();
        };
        match changes {
            Value::Array(items) => items
                .iter()
                .filter_map(|item| {
                    item.get("path")
                        .or_else(|| item.get("file"))
                        .or_else(|| item.get("name"))
                        .and_then(Value::as_str)
                        .map(ToOwned::to_owned)
                })
                .collect(),
            Value::Object(map) => map.keys().cloned().collect(),
            _ => Vec::new(),
        }
    }
}

pub mod claude {
    use super::*;

    pub fn normalize_stream_json(value: Value) -> Result<AgentEvent, AdapterError> {
        let mut event = normalize_value(value, SourceKind::Claude)?;
        event.adapter = "claude.stream-json".to_string();
        Ok(event)
    }
}

pub mod mcp {
    use super::*;

    pub fn normalize_json_rpc(value: Value) -> Result<AgentEvent, AdapterError> {
        let mut event = normalize_value(value, SourceKind::Mcp)?;
        event.adapter = "mcp.json-rpc".to_string();
        Ok(event)
    }
}

#[cfg(test)]
mod tests {
    use super::codex::normalize_transcript;
    use agent_reel_core::EventKind;

    #[test]
    fn codex_transcript_normalizes_display_safe_events() {
        let transcript = r#"
{"type":"session_meta","timestamp":"2026-04-24T03:16:49.696Z","payload":{"id":"019dbd7d-4f56-7a11-9d9d-038a73a694af","cwd":"/home/mosure/repos/burn_dragon"}}
{"type":"turn_context","timestamp":"2026-04-24T03:16:49.697Z","payload":{"cwd":"/home/mosure/repos/burn_dragon","model":"gpt-5.5","turn_id":"turn_1"}}
{"type":"event_msg","timestamp":"2026-04-24T03:16:50.000Z","payload":{"type":"task_started","turn_id":"turn_1"}}
{"type":"event_msg","timestamp":"2026-04-24T03:17:00.000Z","payload":{"type":"exec_command_end","status":"completed","exit_code":0,"duration":"120ms","command":["cargo","test"],"stdout":"secret output"}}
{"type":"event_msg","timestamp":"2026-04-24T03:17:02.000Z","payload":{"type":"patch_apply_end","success":true,"changes":{"src/lib.rs":{}}}}
"#;

        let events = normalize_transcript(transcript, None).expect("transcript normalizes");

        assert_eq!(events.len(), 4);
        assert_eq!(events[0].kind, EventKind::SessionStart);
        assert_eq!(
            events[0].session_id.as_deref(),
            Some("019dbd7d-4f56-7a11-9d9d-038a73a694af")
        );
        assert_eq!(events[1].kind, EventKind::TurnStart);
        assert_eq!(events[2].kind, EventKind::ToolComplete);
        assert_eq!(events[2].command.as_deref(), Some("cargo test"));
        assert!(
            !events[2]
                .summary
                .as_deref()
                .unwrap_or_default()
                .contains("secret output")
        );
        assert_eq!(events[3].kind, EventKind::FileChanged);
        assert_eq!(events[3].files, vec!["src/lib.rs"]);
    }
}
