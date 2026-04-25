use agent_feed_core::{AgentEvent, EventKind, Severity, SourceKind};
use agent_feed_ingest::{IngestError, normalize_value};
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

fn is_test_command(command: &str) -> bool {
    let lowered = command.to_ascii_lowercase();
    lowered.contains("cargo test")
        || lowered.contains("cargo nextest")
        || lowered.contains("pytest")
        || lowered.contains("npm test")
        || lowered.contains("pnpm test")
        || lowered.contains("yarn test")
        || lowered.contains("bun test")
        || lowered.contains("go test")
        || lowered.contains("swift test")
        || lowered.contains("zig test")
        || lowered.contains("dotnet test")
        || lowered.contains("gradle test")
        || lowered.contains("mvn test")
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
            ("event_msg", "task_complete") => task_complete_event(state, timestamp, payload),
            ("event_msg", "turn_aborted") => task_failed_event(state, timestamp, payload),
            ("event_msg", "item_completed") => item_completed_event(state, timestamp, payload),
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

    fn task_complete_event(
        state: &TranscriptState,
        timestamp: Option<&str>,
        payload: &Value,
    ) -> Option<AgentEvent> {
        let summary = payload
            .get("last_agent_message")
            .and_then(Value::as_str)
            .and_then(display_safe_agent_sentence)
            .or_else(|| {
                payload
                    .get("duration_ms")
                    .and_then(Value::as_u64)
                    .map(|duration| format!("turn completed in {}s.", duration / 1000))
            })
            .unwrap_or_else(|| "turn completed.".to_string());
        Some(build_event(
            state,
            timestamp,
            TranscriptEvent::new(
                EventKind::TurnComplete,
                "codex turn completed",
                82,
                Severity::Notice,
            )
            .summary(summary),
        ))
    }

    fn task_failed_event(
        state: &TranscriptState,
        timestamp: Option<&str>,
        payload: &Value,
    ) -> Option<AgentEvent> {
        let summary = payload
            .get("reason")
            .and_then(Value::as_str)
            .and_then(display_safe_agent_sentence)
            .unwrap_or_else(|| "turn stopped before completion.".to_string());
        Some(build_event(
            state,
            timestamp,
            TranscriptEvent::new(
                EventKind::TurnFail,
                "codex turn failed",
                92,
                Severity::Warning,
            )
            .summary(summary),
        ))
    }

    fn item_completed_event(
        state: &TranscriptState,
        timestamp: Option<&str>,
        payload: &Value,
    ) -> Option<AgentEvent> {
        let item = payload.get("item")?;
        let item_type = item.get("type").and_then(Value::as_str).unwrap_or_default();
        if item_type != "Plan" {
            return None;
        }
        Some(build_event(
            state,
            timestamp,
            TranscriptEvent::new(
                EventKind::PlanUpdate,
                "codex updated the plan",
                74,
                Severity::Notice,
            )
            .summary("plan update recorded without raw plan text."),
        ))
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
        let summary = if command.as_deref().is_some_and(is_test_command) {
            match (success, duration) {
                (true, Some(duration)) => format!("test command passed; duration {duration}."),
                (true, None) => "test command passed.".to_string(),
                (false, Some(duration)) => format!("test command failed; duration {duration}."),
                (false, None) => "test command failed.".to_string(),
            }
        } else {
            match (exit_code, duration) {
                (Some(code), Some(duration)) => {
                    format!("exit {code}; duration {duration}. raw output omitted.")
                }
                (Some(code), None) => format!("exit {code}. raw output omitted."),
                (None, Some(duration)) => {
                    format!("status {status}; duration {duration}. raw output omitted.")
                }
                (None, None) => format!("status {status}. raw output omitted."),
            }
        };
        let (kind, title, score, severity) = if command.as_deref().is_some_and(is_test_command) {
            if success {
                (
                    EventKind::TestPass,
                    "codex tests passed",
                    76,
                    Severity::Notice,
                )
            } else {
                (
                    EventKind::TestFail,
                    "codex tests failed",
                    90,
                    Severity::Warning,
                )
            }
        } else if success {
            (
                EventKind::ToolComplete,
                "codex command completed",
                48,
                Severity::Info,
            )
        } else {
            (
                EventKind::ToolFail,
                "codex command failed",
                84,
                Severity::Warning,
            )
        };
        Some(build_event(
            state,
            timestamp,
            TranscriptEvent::new(kind, title, score, severity)
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
            .get("parsed_cmd")
            .and_then(parsed_command_to_string)
            .or_else(|| payload.get("command").and_then(command_value_to_string))
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

    fn display_safe_agent_sentence(value: &str) -> Option<String> {
        let sentence = value
            .lines()
            .map(str::trim)
            .find(|line| {
                !line.is_empty()
                    && !line.starts_with("```")
                    && !line.starts_with('#')
                    && !line.starts_with("- ")
                    && !line.starts_with("* ")
            })?
            .trim_matches(['`', '"', '\''])
            .trim();
        if sentence.is_empty() {
            return None;
        }
        let lowered = sentence.to_ascii_lowercase();
        if [
            "secret",
            "token",
            "password",
            "api key",
            "stdout",
            "stderr",
            "diff --git",
        ]
        .iter()
        .any(|needle| lowered.contains(needle))
        {
            return None;
        }
        Some(clamp_words(sentence, 24))
    }

    fn clamp_words(input: &str, max_words: usize) -> String {
        let mut words = input.split_whitespace();
        let mut output = Vec::new();
        for _ in 0..max_words {
            if let Some(word) = words.next() {
                output.push(word);
            }
        }
        if output.is_empty() {
            return String::new();
        }
        let mut value = output.join(" ");
        if words.next().is_some() {
            value.push_str("...");
        }
        if !value.ends_with(['.', '!', '?']) {
            value.push('.');
        }
        value
    }

    fn command_value_to_string(value: &Value) -> Option<String> {
        let command = match value {
            Value::String(command) => command.to_string(),
            Value::Array(parts) => {
                if let Some(command) = shell_wrapper_inner_command(parts) {
                    return Some(command.to_string());
                }
                parts
                    .iter()
                    .filter_map(Value::as_str)
                    .collect::<Vec<_>>()
                    .join(" ")
            }
            _ => return None,
        };
        Some(command).filter(|command| !command.is_empty())
    }

    fn parsed_command_to_string(value: &Value) -> Option<String> {
        match value {
            Value::Array(items) => items.iter().find_map(parsed_command_to_string),
            Value::Object(map) => map
                .get("cmd")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned),
            Value::String(command) => Some(command.to_string()),
            _ => None,
        }
        .filter(|command| !command.is_empty())
    }

    fn shell_wrapper_inner_command(parts: &[Value]) -> Option<&str> {
        let shell = parts.first()?.as_str()?;
        let shell_name = Path::new(shell)
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or(shell);
        if !matches!(
            shell_name,
            "bash" | "sh" | "zsh" | "fish" | "pwsh" | "powershell"
        ) {
            return None;
        }
        parts
            .windows(2)
            .find(|window| matches!(window[0].as_str(), Some("-c" | "-lc" | "/c" | "-Command")))
            .and_then(|window| window[1].as_str())
            .filter(|command| !command.is_empty())
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

    #[derive(Clone, Debug, Default)]
    pub struct ClaudeState {
        pub session_id: Option<String>,
        pub cwd: Option<String>,
        pub project: Option<String>,
        pub model: Option<String>,
        pub transcript_path: Option<String>,
        pub last_tool: Option<String>,
        pub last_command: Option<String>,
        pub last_files: Vec<String>,
    }

    pub fn normalize_stream_json(value: Value) -> Result<AgentEvent, AdapterError> {
        let mut state = ClaudeState::default();
        if let Some(event) = normalize_stream_value(value.clone(), &mut state, None) {
            return Ok(event);
        }

        let mut event = normalize_value(value, SourceKind::Claude)?;
        event.adapter = "claude.stream-json".to_string();
        event.agent = "claude".to_string();
        Ok(event)
    }

    pub fn normalize_stream(
        input: &str,
        path: Option<&Path>,
    ) -> Result<Vec<AgentEvent>, AdapterError> {
        let mut state = ClaudeState::default();
        normalize_stream_with_state(input, path, &mut state)
    }

    pub fn normalize_stream_with_state(
        input: &str,
        path: Option<&Path>,
        state: &mut ClaudeState,
    ) -> Result<Vec<AgentEvent>, AdapterError> {
        let mut events = Vec::new();
        for line in input.lines().map(str::trim).filter(|line| !line.is_empty()) {
            let value = serde_json::from_str::<Value>(line)?;
            if let Some(event) = normalize_stream_value(value, state, path) {
                events.push(event);
            }
        }
        Ok(events)
    }

    pub fn normalize_stream_value(
        value: Value,
        state: &mut ClaudeState,
        path: Option<&Path>,
    ) -> Option<AgentEvent> {
        update_state_from_value(state, &value, path);

        if let Some(event_name) = value.get("hook_event_name").and_then(Value::as_str) {
            return hook_event(state, event_name, &value);
        }

        let message = value.get("message").unwrap_or(&value);
        let message_type = value
            .get("type")
            .or_else(|| message.get("type"))
            .and_then(Value::as_str)
            .unwrap_or_default();
        let subtype = value
            .get("subtype")
            .or_else(|| message.get("subtype"))
            .and_then(Value::as_str)
            .unwrap_or_default();

        match message_type {
            "system" if subtype == "init" || subtype.is_empty() => Some(build_event(
                state,
                timestamp_from(&value),
                ClaudeEvent::new(
                    EventKind::SessionStart,
                    "claude session started",
                    62,
                    Severity::Notice,
                )
                .summary("stream capture found a claude session."),
            )),
            "assistant" => assistant_event(state, timestamp_from(&value), message),
            "result" => result_event(state, timestamp_from(&value), &value),
            "tool_result" => tool_result_event(state, timestamp_from(&value), &value),
            "error" => Some(build_event(
                state,
                timestamp_from(&value),
                ClaudeEvent::new(
                    EventKind::Error,
                    "claude stream error",
                    90,
                    Severity::Critical,
                )
                .summary("claude stream reported an error. raw output omitted."),
            )),
            "user" => None,
            _ => None,
        }
    }

    fn update_state_from_value(state: &mut ClaudeState, value: &Value, path: Option<&Path>) {
        if let Some(session_id) = value
            .get("session_id")
            .or_else(|| {
                value
                    .get("message")
                    .and_then(|message| message.get("session_id"))
            })
            .and_then(Value::as_str)
        {
            state.session_id = Some(session_id.to_string());
        } else if state.session_id.is_none() {
            state.session_id = session_id_from_path(path);
        }

        if let Some(cwd) = value.get("cwd").and_then(Value::as_str) {
            state.cwd = Some(cwd.to_string());
            state.project = project_from_cwd(cwd);
        }
        if let Some(model) = value
            .get("model")
            .or_else(|| {
                value
                    .get("message")
                    .and_then(|message| message.get("model"))
            })
            .and_then(Value::as_str)
        {
            state.model = Some(model.to_string());
        }
        if let Some(transcript_path) = value.get("transcript_path").and_then(Value::as_str) {
            state.transcript_path = Some(transcript_path.to_string());
        }
    }

    fn hook_event(state: &ClaudeState, event_name: &str, value: &Value) -> Option<AgentEvent> {
        let tool_name = value
            .get("tool_name")
            .and_then(Value::as_str)
            .unwrap_or("tool");
        let denied = value
            .get("permission_decision")
            .or_else(|| value.get("decision"))
            .and_then(Value::as_str)
            .is_some_and(|decision| matches!(decision, "deny" | "denied" | "block" | "blocked"));
        match event_name {
            "SessionStart" => Some(build_event(
                state,
                None,
                ClaudeEvent::new(
                    EventKind::SessionStart,
                    "claude session started",
                    62,
                    Severity::Notice,
                )
                .summary("hook captured a claude session start."),
            )),
            "PreToolUse" if denied => Some(build_event(
                state,
                None,
                ClaudeEvent::new(
                    EventKind::PermissionDenied,
                    format!("claude denied {tool_name}"),
                    95,
                    Severity::Critical,
                )
                .summary("tool permission was denied. raw input omitted.")
                .tool(tool_name)
                .optional_command(command_from_tool_input(value.get("tool_input"))),
            )),
            "PreToolUse" => Some(build_event(
                state,
                None,
                ClaudeEvent::new(
                    EventKind::PermissionRequest,
                    format!("claude requested {tool_name}"),
                    82,
                    Severity::Warning,
                )
                .summary("tool permission request captured without raw output.")
                .tool(tool_name)
                .optional_command(command_from_tool_input(value.get("tool_input"))),
            )),
            "PostToolUse" => {
                let failed = tool_response_failed(value.get("tool_response"));
                let files = files_from_tool_input(value.get("tool_input"));
                let command = command_from_tool_input(value.get("tool_input"));
                let test_command = command.as_deref().is_some_and(is_test_command);
                Some(build_event(
                    state,
                    None,
                    ClaudeEvent::new(
                        if test_command && failed {
                            EventKind::TestFail
                        } else if test_command {
                            EventKind::TestPass
                        } else if failed {
                            EventKind::ToolFail
                        } else if is_file_tool(tool_name) {
                            EventKind::FileChanged
                        } else {
                            EventKind::ToolComplete
                        },
                        if test_command && failed {
                            "claude tests failed".to_string()
                        } else if test_command {
                            "claude tests passed".to_string()
                        } else if failed {
                            format!("claude {tool_name} failed")
                        } else if is_file_tool(tool_name) {
                            "claude changed files".to_string()
                        } else {
                            format!("claude {tool_name} completed")
                        },
                        if test_command && failed {
                            90
                        } else if test_command {
                            76
                        } else if failed {
                            86
                        } else {
                            58
                        },
                        if failed {
                            Severity::Warning
                        } else if test_command {
                            Severity::Notice
                        } else {
                            Severity::Info
                        },
                    )
                    .summary(if test_command && failed {
                        "test command failed."
                    } else if test_command {
                        "test command passed."
                    } else {
                        "tool lifecycle captured without raw output."
                    })
                    .tool(tool_name)
                    .optional_command(command)
                    .files(files),
                ))
            }
            "Stop" | "SubagentStop" => Some(build_event(
                state,
                None,
                ClaudeEvent::new(
                    EventKind::TurnComplete,
                    if event_name == "SubagentStop" {
                        "claude subagent completed"
                    } else {
                        "claude turn completed"
                    },
                    78,
                    Severity::Notice,
                )
                .summary("claude lifecycle completed. raw transcript omitted."),
            )),
            "PreCompact" => Some(build_event(
                state,
                None,
                ClaudeEvent::new(
                    EventKind::SummaryCreated,
                    "claude compacted context",
                    64,
                    Severity::Info,
                )
                .summary("context compaction captured without raw transcript."),
            )),
            "Notification" => Some(build_event(
                state,
                None,
                ClaudeEvent::new(
                    EventKind::AgentMessage,
                    "claude notification received",
                    30,
                    Severity::Info,
                )
                .summary("notification captured without raw content."),
            )),
            _ => None,
        }
    }

    fn assistant_event(
        state: &mut ClaudeState,
        timestamp: Option<&str>,
        message: &Value,
    ) -> Option<AgentEvent> {
        let content = message.get("content").and_then(Value::as_array);
        let tool_use = content.and_then(|items| {
            items.iter().find(|item| {
                item.get("type").and_then(Value::as_str) == Some("tool_use")
                    || item.get("type").and_then(Value::as_str) == Some("server_tool_use")
            })
        });
        if let Some(tool_use) = tool_use {
            let name = tool_use
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("tool");
            let input = tool_use.get("input");
            let command = command_from_tool_input(input);
            let files = files_from_tool_input(input);
            state.last_tool = Some(name.to_string());
            state.last_command = command.clone();
            state.last_files = files.clone();
            return Some(build_event(
                state,
                timestamp,
                ClaudeEvent::new(
                    if name == "Bash" {
                        EventKind::CommandExec
                    } else {
                        EventKind::ToolStart
                    },
                    if name == "Bash" {
                        "claude started a command".to_string()
                    } else {
                        format!("claude started {name}")
                    },
                    if name == "Bash" { 46 } else { 34 },
                    Severity::Info,
                )
                .summary("tool call captured without raw output.")
                .tool(name)
                .optional_command(command)
                .files(files),
            ));
        }

        Some(build_event(
            state,
            timestamp,
            ClaudeEvent::new(
                EventKind::AgentMessage,
                "claude posted an update",
                36,
                Severity::Info,
            )
            .summary("assistant message recorded without raw content."),
        ))
    }

    fn result_event(
        state: &ClaudeState,
        timestamp: Option<&str>,
        value: &Value,
    ) -> Option<AgentEvent> {
        let failed = value
            .get("is_error")
            .and_then(Value::as_bool)
            .unwrap_or_else(|| value.get("subtype").and_then(Value::as_str) == Some("error"));
        let duration = value
            .get("duration_ms")
            .and_then(Value::as_u64)
            .map(|duration| format!("{duration}ms"));
        let summary = duration.map_or_else(
            || "result captured without raw content.".to_string(),
            |duration| format!("duration {duration}. raw content omitted."),
        );
        Some(build_event(
            state,
            timestamp,
            ClaudeEvent::new(
                if failed {
                    EventKind::TurnFail
                } else {
                    EventKind::TurnComplete
                },
                if failed {
                    "claude turn failed"
                } else {
                    "claude turn completed"
                },
                if failed { 90 } else { 80 },
                if failed {
                    Severity::Warning
                } else {
                    Severity::Notice
                },
            )
            .summary(summary),
        ))
    }

    fn tool_result_event(
        state: &ClaudeState,
        timestamp: Option<&str>,
        value: &Value,
    ) -> Option<AgentEvent> {
        let failed = value
            .get("is_error")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let command = state.last_command.clone();
        let tool = state
            .last_tool
            .clone()
            .unwrap_or_else(|| "tool".to_string());
        let files = state.last_files.clone();
        let test_command = command.as_deref().is_some_and(is_test_command);
        Some(build_event(
            state,
            timestamp,
            ClaudeEvent::new(
                if test_command && failed {
                    EventKind::TestFail
                } else if test_command {
                    EventKind::TestPass
                } else if failed {
                    EventKind::ToolFail
                } else {
                    EventKind::ToolComplete
                },
                if test_command && failed {
                    "claude tests failed"
                } else if test_command {
                    "claude tests passed"
                } else if failed {
                    "claude tool failed"
                } else {
                    "claude tool completed"
                },
                if test_command && failed {
                    90
                } else if test_command {
                    76
                } else if failed {
                    84
                } else {
                    48
                },
                if failed {
                    Severity::Warning
                } else if test_command {
                    Severity::Notice
                } else {
                    Severity::Info
                },
            )
            .summary(if test_command && failed {
                "test command failed."
            } else if test_command {
                "test command passed."
            } else {
                "tool result captured without raw output."
            })
            .tool(tool)
            .optional_command(command)
            .files(files),
        ))
    }

    #[derive(Clone, Debug)]
    struct ClaudeEvent {
        kind: EventKind,
        title: String,
        summary: Option<String>,
        tool: Option<String>,
        command: Option<String>,
        files: Vec<String>,
        score_hint: u8,
        severity: Severity,
    }

    impl ClaudeEvent {
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
                tool: None,
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

        fn tool(mut self, tool: impl Into<String>) -> Self {
            self.tool = Some(tool.into());
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

    fn build_event(state: &ClaudeState, timestamp: Option<&str>, draft: ClaudeEvent) -> AgentEvent {
        let mut event = AgentEvent::new(SourceKind::Claude, draft.kind, draft.title);
        event.agent = "claude".to_string();
        event.adapter = "claude.stream-json".to_string();
        event.session_id = state.session_id.clone();
        event.project = state.project.clone();
        event.cwd = state.cwd.clone();
        event.occurred_at = timestamp.and_then(parse_timestamp);
        event.summary = draft.summary;
        event.tool = draft.tool;
        event.command = draft.command;
        event.files = draft.files;
        event.tags = vec!["claude".to_string(), "stream-json".to_string()];
        event.score_hint = Some(draft.score_hint);
        event.severity = draft.severity;
        event
    }

    fn timestamp_from(value: &Value) -> Option<&str> {
        value
            .get("timestamp")
            .or_else(|| value.get("created_at"))
            .and_then(Value::as_str)
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
        path?
            .file_stem()
            .and_then(|name| name.to_str())
            .map(ToOwned::to_owned)
    }

    fn command_from_tool_input(input: Option<&Value>) -> Option<String> {
        let input = input?;
        input
            .get("command")
            .or_else(|| input.get("cmd"))
            .and_then(Value::as_str)
            .map(ToOwned::to_owned)
    }

    fn files_from_tool_input(input: Option<&Value>) -> Vec<String> {
        let Some(input) = input else {
            return Vec::new();
        };
        ["file_path", "path", "notebook_path"]
            .into_iter()
            .filter_map(|key| {
                input
                    .get(key)
                    .and_then(Value::as_str)
                    .map(ToOwned::to_owned)
            })
            .collect()
    }

    fn tool_response_failed(response: Option<&Value>) -> bool {
        response.is_some_and(|response| {
            response
                .get("is_error")
                .or_else(|| response.get("error"))
                .and_then(Value::as_bool)
                .unwrap_or(false)
                || response.get("status").and_then(Value::as_str) == Some("error")
        })
    }

    fn is_file_tool(tool: &str) -> bool {
        matches!(tool, "Write" | "Edit" | "MultiEdit" | "NotebookEdit")
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
    use super::claude::normalize_stream;
    use super::codex::normalize_transcript;
    use agent_feed_core::EventKind;

    #[test]
    fn codex_transcript_normalizes_display_safe_events() {
        let transcript = r#"
{"type":"session_meta","timestamp":"2026-04-24T03:16:49.696Z","payload":{"id":"019dbd7d-4f56-7a11-9d9d-038a73a694af","cwd":"/home/mosure/repos/burn_dragon"}}
{"type":"turn_context","timestamp":"2026-04-24T03:16:49.697Z","payload":{"cwd":"/home/mosure/repos/burn_dragon","model":"gpt-5.5","turn_id":"turn_1"}}
{"type":"event_msg","timestamp":"2026-04-24T03:16:50.000Z","payload":{"type":"task_started","turn_id":"turn_1"}}
{"type":"event_msg","timestamp":"2026-04-24T03:17:00.000Z","payload":{"type":"exec_command_end","status":"completed","exit_code":0,"duration":"120ms","command":["cargo","test"],"stdout":"secret output"}}
{"type":"event_msg","timestamp":"2026-04-24T03:17:02.000Z","payload":{"type":"patch_apply_end","success":true,"changes":{"src/lib.rs":{}}}}
{"type":"event_msg","timestamp":"2026-04-24T03:17:05.000Z","payload":{"type":"task_complete","turn_id":"turn_1","last_agent_message":"Implemented the release flow.\n\nSecret token output omitted.","duration_ms":15000}}
"#;

        let events = normalize_transcript(transcript, None).expect("transcript normalizes");

        assert_eq!(events.len(), 5);
        assert_eq!(events[0].kind, EventKind::SessionStart);
        assert_eq!(
            events[0].session_id.as_deref(),
            Some("019dbd7d-4f56-7a11-9d9d-038a73a694af")
        );
        assert_eq!(events[1].kind, EventKind::TurnStart);
        assert_eq!(events[2].kind, EventKind::TestPass);
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
        assert_eq!(events[4].kind, EventKind::TurnComplete);
        assert_eq!(events[4].title, "codex turn completed");
        assert_eq!(
            events[4].summary.as_deref(),
            Some("Implemented the release flow.")
        );
        assert!(
            !events[4]
                .summary
                .as_deref()
                .unwrap_or_default()
                .contains("Secret")
        );
    }

    #[test]
    fn codex_transcript_normalizes_plan_and_aborted_turn_events() {
        let transcript = r#"
{"type":"session_meta","timestamp":"2026-04-24T03:16:49.696Z","payload":{"id":"session","cwd":"/home/mosure/repos/agent_feed"}}
{"type":"turn_context","timestamp":"2026-04-24T03:16:49.697Z","payload":{"cwd":"/home/mosure/repos/agent_feed","turn_id":"turn_1"}}
{"type":"event_msg","timestamp":"2026-04-24T03:16:50.000Z","payload":{"type":"item_completed","turn_id":"turn_1","item":{"type":"Plan","text":"raw plan text"}}}
{"type":"event_msg","timestamp":"2026-04-24T03:17:05.000Z","payload":{"type":"turn_aborted","turn_id":"turn_1","reason":"interrupted by operator"}}
"#;

        let events = normalize_transcript(transcript, None).expect("transcript normalizes");

        assert_eq!(events.len(), 3);
        assert_eq!(events[1].kind, EventKind::PlanUpdate);
        assert_eq!(
            events[1].summary.as_deref(),
            Some("plan update recorded without raw plan text.")
        );
        assert_eq!(events[2].kind, EventKind::TurnFail);
        assert_eq!(
            events[2].summary.as_deref(),
            Some("interrupted by operator.")
        );
    }

    #[test]
    fn codex_transcript_prefers_parsed_command_over_shell_wrapper() {
        let transcript = r#"
{"type":"session_meta","timestamp":"2026-04-24T03:16:49.696Z","payload":{"id":"session","cwd":"/home/mosure/repos/agent_feed"}}
{"type":"turn_context","timestamp":"2026-04-24T03:16:49.697Z","payload":{"cwd":"/home/mosure/repos/agent_feed","turn_id":"turn_1"}}
{"type":"event_msg","timestamp":"2026-04-24T03:17:00.000Z","payload":{"type":"exec_command_end","status":"failed","exit_code":1,"command":["/usr/bin/zsh","-lc","cargo test --all"],"parsed_cmd":[{"type":"unknown","cmd":"cargo test --all"}]}}
"#;

        let events = normalize_transcript(transcript, None).expect("transcript normalizes");

        assert_eq!(events[1].kind, EventKind::TestFail);
        assert_eq!(events[1].command.as_deref(), Some("cargo test --all"));
    }

    #[test]
    fn codex_transcript_extracts_shell_inner_command_when_parsed_command_missing() {
        let transcript = r#"
{"type":"session_meta","timestamp":"2026-04-24T03:16:49.696Z","payload":{"id":"session","cwd":"/home/mosure/repos/agent_feed"}}
{"type":"turn_context","timestamp":"2026-04-24T03:16:49.697Z","payload":{"cwd":"/home/mosure/repos/agent_feed","turn_id":"turn_1"}}
{"type":"event_msg","timestamp":"2026-04-24T03:17:00.000Z","payload":{"type":"exec_command_end","status":"failed","exit_code":1,"command":["/usr/bin/zsh","-lc","git status --short"]}}
"#;

        let events = normalize_transcript(transcript, None).expect("transcript normalizes");

        assert_eq!(events[1].kind, EventKind::ToolFail);
        assert_eq!(events[1].command.as_deref(), Some("git status --short"));
    }

    #[test]
    fn codex_transcript_keeps_plain_wrapper_when_no_inner_command_exists() {
        let transcript = r#"
{"type":"session_meta","timestamp":"2026-04-24T03:16:49.696Z","payload":{"id":"session","cwd":"/home/mosure/repos/agent_feed"}}
{"type":"turn_context","timestamp":"2026-04-24T03:16:49.697Z","payload":{"cwd":"/home/mosure/repos/agent_feed","turn_id":"turn_1"}}
{"type":"event_msg","timestamp":"2026-04-24T03:17:00.000Z","payload":{"type":"exec_command_end","status":"failed","exit_code":1,"command":["/usr/bin/zsh"]}}
"#;

        let events = normalize_transcript(transcript, None).expect("transcript normalizes");

        assert_eq!(events[1].kind, EventKind::ToolFail);
        assert_eq!(events[1].command.as_deref(), Some("/usr/bin/zsh"));
    }

    #[test]
    fn claude_stream_json_normalizes_display_safe_events() {
        let stream = r#"
{"type":"system","subtype":"init","session_id":"claude-1","cwd":"/home/mosure/repos/agent_feed","model":"claude-sonnet-4-6"}
{"type":"assistant","message":{"content":[{"type":"tool_use","name":"Bash","input":{"command":"cargo test","raw_secret":"hidden"}}]}}
{"type":"result","subtype":"success","duration_ms":1200,"result":"raw answer omitted"}
"#;

        let events = normalize_stream(stream, None).expect("stream normalizes");

        assert_eq!(events.len(), 3);
        assert_eq!(events[0].kind, EventKind::SessionStart);
        assert_eq!(events[0].session_id.as_deref(), Some("claude-1"));
        assert_eq!(events[1].kind, EventKind::CommandExec);
        assert_eq!(events[1].command.as_deref(), Some("cargo test"));
        assert!(
            !events[1]
                .summary
                .as_deref()
                .unwrap_or_default()
                .contains("hidden")
        );
        assert_eq!(events[2].kind, EventKind::TurnComplete);
    }

    #[test]
    fn claude_tool_result_uses_prior_bash_context_for_test_signal() {
        let stream = r#"
{"type":"system","subtype":"init","session_id":"claude-1","cwd":"/home/mosure/repos/agent_feed","model":"claude-sonnet-4-6"}
{"type":"assistant","message":{"content":[{"type":"tool_use","name":"Bash","input":{"command":"cargo test --all"}}]}}
{"type":"tool_result","is_error":true,"content":"raw failing output"}
"#;

        let events = normalize_stream(stream, None).expect("stream normalizes");

        assert_eq!(events.len(), 3);
        assert_eq!(events[2].kind, EventKind::TestFail);
        assert_eq!(events[2].title, "claude tests failed");
        assert_eq!(events[2].command.as_deref(), Some("cargo test --all"));
        assert!(
            !events[2]
                .summary
                .as_deref()
                .unwrap_or_default()
                .contains("raw failing output")
        );
    }

    #[test]
    fn claude_hook_json_normalizes_permission_events() {
        let stream = r#"
{"hook_event_name":"SessionStart","session_id":"claude-2","cwd":"/home/mosure/repos/agent_feed","source":"startup","model":"claude-sonnet-4-6"}
{"hook_event_name":"PreToolUse","session_id":"claude-2","tool_name":"Bash","tool_input":{"command":"git push"}}
{"hook_event_name":"PostToolUse","session_id":"claude-2","tool_name":"Edit","tool_input":{"file_path":"src/lib.rs"},"tool_response":{"is_error":false}}
"#;

        let events = normalize_stream(stream, None).expect("hooks normalize");

        assert_eq!(events.len(), 3);
        assert_eq!(events[1].kind, EventKind::PermissionRequest);
        assert_eq!(events[1].tool.as_deref(), Some("Bash"));
        assert_eq!(events[1].command.as_deref(), Some("git push"));
        assert_eq!(events[2].kind, EventKind::FileChanged);
        assert_eq!(events[2].files, vec!["src/lib.rs"]);
    }
}
