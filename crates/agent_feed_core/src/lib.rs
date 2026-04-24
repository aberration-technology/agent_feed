use serde::{Deserialize, Serialize};
use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use time::OffsetDateTime;

static ID_COUNTER: AtomicU64 = AtomicU64::new(1);

pub type AgentName = String;
pub type AdapterName = String;
pub type SessionId = String;
pub type TurnId = String;
pub type ItemId = String;
pub type ProjectRef = String;
pub type MaskedPath = String;
pub type MaskedCommand = String;
pub type MaskedUri = String;
pub type ToolRef = String;
pub type Tag = String;

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct EventId(String);

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct BulletinId(String);

impl EventId {
    #[must_use]
    pub fn new() -> Self {
        Self(next_id("evt"))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Default for EventId {
    fn default() -> Self {
        Self::new()
    }
}

impl BulletinId {
    #[must_use]
    pub fn new() -> Self {
        Self(next_id("blt"))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Default for BulletinId {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for EventId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl fmt::Display for BulletinId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

fn next_id(prefix: &str) -> String {
    let counter = ID_COUNTER.fetch_add(1, Ordering::Relaxed);
    let now = OffsetDateTime::now_utc()
        .unix_timestamp_nanos()
        .unsigned_abs();
    format!("{prefix}_{now:032x}_{counter:08x}")
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceKind {
    Codex,
    Claude,
    Mcp,
    Otel,
    Shell,
    #[default]
    Generic,
}

impl SourceKind {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Codex => "codex",
            Self::Claude => "claude",
            Self::Mcp => "mcp",
            Self::Otel => "otel",
            Self::Shell => "shell",
            Self::Generic => "generic",
        }
    }
}

impl fmt::Display for SourceKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    Debug,
    #[default]
    Info,
    Notice,
    Warning,
    Critical,
}

impl Severity {
    #[must_use]
    pub fn score_bonus(self) -> u8 {
        match self {
            Self::Debug => 0,
            Self::Info => 4,
            Self::Notice => 10,
            Self::Warning => 25,
            Self::Critical => 40,
        }
    }
}

#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
pub enum EventKind {
    #[serde(rename = "session.start")]
    SessionStart,
    #[serde(rename = "session.end")]
    SessionEnd,
    #[serde(rename = "turn.start")]
    TurnStart,
    #[serde(rename = "turn.complete")]
    TurnComplete,
    #[serde(rename = "turn.fail")]
    TurnFail,
    #[serde(rename = "plan.update")]
    PlanUpdate,
    #[serde(rename = "agent.message")]
    AgentMessage,
    #[serde(rename = "tool.start")]
    ToolStart,
    #[serde(rename = "tool.complete")]
    ToolComplete,
    #[serde(rename = "tool.fail")]
    ToolFail,
    #[serde(rename = "permission.request")]
    PermissionRequest,
    #[serde(rename = "permission.denied")]
    PermissionDenied,
    #[serde(rename = "file.changed")]
    FileChanged,
    #[serde(rename = "diff.created")]
    DiffCreated,
    #[serde(rename = "command.exec")]
    CommandExec,
    #[serde(rename = "test.pass")]
    TestPass,
    #[serde(rename = "test.fail")]
    TestFail,
    #[serde(rename = "mcp.call")]
    McpCall,
    #[serde(rename = "mcp.fail")]
    McpFail,
    #[serde(rename = "web.search")]
    WebSearch,
    #[serde(rename = "summary.created")]
    SummaryCreated,
    #[default]
    #[serde(rename = "error")]
    Error,
    #[serde(rename = "adapter.health")]
    AdapterHealth,
}

impl EventKind {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::SessionStart => "session.start",
            Self::SessionEnd => "session.end",
            Self::TurnStart => "turn.start",
            Self::TurnComplete => "turn.complete",
            Self::TurnFail => "turn.fail",
            Self::PlanUpdate => "plan.update",
            Self::AgentMessage => "agent.message",
            Self::ToolStart => "tool.start",
            Self::ToolComplete => "tool.complete",
            Self::ToolFail => "tool.fail",
            Self::PermissionRequest => "permission.request",
            Self::PermissionDenied => "permission.denied",
            Self::FileChanged => "file.changed",
            Self::DiffCreated => "diff.created",
            Self::CommandExec => "command.exec",
            Self::TestPass => "test.pass",
            Self::TestFail => "test.fail",
            Self::McpCall => "mcp.call",
            Self::McpFail => "mcp.fail",
            Self::WebSearch => "web.search",
            Self::SummaryCreated => "summary.created",
            Self::Error => "error",
            Self::AdapterHealth => "adapter.health",
        }
    }

    #[must_use]
    pub fn parse(value: &str) -> Self {
        match value {
            "session.start" => Self::SessionStart,
            "session.end" => Self::SessionEnd,
            "turn.start" => Self::TurnStart,
            "turn.complete" => Self::TurnComplete,
            "turn.fail" => Self::TurnFail,
            "plan.update" => Self::PlanUpdate,
            "agent.message" => Self::AgentMessage,
            "tool.start" => Self::ToolStart,
            "tool.complete" => Self::ToolComplete,
            "tool.fail" => Self::ToolFail,
            "permission.request" => Self::PermissionRequest,
            "permission.denied" => Self::PermissionDenied,
            "file.changed" => Self::FileChanged,
            "diff.created" => Self::DiffCreated,
            "command.exec" => Self::CommandExec,
            "test.pass" => Self::TestPass,
            "test.fail" => Self::TestFail,
            "mcp.call" => Self::McpCall,
            "mcp.fail" => Self::McpFail,
            "web.search" => Self::WebSearch,
            "summary.created" => Self::SummaryCreated,
            "adapter.health" => Self::AdapterHealth,
            _ => Self::Error,
        }
    }

    #[must_use]
    pub fn is_urgent(self) -> bool {
        matches!(
            self,
            Self::PermissionRequest
                | Self::PermissionDenied
                | Self::TurnFail
                | Self::ToolFail
                | Self::TestFail
                | Self::McpFail
                | Self::Error
        )
    }
}

impl fmt::Display for EventKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PrivacyClass {
    #[default]
    Redacted,
    DisplaySafe,
    Sensitive,
    Quarantined,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BulletinMode {
    Breaking,
    #[default]
    Dispatch,
    DiffAtlas,
    CommandDesk,
    McpWire,
    Recap,
    Quiet,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VisualKind {
    #[default]
    Stage,
    Wall,
    Ambient,
    Incident,
    Debug,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RawAgentEvent {
    pub source: SourceKind,
    #[serde(with = "time::serde::rfc3339")]
    pub received_at: OffsetDateTime,
    pub payload: serde_json::Value,
}

impl RawAgentEvent {
    #[must_use]
    pub fn new(source: SourceKind, payload: serde_json::Value) -> Self {
        Self {
            source,
            received_at: OffsetDateTime::now_utc(),
            payload,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AgentEvent {
    pub id: EventId,
    #[serde(with = "time::serde::rfc3339")]
    pub received_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339::option")]
    pub occurred_at: Option<OffsetDateTime>,
    pub source: SourceKind,
    pub agent: AgentName,
    pub adapter: AdapterName,
    pub session_id: Option<SessionId>,
    pub turn_id: Option<TurnId>,
    pub item_id: Option<ItemId>,
    pub project: Option<ProjectRef>,
    pub cwd: Option<MaskedPath>,
    pub kind: EventKind,
    pub severity: Severity,
    pub title: String,
    pub summary: Option<String>,
    pub tool: Option<ToolRef>,
    pub command: Option<MaskedCommand>,
    pub uri: Option<MaskedUri>,
    pub files: Vec<MaskedPath>,
    pub tags: Vec<Tag>,
    pub score_hint: Option<u8>,
    pub privacy: PrivacyClass,
}

impl AgentEvent {
    #[must_use]
    pub fn new(source: SourceKind, kind: EventKind, title: impl Into<String>) -> Self {
        Self {
            id: EventId::new(),
            received_at: OffsetDateTime::now_utc(),
            occurred_at: None,
            source,
            agent: source.as_str().to_string(),
            adapter: "generic".to_string(),
            session_id: None,
            turn_id: None,
            item_id: None,
            project: None,
            cwd: None,
            kind,
            severity: Severity::Info,
            title: title.into(),
            summary: None,
            tool: None,
            command: None,
            uri: None,
            files: Vec::new(),
            tags: Vec::new(),
            score_hint: None,
            privacy: PrivacyClass::Redacted,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BulletinChip {
    pub label: String,
}

impl BulletinChip {
    #[must_use]
    pub fn new(label: impl Into<String>) -> Self {
        Self {
            label: label.into(),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TickerItem {
    pub text: String,
}

impl TickerItem {
    #[must_use]
    pub fn new(text: impl Into<String>) -> Self {
        Self { text: text.into() }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HeadlineImage {
    pub uri: String,
    pub alt: String,
    pub source: String,
}

impl HeadlineImage {
    #[must_use]
    pub fn new(uri: impl Into<String>, alt: impl Into<String>, source: impl Into<String>) -> Self {
        Self {
            uri: uri.into(),
            alt: alt.into(),
            source: source.into(),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Bulletin {
    pub id: BulletinId,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
    pub mode: BulletinMode,
    pub priority: u8,
    pub dwell_ms: u64,
    pub eyebrow: String,
    pub headline: String,
    pub deck: String,
    pub lower_third: String,
    pub chips: Vec<BulletinChip>,
    pub ticker: Vec<TickerItem>,
    pub image: Option<HeadlineImage>,
    pub visual: VisualKind,
    pub privacy: PrivacyClass,
}

#[derive(Debug, thiserror::Error)]
pub enum AgentReelError {
    #[error("invalid input: {0}")]
    InvalidInput(String),
}
