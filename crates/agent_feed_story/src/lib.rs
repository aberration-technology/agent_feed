use agent_feed_core::{
    AgentEvent, Bulletin, BulletinChip, BulletinId, BulletinMode, EventId, EventKind, PrivacyClass,
    Severity, TickerItem, VisualKind,
};
use agent_feed_highlight::score_event;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, VecDeque};
use time::OffsetDateTime;

const DEFAULT_DWELL_MS: u64 = 14_000;
const URGENT_DWELL_MS: u64 = 20_000;
const DEFAULT_MIN_SCORE: u8 = 65;
const DEFAULT_MIN_CONTEXT_SCORE: u8 = 70;
const DEFAULT_DEDUPE_WINDOW: usize = 32;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct StoryCompilerConfig {
    pub min_score: u8,
    pub min_context_score: u8,
    pub per_agent_cooldown_events: usize,
    pub dedupe_window_events: usize,
}

impl Default for StoryCompilerConfig {
    fn default() -> Self {
        Self {
            min_score: DEFAULT_MIN_SCORE,
            min_context_score: DEFAULT_MIN_CONTEXT_SCORE,
            per_agent_cooldown_events: 0,
            dedupe_window_events: DEFAULT_DEDUPE_WINDOW,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct StoryKey {
    pub feed_id: Option<String>,
    pub agent: String,
    pub project_hash: Option<String>,
    pub session_id: Option<String>,
    pub turn_id: Option<String>,
    pub family: StoryFamily,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum StoryFamily {
    Turn,
    Plan,
    Test,
    Permission,
    Command,
    FileChange,
    Mcp,
    Incident,
    IdleRecap,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StoryWindowState {
    Open,
    Settled,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct StoryCounters {
    pub events: usize,
    pub commands: usize,
    pub files_changed: usize,
    pub tool_failures: usize,
    pub tests_passed: usize,
    pub tests_failed: usize,
    pub permissions: usize,
    pub mcp_failures: usize,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct StorySignals {
    pub highest_score: u8,
    pub highest_severity: Severity,
    pub latest_kind: Option<EventKind>,
    pub latest_title: Option<String>,
    pub latest_summary: Option<String>,
    pub latest_tool: Option<String>,
    pub latest_command_class: Option<String>,
    pub files: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct StoryWindow {
    pub key: StoryKey,
    #[serde(with = "time::serde::rfc3339")]
    pub opened_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    pub last_event_at: OffsetDateTime,
    pub events: Vec<EventId>,
    pub counters: StoryCounters,
    pub signals: StorySignals,
    pub state: StoryWindowState,
}

impl StoryWindow {
    fn new(key: StoryKey, now: OffsetDateTime) -> Self {
        Self {
            key,
            opened_at: now,
            last_event_at: now,
            events: Vec::new(),
            counters: StoryCounters::default(),
            signals: StorySignals::default(),
            state: StoryWindowState::Open,
        }
    }

    fn observe(&mut self, event: &AgentEvent) {
        self.last_event_at = event.occurred_at.unwrap_or(event.received_at);
        self.events.push(event.id.clone());
        self.counters.events += 1;
        self.signals.highest_score = self.signals.highest_score.max(score_event(event));
        if severity_rank(event.severity) > severity_rank(self.signals.highest_severity) {
            self.signals.highest_severity = event.severity;
        }
        self.signals.latest_kind = Some(event.kind);
        self.signals.latest_title = Some(event.title.clone());
        self.signals.latest_summary = event.summary.clone();
        self.signals.latest_tool.clone_from(&event.tool);
        if let Some(command) = event.command.as_deref().and_then(command_class) {
            self.signals.latest_command_class = Some(command);
        }
        for file in event.files.iter().take(8) {
            if !self.signals.files.iter().any(|existing| existing == file) {
                self.signals.files.push(file.clone());
            }
        }

        match event.kind {
            EventKind::CommandExec | EventKind::ToolComplete => self.counters.commands += 1,
            EventKind::ToolFail => self.counters.tool_failures += 1,
            EventKind::FileChanged | EventKind::DiffCreated => {
                self.counters.files_changed += event.files.len().max(1);
            }
            EventKind::TestPass => self.counters.tests_passed += 1,
            EventKind::TestFail => self.counters.tests_failed += 1,
            EventKind::PermissionRequest | EventKind::PermissionDenied => {
                self.counters.permissions += 1;
            }
            EventKind::McpFail => self.counters.mcp_failures += 1,
            _ => {}
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CompiledStory {
    pub key: StoryKey,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
    pub family: StoryFamily,
    pub agent: String,
    pub project: Option<String>,
    pub headline: String,
    pub deck: String,
    pub lower_third: String,
    pub chips: Vec<String>,
    pub severity: Severity,
    pub score: u8,
    pub context_score: u8,
    pub privacy: PrivacyClass,
    pub evidence_event_ids: Vec<String>,
}

impl CompiledStory {
    #[must_use]
    pub fn to_bulletin(&self) -> Bulletin {
        let mode = match self.family {
            StoryFamily::Permission | StoryFamily::Incident => BulletinMode::Breaking,
            StoryFamily::FileChange => BulletinMode::DiffAtlas,
            StoryFamily::Command => BulletinMode::CommandDesk,
            StoryFamily::Mcp => BulletinMode::McpWire,
            StoryFamily::IdleRecap => BulletinMode::Recap,
            _ => BulletinMode::Dispatch,
        };
        let dwell_ms = if matches!(mode, BulletinMode::Breaking) {
            URGENT_DWELL_MS
        } else {
            DEFAULT_DWELL_MS
        };
        Bulletin {
            id: BulletinId::new(),
            created_at: self.created_at,
            mode,
            priority: self.score,
            dwell_ms,
            eyebrow: self.eyebrow(),
            headline: clamp_words(&self.headline, 16),
            deck: clamp_words(&self.deck, 34),
            lower_third: self.lower_third.clone(),
            chips: self
                .chips
                .iter()
                .take(5)
                .cloned()
                .map(BulletinChip::new)
                .collect(),
            ticker: Vec::new(),
            image: None,
            visual: VisualKind::Stage,
            privacy: self.privacy,
        }
    }

    #[must_use]
    pub fn ticker_item(&self) -> TickerItem {
        TickerItem::new(format!("{}: {}", self.agent, self.headline))
    }

    fn eyebrow(&self) -> String {
        let project = self.project.as_deref().unwrap_or("local");
        format!(
            "{} / {} / {}",
            self.agent,
            project,
            story_family_label(self.family)
        )
    }
}

#[derive(Clone, Debug)]
pub struct StoryCompiler {
    config: StoryCompilerConfig,
    windows: BTreeMap<StoryKey, StoryWindow>,
    recent_fingerprints: VecDeque<String>,
}

impl Default for StoryCompiler {
    fn default() -> Self {
        Self::new(StoryCompilerConfig::default())
    }
}

impl StoryCompiler {
    #[must_use]
    pub fn new(config: StoryCompilerConfig) -> Self {
        Self {
            config,
            windows: BTreeMap::new(),
            recent_fingerprints: VecDeque::new(),
        }
    }

    #[must_use]
    pub fn ingest(&mut self, event: AgentEvent) -> Vec<CompiledStory> {
        if is_never_publish_event(&event) {
            self.touch_window(event);
            return Vec::new();
        }

        let key = story_key(&event);
        let now = event.occurred_at.unwrap_or(event.received_at);
        let mut window = self
            .windows
            .remove(&key)
            .unwrap_or_else(|| StoryWindow::new(key.clone(), now));
        window.observe(&event);

        if !should_settle(&event, &window) {
            self.windows.insert(key, window);
            return Vec::new();
        }

        window.state = StoryWindowState::Settled;
        self.compile_window(window)
            .into_iter()
            .filter(|story| self.accept_story(story))
            .collect()
    }

    #[must_use]
    pub fn flush(&mut self) -> Vec<CompiledStory> {
        let windows = std::mem::take(&mut self.windows);
        let mut stories = Vec::new();
        for mut window in windows.into_values() {
            if window.signals.highest_score < self.config.min_score {
                continue;
            }
            window.state = StoryWindowState::Settled;
            let Some(story) = self.compile_window(window) else {
                continue;
            };
            if self.accept_story(&story) {
                stories.push(story);
            }
        }
        stories
    }

    fn touch_window(&mut self, event: AgentEvent) {
        let key = story_key(&event);
        let now = event.occurred_at.unwrap_or(event.received_at);
        self.windows
            .entry(key.clone())
            .or_insert_with(|| StoryWindow::new(key, now))
            .observe(&event);
    }

    fn compile_window(&self, window: StoryWindow) -> Option<CompiledStory> {
        let score = window.signals.highest_score;
        let context_score = context_score(&window);
        if score < self.config.min_score || context_score < self.config.min_context_score {
            return None;
        }

        let headline = headline(&window);
        let deck = deck(&window);
        let project = window.key.project_hash.clone();
        let mut chips = vec![
            window.key.agent.clone(),
            story_family_label(window.key.family).to_string(),
            format!("score {score}"),
            "redacted".to_string(),
        ];
        if let Some(project) = &project {
            chips.insert(1, project.clone());
        }
        if window.counters.files_changed > 0 {
            chips.insert(2, format!("{} files", window.counters.files_changed));
        }
        chips.truncate(5);

        let lower_third = lower_third(&window, score);
        Some(CompiledStory {
            key: window.key.clone(),
            created_at: OffsetDateTime::now_utc(),
            family: window.key.family,
            agent: window.key.agent.clone(),
            project,
            headline,
            deck,
            lower_third,
            chips,
            severity: window.signals.highest_severity,
            score,
            context_score,
            privacy: PrivacyClass::Redacted,
            evidence_event_ids: window
                .events
                .iter()
                .take(12)
                .map(ToString::to_string)
                .collect(),
        })
    }

    fn accept_story(&mut self, story: &CompiledStory) -> bool {
        let fingerprint = format!(
            "{}:{}:{}:{}:{:?}",
            story.agent,
            story.project.as_deref().unwrap_or("local"),
            story.key.session_id.as_deref().unwrap_or("session"),
            story.key.turn_id.as_deref().unwrap_or("turn"),
            story.family
        );
        if self
            .recent_fingerprints
            .iter()
            .any(|existing| existing == &fingerprint)
        {
            return false;
        }
        self.recent_fingerprints.push_back(fingerprint);
        while self.recent_fingerprints.len() > self.config.dedupe_window_events.max(1) {
            self.recent_fingerprints.pop_front();
        }
        true
    }
}

#[must_use]
pub fn compile_events(events: impl IntoIterator<Item = AgentEvent>) -> Vec<CompiledStory> {
    let mut compiler = StoryCompiler::default();
    let mut stories = Vec::new();
    for event in events {
        stories.extend(compiler.ingest(event));
    }
    stories.extend(compiler.flush());
    stories
}

fn story_key(event: &AgentEvent) -> StoryKey {
    StoryKey {
        feed_id: None,
        agent: event.agent.clone(),
        project_hash: event.project.clone(),
        session_id: event.session_id.clone(),
        turn_id: event.turn_id.clone(),
        family: family_for(event.kind),
    }
}

fn family_for(kind: EventKind) -> StoryFamily {
    match kind {
        EventKind::PlanUpdate => StoryFamily::Plan,
        EventKind::TestPass | EventKind::TestFail => StoryFamily::Test,
        EventKind::PermissionRequest | EventKind::PermissionDenied => StoryFamily::Permission,
        EventKind::CommandExec | EventKind::ToolStart | EventKind::ToolComplete => {
            StoryFamily::Command
        }
        EventKind::FileChanged | EventKind::DiffCreated => StoryFamily::FileChange,
        EventKind::McpCall | EventKind::McpFail => StoryFamily::Mcp,
        EventKind::ToolFail | EventKind::TurnFail | EventKind::Error => StoryFamily::Incident,
        EventKind::SummaryCreated => StoryFamily::IdleRecap,
        _ => StoryFamily::Turn,
    }
}

fn should_settle(event: &AgentEvent, window: &StoryWindow) -> bool {
    matches!(
        event.kind,
        EventKind::TurnComplete
            | EventKind::TurnFail
            | EventKind::PlanUpdate
            | EventKind::TestPass
            | EventKind::TestFail
            | EventKind::PermissionRequest
            | EventKind::PermissionDenied
            | EventKind::FileChanged
            | EventKind::ToolFail
            | EventKind::McpFail
            | EventKind::SummaryCreated
    ) || (event.kind.is_urgent() && window.signals.highest_score >= 90)
}

fn is_never_publish_event(event: &AgentEvent) -> bool {
    matches!(
        event.kind,
        EventKind::SessionStart
            | EventKind::SessionEnd
            | EventKind::TurnStart
            | EventKind::CommandExec
            | EventKind::ToolStart
            | EventKind::McpCall
            | EventKind::WebSearch
            | EventKind::AgentMessage
    )
}

fn context_score(window: &StoryWindow) -> u8 {
    let mut score = 0u8;
    if !window.key.agent.is_empty() {
        score = score.saturating_add(18);
    }
    if window.signals.latest_kind.is_some() {
        score = score.saturating_add(18);
    }
    if window.key.project_hash.is_some()
        || !window.signals.files.is_empty()
        || window.signals.latest_tool.is_some()
        || window.signals.latest_command_class.is_some()
    {
        score = score.saturating_add(18);
    }
    if outcome_label(window).is_some() {
        score = score.saturating_add(22);
    }
    if window.signals.highest_score >= DEFAULT_MIN_SCORE {
        score = score.saturating_add(16);
    }
    if matches!(
        window.signals.highest_severity,
        Severity::Warning | Severity::Critical
    ) {
        score = score.saturating_add(8);
    }
    score.min(100)
}

fn headline(window: &StoryWindow) -> String {
    let agent = &window.key.agent;
    let object = object_label(window);
    match window.key.family {
        StoryFamily::Test => {
            if window.counters.tests_failed > 0 {
                format!("{agent} found failing tests")
            } else {
                format!("{agent} verified tests")
            }
        }
        StoryFamily::Permission => {
            if window
                .signals
                .latest_kind
                .is_some_and(|kind| kind == EventKind::PermissionDenied)
            {
                format!("{agent} hit a permission denial")
            } else {
                format!("{agent} requested permission")
            }
        }
        StoryFamily::FileChange => format!("{agent} changed {object}"),
        StoryFamily::Incident => format!("{agent} hit {}", object),
        StoryFamily::Mcp => format!("{agent} saw mcp degradation"),
        StoryFamily::Plan => format!("{agent} updated the plan"),
        StoryFamily::Command => format!("{agent} completed {object}"),
        StoryFamily::IdleRecap => format!("{agent} activity settled"),
        StoryFamily::Turn => format!("{agent} completed {object}"),
    }
}

fn deck(window: &StoryWindow) -> String {
    let mut parts = Vec::new();
    if window.counters.files_changed > 0 {
        parts.push(format!(
            "{} changed files",
            window.counters.files_changed.min(99)
        ));
    }
    if window.counters.tests_failed > 0 {
        parts.push("tests are red".to_string());
    } else if window.counters.tests_passed > 0 {
        parts.push("tests passed".to_string());
    }
    if window.counters.tool_failures > 0 {
        parts.push(format!("{} tool failures", window.counters.tool_failures));
    }
    if window.counters.permissions > 0 {
        parts.push(format!("{} permission events", window.counters.permissions));
    }
    if window.counters.mcp_failures > 0 {
        parts.push("mcp failed".to_string());
    }
    if let Some(summary) = &window.signals.latest_summary
        && !summary.is_empty()
        && parts.len() < 2
        && !summary_is_redundant(summary)
    {
        parts.push(safe_sentence(summary));
    }
    if parts.is_empty()
        && let Some(outcome) = outcome_label(window)
    {
        parts.push(outcome.to_string());
    }
    if parts.is_empty() {
        parts.push("activity settled".to_string());
    }
    format!("{}.", parts.join(". "))
}

fn lower_third(window: &StoryWindow, score: u8) -> String {
    let mut parts = vec![window.key.agent.clone()];
    if let Some(project) = &window.key.project_hash {
        parts.push(project.clone());
    } else {
        parts.push("local".to_string());
    }
    parts.push(story_family_label(window.key.family).to_string());
    parts.push(format!("score {score}"));
    parts.push("redacted".to_string());
    parts.join(" · ")
}

fn object_label(window: &StoryWindow) -> String {
    if window.counters.files_changed > 0 {
        return format!("{} files", window.counters.files_changed.min(99));
    }
    if let Some(tool) = &window.signals.latest_tool {
        return format!("{tool} tool");
    }
    if let Some(command) = &window.signals.latest_command_class {
        return command.clone();
    }
    if let Some(project) = &window.key.project_hash {
        return project.clone();
    }
    story_family_label(window.key.family).to_string()
}

fn outcome_label(window: &StoryWindow) -> Option<&'static str> {
    match window.signals.latest_kind {
        Some(EventKind::TurnComplete) => Some("turn completed"),
        Some(EventKind::TurnFail) => Some("turn failed"),
        Some(EventKind::ToolComplete) => Some("tool completed"),
        Some(EventKind::ToolFail) => Some("tool failed"),
        Some(EventKind::PermissionDenied) => Some("permission denied"),
        Some(EventKind::PermissionRequest) => Some("permission requested"),
        Some(EventKind::TestPass) => Some("test passed"),
        Some(EventKind::TestFail) => Some("test failed"),
        Some(EventKind::FileChanged) => Some("files changed"),
        Some(EventKind::DiffCreated) => Some("diff created"),
        Some(EventKind::McpFail) => Some("mcp failed"),
        Some(EventKind::PlanUpdate) => Some("plan updated"),
        Some(EventKind::SummaryCreated) => Some("summary created"),
        _ => None,
    }
}

fn story_family_label(family: StoryFamily) -> &'static str {
    match family {
        StoryFamily::Turn => "turn",
        StoryFamily::Plan => "plan",
        StoryFamily::Test => "test",
        StoryFamily::Permission => "permission",
        StoryFamily::Command => "command",
        StoryFamily::FileChange => "file-change",
        StoryFamily::Mcp => "mcp",
        StoryFamily::Incident => "incident",
        StoryFamily::IdleRecap => "recap",
    }
}

fn command_class(command: &str) -> Option<String> {
    let first = command.split_whitespace().next()?;
    if first.is_empty() {
        None
    } else {
        Some(format!("{first} command"))
    }
}

fn severity_rank(severity: Severity) -> u8 {
    match severity {
        Severity::Debug => 0,
        Severity::Info => 1,
        Severity::Notice => 2,
        Severity::Warning => 3,
        Severity::Critical => 4,
    }
}

fn safe_sentence(input: &str) -> String {
    let lowered = input.to_ascii_lowercase();
    if [
        "secret",
        "token",
        "password",
        "stdout",
        "stderr",
        "diff --git",
    ]
    .iter()
    .any(|needle| lowered.contains(needle))
    {
        return "display-safe summary recorded".to_string();
    }
    clamp_words(input.trim_end_matches(['.', '!', '?']), 20)
}

fn summary_is_redundant(input: &str) -> bool {
    let lowered = input.to_ascii_lowercase();
    [
        "changed files",
        "raw diff omitted",
        "raw output omitted",
        "status ",
        "exit ",
    ]
    .iter()
    .any(|needle| lowered.contains(needle))
}

fn clamp_words(input: &str, max_words: usize) -> String {
    let mut words = input.split_whitespace();
    let mut output = Vec::new();
    for _ in 0..max_words {
        if let Some(word) = words.next() {
            output.push(word);
        }
    }
    if words.next().is_some() {
        format!("{}...", output.join(" "))
    } else {
        output.join(" ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_feed_core::{SourceKind, TickerItem};

    fn event(kind: EventKind, title: &str) -> AgentEvent {
        let mut event = AgentEvent::new(SourceKind::Codex, kind, title);
        event.agent = "codex".to_string();
        event.project = Some("agent_feed".to_string());
        event.session_id = Some("session".to_string());
        event.turn_id = Some("turn".to_string());
        event
    }

    #[test]
    fn low_context_burst_does_not_publish() {
        let mut compiler = StoryCompiler::default();
        let mut message = event(EventKind::AgentMessage, "partial token stream");
        message.summary = Some("assistant message recorded without raw content.".to_string());
        assert!(compiler.ingest(message).is_empty());
        assert!(compiler.flush().is_empty());
    }

    #[test]
    fn file_change_settles_contextual_story() {
        let mut compiler = StoryCompiler::default();
        let mut start = event(EventKind::CommandExec, "codex started a command");
        start.command = Some("cargo test --all".to_string());
        assert!(compiler.ingest(start).is_empty());

        let mut changed = event(EventKind::FileChanged, "codex patch applied");
        changed.files = vec!["src/lib.rs".to_string(), "src/main.rs".to_string()];
        changed.summary = Some("2 changed files. raw diff omitted.".to_string());
        changed.score_hint = Some(82);
        let stories = compiler.ingest(changed);

        assert_eq!(stories.len(), 1);
        assert!(stories[0].headline.contains("codex changed"));
        assert!(stories[0].deck.contains("changed files"));
        assert!(
            stories[0]
                .to_bulletin()
                .eyebrow
                .contains("codex / agent_feed / file-change")
        );
        assert!(!stories[0].deck.contains("raw detail omitted"));
        assert!(!stories[0].deck.contains("cargo test --all"));
    }

    #[test]
    fn severe_tool_failure_publishes_breaking_story() {
        let mut compiler = StoryCompiler::default();
        let mut failed = event(EventKind::ToolFail, "codex command failed");
        failed.summary = Some("exit 1. raw output omitted.".to_string());
        failed.score_hint = Some(92);
        failed.severity = Severity::Warning;

        let stories = compiler.ingest(failed);

        assert_eq!(stories.len(), 1);
        assert_eq!(stories[0].family, StoryFamily::Incident);
        assert!(stories[0].score >= 90);
        assert_eq!(stories[0].to_bulletin().mode, BulletinMode::Breaking);
    }

    #[test]
    fn compiled_story_ticker_is_display_safe() {
        let mut compiler = StoryCompiler::default();
        let mut pass = event(EventKind::TestPass, "tests passed");
        pass.summary = Some("cargo test passed after edit".to_string());
        pass.score_hint = Some(72);
        let stories = compiler.ingest(pass);
        let ticker: TickerItem = stories[0].ticker_item();

        assert!(ticker.text.contains("codex"));
        assert!(!ticker.text.contains("raw"));
    }
}
